//! Reusable host-time slice timer. Unicorn's built-in timeout creates and joins
//! a native thread on every emu_start, including very short API-bound slices.
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    enabled: bool,
    deadline: Option<Instant>,
    shutdown: bool,
}

pub(crate) struct SliceTimer {
    state: Arc<(Mutex<State>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl SliceTimer {
    pub(crate) fn new(handle: usize) -> std::io::Result<Self> {
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let shared = state.clone();
        let worker = std::thread::Builder::new()
            .name("pockethle-cpu-slice".into())
            .spawn(move || {
                let (mutex, wake) = &*shared;
                let mut state = mutex.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if state.shutdown {
                        break;
                    }
                    if !state.enabled {
                        state = wake.wait(state).unwrap_or_else(|e| e.into_inner());
                        continue;
                    }
                    if state
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        // SAFETY: this thread only requests emulation stop, as
                        // Unicorn's own timeout thread does. The owner disarms
                        // under this mutex before any further CPU access, and
                        // joins this thread before releasing the engine handle.
                        // Retry until disarmed: the host may have been delayed
                        // between arming and entering emu_start, which resets
                        // Unicorn's stop flag at entry.
                        unsafe {
                            let _ = unicorn_engine::uc_emu_stop(handle as *mut _);
                        }
                    }
                    state = wake
                        .wait_timeout(state, Duration::from_millis(1))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        let (mutex, wake) = &*self.state;
        let mut state = mutex.lock().unwrap_or_else(|e| e.into_inner());
        let changed = state.enabled != enabled;
        state.enabled = enabled;
        state.deadline = None;
        if changed {
            wake.notify_one();
        }
    }

    pub(crate) fn arm(&self, timeout_us: u64) {
        let mut state = self.state.0.lock().unwrap_or_else(|e| e.into_inner());
        state.deadline = Some(Instant::now() + Duration::from_micros(timeout_us));
    }

    pub(crate) fn disarm(&self) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .deadline = None;
    }
}

impl Drop for SliceTimer {
    fn drop(&mut self) {
        let (mutex, wake) = &*self.state;
        {
            let mut state = mutex.lock().unwrap_or_else(|e| e.into_inner());
            state.shutdown = true;
            state.deadline = None;
            wake.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
