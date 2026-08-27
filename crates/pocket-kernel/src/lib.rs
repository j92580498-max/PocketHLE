#![allow(clippy::chunks_exact_to_as_chunks)]
//! Kernel-side scaffolding: virtual address space, thunk allocator,
//! thread state, scheduling.
//!
//! In PocketHLE every emulated process owns a single 32-bit address
//! space. The kernel is responsible for:
//!
//! * Mapping the loaded PE image into the CPU.
//! * Allocating a contiguous "thunk" region — one 4-byte slot per
//!   imported symbol — and patching the IAT so that calls into a
//!   foreign DLL transfer control to a known address that the CPU
//!   has marked with a code hook. When the hook fires, the host
//!   dispatches the call through [`Dispatcher`].
//! * Maintaining a stack and minimal heap for the emulated thread.
//!
//! The kernel does **not** implement individual API functions — that
//! is the responsibility of `pocket-winceapi`. Instead, the kernel
//! exposes a [`Dispatcher`] trait that an API layer registers itself
//! against.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use byteorder::{ByteOrder, LittleEndian};
use indexmap::IndexMap;
use thiserror::Error;

use pocket_cpu::{
    dump_mem_around, dump_regs, dump_stack_code_addrs, regs::ArmReg, Arch, Cpu, CpuError, Prot,
    StopReason,
};
use pocket_pe::{machine, ImportBinding, ImportSymbol, LoadedImage, ResourceEntry};

pub mod audio;
pub mod controls;
pub mod font;
pub mod framebuffer;
pub mod gapi;
pub mod gdi;
pub mod msgbox;
pub mod native_thunks;
pub mod profile;
pub mod registry;
pub mod tracker;
pub mod vfs;

pub use audio::{AudioEngine, AudioTap, GuestFormat, VoiceParams};
pub use framebuffer::{Framebuffer, FB_BYTES, FB_HEIGHT, FB_WIDTH};
pub use gdi::{GdiState, Surface};

/// Default base address of the synthetic IAT thunk pool.
pub const THUNK_REGION_BASE: u32 = 0x7000_0000;
/// Size, in bytes, of every IAT thunk slot. Most slots only hold a
/// single `bx lr` (the CPU hook stops us first and the dispatcher
/// handles the call), but a handful of pure-arithmetic CRT helpers
/// (`__rt_sdiv`, `__adds`, `__stoi`, …) are patched with native
/// ARM/VFP code so the JIT executes them with zero callback
/// overhead. The largest of those — the f64 helpers — needs ~6
/// instructions, so 32 bytes (8 instructions) is plenty and keeps
/// every thunk on a 32-byte cache line.
pub const THUNK_STRIDE: u32 = 32;
/// Base of the WinCE "slot 0" process view. Images based above this
/// address are additionally aliased here, because WinCE ARM CRT
/// startup code addresses itself through slot-relative pointers.
/// Images already based at [`SLOT_ALIAS_BASE`] need no alias — the
/// real section mapping *is* the slot view.
pub const SLOT_ALIAS_BASE: u32 = 0x0001_0000;
/// Size of the slot-0 alias window (128 KiB).
pub const SLOT_ALIAS_SIZE: u32 = 0x0002_0000;
/// Default stack size (256 KiB).
pub const DEFAULT_STACK_SIZE: u32 = 0x40000;
/// Default top of stack — chosen so that ARM-style descending stacks
/// stay below the thunk region.
pub const DEFAULT_STACK_TOP: u32 = 0x6000_0000;

/// Base of the WinCE kernel callback / trap region. Real Pocket PC
/// kernels publish a sea of small syscall trampolines starting at
/// `0xF000_0000`; coredll routes things like exception delivery and
/// `KernelIoControl` through fixed offsets into this page. Under HLE
/// we don't run the kernel at all, but several library routines still
/// load function pointers out of this range and `bx` to them, so we
/// have to make the address space at least valid. We map a 64 KiB
/// page filled with `bx lr` so any such jump returns harmlessly.
pub const KERNEL_TRAP_BASE: u32 = 0xF000_0000;
pub const KERNEL_TRAP_SIZE: u32 = 0x0001_0000;
/// Synthetic "process exit" trampoline. We install this address as
/// the initial value of `LR` before the guest enters its entry
/// point, so that when the entry point eventually returns (via a
/// regular `bx lr` / `pop {pc}` sequence) the CPU jumps to this
/// well-known address instead of an uninitialised LR=0. The run
/// loop has a code hook on this address that turns the hit into a
/// graceful `DispatchOutcome::Halt`-equivalent shutdown — without
/// this, every Pocket PC game crashes with `pc=0x00000000` once it
/// finishes running its `mainCRTStartup` even though it ran
/// hundreds of thousands of API calls successfully on the way out.
pub const PROCESS_EXIT_TRAMPOLINE_VA: u32 = 0xF000_FF00;
/// First synthetic return address for a guest thread created by CreateThread.
pub const THREAD_EXIT_TRAMPOLINE_BASE: u32 = 0xF000_FE00;

/// Base of the WinCE user-mode shared kernel data page. Real Pocket
/// PC kernels publish a per-process read-only view of the
/// `KDataStruct` at `USER_KPAGE = 0xFFFF_C800`. coredll itself, the
/// MS C runtime, and a lot of inline-assembly inside Pocket PC games
/// read this struct directly: `lpvTls` (offset 0x000) is a pointer
/// to the per-thread TLS slot array, and `ahSys[1..2]` (offsets
/// 0x008 / 0x00C) hold the current-thread / current-process
/// pseudo-handles. Without this page mapped, any game that touches
/// TLS or queries its own process handle through the user kdata
/// short-cut crashes with `READ_UNMAPPED` long before it reaches
/// `WinMain` (Bejeweled, Zuma, Bejeweled 2 — all do this).
///
/// We map a single 4 KiB page covering 0xFFFF_C000..0xFFFF_D000.
/// That's enough room for the entire `KDataStruct` (the largest
/// documented field — `aInfo[]` — ends well before 0x300) plus a
/// 256-byte TLS array which we host inside the same page and point
/// `lpvTls` at.
pub const USER_KDATA_PAGE_BASE: u32 = 0xFFFF_C000;
pub const USER_KDATA_PAGE_SIZE: u32 = 0x0000_1000;
/// Address of the `KDataStruct` itself.
pub const USER_KDATA_STRUCT_VA: u32 = 0xFFFF_C800;
/// Address of the in-page TLS slot array (`lpvTls` value). We pick
/// 0xFFFFCB00 — well past the `KDataStruct` so we don't collide
/// with documented fields.
pub const USER_KDATA_TLS_ARRAY_VA: u32 = 0xFFFF_CB00;
/// Number of TLS slots we expose. The real WinCE limit is 64
/// (`TLS_MINIMUM_AVAILABLE`), and slots are 4 bytes each, so the
/// array fits in 256 bytes.
pub const TLS_SLOT_COUNT: u32 = 64;
/// Fake (non-zero) handle stored at `ahSys[SH_CURTHREAD]`.
pub const FAKE_CURRENT_THREAD_HANDLE: u32 = 0xC0DE_0001;
/// Fake (non-zero) handle stored at `ahSys[SH_CURPROC]`.
pub const FAKE_CURRENT_PROCESS_HANDLE: u32 = 0xC0DE_0002;

/// `HINSTANCE` of the game module. Handed to the guest entry point in
/// r0 (Windows CE passes the `WinMain` arguments straight to the EXE
/// entry point) and returned by `GetModuleHandleW(NULL)`, so the two
/// always agree: games routinely compare `hInstance` against
/// `GetModuleHandle(NULL)` before registering their window class.
pub const PROCESS_INSTANCE_HANDLE: u32 = 0x1000_0000;

/// Fallback value for [`KernelState::module_path`] when the frontend
/// has no better information (a bare `.exe` handed to the CLI).
pub const DEFAULT_MODULE_PATH: &str = "\\Program Files\\Game\\Game.exe";
/// `SW_SHOWNORMAL`, the `nCmdShow` value the CE shell passes when it
/// launches an application from the Programs list.
pub const SW_SHOWNORMAL: u32 = 1;

/// Base of the guest-side heap region. 16 MiB is plenty for the
/// little games we target and still leaves headroom for the stack.
pub const HEAP_BASE: u32 = 0x5000_0000;
/// 64 MiB. Real Pocket PC processes only get ~32 MiB of total VA,
/// but games almost never come close to that — and our handle table
/// for `CreateDIBSection` etc. lives inside the same heap region,
/// so we keep it generous so back-to-back DIB allocations don't run
/// the game into an unmapped page.
pub const HEAP_SIZE: u32 = 0x0400_0000;

/// Guest-visible RAM window used by GAPI for direct framebuffer writes.
pub const SYNTHETIC_FRAMEBUFFER_BASE: u32 = 0x7800_0000;

/// Base of the region runtime-loaded modules are mapped at.
///
/// A fair number of Pocket PC titles split their artwork into a
/// resource-only satellite DLL and pull it in with `LoadLibraryW`
/// (Solitaire's `pegcards.dll` carries nothing but the card faces —
/// zero exports, 132 KiB of `.rsrc`). Those images have to live
/// somewhere the main image, heap, stack and thunk pool don't, so we
/// hand each one a 16 MiB slot in the otherwise unused window between
/// the EXE (low addresses) and the heap at [`HEAP_BASE`].
pub const MODULE_REGION_BASE: u32 = 0x3000_0000;
/// Distance between consecutive runtime module bases. WinCE satellite
/// DLLs are tiny; 16 MiB each is absurdly generous and keeps the
/// arithmetic trivial.
pub const MODULE_REGION_STRIDE: u32 = 0x0100_0000;
/// One past the last address [`MODULE_REGION_BASE`] may grow into, so
/// the 16th `LoadLibraryW` fails instead of colliding with the heap.
pub const MODULE_REGION_END: u32 = 0x4000_0000;

/// "bx lr" in ARM mode (little endian).
pub const ARM_BX_LR: [u8; 4] = [0x1e, 0xff, 0x2f, 0xe1];
/// Windows CE's ARM debugger breakpoint instruction.
pub const WINCE_BREAKPOINT: u32 = 0xe600_0010;

/// `jr ra; nop` in MIPS32 little-endian mode.
pub const MIPS_JR_RA: [u8; 8] = [0x08, 0x00, 0xe0, 0x03, 0x00, 0x00, 0x00, 0x00];

fn return_stub_bytes(arch: Arch) -> [u8; 8] {
    match arch {
        Arch::Arm => [
            ARM_BX_LR[0],
            ARM_BX_LR[1],
            ARM_BX_LR[2],
            ARM_BX_LR[3],
            ARM_BX_LR[0],
            ARM_BX_LR[1],
            ARM_BX_LR[2],
            ARM_BX_LR[3],
        ],
        Arch::Mips => MIPS_JR_RA,
    }
}

/// How often the run loop is allowed to poll the guest framebuffer and
/// call the frame hook when the guest has not announced a new frame
/// itself.
///
/// A slice ends on *every* emulated WinCE API call, and Zuma makes
/// ~8 500 of them per displayed frame. `sync_guest_framebuffer` has to
/// read the whole guest framebuffer back through the CPU backend and
/// compare it against the host copy in order to notice that a software
/// renderer wrote pixels with plain stores — 6 µs at 240x320 and 38 µs
/// at 800x480 (`mem_read_framebuffer_cost` in pocket-cpu measures
/// both). Doing that once per API call burned 7.5 s of Zuma's 22.7 s
/// run at 240x320, and 8 500 × 38 µs is 327 ms per frame at 800x480 —
/// a hard ~3 fps ceiling, which is exactly how the 800x480 mode
/// behaved. The pixels change at most ~60 times a second, so polling
/// on a wall clock finds every frame a display can show while costing
/// ~1 % of one core.
///
/// Frames the guest announces itself (`eglSwapBuffers`, `GXEndDraw`, a
/// `memcpy` into the raw framebuffer) still reach the hook on the very
/// slice they happen, because the run loop also presents whenever
/// `frame_counter` has moved — the cadence only bounds the *polling*.
const PRESENT_POLL_INTERVAL: Duration = Duration::from_millis(4);

/// Polling cadence once the guest has proven it announces its own
/// frames, and how long that proof stays good.
///
/// Zuma presents by `memcpy`ing its backbuffer over the raw
/// framebuffer in one call, which `sync_direct_framebuffer_write`
/// catches on the slice it happens. For a game that does that, the
/// [`PRESENT_POLL_INTERVAL`] read-back is pure loss: it found zero
/// changes across a 60-frame capture while costing 104 µs per poll at
/// 800x480 — 250 polls a second, 2.6 % of a core spent proving that
/// the pixels are the ones we already have.
///
/// So back the poll off to a slow safety net while the announcements
/// keep arriving. The poll cannot be dropped outright: a game is free
/// to blit part of its frame with `memcpy` and the rest with plain
/// stores, and only the read-back sees the latter. Trust is therefore
/// leased, not granted — [`DIRECT_PRESENT_TRUST`] after the last
/// announced frame the fast cadence comes back, so a game that
/// switches blit routines mid-run recovers on its own.
const PRESENT_POLL_BACKOFF: Duration = Duration::from_millis(250);

/// How long an announced frame keeps the poll on its slow cadence.
/// Comfortably longer than one frame at any playable rate, short
/// enough that recovering from a mid-run switch of blit routine costs
/// a stutter rather than a hang.
const DIRECT_PRESENT_TRUST: Duration = Duration::from_millis(500);

/// Install stop-on-execute hooks over `slots`, merging every run of
/// adjacent [`THUNK_STRIDE`]-spaced slots into one ranged hook.
///
/// Unicorn walks its whole code-hook list on each `emu_start`, and
/// PocketHLE re-enters the guest once per emulated WinCE API call, so
/// that walk is on the hottest path there is. A software-rendered game
/// such as Zuma makes ~8 500 API calls per displayed frame; with one
/// hook per thunk slot — ~200 imports plus ~4 970 `GetProcAddress`-able
/// dynamic exports across the seven emulated DLLs — the walk measured
/// 27 µs per call (~3.7 ns/hook), i.e. 230 ms of pure hook-list
/// bookkeeping per frame. One hook per contiguous run brings that back
/// to ~120 ns (`jit_slice_round_trip_cost` in pocket-cpu measures both).
///
/// `slots` must contain *only* slots whose stub is the `bx lr` return
/// stub: a native-ARM or constant-return thunk swallowed by a range
/// would trap into the dispatcher instead of executing the code that
/// was patched into it. The loader guarantees this by pushing a slot
/// only on the branch that writes the return stub.
fn install_coalesced_code_hooks(cpu: &mut dyn Cpu, slots: &[u32]) -> Result<(), KernelError> {
    let mut iter = slots.iter().copied();
    let Some(mut lo) = iter.next() else {
        return Ok(());
    };
    let mut hi = lo;
    for va in iter {
        if va == hi + THUNK_STRIDE {
            hi = va;
            continue;
        }
        cpu.add_code_hook_range(lo, hi + THUNK_STRIDE - 1)?;
        lo = va;
        hi = va;
    }
    cpu.add_code_hook_range(lo, hi + THUNK_STRIDE - 1)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("cpu error: {0}")]
    Cpu(#[from] CpuError),
    #[error("loader error: {0}")]
    Loader(String),
    #[error("dispatcher error: {0}")]
    Dispatch(String),
}

