# PocketHLE — architecture reference for coding agents

PocketHLE runs Windows CE / Windows Mobile (Pocket PC) applications on
modern hosts by **high-level emulation**: the guest's ARM (or MIPS) code
is executed instruction-by-instruction, but every call into a Windows CE
DLL is intercepted at the import boundary and serviced by clean-room Rust
instead of by emulated OS code. There is no emulated `coredll.dll`, no
emulated kernel, and no emulated device drivers.

Read this file before changing anything. It records the invariants that
are *not* recoverable from reading a single file, and the places where an
obvious-looking change breaks a specific shipped game. Everything here
was verified against the tree it ships with — when you change behaviour,
update this file in the same commit.

## 1. Non-negotiable invariants

Violating any of these breaks games that currently work. They are listed
first because they are the ones most often broken by a plausible-looking
refactor.

1. **Dispatcher keys always carry the `.dll` suffix, lower-cased.**
   Handler keys look like `("coredll.dll", "MessageBoxW")`. Four
   independent consumers assume this: `WinCeDispatcher::resolve_handler`,
   `native_thunks::native_thunk_for`, `Dispatcher::constant_for`, and the
   hard-coded DLL list in `Process::map_into`
   (`crates/pocket-kernel/src/lib.rs:1620`). CeGCC images write the bare
   name (`COREDLL`), so `pocket-pe` normalizes it at the loader boundary —
   see §7.
2. **`Dispatcher::constant_for` must be deterministic per thunk.** The
   kernel calls it exactly once per import at load time and bakes the
   answer into the guest's IAT. A handler that needs to observe mutable
   state cannot be a constant.
3. **The guest entry point is entered with `WinMain` arguments already in
   registers.** The CE loader does this, so PocketHLE does too:
   `r0 = hInstance` (`PROCESS_INSTANCE_HANDLE`), `r1 = 0`,
   `r2 = lpCmdLine`, `r3 = nCmdShow` (`SW_SHOWNORMAL`). `lpCmdLine` is
   **always a valid pointer** — empty `L""` when there are no arguments,
   never `NULL`. Games dereference it without checking.
4. **`LR` on entry is `PROCESS_EXIT_TRAMPOLINE_VA`, not 0.** When
   `mainCRTStartup` eventually returns, the CPU lands on a hooked address
   and shuts down gracefully. With `LR = 0`, every game "crashes" at
   `pc=0x00000000` after a completely successful run.
5. **`eglSwapBuffers` is the only point GL output becomes visible**, and
   when GAPI is also mapped it must push pixels into the guest mapping.
   See §6 — this is a fixed bug with a regression test; don't reintroduce
   it.
6. **`WaitForSingleObject` must respect real event state.** Answering
   every wait with `WAIT_OBJECT_0` makes worker threads exit on their
   first iteration; Asphalt 2 3D opens its wave device and then never
   writes a single buffer. An `INFINITE` wait from a *worker* is also a
   scheduling point: it re-parks the thread at its own thunk so the call
   re-runs with its arguments intact — see §10.
7. **Worker threads never receive the window queue's synthetic traffic.**
   A worker running its own pump would dispatch forever and the thread
   that actually renders would never get the CPU back
   (`crates/pocket-winceapi/src/coredll.rs:6686` and `:6743`).
8. **`arch_all` is deliberately disabled on `unicorn-engine`.** Enabling
   it breaks the Android NDK cross-compile on QEMU's x86 `cpuid.h`.
9. **`CreateThread` returns to its creator; it never enters the new
   thread.** Every thread parks at its entry point, `CREATE_SUSPENDED` or
   not — see §10.
10. **`frame_counter` moves only when pixels change.** It is the host's
    "a new frame is ready" signal, and every frontend's frame budget is
    spent off it. `InvalidateRect` bumping it cost Toy Golf its entire
    `--max-frames` budget on identical black startup frames — see §6.

## 2. Crate graph

Dependencies point downward; nothing below depends on anything above.

```
frontends/  pocket-cli      pocket-desktop (egui)   pocket-android-jni
                  \                |                      /
                   +-------- pocket-core -----------------+   orchestration:
                                  |                          load, run loop, frames
                   +--------------+----------------+
                   |              |                |
            pocket-winceapi  pocket-kernel   pocket-library
            (guest DLL         (state,         (game catalog,
             surface)           memory map,     install metadata)
                   |            run loop)
                   |              |      \
            pocket-gles     pocket-cpu    pocket-pe --- pocket-cab
            (software GL)   (Unicorn)     (PE32 loader)  (installers)
```

