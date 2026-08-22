//! Background runner that drives [`pocket_core::Emulator`] for the
//! desktop GUI.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pocket_core::kernel::{FrameAction, FrameHook, InputEvent, KernelState};
use pocket_core::Emulator;
use pocket_library::{is_gizmondo_game, CpuBackendPref, GameEntry};

/// Minimum wall-clock interval between two `FrameSnapshot`s pushed
/// from the runner thread to the GUI thread. The frame hook fires
/// after every dispatched WinCE API call (potentially thousands of
/// times per logical game frame, since every `FillRect` / `BitBlt`
/// bumps `frame_counter`); without throttling we would generate and
/// queue a fresh 300 KiB RGBA snapshot for every one of those calls,
/// which is exactly the per-frame cost the desktop launcher used to
/// drown in. ~60 fps is plenty for a 320×240 LCD preview.
const FRAME_PUSH_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, Default)]
pub struct Runner {
    inner: Arc<Mutex<()>>,
}

impl Runner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn run_game(
        &self,
        library_root: PathBuf,
        game: GameEntry,
        live_tx: Option<Sender<FrameSnapshot>>,
        input_rx: Option<Receiver<InputCommand>>,
    ) -> RunOutcome {
        let _guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let exe = game.launch_path(&library_root);
        let mut summary_lines = vec![format!("Game: {}", game.display_name)];

        let machine = pocket_core::pe::load_file(&exe)
            .map(|image| image.machine)
            .unwrap_or(pocket_core::pe::machine::ARM);
        // The Stub CPU does not interpret instructions — it is a
        // trace-only harness that exists so loader-level code can be
        // unit-tested without pulling in unicorn-engine. Trying to
        // actually `run` a game on it is guaranteed to crash with
        // "guest jumped to unmapped address 0x00000000" because the
        // stub never sets LR while pretending to call a guest
        // function. End users who click "Run" in the GUI always
        // want the real ARM core, regardless of what is persisted in
        // their `library.json` — so when a Stub-backed game is
        // launched and unicorn is compiled in, we silently promote
        // the run to Unicorn. This is defense-in-depth on top of
        // [`pocket_library::Library::migrate_legacy_entries`], which
        // covers users who downgrade or who have a stale
        // `library.json` from before that migration existed.
        let requested_backend = game.settings.cpu_backend;
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
        summary_lines.push(format!("Backend: {}", effective_backend.label()));
        summary_lines.push(format!("Executable: {}", exe.display()));

        emu.set_halt_on_unimplemented(game.settings.halt_on_unimplemented);
        emu.max_slices = game.settings.max_slices;
        emu.instruction_budget_per_slice = game.settings.instructions_per_slice;

        if let Err(e) = emu.load_pe(&exe) {
            summary_lines.push(format!("load_pe failed: {e:#}"));
            return RunOutcome {
                summary: summary_lines.join("\n"),
                framebuffer: None,
            };
        }

        // The screen has to be sized before the game runs: a GAPI title
        // reads the display geometry once during start-up and lays its
        // HUD out around it. `set_screen_size` requires `load_pe` to have
        // created the process first, so this call must come after the
        // load succeeds.
        let (screen_w, screen_h) = game.settings.screen.size();
        emu.set_screen_size(screen_w, screen_h);
        summary_lines.push(format!("Screen: {screen_w}x{screen_h}"));

        for value in &game.registry {
            let registry_value = if let Some(text) = value.string.as_deref() {
                pocket_core::kernel::registry::RegistryValue::Sz(text.to_string())
            } else if let Some(number) = value.dword {
                pocket_core::kernel::registry::RegistryValue::Dword(number)
            } else {
                continue;
            };
            emu.set_registry_value(&value.key, &value.name, registry_value);
        }
        if !game
            .registry
            .iter()
            .any(|value| value.name.eq_ignore_ascii_case("SaveDir"))
        {
            if let Some(install_dir) = game
                .registry
                .iter()
                .find(|value| value.name.eq_ignore_ascii_case("InstallDir"))
                .and_then(|value| value.string.clone())
                .or_else(|| game.install_dir.clone())
            {
                emu.set_registry_value(
                    r"HKLM\SOFTWARE\Apps\Astraware Cubis",
                    "SaveDir",
                    pocket_core::kernel::registry::RegistryValue::Sz(install_dir),
                );
            }
        }

        let extracted = game.extracted_dir(&library_root);
        emu.mount_read_only_dir("\\Application\\", &extracted);
        emu.mount_read_only_dir("\\Program Files\\", &extracted);
        emu.mount_read_only_dir("\\Program Files\\Game\\", &extracted);
        emu.mount_read_only_dir("\\expresso\\", &extracted);
        let is_gizmondo = is_gizmondo_game(&game, &library_root);
        if is_gizmondo {
            emu.mount_read_only_dir("\\SD Card\\", &extracted);
            emu.mount_read_only_dir("\\Storage Card\\", &extracted);
            emu.mount_read_only_dir("\\SD Card\\Game\\", &extracted);
            emu.mount_read_only_dir("\\Storage Card\\Game\\", &extracted);
            summary_lines.push(format!(
                "Mounted Gizmondo SD-card layout at {}",
                extracted.display()
            ));
            if let Some(name) = game.executable.file_name().and_then(|name| name.to_str()) {
                let guest_exe = format!("\\SD Card\\{name}");
                emu.set_module_path(&guest_exe);
                emu.set_default_dir("\\SD Card\\");
                summary_lines.push(format!("Module path: {guest_exe}"));
            }
        }
        // A game that keeps its assets in one archive opens it by
        // absolute path built from its own module path -- Spore Origins
        // asks for `\Program Files\EA\Spore v1.0.4\data.vfs`. Mount the
        // extracted directory where the cabinet said it would be
        // installed and report that path from `GetModuleFileNameW`,
        // the way the CLI already does for a cab. Without it the
        // archive never opens and the game calls through a pointer it
        // never stored, which surfaces as READ_UNMAPPED at 0x2.
        for prefix in &game.install_dirs {
            emu.mount_read_only_dir(prefix, &extracted);
        }
        if let Some(prefix) = game.guest_install_prefix() {
            emu.mount_read_only_dir(&prefix, &extracted);
            summary_lines.push(format!("Mounted {} at {prefix:?}", extracted.display()));
            if let Some(guest_exe) = game.guest_exe_path() {
                emu.set_module_path(&guest_exe);
                emu.set_default_dir(&prefix);
                summary_lines.push(format!("Module path: {guest_exe}"));
            }
            if let Some(save_prefix) = game.guest_save_prefix() {
                let save_dir = game.save_dir(&library_root);
                emu.mount_save_dir(&save_prefix, &save_dir);
                summary_lines.push(format!(
                    "Save data: {} -> {save_prefix:?}",
                    save_dir.display()
                ));
            }
        }
        // GUI users actually play the game, so don't auto-fire
        // `WM_QUIT` after a fixed number of synthetic messages —
        // budget=0 means "run until the user stops or the game
        // calls ExitProcess". Real input from the virtual D-pad /
        // stylus tap arrives through `input_rx` and feeds the
        // synthetic message pump in pocket-winceapi.
        emu.set_synthetic_message_budget(0);

        emu.start_audio();
        let run_result = {
            let mut hook = RunHook::new(live_tx, input_rx);
            emu.run_with_hook(&mut hook)
        };
        match run_result {
            Ok(()) => summary_lines.push("Emulator exited cleanly.".to_string()),
            Err(e) => summary_lines.push(format!("Emulator stopped: {e:#}")),
        }
        emu.stop_audio();

        let framebuffer = emu.process().and_then(|p| {
            (!p.state.framebuffer.is_all_black())
                .then(|| FrameSnapshot::from_framebuffer(&p.state.framebuffer))
        });
        RunOutcome {
            summary: summary_lines.join("\n"),
            framebuffer,
        }
    }
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

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub summary: String,
    pub framebuffer: Option<FrameSnapshot>,
}

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

    /// Like [`FrameSnapshot::from_framebuffer`] but reuses an existing
    /// `rgba` buffer the caller owns. Saves the per-frame 300 KiB
    /// allocation that the naive constructor does.
    fn fill_from_framebuffer(fb: &pocket_core::kernel::Framebuffer, scratch: &mut Vec<u8>) -> Self {
        fb.snapshot_rgba8888_into(scratch);
        Self {
            width: fb.width,
            height: fb.height,
            // `std::mem::take` hands ownership of the scratch buffer
            // to the snapshot we are about to ship across the
            // channel and leaves an empty `Vec` behind. The next
            // call resizes it back up — same allocation, no churn.
            rgba: std::mem::take(scratch),
        }
    }
}