/// External input the host frontend wants delivered to the guest.
///
/// Real Pocket PC apps see input as window messages — `WM_LBUTTONDOWN` /
/// `WM_LBUTTONUP` / `WM_KEYDOWN` / `WM_KEYUP` arriving via the
/// `GetMessageW` queue. The host frontends (egui desktop, JNI Android,
/// optionally CLI) push these events into [`KernelState::pending_input`]
/// and the synthetic message pump in `pocket-winceapi::coredll` drains
/// the queue and converts each event into the right MSG before falling
/// back to its synthetic timer / paint loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Stylus down at game-space `(x, y)` (0..240, 0..320). Translated
    /// into `WM_LBUTTONDOWN` with the standard `lParam = (y << 16) | x`.
    PointerDown {
        x: u16,
        y: u16,
    },
    /// Stylus up. Translated into `WM_LBUTTONUP`.
    PointerUp {
        x: u16,
        y: u16,
    },
    /// Stylus movement while held down. Translated into `WM_MOUSEMOVE`.
    PointerMove {
        x: u16,
        y: u16,
    },
    /// Hardware / D-pad / virtual-keyboard key press. `vk` is a
    /// standard Win32 virtual-key code (e.g. `VK_LEFT = 0x25`).
    KeyDown {
        vk: u16,
    },
    KeyUp {
        vk: u16,
    },
}

/// How a guest asked to be told that a `waveOut` buffer finished
/// playing. Taken from the `fdwOpen` flags handed to `waveOutOpen`.
///
/// Getting this right is what keeps streamed music going: a game
/// submits two or three buffers up front and only queues the next one
/// once it is told an old one drained. Ignoring the notification meant
/// audio stopped after the first couple of buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaveCallbackKind {
    /// `CALLBACK_NULL` -- the guest polls `WHDR_DONE` instead.
    #[default]
    None,
    /// `CALLBACK_WINDOW` -- post `MM_WOM_DONE` to a window.
    Window,
    /// `CALLBACK_TASK` / `CALLBACK_THREAD` -- post `MM_WOM_DONE` as a
    /// thread message (`hwnd == 0`).
    Thread,
    /// `CALLBACK_FUNCTION` -- call the guest's `waveOutProc`.
    Function,
    /// `CALLBACK_EVENT` -- signal an event handle.
    Event,
}

/// A `WAVEHDR` the guest handed to `waveOutWrite` that we have not
/// reported as finished yet.
#[derive(Debug, Clone, Copy)]
pub struct PendingWaveBuffer {
    /// Guest address of the `WAVEHDR`.
    pub hdr: u32,
    /// Playback cursor, in guest samples since the last
    /// `waveOutOpen` / `waveOutReset`, at which this buffer is done.
    pub end_cursor: u64,
}

/// Everything we track for the single `waveOut` device we emulate.
#[derive(Debug, Default)]
pub struct WaveOutState {
    /// Fake `HWAVEOUT` handed to the guest, or 0 while closed.
    pub handle: u32,
    pub callback_kind: WaveCallbackKind,
    /// Window handle, thread id, `waveOutProc` pointer or event
    /// handle, depending on `callback_kind`.
    pub callback_target: u32,
    /// `dwInstance` from `waveOutOpen`, passed back unchanged.
    pub instance: u32,
    /// Cumulative guest samples accepted by `waveOutWrite`.
    pub written_samples: u64,
    /// Which guest thread opened the device (0 = main, n = `threads[n-1]`).
    /// `CALLBACK_TASK` notifications belong to that thread's queue, so
    /// the main loop must not swallow them.
    pub owner_thread: usize,
    /// Buffers still playing, in submission order.
    pub pending: VecDeque<PendingWaveBuffer>,
    /// `waveOutPause` stops retiring buffers until `waveOutRestart`.
    pub paused: bool,
    /// `CALLBACK_FUNCTION` buffers waiting for their `waveOutProc`
    /// call. The message pump drains this by re-entering the guest.
    pub function_done: VecDeque<u32>,
    /// Set while the guest is executing `waveOutProc`, so the pump
    /// knows to restore the interrupted call's registers when the
    /// callback returns.
    pub function_frame: Option<GuestCallFrame>,
}

/// One `hssSound` or `hssMusic` object the guest constructed.
///
/// HSS is a C++ library: the game news up an object, calls `load` with
/// a filename, and then hands the object's `this` pointer to
/// `hssSpeaker::playSound` / `playMusic`. We keep the decoded PCM here,
/// keyed by that same `this` pointer, because the guest-side object is
/// opaque to us — it is whatever the real `hss.dll` would have put
/// there, and we never run the real one.
#[derive(Debug, Default, Clone)]
pub struct HssClip {
    /// Decoded interleaved PCM, ready for [`AudioEngine::play_voice_with`].
    ///
    /// Shared rather than owned: a game re-loads the same track into a
    /// fresh object on every level change, and a rendered module runs
    /// to tens of megabytes. The `Arc` lets those loads — and each
    /// `play`, which needs the samples while `kernel` is borrowed —
    /// alias one decode instead of copying it.
    pub samples: Arc<Vec<i16>>,
    pub format: GuestFormat,
    /// Set by `hssSound::loop` / `hssMusic::loop`.
    pub looped: bool,
    /// Per-object volume, 0..=64 in HSS's own scale.
    pub volume: u32,
    /// Guest path the clip came from, for logging.
    pub path: String,
}

/// Everything we track for `hss.dll`.
#[derive(Debug)]
pub struct HssState {
    /// Loaded clips, keyed by the `this` pointer of the `hssSound` /
    /// `hssMusic` object that owns them.
    pub clips: HashMap<u32, HssClip>,
    /// Decoded PCM keyed by guest path, so loading a file a second time
    /// reuses the first decode. Rendering a Protracker module is the
    /// expensive half of `load` — JumpyBall re-loads `mainmenu.tkm`
    /// three times before the first level even starts — and the result
    /// is a pure function of the file, so it is safe to keep.
    pub decoded: HashMap<String, (GuestFormat, Arc<Vec<i16>>)>,
    /// Mixer output rate from `hssSpeaker::open`. HSS resamples every
    /// clip to this, and so do we — the guest picked it, so it is what
    /// the game expects to hear.
    pub format: GuestFormat,
    /// Master volumes from `volumeSounds` / `volumeMusics`, 0..=64.
    /// The getters the guest also imports have to return what it set.
    pub sound_volume: u32,
    pub music_volume: u32,
    /// `true` once `hssSpeaker::open` has run.
    pub opened: bool,
}

impl Default for HssState {
    fn default() -> Self {
        Self {
            clips: HashMap::new(),
            decoded: HashMap::new(),
            format: GuestFormat::default(),
            // HSS's own default, and what JumpyBall sets anyway.
            sound_volume: 64,
            music_volume: 64,
            opened: false,
        }
    }
}

/// Voice group for `hssSpeaker::playSound`, so `stopSounds` can silence
/// effects without touching the music.
pub const HSS_SOUND_GROUP: u32 = 1;
/// Voice group for `hssSpeaker::playMusic`.
pub const HSS_MUSIC_GROUP: u32 = 2;

/// A modal dialog put up by `DialogBoxIndirectParamW` / `DialogBoxParamW`.
///
/// These APIs block: they run their own message pump and do not return
/// until the dialog's procedure calls `EndDialog`, whose `nResult`
/// becomes the return value. The HLE keeps the guest parked inside the
/// API thunk for the duration — the same technique `MessageBoxW` uses —
/// so this records what the parked call is waiting for.
#[derive(Debug, Clone)]
pub struct ModalDialog {
    /// Synthetic `HWND` the dialog's panel and controls are registered
    /// under, so its children are addressed separately from a
    /// `CreateDialog*` panel that may already be on screen.
    pub hwnd: u32,
    /// The guest's `DialogProc`.
    pub proc: u32,
    /// `EndDialog`'s `nResult` once it has been called. `None` means the
    /// dialog is still up.
    pub result: Option<u32>,
    /// Set once `WM_INITDIALOG` has been delivered, so the parked call
    /// does not send it again on every re-dispatch.
    pub initialised: bool,
    /// Control id of the `WM_COMMAND` most recently handed to the
    /// `DialogProc`, cleared as soon as it has been examined.
    ///
    /// This is how `DefDlgProc`'s behaviour is reproduced: an `IDOK` or
    /// `IDCANCEL` the application does not act on itself ends the dialog
    /// anyway, and the only way to know it did not act is to look once
    /// its handler has returned.
    pub dispatched: Option<u32>,
    /// Re-dispatch counter, bounding a headless run the same way
    /// [`KernelState::message_box_spins`] does.
    pub spins: u32,
}

/// A commctrl status bar created via `CreateStatusWindow{A,W}`.
///
/// Pocket PC titles use this for the strip along the bottom edge of
/// the client area — PPC2002 Solitaire puts `Time: n` and `Score: n`
/// there. The control owns its own pixels on a real device: the app
/// only ever sends it `SB_SETPARTS` / `SB_SETTEXT`, and never paints
/// the text itself (Solitaire imports no text-output API at all). So
/// the HLE has to both remember the parts and draw them.
#[derive(Debug, Clone, Default)]
pub struct StatusBar {
    /// Parent window the bar is docked to the bottom of.
    pub parent: u32,
    /// Height in pixels, from the `CreateStatusWindow` call.
    pub height: i32,
    /// Right edge of each part, in client coordinates, as handed to us
    /// by `SB_SETPARTS`. A trailing `-1` means "out to the right edge"
    /// and is stored as the client width at draw time.
    pub part_edges: Vec<i32>,
    /// Current text of each part, indexed by part number. Sparse:
    /// `SB_SETTEXT` may set part 3 before part 0 exists.
    pub part_text: Vec<String>,
    /// Cleared whenever the text or parts change so the next frame
    /// repaints the strip.
    pub dirty: bool,
}

impl StatusBar {
    /// Height a real Pocket PC status bar occupies. The control sizes
    /// itself from the UI font on a device; we can't, so this is
    /// measured off the PPC2002 reference screenshot, where the strip
    /// spans rows 275..294 of a 320-row screen — 19 px. Our built-in
    /// font is 8 px tall, which [`Self::render`] centres in that space.
    ///
    /// Note the reference has 45 px of chrome below the board, but the
    /// remaining 26 px are the shell's "New Tools" command bar, a
    /// separate window PocketHLE doesn't draw at all.
    pub const DEFAULT_HEIGHT: i32 = 19;

    /// Face colour — the classic `COLOR_BTNFACE` light grey.
    const BG: u16 = 0xC618;
    /// Text colour (`COLOR_BTNTEXT`).
    const FG: u16 = 0x0000;
    /// One-pixel highlight along the top edge, which is what makes the
    /// strip read as a raised control rather than a painted rectangle.
    const EDGE: u16 = 0xFFFF;

    /// Store `text` for `part`, growing the sparse vector as needed.
    pub fn set_part_text(&mut self, part: usize, text: String) {
        if part >= self.part_text.len() {
            self.part_text.resize(part + 1, String::new());
        }
        if self.part_text[part] != text {
            self.part_text[part] = text;
            self.dirty = true;
        }
    }

    /// Replace the part layout. Edges are right-hand x coordinates in
    /// client space; a negative edge means "extend to the right margin".
    pub fn set_parts(&mut self, edges: Vec<i32>) {
        if self.part_edges != edges {
            self.part_edges = edges;
            self.dirty = true;
        }
    }

    /// Effective height in pixels.
    pub fn height(&self) -> i32 {
        if self.height > 0 {
            self.height
        } else {
            Self::DEFAULT_HEIGHT
        }
    }

    /// `[left, right)` x range of part `i` for a `client_w`-wide bar.
    fn part_span(&self, i: usize, client_w: i32) -> (i32, i32) {
        let left = if i == 0 {
            0
        } else {
            self.part_edges.get(i - 1).copied().unwrap_or(0)
        };
        // SB_SETPARTS uses -1 for the final part to mean "run out to
        // the right edge"; anything unset behaves the same way.
        let right = match self.part_edges.get(i).copied() {
            Some(edge) if edge >= 0 => edge,
            _ => client_w,
        };
        (left.clamp(0, client_w), right.clamp(0, client_w))
    }

    /// Paint the bar along the bottom edge of `surf`.
    ///
    /// Called after the guest finishes its own `WM_PAINT`: on a device
    /// the bar is a sibling window that paints itself *after* the parent
    /// has filled the client area, and Solitaire does blit over the full
    /// client rect, so painting earlier would just be overdrawn.
    pub fn render(&self, surf: &mut Surface<'_>) {
        let (surf_w, surf_h) = surf.dimensions();
        let (client_w, client_h) = (surf_w as i32, surf_h as i32);
        let bar_h = self.height().min(client_h);
        if bar_h <= 0 || client_w <= 0 {
            return;
        }
        let top = client_h - bar_h;
        surf.fill_rect(0, top, client_w, bar_h, Self::BG);
        surf.fill_rect(0, top, client_w, 1, Self::EDGE);

        let text_y = top + 1 + ((bar_h - 1 - font::GLYPH_H) / 2).max(0);
        for (i, text) in self.part_text.iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            let (left, right) = self.part_span(i, client_w);
            // Truncate rather than spill into the neighbouring part.
            let room = (right - left - 4).max(0);
            let max_chars = (room / font::GLYPH_W) as usize;
            if max_chars == 0 {
                continue;
            }
            let shown: String = text.chars().take(max_chars).collect();
            font::draw_str(surf, left + 2, text_y, &shown, Self::FG);
        }
    }
}

/// Registers of the API call we interrupted to run a guest callback
/// (`waveOutProc`, a `WndProc` receiving `WM_CREATE`, ...), so the
/// interrupted call can be resumed transparently afterwards.
#[derive(Debug, Clone, Copy)]
pub struct GuestCallFrame {
    /// R0..R3 of the interrupted call.
    pub args: [u32; 4],
    /// LR of the interrupted call — where it should return to.
    pub lr: u32,
    /// SP at the point we hijacked the call. Also used to tell a
    /// genuine callback return (SP back where it was) from a *nested*
    /// call to the same API from inside the callback (SP lower).
    pub sp: u32,
}

/// Which leg of the synchronous `CreateWindowExW` message sequence is
/// currently running inside the guest `WndProc`.
///
/// Real Windows delivers `WM_CREATE` and then `WM_SIZE` before
/// `CreateWindowExW` returns. Solitaire computes its whole board layout
/// in the `WM_SIZE` arm, so a title that draws immediately after
/// `CreateWindowExW` — before it ever reaches its message pump — sees a
/// zeroed layout and refuses to paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CreateStage {
    /// No detour in flight.
    #[default]
    Idle,
    /// The `WndProc` is running `WM_CREATE`; `WM_SIZE` comes next.
    Create,
    /// The `WndProc` is running `WM_SIZE`, the last message of the
    /// sequence. When it returns, `CreateWindowExW` finally does too.
    Size,
}

/// A Win32 event object created by `CreateEventW`.
///
/// Real state matters: a Pocket PC worker thread typically loops on
/// `WaitForSingleObject(hQuit, small_timeout)` and only exits once the
/// event is actually signalled. Answering every wait with
/// `WAIT_OBJECT_0` made such threads quit on their first iteration —
/// which is why Asphalt 2 3D opened its wave device and then never
/// wrote a single buffer.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventObject {
    /// `bManualReset` — an auto-reset event clears itself when a wait
    /// on it succeeds.
    pub manual_reset: bool,
    pub signalled: bool,
}

/// Result of dispatching a hooked call back to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// Returned a value via R0; emulator should resume from LR.
    ReturnedR0(u32),
    /// Returned a 64-bit value via R0:R1.
    ReturnedR0R1(u32, u32),
    /// The host wants the emulator to stop entirely (graceful exit).
    Halt,
    /// The host has not implemented this API. PocketHLE will log a
    /// loud warning and synthesize a `0` return.
    Unimplemented,
    /// Reroute control flow into the guest at `pc`, leaving LR/SP and
    /// argument registers exactly as the handler set them up. Used to
    /// trampoline into guest WndProc / atexit / signal handlers from
    /// inside an HLE call (e.g. `DispatchMessageW`).
    JumpTo(u32),
}