| Crate | Responsibility |
| --- | --- |
| `pocket-cab` | Extracts CAB, InstallShield and zip installers; decides which install directories to mount. |
| `pocket-pe` | Parses and lays out PE32 images; collects imports, exports, resources, icons. Detects managed (.NET CF) images. |
| `pocket-cpu` | `Cpu` trait over two backends: `stub` (trace-only) and `unicorn` (Unicorn Engine 2). |
| `pocket-kernel` | The address space, `KernelState`, thunk/IAT machinery, the run loop, and host subsystems: VFS, registry, framebuffer, GDI, GAPI, audio, fonts, message boxes, native thunks. |
| `pocket-winceapi` | The guest-facing API surface. One module per emulated DLL; registers handlers into `WinCeDispatcher`. |
| `pocket-gles` | Software OpenGL ES 1.x: geometry, rasterizer, textures, fixed-point and matrix math. Host-side only — knows nothing about guest memory. |
| `pocket-core` | Wires loader + CPU + kernel + dispatcher into an `Emulator` the frontends drive. |
| `pocket-library` | Game catalog: identifies titles, tracks install state and per-game metadata. |

Frontends: `pocket-cli` (headless, scriptable — the one to use for
debugging), `pocket-desktop` (egui), `pocket-android-jni` plus the Kotlin
app in `pocket-android`.

## 3. The guest address space

Every constant below lives in `crates/pocket-kernel/src/lib.rs`. The map
is fixed, not discovered — a game that hard-codes an address is relying on
these exact values.

| Region | Base | Size / stride | Purpose |
| --- | --- | --- | --- |
| Slot 0 alias | `0x0001_0000` | `0x0002_0000` | `SLOT_ALIAS_BASE`. WinCE ran the active process aliased into slot 0; images based at `0x0001_0000` are mapped here *as well as* at their own base. |
| Module region | `0x3000_0000` | stride `0x0100_0000`, end `0x4000_0000` | `MODULE_REGION_BASE`. Where an image goes when its requested base is taken or unusable. 16 slots. |
| Heap | `0x5000_0000` | `0x0400_0000` | `HEAP_BASE` / `HEAP_SIZE`. Backs `malloc`, `LocalAlloc`, `VirtualAlloc`, `HeapAlloc` — one bump/free allocator, not per-heap. |
| Stack | top `0x6000_0000` | `0x40000` | `DEFAULT_STACK_TOP` / `DEFAULT_STACK_SIZE`. Grows down. Each extra thread gets its own stack below this. |
| Thunk pool | `0x7000_0000` | `THUNK_STRIDE` = 32 | `THUNK_REGION_BASE`. One 32-byte slot per import. The slot address *is* the identity of the import — see §5. |
| Synthetic framebuffer | `0x7800_0000` | screen-sized | `SYNTHETIC_FRAMEBUFFER_BASE`. What `GXBeginDraw` hands the guest when there is no real device buffer. |
| Kernel traps | `0xF000_0000` | `0x0001_0000` | `KERNEL_TRAP_BASE`. Hooked addresses that are never real code. |
| Process exit | `0xF000_FF00` | — | `PROCESS_EXIT_TRAMPOLINE_VA`. Initial `LR`; see invariant 4. |
| Thread exit | `0xF000_FE00` | one per thread | `THREAD_EXIT_TRAMPOLINE_BASE`. Initial `LR` for spawned threads. |
| User KData page | `0xFFFF_C000` | `0x1000` | `USER_KDATA_PAGE_BASE`. CE's shared read-only kernel page. `USER_KDATA_STRUCT_VA` = `0xFFFF_C800`; the TLS array is at `USER_KDATA_TLS_ARRAY_VA` = `0xFFFF_CB00` with `TLS_SLOT_COUNT` = 64. Games read the tick count straight out of this page instead of calling `GetTickCount`, so it must be kept current every slice. |

Sentinel handles, deliberately outside every mapped region so a guest
that dereferences one faults loudly instead of corrupting data:

