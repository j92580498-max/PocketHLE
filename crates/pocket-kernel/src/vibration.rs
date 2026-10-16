//! Gizmondo's single NLED motor. Settings belong to the emulated device,
//! not the VIB1 open handle: the SDK closes that handle before sleeping.
use std::sync::{Arc, Mutex, OnceLock, RwLock};
pub type Result<T> = std::result::Result<T, u32>;
pub trait Motor: Send {
    fn apply(&mut self, timings_ms: &[i64]) -> Result<()>;
}
pub trait Backend: Send + Sync {
    fn open(&self) -> Result<Box<dyn Motor>>;
}
static HOST: OnceLock<RwLock<Option<Arc<dyn Backend>>>> = OnceLock::new();
pub fn install_host(backend: Arc<dyn Backend>) {
    *HOST.get_or_init(|| RwLock::new(None)).write().unwrap() = Some(backend);
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub mode: u32,
    pub total: i32,
    pub on: i32,
    pub off: i32,
    pub meta_on: i32,
    pub meta_off: i32,
}
impl Settings {
    pub fn waveform(self) -> Result<Vec<i64>> {
        match self.mode {
            0 => Ok(vec![]),
            1 => Ok(vec![0, 1000]), // repeat continuously until another setting/teardown
            2 => {
                if self.on <= 0
                    || self.off < 0
                    || self.total <= 0
                    || self.on.checked_add(self.off) != Some(self.total)
                    || self.meta_on < 0
                    || self.meta_off < 0
                {
                    return Err(87);
                }
                if self.meta_on == 0 && self.meta_off != 0 {
                    return Err(87);
                }
                let cycles = self.meta_on.max(1) as usize;
                // A malformed guest must not allocate unbounded host arrays.
                if cycles > 1024 {
                    return Err(50);
                }
                let ms = |us: i64| if us == 0 { 0 } else { (us + 999) / 1000 };
                let mut out = vec![0];
                for i in 0..cycles {
                    out.push(ms(self.on as i64));
                    out.push(ms(self.off as i64
                        + if i + 1 == cycles {
                            self.meta_off as i64 * self.total as i64
                        } else {
                            0
                        }));
                }
                // Even length gives the legacy Android waveform a stable off/on
                // phase on repeat. The final zero is a zero-duration on segment.
                out.push(0);
                Ok(out)
            }
            _ => Err(87),
        }
    }
}
struct Inner {
    settings: Settings,
    backend: Option<Arc<dyn Backend>>,
    motor: Option<Box<dyn Motor>>,
}
#[derive(Clone)]
pub struct Service(Arc<Mutex<Inner>>);
impl Default for Service {
    fn default() -> Self {
        Self::new(
            HOST.get_or_init(|| RwLock::new(None))
                .read()
                .unwrap()
                .clone(),
        )
    }
}
impl Service {
    fn new(backend: Option<Arc<dyn Backend>>) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            settings: Settings::default(),
            backend,
            motor: None,
        })))
    }
    pub fn with_backend(backend: Arc<dyn Backend>) -> Self {
        Self::new(Some(backend))
    }
    pub fn settings(&self) -> Settings {
        self.0.lock().unwrap().settings
    }
    pub fn set(&self, mut value: Settings) -> Result<()> {
        let mut inner = self.0.lock().unwrap();
        // Off/On callers need not initialize fields only used for blinking.
        if value.mode < 2 {
            value = Settings {
                mode: value.mode,
                ..inner.settings
            };
        }
        let waveform = value.waveform()?;
        let was_closed = inner.motor.is_none();
        if was_closed && !waveform.is_empty() {
            if let Some(b) = &inner.backend {
                inner.motor = Some(b.open()?);
            }
        }
        if let Some(motor) = &mut inner.motor {
            if let Err(error) = motor.apply(&waveform) {
                if was_closed {
                    inner.motor.take();
                }
                return Err(error);
            }
        }
        if value.mode == 0 {
            inner.motor.take();
        }
        inner.settings = value;
        Ok(())
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(m) = &mut self.motor {
            let _ = m.apply(&[]);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_and_meta_cycle_waveforms() {
        assert_eq!(
            Settings {
                mode: 2,
                total: 4000000,
                on: 2000000,
                off: 2000000,
                ..Settings::default()
            }
            .waveform()
            .unwrap(),
            vec![0, 2000, 2000, 0]
        );
        assert_eq!(
            Settings {
                mode: 2,
                total: 3000,
                on: 1000,
                off: 2000,
                meta_on: 2,
                meta_off: 3
            }
            .waveform()
            .unwrap(),
            vec![0, 1, 2, 1, 11, 0]
        );
        assert_eq!(
            Settings {
                mode: 2,
                total: 2,
                on: 1,
                off: 1,
                ..Settings::default()
            }
            .waveform()
            .unwrap(),
            vec![0, 1, 1, 0]
        );
        assert_eq!(
            Settings {
                mode: 3,
                ..Settings::default()
            }
            .waveform(),
            Err(87)
        );
        assert_eq!(
            Settings {
                mode: 2,
                total: 2,
                on: 1,
                off: 0,
                ..Settings::default()
            }
            .waveform(),
            Err(87)
        );
    }
    struct Fake(Arc<Mutex<Vec<Vec<i64>>>>);
    impl Backend for Fake {
        fn open(&self) -> Result<Box<dyn Motor>> {
            Ok(Box::new(Fake(self.0.clone())))
        }
    }
    impl Motor for Fake {
        fn apply(&mut self, v: &[i64]) -> Result<()> {
            self.0.lock().unwrap().push(v.to_vec());
            Ok(())
        }
    }
    #[test]
    fn shared_owners_preserve_motor_until_last_device_drop() {
        let events = Arc::new(Mutex::new(vec![]));
        let s = Service::with_backend(Arc::new(Fake(events.clone())));
        let other = s.clone();
        s.set(Settings {
            mode: 1,
            ..Settings::default()
        })
        .unwrap();
        drop(s);
        assert_eq!(events.lock().unwrap().len(), 1);
        drop(other);
        assert_eq!(*events.lock().unwrap(), vec![vec![0, 1000], vec![]]);
    }
    struct CountBackend {
        live: Arc<std::sync::atomic::AtomicUsize>,
        fail: bool,
    }
    struct CountMotor {
        live: Arc<std::sync::atomic::AtomicUsize>,
        fail: bool,
    }
    impl Backend for CountBackend {
        fn open(&self) -> Result<Box<dyn Motor>> {
            self.live.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(CountMotor {
                live: self.live.clone(),
                fail: self.fail,
            }))
        }
    }
    impl Motor for CountMotor {
        fn apply(&mut self, _: &[i64]) -> Result<()> {
            if self.fail {
                Err(5)
            } else {
                Ok(())
            }
        }
    }
    impl Drop for CountMotor {
        fn drop(&mut self) {
            self.live.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    #[test]
    fn stop_failed_start_and_teardown_release_every_native_motor() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let live = Arc::new(AtomicUsize::new(0));
        let service = Service::with_backend(Arc::new(CountBackend {
            live: live.clone(),
            fail: false,
        }));
        for _ in 0..100 {
            service
                .set(Settings {
                    mode: 1,
                    ..Settings::default()
                })
                .unwrap();
            assert_eq!(live.load(Ordering::SeqCst), 1);
            service.set(Settings::default()).unwrap();
            assert_eq!(live.load(Ordering::SeqCst), 0);
        }
        service
            .set(Settings {
                mode: 1,
                ..Settings::default()
            })
            .unwrap();
        drop(service);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        let failed = Service::with_backend(Arc::new(CountBackend {
            live: live.clone(),
            fail: true,
        }));
        assert_eq!(
            failed.set(Settings {
                mode: 1,
                ..Settings::default()
            }),
            Err(5)
        );
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert_eq!(failed.settings().mode, 0);
    }
}