/// Command pushed by the GUI thread into the running emulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputCommand {
    /// User input (tap / D-pad / key) to forward to the guest.
    Input(InputEvent),
    /// "Back to library" button — ask the run loop to stop cleanly.
    Stop,
}

/// Frame hook that:
///  - pushes one [`FrameSnapshot`] across `frame_tx` every time the
///    guest produces a new frame, so the GUI paints a live preview;
///  - drains pending [`InputCommand`]s from `input_rx` and forwards
///    them into the kernel's input queue / stop flag.
struct RunHook {
    frame_tx: Option<Sender<FrameSnapshot>>,
    input_rx: Option<Receiver<InputCommand>>,
    last_frame: u64,
    frame_send_failed: bool,
    input_disconnected: bool,
    /// Wall-clock timestamp of the last snapshot we emitted. Used to
    /// rate-limit the snapshot conversion + channel send to roughly
    /// `FRAME_PUSH_INTERVAL` (60 fps) so a chatty guest that bumps
    /// `frame_counter` thousands of times per logical frame doesn't
    /// pin the runner thread doing redundant RGB565→RGBA8888 work
    /// the GUI never gets to paint anyway.
    last_emit_at: Option<Instant>,
    /// Reusable host-side scratch buffer for the RGBA conversion.
    /// `FrameSnapshot::fill_from_framebuffer` swaps it with the
    /// outgoing `Vec<u8>` so we keep amortising the same allocation
    /// for the entire run instead of growing one per delivered frame.
    scratch: Vec<u8>,
    saw_non_black: bool,
}

