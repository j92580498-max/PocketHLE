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
   writes a single buffer.
7. **Worker threads never receive the window queue's synthetic traffic.**
   A worker running its own pump would dispatch forever and the thread
   that actually renders would never get the CPU back
   (`crates/pocket-winceapi/src/coredll.rs:6686` and `:6743`).
8. **`arch_all` is deliberately disabled on `unicorn-engine`.** Enabling
   it breaks the Android NDK cross-compile on QEMU's x86 `cpuid.h`.

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
`gapi`, `gdi`, `msgbox`, `native_thunks`, `registry`, `vfs`.

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

`frame_counter` is the liveness diagnostic. `frame_counter=0` after a run
that reported no errors means the guest executed fine and nothing reached
a presentation point — look at the two paths above, not at the CPU.

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

## 10. Working on this repo

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
4. Does the game map both GAPI and GL? Check the `eglSwapBuffers` push.
5. Only then read guest disassembly.

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
