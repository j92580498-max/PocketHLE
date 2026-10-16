//! Actual ARM hardware diagnostics with controlled host providers.
//! These verify ABI/ownership, never real Bluetooth/camera/GPS/TLS hardware.
#![cfg(feature = "unicorn")]
use pocket_core::{
    kernel::{self, FrameAction, InputEvent},
    Emulator,
};
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

struct Files(PathBuf);
impl Drop for Files {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn run(
    executable: &[u8],
    card: &str,
    report: &str,
    configure: impl FnOnce(&mut kernel::KernelState),
) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let files = Files(std::env::temp_dir().join(format!("pockethle-device-{card}-{nonce}")));
    fs::create_dir_all(&files.0).unwrap();
    let flash = files.0.join("flash");
    fs::create_dir_all(&flash).unwrap();
    fs::write(files.0.join("AUTORUN.EXE"), executable).unwrap();
    let mut emu = Emulator::with_unicorn_cpu().unwrap();
    emu.max_slices = 3_000_000;
    emu.set_halt_on_unimplemented(true);
    emu.load_pe(files.0.join("AUTORUN.EXE")).unwrap();
    assert!(emu.set_memory_division(Some(
        kernel::memory_division::MemoryDivision::gizmondo_sdk_default()
    )));
    {
        let state = &mut emu.process_mut().unwrap().state;
        state
            .vfs
            .mount_read_only(&format!("\\SD Card\\{card}\\"), &files.0);
        state.vfs.mount_save_dir("\\Flash Disk\\", &flash);
        state.module_path = format!("\\SD Card\\{card}\\AUTORUN.EXE");
        state.synthetic_message_budget = 0;
        configure(state);
    }
    let mut hook = |state: &mut kernel::KernelState| {
        if state.modal.is_some() {
            state
                .pending_input
                .push_back(InputEvent::KeyDown { vk: 0x0d });
        }
        FrameAction::Continue
    };
    emu.run_with_hook(&mut hook).unwrap();
    let path = fs::read_dir(&flash)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case(&format!("{report}.TXT"))
        })
        .unwrap_or_else(|| {
            panic!(
                "Missing {report} report, exit={:?}",
                emu.process().unwrap().state.process_exit_code
            )
        });
    let text = fs::read_to_string(path).unwrap();
    println!("{text}");
    assert!(text.contains(&format!("{report}_RESULT PASS")), "{text}");
    assert!(!text.lines().any(|l| l.starts_with("FAIL ")), "{text}");
    assert_eq!(emu.process().unwrap().state.process_exit_code, Some(0));
    let state = &emu.process().unwrap().state;
    assert!(state.vfs.open_handles().is_empty());
    assert!(state.vfs.bluetooth.lookups.is_empty());
    assert!(state.vfs.bluetooth.sockets.is_empty());
    assert!(state.vfs.bluetooth.socket_deadlines.is_empty());
    assert_eq!(state.vfs.bluetooth.startups, 0);
}