/// Trait an API layer registers with the kernel. Called every time
/// emulated code reaches a thunk address.
pub trait Dispatcher {
    fn dispatch(
        &mut self,
        cpu: &mut dyn Cpu,
        thunk: &Thunk,
        kernel: &mut KernelState,
    ) -> Result<DispatchOutcome, KernelError>;

    /// If this thunk is bound to a pure constant-return stub (i.e.
    /// the handler reads no arguments and returns a fixed `u32` in
    /// `r0`), return that constant so the kernel can patch the
    /// IAT with a `mov r0, #imm; bx lr` thunk and skip the
    /// dispatcher entirely. Default implementation returns `None`,
    /// meaning "use the regular dispatched path".
    ///
    /// Implementations MUST be deterministic for a given `thunk` —
    /// the kernel calls this exactly once per import at load time
    /// and bakes the result into the guest's IAT.
    fn constant_for(&self, _thunk: &Thunk) -> Option<u32> {
        None
    }

    fn dynamic_names(&self, _dll: &str) -> Vec<String> {
        Vec::new()
    }
}

/// Trivial dispatcher used by tests that don't need any HLE
/// surface. All imports take the standard hooked path — the
/// `dispatch` method just returns `r0 = 0` so a fall-through hit
/// is harmless.
pub struct NullDispatcher;

impl Dispatcher for NullDispatcher {
    fn dispatch(
        &mut self,
        _cpu: &mut dyn Cpu,
        _thunk: &Thunk,
        _kernel: &mut KernelState,
    ) -> Result<DispatchOutcome, KernelError> {
        Ok(DispatchOutcome::ReturnedR0(0))
    }
}

/// A DLL the guest pulled in at runtime with `LoadLibraryW`.
///
/// PocketHLE does not *execute* satellite modules: nothing resolves
/// their exports (a resource-only DLL has none, and the titles that
/// load one import no `GetProcAddress`), so we skip base relocations
/// and the IAT entirely and only make the image's bytes readable so
/// `FindResourceW` / `LoadBitmapW` / `LoadStringW` can reach them.
#[derive(Debug, Clone)]
pub struct LoadedModule {
    /// `HMODULE` handed back to the guest. Equal to [`Self::base`],
    /// which is what the real CE loader returns.
    pub handle: u32,
    /// Lower-cased file name, no directory (`pegcards.dll`).
    pub name: String,
    /// Address the image was mapped at.
    pub base: u32,
    /// PE SizeOfImage copied from the loaded module header.
    pub image_size: u32,
    /// PE entry point as a guest virtual address.
    pub image_entry: u32,
    /// Flattened `.rsrc` directory of that image.
    pub resources: Vec<ResourceEntry>,
    /// `LoadLibrary` count, decremented by `FreeLibrary`. We never
    /// unmap on zero — a game that frees and re-loads its artwork DLL
    /// would otherwise pay for a second slot.
    pub refcount: u32,
}

/// Mutable kernel state that persists across calls and that handlers
/// need to read or modify. Bundled into one struct so we can hand it
/// out by `&mut` without conflicting with the immutable parts of
/// [`Process`] (image bytes, thunk table) that the run loop uses.
pub struct KernelState {
    pub heap: Heap,
    pub vfs: vfs::Vfs,
    /// Guest path reported by `GetModuleFileName{A,W}`.
    ///
    /// Real Pocket PC games routinely derive their asset paths from
    /// their own module path: they call `GetModuleFileNameW`, chop off
    /// the trailing `<exe name>` (often by subtracting the length of a
    /// hard-coded literal such as `L"SkyForceReloaded.exe"`), and
    /// append `data.pak`. A generic placeholder path therefore breaks
    /// them — the subtraction lands mid-directory and produces
    /// nonsense like `\\Programdata.pak`. Frontends set this to the
    /// install path the CAB would have used on a device.
    pub module_path: String,
    /// Software-rendered display the GDI/GAPI handlers paint into.
    pub framebuffer: Framebuffer,
    /// Tracked GDI objects (DCs, bitmaps, brushes, pens, fonts).
    pub gdi: GdiState,
    /// Flat resource table for `FindResourceW` / `LoadResource`.
    pub resources: Vec<ResourceEntry>,
    /// Image base for resource RVA → VA conversion.
    pub image_base: u32,
    /// Size and entry point copied from the loaded PE header for module introspection APIs.
    pub image_size: u32,
    pub image_entry: u32,
    pub dynamic_exports: HashMap<u32, HashMap<String, u32>>,
    pub next_module_handle: u32,
    /// DLLs the guest brought in at runtime via `LoadLibraryW`, in load
    /// order. Only resource-carrying satellite modules end up here; the
    /// HLE'd system DLLs are answered with fixed fake handles instead.
    pub modules: Vec<LoadedModule>,
    /// Base address the next `LoadLibraryW` will map at. Starts at
    /// [`MODULE_REGION_BASE`] and walks up by [`MODULE_REGION_STRIDE`].
    pub next_module_base: u32,
    /// Host directories searched for a runtime-loaded module, most
    /// specific first. Normally just the directory the EXE came from
    /// (a bare-exe run) or the CAB extraction root, which is where a
    /// game's satellite DLLs sit next to it on a real device.
    pub module_search_dirs: Vec<PathBuf>,
    /// Set the first time `GXBeginDraw` runs. The dispatcher maps the
    /// framebuffer region into the guest VA space lazily — but that
    /// requires `&mut dyn Cpu`, which isn't available outside a call,
    /// so we let the GAPI handlers do it.
    pub fb_mapped: bool,
    /// How many frames the guest has presented by writing the raw
    /// framebuffer with a `memcpy` we caught synchronously, rather
    /// than with the plain stores that only the polling read-back in
    /// [`sync_guest_framebuffer`] can notice. Read by the run loop to
    /// decide how often that poll is worth running; see
    /// [`PRESENT_POLL_BACKOFF`].
    pub direct_fb_frames: u64,
    /// Pre-allocated scratch buffer the GAPI flush uses when copying
    /// the guest-mapped framebuffer back into [`Framebuffer::pixels`].
    /// Sized to `FB_BYTES` once on first use so there is no
    /// allocation per `GXEndDraw` (which can fire 60+ times a
    /// second).
    pub gx_readback_scratch: Vec<u8>,
    /// Reusable scratch buffer for guest-side bulk-memory ops
    /// (`memcpy`, `memmove`, `memcmp`, `memset`). Derby and similar
    /// games drive blits row-by-row through guest `memcpy`, hitting
    /// this path tens of thousands of times per frame. Sharing one
    /// `Vec` instead of allocating a fresh `vec![0u8; len]` per call
    /// removes the heap-allocator from the inner loop.
    pub mem_op_scratch: Vec<u8>,
    /// Second scratch buffer for ops that need to compare two
    /// guest-memory ranges (`memcmp`). Kept separate from
    /// [`Self::mem_op_scratch`] so the two can grow independently.
    pub mem_op_scratch_b: Vec<u8>,
    /// Reusable scratch buffer holding a snapshot of the source
    /// pixels for `BitBlt` / `StretchBlt`. Sharing one buffer across
    /// every blit call avoids the per-call `Vec<u8>` allocation that
    /// used to clone the entire 150 KiB framebuffer every time the
    /// game `BitBlt`-ed *from* the screen surface.
    pub bit_blt_src_scratch: Vec<u8>,
    /// Reusable scratch buffer for the host -> guest DIB section
    /// pixel encode at the end of every BitBlt to a memory DC.
    /// Sized to `dib_row_stride * height` of the bitmap being
    /// synced; encoder writes pixels in-place each frame.
    pub dib_sync_scratch: Vec<u8>,
    /// Reusable scratch buffer for reading raw guest-side DIB pixels
    /// before decoding them into the bitmap's RGB565 cache.
    pub dib_decode_scratch: Vec<u8>,
    /// Frame counter snapshot taken the last time `GXEndDraw`
    /// finished pushing pixels into the host framebuffer. If
    /// `framebuffer.frame_counter` still equals this when the next
    /// `GXBeginDraw` runs, we know no GDI handler has touched the
    /// host fb in the meantime, and we can skip the
    /// host-fb -> guest-fb copy that primes the back-buffer.
    pub gx_last_pushed_counter: u64,
    /// Signature of sampled guest framebuffer rows from the last sync.
    pub gx_guest_signature: Option<u64>,
    /// Guest addresses of the strings `glGetString` / `eglQueryString`
    /// have handed out, keyed by the string itself. GL promises the
    /// returned pointer stays valid for the life of the context, and
    /// a renderer that queries `GL_EXTENSIONS` every frame would leak
    /// the heap dry without this cache.
    pub gles_strings: std::collections::HashMap<String, u32>,
    /// Number of synthetic `WM_PAINT` / `WM_TIMER` messages already
    /// fed to the guest. Used by `GetMessageW` / `PeekMessageW` to
    /// terminate the message loop after a configurable number of
    /// frames, so headless runs don't loop forever.
    pub synthetic_message_count: u64,
    /// Maximum synthetic frame messages to inject. `0` means
    /// unlimited.
    pub synthetic_message_budget: u64,
    /// Address of the guest's last-registered window procedure. Set
    /// by `RegisterClassW`, used by `DispatchMessageW` to trampoline
    /// into guest-side WM_PAINT / WM_KEYDOWN handlers.
    pub wnd_proc: u32,
    /// WndProc addresses keyed by the registered class name.
    pub window_class_procs: HashMap<String, u32>,
    /// `WNDCLASS::hbrBackground` of the class the guest registered.
    ///
    /// `BeginPaint` erases the client area with this brush before it
    /// hands the DC over, which is what `DefWindowProc` does for
    /// `WM_ERASEBKGND` on a device. An app that paints only its
    /// controls — HelloWorld draws nothing but a `STATIC` and a
    /// `BUTTON` — relies on the class brush for every other pixel, and
    /// without it the window stays whatever the framebuffer was
    /// cleared to (black).
    pub window_background: Option<u32>,
    /// WndProc and CREATESTRUCT for synthetic window creation messages.
    pub pending_create: Option<(u32, u32)>,
    /// Window messages a real Pocket PC shell posts right after a
    /// top-level window is created: `WM_SIZE`, `WM_SHOWWINDOW`,
    /// `WM_ACTIVATE` and `WM_SETFOCUS`.
    ///
    /// GAPI titles gate their render loop on activation — SkyForce
    /// Reloaded only calls `GXBeginDraw` once it has seen
    /// `WM_ACTIVATE(WA_ACTIVE)` — so a queue that only ever produced
    /// `WM_PAINT` left them spinning in `PeekMessage` with a single
    /// frame on screen.
    pub pending_startup: std::collections::VecDeque<(u32, u32, u32)>,
    /// Open `FindFirstFileW` enumerations: handle -> remaining
    /// `(name, size, is_dir)` entries.
    pub find_handles: HashMap<u32, std::collections::VecDeque<(String, u64, bool)>>,
    /// Next `FindFirstFileW` handle to hand out.
    pub next_find_handle: u32,
    /// In-memory Windows CE registry. Seeded from the CAB's
    /// `_setup.xml` `Registry` section so a title finds the values its
    /// installer would have written.
    pub registry: crate::registry::Registry,
    /// Window handles and their class procedures.
    pub window_procs: HashMap<u32, u32>,
    /// Window handles and their user data pointers.
    pub window_userdata: HashMap<u32, u32>,
    /// Window handles and their class names.
    pub window_classes: HashMap<u32, String>,
    /// Per-window user data used by WndProc implementations (GWL_USERDATA).
    pub window_user_data: u32,
    /// `nIDEvent` of the timer the guest most recently registered via
    /// `SetTimer`, or `0` if none. The synthetic message pump uses
    /// this to inject `WM_TIMER` messages with a wParam the guest
    /// will recognise.
    pub synthetic_timer_id: u32,
    /// Timer interval and host-clock deadline used by the synthetic message pump.
    pub synthetic_timer_interval_ms: u32,
    pub synthetic_timer_next_ms: u64,
    /// Host-clock deadline for the next synthetic paint message.
    pub synthetic_paint_next_ms: u64,
    /// `true` once the synthetic message pump has delivered
    /// `WM_CREATE`. Real Windows fires `WM_CREATE` synchronously
    /// from `CreateWindowExW`; we instead fire it on the very first
    /// `GetMessageW` so the guest's `WndProc` runs its window-init
    /// code (which typically calls `SetTimer`).
    pub synthetic_create_sent: bool,
    /// Set once the startup `WM_SIZE` has been handed to the guest, so
    /// the synchronous `CreateWindowExW` / `ShowWindow` sequence only
    /// runs it once.
    pub synthetic_size_sent: bool,
    /// Registers of the `CreateWindowExW` / `ShowWindow` call we
    /// interrupted to run the guest's `WndProc` synchronously. See
    /// [`CreateStage`].
    pub create_frame: Option<GuestCallFrame>,
    /// Which leg of the synchronous window-creation message sequence is
    /// in flight inside the guest `WndProc`.
    pub create_stage: CreateStage,
    /// Registers of the `CreateDialogIndirectParamW` call we interrupted
    /// to deliver `WM_INITDIALOG` to the guest's `DialogProc`. Same
    /// round-trip idiom as [`create_frame`](Self::create_frame): the
    /// dialog HWND is only returned to the caller once the callback
    /// unwinds, otherwise the caller stores the callback's `BOOL`.
    pub dialog_frame: Option<GuestCallFrame>,
    /// Registers of a `DispatchMessageW` call interrupted to run the
    /// guest window procedure. The trap return restores the caller's
    /// continuation after the WndProc returns through its stack frame.
    pub message_frame: Option<GuestCallFrame>,
    /// Bottom status bar created via commctrl's `CreateStatusWindowW`.
    ///
    /// Pocket PC apps get a real shell-drawn bar here; PocketHLE has no
    /// shell, so we keep the text the guest pushes at it via
    /// `SB_SETTEXT` and paint it ourselves at the bottom of the screen.
    /// `None` until the guest actually asks for a status window.
    pub status_bar: Option<StatusBar>,
    /// Built-in child controls (`BUTTON`, `EDIT`, `STATIC`) the guest
    /// created through `CreateWindowExW`.
    ///
    /// On a device these window classes live inside coredll: the OS owns
    /// their pixels, their text and their focus, and turns a tap into a
    /// `WM_COMMAND` for the parent. The application never draws them, so
    /// neither can we leave them to the guest — see [`controls`].
    pub controls: controls::Controls,
    /// The modal message box currently on screen, if any.
    ///
    /// `MessageBoxW` does not return until the user has answered, so the
    /// guest is parked inside its own API thunk while this is `Some` and
    /// every stylus event goes to the box instead of to the application —
    /// see [`msgbox`]. It lives beside [`KernelState::controls`] rather
    /// than inside them because it is not a child of the guest's window:
    /// it owns its own frame, its own caption bar and the input focus.
    pub modal: Option<msgbox::MessageBox>,
    /// Re-dispatch counter for the parked `MessageBoxW`.
    ///
    /// A modal box on a device waits forever, which is right in front of
    /// a user and wrong in a headless `--max-frames` run where nobody can
    /// answer. This cap keeps the test suite finite without visibly
    /// rushing an interactive session.
    pub message_box_spins: u32,
    /// The modal dialog a `DialogBoxIndirectParamW` is currently running,
    /// if any.
    ///
    /// `DialogBox*` blocks exactly as `MessageBoxW` does — it does not
    /// return until the dialog calls `EndDialog` — so the same parking
    /// trick applies: the guest sits in its own API thunk pumping the
    /// dialog while the host presents frames. `Some(result)` in the inner
    /// option is `EndDialog`'s `nResult`, which becomes the blocking
    /// call's return value.
    pub modal_dialog: Option<ModalDialog>,
    /// Input events queued by the host frontend (mouse / D-pad /
    /// keyboard). Drained by `GetMessageW` / `PeekMessageW` before
    /// any synthetic timer / paint message is fabricated, so real
    /// user input always wins over the synthetic pump.
    pub pending_input: VecDeque<InputEvent>,
    /// Set once the guest has fetched the GAPI key list through
    /// `GXGetDefaultKeys`. Such a guest only reacts to the virtual keys
    /// that table names, so the host's confirm key has to be delivered
    /// as `vkA` — see [`gapi::remap_host_key`].
    pub gapi_keys_queried: bool,
    /// Message previewed by `PeekMessageW` with PM_NOREMOVE. The next
    /// `GetMessageW` must return the same message instead of advancing
    /// the synthetic queue a second time. Carries the target `hwnd`
    /// alongside the message so a dialog control's `WM_COMMAND` is
    /// still addressed to its own dialog when it is replayed.
    pub pending_message: Option<(u32, u32, u32, u32)>,
    /// Cooperative guest threads created through `CreateThread`. The host
    /// scheduler runs one ready thread at a time between API boundaries.
    pub threads: Vec<GuestThread>,
    /// `CreateEventW` objects keyed by the fake handle we handed the
    /// guest. See [`EventObject`].
    pub events: HashMap<u32, EventObject>,
    /// CreateSemaphoreW objects keyed by the fake handle we handed the
    /// guest. See [`SemaphoreObject`].
    pub semaphores: HashMap<u32, SemaphoreObject>,
    /// Index of the thread whose register context is currently active.
    pub current_thread: usize,
    /// Current state of the Pocket PC virtual keys.
    pub pressed_keys: [bool; 256],
    /// Set by the host frontend to ask the run loop to stop cleanly
    /// at the next slice boundary. Used by the desktop GUI's "Back to
    /// library" button so the user can interrupt a running game.
    pub should_stop: bool,
    /// Bitset of TLS slots that have been handed out by `TlsAlloc`.
    /// Bit `i` set means slot `i` is currently in use. Real WinCE
    /// would track this in the per-process kdata `aTlsSlotsUsed`
    /// bitmap; we mirror it here so `TlsAlloc` / `TlsFree` are well
    /// defined. Storage for the slot *values* lives in guest memory
    /// at [`USER_KDATA_TLS_ARRAY_VA`] so the guest can reach them
    /// through the `lpvTls` pointer in the user kdata page.
    pub tls_slots_used: u64,
    /// In-progress per-thunk re-entry frames for the C++ vector
    /// constructor / destructor iterators (`??_L` / `??_M`).
    ///
    /// The MSVC ARM CE CRT implements `??_L` as a host-callable
    /// `for (i=0..N) pCtor(p+i*size);` loop. PocketHLE has no way
    /// to call back into guest code N times from a single Rust
    /// dispatch, so we instead drive the loop one element per
    /// `JumpTo` round-trip: the handler stashes the iteration state
    /// here, sets `LR = ??_L thunk_va` and `JumpTo = pCtor`, and the
    /// guest's `bx lr` brings us back to the same handler for the
    /// next iteration.
    ///
    /// Element constructors regularly start their own array —
    /// Zuma's board holds two sub-boards that each hold 200 balls —
    /// so `??_L` re-enters itself while an outer iteration is still
    /// in flight. The in-flight iterations therefore form a stack,
    /// with the innermost one on top; see
    /// [`VectorIterFrame::sp_at_call`] for how a nested call is told
    /// apart from a callback returning into the thunk.
    pub vector_iter_stack: Vec<VectorIterFrame>,
    /// In-flight `qsort` calls, keyed the same way as
    /// [`Self::vector_iter_stack`]. `qsort`'s comparison function
    /// also lives in guest code, so the sort has to be driven one
    /// comparison per `JumpTo` round-trip — see [`QsortFrame`].
    pub qsort_frames: HashMap<u32, QsortFrame>,
    /// Where `strtok` resumes when it is called with a NULL string.
    ///
    /// The CRT keeps this position in per-thread state and a guest
    /// tokenising in a loop depends on it surviving between calls, so
    /// there is nowhere else it can live. `0` means the scan is
    /// finished — the next NULL-string call returns NULL.
    pub strtok_pos: u32,
    /// Cached `__security_cookie` value handed out by
    /// `__security_gen_cookie`. Generated lazily the first time the
    /// guest calls the export, then returned for every subsequent
    /// call so that the guest's `__security_cookie` global stays in
    /// sync with the per-function epilogue's stored copy. Real
    /// coredll uses a one-shot `__security_init_cookie` for the same
    /// reason; behaviourally we are equivalent.
    pub security_cookie: u32,
    /// Real-time PCM output. Lazily started by `waveOutOpen` /
    /// `PlaySound` / similar so a headless run that never asks for
    /// audio doesn't open the host device. Falls back to a silent
    /// in-memory ring buffer when the `audio-cpal` feature is off.
    pub audio: AudioEngine,
    /// Tracks `waveOut*` and `PlaySound*` PCM headers the guest is
    /// asking us to play. Maps a fake handle (returned to the guest)
    /// to the format it was opened with. The headers themselves
    /// arrive through `waveOutWrite` and we copy the i16 samples
    /// into [`AudioEngine`] right then.
    pub wave_out_format: GuestFormat,
    /// Buffer-done bookkeeping for the emulated `waveOut` device.
    pub wave_out: WaveOutState,
    /// Clips and mixer settings for `hss.dll`. Independent of
    /// [`Self::wave_out`]: HSS games talk to the library's C++ classes
    /// and never touch `waveOut` themselves.
    pub hss: HssState,
    /// Messages posted with `PostMessageW` (and the `MM_WOM_DONE`
    /// notifications we synthesise for `waveOut`), waiting to be
    /// handed to the guest by `GetMessageW` / `PeekMessageW`.
    /// `(hwnd, msg, wparam, lparam)`.
    pub posted_messages: VecDeque<(u32, u32, u32, u32)>,
    pub msg_queues: HashMap<u32, MsgQueue>,
    pub next_msg_queue_handle: u32,
    /// Per-menu table of `(item_id -> flags)`. We track the flags so
    /// that `CheckMenuItem`/`GetMenuState` round-trip the previously
    /// set state instead of always returning the same constant —
    /// PPcAtaxx and similar games rely on the previous value to
    /// implement toggle semantics.
    pub menus: HashMap<u32, HashMap<u32, u32>>,
    /// Next menu handle to hand out from `LoadMenuW` / `CreateMenu`.
    /// Starts in the same `0xDEAD_xxxx` range as the rest of the
    /// synthetic handles so logs stay legible.
    pub next_menu_handle: u32,
    /// Per-`HMENU` map of position -> sub-menu handle. Pocket PC
    /// games call `GetSubMenu` repeatedly with the same `(menu, pos)`
    /// pair and expect the result to compare equal across calls;
    /// this caches the synthetic sub-menu we hand back so the
    /// invariant holds.
    pub sub_menus: HashMap<(u32, u32), u32>,
}