impl RunHook {
    fn new(
        frame_tx: Option<Sender<FrameSnapshot>>,
        input_rx: Option<Receiver<InputCommand>>,
    ) -> Self {
        Self {
            frame_tx,
            input_rx,
            last_frame: 0,
            frame_send_failed: false,
            input_disconnected: false,
            last_emit_at: None,
            scratch: Vec::new(),
            saw_non_black: false,
        }
    }
}

impl FrameHook for RunHook {
    fn on_frame(&mut self, state: &mut KernelState) -> FrameAction {
        // Forward any UI input the user has produced since the last
        // slice into the kernel's pending input queue. Stop signals
        // turn into a `FrameAction::Stop`.
        let mut stop_requested = false;
        if !self.input_disconnected {
            if let Some(rx) = self.input_rx.as_ref() {
                loop {
                    match rx.try_recv() {
                        Ok(InputCommand::Input(ev)) => state.pending_input.push_back(ev),
                        Ok(InputCommand::Stop) => stop_requested = true,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            self.input_disconnected = true;
                            break;
                        }
                    }
                }
            }
        }

        // Stream the latest framebuffer up to the GUI, but at most
        // once per `FRAME_PUSH_INTERVAL`. The hook is invoked after
        // every dispatched WinCE call; without throttling a single
        // game tick that does ~2k `BitBlt`s would generate ~2k
        // 300 KiB snapshots — the work that turned a logical 60 fps
        // game into a sub-1 fps preview in the desktop launcher.
        if !self.frame_send_failed {
            if let Some(tx) = self.frame_tx.as_ref() {
                let counter = state.framebuffer.frame_counter;
                if counter != self.last_frame {
                    if state.framebuffer.is_all_black() && self.saw_non_black {
                        return if stop_requested {
                            state.should_stop = true;
                            FrameAction::Stop
                        } else {
                            FrameAction::Continue
                        };
                    }
                    self.saw_non_black = !state.framebuffer.is_all_black();
                    let now = Instant::now();
                    let due = self
                        .last_emit_at
                        .map(|t| now.duration_since(t) >= FRAME_PUSH_INTERVAL)
                        .unwrap_or(true);
                    if due {
                        self.last_frame = counter;
                        self.last_emit_at = Some(now);
                        let snapshot = FrameSnapshot::fill_from_framebuffer(
                            &state.framebuffer,
                            &mut self.scratch,
                        );
                        if tx.send(snapshot).is_err() {
                            // GUI thread dropped the receiver — the user
                            // closed the run screen / quit the launcher.
                            self.frame_send_failed = true;
                        }
                    }
                }
            }
        }

        if stop_requested {
            state.should_stop = true;
            FrameAction::Stop
        } else {
            FrameAction::Continue
        }
    }
}
