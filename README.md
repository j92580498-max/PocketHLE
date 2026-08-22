<!--
Copyright (C) 2026 PocketHLE Emulator Project
SPDX-License-Identifier: Apache-2.0 OR MIT
-->

# PocketHLE

<p align="center">
  <img src="./frontends/pocket-desktop/assets/pockethle-logo.png" width="25%" alt="PocketHLE logo" />
</p>

<p align="center">
  A high-level Windows Mobile / Pocket PC / Gizmondo emulator for Linux, Windows and Android.
</p>

<p align="center">
  <a href="https://discord.gg/x5uE873e9">
    <img src="https://img.shields.io/badge/Discord-Join%20our%20Community-5865F2?style=for-the-badge&logo=discord&logoColor=white" alt="Join the PocketHLE Discord community" />
  </a>
</p>

<p align="center">
  <strong>Join our Discord for development updates, compatibility discussions, support and community chat.</strong>
</p>

[Русская версия → `README.ru.md`](README.ru.md)

---

> [!NOTE]
> PocketHLE targets native ARM Windows CE / Windows Mobile executables packaged in `.CAB` archives. It is experimental software: compatibility, performance and input behavior vary by game and host platform.

> [!WARNING]
> PocketHLE is developed for research and educational purposes. It does not include Microsoft system files, firmware or game data. Use only legally obtained game copies and archives.

## Info

PocketHLE is an early-stage high-level emulator for Pocket PC 2002/2003 and Windows Mobile 5/6 games. Instead of emulating a complete Windows CE device, it loads the original game executable, runs its ARM code through a CPU backend, and provides clean-room host-side implementations of the Windows CE APIs the game expects.