| Constant | Value |
| --- | --- |
| `FAKE_CURRENT_THREAD_HANDLE` | `0xC0DE_0001` |
| `FAKE_CURRENT_PROCESS_HANDLE` | `0xC0DE_0002` |
| `PROCESS_INSTANCE_HANDLE` | `0x1000_0000` |
| `GLES_CM_MODULE_HANDLE` | `0x1000_0004` |
| `GLES_CL_MODULE_HANDLE` | `0x1000_0005` |
| `HSS_MODULE_HANDLE` | `0x1000_0006` |

`pocket-pe` deliberately does **not** apply WinCE's XIP-in-ROM mapping or
per-process slot relocation. Each game gets a private flat 32-bit space,
so one contiguous mapping is enough; base relocations are applied only
when the requested image base is not free.

## 4. Loading, and the three IAT strategies

`Process::map_into` (`crates/pocket-kernel/src/lib.rs:1453`) is the whole
load path. In order:

1. Map each section at `image_base + virtual_address`, honouring the
   section characteristics for permissions.
2. If the image is based at `SLOT_ALIAS_BASE`, map it a second time into
   the slot-0 alias window.
3. Map the thunk pool, one `THUNK_STRIDE`-byte slot per import.
4. Map the User KData page and prime the tick fields.
5. For each import, pick **one** of the three strategies below and write
   the resulting address into `iat_va`.
6. Register dynamic exports so `GetProcAddress` can resolve names that
   were never in the import directory.

The strategies, in the order they are tried:

| Order | Strategy | Condition | Cost |
| --- | --- | --- | --- |
| 1 | **Native ARM/VFP code** written into the thunk slot | `native_thunks::native_thunk_for` returns code. Gated on `dll.eq_ignore_ascii_case("coredll.dll")` (`native_thunks.rs:225`) and **never** used for MIPS. | Zero host round-trips. `memcpy`, `strlen`, float helpers. |
| 2 | **Constant return**: `mov r0, #imm; bx lr` | Native declined *and* `cpu.arch() == Arm` *and* `Dispatcher::constant_for` answers. | Zero host round-trips. Derby's imports hit this ~33% of the time. |
| 3 | **`bx lr` + code hook** → Rust `Handler` | Everything else. `ARM_BX_LR` = `[0x1e, 0xff, 0x2f, 0xe1]`; MIPS gets `MIPS_JR_RA`. | One host call per guest call. |

Strategy 2 is why invariant 2 exists: `constant_for` is consulted once,
at load time, and its answer is frozen into guest memory. A handler whose
return value depends on mutable state must be strategy 3.

The dynamic-export DLL list in step 6 is hard-coded at
`crates/pocket-kernel/src/lib.rs:1620`: `coredll.dll`, `commctrl.dll`,
`gx.dll`, `ddraw.dll`, `libgles_cm.dll`, `libgles_cl.dll`, `hss.dll`.
Adding a module to `pocket-winceapi` without adding it here means
`GetProcAddress` on it returns `NULL`.

## 5. Dispatch

A `Thunk` (`crates/pocket-kernel/src/lib.rs:1211`) is the unit of
identity for an intercepted import:

```rust
pub struct Thunk {
    pub thunk_va: u32,        // slot in the thunk pool — the primary key
    pub iat_va: u32,          // where in the guest IAT the address was written
    pub dll: String,          // lower-cased, WITH the .dll suffix
    pub binding: ImportBinding,
    pub friendly_name: String,
}
```

A handler returns a `DispatchOutcome` (`:521`):

| Variant | Meaning |
| --- | --- |
| `ReturnedR0(u32)` | Normal return. `void` handlers return 0; the guest ignores it. |
| `ReturnedR0R1(u32, u32)` | 64-bit return, low word in r0. |
| `JumpTo(va)` | Resume the guest at `va` instead of `LR`. Used to block modally by re-entering the same thunk — see `message_box_w`. |
| `Halt(reason)` | Stop the run loop. `ExitProcess`, `TerminateProcess`. |

**`resolve_handler` memoizes by `thunk_va`, including negative results**
(`crates/pocket-winceapi/src/lib.rs:280`). Two consequences:

* Registering a handler after the first call to a thunk has no effect on
  that thunk.
