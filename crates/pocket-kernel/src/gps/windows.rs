//! WinRT location subscription on an MTA; guest reads never wait for a fix.
use super::{Backend, Capture, Position, Result};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use windows::{
    Devices::Geolocation::*,
    Foundation::{AsyncStatus, IAsyncOperation, TypedEventHandler},
    Win32::System::WinRT::{
        RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED,
    },
};
pub struct WindowsBackend;
thread_local! {static UI_APARTMENT:std::cell::RefCell<Option<Apartment>>=const {std::cell::RefCell::new(None)};}
static ACCESS: std::sync::OnceLock<
    std::result::Result<IAsyncOperation<GeolocationAccessStatus>, u32>,
> = std::sync::OnceLock::new();
/// Called by the desktop UI while foregrounded, before starting any guest.
/// WinRT explicitly forbids requesting consent from a capture worker.
pub fn prepare_access() {
    ACCESS.get_or_init(|| unsafe {
        let init = RoInitialize(RO_INIT_SINGLETHREADED);
        if let Err(e) = &init {
            if e.code().0 as u32 != 0x80010106 {
                return Err(error(e.clone()));
            }
        }
        let result = Geolocator::RequestAccessAsync().map_err(error);
        if init.is_ok() {
            UI_APARTMENT.with(|slot| *slot.borrow_mut() = Some(Apartment));
        }
        result
    });
}
#[derive(Default)]
struct Mail {
    position: Option<Position>,
    error: Option<u32>,
}
struct WindowsCapture {
    mail: Arc<Mutex<Mail>>,
    stop: mpsc::Sender<()>,
}
impl Backend for WindowsBackend {
    fn start(&self) -> Result<Box<dyn Capture>> {
        let mail = Arc::new(Mutex::new(Mail::default()));
        let output = mail.clone();
        let (stop, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("pockethle-gps".into())
            .spawn(move || {
                if let Err(e) = run(&output, rx) {
                    output.lock().unwrap().error = Some(e);
                }
            })
            .map_err(|_| 8u32)?;
        Ok(Box::new(WindowsCapture { mail, stop }))
    }
}
impl Capture for WindowsCapture {
    fn latest(&mut self) -> Result<Option<Position>> {
        let m = self.mail.lock().unwrap();
        if let Some(e) = m.error {
            Err(e)
        } else {
            Ok(m.position.clone())
        }
    }
}
impl Drop for WindowsCapture {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}
struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}
fn error(e: windows::core::Error) -> u32 {
    let hr = e.code().0 as u32;
    if hr & 0xffff0000 == 0x80070000 {
        hr & 0xffff
    } else {
        21
    }
}
fn position(p: Geoposition) -> windows::core::Result<Position> {
    let c = p.Coordinate()?;
    let point = c.Point()?.Position()?;
    Ok(Position {
        unix_ms: ((c.Timestamp()?.UniversalTime as i128 / 10000) - 11644473600000).max(0) as u64,
        latitude: point.Latitude,
        longitude: point.Longitude,
        // WinRT altitude is referenced to the ellipsoid, not guaranteed MSL.
        altitude_msl: None,
        speed: c.Speed().and_then(|v| v.Value()).ok(),
        course: c.Heading().and_then(|v| v.Value()).ok(),
        horizontal_error: c.Accuracy()?,
        vertical_error: c.AltitudeAccuracy().and_then(|v| v.Value()).ok(),
    })
}
fn run(mail: &Arc<Mutex<Mail>>, stop: mpsc::Receiver<()>) -> Result<()> {
    unsafe {
        RoInitialize(RO_INIT_MULTITHREADED).map_err(error)?;
    }
    let _apartment = Apartment;
    let access = ACCESS.get().ok_or(21u32)?.as_ref().map_err(|e| *e)?.clone();
    loop {
        if access.Status().map_err(error)? != AsyncStatus::Started {
            break;
        }
        match stop.recv_timeout(Duration::from_millis(100)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Ok(());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    if access.GetResults().map_err(error)? != GeolocationAccessStatus::Allowed {
        return Err(5);
    }
    if stop.try_recv().is_ok() {
        return Ok(());
    }
    let locator = Geolocator::new().map_err(error)?;
    locator
        .SetDesiredAccuracy(PositionAccuracy::High)
        .map_err(error)?;
    locator.SetReportInterval(1000).map_err(error)?;
    let weak = Arc::downgrade(mail);
    let token = locator
        .PositionChanged(
            &TypedEventHandler::<Geolocator, PositionChangedEventArgs>::new(move |_, args| {
                if let (Some(mail), Some(args)) = (weak.upgrade(), args) {
                    let result = args.Position().and_then(position);
                    let mut m = mail.lock().unwrap();
                    match result {
                        Ok(p) => {
                            m.position = Some(p);
                            m.error = None;
                        }
                        Err(e) => m.error = Some(error(e)),
                    }
                }
                Ok(())
            }),
        )
        .map_err(error)?;
    let weak = Arc::downgrade(mail);
    let status = match locator.StatusChanged(
        &TypedEventHandler::<Geolocator, StatusChangedEventArgs>::new(move |_, args| {
            if let (Some(mail), Some(args)) = (weak.upgrade(), args) {
                let s = args.Status()?;
                let mut m = mail.lock().unwrap();
                m.error = if s == PositionStatus::Disabled {
                    Some(5)
                } else if s == PositionStatus::NotAvailable {
                    Some(21)
                } else {
                    None
                };
                if s != PositionStatus::Ready {
                    m.position = None;
                }
            }
            Ok(())
        }),
    ) {
        Ok(t) => t,
        Err(e) => {
            let _ = locator.RemovePositionChanged(token);
            return Err(error(e));
        }
    };
    let _ = stop.recv();
    let _ = locator.RemoveStatusChanged(status);
    let _ = locator.RemovePositionChanged(token);
    Ok(())
}
