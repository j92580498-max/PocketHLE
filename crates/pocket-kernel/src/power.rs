//! Host battery projection. No timers, device handles or subscriptions.
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub ac: u8,
    pub flags: u8,
    pub percent: u8,
    pub remaining: u32,
    pub full: u32,
}
impl Status {
    fn with_guest_thresholds(mut self) -> Self {
        if self.percent <= 100 && (self.flags == 255 || self.flags & 128 == 0) {
            // Host LOW/CRITICAL thresholds differ. Use the same guest levels
            // on Windows and Android, preserving charging and other known bits.
            let other = if self.flags == 255 {
                0
            } else {
                self.flags & !7
            };
            self.flags = other
                | match self.percent {
                    0..=5 => 4,
                    6..=20 => 2,
                    67..=100 => 1,
                    _ => 0,
                };
        }
        self
    }
    pub const UNKNOWN: Self = Self {
        ac: 255,
        flags: 255,
        percent: 255,
        remaining: u32::MAX,
        full: u32::MAX,
    };
    pub fn ce_bytes(self) -> [u8; 56] {
        let mut b = [0; 56];
        b[0] = self.ac;
        b[1] = self.flags;
        b[2] = self.percent;
        b[4..8].copy_from_slice(&self.remaining.to_le_bytes());
        b[8..12].copy_from_slice(&self.full.to_le_bytes());
        b[13] = 128;
        b[14] = 255;
        b[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        b[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        b
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
            sample: Status::UNKNOWN,
            at: None,
        })
    })
}
pub fn install_host(backend: Arc<dyn Backend>) {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    c.backend = Some(backend);
    c.at = None;
    c.sample = Status::UNKNOWN;
}
pub fn status(refresh: bool) -> Status {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if refresh || c.at.map_or(true, |t| t.elapsed() >= Duration::from_secs(1)) {
        c.sample = c
            .backend
            .as_ref()
            .map_or_else(native, |b| b.read())
            .map(Status::with_guest_thresholds)
            .unwrap_or(Status::UNKNOWN);
        c.at = Some(Instant::now());
    }
    c.sample
}
#[cfg(windows)]
fn native() -> Option<Status> {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
        return None;
    }
    Some(Status {
        ac: s.ACLineStatus,
        flags: s.BatteryFlag,
        percent: s.BatteryLifePercent,
        remaining: s.BatteryLifeTime,
        full: s.BatteryFullLifeTime,
    })
}
#[cfg(not(windows))]
fn native() -> Option<Status> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ce_layout() {
        let s = Status {
            ac: 0,
            flags: 4,
            percent: 3,
            remaining: 120,
            full: 3600,
        };
        let b = s.ce_bytes();
        assert_eq!(&b[..4], &[0, 4, 3, 0]);
        assert_eq!(&b[4..8], &120u32.to_le_bytes());
        assert_eq!(&b[8..12], &3600u32.to_le_bytes());
        assert_eq!(&b[13..15], &[128, 255]);
        assert_eq!(&b[16..24], &[255; 8]);
        assert_eq!(&b[24..], &[0; 32]);
    }
    #[test]
    fn zero_percent_is_valid() {
        assert_eq!(
            Status {
                percent: 0,
                ..Status::UNKNOWN
            }
            .ce_bytes()[2],
            0
        );
        assert_eq!(Status::UNKNOWN.ce_bytes()[2], 255);
    }
    #[test]
    fn host_levels_are_normalized_at_guest_boundaries() {
        for (percent, level) in [
            (0, 4),
            (5, 4),
            (6, 2),
            (20, 2),
            (21, 0),
            (33, 0),
            (66, 0),
            (67, 1),
            (100, 1),
        ] {
            for host_flags in [0, 1, 2, 4, 8, 10, 12, 255] {
                let input = Status {
                    percent,
                    flags: host_flags,
                    ..Status::UNKNOWN
                };
                let result = input.with_guest_thresholds();
                let charging = if host_flags == 255 { 0 } else { host_flags & 8 };
                assert_eq!(result.flags, level | charging);
                assert_eq!(result.percent, percent);
                assert_eq!(result.ce_bytes()[1], level | charging);
            }
        }
        assert_eq!(Status::UNKNOWN.with_guest_thresholds(), Status::UNKNOWN);
        let absent = Status {
            flags: 128,
            percent: 0,
            ..Status::UNKNOWN
        };
        assert_eq!(absent.with_guest_thresholds(), absent);
    }
    #[test]
    fn cache_and_forced_refresh() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Fake(AtomicUsize);
        impl Backend for Fake {
            fn read(&self) -> Option<Status> {
                Some(Status {
                    percent: self.0.fetch_add(1, Ordering::SeqCst) as u8,
                    ..Status::UNKNOWN
                })
            }
        }
        let f = Arc::new(Fake(AtomicUsize::new(0)));
        install_host(f.clone());
        assert_eq!(status(false).percent, 0);
        assert_eq!(status(false).percent, 0);
        assert_eq!(status(true).percent, 1);
        assert_eq!(f.0.load(Ordering::SeqCst), 2);
        struct Failed;
        impl Backend for Failed {
            fn read(&self) -> Option<Status> {
                None
            }
        }
        install_host(Arc::new(Failed));
        assert_eq!(status(false), Status::UNKNOWN);
        assert_eq!(Arc::strong_count(&f), 1); // Replaced backend is released.
    }
}