The project is inspired by [touchHLE](https://github.com/touchHLE/touchHLE) and [EKA2L1](https://github.com/EKA2L1/EKA2L1), with a launcher and library workflow influenced by [j2me-loader](https://github.com/nikita36078/j2me-loader).

## Games Tested

| Asphalt 4 Elite Racing | Call of Duty 2 |
| :---: | :---: |
| ![Asphalt 4 gameplay](./proof/games/asphalt-4-elite-racing.jpg) | ![Call of Duty 2](./proof/games/call-of-duty-2.jpg) |

| Zuma Deluxe | Bejeweled |
| :---: | :---: |
| ![Zuma Deluxe](./proof/games/zuma-deluxe.jpg) | ![Bejeweled](./proof/games/bejeweled.jpg) |

> [!IMPORTANT]
> The screenshots above are representative game references supplied for the project presentation. The checked-in, reproducible emulator proof currently covers Asphalt 4 WVGA and Call of Duty 2's title-screen boot through the OpenGL ES layer. Zuma Deluxe and Bejeweled have game-specific compatibility work in the codebase, but should not be described as fully verified here without a corresponding checked-in run capture.

Additional compatibility probes and rendering captures are available under [`proof/`](proof/), including Asphalt 2 3D, Crazy Taxi, Diamond Twister, Splinter Cell Conviction and other Windows Mobile titles.

## Status

PocketHLE can currently:

* import and unpack Windows Mobile `.CAB` archives;
* restore long filenames and installation paths from `_setup.xml`, or
  from the binary `MSCE` install header older cabinets use instead;
* select the intended game executable instead of helper binaries;
* load native ARM and MIPS PE images and inspect their imports;
* execute ARM code with the Unicorn Engine backend;
* intercept imported Windows CE DLL calls through HLE thunks;
* provide ordinal-to-name resolution for `coredll.dll` and `aygshell.dll`;
* emulate core windowing, GDI, GAPI, registry, filesystem, timing, input and audio paths;
* render software framebuffers in the desktop frontend and Android `SurfaceView` / OpenGL frontend;
* run a shared game library with per-game settings on desktop and Android;
* build Linux, Windows and Android artifacts through GitHub Actions.

The current reference proof demonstrates Asphalt 4 rendering at WVGA with captured PCM audio. Android also has a game launcher, fullscreen/orientation controls, display modes, per-game settings and a turbo control for titles that need accelerated startup.

PocketHLE is not a full Windows CE emulator. Some games still stop during CRT initialization, dynamic imports, worker-thread setup or unimplemented APIs. A successful boot or first frame does not automatically mean that an entire game is playable from start to finish.

## Architecture

```
+----------------+    +----------------+    +----------------+
| pocket-cli     |    | pocket-desktop |    | pocket-android |
| CLI / captures |    | egui launcher  |    | Kotlin + JNI   |
+--------+-------+    +--------+-------+    +--------+-------+
         \                     |                     /
          \                    |                    /
           +-------------------+--------------------+
                               |
                      +--------v--------+
                      |   pocket-core   |
                      | Emulator / VFS  |
                      +--------+--------+
                               |
          +--------------------+--------------------+
          |                    |                    |
  +-------v-------+    +-------v--------+   +-------v--------+
  | pocket-cab    |    | pocket-pe      |   | pocket-library |
  | CAB installer |    | ARM/MIPS PE    |   | game database  |
  +---------------+    +----------------+   +----------------+
                               |
                      +--------v--------+
                      | pocket-kernel   |
                      | process / GDI   |
                      | HLE dispatch    |
                      +--------+--------+
                               |
                  +------------+------------+
                  |                         |
          +-------v-------+         +-------v--------+
          | pocket-cpu    |         | pocket-winceapi |
          | stub/Unicorn  |         | coredll/gx/hss  |
          +---------------+         +----------------+
```

When a game starts:

1. `pocket-cab` extracts the archive and reconstructs its installed files and registry values.
2. `pocket-pe` parses the PE image, sections and imported symbols.
3. `pocket-kernel` maps the image, patches the import address table with HLE thunks and creates the guest process state.
4. `pocket-cpu` executes the guest ARM or MIPS code.
5. `pocket-winceapi` dispatches imported Windows CE calls to Rust handlers for graphics, files, windows, input, registry, timing and audio.
6. The selected frontend displays the framebuffer and forwards host input back to the guest.

## Using

Download a release archive for your platform, import a legally obtained Pocket PC `.CAB`, and choose the game in the desktop or Android launcher.

For command-line runs:

```bash
# Inspect a cabinet
pockethle inspect-cab ~/Games/Asphalt4.cab

# Run a cabinet; the launcher extracts and mounts its install directory
pockethle run ~/Games/Asphalt4.cab

# Run with the real ARM CPU backend and capture frames
pockethle run ~/Games/Asphalt4.cab \
  --cpu unicorn \
  --dump-frames-to /tmp/pockethle-frames \
  --max-frames 8

# Inspect or run an extracted executable
pockethle pe-info /tmp/pockethle/Asphalt4.exe
pockethle run /tmp/pockethle/Asphalt4.exe --cpu unicorn
```

The CLI also supports `.exe` and `.zip` inputs. Use `--cpu stub` for trace-only and loader tests; it does not execute real game instructions.

## Build

### Linux

```bash
# Rust 1.85+ is recommended
rustup default stable

# Native dependencies for the desktop frontend
sudo apt install -y cmake build-essential pkg-config libclang-dev \
  libgtk-3-dev libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev \
  libxrandr-dev libxinerama-dev libxi-dev libxcursor-dev \
  libxdamage-dev libxext-dev libxfixes-dev

# Build and test the workspace
cargo build --release --workspace
cargo test --workspace

# Build the real CPU backend
cargo build --release -p pocket-cli --features unicorn
cargo build --release -p pocket-desktop --features unicorn
```

The binaries are written to `target/release/`:

* `pockethle` — command-line frontend;
* `pockethle-gui` — desktop launcher.

### Windows

Install Rust with the MSVC toolchain and CMake, then run:

```powershell
cargo build --release --workspace
cargo build --release -p pocket-cli --features unicorn
cargo build --release -p pocket-desktop --features unicorn
```

The resulting binaries are `target\release\pockethle.exe` and `target\release\pockethle-gui.exe`.

### Android

The Android frontend lives in [`frontends/pocket-android`](frontends/pocket-android). It requires Android Studio Iguana or newer, Android NDK r26 or newer and [`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk).

```bash
cargo ndk \
  -t arm64-v8a \
  -t armeabi-v7a \
  -o frontends/pocket-android/app/src/main/jniLibs \
  build --release -p pocket-android-jni

cd frontends/pocket-android
./gradlew assembleRelease
```

The APK is created under `frontends/pocket-android/app/build/outputs/apk/release/`.

The Android launcher imports `.CAB` files through the system picker, stores the game library in app storage, offers global and per-game settings, and opens each title in a `GameActivity` backed by the native emulator and a framebuffer renderer.

## Library

Desktop and Android share the `pocket-library` model:

```
<library-root>/
├── library.json
├── config.json
└── games/
    └── <sanitized-id>/
        ├── game.json
        ├── source.cab
        └── extracted/
            └── ... game files ...
```

On Linux and Windows, the default location is `~/.local/share/PocketHLE/library` or the platform equivalent. Android stores it under the app's external files directory.

## Roadmap

* complete the remaining CRT and legacy ARM startup paths;
* expand dynamic import and thread/runtime compatibility;
* improve resource, GDI and dialog coverage across more games;
* add broader gamepad, keyboard and touch mapping;
* continue improving audio mixing and host output;
* add reproducible compatibility captures for more titles;
* improve desktop and Android UX and diagnostics.

## Legal notice

PocketHLE does not contain or distribute copyrighted Microsoft system DLLs, firmware or game assets. The Windows CE API layer is a clean-room reimplementation based on public API behavior and ordinal data. Users are responsible for the games and archives they provide.

## License

PocketHLE is dual-licensed under [Apache-2.0](LICENSE-APACHE) **OR** [MIT](LICENSE-MIT), at your option.

## Contributing

Bug reports, compatibility results, API traces and pull requests are welcome. Please include the game version, target screen size, frontend, CPU backend, command line and relevant logs or frame captures when reporting a problem.

Join the [PocketHLE Discord community](https://discord.gg/pSjD428p2) for development updates, compatibility discussions and support.

## Special Thanks

* [touchHLE](https://github.com/touchHLE/touchHLE) for the high-level emulation model and project inspiration;
* [EKA2L1](https://github.com/EKA2L1/EKA2L1) for another practical HLE-oriented emulator architecture;
* [j2me-loader](https://github.com/nikita36078/j2me-loader) for launcher and library UX inspiration;
* the PocketHLE contributors and testers who provide legally obtained software, traces and compatibility reports.