* In tests, every `Thunk` you build must get a **distinct** `thunk_va`.
  The `fake_thunk` helper (`:459`) leaves it at 0, so a loop that reuses
  it answers every lookup from the first entry's cache — this has already
  produced one confusing test failure.

Unresolved lookups fall back to `ignored_dll` → `ignored_dll_stub`.
`IGNORED_DLLS` (`:83`) currently holds only the `fmodce` prefix, with a
`FSOUND_GetVersion` → `0x4070_0000` override so version checks pass.

WinCE `coredll`, `aygshell` and the GLES libraries are frequently imported
**by ordinal**, with no name in the import directory at all. The ordinal →
name tables are JSON data files, not code:
`crates/pocket-winceapi/data/coredll-ordinals.json`,
`aygshell-ordinals.json`, and `crates/pocket-gles/data/`
`libgles_cl-ordinals.json` / `libgles_cm-ordinals.json`. A game whose
imports all show as `ord NNN` in the trace is missing an entry here, not a
handler.

One module per emulated DLL in `crates/pocket-winceapi/src/`: `aygshell`,
`commctrl`, `coredll`, `ddraw`, `dlgtemplate`, `game_dlls`, `gles`, `gx`,
`hss`, `ole32`, plus `ordinals` and `lib`. Host-side subsystems live in
`crates/pocket-kernel/src/`: `audio`, `controls`, `font`, `framebuffer`,
`gapi`, `gdi`, `msgbox`, `native_thunks`, `registry`, `tracker`, `vfs`.

## 6. Presentation — two paths

Everything a user sees arrives by exactly one of these:

**EGL / GLES.** The guest draws through `libGLES_CM` / `libGLES_CL` into
`pocket-gles`'s software rasterizer, and `eglSwapBuffers` converts the
RGBA8888 result to RGB565 and publishes it. Nothing before
`eglSwapBuffers` is visible — invariant 5.

**GAPI (`gx.dll`).** The guest asks `GXBeginDraw` for a raw framebuffer
pointer, writes pixels directly, and calls `GXEndDraw`.
`sync_guest_framebuffer` (`:1938`, called from the run loop at `:2288`)
reads that mapping back out of guest memory each slice.

**When a game maps both**, `eglSwapBuffers` must also push the presented
pixels into the guest GAPI mapping. Call of Duty 2 does exactly this: it
sets up GAPI, then renders through GL, and without the push
`sync_guest_framebuffer` keeps reading the never-written GAPI mapping and
the screen stays black through a run that is otherwise completely
correct. The fix is guarded by `ctx.kernel.fb_mapped` in
`crates/pocket-winceapi/src/gles.rs` and pinned by
`swap_buffers_pushes_pixels_into_a_mapped_gapi_framebuffer`.

`frame_counter` is the liveness diagnostic *and* the host's "new pixels
are ready" signal — the CLI's frame-dump hook and `--max-frames` both
fire off it. `frame_counter=0` after a run that reported no errors means
the guest executed fine and nothing reached a presentation point — look
at the two paths above, not at the CPU.

**The display is 240x320 portrait unless something says otherwise**, which
is the Pocket PC these games mostly shipped on. Geometry is not a
preference a user can revise mid-run: a GAPI or GL ES title reads the
display size once during start-up and lays its whole scene out around it.
So a launcher that recognises the device an archive shipped on sets the
geometry before the run. `Launcher::native_screen`
(`frontends/pocket-cli/src/archive.rs`) carries that — a Gizmondo card
layout yields `GIZMONDO_SCREEN`, the console's 320x240 landscape LCD — and
`--screen` overrides it; `pocket-library`'s `ScreenPref::Landscape` is the
same fact for the desktop and Android launchers. At the portrait default,
Sticky Balls asked GL ES for a viewport the size of the display and
rendered a portrait slice of a landscape scene with its HUD off the edge.

The inverse failure is subtler and cost a day on Toy Golf: a handler that
bumps the counter *without* changing pixels spends the frame budget on
duplicates. `InvalidateRect` did exactly that, 99,490 times from Toy
Golf's idle loop, so every captured frame was the same black startup
image and the run ended before the game drew a thing — with 153,600
non-zero pixels sitting in the framebuffer the whole time. Only
`eglSwapBuffers` and `sync_guest_framebuffer` may move the counter.
`--dump-frame-stride` thins the captures of a GDI title that legitimately
blits far more often than it changes anything interesting.