impl KernelState {
    /// Paint the built-in child controls on top of whatever the guest
    /// last drew, so the frame the host is about to show has them.
    ///
    /// `EndPaint` already repaints the controls, but the guest's
    /// `WM_PAINT` is several dirtying steps long — CERF BlankApp does
    /// `FillRect(white)`, then `BitBlt`s its logo, then `EndPaint` —
    /// and a frontend snapshots on *every* `frame_counter` change. A
    /// snapshot landing between the white fill and `EndPaint` shows a
    /// window with no children, which reads as flicker.
    ///
    /// A device has no such window: the display driver composites the
    /// child windows over the parent's pixels on the way to the panel,
    /// so a half-finished parent repaint can never hide them. This is
    /// that composite step, and it deliberately does *not* call
    /// [`Framebuffer::mark_dirty`] — it runs at the presentation
    /// boundary, and dirtying there would ask for another frame
    /// forever.
    pub fn composite_controls(&mut self) {
        if self.controls.is_empty() && self.modal.is_none() {
            return;
        }
        let controls = self.controls.clone();
        let modal = self.modal.clone();
        let mut surf = gdi::Surface::Screen(&mut self.framebuffer);
        controls.render(&mut surf);
        // The modal goes last: it is a separate top-level window and
        // has to cover whatever the application and its children drew.
        if let Some(modal) = modal {
            modal.render(&mut surf);
        }
    }

    /// Look up an already-loaded runtime module by its `HMODULE`.
    pub fn module_by_handle(&self, handle: u32) -> Option<&LoadedModule> {
        self.modules.iter().find(|m| m.handle == handle)
    }

    /// Look up an already-loaded runtime module by file name. `name` may
    /// carry a directory and/or mixed case; only the file stem+extension
    /// is compared, case-insensitively.
    pub fn module_by_name(&self, name: &str) -> Option<&LoadedModule> {
        let key = module_file_name(name);
        self.modules.iter().find(|m| m.name == key)
    }

    /// Resources and image base to search for a given `hModule`.
    ///
    /// A null / process instance handle means "the EXE", which is also
    /// what we fall back to for a handle we don't recognise: games are
    /// sloppy about what they pass here and the main image is the only
    /// sane default.
    pub fn resource_scope(&self, hmodule: u32) -> (&[ResourceEntry], u32) {
        if hmodule != 0 {
            if let Some(m) = self.module_by_handle(hmodule) {
                return (&m.resources, m.base);
            }
        }
        (&self.resources, self.image_base)
    }

    /// Every resource scope in the process, main image first.
    ///
    /// Used by the handlers that only get a resource *address* (or that
    /// were handed a bogus `hModule`) and have to search everywhere.
    pub fn resource_scopes(&self) -> impl Iterator<Item = (&[ResourceEntry], u32)> {
        std::iter::once((self.resources.as_slice(), self.image_base)).chain(
            self.modules
                .iter()
                .map(|m| (m.resources.as_slice(), m.base)),
        )
    }

    /// Find the host file backing a guest `LoadLibraryW` request.
    ///
    /// Tries the VFS first (so a CAB-mounted absolute path such as
    /// `\Program Files\Solitaire\pegcards.dll` resolves exactly), then
    /// falls back to a case-insensitive scan of [`Self::module_search_dirs`]
    /// for the bare file name — which is how a bare-EXE run finds the
    /// satellite DLLs sitting next to the executable.
    pub fn find_module_file(&self, request: &str) -> Option<PathBuf> {
        if request.contains('\\') || request.contains('/') {
            if let Some(p) = self.vfs.resolve(request) {
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        let want = module_file_name(request);
        for dir in &self.module_search_dirs {
            let direct = dir.join(&want);
            if direct.is_file() {
                return Some(direct);
            }
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                if name == want && entry.path().is_file() {
                    return Some(entry.path());
                }
            }
        }
        None
    }
}

/// Normalise a guest module request to a lower-cased bare file name with
/// a `.dll` extension (`"\\Windows\\PegCards"` -> `"pegcards.dll"`).
pub fn module_file_name(request: &str) -> String {
    let base = request
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(request)
        .to_ascii_lowercase();
    if base.contains('.') {
        base
    } else {
        format!("{base}.dll")
    }
}

/// Saved register context for one cooperative guest thread.
#[derive(Debug, Clone)]
pub struct GuestThread {
    pub entry: u32,
    pub parameter: u32,
    pub stack_top: u32,
    pub stack_size: u32,
    pub exit_va: u32,
    pub resume_pc: u32,
    pub handle: u32,
    /// Synthetic `GetCurrentThreadId` value. `PostThreadMessageW`
    /// targets a thread by this id, so it has to be stable and
    /// non-zero for the whole life of the thread.
    pub id: u32,
    /// Messages posted to *this thread* rather than to a window.
    /// `GetMessageW` running on the thread drains this queue; the
    /// window queue (`KernelState::posted_messages`) belongs to the
    /// main thread only.
    pub messages: std::collections::VecDeque<(u32, u32, u32)>,
    pub saved_regs: [u32; 17],
    pub worker_regs: [u32; 17],
    pub worker_saved: bool,
    pub started: bool,
    pub finished: bool,
}
#[allow(clippy::too_many_arguments)]
impl GuestThread {
    pub fn new(
        entry: u32,
        parameter: u32,
        stack_top: u32,
        stack_size: u32,
        exit_va: u32,
        resume_pc: u32,
        handle: u32,
        saved_regs: [u32; 17],
    ) -> Self {
        Self {
            entry,
            parameter,
            stack_top,
            stack_size,
            exit_va,
            resume_pc,
            handle,
            id: 0,
            messages: std::collections::VecDeque::new(),
            saved_regs,
            worker_regs: [0; 17],
            worker_saved: false,
            started: false,
            finished: false,
        }
    }
}

/// State of one in-flight `qsort` call.
///
/// The C library sorts in place with `int (*compar)(const void *, const
/// void *)`, and that comparison function is guest code, so the sort
/// cannot simply run inside the Rust handler. Instead the handler
/// performs a *binary* insertion sort and suspends at every comparison:
/// it points R0/R1 at the two elements, sets LR back to its own thunk
/// and `JumpTo`s the comparator. Binary insertion sort is the right
/// shape for that trade-off — it needs only `O(n log n)` comparisons
/// (each one an expensive guest round-trip) while the `O(n^2)` element
/// moves are plain host-side `memmove`s.
#[derive(Debug, Clone, Copy)]
pub struct QsortFrame {
    /// `base` — start of the array, from R0 on the first call.
    pub base: u32,
    /// `num` — element count, from R1.
    pub num: u32,
    /// `width` — `sizeof(element)`, from R2.
    pub width: u32,
    /// `compar` — guest comparison function, from R3.
    pub compar: u32,
    /// Index of the element currently being inserted (`1..num`).
    pub i: u32,
    /// Low bound of the binary search for element `i`'s new home.
    pub lo: u32,
    /// High bound of that binary search.
    pub hi: u32,
    /// Midpoint whose comparison result we are waiting for.
    pub mid: u32,
    /// LR on entry — where `qsort` returns once the array is sorted.
    pub saved_lr: u32,
}

/// One in-flight iteration of the MSVC C++ EH vector iterators
/// (`??_L@YAXPAXIHP6AX0@Z1@Z` / `??_M@YAXPAXIHP6AX0@Z@Z`).
///
/// Field naming mirrors the documented MSVC prototype:
/// ```text
///   void __cdecl `vector constructor iterator'(
///       void *  pBegin,
///       UINT    cbElement,
///       int     nElements,
///       void   (__cdecl *pCtor)(void *),
///       void   (__cdecl *pCleanupCtor)(void *));
///
///   void __cdecl `vector destructor iterator'(
///       void *  pBegin,
///       UINT    cbElement,
///       int     nElements,
///       void   (__cdecl *pDtor)(void *));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct VectorIterFrame {
    /// Pointer to element 0 (`??_L`) / element N-1 (`??_M`) of the
    /// array, captured from R0 on the very first call.
    pub p_begin: u32,
    /// `sizeof(T)` from R1.
    pub cb_element: u32,
    /// `N` from R2.
    pub n_elements: i32,
    /// Per-element callback pointer from R3 (ctor for `??_L`, dtor
    /// for `??_M`).
    pub p_func: u32,
    /// Cleanup ctor function pointer from `[sp+0]`. Used only by
    /// `??_L` to unwind partially-constructed arrays — `??_M` always
    /// stores `0` here.
    pub p_cleanup: u32,
    /// `true` for `??_M` (destructor iterator), which walks the
    /// array in reverse order.
    pub is_dtor: bool,
    /// Index of the next element to construct / destruct.
    pub i: i32,
    /// LR on entry, i.e. the address the iterator should return to
    /// once every element has been processed. Restored into LR on
    /// the final iteration's `ReturnedR0`.
    pub saved_lr: u32,
    /// Thunk this iteration is running on. `??_L` and `??_M` are
    /// separate imports with separate thunks, and both can be live
    /// at once.
    pub thunk_va: u32,
    /// Guest SP when the iterator was called. The per-element
    /// callback is entered with this exact SP and restores it before
    /// branching back to the thunk, so a re-entry whose SP still
    /// matches is that callback returning, while a lower SP means
    /// the callback has started a nested array of its own.
    pub sp_at_call: u32,
}

