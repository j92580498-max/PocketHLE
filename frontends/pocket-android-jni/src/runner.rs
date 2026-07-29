//! Session-based emulator runner exposed to the Android JNI layer.
//!
//! The desktop GUI ([`pocket_desktop::runner`]) drives the emulator
//! on a background thread and streams a [`FrameSnapshot`] to the UI
//! every time the guest produces a new framebuffer. The Android
//! frontend used to do something fundamentally different: a single
//! blocking JNI call (`runGame`) that ran the emulator to
//! completion, captured the **final** framebuffer, and only then
//! returned. With the trace-only stub backend that returned in a
//! few milliseconds and the user just saw a static screenshot. With
//! the real Unicorn backend wired up in
//! [PR #11](https://github.com/j92580498-max/PocketHLE/pull/11) the
//! emulator now actually executes ARM code and reaches the menu, so
//! `runGame` would happily churn through 1024 dispatch slices ×
//! 1 000 000 instructions/slice on the phone CPU before returning —
//! visually that looks identical to a hang ("infinite loading
//! spinner") and there is no way for the user to push input or
//! quit. That's the symptom this module fixes.
//!
//! The new flow mirrors the desktop runner, just over JNI:
//!
//! 1. Kotlin calls [`start`] with the library root and game id. We
//!    spawn a worker thread that owns the [`Emulator`] and runs it
//!    with a [`FrameHook`]. The worker shares a [`SessionState`]
//!    with the UI thread:
//!      * a `Mutex<Option<FrameSnapshot>>` slot holding the most
//!        recent framebuffer — the Kotlin polling loop drains it
//!        with [`poll_frame`];
//!      * an [`InputCommand`] channel — Kotlin pushes touches,
//!        D-pad presses and the "stop" signal with [`send_input`] /
//!        [`request_stop`].
//! 2. When Kotlin's [`finish`] runs (Back button or
//!    `onDestroy`), we set `should_stop`, join the worker thread
//!    and return a textual summary that the UI shows in its status
//!    panel.
//!
//! Sessions are owned by Kotlin via a `jlong` handle. The handle is
//! a `Box::into_raw`'d pointer to a [`Session`]; [`finish`]
//! reconstructs the box and drops it. The pointer is opaque to
//! Kotlin and, crucially, the JNI methods bounds-check it against
//! `null` and the dispatch refuses to operate on a freed session
//! (we set the in-flight `running` flag to `false` once the worker
//! exits, which lets the polling loop on the UI thread notice the
//! session ended and stop calling back in).

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use pocket_core::kernel::{FrameAction, FrameHook, InputEvent, KernelState};
use pocket_core::Emulator;
use pocket_library::{CpuBackendPref, GameEntry, Library};

const FRAME_PUSH_INTERVAL: Duration = Duration::from_millis(16);

/// Snapshot of the guest framebuffer plus the dimensions Kotlin
/// needs to paint it onto a `SurfaceView`.
#[derive(Debug, Clone)]
pub struct FrameSnapshot {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl FrameSnapshot {
    fn from_framebuffer(fb: &pocket_core::kernel::Framebuffer) -> Self {
        Self {
            width: fb.width,
            height: fb.height,
            rgba: fb.snapshot_rgba8888(),
        }
    }

    fn from_framebuffer_into(fb: &pocket_core::kernel::Framebuffer, scratch: &mut Vec<u8>) -> Self {
        fb.snapshot_rgba8888_into(scratch);
        Self {
            width: fb.width,
            height: fb.height,
            rgba: std::mem::take(scratch),
        }
    }
}

/// Kotlin → emulator command. Mirrors `pocket_desktop::runner::InputCommand`.
#[derive(Debug, Clone, Copy)]
pub enum InputCommand {
    Input(InputEvent),
    Stop,
}

/// Shared between the worker thread and the UI thread for the
/// lifetime of one game session.
struct SessionState {
    /// Latest framebuffer the guest produced. The polling loop on
    /// the UI thread drains this slot and paints it; a write
    /// overwrites whatever was there because the UI only ever
    /// cares about the newest frame.
    latest_frame: Mutex<Option<FrameSnapshot>>,
    /// `true` while the worker thread is still running. Flipped to
    /// `false` exactly once, just before the worker returns.
    running: Mutex<bool>,
    /// Final summary string. Populated by the worker right before
    /// it exits; read by [`finish`] after the join.
    summary: Mutex<Option<String>>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            latest_frame: Mutex::new(None),
            running: Mutex::new(true),
            summary: Mutex::new(None),
        }
    }
}