Two rasterizer details that look like bugs and are not: an incomplete
texture samples as opaque white (matching GL ES), and the software GL
tests share a `TEST_LOCK` because the context is process-global.

## 7. Two guest toolchains

PocketHLE loads images from both the MSVC-era official toolchains and the
open-source CeGCC / mingw32ce one. They differ in ways that reach the
loader:

| | MSVC (eVC++ / VS2005-2008) | CeGCC / mingw32ce (GCC 4.1.0) |
| --- | --- | --- |
| Import DLL name | `COREDLL.dll` | `COREDLL` — **no extension** |
| CRT startup | `mainCRTStartup` | `crt3.c`, `__mingw_do_global_ctors` |
| Startup CRT calls | — | `_fpreset` before `main`, `_fcloseall` on the way into `ExitProcess` |
| Debug sections | MSVC-named | GNU-style numbered |

The suffix-less name is the one that matters. Every lookup path keys on
the lower-cased name **with** `.dll` (invariant 1), so a CeGCC image built
`("coredll", "malloc")` and missed `("coredll.dll", "malloc")` — and *every*
import silently degraded to an unimplemented stub. Because four
independent consumers share that assumption, the fix belongs at the single
boundary where the name enters the system, not in each consumer:
`normalize_import_dll` in `crates/pocket-pe/src/lib.rs` appends `.dll` to
a name that has no extension at all, and leaves anything already carrying
one alone (`.cpl` and `.drv` are real WinCE module extensions, and
`fmodce370.dll` must keep its dots).

`hello.exe` at the repository root is the CeGCC smoke test. A correct run
shows **zero** "unimplemented call" warnings, renders a "Test App /
Hello, World! / OK" message box, accepts Enter, and halts through
`ExitProcess`.

## 8. The synthetic message pump — read this before diagnosing a stall

There is no host message queue. `GetMessageW` and `PeekMessageW`
fabricate `WM_PAINT` / `WM_TIMER` traffic, and once
`synthetic_message_count` reaches `synthetic_message_budget` they
fabricate `WM_QUIT` (`crates/pocket-winceapi/src/coredll.rs:6682` and
`:6739`). The guest then runs its own perfectly ordinary shutdown.

The default differs by frontend, and this is the trap:

| Frontend | Budget |
| --- | --- |
| `pocket-cli` | **240** (`--message-budget`, `0` = unlimited) |
| `pocket-desktop` | 0 — unlimited (`src/runner.rs:159`) |
| `pocket-android-jni` | 0 — unlimited (`src/runner.rs:332`) |

**Frames stopping is not automatically a graphics bug.** Call of Duty 2
exhausts the 240-message budget during its menu fade-in, around frame 6.
It then saves `profiles.dat`, tears down GL and audio, and calls
`ExitProcess(0x42)` — a shutdown indistinguishable in the trace from the
user choosing Quit. Under `--message-budget 0` the same build keeps
rendering indefinitely, with non-black content growing monotonically as
the menu fades in.

So: when a game stops producing frames mid-load but exits *cleanly*, re-run
with `--message-budget 0` before touching anything else. If that fixes it,
the emulator was working and the CLI's cap was the whole story. The
default stays at 240 deliberately — it keeps headless and CI runs bounded.

**A guest that ignores `WM_QUIT` is halted rather than answered forever.**
`quit_or_halt` (`coredll.rs:7211`) hands out the quit and, once the budget
has been spent for `QUIT_POLL_GRACE` = 64 further polls, ends the run.
X-Forge (Ball Busters, Sticky Balls) only acts on a `WM_QUIT` that came
from `GetMessage`; the one its `PeekMessage` pump receives is dispatched
like any other message and dropped. Repeating it meant the game never
reached its render branch again and spun in the pump until `max_slices`
ran out — Ball Busters froze on the publisher logo at frame ~230 and spent
the rest of the run at a fortieth of its frame rate. A pump that *does*
honour the quit breaks out immediately and only comes back through here
while tearing down, so 64 is slack, not a second budget.

## 9. Run loop