struct Camera(Arc<AtomicUsize>);
struct CameraCapture(Arc<AtomicUsize>, u64);
impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl kernel::camera::Backend for Camera {
    fn start(&self) -> kernel::camera::Result<Box<dyn kernel::camera::Capture>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CameraCapture(self.0.clone(), 0)))
    }
}
impl kernel::camera::Capture for CameraCapture {
    fn latest(&mut self) -> kernel::camera::Result<Option<Arc<kernel::camera::Frame>>> {
        self.1 += 1;
        Ok(Some(Arc::new(kernel::camera::Frame {
            width: 2,
            height: 2,
            serial: self.1,
            rgb: vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255],
        })))
    }
}
#[test]
fn native_arm_camera_cycles_release_capture() {
    let live = Arc::new(AtomicUsize::new(0));
    let count = live.clone();
    run(
        include_bytes!("fixtures/camtest/GZCT999997/AUTORUN.EXE"),
        "GZCT999997",
        "CAMTEST",
        move |state| {
            let service = kernel::camera::Service::with_backend(Arc::new(Camera(count)));
            service.set_allowed(true);
            state.vfs.set_camera_service(service);
        },
    );
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

struct Gps(Arc<AtomicUsize>);
struct GpsCapture(Arc<AtomicUsize>);
impl Drop for GpsCapture {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl kernel::gps::Backend for Gps {
    fn start(&self) -> kernel::gps::Result<Box<dyn kernel::gps::Capture>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(GpsCapture(self.0.clone())))
    }
}
impl kernel::gps::Capture for GpsCapture {
    fn latest(&mut self) -> kernel::gps::Result<Option<kernel::gps::Position>> {
        Ok(Some(kernel::gps::Position {
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            latitude: 48.8584,
            longitude: 2.2945,
            altitude_msl: None,
            speed: None,
            course: None,
            horizontal_error: 5.0,
            vertical_error: None,
        }))
    }
}
#[test]
fn native_arm_gps_cycles_release_provider() {
    let live = Arc::new(AtomicUsize::new(0));
    let count = live.clone();
    run(
        include_bytes!("fixtures/gpstest/GZGP999994/AUTORUN.EXE"),
        "GZGP999994",
        "GPSTEST",
        move |state| {
            let service = kernel::gps::Service::with_backend(Arc::new(Gps(count)));
            service.set_allowed(true);
            state.vfs.set_gps_service(service);
        },
    );
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

struct Vibrator(Arc<AtomicUsize>);
struct Motor(Arc<AtomicUsize>);
impl Drop for Motor {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl kernel::vibration::Motor for Motor {
    fn apply(&mut self, _: &[i64]) -> kernel::vibration::Result<()> {
        Ok(())
    }
}
impl kernel::vibration::Backend for Vibrator {
    fn open(&self) -> kernel::vibration::Result<Box<dyn kernel::vibration::Motor>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Motor(self.0.clone())))
    }
}
#[test]
fn native_arm_vibration_cycles_release_motor() {
    let live = Arc::new(AtomicUsize::new(0));
    let count = live.clone();
    run(
        include_bytes!("fixtures/vibtest/GZVB999990/AUTORUN.EXE"),
        "GZVB999990",
        "VIBTEST",
        move |state| {
            state
                .vfs
                .set_vibration_service(kernel::vibration::Service::with_backend(Arc::new(
                    Vibrator(count),
                )));
        },
    );
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

struct Network(Arc<AtomicUsize>);
struct Client(Arc<AtomicUsize>);
struct Transfer {
    live: Arc<AtomicUsize>,
    offset: usize,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Drop for Transfer {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}
impl kernel::internet::Backend for Network {
    fn open(
        &self,
        _: &kernel::internet::SessionSpec,
    ) -> kernel::internet::Result<Arc<dyn kernel::internet::Client>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Client(self.0.clone())))
    }
}
impl kernel::internet::Client for Client {
    fn start(
        &self,
        _: kernel::internet::RequestSpec,
        _: Vec<u8>,
    ) -> kernel::internet::Result<Box<dyn kernel::internet::Transfer>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Transfer {
            live: self.0.clone(),
            offset: 0,
        }))
    }
}
impl kernel::internet::Transfer for Transfer {
    fn head(&mut self) -> kernel::internet::Result<Option<kernel::internet::ResponseHead>> {
        Ok(Some(kernel::internet::ResponseHead {
            status: 200,
            version: "HTTP/1.1".into(),
            reason: "OK".into(),
            headers: vec![("Content-Length".into(), "4".into())],
        }))
    }
    fn available(&mut self) -> kernel::internet::Result<(usize, bool)> {
        Ok((4 - self.offset, true))
    }
    fn read(&mut self, out: &mut [u8]) -> kernel::internet::Result<Option<usize>> {
        let n = out.len().min(4 - self.offset);
        out[..n].copy_from_slice(&b"TEST"[self.offset..self.offset + n]);
        self.offset += n;
        Ok(Some(n))
    }
}
#[test]
fn native_arm_network_cycles_release_sessions_and_transfers() {
    let live = Arc::new(AtomicUsize::new(0));
    let count = live.clone();
    run(
        include_bytes!("fixtures/nettest/GZNT999993/AUTORUN.EXE"),
        "GZNT999993",
        "NETTEST",
        move |state| {
            state.internet = kernel::internet::State::with_backend(Arc::new(Network(count)));
            state.internet.set_gprs_enabled(true);
        },
    );
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

struct Bluetooth;
impl kernel::bluetooth::Backend for Bluetooth {
    fn hostname(&self) -> kernel::bluetooth::BtResult<String> {
        Ok("Controlled test host".into())
    }
    fn scan(&self) -> kernel::bluetooth::BtResult<Vec<kernel::bluetooth::Device>> {
        Ok(vec![])
    }
    fn open(
        &self,
        _: &kernel::bluetooth::PortParams,
    ) -> kernel::bluetooth::BtResult<Box<dyn kernel::bluetooth::Stream>> {
        Err(50)
    }
}
#[test]
fn native_arm_bluetooth_startup_and_cancelled_inquiry_cycles() {
    run(
        include_bytes!("fixtures/bttest/GZBT999996/AUTORUN.EXE"),
        "GZBT999996",
        "BTTEST",
        |state| {
            let service = kernel::bluetooth::Service::with_backend(Arc::new(Bluetooth));
            service.set_allowed(true);
            state.vfs.set_bluetooth_service(service);
        },
    );
}
