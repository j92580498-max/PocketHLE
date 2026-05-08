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

use byteorder::{ByteOrder, LittleEndian};
use indexmap::IndexMap;
use thiserror::Error;

use pocket_cpu::{dump_mem_around, dump_regs, regs::ArmReg, Cpu, CpuError, Prot, StopReason};
use pocket_pe::{ImportBinding, ImportSymbol, LoadedImage, ResourceEntry};

pub mod audio;
pub mod font;
pub mod framebuffer;
pub mod gdi;
pub mod vfs;

pub use audio::{AudioEngine, GuestFormat};
pub use framebuffer::{Framebuffer, FB_BYTES, FB_HEIGHT, FB_WIDTH};
pub use gdi::{GdiState, Surface};

/// Default base address of the synthetic IAT thunk pool.
pub const THUNK_REGION_BASE: u32 = 0x7000_0000;
/// Each thunk is exactly one 32-bit instruction. We never execute it
/// — the CPU hook stops us first — but we still write a `bx lr` so
/// that an accidental fall-through returns rather than crashes.
pub const THUNK_STRIDE: u32 = 4;
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

/// Base of the guest-side heap region. 16 MiB is plenty for the
/// little games we target and still leaves headroom for the stack.
pub const HEAP_BASE: u32 = 0x5000_0000;
/// 64 MiB. Real Pocket PC processes only get ~32 MiB of total VA,
/// but games almost never come close to that — and our handle table
/// for `CreateDIBSection` etc. lives inside the same heap region,
/// so we keep it generous so back-to-back DIB allocations don't run
/// the game into an unmapped page.
pub const HEAP_SIZE: u32 = 0x0400_0000;

/// "bx lr" in ARM mode (little endian).
pub const ARM_BX_LR: [u8; 4] = [0x1e, 0xff, 0x2f, 0xe1];

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
    /// Hardware / D-pad / virtual-keyboard key press. `vk` is a
    /// standard Win32 virtual-key code (e.g. `VK_LEFT = 0x25`).
    KeyDown {
        vk: u16,
    },
    KeyUp {
        vk: u16,
    },
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
}

