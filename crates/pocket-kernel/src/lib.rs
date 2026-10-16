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
pub mod bluetooth;
pub mod camera;
pub mod cedb;
pub mod colors;
pub mod controls;
pub mod device_status;
pub mod dll_lifecycle;
pub mod font;
pub mod framebuffer;
pub mod gapi;
pub mod gdi;
pub mod gps;
pub mod gz;
pub mod handles;
pub mod image_memory;
pub mod internet;
pub mod memory_division;
pub mod msgbox;
pub mod native_thunks;
pub mod power;
mod process_control;
pub mod registry;
pub mod sdl;
pub mod shared_objects;
mod tls;
pub mod tracker;
pub mod vfs;
pub mod vibration;
pub mod zlib;

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
// CE implicit-call ABI: FIRST_METHOD - ((api_set << 8) | method) * 4.
const CE_FIRST_METHOD: u32 = 0xF001_0000;
const CE_PROCESS_API_SET: u32 = 2;
const CE_PROCESS_TERMINATE_METHOD: u32 = 2;
pub const CE_TERMINATE_PROCESS_TRAP: u32 =
    CE_FIRST_METHOD - ((CE_PROCESS_API_SET << 8) | CE_PROCESS_TERMINATE_METHOD) * 4;
pub const CE_CURRENT_PROCESS_HANDLE: u32 = 64 + CE_PROCESS_API_SET;

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
/// Host scheduler continuation; never executed as guest ARM code.
pub const THREAD_SCHEDULER_IDLE_VA: u32 = 0xF000_0004;
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

/// Input and stop requests must progress even when the image is unchanged.
const FRONTEND_POLL_INTERVAL: Duration = Duration::from_millis(4);

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
    /// Fake HWAVEOUT this buffer belongs to.
    pub handle: u32,
    /// Guest address of the `WAVEHDR`.
    pub hdr: u32,
    /// Playback cursor, in guest samples since the last
    /// `waveOutOpen` / `waveOutReset`, at which this buffer is done.
    pub end_cursor: u64,
}

/// Guest memory behind a single-header waveOut loop. Interstellar Flames 2
/// keeps its SFX mixer ring here and edits it after waveOutWrite returns.
#[derive(Debug, Clone, Copy)]
pub struct LiveWaveLoop {
    pub hdr: u32,
    pub data: u32,
    pub bytes: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct WaveOutDevice {
    pub callback_kind: WaveCallbackKind,
    pub callback_target: u32,
    pub instance: u32,
    pub owner_thread: usize,
    pub format: GuestFormat,
    pub paused: bool,
}

/// State shared by all open `waveOut` handles.
#[derive(Debug, Default)]
pub struct WaveOutState {
    /// Legacy/current handle used while dispatching a callback.
    pub handle: u32,
    /// Independently opened WinMM output handles.
    pub devices: HashMap<u32, WaveOutDevice>,
    pub live_loops: HashMap<u32, LiveWaveLoop>,
    /// Monotonic allocator for synthetic HWAVEOUT values.
    pub next_handle: u32,
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
    pub function_done: VecDeque<(u32, u32, u32, u32)>,
    /// Driver completions awaiting delivery to an already-signalled auto-reset
    /// callback event. A large host block must not collapse several returns.
    pub event_done: VecDeque<u32>,
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
    /// Give time-sensitive workers a turn when guest code exhausts a CPU slice
    /// without entering an API. Implementations must preserve the exact main
    /// continuation, including its registers and ARM/Thumb state.
    fn schedule_cpu_slice(
        &mut self,
        _cpu: &mut dyn Cpu,
        _kernel: &mut KernelState,
    ) -> Result<Option<u32>, KernelError> {
        Ok(None)
    }

    fn schedule_idle(
        &mut self,
        _cpu: &mut dyn Cpu,
        _kernel: &mut KernelState,
    ) -> Result<DispatchOutcome, KernelError> {
        Ok(DispatchOutcome::JumpTo(THREAD_SCHEDULER_IDLE_VA))
    }

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
/// Resource satellites remain passive; native runtime DLLs have relocated
/// code, patched imports, and exports registered in `dynamic_exports`.
#[derive(Debug, Clone)]
pub struct LoadedModule {
    /// Native import edges; lifetime follows reachability from explicit loads.
    pub dependencies: Vec<u32>,
    pub attached: bool,
    /// Resource satellites retained on behalf of this module.
    pub satellites: Vec<u32>,
    /// Exact mapped ranges, including import thunks, for teardown and RAM accounting.
    pub resident_regions: Vec<(u32, u32)>,
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
    /// Explicit `LoadLibrary` references (plus satellite owner references).
    /// Native import edges retain modules through graph reachability.
    pub refcount: u32,
}

#[derive(Debug, Clone)]
pub struct GuestFileView {
    pub mapping_handle: u32,
    pub allocation: u32,
    pub offset: u64,
    pub length: u32,
    pub write_back: bool,
}

/// Pending DLL lifetime callback, returned through its loader API thunk.
#[derive(Debug, Clone)]
pub struct ModuleAttachFrame {
    pub sequence: Vec<u32>,
    pub cleanup: Vec<u32>,
    pub completed: Vec<u32>,
    pub result: u32,
    pub error: Option<u32>,
    pub detaching: bool,
    pub base: u32,
    pub thunk: u32,
    pub thread: usize,
    pub frame: GuestCallFrame,
}

/// Mutable kernel state that persists across calls and that handlers
/// need to read or modify. Bundled into one struct so we can hand it
/// out by `&mut` without conflicting with the immutable parts of
/// [`Process`] (image bytes, thunk table) that the run loop uses.
/// Foreground process handoff requested by a guest launcher.
#[derive(Debug, Clone)]
pub struct ProcessLaunch {
    pub concurrent: bool,
    pub command_line: String,
    pub creation_flags: u32,
    pub call_key: (usize, u32, u32),
    pub executable: PathBuf,
    pub guest_path: String,
    pub process_handle: u32,
    pub thread_handle: u32,
}

#[derive(Debug, Clone)]
pub struct ChildProcess {
    pub thread_handle: u32,
    pub process_handle_closed: bool,
    pub thread_handle_closed: bool,
    pub exit_code: Option<u32>,
}

/// One kernel-object domain shared by a launcher and its foreground children.
#[derive(Clone)]
pub struct ProcessHandleContext {
    pub table: handles::HandleTable,
    vfs_shared: vfs::VfsShared,
    events: shared_objects::SharedObjects<EventObject>,
    semaphores: shared_objects::SharedObjects<SemaphoreObject>,
    mutexes: shared_objects::SharedObjects<MutexObject>,
}
#[derive(Debug, Default)]
pub struct MutexObject {
    pub owner: Option<(u32, usize)>,
    pub depth: u32,
    pub abandoned: bool,
}

pub struct KernelState {
    pub heap: Heap,
    pub memory_division: Option<memory_division::MemoryDivision>,
    pub vfs: vfs::Vfs,
    pub file_views: HashMap<u32, GuestFileView>,
    pub internet: internet::State,
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
    /// Only frontends with foreground handoff support may expose SD launch.
    pub process_launch_enabled: bool,
    pub pending_process_launch: Option<ProcessLaunch>,
    pub command_line_cache: Option<u32>,
    pub command_line: Option<String>,
    pub process_launch_results: HashMap<(usize, u32, u32), Result<u32, u32>>,
    pub child_processes: HashMap<u32, ChildProcess>,
    pub next_process_id: u32,
    pub process_exit_code: Option<u32>,
    pub main_thread: MainThreadState,
    /// Closed aliases remain as tombstones to reject stale handles.
    pub object_handles: handles::HandleTable,
    pub random_seed: u32,
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
    pub runtime_thunks: HashMap<u32, Thunk>,
    pub module_attach_frames: Vec<ModuleAttachFrame>,
    pub dll_notification_frame: Option<dll_lifecycle::DllNotificationFrame>,
    pub next_module_handle: u32,
    /// DLLs the guest brought in at runtime via `LoadLibraryW`, in load
    /// order. Contains resource satellites and executable companion DLLs;
    /// HLE system DLLs use fixed synthetic handles.
    pub modules: Vec<LoadedModule>,
    /// Base address the next `LoadLibraryW` will map at. Starts at
    /// [`MODULE_REGION_BASE`] and walks up by [`MODULE_REGION_STRIDE`].
    pub next_module_base: u32,
    /// Slots returned by completed module teardown.
    pub free_module_bases: Vec<u32>,
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
    /// Scanline pitch in the guest's raw framebuffer mapping. Zero means the canonical tightly packed stride.
    pub guest_fb_pitch: u32,
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
    /// `BeginPaint` and `DefWindowProc(WM_ERASEBKGND)` use this brush
    /// when an erase is pending. An app that paints only its controls —
    /// HelloWorld draws nothing but a `STATIC` and a `BUTTON` — relies on
    /// the class brush for every other pixel.
    pub window_background: Option<u32>,
    /// Whether the next paint should erase the client area with its class
    /// background. Synthetic periodic `WM_PAINT` messages do not imply an
    /// erase; initial window painting and `InvalidateRect(..., TRUE)` do.
    pub window_background_erase_pending: bool,
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
    pub find_handles: HashMap<u32, std::collections::VecDeque<(String, u64, bool, u32)>>,
    /// Next `FindFirstFileW` handle to hand out.
    pub next_find_handle: u32,
    /// In-memory Windows CE registry. Seeded from the CAB's
    /// `_setup.xml` `Registry` section so a title finds the values its
    /// installer would have written.
    pub registry: crate::registry::Registry,
    pub cedb: crate::cedb::DatabaseStore,
    /// Window handles and their class procedures.
    pub window_procs: HashMap<u32, u32>,
    /// Window handles and their user data pointers.
    pub window_userdata: HashMap<u32, u32>,
    /// Window handles and their class names.
    pub window_classes: HashMap<u32, String>,
    /// Per-window user data used by WndProc implementations (GWL_USERDATA).
    pub window_user_data: u32,
    /// Timers keyed by (owning thread, window, ID); values are
    /// (interval in ms, host deadline, optional guest TIMERPROC).
    pub timers: HashMap<(usize, u32, u32), (u32, u64, u32)>,
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
    pub accelerator_frames: Vec<(usize, u32, GuestCallFrame)>,
    pub shell_status: Option<u32>,
    /// Per-thread DispatchMessageW continuations. A worker may run while
    /// another thread is inside WndProc; its callback return must restore
    /// only its own arguments/stack, never another thread's continuation.
    pub message_frames: HashMap<usize, GuestCallFrame>,
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
    pub events: shared_objects::SharedObjects<EventObject>,
    /// CreateSemaphoreW objects keyed by the fake handle we handed the
    /// guest. See [`SemaphoreObject`].
    pub semaphores: shared_objects::SharedObjects<SemaphoreObject>,
    pub mutexes: shared_objects::SharedObjects<MutexObject>,
    /// Index of the thread whose register context is currently active.
    pub current_thread: usize,
    /// Last-error values are isolated by guest thread, including main (0).
    pub thread_last_errors: HashMap<usize, u32>,
    /// Winsock errors belong to the calling thread and survive status reads.
    pub winsock_last_errors: HashMap<usize, u32>,
    pub crt_new_handler: u32,
    pub cpp_new_handler: u32,
    /// Next worker index to consider at a cooperative scheduling point.
    pub worker_schedule_cursor: usize,
    /// Workers already run since the main thread last yielded.
    pub worker_round_seen: Vec<usize>,
    pub worker_preempt_after_ms: u64,
    /// Finite wait deadlines keyed by guest thread, API thunk and caller SP.
    /// Re-entering a blocked call must not restart its timeout.
    pub wait_deadlines: HashMap<(usize, u32, u32), u64>,
    pub guest_fpscr: HashMap<usize, u32>,
    pub timer_period_requests: std::collections::BTreeMap<u32, u32>,
    /// Critical section address -> (owning guest thread index, recursion depth).
    pub critical_sections: HashMap<u32, (usize, u32)>,
    /// Current state of the Pocket PC virtual keys.
    pub pressed_keys: [bool; 256],
    pub cursor_pos: (u16, u16),
    /// Virtual keys the host is holding down, oldest press first.
    ///
    /// A host frontend sends one `KeyDown` when the user presses a key
    /// and one `KeyUp` when they let go, so without this the guest sees
    /// a single `WM_KEYDOWN` no matter how long the key is held. A real
    /// Pocket PC keypad auto-repeats, and a game that steers from
    /// `WM_KEYDOWN` rather than `GetAsyncKeyState` therefore moved
    /// exactly one step per press — fine for a menu, unplayable in
    /// JumpyBall. The pump in `pocket-winceapi::coredll` walks this
    /// list to fabricate the repeats.
    pub held_keys: Vec<u16>,
    /// Monotonic-ms deadline for the next fabricated key repeat.
    /// `None` means nothing is held, so no repeat is pending.
    pub key_repeat_next_ms: Option<u64>,
    /// Round-robin cursor into [`Self::held_keys`], so two keys held at
    /// once (diagonal on the D-pad) each get their share of repeats
    /// instead of the first one starving the second.
    pub key_repeat_cursor: usize,
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
    /// Owner of the active guest KData TLS window; other threads are saved.
    pub tls_owner: usize,
    pub thread_tls: HashMap<usize, [u32; TLS_SLOT_COUNT as usize]>,
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
    /// Host-side gzip file table backing the `alib.dll` HLE
    /// (`gzopen` / `gzread` / `gzseek` / `gzclose`). Rayman Ultimate
    /// loads every map asset through these imports.
    pub gz_files: crate::gz::GzFiles,
    pub zlib_streams: crate::zlib::ZlibStreams,
    pub sdl: crate::sdl::SdlState,
    /// Messages posted with `PostMessageW` (and the `MM_WOM_DONE`
    /// notifications we synthesise for `waveOut`), waiting to be
    /// handed to the guest by `GetMessageW` / `PeekMessageW`.
    /// `(hwnd, msg, wparam, lparam)`.
    pub posted_messages: VecDeque<(u32, u32, u32, u32)>,
    pub msg_queues: HashMap<u32, MsgQueue>,
    pub next_msg_queue_handle: u32,
    pub power_notifications: power::Notifications,
    pub battery_alerts: power::GizmondoAlerts,
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

/// Main thread state is independent of the process and of worker return frames.
#[derive(Default)]
pub struct MainThreadState {
    /// The mapping is released at thread exit; exit status outlives it.
    pub stack_region: Option<(u32, u32)>,
    pub exit_code: Option<u32>,
    pub suspend_count: u32,
    pub last_exit_code: Option<u32>,
    pub saved_regs: Option<[u32; 17]>,
}

impl KernelState {
    /// Colors writes the shared device registry from its own brightness slider.
    pub fn sync_power_notifications(&mut self) {
        if !self.power_notifications.is_empty() || !self.battery_alerts.is_empty() {
            let current = power::status(false);
            self.power_notifications
                .deliver(&mut self.msg_queues, current);
            if self.wnd_proc != 0 {
                self.battery_alerts
                    .deliver(&mut self.posted_messages, current);
            }
        }
    }
    pub fn sync_backlight(&mut self) {
        let level = match self
            .registry
            .value(r"HKCU\ControlPanel\Backlight", "OnLevel")
        {
            Some(crate::registry::RegistryValue::Dword(level)) => level,
            _ => 11,
        };
        self.framebuffer.set_backlight_level(level);
    }