/// One IAT entry that has been resolved to a host-side stub.
#[derive(Debug, Clone)]
pub struct Thunk {
    pub thunk_va: u32,
    pub iat_va: u32,
    pub dll: String,
    pub binding: ImportBinding,
    /// Optional human-readable name used in logs (e.g. resolved from
    /// an ordinal map).
    pub friendly_name: Option<String>,
}

impl Thunk {
    pub fn label(&self) -> String {
        match (&self.binding, &self.friendly_name) {
            (_, Some(n)) => format!("{}!{}", self.dll, n),
            (ImportBinding::Name(n), _) => format!("{}!{}", self.dll, n),
            (ImportBinding::Ordinal(o), _) => format!("{}!#{}", self.dll, o),
        }
    }
}

/// Very small chunk-based heap allocator that hands out chunks from a
/// fixed guest VA range. Implemented as a free list of free blocks
/// keyed by start VA, with coalescing on free.
///
/// The expected use case is *games* that do a couple thousand small
/// allocations — fragmentation behaviour is fine for that. We do not
/// try to compete with `dlmalloc`. Each allocated block is preceded
/// by an 8-byte header so `free()` can recover the size and link the
/// block back into the free list.
#[derive(Debug)]
pub struct Heap {
    base: u32,
    size: u32,
    /// Sorted by start VA. Each entry is `(start, size)` of free space.
    free: Vec<(u32, u32)>,
    /// Out-of-band tracker of `(user_ptr -> requested_size)` for every
    /// outstanding allocation. We keep it host-side so the guest can
    /// not accidentally corrupt the bookkeeping by writing past its
    /// own buffer (Pocket PC games do this all the time). It also lets
    /// `Heap::msize(p)` answer in O(1).
    live: HashMap<u32, u32>,
}

const HEAP_HEADER_BYTES: u32 = 8;
const HEAP_ALIGN: u32 = 8;

impl Heap {
    pub fn new(base: u32, size: u32) -> Self {
        Self {
            base,
            size,
            free: vec![(base, size)],
            live: HashMap::new(),
        }
    }

    pub fn base(&self) -> u32 {
        self.base
    }
    pub fn size(&self) -> u32 {
        self.size
    }

    fn align_up(n: u32) -> u32 {
        (n + (HEAP_ALIGN - 1)) & !(HEAP_ALIGN - 1)
    }

    /// Return the user pointer (after the 8-byte header), or `None`
    /// if the heap has no large-enough free block.
    pub fn alloc(&mut self, requested: u32) -> Option<u32> {
        let need = Self::align_up(requested.max(1)) + HEAP_HEADER_BYTES;
        for i in 0..self.free.len() {
            let (start, sz) = self.free[i];
            if sz >= need {
                if sz == need {
                    self.free.remove(i);
                } else {
                    self.free[i] = (start + need, sz - need);
                }
                let user_ptr = start + HEAP_HEADER_BYTES;
                self.live.insert(user_ptr, requested);
                return Some(user_ptr);
            }
        }
        None
    }

    /// Look up the user-requested size of `user_ptr`, or `None` if the
    /// pointer is not the result of a still-live `Heap::alloc`.
    pub fn msize(&self, user_ptr: u32) -> Option<u32> {
        self.live.get(&user_ptr).copied()
    }

    /// Free a previously allocated chunk. The size is recovered from
    /// our live-block table; if the caller passes a bogus pointer we
    /// log and ignore.
    pub fn free(&mut self, user_ptr: u32) {
        if user_ptr == 0 {
            return;
        }
        let Some(user_size) = self.live.remove(&user_ptr) else {
            log::warn!("heap.free: unknown pointer 0x{user_ptr:08x} (double free?)");
            return;
        };
        if user_ptr < self.base + HEAP_HEADER_BYTES {
            log::warn!("heap.free: ignoring out-of-range pointer 0x{user_ptr:08x}");
            return;
        }
        let block_start = user_ptr - HEAP_HEADER_BYTES;
        let block_size = Self::align_up(user_size.max(1)) + HEAP_HEADER_BYTES;
        if block_start + block_size > self.base + self.size {
            log::warn!("heap.free: chunk overflows heap; ignoring");
            return;
        }
        // insert and coalesce
        let pos = self.free.partition_point(|(s, _)| *s < block_start);
        self.free.insert(pos, (block_start, block_size));
        // coalesce with neighbours
        let mut merged = Vec::with_capacity(self.free.len());
        for (s, sz) in self.free.drain(..) {
            if let Some((ps, psz)) = merged.last_mut() {
                if *ps + *psz == s {
                    *psz += sz;
                    continue;
                }
            }
            merged.push((s, sz));
        }
        self.free = merged;
    }

    pub fn free_bytes(&self) -> u32 {
        self.free.iter().map(|(_, s)| *s).sum()
    }
}

/// Fake `HMODULE` for the resident `ole32.dll` compatibility module.
pub const OLE32_MODULE_HANDLE: u32 = 0x1000_0007;
/// Fake `HMODULE` for `libGLES_CM.dll`, the Common profile.
pub const GLES_CM_MODULE_HANDLE: u32 = 0x1000_0004;
/// Fake `HMODULE` for `libGLES_CL.dll`, the Common-Lite profile.
pub const GLES_CL_MODULE_HANDLE: u32 = 0x1000_0005;
/// Fake `HMODULE` for the Hekkus Sound System compatibility layer.
pub const HSS_MODULE_HANDLE: u32 = 0x1000_0006;

/// The whole emulated process state owned by the kernel.
fn build_dynamic_exports(thunks: &[Thunk]) -> HashMap<u32, HashMap<String, u32>> {
    let mut exports = HashMap::new();
    let mut coredll = HashMap::new();
    let mut ddraw = HashMap::new();
    let mut gx = HashMap::new();
    let mut commctrl = HashMap::new();
    let mut ole32 = HashMap::new();
    // The OpenGL ES client libraries are imported purely by ordinal, so
    // the `friendly_name` the dispatcher attached during load is the
    // only thing that makes `GetProcAddress("glDrawElements")` work.
    let mut gles_cm = HashMap::new();
    let mut gles_cl = HashMap::new();
    for thunk in thunks {
        let name = match (&thunk.binding, &thunk.friendly_name) {
            (_, Some(name)) => name.clone(),
            (ImportBinding::Name(name), _) => name.clone(),
            (ImportBinding::Ordinal(ord), _) => format!("#{ord}"),
        };
        if thunk.dll.eq_ignore_ascii_case("coredll.dll") {
            coredll.insert(name, thunk.thunk_va);
            if let ImportBinding::Ordinal(ord) = &thunk.binding {
                coredll.insert(format!("#{}", ord), thunk.thunk_va);
            }
        } else if thunk.dll.eq_ignore_ascii_case("ddraw.dll") {
            ddraw.insert(name.clone(), thunk.thunk_va);
        } else if thunk.dll.eq_ignore_ascii_case("gx.dll") {
            gx.insert(name.clone(), thunk.thunk_va);
            if let ImportBinding::Ordinal(ord) = &thunk.binding {
                gx.insert(format!("#{}", ord), thunk.thunk_va);
            }
        } else if thunk.dll.eq_ignore_ascii_case("commctrl.dll") {
            commctrl.insert(name.clone(), thunk.thunk_va);
            if let ImportBinding::Ordinal(ord) = &thunk.binding {
                commctrl.insert(format!("#{}", ord), thunk.thunk_va);
            }
        } else if thunk.dll.eq_ignore_ascii_case("hss.dll") {
            let table = exports
                .entry(HSS_MODULE_HANDLE)
                .or_insert_with(HashMap::new);
            table.insert(name.clone(), thunk.thunk_va);
            if let ImportBinding::Ordinal(ord) = &thunk.binding {
                table.insert(format!("#{ord}"), thunk.thunk_va);
            }
        } else if thunk.dll.eq_ignore_ascii_case("ole32.dll") {
            ole32.insert(name.clone(), thunk.thunk_va);
        } else if thunk.dll.eq_ignore_ascii_case("libgles_cm.dll")
            || thunk.dll.eq_ignore_ascii_case("libgles_cl.dll")
        {
            let table = if thunk.dll.eq_ignore_ascii_case("libgles_cm.dll") {
                &mut gles_cm
            } else {
                &mut gles_cl
            };
            table.insert(name.clone(), thunk.thunk_va);
            if let ImportBinding::Ordinal(ord) = &thunk.binding {
                table.insert(format!("#{ord}"), thunk.thunk_va);
                table.insert(format!("ord:{ord}"), thunk.thunk_va);
            }
        }
    }
    if !coredll.is_empty() {
        exports.insert(0x1000_0000, coredll);
    }
    if !ddraw.is_empty() {
        exports.insert(0x1000_0003, ddraw);
    }
    if !commctrl.is_empty() {
        exports.insert(0x1000_0002, commctrl);
    }
    if !gx.is_empty() {
        exports.insert(0x1000_0001, gx);
    }
    if !gles_cm.is_empty() {
        exports.insert(GLES_CM_MODULE_HANDLE, gles_cm);
    }
    if !gles_cl.is_empty() {
        exports.insert(GLES_CL_MODULE_HANDLE, gles_cl);
    }
    if !ole32.is_empty() {
        exports.insert(OLE32_MODULE_HANDLE, ole32);
    }
    exports
}
pub struct Process {
    pub image: LoadedImage,
    pub thunks: Vec<Thunk>,
    pub thunk_by_va: HashMap<u32, usize>,
    pub stack_top: u32,
    pub stack_size: u32,
    pub state: KernelState,
}