/// CE power-manager notifications (pm.h), not desktop WM_POWERBROADCAST IDs.
pub const PBT_POWERSTATUSCHANGE: u32 = 4;
pub const PBT_POWERINFOCHANGE: u32 = 8;
#[derive(Default)]
pub struct Notifications {
    subscriptions: std::collections::HashMap<u32, Subscription>,
    next: u32,
}
struct Subscription {
    queue: u32,
    flags: u32,
    info: Status,
    ac: u8,
}
impl Notifications {
    pub fn register(&mut self, queue: u32, flags: u32, current: Status) -> Result<u32, u32> {
        // Bound registrations and never recycle handles during this process.
        if self.subscriptions.len() >= 1024 {
            return Err(8);
        }
        let next = self
            .next
            .checked_add(1)
            .filter(|n| *n < 0x100000)
            .ok_or(8u32)?;
        self.next = next;
        let handle = 0xda700000 + next;
        self.subscriptions.insert(
            handle,
            Subscription {
                queue,
                flags,
                info: current,
                ac: current.ac,
            },
        );
        Ok(handle)
    }
    pub fn stop(&mut self, handle: u32) -> bool {
        self.subscriptions.remove(&handle).is_some()
    }
    pub fn close_queue(&mut self, queue: u32) {
        self.subscriptions.retain(|_, s| s.queue != queue);
    }
    pub fn is_empty(&self) -> bool {
        self.subscriptions.is_empty()
    }
    pub fn deliver(
        &mut self,
        queues: &mut std::collections::HashMap<u32, crate::MsgQueue>,
        current: Status,
    ) {
        self.subscriptions.retain(|_, s| {
            let Some(q) = queues.get_mut(&s.queue) else {
                return false;
            };
            // Keep undelivered changes pending when a bounded guest queue is full.
            if s.ac != current.ac {
                if s.flags & PBT_POWERSTATUSCHANGE == 0
                    || enqueue(q, broadcast(PBT_POWERSTATUSCHANGE, current))
                {
                    s.ac = current.ac;
                }
            }
            if s.info != current {
                if s.flags & PBT_POWERINFOCHANGE == 0
                    || enqueue(q, broadcast(PBT_POWERINFOCHANGE, current))
                {
                    s.info = current;
                }
            }
            true
        });
    }
}
fn enqueue(queue: &mut crate::MsgQueue, bytes: Vec<u8>) -> bool {
    if bytes.len() > queue.max_message_size as usize
        || queue.messages.len() >= queue.max_messages as usize
    {
        return false;
    }
    queue.messages.push_back(bytes);
    true
}
fn broadcast(message: u32, status: Status) -> Vec<u8> {
    // POWER_BROADCAST payload starts at byte 12; POWER_INFO is 24 bytes
    // on CE ARM (4 DWORDs, 5 BYTEs, 3 bytes tail padding).
    let info = message == PBT_POWERINFOCHANGE;
    let mut bytes = vec![0; if info { 36 } else { 16 }];
    bytes[..4].copy_from_slice(&message.to_le_bytes());
    if info {
        bytes[8..12].copy_from_slice(&24u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&status.remaining.to_le_bytes());
        bytes[16..20].copy_from_slice(&status.full.to_le_bytes());
        bytes[20..28].fill(255);
        bytes[28..33].copy_from_slice(&[status.ac, status.flags, status.percent, 128, 255]);
    }
    bytes
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    fn queues(capacity: u32) -> HashMap<u32, crate::MsgQueue> {
        HashMap::from([(
            1,
            crate::MsgQueue {
                max_messages: capacity,
                max_message_size: 36,
                read_access: true,
                messages: VecDeque::new(),
            },
        )])
    }
    fn sample(percent: u8, ac: u8) -> Status {
        Status {
            percent,
            ac,
            flags: if percent <= 5 { 4 } else { 1 },
            ..Status::UNKNOWN
        }
    }
    #[test]
    fn change_filter_and_binary_payload() {
        let mut n = Notifications::default();
        let mut q = queues(4);
        n.register(1, 8, sample(50, 0)).unwrap();
        n.deliver(&mut q, sample(50, 0));
        assert!(q[&1].messages.is_empty());
        n.deliver(&mut q, sample(3, 1));
        let b = q.get_mut(&1).unwrap().messages.pop_front().unwrap();
        assert_eq!(b.len(), 36);
        assert_eq!(&b[..4], &8u32.to_le_bytes());
        assert_eq!(&b[8..12], &24u32.to_le_bytes());
        assert_eq!(&b[28..33], &[1, 4, 3, 128, 255]);
        n.deliver(&mut q, sample(3, 1));
        assert!(q[&1].messages.is_empty());
    }
    #[test]
    fn full_queue_retries_both_events_and_stop_releases() {
        let mut n = Notifications::default();
        let mut q = queues(1);
        let h = n.register(1, u32::MAX, sample(80, 0)).unwrap();
        n.deliver(&mut q, sample(4, 1));
        assert_eq!(q[&1].messages.len(), 1);
        assert_eq!(
            &q.get_mut(&1).unwrap().messages.pop_front().unwrap()[..4],
            &4u32.to_le_bytes()
        );
        n.deliver(&mut q, sample(4, 1));
        assert_eq!(
            &q.get_mut(&1).unwrap().messages.pop_front().unwrap()[..4],
            &8u32.to_le_bytes()
        );
        assert!(n.stop(h));
        assert!(!n.stop(h));
        n.deliver(&mut q, sample(2, 0));
        assert!(q[&1].messages.is_empty());
    }
    #[test]
    fn closed_queue_and_process_state_release_subscriptions() {
        let mut n = Notifications::default();
        let h = n.register(1, 8, sample(80, 0)).unwrap();
        n.close_queue(1);
        assert!(n.is_empty());
        assert!(!n.stop(h));
        n.register(1, 8, sample(80, 0)).unwrap();
        n.deliver(&mut HashMap::new(), sample(3, 0));
        assert!(n.is_empty());
    }
    #[test]
    fn supply_only_filter_ignores_percent() {
        let mut n = Notifications::default();
        let mut q = queues(3);
        n.register(1, 4, sample(80, 0)).unwrap();
        n.deliver(&mut q, sample(3, 0));
        assert!(q[&1].messages.is_empty());
        n.deliver(&mut q, sample(3, 1));
        assert_eq!(q[&1].messages.len(), 1);
    }
}