`pocket-core` drives fixed slices of guest execution. Each slice: run the
CPU until the slice budget is spent or a hook fires, refresh the User
KData tick fields, then `sync_guest_framebuffer`. `--max-slices` bounds
the run (checked at `crates/pocket-kernel/src/lib.rs:2060`); `--max-frames`
bounds the frames captured. A `Halt` outcome ends the loop immediately.

`message_box_w` (`coredll.rs:8928`) is modal by re-entering its own thunk
via `JumpTo(ctx.thunk.thunk_va)`, capped at `MESSAGE_BOX_MAX_SPINS`
= 100 000 (`:8912`). Hundreds of identical `MessageBoxW ... status:"trampoline"`
records in a trace are that mechanism working, not a spin — they are what
lets the host present frames while the box is up.

## 10. Threads and the cooperative scheduler

One guest thread runs at a time. There is no host thread per guest
thread; the scheduler is cooperative and the only scheduling points are
`Sleep`, `WaitForSingleObject`, `WaitForMultipleObjects`, `GetMessageW`
and `PeekMessageW`. A guest that spins without calling one of those
starves every other thread by construction.

**`CreateThread` parks the new thread and returns the handle to its
creator** (invariant 9). It does not enter the thread, `CREATE_SUSPENDED`
or not — that flag only decides whether `started` is set, i.e. whether
the scheduler may pick the thread up yet. Entering immediately is the
obvious-looking implementation and it breaks Toy Golf: its audio thread
dereferences the mixer at `[r5,#0x38]` that its creator has not stored
yet, faulting on a NULL read at `0x00000038`.

Two park helpers, both in `coredll.rs`:

| Helper | Use |
| --- | --- |
| `park_worker_at(.., return_r0)` | The call is finished. Resume past it, optionally with a return value in `r0`. |
| `park_worker_and_reevaluate` | The call is *blocked*. Re-park at `ctx.thunk.thunk_va` so it re-runs later with its arguments untouched. |

The distinction is not cosmetic. An `INFINITE` `WaitForSingleObject` that
resumes through `park_worker_at` finds `r0` overwritten with a return
value and waits on that instead of its handle. When the *main* thread
issues an infinite wait, nothing can signal the object from there, so the
permissive `WAIT_OBJECT_0` stays — honouring it would deadlock.

`retire_wave_buffer` must deliver **every** `WaveCallbackKind`, including
`Event`, which signals the event rather than calling anything. That event
is typically a mixer thread's only back-pressure: leave it clear and the
wait falls through, the thread refills as fast as the CPU allows, never
yields, and the renderer never runs again. Toy Golf hung this way with
1,036,104 `waveOutWrite` calls and exactly one thread switch.

Worker threads never see the window queue's synthetic traffic
(invariant 7) — a worker running its own pump would dispatch forever.

## 11. Removable storage and stream devices

A game that shipped on a card checks the card is still there, and a
Windows CE program does that through the filesystem rather than through
any storage API. Three pieces make that work, all reached from
`CreateFileW`:

**`Vol:` is a handle on a volume, not a file.** CE exposes every mounted
volume as a `Vol:` pseudo-file inside it, so `CreateFileW("\\SD Card\\Vol:")`
returns something `DeviceIoControl` accepts for storage queries
(`crates/pocket-kernel/src/vfs.rs:32`). `Vfs` keeps those handles in a
table of their own, apart from open files — nothing reads or writes them,
and each carries the mount it named so a query can answer about *that*
volume.

**A volume has a serial, and for a Gizmondo card it is the card's own.**
`OpenVolume::serial()` never returns zero, because a guest that asks for
a serial and gets zero concludes the slot is empty. A Gizmondo card
states its serial in its own contents: beside the game directory sits a
four-byte marker file with the same name as that directory
(`\SD Card\GZGA200045\GZGA200045`), holding the serial of the card the
title was published on. Reporting that value is what makes the card in
the slot *be* the one the content was written for, which is what the
game is really asking. Any other volume gets an FNV-1a hash of its host
path, forced non-zero: arbitrary, but stable across runs.

**`IOCTL_DISK_GET_STORAGEID` (0x0007_1c24) is a two-call protocol.** It
fills a `STORAGE_IDENTIFICATION`: four DWORDs — size, flags, then *byte
offsets from the start of the header* to a manufacturer and a serial
string — followed by the strings. Two details are load-bearing
(`coredll.rs:1945`):

