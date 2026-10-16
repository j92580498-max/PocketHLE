//! Gizmondo GPS1 position snapshots. Host location or an explicitly configured simulated position; no NMEA stream.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock, RwLock, Weak,
};
pub const PACKET_SIZE: usize = 180;
pub type Result<T> = std::result::Result<T, u32>;
#[derive(Clone, Debug)]
pub struct Position {
    pub unix_ms: u64,
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_msl: Option<f64>,
    pub speed: Option<f64>,
    pub course: Option<f64>,
    pub horizontal_error: f64,
    pub vertical_error: Option<f64>,
}
pub trait Capture: Send {
    fn latest(&mut self) -> Result<Option<Position>>;
}
pub trait Backend: Send + Sync {
    fn start(&self) -> Result<Box<dyn Capture>>;
}
static HOST: OnceLock<RwLock<Option<Arc<dyn Backend>>>> = OnceLock::new();
pub fn install_host(backend: Arc<dyn Backend>) {
    *HOST.get_or_init(|| RwLock::new(None)).write().unwrap() = Some(backend);
}
fn host() -> Option<Arc<dyn Backend>> {
    let slot = HOST.get_or_init(|| RwLock::new(None));
    #[cfg(windows)]
    {
        let mut h = slot.write().unwrap();
        if h.is_none() {
            *h = Some(Arc::new(windows::WindowsBackend));
        }
    }
    slot.read().unwrap().clone()
}
#[derive(Clone)]
pub struct Service(Arc<Inner>);
struct Inner {
    allowed: AtomicBool,
    backend: Option<Arc<dyn Backend>>,
    device: Mutex<Weak<Mutex<Device>>>,
    fixed_position: Mutex<Option<(f64, f64)>>,
}
impl Default for Service {
    fn default() -> Self {
        Self::new(host())
    }
}
impl Service {
    fn new(backend: Option<Arc<dyn Backend>>) -> Self {
        Self(Arc::new(Inner {
            allowed: AtomicBool::new(false),
            backend,
            device: Mutex::new(Weak::new()),
            fixed_position: Mutex::new(None),
        }))
    }
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self::new(Some(backend))
    }
    pub fn is_enabled(&self) -> bool {
        self.0.allowed.load(Ordering::Acquire)
    }
    pub fn set_allowed(&self, allowed: bool) {
        self.0.allowed.store(allowed, Ordering::Release);
        if !allowed {
            if let Some(d) = self.0.device.lock().unwrap().upgrade() {
                d.lock().unwrap().capture.take();
            }
        }
    }
    /// Explicit simulator setting; an open GPS1 stream uses updates on its next read.
    /// This does not enable device access by itself.
    pub fn set_fixed_position(&self, position: Option<(f64, f64)>) -> Result<()> {
        if let Some((lat, lon)) = position {
            if !lat.is_finite()
                || !(-90. ..=90.).contains(&lat)
                || !lon.is_finite()
                || !(-180. ..=180.).contains(&lon)
            {
                return Err(13);
            }
        }
        *self.0.fixed_position.lock().unwrap() = position;
        if let Some(d) = self.0.device.lock().unwrap().upgrade() {
            d.lock().unwrap().capture.take();
        }
        Ok(())
    }
    pub fn open(&self) -> Result<Arc<Mutex<Device>>> {
        if !self.0.allowed.load(Ordering::Acquire) {
            log::warn!("GPS1 open: error=5 (device access disabled)");
            return Err(5);
        }
        let mut slot = self.0.device.lock().unwrap();
        if let Some(d) = slot.upgrade() {
            return Ok(d);
        }
        let capture = match self.0.start_capture() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("GPS1 open: error={}", e);
                return Err(e);
            }
        };
        let d = Arc::new(Mutex::new(Device {
            capture: Some(capture),
            gate: self.0.clone(),
            fixed: false,
            last_diagnostic: None,
        }));
        *slot = Arc::downgrade(&d);
        Ok(d)
    }
}
impl Inner {
    fn start_capture(&self) -> Result<Box<dyn Capture>> {
        if let Some((latitude, longitude)) = *self.fixed_position.lock().unwrap() {
            return Ok(Box::new(FixedCapture {
                latitude,
                longitude,
            }));
        }
        self.backend.as_ref().ok_or(21u32)?.start()
    }
}
struct FixedCapture {
    latitude: f64,
    longitude: f64,
}
impl Capture for FixedCapture {
    fn latest(&mut self) -> Result<Option<Position>> {
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| 13u32)?
            .as_millis() as u64;
        Ok(Some(Position {
            unix_ms,
            latitude: self.latitude,
            longitude: self.longitude,
            altitude_msl: None,
            speed: Some(0.),
            course: None,
            horizontal_error: 5.,
            vertical_error: None,
        }))
    }
}
pub struct Device {
    capture: Option<Box<dyn Capture>>,
    gate: Arc<Inner>,
    fixed: bool,
    last_diagnostic: Option<(u32, u32, u32)>,
}
impl Device {
    fn diagnostic(
        &mut self,
        error: u32,
        initialized: u32,
        validated: u32,
        utc: u32,
        accuracy: u32,
    ) {
        let state = (error, initialized, validated);
        if self.last_diagnostic != Some(state) {
            log::info!("GPS1 read: error={} initialized={} validated={} utc_1972={} horizontal_error_cm={}",error,initialized,validated,utc,accuracy);
            self.last_diagnostic = Some(state);
        }
    }
    pub fn packet(&mut self) -> Result<[u8; PACKET_SIZE]> {
        if !self.gate.allowed.load(Ordering::Acquire) {
            self.diagnostic(5, 0, 0, 0, u32::MAX);
            return Err(5);
        }
        if self.capture.is_none() {
            match self.gate.start_capture() {
                Ok(c) => self.capture = Some(c),
                Err(e) => {
                    self.diagnostic(e, 0, 0, 0, u32::MAX);
                    return Err(e);
                }
            }
        }
        let position = match self.capture.as_mut().unwrap().latest() {
            Ok(p) => p,
            Err(e) => {
                self.diagnostic(e, 0, 0, 0, u32::MAX);
                return Err(e);
            }
        };
        let mut b = [0u8; PACKET_SIZE];
        b[0] = 1;
        if let Some(p) = position {
            if let Err(e) = validate(&p) {
                self.diagnostic(e, 0, 0, 0, u32::MAX);
                return Err(e);
            }
            self.fixed = true;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            // Preserve the last known coordinates, but do not label old data a fresh fix.
            let fresh =
                p.unix_ms <= now.saturating_add(5000) && now.saturating_sub(p.unix_ms) <= 30000;
            put(&mut b, 1, 1u32);
            let leap_seconds = [
                362793600, 394329600, 425865600, 489024000, 567993600, 631152000, 662688000,
                709948800, 741484800, 773020800, 820454400, 867715200, 915148800, 1136073600,
                1230768000, 1341100800, 1435708800, 1483228800,
            ]
            .into_iter()
            .filter(|t| p.unix_ms / 1000 >= *t)
            .count() as u64;
            let gps_ms = p
                .unix_ms
                .saturating_sub(315964800000)
                .saturating_add(leap_seconds * 1000);
            put(&mut b, 5, (gps_ms / 604800000).min(u16::MAX as u64) as u16);
            put(&mut b, 7, (gps_ms % 604800000) as u32);
            put(
                &mut b,
                11,
                (p.unix_ms / 1000)
                    .saturating_sub(63072000)
                    .min(u32::MAX as u64) as u32,
            );
            // Adapt the SDK validity bit to a fresh native location fix. Host
            // providers do not expose SiRF satellite validation; constellation
            // counts stay zero rather than pretending to receive satellites.
            put(&mut b, 15, u32::from(fresh));
            put(
                &mut b,
                19,
                if fresh {
                    if p.altitude_msl.is_some() {
                        6u16
                    } else {
                        5u16
                    }
                } else {
                    0
                },
            );
            put(&mut b, 21, (p.latitude * 1e7).round() as i32);
            put(&mut b, 25, (p.longitude * 1e7).round() as i32);
            put(
                &mut b,
                29,
                (p.altitude_msl.unwrap_or(0.) * 100.).round() as i32,
            );
            put(
                &mut b,
                33,
                (p.speed.unwrap_or(0.) * 100.).round().clamp(0., 65535.) as u16,
            );
            put(
                &mut b,
                35,
                (p.course.unwrap_or(0.).rem_euclid(360.) * 100.).round() as u16,
            );
            put(
                &mut b,
                37,
                (p.horizontal_error * 100.)
                    .ceil()
                    .clamp(0., u32::MAX as f64) as u32,
            );
            put(
                &mut b,
                41,
                p.vertical_error
                    .map(|v| (v * 100.).ceil().clamp(0., u32::MAX as f64) as u32)
                    .unwrap_or(u32::MAX),
            );
        } else {
            put(&mut b, 1, u32::from(self.fixed));
            put(&mut b, 37, u32::MAX);
            put(&mut b, 41, u32::MAX);
        }
        let word = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        self.diagnostic(0, word(1), word(15), word(11), word(37));
        Ok(b)
    }
}
fn validate(p: &Position) -> Result<()> {
    if !p.latitude.is_finite()
        || !(-90. ..=90.).contains(&p.latitude)
        || !p.longitude.is_finite()
        || !(-180. ..=180.).contains(&p.longitude)
        || !p.horizontal_error.is_finite()
        || p.horizontal_error < 0.
        || p.unix_ms < 63072000000
        || p.altitude_msl.is_some_and(|v| !v.is_finite())
        || p.speed.is_some_and(|v| !v.is_finite() || v < 0.)
        || p.course.is_some_and(|v| !v.is_finite())
        || p.vertical_error.is_some_and(|v| !v.is_finite() || v < 0.)
    {
        return Err(13);
    }
    Ok(())
}
trait Le {
    fn write(self, b: &mut [u8], o: usize);
}
impl Le for u16 {
    fn write(self, b: &mut [u8], o: usize) {
        b[o..o + 2].copy_from_slice(&self.to_le_bytes());
    }
}
impl Le for u32 {
    fn write(self, b: &mut [u8], o: usize) {
        b[o..o + 4].copy_from_slice(&self.to_le_bytes());
    }
}
impl Le for i32 {
    fn write(self, b: &mut [u8], o: usize) {
        b[o..o + 4].copy_from_slice(&self.to_le_bytes());
    }
}
fn put(b: &mut [u8], offset: usize, v: impl Le) {
    v.write(b, offset);
}
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub fn prepare_host_access() {
    windows::prepare_access();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Fake {
        position: Arc<Mutex<Option<Position>>>,
        drops: Arc<AtomicUsize>,
    }
    struct Stream {
        position: Arc<Mutex<Option<Position>>>,
        drops: Arc<AtomicUsize>,
    }
    impl Backend for Fake {
        fn start(&self) -> Result<Box<dyn Capture>> {
            Ok(Box::new(Stream {
                position: self.position.clone(),
                drops: self.drops.clone(),
            }))
        }
    }
    impl Capture for Stream {
        fn latest(&mut self) -> Result<Option<Position>> {
            Ok(self.position.lock().unwrap().clone())
        }
    }
    impl Drop for Stream {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn fixture() -> (Service, Arc<Mutex<Option<Position>>>, Arc<AtomicUsize>) {
        let position = Arc::new(Mutex::new(None));
        let drops = Arc::new(AtomicUsize::new(0));
        let s = Service::with_backend(Arc::new(Fake {
            position: position.clone(),
            drops: drops.clone(),
        }));
        (s, position, drops)
    }
    fn position() -> Position {
        Position {
            unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            latitude: -33.1234567,
            longitude: 151.7654321,
            altitude_msl: Some(-12.34),
            speed: Some(3.25),
            course: Some(270.5),
            horizontal_error: 4.5,
            vertical_error: Some(9.),
        }
    }
    fn word(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }
    #[test]
    fn fixed_position_without_host_validates_coordinates_and_refreshes_time() {
        let s = Service::new(None);
        assert_eq!(s.set_fixed_position(Some((f64::NAN, 0.))), Err(13));
        assert_eq!(s.set_fixed_position(Some((0., 181.))), Err(13));
        s.set_fixed_position(Some((-33.1234567, 151.7654321)))
            .unwrap();
        assert!(matches!(s.open(), Err(5)));
        s.set_allowed(true);
        let d = s.open().unwrap();
        let b = d.lock().unwrap().packet().unwrap();
        assert_eq!(word(&b, 15), 1);
        assert_eq!(word(&b, 21) as i32, -331234567);
        assert_eq!(word(&b, 25) as i32, 1517654321);
        assert_eq!(word(&b, 37), 500);
        assert!(b[45..].iter().all(|v| *v == 0));
        // Reposition a simulator without closing the guest's existing GPS1 handle.
        s.set_fixed_position(Some((48.8584, 2.2945))).unwrap();
        let updated = d.lock().unwrap().packet().unwrap();
        assert_eq!(word(&updated, 21) as i32, 488584000);
        assert_eq!(word(&updated, 25) as i32, 22945000);
        let mut capture = FixedCapture {
            latitude: 0.,
            longitude: 0.,
        };
        let first = capture.latest().unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        assert!(capture.latest().unwrap().unwrap().unix_ms > first.unix_ms);
        s.set_fixed_position(None).unwrap();
        assert_eq!(d.lock().unwrap().packet(), Err(21));
        s.set_allowed(false);
        assert!(matches!(s.open(), Err(5)));
    }
    #[test]
    fn packed_snapshot_signed_coordinates_units_epochs_and_unknown_satellites() {
        let (s, p, _) = fixture();
        s.set_allowed(true);
        *p.lock().unwrap() = Some(position());
        let d = s.open().unwrap();
        let b = d.lock().unwrap().packet().unwrap();
        assert_eq!(b.len(), 180);
        assert_eq!(word(&b, 1), 1);
        assert_eq!(word(&b, 15), 1);
        assert_eq!(word(&b, 21) as i32, -331234567);
        assert_eq!(word(&b, 25) as i32, 1517654321);
        assert_eq!(word(&b, 29) as i32, -1234);
        assert_eq!(u16::from_le_bytes(b[33..35].try_into().unwrap()), 325);
        assert_eq!(u16::from_le_bytes(b[35..37].try_into().unwrap()), 27050);
        assert_eq!(word(&b, 37), 450);
        assert_eq!(word(&b, 41), 900);
        assert!(b[45..].iter().all(|v| *v == 0));
        assert_eq!(
            word(&b, 11) as u64,
            p.lock().unwrap().as_ref().unwrap().unix_ms / 1000 - 63072000
        );
    }
    #[test]
    fn no_fix_stale_fix_invalid_data_and_permission_revocation() {
        let (s, p, drops) = fixture();
        assert!(matches!(s.open(), Err(5)));
        s.set_allowed(true);
        let d = s.open().unwrap();
        let b = d.lock().unwrap().packet().unwrap();
        assert_eq!(word(&b, 11), 0);
        assert_eq!(word(&b, 15), 0);
        assert_eq!(word(&b, 37), u32::MAX);
        let mut v = position();
        v.unix_ms -= 60000;
        *p.lock().unwrap() = Some(v.clone());
        let b = d.lock().unwrap().packet().unwrap();
        assert_eq!(word(&b, 15), 0);
        assert_eq!(&b[19..21], &[0, 0]);
        v.latitude = f64::NAN;
        *p.lock().unwrap() = Some(v);
        assert_eq!(d.lock().unwrap().packet(), Err(13));
        s.set_allowed(false);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(d.lock().unwrap().packet(), Err(5));
    }
    #[test]
    fn vfs_cross_process_duplicate_close_all_and_repeated_reopens_release_capture() {
        let (s, _, drops) = fixture();
        s.set_allowed(true);
        let mut parent = crate::vfs::Vfs::new();
        parent.set_gps_service(s);
        let mut child = crate::vfs::Vfs::new();
        child.attach_shared_context(parent.shared_context());
        for i in 0..100 {
            let h = parent.open_file("GPS1:", 1, 0, 3, false, 0).unwrap().handle;
            assert!(child.import_handle(77, parent.export_handle(h).unwrap()));
            parent.close_all();
            assert_eq!(drops.load(Ordering::SeqCst), i);
            assert!(child
                .gps_open(77)
                .unwrap()
                .device
                .lock()
                .unwrap()
                .packet()
                .is_ok());
            assert_eq!(parent.open_file("GPS1:", 1, 0, 3, false, 0), Err(32));
            child.close_all();
            assert_eq!(drops.load(Ordering::SeqCst), i + 1);
        }
        assert!(parent.open_handles().is_empty());
        assert!(child.open_handles().is_empty());
    }
}
