//! Host radio/SIM snapshot, with no host subscriptions or guest identifiers.
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub airplane: Option<bool>,
    /// Android SIM_STATE values; 0 means unknown, 1 absent, 5 ready.
    pub sim: u8,
    /// Android ServiceState: 0 in service, 1 out, 2 emergency, 3 powered off.
    pub cellular: u8,
    pub active_sync: bool,
}
impl Status {
    pub const SIMULATED: Self = Self {
        airplane: Some(false),
        sim: 5,
        cellular: 3,
        active_sync: false,
    };
    pub const UNKNOWN: Self = Self {
        airplane: None,
        sim: 0,
        cellular: 255,
        active_sync: false,
    };
    pub fn from_packed(v: u32) -> Self {
        Self {
            airplane: match v as u8 {
                1 => Some(false),
                2 => Some(true),
                _ => None,
            },
            sim: (v >> 8) as u8,
            cellular: (v >> 16) as u8,
            active_sync: false,
        }
    }
    pub fn shell_bits(self) -> u32 {
        u32::from(self.airplane == Some(true))
            | (u32::from(matches!(self.cellular, 0..=2) && self.airplane != Some(true)) << 2)
            | (u32::from(self.active_sync) << 6)
    }
}
pub trait Backend: Send + Sync {
    fn read(&self) -> Option<Status>;
}
struct Cache {
    backend: Option<Arc<dyn Backend>>,
    sample: Status,
    at: Option<Instant>,
}
fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            backend: None,
            sample: Status::SIMULATED,
            at: None,
        })
    })
}
pub fn install_host(backend: Arc<dyn Backend>) {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    c.backend = Some(backend);
    c.at = None;
}
pub fn status() -> Status {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if c.at.map_or(true, |t| t.elapsed() >= Duration::from_secs(1)) {
        c.sample = match &c.backend {
            Some(b) => b.read().unwrap_or(Status::UNKNOWN),
            None => Status::SIMULATED,
        };
        c.at = Some(Instant::now());
    }
    c.sample
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_absent_emergency_and_airplane_are_distinct() {
        assert_eq!(Status::UNKNOWN.shell_bits(), 0);
        assert_eq!(Status::SIMULATED.shell_bits(), 0);
        let on = Status::from_packed(1 | (5 << 8));
        assert_eq!(on.shell_bits(), 4);
        assert_eq!(on.sim, 5);
        assert_eq!(
            Status::from_packed(2 | (1 << 8) | (3 << 16)).shell_bits(),
            1
        );
        assert_eq!(
            Status::from_packed(1 | (1 << 8) | (2 << 16)).shell_bits(),
            4
        );
        assert!(!Status::from_packed(u32::MAX).active_sync);
    }
    #[test]
    fn snapshots_are_cached_failures_unknown_and_replaced_backends_release() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Probe(AtomicUsize);
        impl Backend for Probe {
            fn read(&self) -> Option<Status> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Some(Status::SIMULATED)
            }
        }
        let probe = Arc::new(Probe(AtomicUsize::new(0)));
        let weak = Arc::downgrade(&probe);
        install_host(probe.clone());
        for _ in 0..100 {
            assert_eq!(status(), Status::SIMULATED);
        }
        assert_eq!(probe.0.load(Ordering::Relaxed), 1);
        drop(probe);
        struct Failed;
        impl Backend for Failed {
            fn read(&self) -> Option<Status> {
                None
            }
        }
        install_host(Arc::new(Failed));
        assert!(weak.upgrade().is_none());
        assert_eq!(status(), Status::UNKNOWN);
        let mut c = cache().lock().unwrap();
        c.backend = None;
        c.sample = Status::SIMULATED;
        c.at = None;
    }
}