/// Mutable kernel state that persists across calls and that handlers
/// need to read or modify. Bundled into one struct so we can hand it
/// out by `&mut` without conflicting with the immutable parts of
/// [`Process`] (image bytes, thunk table) that the run loop uses.
pub struct KernelState {
    pub heap: Heap,
    pub vfs: vfs::Vfs,
    /// Software-rendered display the GDI/GAPI handlers paint into.
    pub framebuffer: Framebuffer,
    /// Tracked GDI objects (DCs, bitmaps, brushes, pens, fonts).
    pub gdi: GdiState,
    /// Flat resource table for `FindResourceW` / `LoadResource`.
    pub resources: Vec<ResourceEntry>,
    /// Image base for resource RVA → VA conversion.
    pub image_base: u32,
    /// Set the first time `GXBeginDraw` runs. The dispatcher maps the
    /// framebuffer region into the guest VA space lazily — but that
    /// requires `&mut dyn Cpu`, which isn't available outside a call,
    /// so we let the GAPI handlers do it.
    pub fb_mapped: bool,
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
    /// Frame counter snapshot taken the last time `GXEndDraw`
    /// finished pushing pixels into the host framebuffer. If
    /// `framebuffer.frame_counter` still equals this when the next
    /// `GXBeginDraw` runs, we know no GDI handler has touched the
    /// host fb in the meantime, and we can skip the
    /// host-fb -> guest-fb copy that primes the back-buffer.
    pub gx_last_pushed_counter: u64,
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
    /// `nIDEvent` of the timer the guest most recently registered via
    /// `SetTimer`, or `0` if none. The synthetic message pump uses
    /// this to inject `WM_TIMER` messages with a wParam the guest
    /// will recognise.
    pub synthetic_timer_id: u32,
    /// `true` once the synthetic message pump has delivered
    /// `WM_CREATE`. Real Windows fires `WM_CREATE` synchronously
    /// from `CreateWindowExW`; we instead fire it on the very first
    /// `GetMessageW` so the guest's `WndProc` runs its window-init
    /// code (which typically calls `SetTimer`).
    pub synthetic_create_sent: bool,
    /// Input events queued by the host frontend (mouse / D-pad /
    /// keyboard). Drained by `GetMessageW` / `PeekMessageW` before
    /// any synthetic timer / paint message is fabricated, so real
    /// user input always wins over the synthetic pump.
    pub pending_input: VecDeque<InputEvent>,
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
    /// next iteration. The map is keyed by the iterator's own
    /// `thunk_va` so a nested `??_L` from inside a ctor doesn't
    /// collide with the outer one.
    pub vector_iter_frames: HashMap<u32, VectorIterFrame>,
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

/// The whole emulated process state owned by the kernel.
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
    pub fn map_into(
        image: LoadedImage,
        cpu: &mut dyn Cpu,
        ordinal_resolver: &dyn Fn(&str, u16) -> Option<String>,
    ) -> Result<Self, KernelError> {
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

        // 2. Allocate a thunk pool and patch the IAT to point into it.
        let thunk_count = image.imports.len() as u32;
        let thunk_size = pocket_cpu::round_up_to_page(thunk_count * THUNK_STRIDE).max(0x1000);
        cpu.map_region(THUNK_REGION_BASE, thunk_size, Prot::READ | Prot::EXEC)?;
        let mut thunks = Vec::with_capacity(image.imports.len());
        let mut thunk_by_va = HashMap::with_capacity(image.imports.len());
        for (i, imp) in image.imports.iter().enumerate() {
            let thunk_va = THUNK_REGION_BASE + (i as u32) * THUNK_STRIDE;
            cpu.write_mem(thunk_va, &ARM_BX_LR)?;
            cpu.add_code_hook(thunk_va)?;
            let friendly_name = match &imp.binding {
                ImportBinding::Name(n) => Some(n.clone()),
                ImportBinding::Ordinal(o) => ordinal_resolver(&imp.dll, *o),
            };
            let mut iat_bytes = [0u8; 4];
            LittleEndian::write_u32(&mut iat_bytes, thunk_va);
            cpu.write_mem(imp.iat_va, &iat_bytes)?;
            thunks.push(Thunk {
                thunk_va,
                iat_va: imp.iat_va,
                dll: imp.dll.clone(),
                binding: imp.binding.clone(),
                friendly_name,
            });
            thunk_by_va.insert(thunk_va, i);
        }

        // 3. Map a stack.
        let stack_size = DEFAULT_STACK_SIZE;
        let stack_top = DEFAULT_STACK_TOP;
        let stack_base = stack_top - stack_size;
        cpu.map_region(stack_base, stack_size, Prot::READ | Prot::WRITE)?;
        cpu.write_reg(ArmReg::Sp, stack_top - 16)?;
        // Seed `LR` so that when the entry point eventually returns,
        // the CPU jumps to a known address we have a hook on
        // (`PROCESS_EXIT_TRAMPOLINE_VA`) instead of an
        // uninitialised 0. The run loop turns hits there into a
        // graceful shutdown — see `run_main_loop_with_hook`.
        cpu.write_reg(ArmReg::Lr, PROCESS_EXIT_TRAMPOLINE_VA)?;

        // 4. Map a heap.
        cpu.map_region(HEAP_BASE, HEAP_SIZE, Prot::READ | Prot::WRITE)?;
        let heap = Heap::new(HEAP_BASE, HEAP_SIZE);

        // 5. Map the WinCE kernel trap region. Real WinCE kernels
        //    publish syscall entry points at fixed offsets inside
        //    `0xF000_0000+`. We don't know the exact callsites
        //    coredll routes through this range, so we fill the page
        //    with `bx lr` — any guest jump there returns harmlessly.
        cpu.map_region(KERNEL_TRAP_BASE, KERNEL_TRAP_SIZE, Prot::READ | Prot::EXEC)?;
        let mut trap_page = Vec::with_capacity(KERNEL_TRAP_SIZE as usize);
        while trap_page.len() < KERNEL_TRAP_SIZE as usize {
            trap_page.extend_from_slice(&ARM_BX_LR);
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
        Ok(Process {
            image,
            thunks,
            thunk_by_va,
            stack_top,
            stack_size,
            state: KernelState {
                heap,
                vfs: vfs::Vfs::new(),
                framebuffer: Framebuffer::default(),
                gdi: GdiState::new(),
                resources,
                image_base: img_base,
                fb_mapped: false,
                gx_readback_scratch: Vec::new(),
                mem_op_scratch: Vec::new(),
                mem_op_scratch_b: Vec::new(),
                gx_last_pushed_counter: 0,
                synthetic_message_count: 0,
                synthetic_message_budget: 240,
                wnd_proc: 0,
                synthetic_timer_id: 0,
                synthetic_create_sent: false,
                pending_input: VecDeque::new(),
                should_stop: false,
                tls_slots_used: 0,
                vector_iter_frames: HashMap::new(),
                security_cookie: 0,
                audio: AudioEngine::new(),
                wave_out_format: GuestFormat::default(),
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
pub fn run_main_loop_with_hook(
    cpu: &mut dyn Cpu,
    process: &mut Process,
    dispatcher: &mut dyn Dispatcher,
    instruction_budget_per_slice: u64,
    max_slices: u64,
    mut frame_hook: Option<&mut dyn FrameHook>,
) -> Result<(), KernelError> {
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
        Err(_) => process.image.entry_va(),
    };
    log::info!(
        "entering emulated main: entry=0x{:08x}, stack_top=0x{:08x}",
        pc,
        process.stack_top
    );
    for _slice in 0..max_slices {
        // PC=0 (or any address in the unmapped null page) means
        // the guest jumped through a null function pointer or popped
        // a poisoned LR off the stack. Without an explicit halt,
        // unicorn's `emu_start` typically returns `Ok(0 instructions)`
        // and we'd spin forever. Surface it as a real crash with the
        // CPU dump.
        if pc < 0x1000 {
            log::error!(
                "guest jumped to NULL/low address pc=0x{pc:08x}\n{regs}",
                regs = dump_regs(cpu),
            );
            return Err(KernelError::Loader(format!(
                "guest jumped to unmapped address 0x{pc:08x}"
            )));
        }
        let stop = match cpu.run_until_hook(pc, instruction_budget_per_slice) {
            Ok(s) => s,
            Err(e) => {
                let pc_now = cpu.read_reg(ArmReg::Pc).unwrap_or(pc);
                log::error!(
                    "cpu crashed: {e}\n  last requested pc=0x{pc:08x}, current pc=0x{pc_now:08x}\n{regs}{mem}",
                    regs = dump_regs(cpu),
                    mem = dump_mem_around(cpu, pc_now, 16),
                );
                return Err(e.into());
            }
        };
        match stop {
            StopReason::InstructionLimit => {
                log::trace!("instruction slice exhausted; resuming");
                pc = cpu.read_reg(ArmReg::Pc)?;
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
                let outcome = match process.find_thunk_and_state(addr) {
                    Some((thunk, state)) => {
                        // Split borrow: `thunk` borrows
                        // `process.thunks` immutably and `state`
                        // borrows `process.state` mutably. No clone,
                        // and no per-call heap allocation — that
                        // matters because this branch fires on every
                        // single emulated WinCE API call.
                        let outcome = dispatcher.dispatch(cpu, thunk, state)?;
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
                            log::debug!(
                                "kernel-trap soft-return at 0x{addr:08x} (R0=0x{r0:08x}, LR=0x{lr:08x})",
                                r0 = cpu.read_reg(ArmReg::R0).unwrap_or(0),
                                lr = cpu.read_reg(ArmReg::Lr).unwrap_or(0),
                            );
                            let lr = cpu.read_reg(ArmReg::Lr)?;
                            pc = lr & !1;
                            // Skip the frame-hook for this slice —
                            // we did not actually advance the
                            // emulator, just bounced through a trap.
                            continue;
                        }
                        // Some other host-installed hook (e.g.
                        // `--watch` from the CLI). Dump CPU state
                        // and halt cleanly.
                        log::warn!("watch hit at 0x{addr:08x}\n{regs}", regs = dump_regs(cpu),);
                        return Ok(());
                    }
                };
                match outcome {
                    DispatchOutcome::Halt => {
                        return Ok(());
                    }
                    DispatchOutcome::ReturnedR0(v) => {
                        cpu.write_reg(ArmReg::R0, v)?;
                        let lr = cpu.read_reg(ArmReg::Lr)?;
                        pc = lr & !1;
                    }
                    DispatchOutcome::ReturnedR0R1(a, b) => {
                        cpu.write_reg(ArmReg::R0, a)?;
                        cpu.write_reg(ArmReg::R1, b)?;
                        let lr = cpu.read_reg(ArmReg::Lr)?;
                        pc = lr & !1;
                    }
                    DispatchOutcome::Unimplemented => {
                        cpu.write_reg(ArmReg::R0, 0)?;
                        let lr = cpu.read_reg(ArmReg::Lr)?;
                        pc = lr & !1;
                    }
                    DispatchOutcome::JumpTo(target) => {
                        // Trampoline into a guest function — `target` is
                        // the new PC, the handler is responsible for
                        // setting LR / R0..R3 / SP appropriately.
                        pc = target & !1;
                    }
                }
            }
            StopReason::Requested | StopReason::OutOfBounds => return Ok(()),
        }
        if let Some(hook) = frame_hook.as_deref_mut() {
            if hook.on_frame(&mut process.state) == FrameAction::Stop {
                log::info!("frame hook requested stop");
                return Ok(());
            }
        }
        if process.state.should_stop {
            log::info!("frame hook flagged should_stop");
            return Ok(());
        }
    }
    let pc_now = cpu.read_reg(ArmReg::Pc).unwrap_or(0);
    log::warn!(
        "main loop hit max_slices={max_slices}; exiting at pc=0x{pc_now:08x}\n{regs}{mem}",
        regs = dump_regs(cpu),
        mem = dump_mem_around(cpu, pc_now, 16),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::stub::StubCpu;
    use pocket_pe::{LoadedImage, LoadedSection};

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
        };
        let mut cpu = StubCpu::new();
        let p = Process::map_into(img, &mut cpu, &|_, _| None).unwrap();
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
