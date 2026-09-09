//! TEMPORARY INSTRUMENTATION: find an API call that never returns.
//!
//! The slice loop's spin watchdog can only report a guest that is
//! looping in its *own* code, because it runs between slices. When a
//! host-side handler blocks instead, the slice loop is blocked with
//! it and never reaches the point where it would print anything --
//! which is exactly the shape of Rayman Ultimate's freeze on entering
//! a level: the runner thread goes quiet mid-call while the UI stays
//! perfectly responsive, so nothing is stuck except one call that
//! never came back.
//!
//! Watching from a separate thread is the only way to see it. The
//! dispatcher records the call it is about to make, clears it on
//! return, and this module's thread reports anything still in flight
//! after a few seconds.
//!
//! Remove once the offending call is identified.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a single call may run before it is considered stuck. Some
/// legitimate calls are slow -- decompressing a level file takes a
/// while -- so this is deliberately far longer than any of those.
const STUCK_AFTER: Duration = Duration::from_secs(3);

struct InFlight {
    label: String,
    since: Instant,
    /// Bumped on every `enter`, so the watcher can tell "the same call
    /// is still running" from "a new call happens to have the same
    /// name".
    seq: u64,
}

static IN_FLIGHT: OnceLock<Mutex<Option<InFlight>>> = OnceLock::new();
static SEQ: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

fn slot() -> &'static Mutex<Option<InFlight>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(None))
}

/// Record that a handler is starting. Cheap enough for the dispatch
/// path: one allocation and one uncontended lock per API call.
pub fn enter(label: String) {
    start_watcher();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut g) = slot().lock() {
        *g = Some(InFlight {
            label,
            since: Instant::now(),
            seq,
        });
    }
}

/// Record that the handler returned.
pub fn leave() {
    if let Ok(mut g) = slot().lock() {
        *g = None;
    }
}

fn start_watcher() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let mut reported_seq: Option<u64> = None;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let Ok(g) = slot().lock() else { continue };
            match g.as_ref() {
                Some(call)
                    if call.since.elapsed() >= STUCK_AFTER && reported_seq != Some(call.seq) =>
                {
                    // Report each stuck call once, not twice a second.
                    reported_seq = Some(call.seq);
                    log::warn!(
                        "call has not returned after {:?}: {}",
                        call.since.elapsed(),
                        call.label
                    );
                }
                _ => {}
            }
        }
    });
}