/// Owned by Kotlin via a `Box::into_raw`'d pointer.
pub struct Session {
    state: Arc<SessionState>,
    input_tx: Sender<InputCommand>,
    worker: Option<JoinHandle<()>>,
}

impl Session {
    /// Move the latest framebuffer out of the shared slot.
    pub fn poll_frame(&self) -> Option<FrameSnapshot> {
        self.state
            .latest_frame
            .lock()
            .ok()
            .and_then(|mut g| g.take())
    }

    pub fn send_input(&self, cmd: InputCommand) {
        // The receiver only goes away after the worker exits, in
        // which case we don't care about the input anymore.
        let _ = self.input_tx.send(cmd);
    }

    pub fn request_stop(&self) {
        self.send_input(InputCommand::Stop);
    }

    pub fn is_running(&self) -> bool {
        self.state.running.lock().map(|g| *g).unwrap_or(false)
    }

    /// Join the worker thread (with a stop signal already sent) and
    /// return the textual summary captured while it was running.
    pub fn finish(mut self) -> String {
        // Belt-and-braces: ask the worker to stop in case the
        // caller forgot to.
        let _ = self.input_tx.send(InputCommand::Stop);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        self.state
            .summary
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "(no summary captured)".to_string())
    }
}

/// Spawn the worker thread that drives the emulator for a single
/// game. The returned `Session` is the handle Kotlin holds.
pub fn start(library_root: PathBuf, game_id: String) -> anyhow::Result<Session> {
    let lib = Library::open(&library_root).context("Library::open")?;
    let entry = lib
        .get(&game_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown game id {game_id}"))?;

    let state = Arc::new(SessionState::new());
    let (input_tx, input_rx) = channel::<InputCommand>();

    let state_for_worker = Arc::clone(&state);
    let worker = std::thread::Builder::new()
        .name(format!("pockethle-emu-{game_id}"))
        .spawn(move || {
            let summary =
                run_game_to_completion(&library_root, &entry, &state_for_worker, input_rx);
            if let Ok(mut slot) = state_for_worker.summary.lock() {
                *slot = Some(summary);
            }
            if let Ok(mut running) = state_for_worker.running.lock() {
                *running = false;
            }
        })
        .context("spawn pockethle worker thread")?;

    Ok(Session {
        state,
        input_tx,
        worker: Some(worker),
    })
}

/// Runs the emulator from start to finish, returning a summary
/// suitable for the UI's status panel. Streams framebuffers and
/// drains UI input via [`SessionHook`].
fn run_game_to_completion(
    library_root: &std::path::Path,
    entry: &GameEntry,
    state: &Arc<SessionState>,
    input_rx: Receiver<InputCommand>,
) -> String {
    let mut summary_lines = vec![
        format!("Game: {}", entry.display_name),
        format!("Backend: {}", entry.settings.cpu_backend.label()),
    ];
    let exe = entry.executable_path(library_root);
    let machine = pocket_core::pe::load_file(&exe)
        .map(|image| image.machine)
        .unwrap_or(pocket_core::pe::machine::ARM);
    summary_lines.push(format!("Executable: {}", exe.display()));

    // Same Stub→Unicorn promotion logic as `pocket_desktop::runner`:
    // a user who clicks "Run" wants the real ARM core regardless of
    // what is persisted in their library.json.
    let requested_backend = entry.settings.cpu_backend;
    let mut effective_backend = requested_backend;
    let mut emu = match requested_backend {
        CpuBackendPref::Unicorn => match build_unicorn_for_machine(machine) {
            Ok(emu) => emu,
            Err(e) => {
                summary_lines.push(format!("Unicorn unavailable, falling back to stub: {e}"));
                effective_backend = CpuBackendPref::Stub;
                Emulator::with_stub_cpu()
            }
        },
        CpuBackendPref::Stub => match build_unicorn_for_machine(machine) {
            Ok(emu) => {
                summary_lines.push(
                    "Saved CPU backend was Stub (trace-only); promoting to \
                     Unicorn so the game can actually execute."
                        .to_string(),
                );
                effective_backend = CpuBackendPref::Unicorn;
                emu
            }
            Err(_) => Emulator::with_stub_cpu(),
        },
    };
    summary_lines.push(format!("Effective backend: {}", effective_backend.label()));

    emu.set_halt_on_unimplemented(entry.settings.halt_on_unimplemented);
    emu.max_slices = entry.settings.max_slices;
    emu.instruction_budget_per_slice = entry.settings.instructions_per_slice;

    if let Err(e) = emu.load_pe(&exe) {
        summary_lines.push(format!("load_pe failed: {e:#}"));
        return summary_lines.join("\n");
    }
    let extracted = entry.extracted_dir(library_root);
    emu.mount_dir("\\Application\\", &extracted);
    emu.mount_dir("\\Program Files\\", &extracted);
    emu.mount_dir("\\Program Files\\Game\\", &extracted);
    if let Some(install_dir) = &entry.install_dir {
        emu.mount_dir(install_dir, &extracted);
    }
    // Match the desktop GUI: a real user is in the loop, so don't
    // auto-fire WM_QUIT after a fixed number of synthetic messages.
    emu.set_synthetic_message_budget(0);

    let mut hook = SessionHook::new(Arc::clone(state), input_rx);
    let run_result = emu.run_with_hook(&mut hook);
    match run_result {
        Ok(()) => summary_lines.push("Emulator exited cleanly.".to_string()),
        Err(e) => summary_lines.push(format!("Emulator stopped: {e:#}")),
    }

    // Push one last framebuffer so the UI ends up showing whatever
    // the guest left on screen even if it stopped between frames.
    if let Some(p) = emu.process() {
        push_frame(state, FrameSnapshot::from_framebuffer(&p.state.framebuffer));
    }

    summary_lines.join("\n")
}

#[cfg(feature = "unicorn")]
fn build_unicorn_for_machine(machine: u16) -> anyhow::Result<Emulator> {
    if matches!(
        machine,
        pocket_core::pe::machine::MIPS_R3000 | pocket_core::pe::machine::MIPS_R4000
    ) {
        Emulator::with_unicorn_cpu_for_arch(pocket_core::cpu::Arch::Mips)
    } else {
        Emulator::with_unicorn_cpu()
    }
}

#[cfg(not(feature = "unicorn"))]
fn build_unicorn_for_machine(_machine: u16) -> anyhow::Result<Emulator> {
    Err(anyhow::anyhow!(
        "binary was not compiled with the `unicorn` feature"
    ))
}

fn push_frame(state: &Arc<SessionState>, frame: FrameSnapshot) {
    if let Ok(mut slot) = state.latest_frame.lock() {
        *slot = Some(frame);
    }
}

/// Bridges the UI thread (Kotlin) and the running emulator on the
/// worker thread.
struct SessionHook {
    state: Arc<SessionState>,
    input_rx: Receiver<InputCommand>,
    last_frame: u64,
    input_disconnected: bool,
    last_emit_at: Option<Instant>,
    scratch: Vec<u8>,
}

impl SessionHook {
    fn new(state: Arc<SessionState>, input_rx: Receiver<InputCommand>) -> Self {
        Self {
            state,
            input_rx,
            last_frame: 0,
            input_disconnected: false,
            last_emit_at: None,
            scratch: Vec::new(),
        }
    }
}

impl FrameHook for SessionHook {
    fn on_frame(&mut self, kernel: &mut KernelState) -> FrameAction {
        // Drain any pending UI input into the kernel's queue.
        let mut stop_requested = false;
        if !self.input_disconnected {
            loop {
                match self.input_rx.try_recv() {
                    Ok(InputCommand::Input(ev)) => kernel.pending_input.push_back(ev),
                    Ok(InputCommand::Stop) => stop_requested = true,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.input_disconnected = true;
                        break;
                    }
                }
            }
        }

        // Stream a fresh framebuffer if the guest produced one.
        let counter = kernel.framebuffer.frame_counter;
        if counter != self.last_frame {
            let now = Instant::now();
            let due = self
                .last_emit_at
                .map(|t| now.duration_since(t) >= FRAME_PUSH_INTERVAL)
                .unwrap_or(true);
            if due {
                self.last_frame = counter;
                self.last_emit_at = Some(now);
                let frame =
                    FrameSnapshot::from_framebuffer_into(&kernel.framebuffer, &mut self.scratch);
                push_frame(&self.state, frame);
            }
        }

        if stop_requested {
            kernel.should_stop = true;
            FrameAction::Stop
        } else {
            FrameAction::Continue
        }
    }
}