impl Process {
    /// Map the image and synthesize thunks. Does **not** start the
    /// CPU.
    ///
    /// `dispatcher` is consulted at IAT install time so trivially
    /// constant-returning handlers (`zero_returning` / `one_returning`)
    /// can be patched directly into the IAT as `mov r0, #imm; bx lr`
    /// native thunks. Those imports then run inside the JIT without
    /// any callback overhead. Pass an empty stub dispatcher (e.g.
    /// [`NullDispatcher`]) if no API layer is installed yet — every
    /// import will then take the regular hooked dispatcher path at
    /// runtime.
    pub fn map_into(
        image: LoadedImage,
        cpu: &mut dyn Cpu,
        ordinal_resolver: &dyn Fn(&str, u16) -> Option<String>,
        dispatcher: &dyn Dispatcher,
    ) -> Result<Self, KernelError> {
        if let Some(runtime) = image.managed_runtime.as_deref() {
            return Err(KernelError::Loader(format!(
                "managed PE requires a .NET Compact Framework runtime ({runtime}); PocketHLE currently executes native ARM/MIPS WinCE images only"
            )));
        }
        if !matches!(
            image.machine,
            pocket_pe::machine::ARM
                | pocket_pe::machine::THUMB
                | pocket_pe::machine::ARMNT
                | pocket_pe::machine::MIPS_R3000
                | pocket_pe::machine::MIPS_R4000
        ) {
            return Err(KernelError::Loader(format!(
                "unsupported executable machine 0x{:04x}; PocketHLE's CPU backend executes ARM and MIPS WinCE images only",
                image.machine
            )));
        }
        // 1. Map every section.
        for s in &image.sections {
            let mut prot = Prot::READ;
            if s.is_writable() {
                prot |= Prot::WRITE;
            }
            if s.is_executable() {
                prot |= Prot::EXEC;
            }
            let aligned = pocket_cpu::round_up_to_page(s.virtual_size.max(s.data.len() as u32));
            cpu.map_region(image.image_base + s.virtual_address, aligned, prot)?;
            cpu.write_mem(image.image_base + s.virtual_address, &s.data)?;
            log::debug!(
                "mapped section {:>8} va=0x{:08x} size=0x{:x} prot={:?}",
                s.name,
                image.image_base + s.virtual_address,
                aligned,
                prot
            );
        }

        // WinCE ARM images also address their process-slot view through
        // low slot-relative pointers during CRT startup.
        //
        // Only worth aliasing when the image is based *above* slot 0.
        // An image based at 0x10000 (the default for Windows Mobile
        // EXEs) already has its sections mapped exactly where the
        // slot-relative view would put them, so the alias would be the
        // identity — and `mem_map` fails outright on the overlap,
        // which is what used to abort the load for any image whose
        // sections reach past 0x30000 (COD2's `.data` alone is 3 MiB).
        let slot_alias_end = SLOT_ALIAS_BASE + SLOT_ALIAS_SIZE;
        let image_overlaps_slot = image.sections.iter().any(|s| {
            let start = image.image_base.saturating_add(s.virtual_address);
            let size = pocket_cpu::round_up_to_page(s.virtual_size.max(s.data.len() as u32));
            start < slot_alias_end && start.saturating_add(size) > SLOT_ALIAS_BASE
        });
        let slot_alias_mapped = !image_overlaps_slot;
        if cpu.arch() == Arch::Arm {
            for section in &image.sections {
                if !section.is_executable() {
                    continue;
                }
                let section_start = image.image_base + section.virtual_address;
                for (offset, word) in section.data.windows(4).enumerate() {
                    if u32::from_le_bytes(word.try_into().expect("4-byte window"))
                        == WINCE_BREAKPOINT
                    {
                        cpu.add_instruction_hook(section_start + offset as u32)?;
                    }
                }
            }
        }
        if slot_alias_mapped {
            cpu.map_region(
                SLOT_ALIAS_BASE,
                SLOT_ALIAS_SIZE,
                Prot::READ | Prot::WRITE | Prot::EXEC,
            )?;
            for section in &image.sections {
                let alias = SLOT_ALIAS_BASE.saturating_add(section.virtual_address);
                let size = pocket_cpu::round_up_to_page(
                    section.virtual_size.max(section.data.len() as u32),
                );
                if alias >= SLOT_ALIAS_BASE && alias.saturating_add(size) <= slot_alias_end {
                    cpu.write_mem(alias, &section.data)?;
                }
            }
        }

        // 2. Allocate a thunk pool and patch the IAT to point into it.
        let thunk_count = image.imports.len() as u32;
        let thunk_size = pocket_cpu::round_up_to_page(thunk_count * THUNK_STRIDE).max(0x1000);
        cpu.map_region(THUNK_REGION_BASE, thunk_size, Prot::READ | Prot::EXEC)?;
        // Map the host-managed tick page (read-only from the guest's
        // POV — the host updates it via `cpu.write_mem` between
        // slices). The native `GetTickCount` thunk does a plain
        // `LDR` from this page, so it must be mapped before any
        // guest code runs. Mapping it `READ | WRITE` rather than
        // `READ` only, because Unicorn's `mem_write` on a strictly
        // read-only mapping fails on some host kernels.
        cpu.map_region(
            native_thunks::TICK_PAGE_VA,
            0x1000,
            Prot::READ | Prot::WRITE,
        )?;
        cpu.write_mem(native_thunks::TICK_PAGE_VA, &[0u8; 16])?;
        let mut thunks = Vec::with_capacity(image.imports.len());
        let mut thunk_by_va = HashMap::with_capacity(image.imports.len());
        // Thunk slots that trap into the dispatcher, in ascending
        // address order. Collected rather than hooked one at a time so
        // that contiguous runs can be installed as a single ranged
        // hook — see `install_coalesced_code_hooks`.
        let mut hooked_slots: Vec<u32> = Vec::with_capacity(image.imports.len());
        for (i, imp) in image.imports.iter().enumerate() {
            let thunk_va = THUNK_REGION_BASE + (i as u32) * THUNK_STRIDE;
            let friendly_name = match &imp.binding {
                ImportBinding::Name(n) => Some(n.clone()),
                ImportBinding::Ordinal(o) => ordinal_resolver(&imp.dll, *o),
            };
            // Pure-arithmetic CRT helpers (`__rt_sdiv`, `__adds`,
            // `__stoi`, …) get patched with hand-rolled native ARM /
            // VFP code so the JIT executes them inline with zero
            // callback overhead. Everything else falls back to the
            // original `bx lr` + code-hook path so the dispatcher can
            // service the call from Rust.
            let native_name: Option<&str> = match (&imp.binding, &friendly_name) {
                (_, Some(n)) => Some(n.as_str()),
                (ImportBinding::Name(n), _) => Some(n.as_str()),
                _ => None,
            };
            let native =
                if image.machine == machine::MIPS_R3000 || image.machine == machine::MIPS_R4000 {
                    None
                } else {
                    native_name.and_then(|n| native_thunks::native_thunk_for(&imp.dll, n))
                };
            // Build the thunk metadata up front so we can query the
            // dispatcher for a constant-return shortcut.
            let thunk = Thunk {
                thunk_va,
                iat_va: imp.iat_va,
                dll: imp.dll.clone(),
                binding: imp.binding.clone(),
                friendly_name,
            };
            // Pure constant-returning stubs (`zero_returning` /
            // `one_returning`) get a `mov r0, #imm; bx lr` patched
            // into the IAT instead of a code hook. That removes the
            // hook + dispatch round-trip on every call — in Derby's
            // hot loop, 33% of API calls (TranslateMessage,
            // TranslateAcceleratorW, DefWindowProcW) hit this path.
            let constant = if native.is_none() && cpu.arch() == Arch::Arm {
                dispatcher
                    .constant_for(&thunk)
                    .and_then(native_thunks::constant_return_thunk)
            } else {
                None
            };
            if let Some(words) = native.or(constant) {
                let bytes = native_thunks::thunk_bytes(&words);
                cpu.write_mem(thunk_va, &bytes)?;
            } else {
                let mut buf = [0u8; THUNK_STRIDE as usize];
                let stub = return_stub_bytes(cpu.arch());
                for (chunk, bytes) in buf.chunks_exact_mut(8).zip(stub.chunks_exact(8)) {
                    chunk.copy_from_slice(bytes);
                }
                cpu.write_mem(thunk_va, &buf)?;
                hooked_slots.push(thunk_va);
            }
            let mut iat_bytes = [0u8; 4];
            LittleEndian::write_u32(&mut iat_bytes, thunk_va);
            cpu.write_mem(imp.iat_va, &iat_bytes)?;
            let alias_iat = SLOT_ALIAS_BASE.wrapping_add(imp.iat_va.wrapping_sub(image.image_base));
            if slot_alias_mapped && (SLOT_ALIAS_BASE..slot_alias_end).contains(&alias_iat) {
                cpu.write_mem(alias_iat, &iat_bytes)?;
            }
            thunks.push(thunk);
            thunk_by_va.insert(thunk_va, i);
        }

        let mut dynamic_exports_to_add = Vec::new();
        for dll in [
            "coredll.dll",
            "commctrl.dll",
            "gx.dll",
            "ddraw.dll",
            "libgles_cm.dll",
            "libgles_cl.dll",
            "hss.dll",
            "ole32.dll",
        ] {
            dynamic_exports_to_add.extend(
                dispatcher
                    .dynamic_names(dll)
                    .into_iter()
                    .map(|name| (dll.to_string(), name)),
            );
        }
        let dynamic_base = THUNK_REGION_BASE + thunk_size + 0x1000;
        let dynamic_size = pocket_cpu::round_up_to_page(
            (dynamic_exports_to_add.len() as u32)
                .saturating_mul(THUNK_STRIDE)
                .max(0x1000),
        );
        if !dynamic_exports_to_add.is_empty() {
            cpu.map_region(dynamic_base, dynamic_size, Prot::READ | Prot::EXEC)?;
            for (index, (dll, name)) in dynamic_exports_to_add.into_iter().enumerate() {
                let thunk_va = dynamic_base + index as u32 * THUNK_STRIDE;
                let mut buf = [0u8; THUNK_STRIDE as usize];
                let stub = return_stub_bytes(cpu.arch());
                for (chunk, bytes) in buf.chunks_exact_mut(8).zip(stub.chunks_exact(8)) {
                    chunk.copy_from_slice(bytes);
                }
                cpu.write_mem(thunk_va, &buf)?;
                hooked_slots.push(thunk_va);
                thunks.push(Thunk {
                    thunk_va,
                    iat_va: 0,
                    dll,
                    binding: ImportBinding::Name(name.clone()),
                    friendly_name: Some(name),
                });
                thunk_by_va.insert(thunk_va, thunks.len() - 1);
            }
        }
        install_coalesced_code_hooks(cpu, &hooked_slots)?;

        // 3. Map a stack.
        let stack_size = DEFAULT_STACK_SIZE;
        let stack_top = DEFAULT_STACK_TOP;
        let dynamic_exports = build_dynamic_exports(&thunks);
        // Keep one writable guard page below the nominal stack base.
        // ARM prologues may pre-decrement SP before the first store, and
        // Total Commander reaches exactly that boundary during startup.
        let stack_base = stack_top - stack_size;
        cpu.map_region(
            stack_base - 0x2000,
            stack_size + 0x3000,
            Prot::READ | Prot::WRITE,
        )?;
        cpu.write_reg(ArmReg::Sp, stack_top - 16)?;
        cpu.write_reg(ArmReg::Lr, PROCESS_EXIT_TRAMPOLINE_VA)?;
        if image.machine != machine::MIPS_R3000
            && image.machine != machine::MIPS_R4000
            && image_uses_thumb_entry(cpu, &image)?
        {
            let cpsr = cpu.read_reg(ArmReg::Cpsr)? | (1 << 5);
            cpu.write_reg(ArmReg::Cpsr, cpsr)?;
            log::debug!("starting Thumb-mode entry with CPSR.T set");
        }

        // 4. Map a heap.
        cpu.map_region(HEAP_BASE, HEAP_SIZE, Prot::READ | Prot::WRITE)?;
        let mut heap = Heap::new(HEAP_BASE, HEAP_SIZE);

        // 4b. Publish the Windows CE process entry arguments.
        //
        // Unlike desktop Win32 — where the loader jumps to a
        // parameterless entry thunk and the CRT calls
        // `GetCommandLine` itself — the Windows CE loader invokes the
        // EXE entry point *with the four WinMain arguments already in
        // r0-r3*:
        //
        //     WinMainCRTStartup(HINSTANCE hInstance, HINSTANCE hPrev,
        //                       LPWSTR lpCmdLine, int nCmdShow)
        //
        // (see cegcc / mingw32ce `src/mingw/crt3.c`, the CE flavour of
        // mingw's `crt1.c`). CE always hands over a valid pointer for
        // `lpCmdLine` — an empty `L""` when the app was launched
        // without arguments, never NULL.
        //
        // Leaving r0-r3 at zero makes every game whose entry point
        // forwards its arguments straight into `WinMain` crash on the
        // first `wcslen(lpCmdLine)` or `hInstance` resource lookup.
        // Sonic Unleashed does exactly that twelve API calls into its
        // startup, which is why its frame counter never left zero.
        let cmd_line_va = heap.alloc(4).unwrap_or(0);
        if cmd_line_va != 0 {
            cpu.write_mem(cmd_line_va, &[0u8; 4])?;
        }
        cpu.write_reg(ArmReg::R0, PROCESS_INSTANCE_HANDLE)?;
        cpu.write_reg(ArmReg::R1, 0)?;
        cpu.write_reg(ArmReg::R2, cmd_line_va)?;
        cpu.write_reg(ArmReg::R3, SW_SHOWNORMAL)?;

        // 5. Map the WinCE kernel trap region. Real WinCE kernels
        //    publish syscall entry points at fixed offsets inside
        //    `0xF000_0000+`. We don't know the exact callsites
        //    coredll routes through this range, so we fill the page
        //    with `bx lr` — any guest jump there returns harmlessly.
        cpu.map_region(KERNEL_TRAP_BASE, KERNEL_TRAP_SIZE, Prot::READ | Prot::EXEC)?;
        let mut trap_page = Vec::with_capacity(KERNEL_TRAP_SIZE as usize);
        let trap_stub = return_stub_bytes(cpu.arch());
        while trap_page.len() < KERNEL_TRAP_SIZE as usize {
            trap_page.extend_from_slice(&trap_stub);
        }
        cpu.write_mem(KERNEL_TRAP_BASE, &trap_page)?;
        // Install code hooks on the well-known WinCE kernel-trap
        // entry points reached via the MS CRT __doexit path. The run
        // loop treats hits there as a soft `bx lr` return: real
        // games periodically jump through `0xF000_F7F8` / `_F7FC`
        // for syscalls (Sleep, EventModify, etc.) and we have no way
        // to dispatch those individually under HLE — but the trap
        // page is filled with `bx lr`, so a soft return mirrors the
        // "naked syscall returns straight back" behaviour. The
        // separate `pc < 0x1000` guard in `run_main_loop_with_hook`
        // still catches the actual `ExitProcess` case where the CRT
        // popped a poisoned LR == 0.
        for &exit_va in &[0xF000_F7F8u32, 0xF000_F7FCu32, 0xF000_FFFCu32] {
            cpu.add_code_hook(exit_va)?;
        }
        // Install the dedicated process-exit trampoline. The page is
        // already filled with `bx lr`; the run loop checks for hits
        // on this exact address and treats them as a graceful
        // shutdown (== `ExitProcess(0)`). Setting the initial `LR`
        // to this address (below) is what teaches the entry point
        // to come back here when its top-level frame returns.
        cpu.add_code_hook(PROCESS_EXIT_TRAMPOLINE_VA)?;

        // 6. Map the WinCE user-mode kernel data page. Pocket PC
        //    games and the MS C runtime read directly from this
        //    page (`KDataStruct` at `USER_KPAGE = 0xFFFF_C800`) to
        //    look up `lpvTls`, `ahSys[SH_CURTHREAD]`, and
        //    `ahSys[SH_CURPROC]`. Without this mapping any access
        //    crashes the game instantly with `READ_UNMAPPED`.
        cpu.map_region(
            USER_KDATA_PAGE_BASE,
            USER_KDATA_PAGE_SIZE,
            Prot::READ | Prot::WRITE,
        )?;
        let mut kdata_page = vec![0u8; USER_KDATA_PAGE_SIZE as usize];
        // The struct lives at offset 0x800 within the page.
        let struct_off = (USER_KDATA_STRUCT_VA - USER_KDATA_PAGE_BASE) as usize;
        // Field layout at the top of `KDataStruct`:
        //   +0x000  LPVOID lpvTls;          // per-thread TLS array
        //   +0x004  HANDLE ahSys[0];        // SH_WIN32 (unused here)
        //   +0x008  HANDLE ahSys[1];        // SH_CURTHREAD
        //   +0x00C  HANDLE ahSys[2];        // SH_CURPROC
        LittleEndian::write_u32(
            &mut kdata_page[struct_off..struct_off + 4],
            USER_KDATA_TLS_ARRAY_VA,
        );
        LittleEndian::write_u32(
            &mut kdata_page[struct_off + 8..struct_off + 12],
            FAKE_CURRENT_THREAD_HANDLE,
        );
        LittleEndian::write_u32(
            &mut kdata_page[struct_off + 12..struct_off + 16],
            FAKE_CURRENT_PROCESS_HANDLE,
        );
        cpu.write_mem(USER_KDATA_PAGE_BASE, &kdata_page)?;

        let resources = image.resources.clone();
        let img_base = image.image_base;
        let img_size = image.size_of_image;
        let img_entry = image.entry_va();
        // A game's satellite DLLs sit next to the EXE on a real device, so
        // the directory we loaded the EXE from is the natural search path
        // for `LoadLibraryW`. For a CAB/ZIP run that directory *is* the
        // extraction root, so this covers both cases.
        let module_search_dirs: Vec<PathBuf> = Path::new(&image.source_path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| vec![p.to_path_buf()])
            .unwrap_or_default();
        Ok(Process {
            image,
            thunks,
            thunk_by_va,
            stack_top,
            stack_size,
            state: KernelState {
                heap,
                vfs: vfs::Vfs::new(),
                module_path: DEFAULT_MODULE_PATH.to_string(),
                framebuffer: Framebuffer::default(),
                gdi: GdiState::new(),
                resources,
                image_base: img_base,
                image_size: img_size,
                image_entry: img_entry,
                dynamic_exports,
                next_module_handle: 0x1000_0001,
                modules: Vec::new(),
                next_module_base: MODULE_REGION_BASE,
                module_search_dirs,
                fb_mapped: false,
                direct_fb_frames: 0,
                gx_readback_scratch: Vec::new(),
                mem_op_scratch: Vec::new(),
                mem_op_scratch_b: Vec::new(),
                bit_blt_src_scratch: Vec::new(),
                dib_sync_scratch: Vec::new(),
                dib_decode_scratch: Vec::new(),
                gx_last_pushed_counter: 0,
                gx_guest_signature: None,
                gles_strings: HashMap::new(),
                synthetic_message_count: 0,
                synthetic_message_budget: 240,
                wnd_proc: 0,
                window_class_procs: HashMap::new(),
                window_background: None,
                pending_create: None,
                find_handles: HashMap::new(),
                next_find_handle: 0,
                pending_startup: std::collections::VecDeque::new(),
                registry: crate::registry::Registry::with_device_defaults(),
                window_procs: HashMap::new(),
                window_userdata: HashMap::new(),
                window_classes: HashMap::new(),
                window_user_data: 0,
                synthetic_timer_id: 0,
                synthetic_timer_interval_ms: 16,
                synthetic_timer_next_ms: 0,
                synthetic_paint_next_ms: 0,
                synthetic_create_sent: false,
                synthetic_size_sent: false,
                create_frame: None,
                create_stage: CreateStage::Idle,
                dialog_frame: None,
                message_frame: None,
                status_bar: None,
                controls: Default::default(),
                modal: None,
                message_box_spins: 0,
                modal_dialog: None,
                pending_input: VecDeque::new(),
                gapi_keys_queried: false,
                pending_message: None,
                threads: Vec::new(),
                events: Default::default(),
                semaphores: Default::default(),
                current_thread: 0,
                pressed_keys: [false; 256],
                should_stop: false,
                tls_slots_used: 0,
                vector_iter_stack: Vec::new(),
                qsort_frames: HashMap::new(),
                strtok_pos: 0,
                security_cookie: 0,
                audio: AudioEngine::new(),
                wave_out_format: GuestFormat::default(),
                wave_out: Default::default(),
                hss: Default::default(),
                posted_messages: Default::default(),
                msg_queues: HashMap::new(),
                next_msg_queue_handle: 0xDEAD_E500,
                menus: HashMap::new(),
                next_menu_handle: 0xDEAD_2000,
                sub_menus: HashMap::new(),
            },
        })
    }

    /// Look up the thunk by its hook address.
    pub fn find_thunk(&self, va: u32) -> Option<&Thunk> {
        self.thunk_by_va.get(&va).and_then(|i| self.thunks.get(*i))
    }