    pub fn child_handle_context(&mut self, handle: u32) -> Option<ProcessHandleContext> {
        self.publish_handles();
        Some(ProcessHandleContext {
            table: self.object_handles.child(handle)?,
            vfs_shared: self.vfs.shared_context(),
            events: self.events.clone(),
            semaphores: self.semaphores.clone(),
            mutexes: self.mutexes.clone(),
        })
    }
    pub fn reclaim_finished_stacks(&mut self, cpu: &mut dyn Cpu) -> Result<(), KernelError> {
        self.reclaim_finished_tls();
        let mut retired = Vec::new();
        if self.main_thread.exit_code.is_some() {
            if let Some((base, size)) = self.main_thread.stack_region {
                self.object_handles
                    .set_exit(Some(0), self.main_thread.exit_code.unwrap());
                retired.push(0);
                cpu.unmap_region(base, size)?;
                self.heap.release_resident(base, size);
                self.main_thread.stack_region = None;
            }
        }
        for (index, thread) in self.threads.iter_mut().enumerate() {
            if !thread.finished {
                continue;
            }
            if let Some((base, size, private)) = thread.stack_region {
                self.object_handles
                    .set_exit(Some(index + 1), thread.exit_code.unwrap_or(0));
                retired.push(index + 1);
                if private {
                    self.heap.release_stack(base, size);
                } else {
                    cpu.unmap_region(base, size)?;
                    self.heap.release_resident(base, size);
                }
                thread.stack_region = None;
            }
        }
        if !retired.is_empty() {
            self.accelerator_frames
                .retain(|(thread, _, _)| !retired.contains(thread));
            let pid = self.object_handles.process_id();
            self.mutexes.retain(|_, mutex| {
                if mutex
                    .owner
                    .is_some_and(|(owner, index)| owner == pid && retired.contains(&index))
                {
                    mutex.owner = None;
                    mutex.depth = 0;
                    mutex.abandoned = true;
                }
                true
            });
        }
        let dead_calls: Vec<_> = self
            .process_launch_results
            .keys()
            .copied()
            .filter(|(thread, _, _)| {
                if *thread == 0 {
                    self.main_thread.exit_code.is_some()
                } else {
                    self.threads.get(*thread - 1).is_some_and(|t| t.finished)
                }
            })
            .collect();
        for key in dead_calls {
            if let Some(Ok(handle)) = self.process_launch_results.remove(&key) {
                if let Some(table) = self.object_handles.child(handle) {
                    table.terminate_remote_process(table.process_id(), 0xc0000001);
                    table.allow_start();
                }
            }
        }
        Ok(())
    }
    pub fn attach_handle_context(&mut self, context: ProcessHandleContext) {
        self.vfs.attach_shared_context(context.vfs_shared);
        self.object_handles = context.table;
        self.events = context.events;
        self.semaphores = context.semaphores;
        self.mutexes = context.mutexes;
        self.sync_transferred_handles();
    }
    /// Publish primary handles before the parent is suspended. Resources keep
    /// their open description; only the receiving process gets the new alias.
    pub fn publish_handles(&mut self) {
        for (index, thread) in self.threads.iter().enumerate() {
            if !thread.handle_closed {
                self.object_handles
                    .bind(thread.handle, handles::HandleObject::Thread(index + 1));
            }
            if let Some(code) = thread.exit_code {
                self.object_handles.set_exit(Some(index + 1), code);
            }
        }
        if let Some(code) = self.main_thread.exit_code {
            self.object_handles.set_exit(Some(0), code);
        }
        for handle in self.vfs.open_handles() {
            if self.object_handles.is_closed(handle) {
                continue;
            }
            let object = if self.vfs.is_open(handle) {
                handles::HandleObject::File(handle)
            } else {
                handles::HandleObject::Device(handle)
            };
            self.object_handles.bind(handle, object);
            if let Some(mut file) = self.vfs.export_handle(handle) {
                if self.vfs.is_mp3_decoder(handle) {
                    let key = self.vfs.mp3_stream_key(handle);
                    file = file.with_mas(key, self.audio.export_mas_stream(key));
                }
                self.object_handles
                    .export_file(self.object_handles.get(handle).unwrap(), file);
            }
        }
    }
    pub fn sync_transferred_handles(&mut self) {
        for (&handle, child) in &mut self.child_processes {
            if let Some(pid) = self.object_handles.child_id(handle) {
                if let Some(code) = self.object_handles.exit_code(pid, None) {
                    child.exit_code = Some(code);
                }
            }
        }

        for (handle, object) in self.object_handles.take_files() {
            if let Some((key, playback)) = object.mas() {
                self.audio.import_mas_stream(key, playback);
            }
            self.vfs.import_handle(handle, object);
        }
        for handle in self.vfs.open_handles() {
            if self.object_handles.is_closed(handle) {
                if self.vfs.is_mp3_decoder(handle) {
                    let key = self.vfs.mp3_stream_key(handle);
                    self.vfs.close(handle);
                    if !self
                        .vfs
                        .mp3_decoder_handles()
                        .iter()
                        .any(|&h| self.vfs.mp3_stream_key(h) == key)
                    {
                        self.audio.detach_mas_stream(key);
                    }
                } else {
                    self.vfs.close(handle);
                }
            }
        }
        if let Some(code) = self.main_thread.exit_code {
            self.object_handles.set_exit(Some(0), code);
        }
        for (index, thread) in self.threads.iter().enumerate() {
            if let Some(code) = thread.exit_code {
                self.object_handles.set_exit(Some(index + 1), code);
            }
        }
    }
    fn clean_object(&mut self, object: handles::HandleObject, last: bool) {
        if !last {
            return;
        }
        match object {
            handles::HandleObject::Event(key) => {
                self.events.remove(&key);
            }
            handles::HandleObject::Semaphore(key) => {
                self.semaphores.remove(&key);
            }
            handles::HandleObject::Mutex(key) => {
                self.mutexes.remove(&key);
            }
            _ => {}
        }
    }

