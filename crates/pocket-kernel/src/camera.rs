//! Gizmondo CAM1 host bridge. Camera acquisition is asynchronous; guest buffers
//! contain RGB565 preview or planar Y/U/V 4:2:0, never host pointers.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock, RwLock, Weak,
};
use std::time::Instant;
pub type Result<T> = std::result::Result<T, u32>;
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub serial: u64,
    pub rgb: Vec<u8>,
}
pub trait Capture: Send {
    fn latest(&mut self) -> Result<Option<Arc<Frame>>>;
}
pub trait Backend: Send + Sync {
    fn start(&self) -> Result<Box<dyn Capture>>;
}
static HOST: OnceLock<RwLock<Option<Arc<dyn Backend>>>> = OnceLock::new();
pub fn install_host(backend: Arc<dyn Backend>) {
    *HOST.get_or_init(|| RwLock::new(None)).write().unwrap() = Some(backend);
}
fn host() -> Option<Arc<dyn Backend>> {
    let cell = HOST.get_or_init(|| RwLock::new(None));
    #[cfg(windows)]
    {
        let mut h = cell.write().unwrap();
        if h.is_none() {
            *h = Some(Arc::new(windows::WindowsBackend));
        }
    }
    cell.read().unwrap().clone()
}
#[derive(Clone)]
pub struct Service(Arc<ServiceInner>);
struct ServiceInner {
    allowed: AtomicBool,
    backend: Option<Arc<dyn Backend>>,
    device: Mutex<Weak<Mutex<Device>>>,
}
impl Default for Service {
    fn default() -> Self {
        Self::with_optional_backend(host())
    }
}
impl Service {
    fn with_optional_backend(backend: Option<Arc<dyn Backend>>) -> Self {
        Self(Arc::new(ServiceInner {
            allowed: AtomicBool::new(false),
            backend,
            device: Mutex::new(Weak::new()),
        }))
    }
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self::with_optional_backend(Some(backend))
    }
    pub fn set_allowed(&self, allowed: bool) {
        self.0.allowed.store(allowed, Ordering::Release);
        if !allowed {
            if let Some(device) = self.0.device.lock().unwrap().upgrade() {
                device.lock().unwrap().stop();
            }
        }
    }
    pub fn open(&self) -> Result<Arc<Mutex<Device>>> {
        if !self.0.allowed.load(Ordering::Acquire) {
            return Err(5);
        }
        if self.0.backend.is_none() {
            return Err(2);
        }
        let mut slot = self.0.device.lock().unwrap();
        if let Some(device) = slot.upgrade() {
            return Ok(device);
        }
        let device = Arc::new(Mutex::new(Device {
            service: self.clone(),
            capture: None,
            format: [640, 480, 320, 240],
            frame_count: 0,
            last_serial: None,
            next_preview: Instant::now(),
        }));
        *slot = Arc::downgrade(&device);
        Ok(device)
    }
}
pub struct Device {
    service: Service,
    pub format: [u32; 4],
    capture: Option<Box<dyn Capture>>,
    pub frame_count: u32,
    last_serial: Option<u64>,
    pub next_preview: Instant,
}
impl Device {
    pub fn start(&mut self) -> Result<()> {
        if !self.service.0.allowed.load(Ordering::Acquire) {
            return Err(5);
        }
        if self.capture.is_none() {
            self.capture = Some(self.service.0.backend.as_ref().ok_or(2u32)?.start()?);
            self.last_serial = None;
            self.next_preview = Instant::now();
        }
        Ok(())
    }
    pub fn stop(&mut self) {
        self.capture.take();
        self.last_serial = None;
    }
    pub fn set_format(&mut self, format: [u32; 4]) -> Result<()> {
        if format[0] != 640
            || format[1] != 480
            || format[2] == 0
            || format[3] == 0
            || format[2] > 640
            || format[3] > 480
            || format[2] % 8 != 0
            || format[3] % 8 != 0
        {
            return Err(87);
        }
        self.format = format;
        Ok(())
    }
    pub fn latest(&mut self) -> Result<Option<Arc<Frame>>> {
        let frame = self.capture.as_mut().ok_or(21u32)?.latest()?;
        let Some(frame) = frame else { return Ok(None) };
        if frame.width == 0
            || frame.height == 0
            || frame.width > 4096
            || frame.height > 4096
            || frame.rgb.len() != frame.width as usize * frame.height as usize * 3
        {
            return Err(13);
        }
        if self.last_serial == Some(frame.serial) {
            return Ok(None);
        }
        Ok(Some(frame))
    }
    pub fn consumed(&mut self, serial: u64) {
        self.last_serial = Some(serial);
        self.frame_count = self.frame_count.wrapping_add(1);
    }
}
fn pixel(frame: &Frame, x: u32, y: u32, w: u32, h: u32) -> [i32; 3] {
    let x = x * frame.width / w;
    let y = y * frame.height / h;
    let at = ((y * frame.width + x) * 3) as usize;
    [
        frame.rgb[at] as i32,
        frame.rgb[at + 1] as i32,
        frame.rgb[at + 2] as i32,
    ]
}
// The SDK camera sample places preview bytes directly in a positive-height
// RGB565 DIB: the first stored row is the bottom row. Host Frame RGB remains
// top-down; reverse rows only at the CAM1 preview ABI, without mirroring x.
pub fn preview(frame: &Frame, w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 2) as usize);
    for y in (0..h).rev() {
        for x in 0..w {
            let [r, g, b] = pixel(frame, x, y, w, h);
            let p = ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3);
            out.extend_from_slice(&p.to_le_bytes());
        }
    }
    out
}
pub fn capture(frame: &Frame) -> Vec<u8> {
    let (w, h) = (640, 480);
    let mut out = vec![0; (w * h * 3 / 2) as usize];
    let plane = (w * h) as usize;
    for y in 0..h {
        for x in 0..w {
            let [r, g, b] = pixel(frame, x, y, w, h);
            out[(y * w + x) as usize] =
                (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
        }
    }
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let mut rgb = [0; 3];
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = pixel(frame, x + dx, y + dy, w, h);
                    for i in 0..3 {
                        rgb[i] += p[i];
                    }
                }
            }
            let [r, g, b] = rgb.map(|v| (v + 2) / 4);
            let at = (y / 2 * (w / 2) + x / 2) as usize;
            out[plane + at] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            out[plane + plane / 4 + at] =
                (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
    out
}
/// Decode Camera2's packed I420 mailbox. Validate before any plane indexing.
pub fn from_i420(width: u32, height: u32, serial: u64, data: &[u8]) -> Result<Frame> {
    if width == 0
        || height == 0
        || width > 4096
        || height > 4096
        || width % 2 != 0
        || height % 2 != 0
    {
        return Err(13);
    }
    let plane = (width * height) as usize;
    if data.len() != plane * 3 / 2 {
        return Err(13);
    }
    let mut rgb = Vec::with_capacity(plane * 3);
    for y in 0..height {
        for x in 0..width {
            let i = (y * width + x) as usize;
            let uv = (y / 2 * (width / 2) + x / 2) as usize;
            let c = (data[i] as i32 - 16).max(0);
            let d = data[plane + uv] as i32 - 128;
            let e = data[plane + plane / 4 + uv] as i32 - 128;
            rgb.extend_from_slice(&[
                ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8,
                ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8,
                ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8,
            ]);
        }
    }
    Ok(Frame {
        width,
        height,
        serial,
        rgb,
    })
}
#[cfg(windows)]
mod windows;
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Fake {
        drops: Arc<AtomicUsize>,
        frame: Arc<Frame>,
    }
    struct Stream {
        drops: Arc<AtomicUsize>,
        frame: Arc<Frame>,
    }
    impl Backend for Fake {
        fn start(&self) -> Result<Box<dyn Capture>> {
            Ok(Box::new(Stream {
                drops: self.drops.clone(),
                frame: self.frame.clone(),
            }))
        }
    }
    impl Capture for Stream {
        fn latest(&mut self) -> Result<Option<Arc<Frame>>> {
            Ok(Some(self.frame.clone()))
        }
    }
    impl Drop for Stream {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn fixture() -> (Service, Arc<AtomicUsize>) {
        let drops = Arc::new(AtomicUsize::new(0));
        let service = Service::with_backend(Arc::new(Fake {
            drops: drops.clone(),
            frame: Arc::new(Frame {
                width: 2,
                height: 2,
                serial: 7,
                rgb: vec![255; 12],
            }),
        }));
        (service, drops)
    }
    #[test]
    fn formats_preserve_sdk_limits_and_reject_partial_pixels() {
        let (s, _) = fixture();
        assert!(matches!(s.open(), Err(5)));
        s.set_allowed(true);
        let d = s.open().unwrap();
        let mut d = d.lock().unwrap();
        assert_eq!(d.format, [640, 480, 320, 240]);
        for bad in [
            [320, 240, 320, 240],
            [640, 480, 0, 240],
            [640, 480, 321, 240],
            [640, 480, 648, 480],
            [640, 480, 640, 488],
        ] {
            assert_eq!(d.set_format(bad), Err(87));
        }
        assert_eq!(d.set_format([640, 480, 8, 8]), Ok(()));
        assert_eq!(d.set_format([640, 480, 640, 480]), Ok(()));
    }
    #[test]
    fn preview_has_sdk_bottom_up_rows_without_mirroring_columns() {
        let f = Frame {
            width: 2,
            height: 2,
            serial: 0,
            rgb: vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
        };
        assert_eq!(
            preview(&f, 2, 2),
            [0x1f, 0x00, 0xff, 0xff, 0x00, 0xf8, 0xe0, 0x07]
        );
        assert_eq!(
            preview(&f, 4, 4)[..8],
            [0x1f, 0, 0x1f, 0, 0xff, 0xff, 0xff, 0xff]
        );
        // The preview layout must not rotate/mirror the host image or I420.
        assert_eq!(&f.rgb[..3], &[255, 0, 0]);
        let still = capture(&f);
        assert_eq!(still[0], 82);
        assert_eq!(still[639], 144);
        assert_eq!(still[479 * 640], 41);
        assert_eq!(still[479 * 640 + 639], 235);
    }
    #[test]
    fn capture_has_fixed_i420_planes_and_bt601_colors() {
        for (rgb, y, u, v) in [
            ([255, 0, 0], 82, 90, 240),
            ([0, 0, 255], 41, 240, 110),
            ([255, 255, 255], 235, 128, 128),
        ] {
            let f = Frame {
                width: 1,
                height: 1,
                serial: 0,
                rgb: rgb.to_vec(),
            };
            let out = capture(&f);
            let plane = 640 * 480;
            assert_eq!(out.len(), plane * 3 / 2);
            assert!(out[..plane].iter().all(|p| *p == y));
            assert!(out[plane..plane + plane / 4].iter().all(|p| *p == u));
            assert!(out[plane + plane / 4..].iter().all(|p| *p == v));
        }
    }
    #[test]
    fn camera2_i420_decode_validates_size_and_plane_order() {
        let f = from_i420(2, 2, 9, &[82, 82, 82, 82, 90, 240]).unwrap();
        assert_eq!(f.serial, 9);
        for pixel in f.rgb.chunks_exact(3) {
            assert!(pixel[0] >= 254 && pixel[1] <= 1 && pixel[2] <= 1);
        }
        for (w, h, data) in [
            (0, 2, &[][..]),
            (3, 2, &[0; 9][..]),
            (2, 2, &[0; 5][..]),
            (u32::MAX, 2, &[][..]),
        ] {
            assert!(matches!(from_i420(w, h, 0, data), Err(13)));
        }
    }
    #[test]
    fn repeated_capture_restart_and_last_owner_drop_release_native_objects() {
        let (service, drops) = fixture();
        service.set_allowed(true);
        for cycle in 0..100 {
            let first = service.open().unwrap();
            let second = service.open().unwrap();
            assert!(Arc::ptr_eq(&first, &second));
            let weak = Arc::downgrade(&first);
            first.lock().unwrap().start().unwrap();
            drop(first);
            assert_eq!(drops.load(Ordering::SeqCst), cycle * 2);
            second.lock().unwrap().stop();
            assert_eq!(drops.load(Ordering::SeqCst), cycle * 2 + 1);
            second.lock().unwrap().start().unwrap();
            drop(second);
            assert!(weak.upgrade().is_none());
            assert_eq!(drops.load(Ordering::SeqCst), cycle * 2 + 2);
        }
    }

    #[test]
    fn shared_device_stops_on_disable_or_last_close_without_cycles() {
        let (s, drops) = fixture();
        s.set_allowed(true);
        let a = s.open().unwrap();
        let b = s.open().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        a.lock().unwrap().start().unwrap();
        drop(a);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        s.set_allowed(false);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(b.lock().unwrap().start(), Err(5));
        s.set_allowed(true);
        b.lock().unwrap().start().unwrap();
        drop(b);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        let a = s.open().unwrap();
        assert_eq!(a.lock().unwrap().frame_count, 0);
    }
    #[test]
    fn delivered_serial_is_not_delivered_twice_and_stop_requires_restart() {
        let (s, _) = fixture();
        s.set_allowed(true);
        let d = s.open().unwrap();
        let mut d = d.lock().unwrap();
        assert!(matches!(d.latest(), Err(21)));
        d.start().unwrap();
        assert_eq!(d.latest().unwrap().unwrap().serial, 7);
        d.consumed(7);
        assert!(d.latest().unwrap().is_none());
        assert_eq!(d.frame_count, 1);
        d.stop();
        assert!(matches!(d.latest(), Err(21)));
        d.start().unwrap();
        assert!(d.latest().unwrap().is_some());
    }
}