/// SDK SysMessages uses BATERY (one T); the guide also spells BATTERY.
/// Registered message notifications need no payload: the SDK ignores both params.
#[derive(Default)]
pub struct GizmondoAlerts {
    low: Option<u32>,
    critical: Option<u32>,
    low_last: Option<u8>,
    critical_last: Option<u8>,
}
impl GizmondoAlerts {
    pub fn register(&mut self, name: &str, id: u32) {
        match name {
            "LOWBATERY_MSG" | "LOWBATTERY_MSG" => {
                self.low.get_or_insert(id);
            }
            "CRITICALBATERY_MSG" | "CRITICALBATTERY_MSG" => {
                self.critical.get_or_insert(id);
            }
            _ => {}
        }
    }
    pub fn is_empty(&self) -> bool {
        self.low.is_none() && self.critical.is_none()
    }
    pub fn deliver(
        &mut self,
        queue: &mut std::collections::VecDeque<(u32, u32, u32, u32)>,
        current: Status,
    ) {
        let Some(level) = severity(current) else {
            return;
        };
        for (id, last, target) in [
            (self.low, &mut self.low_last, 1),
            (self.critical, &mut self.critical_last, 2),
        ] {
            let Some(id) = id else {
                continue;
            };
            if level == target && last.map_or(true, |old| level > old) {
                if queue.len() >= 256 {
                    continue;
                }
                queue.push_back((0xffff, id, 0, 0));
            }
            *last = Some(level);
        }
    }
}
fn severity(s: Status) -> Option<u8> {
    if s.ac == 1 || (s.flags != 255 && s.flags & 128 != 0) {
        return Some(0);
    }
    if s.flags != 255 && s.flags & 8 != 0 {
        return Some(0);
    }
    // A known percentage takes precedence over the host's own alert thresholds.
    if s.percent <= 100 {
        return Some(match s.percent {
            0..=5 => 2,
            6..=20 => 1,
            _ => 0,
        });
    }
    if s.flags != 255 {
        if s.flags & 4 != 0 {
            return Some(2);
        }
        if s.flags & 2 != 0 {
            return Some(1);
        }
    }
    None
}
#[cfg(test)]
mod gizmondo_alert_tests {
    use super::*;
    use std::collections::VecDeque;
    fn s(p: u8, ac: u8) -> Status {
        Status {
            percent: p,
            ac,
            flags: 0,
            ..Status::UNKNOWN
        }
    }
    #[test]
    fn known_percent_overrides_host_low_and_critical_flags() {
        for flags in [0, 2, 4] {
            for (percent, expected) in [(0, 2), (5, 2), (6, 1), (20, 1), (21, 0), (33, 0), (100, 0)]
            {
                assert_eq!(
                    severity(Status {
                        flags,
                        ..s(percent, 0)
                    }),
                    Some(expected)
                );
            }
        }
        assert_eq!(
            severity(Status {
                flags: 2,
                ..s(255, 0)
            }),
            Some(1)
        );
        assert_eq!(
            severity(Status {
                flags: 4,
                ..s(255, 0)
            }),
            Some(2)
        );
        assert_eq!(
            severity(Status {
                flags: 8,
                ..s(4, 0)
            }),
            Some(0)
        );
        assert_eq!(severity(s(4, 1)), Some(0));
    }
    #[test]
    fn sdk_names_thresholds_rearm_and_no_spam() {
        let mut a = GizmondoAlerts::default();
        let mut q = VecDeque::new();
        a.register("LOWBATERY_MSG", 0xc111);
        a.register("CRITICALBATERY_MSG", 0xc222);
        a.deliver(&mut q, s(80, 0));
        a.deliver(&mut q, s(20, 0));
        assert_eq!(q.pop_front(), Some((0xffff, 0xc111, 0, 0)));
        a.deliver(&mut q, s(19, 0));
        assert!(q.is_empty());
        a.deliver(&mut q, s(3, 0));
        assert_eq!(q.pop_front(), Some((0xffff, 0xc222, 0, 0)));
        a.deliver(&mut q, s(3, 0));
        a.deliver(&mut q, Status::UNKNOWN);
        a.deliver(&mut q, s(3, 0));
        assert!(q.is_empty());
        a.deliver(&mut q, s(3, 1));
        a.deliver(&mut q, s(3, 0));
        assert_eq!(q.pop_front(), Some((0xffff, 0xc222, 0, 0)));
    }
    #[test]
    fn startup_low_and_queue_full_retry() {
        let mut a = GizmondoAlerts::default();
        let mut q = VecDeque::from(vec![(0, 0, 0, 0); 256]);
        a.register("LOWBATTERY_MSG", 0xc111);
        a.deliver(&mut q, s(20, 0));
        assert_eq!(q.len(), 256);
        q.clear();
        a.deliver(&mut q, s(20, 0));
        assert_eq!(q.pop_front(), Some((0xffff, 0xc111, 0, 0)));
        a.register("LOWBATERY_MSG", 0xc111);
        a.deliver(&mut q, s(20, 0));
        assert!(q.is_empty());
    }
    #[test]
    fn healthy_and_no_battery_emit_no_alert() {
        let mut a = GizmondoAlerts::default();
        let mut q = VecDeque::new();
        a.register("LOWBATERY_MSG", 1);
        a.register("CRITICALBATERY_MSG", 2);
        a.deliver(
            &mut q,
            Status {
                flags: 128,
                ..s(0, 0)
            },
        );
        a.deliver(&mut q, Status::UNKNOWN);
        a.deliver(&mut q, s(80, 0));
        assert!(q.is_empty());
        a.deliver(&mut q, s(4, 0));
        assert_eq!(q.pop_front(), Some((0xffff, 2, 0, 0)));
        a.deliver(&mut q, s(20, 0));
        assert!(q.is_empty()); // Recovery is not a new low alert.
    }
}