    /// Attach one device budget to every allocator before guest execution.
    /// A failed attachment leaves the previous profile intact.
    pub fn configure_memory_division(
        &mut self,
        division: Option<memory_division::MemoryDivision>,
    ) -> bool {
        let previous = self.memory_division.clone();
        if !self.heap.attach_ram(division.clone()) {
            return false;
        }
        if !self.registry.attach_ram(division.as_ref()) {
            self.heap.attach_ram(previous);
            return false;
        }
        self.vfs.attach_ram(division.clone());
        self.memory_division = division;
        true
    }

    /// Record process termination without changing the suspended parent process.
    pub fn record_process_exit(&mut self, code: u32) {
        self.process_exit_code = Some(code);
        self.tls_slots_used = 0;
        self.thread_tls.clear();
        self.dll_notification_frame = None;
        self.object_handles.set_exit(None, code);
        self.object_handles.mark_inactive();
        self.main_thread.exit_code.get_or_insert(code);
        for thread in &mut self.threads {
            if !thread.finished {
                thread.finished = true;
                thread.exit_code = Some(code);
            }
        }
        self.message_frames.clear();
        self.accelerator_frames.clear();
        self.shell_status = None;
        self.internet.gprs_connected = false;
        self.sync_transferred_handles();
        let pid = self.object_handles.process_id();
        self.mutexes.retain(|_, mutex| {
            if mutex.owner.is_some_and(|(owner, _)| owner == pid) {
                mutex.owner = None;
                mutex.depth = 0;
                mutex.abandoned = true;
            }
            true
        });
    }

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
    /// Owned mapping (base, bytes, inside the pre-mapped heap arena).
    pub stack_region: Option<(u32, u32, bool)>,
    pub dll_thread_attach_delivered: bool,
    pub dll_thread_detach_delivered: bool,
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
    pub suspend_count: u32,
    /// Monotonic millisecond deadline set by worker Sleep; zero is ready.
    pub sleep_until_ms: u64,
    pub finished: bool,
    /// Termination status, retained after the thread stops.
    pub exit_code: Option<u32>,
    /// Closing a handle does not terminate its thread.
    pub handle_closed: bool,
    /// Set while the thread is parked on a *re-entering* blocking call
    /// (`park_worker_and_reevaluate`): the
    /// parked state will re-run the same API call. It is eligible only
    /// when its message queue/object is ready or its finite wait deadline
    /// has expired — see `resume_worker_reenter`.
    pub parked_in_pump: bool,
    /// Waitable handles associated with a re-entering WaitFor* call.
    /// Empty means a message-pump wait (`GetMessageW`). Keeping the
    /// dependency here lets the cooperative scheduler distinguish a
    /// genuinely wakeable waiter from one whose objects are still idle.
    pub parked_wait_handles: Vec<u32>,
    /// `true` for WaitForMultipleObjects(..., TRUE, ...).
    pub parked_wait_all: bool,
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
            stack_region: None,
            dll_thread_attach_delivered: false,
            dll_thread_detach_delivered: false,
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
            suspend_count: 0,
            sleep_until_ms: 0,
            finished: false,
            exit_code: None,
            handle_closed: false,
            parked_in_pump: false,
            parked_wait_handles: Vec::new(),
            parked_wait_all: false,
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
/// CE private allocations grow above the executable, in the active 32-MiB
/// slot. Leave room for the existing writable stack guard and honor PE's
/// reservation. Higher-based/non-CE images retain the previous flat layout.
fn private_process_layout(image: &LoadedImage) -> Option<(u32, u32, u32)> {
    if image.subsystem != pocket_pe::subsystem::WINDOWS_CE_GUI || image.image_base < 0x10000 {
        return None;
    }
    let mut end = image.image_base.checked_add(image.size_of_image)?;
    for section in &image.sections {
        let section_end = image
            .image_base
            .checked_add(section.virtual_address)?
            .checked_add(section.virtual_size.max(section.data.len() as u32))?;
        end = end.max(section_end);
    }
    let stack_base = end.checked_add(0x2000)?.checked_add(0xffff)? & !0xffff;
    let reserve = if image.stack_reserve == 0 {
        DEFAULT_STACK_SIZE
    } else {
        image.stack_reserve
    };
    let stack_size = reserve.max(0x10000).checked_add(0xffff)? & !0xffff;
    let heap_base = stack_base.checked_add(stack_size)?;
    (heap_base <= 0x01ff_0000).then_some((stack_base, stack_size, heap_base))
}

#[derive(Debug)]
pub struct Heap {
    base: u32,
    size: u32,
    regions: Vec<(u32, u32)>,
    /// Sorted by start VA. Each entry is `(start, size)` of free space.
    free: Vec<(u32, u32)>,
    /// Out-of-band tracker of `(user_ptr -> requested_size)` for every
    /// outstanding allocation. We keep it host-side so the guest can
    /// not accidentally corrupt the bookkeeping by writing past its
    /// own buffer (Pocket PC games do this all the time). It also lets
    /// `Heap::msize(p)` answer in O(1).
    live: HashMap<u32, u32>,
    ram: Option<memory_division::MemoryDivision>,
    image_memory: std::sync::Arc<image_memory::ImageMemory>,
    page_refs: HashMap<u32, u32>,
    resident: Vec<(u32, u32)>,
    virtual_blocks: HashMap<u32, VirtualBlock>,
}

#[derive(Debug)]
struct VirtualBlock {
    size: u32,
    committed: std::collections::HashSet<u32>,
}

impl Drop for Heap {
    fn drop(&mut self) {
        if let Some(ram) = &self.ram {
            ram.release_program(self.page_refs.len() as u32);
        }
    }
}

const HEAP_HEADER_BYTES: u32 = 8;
const HEAP_ALIGN: u32 = 8;

impl Heap {
    pub fn new(base: u32, size: u32) -> Self {
        Self {
            base,
            size,
            regions: vec![(base, size)],
            free: vec![(base, size)],
            live: HashMap::new(),
            ram: None,
            image_memory: std::sync::Arc::new(image_memory::ImageMemory::default()),
            page_refs: HashMap::new(),
            resident: Vec::new(),
            virtual_blocks: HashMap::new(),
        }
    }

    /// Add an independently mapped allocation arena, after existing regions.
    pub fn add_region(&mut self, base: u32, size: u32) {
        assert!(size > 0 && base.checked_add(size).is_some());
        assert!(self
            .regions
            .iter()
            .all(|&(start, len)| base >= start + len || base + size <= start));
        self.regions.push((base, size));
        let at = self.free.partition_point(|&(start, _)| start < base);
        self.free.insert(at, (base, size));
    }

    /// Reserve page-aligned stack storage from the top of the private slot.
    /// It is excluded from normal allocations and remains resident for the
    /// lifetime of the thread, then returned to the allocation arena.
    pub fn reserve_stack(&mut self, size: u32) -> Option<u32> {
        for index in (0..self.free.len()).rev() {
            let (start, len) = self.free[index];
            let end = start.checked_add(len)? & !0xfff;
            if end > 0x0200_0000 {
                continue;
            }
            let Some(base) = end.checked_sub(size) else {
                continue;
            };
            if base < start {
                continue;
            }
            if !self.retain_pages(base, size) {
                return None;
            }
            self.resident.push((base, size));
            self.free[index].1 = base - start;
            if self.free[index].1 == 0 {
                self.free.remove(index);
            }
            return Some(base);
        }
        None
    }

