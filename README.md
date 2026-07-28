# PocketHLE

> High-level emulator for Windows Mobile / Pocket PC games.
> Inspired by [touchHLE](https://github.com/touchHLE/touchHLE) (iPhone OS) and
> [EKA2L1](https://github.com/EKA2L1/EKA2L1) (Symbian).

PocketHLE focuses on running individual Pocket PC 2002 / 2003 / Windows Mobile
5/6 games without emulating an entire Windows CE kernel. Like touchHLE, it
loads a real game executable, runs the ARM code in a CPU emulator, and
implements the system DLLs (`coredll`, `aygshell`, `gx`, `hss`, ...) on the
host side so the game thinks it's running on a real Pocket PC.

The first target ROM is a small physics game called **JumpyBall** (provided
by the user, ARM PE32, Windows CE 5 GUI). All work is driven from the public
APIs that this game uses.

> **Status:** early but visibly working — the ARM CPU runs, the loader
> works, system-DLL imports are intercepted, a software-rasterised
> framebuffer is wired through GDI/GAPI, and JumpyBall reaches its
> main menu and credits screen out of the box (`pockethle run
> JumpyBallPPC.cab`). See [Roadmap](#roadmap) for what is still
> stubbed.

[Russian (Русский) version → `README.ru.md`](README.ru.md)

---

## Table of contents

- [Features](#features)
- [Architecture](#architecture)
- [Building on Linux](#building-on-linux)
- [Building on Windows](#building-on-windows)
- [Building for Android](#building-for-android)
- [Trying it on the JumpyBall test ROM](#trying-it-on-the-jumpyball-test-rom)
- [Library layout](#library-layout)
- [Roadmap](#roadmap)
- [Project layout](#project-layout)
- [Comparison to other emulators](#comparison-to-other-emulators)
- [License](#license)

## Features

- **CAB extractor** — unpacks Windows Mobile `.CAB` archives. Auto-detects the
  largest ARM `.exe` inside.
- **PE32 loader** — supports the WinCE flavour of `IMAGE_FILE_MACHINE_ARM`
  (`0x01c0`), parses imports by ordinal *and* name, lays out sections in the
  emulated address space.
- **Pluggable CPU backend** — a tiny `Cpu` trait with two implementations:
  - `stub` — software-only, no real instruction decoding, used for tests.
  - `unicorn` — wraps [Unicorn Engine 2](https://www.unicorn-engine.org/);
    runs real ARM machine code at native-ish speed.
- **HLE thunk dispatcher** — every imported symbol is rewritten in the IAT
  to point at a synthetic 4-byte page. The CPU gets a code hook on each
  thunk; on hit, the host's [`WinCeDispatcher`] looks up a Rust handler and
  resumes execution at `LR`.
- **Ordinal → name resolver** — partial maps for `coredll.dll` and
  `aygshell.dll` shipped as JSON data files. The resolver kicks in
  automatically so logs say `PeekMessageW` instead of `ord 266`.
- **Linux CLI frontend** with `pe-info`, `unpack-cab`, `inspect-cab` and
  `run` subcommands.
- **Cross-platform desktop GUI** — `pocket-desktop` (egui) for Linux & Windows.
  Library / import / settings screens inspired by
  [j2me-loader](https://github.com/nikita36078/j2me-loader).
- **Android launcher** — Gradle project with a j2me-loader-style RecyclerView,
  per-game settings, FAB import, and a `SurfaceView`-based game screen.
- **Cross-platform CI** — GitHub Actions builds release artifacts for Linux
  (`tar.gz`), Windows (`zip`) and Android (`apk`) on every push, the way
  [touchHLE](https://github.com/touchHLE/touchHLE) does.

## Architecture

```
+-----------+    +-----------+    +---------+    +----------+
| pocket-cli|    |pocket-     |    | pocket- |    |pocket-   |
| (CLI)     |    | android    |    | core    |--->| pocket-pe|
+-----+-----+    | (Gradle)   |    +----+----+    +----------+
      |          +-----+------+         |
      v                |                v
+-----+----------------+----------------+----+
|             pocket-kernel                  |   process model, IAT thunks
|  +--------------+   +--------------------+ |
|  |pocket-cpu    |   |pocket-winceapi     | |   coredll / aygshell /
|  | (Unicorn)    |   |  (HLE handlers)    | |   gx / hss handlers
|  +--------------+   +--------------------+ |
+--------------------------------------------+
```

When `Emulator::run()` is invoked:

1. `pocket-cab` unpacks the user's `.CAB` and identifies the game `.exe`.
2. `pocket-pe` parses the PE, builds a list of `LoadedSection`s and the
   `Vec<ImportSymbol>` describing every imported function.
3. `pocket-kernel::Process::map_into` creates the address space:
   - maps each section,
   - allocates a `THUNK_REGION_BASE` pool of 4-byte stubs (`bx lr`),
   - patches the IAT so every imported symbol points at one of those
     stubs,
   - registers a code hook on every stub.
4. `pocket-kernel::run_main_loop` enters Unicorn at the entry point. As soon
   as the guest jumps through any IAT entry, Unicorn returns control;
   `WinCeDispatcher::dispatch` looks up the symbol and invokes the matching
   Rust handler, then resumes the guest at `LR`.

This is exactly the design touchHLE uses for Objective-C runtime calls and
Foundation, just adapted to Win32-style import tables.

## Building on Linux

```bash
# 1. Toolchain
rustup default stable               # 1.85+ recommended

# 2. Native deps (Ubuntu / Debian)
sudo apt install -y cmake build-essential pkg-config libclang-dev \
                    libgtk-3-dev libxkbcommon-dev \
                    libwayland-dev libx11-dev libxcb1-dev \
                    libxrandr-dev libxinerama-dev libxi-dev \
                    libxcursor-dev libxdamage-dev libxext-dev libxfixes-dev

# 3. Build everything (stub CPU only — fast, ~30 s)
cargo build --release --workspace

# 4. Build the CLI with the real ARM CPU backend (~3 minutes first time;
#    Unicorn Engine is built from source).
cargo build --release -p pocket-cli      --features unicorn
cargo build --release -p pocket-desktop  --features unicorn

# 5. Run tests
cargo test --workspace
```

The resulting binaries live at:

- `target/release/pockethle`     — CLI (`pe-info`, `unpack-cab`, `inspect-cab`, `run`, ...)
- `target/release/pockethle-gui` — desktop GUI (`pocket-desktop`)

## Building on Windows

PocketHLE builds out of the box on Windows with the MSVC toolchain (the same
way touchHLE distributes its Windows build).

```powershell
# 1. Install rustup, then:
rustup default stable-x86_64-pc-windows-msvc

# 2. Build CLI + desktop GUI (stub CPU — fast)
cargo build --release -p pocket-cli
cargo build --release -p pocket-desktop

# 3. (Optional) Real ARM CPU via Unicorn Engine — needs cmake on PATH
#    and a working MSVC C/C++ toolchain. Building Unicorn from source the
#    first time takes a few minutes.
cargo build --release -p pocket-cli      --features unicorn
cargo build --release -p pocket-desktop  --features unicorn
```

The resulting binaries are `target\release\pockethle.exe` and
`target\release\pockethle-gui.exe`.

Double-clicking `pockethle-gui.exe` opens a small launcher window: import a
`.CAB`, pick a game from the library, hit Run.

## Trying it on the JumpyBall test ROM

The `run` subcommand accepts a Pocket PC `.exe`, a `.cab`, or a
`.zip` directly — archives are auto-extracted into a temp dir and the
largest ARM PE inside is launched. The default build of `pockethle`
includes both the `unicorn` CPU and the `display` host-window
features so a freshly-checked-out repo can render a game without
extra flags.

```bash
# (a) Run from the original Microsoft cabinet — auto-extracts and
#     auto-mounts the cabinet contents at the guest's `\Application\`
#     so CreateFileW finds the bundled resources.
pockethle run ~/JumpyBallPPC.cab

# (b) Same, but pop a host window with the live framebuffer.
pockethle run ~/JumpyBallPPC.cab --display

# (c) Headless capture: dump every rendered frame as PPM and stop
#     after eight frames — useful for CI / regression diffs.
pockethle run ~/JumpyBallPPC.cab \
    --dump-frames-to /tmp/jumpy_frames --max-frames 8

# (d) The classic flow is still supported if you want to inspect
#     things by hand:
pockethle inspect-cab ~/JumpyBallPPC.cab
pockethle unpack-cab  ~/JumpyBallPPC.cab /tmp/jumpy
pockethle pe-info     /tmp/jumpy/JUMPYB~1.002
pockethle run         /tmp/jumpy/JUMPYB~1.002

# (e) For trace-only analysis (no real CPU), pass `--cpu stub`. This
#     does not require the Unicorn build.
pockethle run ~/JumpyBallPPC.cab --cpu stub --max-slices 1
```

Real PPC2003 games typically need a few hundred thousand emulated
slices to finish their CRT init, build their soft-float lookup
tables and load bitmap resources before the first `WM_PAINT` is
delivered. `pockethle run` therefore defaults to `--max-slices
2_000_000`; pass a smaller value for fast smoke tests, or `0` for no
upper bound.

The first time you run an unfamiliar binary, expect to see lines
like:

```
[INFO  pocket_kernel] entering emulated main: entry=0x000247c8, stack_top=0x60000000
[WARN  pocket_winceapi] unimplemented call -> COREDLL.dll!CreateDirectoryW
...
```

Each `unimplemented call` line is a clue: that API needs a real handler in
`crates/pocket-winceapi/src/coredll.rs` (or wherever appropriate) before the
game can progress. See [Roadmap](#roadmap).

## Building for Android

The Android frontend lives in [`frontends/pocket-android`](frontends/pocket-android).
It depends on:

- Android Studio Iguana (or any AGP 8.4+ install)
- Android NDK r26 or newer
- [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk) (`cargo install cargo-ndk`)

Build:

```bash
# 1. Cross-compile the JNI bridge for the two Android ABIs we ship.
cargo ndk \
    -t arm64-v8a \
    -t armeabi-v7a \
    -o frontends/pocket-android/app/src/main/jniLibs \
    build --release -p pocket-android-jni

# 2. Build the APK (uses the Gradle wrapper).
cd frontends/pocket-android
./gradlew assembleRelease
```

The APK lands in
`frontends/pocket-android/app/build/outputs/apk/release/`.

The Android UI is modelled on
[j2me-loader](https://github.com/nikita36078/j2me-loader): a RecyclerView
launcher with per-game cards (Run / Settings / Remove), a FAB to import
new `.CAB` files via the system file picker, a global settings screen
(default CPU backend, log verbosity), and a per-game settings screen
(CPU backend, dispatch slice budget, halt-on-unimplemented). Running a
game opens a `SurfaceView`-backed `GameActivity` that displays the
emulator's framebuffer.

## Library layout

The desktop GUI and the Android launcher share an on-disk library managed
by the [`pocket-library`](crates/pocket-library) crate. It looks like
this:

```
<library-root>/
├── library.json          # index of imported games
├── config.json           # default CPU backend, log verbosity, ...
└── games/
    └── <sanitized-id>/
        ├── game.json     # display name, source CAB, per-game settings
        ├── source.cab    # original archive (kept for re-extraction)
        └── extracted/
            └── ... PE / data files ...
```

On Linux/Windows the default root is
`~/.local/share/PocketHLE/library` (or platform equivalent via
[`directories`](https://docs.rs/directories)).
On Android it lives under the app's external files dir,
`getExternalFilesDir(null)/library`.

## Roadmap

The order below roughly matches what JumpyBall calls into during its first
hundred milliseconds:

1. **CRT prologue** — `__chkstk`, `_setjmp`, `longjmp`, `_except_handler3`.
2. **Window setup** — `RegisterClassW`, `CreateWindowExW`, `ShowWindow`,
   `SHFullScreen`.
3. **Resource loading** — `FindResourceW`, `LoadResource`, `LockResource`,
   `CreateFileW`, `ReadFile`.
4. **GDI** — `BeginPaint`, `EndPaint`, `BitBlt`, `Rectangle`, `FillRect`,
   `CreateCompatibleDC/Bitmap`, `SelectObject`, `DeleteObject`. We will
   implement this as a software-rasterised framebuffer that gets blitted
   straight to the host's window.
5. **GAPI** — already stubbed in `pocket-winceapi/src/gx.rs`. Hook up the
   `GXBeginDraw` framebuffer to a host-side window (SDL2 on Linux,
   `SurfaceView` on Android).
6. **Audio** — replace the HSS stubs with real SDL2 / OpenSL ES playback.
7. **Input** — translate host keyboard / touch events to Pocket PC
   `WM_KEYDOWN` / `WM_LBUTTONDOWN` and pump them into the message queue.

Each milestone is intended to land as a separate small PR.

## Project layout

```
crates/
  pocket-cab/        Microsoft Cabinet extractor
  pocket-pe/         WinCE PE32 loader (ARM / x86 / Thumb)
  pocket-cpu/        Cpu trait + stub + unicorn backends
  pocket-kernel/     Address space, IAT thunks, dispatcher loop
  pocket-winceapi/   coredll / aygshell / gx (GAPI) / hss handlers
  pocket-core/       Top-level Emulator that frontends drive
  pocket-library/    On-disk game library + per-game config (shared by GUIs)
frontends/
  pocket-cli/        Cross-platform command-line tool (`pockethle`)
  pocket-desktop/    Cross-platform egui GUI for Linux & Windows (`pockethle-gui`)
  pocket-android-jni/Rust JNI bridge consumed by the Android app
  pocket-android/    Gradle project (Kotlin) — j2me-loader-style launcher
data/
  ordinals/          JSON ordinal -> name maps for coredll, aygshell
docs/
  architecture/      Design notes
  api-stubs/         Per-API specification of what stubs need to do
```

## Comparison to other emulators

| Project              | Target OS                       | Approach                  | Status                                  |
|----------------------|---------------------------------|---------------------------|-----------------------------------------|
| `touchHLE`           | iPhone OS 2.x / 3.x             | High-level (HLE)          | Plays a handful of OpenGL ES games      |
| `EKA2L1`             | Symbian S60v3 / S60v5           | Mostly HLE, partial LLE   | Plays many Symbian games                |
| Microsoft Device Emu | Windows CE 5 / 6                | Full LLE (closed source)  | Discontinued                            |
| **PocketHLE** (this) | Windows Mobile 5 / 6 / Pocket PC| HLE                       | Loads + boots CRT for JumpyBall         |

PocketHLE deliberately copies touchHLE's layout — small Rust workspace,
HLE-first — because it scales well: every new game adds a few more API
implementations rather than years of low-level reverse engineering.

## Managed .NET Compact Framework executables

PocketHLE's execution backend currently supports native ARM WinCE PE images. It does not execute managed `.NET Compact Framework` assemblies: those require the CLR/Compact Framework runtime and WinForms/GDI+ managed surface, not only native WinCE API stubs.

The loader detects the CLR metadata and reports the runtime version instead of entering the managed PE as if it were native ARM code. For example, `PocketSnake.exe` from the supplied installer is an x86 managed assembly with CLR metadata `v1.1.4322`; it is not a native ARM WinCE executable. It must run on Windows Mobile 2003 or a later compatible system with .NET Compact Framework 1.1 installed, or on an earlier compatible Windows CE system where that runtime is already installed.

Open-source reference projects for analogous Compact Framework structure include [Pocket1945](https://github.com/timdetering/Pocket1945), [Pocket-Minesweeper](https://github.com/Enovale/Pocket-Minesweeper), and [SokobanCompact](https://github.com/OverQuantum/SokobanCompact). They are useful references for managed startup, resource loading, timer-driven updates, and WinForms painting; they are not native ARM implementations that can be loaded directly by PocketHLE.

## License

Dual licensed under [Apache-2.0](LICENSE-APACHE) **OR** [MIT](LICENSE-MIT) at
your option.

PocketHLE itself does not contain or distribute any copyrighted Microsoft
code, Pocket PC system DLL, or game asset. Users supply their own legally
obtained `.CAB` files. The system-DLL stubs are clean-room reimplementations
based on publicly documented APIs and ordinal numbers.
