# PocketHLE — Android frontend

The Android launcher imports Pocket PC `.cab` installers, standalone ARM
`.exe` files, ZIP packages, RAR packages, and Windows self-extracting installers that contain
Pocket PC CABs. It stores games in its private library, renders the live
framebuffer, and forwards touch, keyboard, and D-pad input to the emulator.

## Prerequisites

- Android Studio Iguana (AGP 8.4+) **or** standalone Gradle 8.x with
  `local.properties` pointing at an Android SDK.
- Android NDK r25+.
- `cargo install cargo-ndk` (the cross-compile helper).

## Building the native library

```bash
# From the repo root:
cargo ndk -t arm64-v8a -t armeabi-v7a -o frontends/pocket-android/app/src/main/jniLibs \
    build --release -p pocket-android-jni
```

This drops `libpockethle_jni.so` under
`frontends/pocket-android/app/src/main/jniLibs/<abi>/`.

> **CPU backend on Android.** `pocket-android-jni` enables the real ARM
> `unicorn` backend by default. `--no-default-features` selects the trace-only
> stub and cannot run game code. The JNI build links the NDK's
> `libclang_rt.builtins-<arch>-android.a` to resolve `__clear_cache`; it checks
> both `lib/clang` and `lib64/clang` layouts.

## Building the APK

Inside `frontends/pocket-android`:

```bash
./gradlew assembleDebug
```

> **Note:** No Gradle wrapper jar is committed yet — run
> `gradle wrapper --gradle-version 8.7` once locally to generate it before
> the first build.