    /// Return a private stack reservation to the normal allocation arena.
    pub fn release_stack(&mut self, base: u32, size: u32) {
        if !self.resident.contains(&(base, size)) {
            return;
        }
        self.release_resident(base, size);
        self.free.push((base, size));
        self.free.sort_unstable_by_key(|r| r.0);
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (start, len) in self.free.drain(..) {
            if let Some(last) = merged.last_mut() {
                if last.0 + last.1 == start {
                    last.1 += len;
                    continue;
                }
            }
            merged.push((start, len));
        }
        self.free = merged;
    }
    pub fn base(&self) -> u32 {
        self.base
    }
    pub fn contains_range(&self, address: u32, size: u32) -> bool {
        self.regions.iter().any(|&(base, len)| {
            address >= base
                && address
                    .checked_add(size)
                    .is_some_and(|end| end <= base + len)
        })
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    fn align_up(n: u32) -> u32 {
        n.saturating_add(HEAP_ALIGN - 1) & !(HEAP_ALIGN - 1)
    }

    /// Return the user pointer (after the 8-byte header), or `None`
    /// if the heap has no large-enough free block.
    pub fn alloc(&mut self, requested: u32) -> Option<u32> {
        let need = requested.max(1).checked_add(HEAP_ALIGN - 1)? & !(HEAP_ALIGN - 1);
        let need = need.checked_add(HEAP_HEADER_BYTES)?;
        for i in 0..self.free.len() {
            let (start, sz) = self.free[i];
            if sz >= need {
                if !self.retain_pages(start, need) {
                    continue;
                }
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
        if !self.regions.iter().any(|&(base, size)| {
            block_start >= base
                && block_start
                    .checked_add(block_size)
                    .is_some_and(|end| end <= base + size)
        }) {
            log::warn!("heap.free: chunk overflows heap; ignoring");
            return;
        }
        self.release_pages(block_start, block_size);
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

    /// Bind all allocations (including allocations made during loading) to
    /// one device. No virtual arena is charged merely because it is mapped.
    pub fn attach_ram(&mut self, ram: Option<memory_division::MemoryDivision>) -> bool {
        if self
            .ram
            .as_ref()
            .zip(ram.as_ref())
            .is_some_and(|(old, next)| old.same_device(next))
        {
            return true;
        }
        if let Some(next) = &ram {
            if !next.acquire_program(self.page_refs.len() as u32) {
                return false;
            }
        }
        if !self.image_memory.attach_ram(ram.clone()) {
            if let Some(next) = &ram {
                next.release_program(self.page_refs.len() as u32);
            }
            return false;
        }
        if let Some(old) = &self.ram {
            old.release_program(self.page_refs.len() as u32);
        }
        self.ram = ram;
        true
    }
    fn page_range(start: u32, size: u32) -> Option<std::ops::RangeInclusive<u32>> {
        if size == 0 {
            return None;
        }
        Some(start / 4096..=start.checked_add(size - 1)? / 4096)
    }
    fn retain_pages(&mut self, start: u32, size: u32) -> bool {
        let Some(range) = Self::page_range(start, size) else {
            return size == 0;
        };
        let added = range
            .clone()
            .filter(|page| !self.page_refs.contains_key(page))
            .count() as u32;
        if self
            .ram
            .as_ref()
            .is_some_and(|ram| !ram.acquire_program(added))
        {
            return false;
        }
        for page in range {
            *self.page_refs.entry(page).or_default() += 1;
        }
        true
    }
    fn release_pages(&mut self, start: u32, size: u32) {
        let Some(range) = Self::page_range(start, size) else {
            return;
        };
        let mut released = 0;
        for page in range {
            if let Some(count) = self.page_refs.get_mut(&page) {
                *count -= 1;
                if *count == 0 {
                    self.page_refs.remove(&page);
                    released += 1;
                }
            }
        }
        if let Some(ram) = &self.ram {
            ram.release_program(released);
        }
    }
    /// Account resident mappings until their owner releases them.
    pub fn retain_resident(&mut self, start: u32, size: u32) -> bool {
        if !self.retain_pages(start, size) {
            return false;
        }
        self.resident.push((start, size));
        true
    }
    pub fn retain_resident_batch(&mut self, regions: &[(u32, u32)]) -> bool {
        let mut unique = std::collections::HashSet::new();
        for &(start, size) in regions {
            if size == 0 {
                continue;
            }
            let Some(range) = Self::page_range(start, size) else {
                return false;
            };
            unique.extend(range);
        }
        let added = unique
            .iter()
            .filter(|page| !self.page_refs.contains_key(*page))
            .count() as u32;
        if self
            .ram
            .as_ref()
            .is_some_and(|ram| !ram.acquire_program(added))
        {
            return false;
        }
        for &(start, size) in regions {
            if let Some(range) = Self::page_range(start, size) {
                for page in range {
                    *self.page_refs.entry(page).or_default() += 1;
                }
            }
            self.resident.push((start, size));
        }
        true
    }
    pub fn release_resident(&mut self, start: u32, size: u32) {
        if let Some(index) = self.resident.iter().position(|&r| r == (start, size)) {
            self.resident.remove(index);
            self.release_pages(start, size);
        }
    }
    pub fn virtual_bytes(&self) -> u32 {
        self.regions.iter().map(|&(_, size)| size).sum()
    }
    /// CE 4.x reports its 32 MiB private process slot here; the extra
    /// shared allocation arena does not enlarge that private address space.
    pub fn private_virtual_free(&self) -> u32 {
        let mut pages = std::collections::HashSet::new();
        for &(start, len) in &self.free {
            let first = ((start as u64 + 4095) / 4096) as u32;
            let end = start.saturating_add(len).min(0x02000000) / 4096;
            if start >= 0x02000000 {
                continue;
            }
            for page in first..end {
                if !self.page_refs.contains_key(&page) {
                    pages.insert(page);
                }
            }
        }
        pages.len() as u32 * 4096
    }
    pub fn program_pages(&self) -> u32 {
        self.page_refs.len() as u32 + self.image_memory.pages()
    }
    pub fn image_page_budget(
        &self,
    ) -> std::sync::Arc<dyn pocket_cpu::image_pages::ImagePageBudget> {
        self.image_memory.clone()
    }

    /// Page-aligned reservations share the same virtual arenas as the heap,
    /// but consume physical pages only when committed.
    pub fn virtual_alloc(
        &mut self,
        address: u32,
        size: u32,
        reserve: bool,
        commit: bool,
    ) -> Option<u32> {
        if size == 0 || (!reserve && !commit) {
            return None;
        }
        if address != 0 && !reserve {
            let (&base, block) = self.virtual_blocks.iter().find(|&(base, block)| {
                address >= *base
                    && address
                        .checked_add(size)
                        .is_some_and(|end| end <= *base + block.size)
            })?;
            let start = address & !0xfff;
            let end = address.checked_add(size)?.checked_add(4095)? & !0xfff;
            let needed: Vec<u32> = (start / 4096..end / 4096)
                .filter(|p| !block.committed.contains(p))
                .collect();
            // One reservation cannot overlap another block, so all these
            // physical pages are new. Validate before changing any state.
            if self
                .ram
                .as_ref()
                .is_some_and(|ram| !ram.acquire_program(needed.len() as u32))
            {
                return None;
            }
            let block = self.virtual_blocks.get_mut(&base)?;
            for page in needed {
                block.committed.insert(page);
                self.page_refs.insert(page, 1);
            }
            return Some(start);
        }
        let base_requested = if address == 0 { 0 } else { address & !0xffff };
        let offset = if address == 0 {
            0
        } else {
            address - base_requested
        };
        let commit_size = size.checked_add(address & 0xfff)?.checked_add(4095)? & !0xfff;
        let size = size.checked_add(offset)?.checked_add(0xffff)? & !0xffff;
        for i in 0..self.free.len() {
            let (start, len) = self.free[i];
            let base = if address == 0 {
                start.checked_add(0xffff)? & !0xffff
            } else {
                base_requested
            };
            let end = base.checked_add(size)?;
            if base < start || end > start.checked_add(len)? {
                continue;
            }
            let commit_start = if address == 0 { base } else { address & !0xfff };
            if commit && !self.retain_pages(commit_start, commit_size) {
                return None;
            }
            self.free.remove(i);
            if base > start {
                self.free.push((start, base - start));
            }
            if end < start + len {
                self.free.push((end, start + len - end));
            }
            self.free.sort_unstable_by_key(|r| r.0);
            self.virtual_blocks.insert(
                base,
                VirtualBlock {
                    size,
                    committed: if commit {
                        (commit_start / 4096..(commit_start + commit_size) / 4096).collect()
                    } else {
                        Default::default()
                    },
                },
            );
            return Some(base);
        }
        None
    }
    pub fn virtual_block(
        &self,
        address: u32,
    ) -> Option<(u32, u32, std::collections::HashSet<u32>)> {
        self.virtual_blocks
            .iter()
            .find(|&(base, block)| address >= *base && address < *base + block.size)
            .map(|(&base, block)| (base, block.size, block.committed.clone()))
    }
    pub fn virtual_free(&mut self, address: u32, size: u32, release: bool) -> bool {
        if release {
            if size != 0 {
                return false;
            }
            let Some(block) = self.virtual_blocks.remove(&address) else {
                return false;
            };
            for page in block.committed {
                self.release_pages(page * 4096, 4096);
            }
            self.free.push((address, block.size));
            self.free.sort_unstable_by_key(|r| r.0);
            let mut merged: Vec<(u32, u32)> = Vec::new();
            for (start, len) in self.free.drain(..) {
                if let Some(last) = merged.last_mut() {
                    if last.0 + last.1 == start {
                        last.1 += len;
                        continue;
                    }
                }
                merged.push((start, len));
            }
            self.free = merged;
            return true;
        }
        let Some((&base, block)) = self
            .virtual_blocks
            .iter()
            .find(|&(base, block)| address >= *base && address < *base + block.size)
        else {
            return false;
        };
        if size == 0 && address != base {
            return false;
        }
        let start = address & !0xfff;
        let Some(end) = (if size == 0 {
            Some(base + block.size)
        } else {
            address
                .checked_add(size)
                .and_then(|end| end.checked_add(4095))
                .map(|end| end & !0xfff)
        }) else {
            return false;
        };
        if end > base + block.size {
            return false;
        }
        let removed: Vec<u32> = (start / 4096..end / 4096)
            .filter(|p| block.committed.contains(p))
            .collect();
        for page in removed {
            self.virtual_blocks
                .get_mut(&base)
                .unwrap()
                .committed
                .remove(&page);
            self.release_pages(page * 4096, 4096);
        }
        true
    }

    pub fn free_bytes(&self) -> u32 {
        self.free.iter().map(|(_, s)| *s).sum()
    }
}

/// Fake `HMODULE` for the resident `ole32.dll` compatibility module.
pub const SDLAUNCH_MODULE_HANDLE: u32 = 0x1000_0009;
pub const OLE32_MODULE_HANDLE: u32 = 0x1000_0007;
pub const WININET_MODULE_HANDLE: u32 = 0x1000_000b;
pub const WS2_MODULE_HANDLE: u32 = 0x1000_000a;
/// Fake `HMODULE` for `libGLES_CM.dll`, the Common profile.
pub const GLES_CM_MODULE_HANDLE: u32 = 0x1000_0004;
/// Fake `HMODULE` for `libGLES_CL.dll`, the Common-Lite profile.
pub const GLES_CL_MODULE_HANDLE: u32 = 0x1000_0005;
/// Fake `HMODULE` for the Hekkus Sound System compatibility layer.
pub const HSS_MODULE_HANDLE: u32 = 0x1000_0006;
/// Fake HMODULE for the resident `aygshell.dll` compatibility module.
pub const AYGSHELL_MODULE_HANDLE: u32 = 0x1000_0008;

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
    let mut aygshell = HashMap::new();
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
        } else if thunk.dll.eq_ignore_ascii_case("aygshell.dll") {
            aygshell.insert(name.clone(), thunk.thunk_va);
            if let ImportBinding::Ordinal(ord) = &thunk.binding {
                aygshell.insert(format!("#{ord}"), thunk.thunk_va);
            }
        } else if thunk.dll.eq_ignore_ascii_case("hss.dll") {
            let table = exports
                .entry(HSS_MODULE_HANDLE)
                .or_insert_with(HashMap::new);
            table.insert(name.clone(), thunk.thunk_va);
            if let ImportBinding::Ordinal(ord) = &thunk.binding {
                table.insert(format!("#{ord}"), thunk.thunk_va);
            }
        } else if thunk.dll.eq_ignore_ascii_case("sdlaunch.dll") {
            exports
                .entry(SDLAUNCH_MODULE_HANDLE)
                .or_insert_with(HashMap::new)
                .insert(name, thunk.thunk_va);
        } else if thunk.dll.eq_ignore_ascii_case("wininet.dll") {
            exports
                .entry(WININET_MODULE_HANDLE)
                .or_insert_with(HashMap::new)
                .insert(name, thunk.thunk_va);
        } else if thunk.dll.eq_ignore_ascii_case("ws2.dll") {
            let table = exports
                .entry(WS2_MODULE_HANDLE)
                .or_insert_with(HashMap::new);
            table.insert(name, thunk.thunk_va);
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
    if !aygshell.is_empty() {
        exports.insert(AYGSHELL_MODULE_HANDLE, aygshell);
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
        mut image: LoadedImage,
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
        // Reserve image sections; IAT writes and the first guest access page them in.
        let image_memory = std::sync::Arc::new(image_memory::ImageMemory::default());
        let mut eager_images = Vec::new();
        for s in &image.sections {
            let mut prot = Prot::READ;
            if s.is_writable() {
                prot |= Prot::WRITE;
            }
            if s.is_executable() {
                prot |= Prot::EXEC;
            }
            let aligned = pocket_cpu::round_up_to_page(s.virtual_size.max(s.data.len() as u32));
            if aligned == 0 {
                continue;
            }
            if !cpu.map_image_region(
                image.image_base + s.virtual_address,
                aligned,
                prot,
                s.data.clone(),
                image_memory.clone(),
            )? {
                eager_images.push((image.image_base + s.virtual_address, aligned));
            }
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
            let fallback_offset = if native.is_some() {
                native_name.and_then(|name| native_thunks::hybrid_fallback_offset(&imp.dll, name))
            } else {
                None
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
            let inline_arithmetic = match thunk.friendly_name.as_deref() {
                Some(name) if native.is_none() && constant.is_none() => {
                    native_thunks::install_inline_arithmetic64(cpu, thunk_va, &imp.dll, name)?
                }
                _ => false,
            };
            if inline_arithmetic {
                // Register-only helper handled inside the CPU run loop.
            } else if let Some(words) = native.or(constant) {
                let bytes = native_thunks::thunk_bytes(&words);
                cpu.write_mem(thunk_va, &bytes)?;
                if let Some(offset) = fallback_offset {
                    let fallback = thunk_va + offset;
                    cpu.add_code_hook(fallback)?;
                    thunk_by_va.insert(fallback, i);
                }
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
            "aygshell.dll",
            "coredll.dll",
            "commctrl.dll",
            "gx.dll",
            "ddraw.dll",
            "libgles_cm.dll",
            "libgles_cl.dll",
            "hss.dll",
            "ole32.dll",
            "sdlaunch.dll",
            "ws2.dll",
            "wininet.dll",
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
                // GetProcAddress must expose the same small-copy fast path
                // as a static import; unrelated dynamic exports keep their
                // existing dispatch behaviour.
                let hybrid = if cpu.arch() == Arch::Arm {
                    native_thunks::hybrid_fallback_offset(&dll, &name).and_then(|offset| {
                        native_thunks::native_thunk_for(&dll, &name).map(|words| (offset, words))
                    })
                } else {
                    None
                };
                if native_thunks::install_inline_arithmetic64(cpu, thunk_va, &dll, &name)? {
                    // Match static imports without a kernel-dispatch hook.
                } else if let Some((offset, words)) = hybrid {
                    cpu.write_mem(thunk_va, &native_thunks::thunk_bytes(&words))?;
                    let fallback = thunk_va + offset;
                    cpu.add_code_hook(fallback)?;
                    thunk_by_va.insert(fallback, thunks.len());
                } else {
                    let mut buf = [0u8; THUNK_STRIDE as usize];
                    let stub = return_stub_bytes(cpu.arch());
                    for (chunk, bytes) in buf.chunks_exact_mut(8).zip(stub.chunks_exact(8)) {
                        chunk.copy_from_slice(bytes);
                    }
                    cpu.write_mem(thunk_va, &buf)?;
                    hooked_slots.push(thunk_va);
                }
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
        let private_layout = private_process_layout(&image);
        let (stack_base, stack_size, stack_top) = private_layout
            .map(|layout| (layout.0, layout.1, layout.0 + layout.1))
            .unwrap_or((
                DEFAULT_STACK_TOP - DEFAULT_STACK_SIZE,
                DEFAULT_STACK_SIZE,
                DEFAULT_STACK_TOP,
            ));
        let dynamic_exports = build_dynamic_exports(&thunks);
        // Keep one writable guard page below the nominal stack base.
        // ARM prologues may pre-decrement SP before the first store, and
        // Total Commander reaches exactly that boundary during startup.
        cpu.map_region(
            stack_base - 0x2000,
            stack_size + 0x2000,
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
        let mut heap = if let Some((_, _, heap_base)) = private_layout {
            let heap_size = 0x0200_0000 - heap_base;
            cpu.map_region(heap_base, heap_size, Prot::READ | Prot::WRITE)?;
            let mut heap = Heap::new(heap_base, heap_size);
            heap.add_region(HEAP_BASE, HEAP_SIZE);
            heap
        } else {
            Heap::new(HEAP_BASE, HEAP_SIZE)
        };

        heap.image_memory = image_memory;
        for (start, bytes) in eager_images {
            heap.retain_resident(start, bytes);
        }
        heap.retain_resident(stack_base - 0x2000, stack_size + 0x2000);
        heap.retain_resident(THUNK_REGION_BASE, thunk_size);

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
        // The CE CRT can invoke process termination directly instead of an IAT
        // import. This implicit process API call is decoded before callbacks.
        // Keep compatibility soft returns for the other existing trap hooks.
        for &trap_va in &[CE_TERMINATE_PROCESS_TRAP, 0xF000_F7FC, 0xF000_FFFC] {
            cpu.add_code_hook(trap_va)?;
        }
        // Install the dedicated process-exit trampoline. The page is
        // already filled with `bx lr`; the run loop checks for hits
        // on this exact address and treats them as a graceful
        // shutdown (== `ExitProcess(0)`). Setting the initial `LR`
        // to this address (below) is what teaches the entry point
        // to come back here when its top-level frame returns.
        cpu.add_code_hook(PROCESS_EXIT_TRAMPOLINE_VA)?;
        cpu.add_code_hook(dll_lifecycle::DLL_NOTIFICATION_RETURN_VA)?;

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

        let resources = std::mem::take(&mut image.resources);
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
        for section in &mut image.sections {
            section.data = Vec::new();
        }
        Ok(Process {
            image,
            thunks,
            thunk_by_va,
            stack_top,
            stack_size,
            state: KernelState {
                heap,
                vfs: vfs::Vfs::new(),
                file_views: HashMap::new(),
                internet: internet::State::default(),
                module_path: DEFAULT_MODULE_PATH.to_string(),
                process_launch_enabled: false,
                pending_process_launch: None,
                command_line_cache: None,
                command_line: None,
                process_launch_results: HashMap::new(),
                child_processes: HashMap::new(),
                next_process_id: 1,
                process_exit_code: None,
                memory_division: None,
                main_thread: MainThreadState {
                    stack_region: Some((stack_base - 0x2000, stack_size + 0x2000)),
                    ..MainThreadState::default()
                },
                object_handles: handles::HandleTable::default(),
                random_seed: 0x1234_abcd,
                framebuffer: Framebuffer::default(),
                gdi: GdiState::new(),
                resources,
                image_base: img_base,
                image_size: img_size,
                image_entry: img_entry,
                dynamic_exports,
                runtime_thunks: HashMap::new(),
                module_attach_frames: Vec::new(),
                dll_notification_frame: None,
                next_module_handle: 0x1000_0001,
                modules: Vec::new(),
                next_module_base: MODULE_REGION_BASE,
                free_module_bases: Vec::new(),
                module_search_dirs,
                fb_mapped: false,
                guest_fb_pitch: 0,
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
                window_background_erase_pending: false,
                pending_create: None,
                find_handles: HashMap::new(),
                next_find_handle: 0,
                pending_startup: std::collections::VecDeque::new(),
                registry: crate::registry::Registry::with_device_defaults(),
                cedb: crate::cedb::DatabaseStore::default(),
                window_procs: HashMap::new(),
                window_userdata: HashMap::new(),
                window_classes: HashMap::new(),
                window_user_data: 0,
                timers: HashMap::new(),
                synthetic_paint_next_ms: 0,
                synthetic_create_sent: false,
                synthetic_size_sent: false,
                create_frame: None,
                create_stage: CreateStage::Idle,
                dialog_frame: None,
                accelerator_frames: Vec::new(),
                shell_status: None,
                message_frames: Default::default(),
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
                mutexes: Default::default(),
                current_thread: 0,
                thread_last_errors: HashMap::new(),
                winsock_last_errors: HashMap::new(),
                crt_new_handler: 0,
                cpp_new_handler: 0,
                worker_schedule_cursor: 0,
                worker_round_seen: Vec::new(),
                worker_preempt_after_ms: 0,
                wait_deadlines: HashMap::new(),
                guest_fpscr: std::collections::HashMap::new(),
                timer_period_requests: std::collections::BTreeMap::new(),
                critical_sections: HashMap::new(),
                pressed_keys: [false; 256],
                cursor_pos: (0, 0),
                held_keys: Vec::new(),
                key_repeat_next_ms: None,
                key_repeat_cursor: 0,
                should_stop: false,
                tls_slots_used: 0,
                tls_owner: 0,
                thread_tls: HashMap::new(),
                vector_iter_stack: Vec::new(),
                qsort_frames: HashMap::new(),
                strtok_pos: 0,
                security_cookie: 0,
                audio: AudioEngine::new(),
                wave_out_format: GuestFormat::default(),
                wave_out: Default::default(),
                hss: Default::default(),
                gz_files: Default::default(),
                zlib_streams: Default::default(),
                sdl: Default::default(),
                posted_messages: Default::default(),
                msg_queues: HashMap::new(),
                next_msg_queue_handle: 0xDEAD_E500,
                power_notifications: Default::default(),
                battery_alerts: Default::default(),
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
    if !state.fb_mapped
        || !state.framebuffer.directdraw.primary_locks.is_empty()
        || state.framebuffer.frame_counter != state.gx_last_pushed_counter
    {
        return;
    }
    let pitch = state.guest_fb_pitch.max(state.framebuffer.stride_bytes()) as usize;
    let row_bytes = state.framebuffer.stride_bytes() as usize;
    let len = state.framebuffer.pixels.len();
    if state.gx_readback_scratch.len() != len {
        state.gx_readback_scratch.resize(len, 0);
    }
    if pitch == row_bytes {
        if let Err(error) =
            cpu.read_mem_into(SYNTHETIC_FRAMEBUFFER_BASE, &mut state.gx_readback_scratch)
        {
            log::warn!("unable to read the GAPI framebuffer: {error}");
            return;
        }
    } else {
        for row in 0..state.framebuffer.height as usize {
            let src = SYNTHETIC_FRAMEBUFFER_BASE + (row * pitch) as u32;
            let dst = row * row_bytes;
            if let Err(error) =
                cpu.read_mem_into(src, &mut state.gx_readback_scratch[dst..dst + row_bytes])
            {
                log::warn!("unable to read the GAPI framebuffer row: {error}");
                return;
            }
        }
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
    let thumb_mode = image_uses_thumb_entry(cpu, &process.image)?;
    let entry = process.image.entry_va() & !1;
    let mut pc = if thumb_mode { entry | 1 } else { entry };
    log::info!(
        "entering emulated main: entry=0x{:08x}, stack_top=0x{:08x}",
        pc,
        process.stack_top
    );
    let mut slice = 0u64;
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
    let mut last_frontend_poll: Option<Instant> = None;
    let process_gate = process.state.object_handles.execution_gate();
    loop {
        if max_slices != 0 && slice >= max_slices {
            break;
        }
        slice = slice.saturating_add(1);
        {
            let _guard = process_gate.lock().unwrap_or_else(|e| e.into_inner());
            if process.state.apply_process_controls(cpu, &mut pc)? {
                return Ok(());
            }
        }
        // Switch TLS before any guest code, including DllMain notifications.
        process.state.sync_thread_tls(cpu)?;
        // PC=0 (or any address in the unmapped null page) means
        // the guest jumped through a null function pointer or popped
        // a poisoned LR off the stack. Without an explicit halt,
        // unicorn's `emu_start` typically returns `Ok(0 instructions)`
        // and we'd spin forever. Surface it as a real crash with the
        // CPU dump.
        if pc == PROCESS_EXIT_TRAMPOLINE_VA {
            let _guard = process_gate.lock().unwrap_or_else(|e| e.into_inner());
            let code = cpu.read_reg(ArmReg::R0).unwrap_or(0);
            if let Some(entry) = process.state.begin_dll_notifications(
                cpu,
                0,
                dll_lifecycle::DllContinuation::ExitProcess(code),
            )? {
                pc = entry;
            } else {
                process.state.finish_dll_process_exit(cpu, code)?;
                return Ok(());
            }
        }
        if let Some(index) = process.state.current_thread.checked_sub(1) {
            if !process.state.threads[index].dll_thread_attach_delivered {
                process.state.threads[index].dll_thread_attach_delivered = true;
                if let Some(entry) = process.state.begin_dll_notifications(
                    cpu,
                    2,
                    dll_lifecycle::DllContinuation::Resume(pc),
                )? {
                    pc = entry;
                }
            }
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
        // millisecond count for any guest call inside this slice. Timed-out
        // API-free slices reach this refresh as well.
        // `tick_written` lets it skip the guest write when the
        // millisecond has not turned over yet, which is the common
        // case at ~280 slices per millisecond.
        native_thunks::refresh_tick_page(cpu, &mut tick_written);
        // Native GetTickCount reads the tick page without trapping. Hit & Myth
        // (May 2005) spins on it at the Texas splash before audio starts: without
        // preemption this loop can neither refresh time nor service Exit/input.
        // Bound every slice by host time, retaining the shorter audio quantum
        // and Unicorn's translation-block chaining (no instruction counter).
        let audio_worker_pending = !process.state.wave_out.live_loops.is_empty()
            && process
                .state
                .threads
                .iter()
                .any(|thread| thread.started && !thread.finished && thread.suspend_count == 0);
        cpu.set_slice_timeout_us(if audio_worker_pending { 5_000 } else { 20_000 })?;
        let stop = if pc == THREAD_SCHEDULER_IDLE_VA {
            Ok(StopReason::Hook(pc))
        } else {
            cpu.run_until_hook(pc, instruction_budget_per_slice)
        };
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
                // Report ownership at the fault, without reading the pointed-to
                // bytes or changing permissions. Colors E3's Android strcmp
                // fault needs reserve/commit metadata to distinguish a stale
                // arena pointer from a CPU protection-tracking error.
                for (label, reg) in [("R0", ArmReg::R0), ("R1", ArmReg::R1)] {
                    let address = cpu.read_reg(reg).unwrap_or(0);
                    if let Some((base, size, committed)) = process.state.heap.virtual_block(address)
                    {
                        log::error!(
                            "{label} memory: addr=0x{address:08x}, virtual reservation=0x{base:08x}+0x{size:x}, page_committed={}, committed_pages={}",
                            committed.contains(&(address / 4096)), committed.len()
                        );
                    } else if let Some((&base, &size)) =
                        process.state.heap.live.iter().find(|&(base, size)| {
                            address >= *base
                                && address
                                    .checked_sub(*base)
                                    .is_some_and(|offset| offset < *size)
                        })
                    {
                        log::error!("{label} memory: addr=0x{address:08x}, live heap allocation=0x{base:08x}+0x{size:x}");
                    } else {
                        log::error!("{label} memory: addr=0x{address:08x}, no matching tracked heap allocation or virtual reservation");
                    }
                }
                return Err(e.into());
            }
        };
        {
            let _guard = process_gate.lock().unwrap_or_else(|e| e.into_inner());
            let mut boundary_pc = cpu.read_reg(ArmReg::Pc)?;
            let old_thread = process.state.current_thread;
            let old_boundary_pc = boundary_pc;
            if process
                .state
                .apply_process_controls(cpu, &mut boundary_pc)?
            {
                return Ok(());
            }
            if boundary_pc != old_boundary_pc || old_thread != process.state.current_thread {
                pc = boundary_pc;
                // A remote request parked/killed the owner before dispatching its call.
                continue;
            }
            match stop {
                StopReason::InstructionLimit => {
                    pc = cpu.read_reg(ArmReg::Pc)?;
                    if let Some(worker_pc) =
                        dispatcher.schedule_cpu_slice(cpu, &mut process.state)?
                    {
                        pc = worker_pc;
                    }
                    log::trace!("instruction slice exhausted; resuming at 0x{pc:08x}");
                    // Budget exhaustion is a normal slice boundary too. Do not
                    // bypass the throttled host input/presentation/stop hook.
                    // Native thunks and CPU-only loops may never dispatch an API.
                }
                StopReason::Hook(addr) => {
                    if addr == CE_TERMINATE_PROCESS_TRAP {
                        let handle = cpu.read_reg(ArmReg::R0)?;
                        if handle == CE_CURRENT_PROCESS_HANDLE
                            || handle == FAKE_CURRENT_PROCESS_HANDLE
                            || process.state.object_handles.get(handle)
                                == Some(handles::HandleObject::CurrentProcess)
                        {
                            let code = cpu.read_reg(ArmReg::R1)?;
                            process.state.record_process_exit(code);
                            log::info!("WinCE implicit TerminateProcess -> exit code 0x{code:08x}");
                            return Ok(());
                        }
                        // This API is not a WndProc-return trampoline. Even failure
                        // must leave an in-flight message continuation untouched.
                        process
                            .state
                            .thread_last_errors
                            .insert(process.state.current_thread, 6);
                        cpu.write_reg(ArmReg::R0, 0)?;
                        pc = cpu.read_reg(ArmReg::Lr)?;
                        continue;
                    }
                    // The synthetic process-exit trampoline is reached
                    // when the guest entry point's top-level frame
                    // returns and pops the seeded `LR` value into `PC`.
                    // Treat it as a clean shutdown — equivalent to the
                    // game calling `ExitProcess(0)` itself. Without this,
                    // every Pocket PC game looks like it crashes
                    // (pc=0x00000000) at the very end of execution.
                    if addr == PROCESS_EXIT_TRAMPOLINE_VA {
                        pc = addr;
                        continue;
                    }
                    if addr == dll_lifecycle::DLL_NOTIFICATION_RETURN_VA {
                        match process.state.next_dll_notification(cpu)? {
                            Some(next) => {
                                pc = next;
                                continue;
                            }
                            None => return Ok(()),
                        }
                    }
                    if addr == KERNEL_TRAP_BASE {
                        let owner = process.state.current_thread;
                        if let Some(frame) = process.state.message_frames.remove(&owner) {
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
                        let exit_code = cpu.read_reg(ArmReg::R0)?;
                        if process.state.main_thread.exit_code.is_some()
                            && process
                                .state
                                .threads
                                .iter()
                                .enumerate()
                                .all(|(index, thread)| index == thread_index || thread.finished)
                        {
                            if let Some(entry) = process.state.begin_dll_notifications(
                                cpu,
                                0,
                                dll_lifecycle::DllContinuation::ExitProcess(exit_code),
                            )? {
                                pc = entry;
                                continue;
                            }
                            process.state.finish_dll_process_exit(cpu, exit_code)?;
                            return Ok(());
                        }
                        if !process.state.threads[thread_index].dll_thread_detach_delivered {
                            process.state.threads[thread_index].dll_thread_detach_delivered = true;
                            if let Some(entry) = process.state.begin_dll_notifications(
                                cpu,
                                3,
                                dll_lifecycle::DllContinuation::Resume(addr),
                            )? {
                                pc = entry;
                                continue;
                            }
                        }
                        process.state.main_thread.last_exit_code = Some(exit_code);
                        process.state.threads[thread_index].exit_code = Some(exit_code);
                        process.state.threads[thread_index].finished = true;
                        process.state.reclaim_finished_stacks(cpu)?;
                        process.state.message_frames.remove(&(thread_index + 1));
                        if process.state.main_thread.exit_code.is_some()
                            || process.state.main_thread.suspend_count != 0
                            || process.state.main_thread.saved_regs.is_some()
                        {
                            let fpscr = cpu.read_fpscr()?;
                            process.state.guest_fpscr.insert(thread_index + 1, fpscr);
                            process.state.current_thread = 0;
                            cpu.write_fpscr(
                                process.state.guest_fpscr.get(&0).copied().unwrap_or(0),
                            )?;
                            pc = THREAD_SCHEDULER_IDLE_VA;
                            continue;
                        }
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
                            let fpscr = cpu.read_fpscr()?;
                            process
                                .state
                                .guest_fpscr
                                .insert(process.state.current_thread, fpscr);
                            cpu.write_fpscr(
                                process.state.guest_fpscr.get(&0).copied().unwrap_or(0),
                            )?;
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
                            let fpscr = cpu.read_fpscr()?;
                            process
                                .state
                                .guest_fpscr
                                .insert(process.state.current_thread, fpscr);
                            cpu.write_fpscr(
                                process.state.guest_fpscr.get(&0).copied().unwrap_or(0),
                            )?;
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

                    let runtime_thunk = process.state.runtime_thunks.get(&addr).cloned();
                    let outcome = if addr == THREAD_SCHEDULER_IDLE_VA {
                        dispatcher.schedule_idle(cpu, &mut process.state)?
                    } else {
                        match process.find_thunk_and_state(addr) {
                            Some((thunk, state)) => {
                                // Split borrow: `thunk` borrows
                                // `process.thunks` immutably and `state`
                                // borrows `process.state` mutably. No clone,
                                // and no per-call heap allocation — that
                                // matters because this branch fires on every
                                // single emulated WinCE API call.
                                let dispatched = dispatcher.dispatch(cpu, thunk, state);
                                let outcome = dispatched?;
                                if matches!(outcome, DispatchOutcome::Halt)
                                    && log::log_enabled!(log::Level::Info)
                                {
                                    log::info!("dispatcher requested halt at {}", thunk.label());
                                }
                                outcome
                            }
                            None if runtime_thunk.is_some() => dispatcher.dispatch(
                                cpu,
                                runtime_thunk.as_ref().unwrap(),
                                &mut process.state,
                            )?,
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
                                if (KERNEL_TRAP_BASE
                                    ..KERNEL_TRAP_BASE.saturating_add(KERNEL_TRAP_SIZE))
                                    .contains(&addr)
                                {
                                    let owner = process.state.current_thread;
                                    if let Some(frame) = process.state.message_frames.remove(&owner)
                                    {
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
        } // Drop the shared API gate before frontend hooks or child startup.
          // An idle/suspended process must not sleep while owning the domain's
          // API gate: its parent needs that gate to issue ResumeThread/Terminate.
        if pc == THREAD_SCHEDULER_IDLE_VA {
            std::thread::sleep(std::time::Duration::from_millis(1));
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
            let due = process.state.pending_process_launch.is_some()
                || counter != last_presented_frame
                || last_present.is_none_or(|then| now.duration_since(then) >= poll_interval);
            if due {
                last_present = Some(now);
                sync_guest_framebuffer(cpu, &mut process.state);
                process.state.composite_controls();
            }
            // The frontend also drains input and stop requests. Its polling
            // cadence must not inherit the expensive framebuffer readback
            // backoff, especially when a DirectDraw menu has static pixels.
            let input_due = last_frontend_poll
                .is_none_or(|then| now.duration_since(then) >= FRONTEND_POLL_INTERVAL);
            if due || input_due {
                last_frontend_poll = Some(now);
                process.state.sync_backlight();
                process.state.sync_power_notifications();
                let action = hook.on_frame(&mut process.state);
                last_presented_frame = process.state.framebuffer.frame_counter;
                if action == FrameAction::Stop {
                    log::info!("frame hook requested stop");
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
    fn memory_division_heap_counts_shared_pages_and_releases_on_drop() {
        use memory_division::{MemoryDivision, ResizeError};
        let ram = MemoryDivision::new(32 * 4096, 16).unwrap();
        let mut parent = Heap::new(0x100000, 0x40000);
        assert!(parent.attach_ram(Some(ram.clone())));
        let a = parent.alloc(1).unwrap();
        let b = parent.alloc(1).unwrap();
        assert_eq!(ram.snapshot().program_used, 1);
        parent.free(a);
        assert_eq!(ram.snapshot().program_used, 1);
        parent.free(b);
        assert_eq!(ram.snapshot().program_used, 0);
        let a = parent.alloc(6 * 4096 - 8).unwrap();
        {
            let mut child = Heap::new(0x100000, 0x40000);
            assert!(child.attach_ram(Some(ram.clone())));
            child.alloc(6 * 4096 - 8).unwrap();
            assert_eq!(ram.snapshot().program_used, 12);
            assert_eq!(ram.resize(21), Err(ResizeError::InUse));
        }
        assert_eq!(ram.snapshot().program_used, 6);
        ram.resize(21).unwrap();
        assert!(parent.alloc(6 * 4096).is_none());
        parent.free(a);
        assert_eq!(ram.snapshot().program_used, 0);
        assert!(parent.alloc(6 * 4096).is_some());
        drop(parent);
        assert_eq!(ram.snapshot().program_used, 0);
    }

    #[test]
    fn memory_division_virtual_reserve_commit_decommit_release() {
        let ram = memory_division::MemoryDivision::new(64 * 4096, 32).unwrap();
        let mut heap = Heap::new(0x100000, 0x40000);
        assert!(heap.attach_ram(Some(ram.clone())));
        let initial = heap.free_bytes();
        let base = heap.virtual_alloc(0, 0x10000, true, false).unwrap();
        assert_eq!(ram.snapshot().program_used, 0);
        assert_eq!(heap.free_bytes(), initial - 0x10000);
        assert_eq!(heap.virtual_alloc(base + 1, 4096, false, true), Some(base));
        assert_eq!(ram.snapshot().program_used, 2);
        heap.virtual_alloc(base + 1, 4096, false, true).unwrap();
        assert_eq!(ram.snapshot().program_used, 2);
        assert!(heap
            .virtual_alloc(base + 0x10000, 4096, false, true)
            .is_none());
        assert!(!heap.virtual_free(base + 4096, 0, true));
        assert!(heap.virtual_free(base, 4096, false));
        assert_eq!(ram.snapshot().program_used, 1);
        assert!(heap.virtual_free(base, 0, true));
        assert_eq!(ram.snapshot().program_used, 0);
        assert_eq!(heap.free_bytes(), initial);
    }

    #[test]
    fn released_stack_restores_budget_and_coalesces_the_heap() {
        let ram = memory_division::MemoryDivision::new(32 * 4096, 8).unwrap();
        let mut heap = Heap::new(0x100000, 0x10000);
        heap.attach_ram(Some(ram.clone()));
        let initial = heap.free_bytes();
        for _ in 0..100 {
            let base = heap.reserve_stack(8 * 4096).unwrap();
            assert_eq!(ram.snapshot().program_used, 8);
            heap.release_stack(base, 8 * 4096);
            heap.release_stack(base, 8 * 4096); // idempotent cleanup
            assert_eq!(ram.snapshot().program_used, 0);
            assert_eq!(heap.free_bytes(), initial);
        }
        assert!(heap.alloc(0xf000).is_some());
    }

    #[test]
    fn mapped_executable_keeps_metadata_without_a_second_payload() {
        let mut image = image_based_at(0x10000, 0x1000);
        image.sections[0].data[..4].copy_from_slice(&[1, 2, 3, 4]);
        let mut cpu = StubCpu::new();
        let process = Process::map_into(image, &mut cpu, &|_, _| None, &NullDispatcher).unwrap();
        assert_eq!(cpu.read_mem(0x11000, 4).unwrap(), vec![1, 2, 3, 4]);
        assert!(process.image.sections[0].data.is_empty());
        assert_eq!(process.image.sections[0].virtual_size, 0x1000);
    }

    #[test]
    fn memory_division_stack_is_resident_and_cannot_bypass_budget() {
        let ram = memory_division::MemoryDivision::new(16 * 4096, 8).unwrap();
        let mut heap = Heap::new(0x100000, 0x10000);
        assert!(heap.attach_ram(Some(ram.clone())));
        assert!(heap.reserve_stack(9 * 4096).is_none());
        assert_eq!(ram.snapshot().program_used, 0);
        heap.reserve_stack(8 * 4096).unwrap();
        assert_eq!(ram.snapshot().program_used, 8);
        assert!(heap.alloc(1).is_none());
    }

    #[test]
    fn ce_memory_layout_uses_pe_reservation_and_private_slot_bounds() {
        let mut image = image_based_at(0x10000, 0x509000);
        image.subsystem = 9;
        image.stack_reserve = 0x10000;
        assert_eq!(
            private_process_layout(&image),
            Some((0x520000, 0x10000, 0x530000))
        );
        image.stack_reserve = 0x30000;
        assert_eq!(
            private_process_layout(&image),
            Some((0x520000, 0x30000, 0x550000))
        );
        image.image_base = 0x30000000;
        assert!(private_process_layout(&image).is_none());
        image.image_base = 0x10000;
        image.subsystem = 2;
        assert!(private_process_layout(&image).is_none());
    }

    #[test]
    fn ce_memory_layout_stack_and_overflow_arena_do_not_overlap_allocations() {
        let mut heap = Heap::new(0x100000, 0x10000);
        heap.add_region(HEAP_BASE, 0x10000);
        let stack = heap.reserve_stack(0x4000).unwrap();
        assert_eq!(stack, 0x10c000);
        let low = heap.alloc(0xbff8).unwrap();
        assert_eq!(low, 0x100008);
        let high = heap.alloc(0x8000).unwrap();
        assert!(high >= HEAP_BASE);
        assert_eq!(heap.msize(high), Some(0x8000));
        assert!(heap.contains_range(high, 0x8000));
        assert!(!heap.contains_range(0x1fff0000, 0x1000));
        heap.free(high);
        heap.free(low);
        assert_eq!(heap.free_bytes(), 0x1c000);
        assert_eq!(heap.reserve_stack(0x4000), Some(0x108000));
    }

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
        last_slice_timeout: Option<u64>,
    }

    impl StrictMapCpu {
        fn new() -> Self {
            Self {
                inner: StubCpu::new(),
                mapped: Vec::new(),
                last_slice_timeout: None,
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

        fn set_slice_timeout_us(&mut self, timeout_us: u64) -> Result<(), CpuError> {
            self.last_slice_timeout = Some(timeout_us);
            Ok(())
        }
    }

    fn image_based_at(image_base: u32, text_size: u32) -> LoadedImage {
        LoadedImage {
            source_path: "test".into(),
            machine: pocket_pe::machine::ARM,
            subsystem: pocket_pe::subsystem::WINDOWS_CE_GUI,
            image_base,
            stack_reserve: 0x10000,
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

    #[test]
    fn ce_implicit_process_termination_precedes_message_return() {
        let mut cpu = StubCpu::new();
        let mut process = Process::map_into(
            image_based_at(0x10000, 0x1000),
            &mut cpu,
            &|_, _| None,
            &NullDispatcher,
        )
        .unwrap();
        process.state.message_frames.insert(
            0,
            GuestCallFrame {
                args: [0x1234, 2, 0, 0],
                sp: 0x2222,
                lr: 0x3333,
            },
        );
        let worker = GuestThread::new(
            0x11000, 0, 0x50000, 0x1000, 0xf000fe00, 0x11000, 0xdead7c00, [0; 17],
        );
        process.state.threads.push(worker);
        cpu.write_reg(ArmReg::R0, CE_CURRENT_PROCESS_HANDLE)
            .unwrap();
        cpu.write_reg(ArmReg::R1, 17).unwrap();
        cpu.write_reg(ArmReg::Lr, 0x4444).unwrap();
        run_main_loop(&mut cpu, &mut process, &mut NullDispatcher, 100, 1).unwrap();
        assert_eq!(process.state.process_exit_code, Some(17));
        assert!(process.state.threads[0].finished);
        assert_eq!(process.state.threads[0].exit_code, Some(17));
        assert!(process.state.message_frames.is_empty());
        // Actual syscall arguments were not replaced by the WndProc frame.
        assert_eq!(cpu.read_reg(ArmReg::R0).unwrap(), CE_CURRENT_PROCESS_HANDLE);
        assert_eq!(cpu.read_reg(ArmReg::Lr).unwrap(), 0x4444);
    }

    #[test]
    fn ce_implicit_process_termination_failure_preserves_message_frame() {
        let mut cpu = StubCpu::new();
        let mut process = Process::map_into(
            image_based_at(0x10000, 0x1000),
            &mut cpu,
            &|_, _| None,
            &NullDispatcher,
        )
        .unwrap();
        process.state.message_frames.insert(
            0,
            GuestCallFrame {
                args: [0x1234, 2, 0, 0],
                sp: 0x2222,
                lr: 0x3333,
            },
        );
        cpu.write_reg(ArmReg::R0, 0xdeadbeef).unwrap();
        cpu.write_reg(ArmReg::R1, 17).unwrap();
        cpu.write_reg(ArmReg::Lr, 0x11000).unwrap();
        run_main_loop(&mut cpu, &mut process, &mut NullDispatcher, 100, 1).unwrap();
        assert_eq!(process.state.process_exit_code, None);
        assert_eq!(process.state.message_frames[&0].args[0], 0x1234);
        assert_eq!(process.state.thread_last_errors[&0], 6);
        assert_eq!(cpu.read_reg(ArmReg::R0).unwrap(), 0);
    }

    #[test]
    fn hybrid_byte_copy_hooks_only_the_host_fallback() {
        let mut cpu = StubCpu::new();
        let mut image = image_based_at(0x10000, 0x1000);
        image.imports.push(pocket_pe::ImportSymbol {
            dll: "COREDLL.dll".into(),
            binding: ImportBinding::Name("memcpy".into()),
            iat_va: 0x11080,
        });
        let process = Process::map_into(image, &mut cpu, &|_, _| None, &NullDispatcher).unwrap();
        let entry = cpu.read_u32_le(0x11080).unwrap();
        let fallback = entry + 28;
        assert_eq!(process.thunk_by_va[&entry], process.thunk_by_va[&fallback]);
        // StubCpu reports the first installed hook: it must be the tail,
        // not the native entry or a range swallowing the fast path.
        assert_eq!(
            cpu.run_until_hook(entry, 100).unwrap(),
            StopReason::Hook(fallback)
        );
        assert_eq!(cpu.read_u32_le(entry).unwrap(), 0xe3520001);
    }

    #[test]
    fn instruction_limit_still_services_host_hook() {
        let mut loader_cpu = StubCpu::new();
        let mut process = Process::map_into(
            image_based_at(0x10000, 0x1000),
            &mut loader_cpu,
            &|_, _| None,
            &NullDispatcher,
        )
        .unwrap();
        // A hook-free stub exhausts its budget on every slice. The loader's
        // stub has exit hooks, so use a fresh CPU for this specific path.
        let mut cpu = StrictMapCpu::new();
        cpu.map_region(0x11000, 0x1000, Prot::READ | Prot::WRITE | Prot::EXEC)
            .unwrap();
        cpu.write_mem(0x11000, &0xeafffffeu32.to_le_bytes())
            .unwrap();
        struct StopHook(usize);
        impl FrameHook for StopHook {
            fn on_frame(&mut self, state: &mut KernelState) -> FrameAction {
                self.0 += 1;
                state
                    .pending_input
                    .push_back(InputEvent::KeyDown { vk: 0x28 });
                FrameAction::Stop
            }
        }
        let mut hook = StopHook(0);
        run_main_loop_with_hook(
            &mut cpu,
            &mut process,
            &mut NullDispatcher,
            100,
            2,
            Some(&mut hook),
        )
        .unwrap();
        assert_eq!(hook.0, 1);
        // No audio or API hooks are needed for the host to regain control.
        assert_eq!(cpu.last_slice_timeout, Some(20_000));
        assert_eq!(
            process.state.pending_input.front(),
            Some(&InputEvent::KeyDown { vk: 0x28 })
        );
    }

    #[test]
    fn static_directdraw_frames_do_not_delay_frontend_input_until_readback() {
        let mut loader_cpu = StubCpu::new();
        let mut process = Process::map_into(
            image_based_at(0x10000, 0x1000),
            &mut loader_cpu,
            &|_, _| None,
            &NullDispatcher,
        )
        .unwrap();
        let mut cpu = StubCpu::new();
        cpu.map_region(0x11000, 0x1000, Prot::READ | Prot::WRITE | Prot::EXEC)
            .unwrap();
        cpu.write_mem(0x11000, &0xeafffffeu32.to_le_bytes())
            .unwrap();
        let started = Instant::now();
        let mut calls = 0;
        let before = process.state.framebuffer.frame_counter;
        let mut hook = |state: &mut KernelState| {
            assert_eq!(
                state.framebuffer.frame_counter, before,
                "a static image is not an input clock"
            );
            assert!(
                started.elapsed() < Duration::from_millis(200),
                "frontend inherited the 250 ms readback backoff"
            );
            let event = match calls {
                0 => {
                    state.direct_fb_frames += 1;
                    InputEvent::KeyDown { vk: 0x27 }
                }
                1 => InputEvent::KeyUp { vk: 0x27 },
                2 => InputEvent::KeyDown { vk: 0x27 },
                _ => InputEvent::KeyUp { vk: 0x27 },
            };
            state.pending_input.push_back(event);
            calls += 1;
            if calls == 4 {
                FrameAction::Stop
            } else {
                std::thread::sleep(Duration::from_millis(6));
                FrameAction::Continue
            }
        };
        run_main_loop_with_hook(
            &mut cpu,
            &mut process,
            &mut NullDispatcher,
            100,
            0,
            Some(&mut hook),
        )
        .unwrap();
        assert_eq!(calls, 4);
        assert_eq!(
            process
                .state
                .pending_input
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![
                InputEvent::KeyDown { vk: 0x27 },
                InputEvent::KeyUp { vk: 0x27 },
                InputEvent::KeyDown { vk: 0x27 },
                InputEvent::KeyUp { vk: 0x27 }
            ]
        );
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
            stack_reserve: 0x10000,
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

impl Drop for KernelState {
    fn drop(&mut self) {
        let gate = self.object_handles.execution_gate();
        let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
        if self.process_exit_code.is_none() {
            self.record_process_exit(0xc0000001);
        }
        for (handle, _) in self.object_handles.owned() {
            if let Some((object, last)) = self.object_handles.close(handle) {
                self.clean_object(object, last);
            }
        }
        self.vfs.close_all();
        self.internet.close_all();
    }
}
