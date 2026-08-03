# PocketHLE — Android frontend

The Android launcher imports Pocket PC `.cab` installers and standalone ARM
`.exe` files, stores them in its private library, renders the live framebuffer,
and forwards touch, keyboard, and D-pad input to the emulator. RAR containers
must be extracted first; import the CAB or EXE found inside them.

## Prerequisites

- Android Studio Iguana (AGP 8.4+) **or** standalone Gradle 8.x with
  `local.properties` pointing at an Android SDK.
- Android SDK Platform 34 and Android NDK r26+.
- For HTC Desire C, use the `armeabi-v7a` artifact; this phone is a 32-bit ARMv7 Android 4 device.
- `cargo install cargo-ndk` (the cross-compile helper).

## Building the native library

```bash
# From the repo root:
cargo ndk -t armeabi-v7a -o frontends/pocket-android/app/src/main/jniLibs \
    build --release -p pocket-android-jni
```

This drops `libpockethle_jni.so` under
`frontends/pocket-android/app/src/main/jniLibs/<abi>/`.

> **HTC Desire C profile.** The APK is intentionally built for `armeabi-v7a` only.
> The Android UI uses API-15-compatible framework calls, OpenGL ES 2.0, and the
> legacy `AudioTrack` constructor. The native build targets Android API 16
> because current NDK toolchains no longer ship an API-15 ARM sysroot; the
> Java package remains installable from API 15. The emulator's guest display
> stays at the game's configured 240x320/320x240 geometry and is pixel-perfect
> scaled to the phone's 320x480 panel.


## Building the APK

Inside `frontends/pocket-android`:

```bash
./gradlew assembleDebug
```

> **Note:** No Gradle wrapper jar is committed yet — run
> `gradle wrapper --gradle-version 8.7` once locally to generate it before
> the first build.