- The strings do not fit in the bare header, so the first call fails with
  the required size in `dwSize`; the caller reallocates and asks again.
  Truncating instead would hand back a shortened decimal serial, which
  parses as a different and wrong number.
- `dwFlags` bit 1 means "serial number invalid". Leave it clear.

Ball Busters reads the serial as
`strtoul((char *)id + id->dwSerialNumOffset, NULL, 10)`, so answering
with a zeroed buffer gave offset 0 and serial 0 — an empty slot, and the
game's "SD card removed" screen instead of its menu.

**`MAS1:` is the Gizmondo's MP3 decoder.** CE exposes the Micronas MAS
chip behind the console's audio as a stream device; a title plays music
by opening `MAS1:`, configuring it with a `DeviceIoControl`, and writing
MP3 frames to it. Nothing here decodes MP3, but the device still has to
open and accept both, because a missing one is not a case these games
handle: Ball Busters builds its music player by opening the device first
and the file second, and on failure leaves the player zeroed — then calls
it anyway on the next loading tick and dereferences a NULL stream.
Swallowing the frames is what gets the game past its loading screen.

Both pseudo-devices resolve *before* path resolution, since neither is a
file and `resolve` would otherwise fail them.

## 12. Audio — two transports

`AudioEngine` (`crates/pocket-kernel/src/audio.rs`) is fed by two
independent paths that mix together. Which one a game uses decides where
a silence bug lives.

**`waveOut` (`coredll`).** The guest decodes audio itself and hands over
finished PCM. `waveOutWrite` → `push_samples`. This is a *stream*: the
guest owns timing and back-pressure, so `CALLBACK_EVENT` must really
signal or the mixer thread spins (§10).

**HSS (`hss.dll`).** Hekkus Sound System is a freeware C++ mixer bundled
with a great many Pocket PC games. The guest hands over a *filename* and
expects the library to decode it, so `crates/pocket-winceapi/src/hss.rs`
owns a decoder for both formats HSS accepts: PCM `.wav` for effects and
Protracker modules for music. Games commonly rename modules to `.tkm`, so
`decode_clip` tries both decoders against the *content* — the extension
is not load-bearing on a device and it isn't here either.

HSS is C++ methods, so every handler takes `this` in `r0`. The guest-side
object is opaque: whatever the real `hss.dll` would have written there, we
never write. All state is host-side in `HssState`, keyed by that pointer.

Two mangled-name traps, both of which cost a whole debugging session on
JumpyBall:

* `load` is overloaded. `?load@hssSound@@QAAHPBG@Z` takes
  `const wchar_t*`; `?load@hssSound@@QAAHPAX_N@Z` takes `void*, bool`.
  Registering one is not registering the other.
* The volumes are setter/getter *pairs* that differ only in the mangled
  signature — `?volumeSounds@hssSpeaker@@QAAXI@Z` sets,
  `?volumeSounds@hssSpeaker@@QAAIXZ` gets. JumpyBall calls both halves
  and reads back what it wrote.

A stub that returns success for a name the game does import is worse than
no stub: the game proceeds as though it has audio and there is no warning
in the trace to find.

**The Protracker renderer** is `crates/pocket-kernel/src/tracker.rs` —
`Module::parse` then `Module::render(rate, max_seconds)`, no
guest-memory awareness, so it is testable on its own. Two things it must
get right that are easy to get wrong:

* The sample number is split across two nibbles — high in cell byte 0,
  low in the *high* nibble of byte 2. Getting this wrong selects an empty
  slot and renders silence.