    /// Split-borrow helper used by [`run_main_loop_with_hook`]: returns
    /// the [`Thunk`] for `va` together with `&mut state`, in a single
    /// call so the borrow checker can see both fields are disjoint.
    /// Avoids the per-call `Thunk::clone()` that would otherwise be
    /// needed to release the immutable borrow on `self.thunks` before
    /// taking a mutable borrow on `self.state`.
    pub fn find_thunk_and_state(&mut self, va: u32) -> Option<(&Thunk, &mut KernelState)> {
        let idx = *self.thunk_by_va.get(&va)?;
        let thunk = self.thunks.get(idx)?;
        Some((thunk, &mut self.state))
    }

    /// Group import symbols by DLL — useful for printing a summary.
    pub fn imports_by_dll(&self) -> IndexMap<String, Vec<&ImportSymbol>> {
        let mut by_dll: IndexMap<String, Vec<&ImportSymbol>> = IndexMap::new();
        for imp in &self.image.imports {
            by_dll
                .entry(imp.dll.to_ascii_lowercase())
                .or_default()
                .push(imp);
        }
        by_dll
    }
}

/// Returned from a [`FrameHook`] to indicate whether emulation
/// should continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAction {
    Continue,
    Stop,
}

/// Callback that observes the framebuffer between dispatch slices.
/// Used by the host-side frontend to display the rendered frame and
/// pump window events.
///
/// The hook receives `&mut KernelState` so it can also push
/// [`InputEvent`]s onto [`KernelState::pending_input`] in response
/// to host UI input — that's how a desktop GUI forwards a tap on the
/// emulated screen into a `WM_LBUTTONDOWN` for the guest.
pub trait FrameHook {
    /// Called between dispatcher slices. The hook receives the
    /// kernel state and returns whether emulation should keep
    /// running.
    fn on_frame(&mut self, state: &mut KernelState) -> FrameAction;
}

impl<F: FnMut(&mut KernelState) -> FrameAction> FrameHook for F {
    fn on_frame(&mut self, state: &mut KernelState) -> FrameAction {
        self(state)
    }
}

fn sync_guest_framebuffer(cpu: &mut dyn Cpu, state: &mut KernelState) {
    if !state.fb_mapped || state.framebuffer.frame_counter != state.gx_last_pushed_counter {
        return;
    }
    let len = state.framebuffer.pixels.len();
    if state.gx_readback_scratch.len() != len {
        state.gx_readback_scratch.resize(len, 0);
    }
    if let Err(error) =
        cpu.read_mem_into(SYNTHETIC_FRAMEBUFFER_BASE, &mut state.gx_readback_scratch)
    {
        log::warn!("unable to read the GAPI framebuffer: {error}");
        return;
    }
    if state.gx_readback_scratch != state.framebuffer.pixels {
        state
            .framebuffer
            .pixels
            .copy_from_slice(&state.gx_readback_scratch);
        state.framebuffer.mark_dirty();
        state.gx_last_pushed_counter = state.framebuffer.frame_counter;
    }
}

/// Drive emulated execution in a loop, dispatching each thunk hit
/// through `dispatcher` until a [`DispatchOutcome::Halt`] is returned
/// or the configured instruction budget is exhausted.
pub fn run_main_loop(
    cpu: &mut dyn Cpu,
    process: &mut Process,
    dispatcher: &mut dyn Dispatcher,
    instruction_budget_per_slice: u64,
    max_slices: u64,
) -> Result<(), KernelError> {
    run_main_loop_with_hook(
        cpu,
        process,
        dispatcher,
        instruction_budget_per_slice,
        max_slices,
        None,
    )
}

/// Same as [`run_main_loop`], but also calls `frame_hook` between
/// each slice so the host-side window can repaint and pump events.
fn image_uses_thumb_entry(cpu: &mut dyn Cpu, image: &LoadedImage) -> Result<bool, KernelError> {
    if image.machine == machine::MIPS_R3000 || image.machine == machine::MIPS_R4000 {
        return Ok(false);
    }
    if image.machine != machine::THUMB && image.machine != machine::ARMNT {
        return Ok(false);
    }
    let entry = image.entry_va() & !1;
    let bytes = cpu.read_mem(entry, 4)?;
    let word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let arm_prologue = (word & 0x0fff_0000) == 0x01a0_0000
        || (word & 0x0fff_0000) == 0x092d_0000
        || (word & 0x0fff_0000) == 0x08bd_0000
        || (word & 0x0e00_0000) == 0x0a00_0000;
    if arm_prologue {
        log::debug!("entry bytes look like ARM code; loading image in ARM mode");
    }
    Ok(!arm_prologue)
}