* Source samples carry a large DC offset (means of +65 to +99 out of
  ±128 in JumpyBall's tracks). An Amiga's AC-coupled output discarded it;
  a modern DAC will not. The DC blocker on the mix bus is why music does
  not arrive as a thump — don't remove it.

Modules render once at load, not on the audio callback: measured at
0.04–0.05 s per 30 s of audio in release. `MODULE_SECONDS` is
deliberately generous so the renderer stops on the *order table* rather
than the clock — a song cut short puts the loop seam mid-phrase, which is
far more audible than the memory costs. Decodes are cached by path
behind an `Arc`, because a game re-loads the same track into a fresh
object on every level change.

**Voice groups.** `play_voice_with` takes `VoiceParams { looped, group,
volume }`. The groups exist so `stopMusics` can silence music without
cutting the sound effects that are still playing — JumpyBall calls it on
every level change. Voice mixing is `#[cfg(feature = "audio-cpal")]`;
without that feature `play_voice` degrades to `push_samples`.

**`--dump-audio-to` records at submission time**, inside
`add_voice`/`push_samples` — not as a real-time mixdown. A capture WAV
therefore shows what the guest *submitted*, in submission order, and its
header takes the format of the first clip seen. A game that starts four
tracks over an 8-frame run yields a capture of four full-length tracks
laid end to end — 499 seconds of WAV. This is the right tool for "did
any PCM reach the engine", and the wrong tool for "what would a user
hear".

**A capture proves submission, not audibility.** The two questions come
apart, and "the WAV looks fine but there is no sound" is the normal way
an audio bug presents. The host side is `run_audio_worker`, and *every*
one of its failure paths — no default device, `default_output_config`
failing, an unsupported sample format, `build_output_stream` failing,
`stream.play()` failing, the thread failing to spawn, or the
`audio-cpal` feature being off — leaves the run silent while the guest
carries on submitting samples and noticing nothing. They all log at
`warn` for that reason: a silent run is a user-visible failure, not a
detail. The one line that confirms real output is

```
AudioEngine: opened "<device>" at 44100 Hz / 2 ch (F32)
```

If that line is absent, the problem is the host device, not the decoder.

**The desktop GUI has no console on Windows.** `main.rs` is built with
`windows_subsystem = "windows"` so launching it does not flash up a
terminal, which also means stderr goes nowhere and log output reaches
nobody. It therefore tees `log` to `<library root>/pockethle-gui.log`,
truncated per launch. When diagnosing "no sound in the GUI but the CLI
is fine", read that file first — and prefer reproducing through the CLI,
which has a console and takes the same code path.

## 13. Working on this repo

```bash
cargo build --release -p pocket-cli --features unicorn   # → target/release/pockethle
cargo test --workspace
cargo clippy --workspace --all-targets

# CeGCC smoke test — expect zero "unimplemented call" warnings
./target/release/pockethle -v run hello.exe --cpu unicorn --max-slices 5000 \
  --key 3:enter --dump-frames-to /tmp/hello-frames --max-frames 6

# A GLES game, with the message cap lifted
./target/release/pockethle -v run /tmp/cod2-install/cod2_gles.exe \
  --rom-dir /tmp/cod2-install \
  --module-path '\Program Files\COD2\cod2_gles.exe' \
  --key 1:enter --key 2:enter --key 3:enter \
  --message-budget 0 \
  --dump-frames-to /tmp/cod2-unlimited --max-frames 40 \
  --max-slices 40000000
```

Diagnose in this order — cheapest first, and each step rules out the ones
below it:

1. Did it exit cleanly? Re-run with `--message-budget 0` (§8).
2. Any "unimplemented call" warnings? Those are missing handlers, or a
   missing ordinal-table entry if the names show as `ord NNN` (§5).
3. `frame_counter=0` with no errors? A presentation problem, not a CPU
   one (§6).
4. `frame_counter` huge but every captured frame identical? Something is
   bumping the counter without drawing. Raise `--dump-frame-stride` to
   confirm, then find the handler (§6).
5. Hangs with one thread doing all the work? A missing wake-up, not a
   slow CPU. Count "scheduling worker" lines in the trace (§10).
6. Does the game map both GAPI and GL? Check the `eglSwapBuffers` push.
7. Only then read guest disassembly.

Conventions:

* Match the surrounding comment density. Comments here explain *why a
  game needs this*, naming the title — that is what makes them worth
  keeping.
* Fix a shared wrong assumption at the boundary where the data enters, not
  in each consumer.
* Write the regression test so that reverting the fix fails it. Name it
  after the behaviour, not the function.
* Frames that prove a fix go in `proof/<game>/` with a short README. Note
  that COD2's framebuffer needs a 90° rotation to be read right-side up.
* No credentials, keys or tokens in anything written, logged or committed.
* Do not commit unless asked, and never push directly to the target
  branch — changes go through a Pull Request.