pub fn run_main_loop_with_hook(
    cpu: &mut dyn Cpu,
    process: &mut Process,
    dispatcher: &mut dyn Dispatcher,
    instruction_budget_per_slice: u64,
    max_slices: u64,
    mut frame_hook: Option<&mut dyn FrameHook>,
) -> Result<(), KernelError> {
    let detected_thumb_mode = image_uses_thumb_entry(cpu, &process.image)?;
    let override_mode = std::env::var("POCKETHLE_ENTRY_MODE").ok();
    let thumb_mode = match override_mode.as_deref() {
        Some("arm") => false,
        Some("thumb") => true,
        Some(other) => {
            return Err(KernelError::Loader(format!(
                "invalid POCKETHLE_ENTRY_MODE={other:?}; expected arm or thumb"
            )))
        }
        None => detected_thumb_mode,
    };
    if (process.image.machine == machine::THUMB || process.image.machine == machine::ARMNT)
        && thumb_mode != detected_thumb_mode
    {
        log::info!(
            "POCKETHLE_ENTRY_MODE selected {} instead of automatic {} detection",
            if thumb_mode { "Thumb" } else { "ARM" },
            if detected_thumb_mode { "Thumb" } else { "ARM" }
        );
    }
    let mut pc = match std::env::var("POCKETHLE_OVERRIDE_ENTRY") {
        Ok(v) => {
            let parsed = if let Some(stripped) = v.strip_prefix("0x") {
                u32::from_str_radix(stripped, 16)
            } else {
                v.parse::<u32>()
            }
            .map_err(|_| KernelError::Loader("invalid POCKETHLE_OVERRIDE_ENTRY".into()))?;
            log::info!("POCKETHLE_OVERRIDE_ENTRY=0x{parsed:08x}");
            parsed
        }
        Err(_) => {
            let entry = process.image.entry_va() & !1;
            if thumb_mode {
                entry | 1
            } else {
                entry
            }
        }
    };
    log::info!(
        "entering emulated main: entry=0x{:08x}, stack_top=0x{:08x}",
        pc,
        process.stack_top
    );
    let mut slice = 0u64;
    let mut prof = crate::profile::Profiler::from_env();
    // Millisecond value already sitting in the tick page; see
    // [`native_thunks::refresh_tick_page`].
    let mut tick_written: Option<u32> = None;
    // Wall-clock gate for the presentation block; see
    // [`PRESENT_POLL_INTERVAL`]. `None` means "never presented", which
    // forces the first slice through so a game that paints once and
    // then idles still shows up.
    let mut last_present: Option<Instant> = None;
    let mut last_presented_frame = process.state.framebuffer.frame_counter;
    // Last time the guest announced a frame itself, and the counter
    // that told us; see [`PRESENT_POLL_BACKOFF`].
    let mut last_direct_frames = process.state.direct_fb_frames;
    let mut last_direct_present: Option<Instant> = None;
    loop {
        if max_slices != 0 && slice >= max_slices {
            break;
        }
        slice = slice.saturating_add(1);
        prof.count_slice();
        if prof.enabled() {
            prof.note_frames(process.state.framebuffer.frame_counter);
        }
        // PC=0 (or any address in the unmapped null page) means
        // the guest jumped through a null function pointer or popped
        // a poisoned LR off the stack. Without an explicit halt,
        // unicorn's `emu_start` typically returns `Ok(0 instructions)`
        // and we'd spin forever. Surface it as a real crash with the
        // CPU dump.
        if pc == PROCESS_EXIT_TRAMPOLINE_VA {
            log::info!("process exit trampoline reached at 0x{pc:08x}; shutting down");
            return Ok(());
        }
        if pc < 0x1000 {
            log::error!(
                "guest jumped to NULL/low address pc=0x{pc:08x}\n{regs}",
                regs = dump_regs(cpu),
            );
            return Err(KernelError::Loader(format!(
                "guest jumped to unmapped address 0x{pc:08x}"
            )));
        }
        // Unicorn stops with its PC parked on the hook that fired. The
        // continuation address is kept separately in `pc`, so restore it
        // explicitly before each new slice; otherwise a return from one
        // API thunk re-enters that same thunk forever.
        cpu.write_reg(ArmReg::Pc, pc)?;
        // Refresh the host-managed tick page so the native
        // `GetTickCount` thunk's plain `LDR` returns a fresh
        // millisecond count for any guest call inside this slice.
        // `tick_written` lets it skip the guest write when the
        // millisecond has not turned over yet, which is the common
        // case at ~280 slices per millisecond.
        let t_tick = prof.mark();
        native_thunks::refresh_tick_page(cpu, &mut tick_written);
        prof.add_tick(t_tick);
        let t_cpu = prof.mark();
        let stop = cpu.run_until_hook(pc, instruction_budget_per_slice);
        prof.add_cpu(t_cpu);
        let stop = match stop {
            Ok(s) => s,
            Err(e) => {
                let pc_now = cpu.read_reg(ArmReg::Pc).unwrap_or(pc);
                let sp_now = cpu.read_reg(ArmReg::Sp).unwrap_or(0);
                let image_base = process.image.image_base;
                let image_end = image_base.saturating_add(process.image.size_of_image);
                log::error!(
                        "cpu crashed: {e}\n  last requested pc=0x{pc:08x}, current pc=0x{pc_now:08x}\n{regs}{mem}{stack}",
                        regs = dump_regs(cpu),
                        mem = dump_mem_around(cpu, pc_now, 16),
                        stack = dump_stack_code_addrs(cpu, sp_now, 64, image_base, image_end),
                    );
                return Err(e.into());
            }
        };
        match stop {
            StopReason::InstructionLimit => {
                pc = cpu.read_reg(ArmReg::Pc)?;
                log::trace!("instruction slice exhausted; resuming at 0x{pc:08x}");
                continue;
            }
            StopReason::Hook(addr) => {
                // The synthetic process-exit trampoline is reached
                // when the guest entry point's top-level frame
                // returns and pops the seeded `LR` value into `PC`.
                // Treat it as a clean shutdown — equivalent to the
                // game calling `ExitProcess(0)` itself. Without this,
                // every Pocket PC game looks like it crashes
                // (pc=0x00000000) at the very end of execution.
                if addr == PROCESS_EXIT_TRAMPOLINE_VA {
                    log::info!(
                            "process exit trampoline hit at 0x{addr:08x} (R0=0x{r0:08x}); shutting down",
                            r0 = cpu.read_reg(ArmReg::R0).unwrap_or(0),
                        );
                    return Ok(());
                }
                if addr == KERNEL_TRAP_BASE {
                    if let Some(frame) = process.state.message_frame.take() {
                        for (index, value) in frame.args.iter().enumerate() {
                            cpu.write_reg(
                                match index {
                                    0 => ArmReg::R0,
                                    1 => ArmReg::R1,
                                    2 => ArmReg::R2,
                                    3 => ArmReg::R3,
                                    _ => unreachable!(),
                                },
                                *value,
                            )?;
                        }
                        cpu.write_reg(ArmReg::Sp, frame.sp)?;
                        cpu.write_reg(ArmReg::Lr, frame.lr)?;
                        pc = frame.lr;
                        continue;
                    }
                }
                if let Some(thread_index) = process
                    .state
                    .threads
                    .iter()
                    .position(|thread| thread.exit_va == addr && !thread.finished)
                {
                    let thread = process.state.threads[thread_index].clone();
                    if thread.worker_saved {
                        for (index, value) in thread.saved_regs.iter().enumerate() {
                            cpu.write_reg(
                                match index {
                                    0 => ArmReg::R0,
                                    1 => ArmReg::R1,
                                    2 => ArmReg::R2,
                                    3 => ArmReg::R3,
                                    4 => ArmReg::R4,
                                    5 => ArmReg::R5,
                                    6 => ArmReg::R6,
                                    7 => ArmReg::R7,
                                    8 => ArmReg::R8,
                                    9 => ArmReg::R9,
                                    10 => ArmReg::R10,
                                    11 => ArmReg::R11,
                                    12 => ArmReg::R12,
                                    13 => ArmReg::Sp,
                                    14 => ArmReg::Lr,
                                    15 => ArmReg::Pc,
                                    _ => ArmReg::Cpsr,
                                },
                                *value,
                            )?;
                        }
                        cpu.write_reg(ArmReg::R0, thread.handle)?;
                        process.state.threads[thread_index].finished = true;
                        process.state.current_thread = 0;
                        pc = thread.resume_pc;
                    } else {
                        let values = thread.saved_regs;
                        for (index, value) in values.iter().enumerate() {
                            cpu.write_reg(
                                match index {
                                    0 => ArmReg::R0,
                                    1 => ArmReg::R1,
                                    2 => ArmReg::R2,
                                    3 => ArmReg::R3,
                                    4 => ArmReg::R4,
                                    5 => ArmReg::R5,
                                    6 => ArmReg::R6,
                                    7 => ArmReg::R7,
                                    8 => ArmReg::R8,
                                    9 => ArmReg::R9,
                                    10 => ArmReg::R10,
                                    11 => ArmReg::R11,
                                    12 => ArmReg::R12,
                                    13 => ArmReg::Sp,
                                    14 => ArmReg::Lr,
                                    15 => ArmReg::Pc,
                                    _ => ArmReg::Cpsr,
                                },
                                *value,
                            )?;
                        }
                        process.state.current_thread = 0;
                        pc = thread.resume_pc;
                    }
                    log::debug!(
                        "guest thread {} returned; resuming main at 0x{:08x}",
                        thread_index,
                        pc
                    );
                    continue;
                }

                let is_wince_breakpoint = cpu
                    .read_mem(addr, 4)
                    .ok()
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u32::from_le_bytes)
                    == Some(WINCE_BREAKPOINT);
                if is_wince_breakpoint {
                    cpu.write_reg(ArmReg::Pc, addr + 4)?;
                    pc = addr + 4;
                    log::debug!("WinCE debugger breakpoint treated as a no-op at 0x{addr:08x}");
                    continue;
                }

                let outcome = match process.find_thunk_and_state(addr) {
                    Some((thunk, state)) => {
                        // Split borrow: `thunk` borrows
                        // `process.thunks` immutably and `state`
                        // borrows `process.state` mutably. No clone,
                        // and no per-call heap allocation — that
                        // matters because this branch fires on every
                        // single emulated WinCE API call.
                        let t_dispatch = prof.mark();
                        let dispatched = dispatcher.dispatch(cpu, thunk, state);
                        if prof.enabled() {
                            let va = thunk.thunk_va;
                            let label = thunk.label();
                            prof.add_dispatch(t_dispatch, va, || label);
                        }
                        let outcome = dispatched?;
                        if matches!(outcome, DispatchOutcome::Halt)
                            && log::log_enabled!(log::Level::Info)
                        {
                            log::info!("dispatcher requested halt at {}", thunk.label());
                        }
                        outcome
                    }
                    None => {
                        // A non-thunk code hook fired. The expected
                        // case is the WinCE kernel-trap region
                        // (0xF000_0000..) — every page there is
                        // pre-filled with `bx lr`, so a real device
                        // would just return straight to the caller.
                        // Treat it that way: log once at debug level
                        // and emulate `bx lr` ourselves (set PC to LR
                        // & ~1, continue). The `pc < 0x1000` guard
                        // at the top of the loop still catches the
                        // genuine "poisoned LR after ExitProcess"
                        // case where LR is 0.
                        if (KERNEL_TRAP_BASE..KERNEL_TRAP_BASE.saturating_add(KERNEL_TRAP_SIZE))
                            .contains(&addr)
                        {
                            if let Some(frame) = process.state.message_frame.take() {
                                for (index, value) in frame.args.iter().enumerate() {
                                    cpu.write_reg(
                                        match index {
                                            0 => ArmReg::R0,
                                            1 => ArmReg::R1,
                                            2 => ArmReg::R2,
                                            _ => ArmReg::R3,
                                        },
                                        *value,
                                    )?;
                                }
                                cpu.write_reg(ArmReg::Lr, frame.lr)?;
                                cpu.write_reg(ArmReg::Sp, frame.sp)?;
                                pc = frame.lr;
                                continue;
                            }
                            log::debug!(
                                    "kernel-trap soft-return at 0x{addr:08x} (R0=0x{r0:08x}, LR=0x{lr:08x})",
                                    r0 = cpu.read_reg(ArmReg::R0).unwrap_or(0),
                                    lr = cpu.read_reg(ArmReg::Lr).unwrap_or(0),
                                );
                            let lr = cpu.read_reg(ArmReg::Lr)?;
                            pc = lr;
                            // Skip the frame-hook for this slice —
                            // we did not actually advance the
                            // emulator, just bounced through a trap.
                            continue;
                        }
                        // Some other host-installed hook (e.g.
                        // `--watch` from the CLI). Dump CPU state
                        // and halt cleanly.
                        log::warn!(
                            "watch hit at 0x{addr:08x}\n{regs}{mem}",
                            regs = dump_regs(cpu),
                            mem = dump_mem_around(cpu, addr, 64),
                        );
                        return Ok(());
                    }
                };
                match outcome {
                    DispatchOutcome::Halt => {
                        return Ok(());
                    }
                    DispatchOutcome::ReturnedR0(v) => {
                        cpu.write_return(v)?;
                        let lr = cpu.read_reg(ArmReg::Lr)?;
                        pc = lr;
                    }
                    DispatchOutcome::ReturnedR0R1(a, b) => {
                        cpu.write_return_pair(a, b)?;
                        let lr = cpu.read_reg(ArmReg::Lr)?;
                        pc = lr;
                    }
                    DispatchOutcome::Unimplemented => {
                        cpu.write_return(0)?;
                        let lr = cpu.read_reg(ArmReg::Lr)?;
                        pc = lr;
                    }
                    DispatchOutcome::JumpTo(target) => {
                        // Trampoline into a guest function — `target` is
                        // the new PC, the handler is responsible for
                        // setting LR / R0..R3 / SP appropriately.
                        pc = target;
                    }
                }
            }
            StopReason::Requested | StopReason::OutOfBounds => return Ok(()),
        }
        if let Some(hook) = frame_hook.as_deref_mut() {
            // Present immediately when the guest has already announced
            // a new frame, otherwise only on the polling cadence — the
            // read-back that detects a software renderer's plain stores
            // is far too expensive to run once per API call.
            let counter = process.state.framebuffer.frame_counter;
            let now = Instant::now();
            // A guest-announced frame moves `frame_counter`, so this
            // block runs on the slice it happens and the lease is
            // always renewed from a fresh observation.
            if process.state.direct_fb_frames != last_direct_frames {
                last_direct_frames = process.state.direct_fb_frames;
                last_direct_present = Some(now);
            }
            let poll_interval = match last_direct_present {
                Some(then) if now.duration_since(then) < DIRECT_PRESENT_TRUST => {
                    PRESENT_POLL_BACKOFF
                }
                _ => PRESENT_POLL_INTERVAL,
            };
            let due = counter != last_presented_frame
                || last_present.is_none_or(|then| now.duration_since(then) >= poll_interval);
            if due {
                last_present = Some(now);
                let t_sync = prof.mark();
                sync_guest_framebuffer(cpu, &mut process.state);
                prof.add_fb_sync(t_sync);
                let t_controls = prof.mark();
                process.state.composite_controls();
                prof.add_controls(t_controls);
                let t_hook = prof.mark();
                let action = hook.on_frame(&mut process.state);
                prof.add_hook(t_hook);
                last_presented_frame = process.state.framebuffer.frame_counter;
                prof.note_present(last_presented_frame);
                if action == FrameAction::Stop {
                    log::info!("frame hook requested stop");
                    prof.note_frames(process.state.framebuffer.frame_counter);
                    return Ok(());
                }
            }
        }
        if process.state.should_stop {
            log::info!("frame hook flagged should_stop");
            return Ok(());
        }
    }
    if max_slices != 0 {
        let pc_now = cpu.read_reg(ArmReg::Pc).unwrap_or(0);
        log::warn!(
            "main loop hit max_slices={max_slices}; exiting at pc=0x{pc_now:08x}\n{regs}{mem}",
            regs = dump_regs(cpu),
            mem = dump_mem_around(cpu, pc_now, 16),
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::stub::StubCpu;
    use pocket_pe::{LoadedImage, LoadedSection};

    /// `SB_SETPARTS` hands us right-hand edges, with a trailing -1
    /// meaning "out to the right margin". Solitaire uses exactly
    /// `[70, -1]`, so part 1 must start at 70 and run to the edge.
    #[test]
    fn status_bar_part_spans_follow_edges() {
        let mut bar = StatusBar::default();
        bar.set_parts(vec![70, -1]);
        assert_eq!(bar.part_span(0, 240), (0, 70));
        assert_eq!(bar.part_span(1, 240), (70, 240));
    }

    /// An unset trailing edge behaves like -1 rather than collapsing
    /// the part to zero width.
    #[test]
    fn status_bar_missing_edge_runs_to_margin() {
        let mut bar = StatusBar::default();
        bar.set_parts(vec![50]);
        assert_eq!(bar.part_span(0, 200), (0, 50));
        assert_eq!(bar.part_span(1, 200), (50, 200));
    }

    /// `SB_SETTEXT` may address a high part before the lower ones
    /// exist; the sparse vector has to grow instead of panicking.
    #[test]
    fn status_bar_set_text_grows_sparsely() {
        let mut bar = StatusBar::default();
        bar.set_part_text(3, "Score: 0".into());
        assert_eq!(bar.part_text.len(), 4);
        assert_eq!(bar.part_text[3], "Score: 0");
        assert!(bar.part_text[0].is_empty());
        assert!(bar.dirty);
    }

    /// Re-setting identical text must not mark the bar dirty — the
    /// game re-sends both parts on every timer tick.
    #[test]
    fn status_bar_identical_text_is_not_dirty() {
        let mut bar = StatusBar::default();
        bar.set_part_text(0, "Time: 0 ".into());
        bar.dirty = false;
        bar.set_part_text(0, "Time: 0 ".into());
        assert!(!bar.dirty);
        bar.set_part_text(0, "Time: 1 ".into());
        assert!(bar.dirty);
    }

    /// The strip occupies the bottom `height` rows and nothing above.
    #[test]
    fn status_bar_renders_only_bottom_strip() {
        let mut bm = crate::gdi::Bitmap::new(64, 40);
        {
            let mut surf = Surface::Bitmap(&mut bm);
            surf.fill_rect(0, 0, 64, 40, 0x07E0); // green field
            let mut bar = StatusBar {
                height: 16,
                ..Default::default()
            };
            bar.set_parts(vec![30, -1]);
            bar.set_part_text(0, "Time: 0".into());
            bar.render(&mut surf);
        }
        let px = |x: usize, y: usize| -> u16 {
            let o = (y * 64 + x) * 2;
            u16::from_le_bytes([bm.pixels[o], bm.pixels[o + 1]])
        };
        // Untouched above the strip.
        assert_eq!(px(0, 0), 0x07E0);
        assert_eq!(px(63, 23), 0x07E0);
        // Highlight row, then face colour below it.
        assert_eq!(px(0, 24), 0xFFFF);
        assert_eq!(px(63, 39), 0xC618);
    }

    #[test]
    fn heap_alloc_then_free_round_trips() {
        let mut h = Heap::new(0x1000_0000, 0x1_0000);
        let initial_free = h.free_bytes();
        let a = h.alloc(64).unwrap();
        let b = h.alloc(128).unwrap();
        assert!(b > a);
        assert!(h.free_bytes() < initial_free);
        assert_eq!(h.msize(a), Some(64));
        assert_eq!(h.msize(b), Some(128));
        h.free(a);
        h.free(b);
        // After freeing both, the heap should be fully coalesced.
        assert_eq!(h.free_bytes(), initial_free);
    }

    #[test]
    fn heap_returns_aligned_pointers() {
        let mut h = Heap::new(0x1000_0000, 0x1_0000);
        let a = h.alloc(1).unwrap();
        let b = h.alloc(7).unwrap();
        assert_eq!(a % 8, 0);
        assert_eq!(b % 8, 0);
    }

    #[test]
    fn heap_exhaustion_returns_none() {
        let mut h = Heap::new(0x1000_0000, 0x80);
        let _ = h.alloc(60).unwrap();
        // Header overhead leaves only ~52 bytes free; 60 should fail.
        assert!(h.alloc(60).is_none());
    }

    /// A `StubCpu` that rejects overlapping `map_region` calls, the way
    /// `unicorn`'s `mem_map` does (`UC_ERR_MAP`). `StubCpu` itself is
    /// tolerant — it merges overlapping ranges into its page map — so a
    /// loader bug that double-maps an address range is invisible to it.
    /// That is exactly how the slot-0 alias collision reached a release
    /// build: every unit test passed while real ARM images based at
    /// `0x10000` failed to load at all.
    struct StrictMapCpu {
        inner: StubCpu,
        mapped: Vec<(u32, u32)>,
    }

    impl StrictMapCpu {
        fn new() -> Self {
            Self {
                inner: StubCpu::new(),
                mapped: Vec::new(),
            }
        }
    }

    impl Cpu for StrictMapCpu {
        fn arch(&self) -> Arch {
            self.inner.arch()
        }

        fn map_region(&mut self, va: u32, size: u32, prot: Prot) -> Result<(), CpuError> {
            let end = va.saturating_add(size);
            for (s, e) in &self.mapped {
                if va < *e && end > *s {
                    return Err(CpuError::Backend(format!(
                        "mem_map: MAP (0x{va:08x}..0x{end:08x} overlaps 0x{s:08x}..0x{e:08x})"
                    )));
                }
            }
            self.mapped.push((va, end));
            self.inner.map_region(va, size, prot)
        }

        fn write_mem(&mut self, va: u32, data: &[u8]) -> Result<(), CpuError> {
            self.inner.write_mem(va, data)
        }

        fn read_mem(&mut self, va: u32, len: u32) -> Result<Vec<u8>, CpuError> {
            self.inner.read_mem(va, len)
        }

        fn read_reg(&mut self, reg: pocket_cpu::regs::ArmReg) -> Result<u32, CpuError> {
            self.inner.read_reg(reg)
        }

        fn write_reg(&mut self, reg: pocket_cpu::regs::ArmReg, value: u32) -> Result<(), CpuError> {
            self.inner.write_reg(reg, value)
        }

        fn add_code_hook(&mut self, va: u32) -> Result<(), CpuError> {
            self.inner.add_code_hook(va)
        }

        fn add_code_hook_range(&mut self, lo: u32, hi: u32) -> Result<(), CpuError> {
            self.inner.add_code_hook_range(lo, hi)
        }

        fn add_instruction_hook(&mut self, va: u32) -> Result<(), CpuError> {
            self.inner.add_instruction_hook(va)
        }

        fn run_until_hook(
            &mut self,
            start_va: u32,
            max_instructions: u64,
        ) -> Result<StopReason, CpuError> {
            self.inner.run_until_hook(start_va, max_instructions)
        }

        fn request_stop(&mut self) {
            self.inner.request_stop()
        }
    }

    fn image_based_at(image_base: u32, text_size: u32) -> LoadedImage {
        LoadedImage {
            source_path: "test".into(),
            machine: pocket_pe::machine::ARM,
            subsystem: pocket_pe::subsystem::WINDOWS_CE_GUI,
            image_base,
            size_of_image: text_size + 0x1000,
            entry_point: 0x1000,
            sections: vec![LoadedSection {
                name: ".text".into(),
                virtual_address: 0x1000,
                virtual_size: text_size,
                characteristics: 0x6000_0020,
                data: vec![0u8; text_size as usize],
            }],
            imports: vec![],
            exports: IndexMap::new(),
            resources: vec![],
            managed_runtime: None,
        }
    }

    /// Regression test for the slot-0 alias collision. A standard
    /// Windows Mobile EXE is based at `0x10000` — the same address as
    /// the slot alias window — and its sections may extend well past
    /// the 128 KiB window (Call of Duty 2's `.data` alone is 3 MiB).
    /// Mapping the alias unconditionally made `mem_map` fail and
    /// aborted the load for every such image.
    #[test]
    fn image_based_in_slot_zero_does_not_double_map() {
        // 3 MiB of .text, comfortably past the 0x30000 alias end.
        let img = image_based_at(0x10000, 0x30_0000);
        let mut cpu = StrictMapCpu::new();
        let p = Process::map_into(img, &mut cpu, &|_, _| None, &NullDispatcher)
            .expect("an image based at 0x10000 must load without an alias collision");
        assert_eq!(p.image.entry_va(), 0x11000);
    }

    /// An image based *above* slot 0 still gets the alias window, since
    /// its CRT startup addresses itself through slot-relative pointers
    /// and nothing is mapped at `0x10000` to collide with.
    #[test]
    fn image_based_above_slot_zero_still_gets_alias() {
        let img = image_based_at(0x40_0000, 0x2000);
        let mut cpu = StrictMapCpu::new();
        Process::map_into(img, &mut cpu, &|_, _| None, &NullDispatcher)
            .expect("an image based above slot 0 must still load");
        // The alias window is present and readable.
        cpu.read_mem(SLOT_ALIAS_BASE + 0x1000, 4)
            .expect("slot alias window must be mapped for a high-based image");
    }

    #[test]
    fn map_simple_image() {
        let img = LoadedImage {
            source_path: "test".into(),
            machine: pocket_pe::machine::ARM,
            subsystem: pocket_pe::subsystem::WINDOWS_CE_GUI,
            image_base: 0x10000,
            size_of_image: 0x2000,
            entry_point: 0x1000,
            sections: vec![LoadedSection {
                name: ".text".into(),
                virtual_address: 0x1000,
                virtual_size: 0x800,
                characteristics: 0x6000_0020,
                data: vec![0u8; 0x800],
            }],
            imports: vec![],
            exports: IndexMap::new(),
            resources: vec![],
            managed_runtime: None,
        };
        let mut cpu = StubCpu::new();
        let p = Process::map_into(img, &mut cpu, &|_, _| None, &NullDispatcher).unwrap();
        assert_eq!(p.image.entry_va(), 0x11000);
        // The process must boot with `LR` pointing at the synthetic
        // exit trampoline so the run loop can detect a clean
        // return-from-main and shut down without the spurious
        // "guest jumped to 0x00000000" loader error.
        assert_eq!(
            cpu.read_reg(pocket_cpu::regs::ArmReg::Lr).unwrap(),
            PROCESS_EXIT_TRAMPOLINE_VA
        );
        // The kernel-trap page (`0xF000_0000..0xF000_FFFF`) must be
        // mapped read-only. Probing inside the trampoline page is
        // enough to assert the page is present.
        let _ = cpu
            .read_mem(PROCESS_EXIT_TRAMPOLINE_VA, 4)
            .expect("kernel trap page must be mapped");
    }
}

#[derive(Debug, Default)]
pub struct MsgQueue {
    pub max_messages: u32,
    pub max_message_size: u32,
    pub read_access: bool,
    pub messages: VecDeque<Vec<u8>>,
}

/// State for a Windows CE semaphore.
#[derive(Debug, Clone, Copy)]
pub struct SemaphoreObject {
    pub count: u32,
    pub max_count: u32,
}
