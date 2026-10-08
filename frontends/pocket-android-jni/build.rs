// Build glue for `pocket-android-jni` cross-compiles.
//
// Two Android-specific link fixups happen here:
//
// 1. **`-lpthread` stub.** `unicorn-engine-sys`'s `build.rs` unconditionally
//    emits `cargo:rustc-link-lib=pthread` for any non-MSVC target. Android's
//    bionic libc provides the pthread API directly — there is no separate
//    `libpthread.so` shipped with the NDK — so the linker would fail with
//    `unable to find library -lpthread`. To satisfy the `-lpthread` flag
//    without touching the upstream crate, drop an empty `libpthread.a`
//    (8-byte `ar` magic only) next to a `cargo:rustc-link-search` path.
//    `ld.lld` accepts that as a valid archive contributing zero symbols and
//    the actual `pthread_*` symbols are resolved against `libc.so` at load.
//
// 2. **`libclang_rt.builtins-<arch>-android.a` linkage.** AArch64/ARM TCG in
//    the QEMU vendored by `unicorn-engine-sys` calls `__builtin___clear_cache`
//    after generating each translation block. Clang lowers that builtin to a
//    libcall to `__clear_cache`, which is *not* exported by bionic libc — it
//    lives in `libclang_rt.builtins-<arch>-android.a` (NDK's compiler-rt).
//    Rust's cross link does not pull `--rtlib=compiler-rt` automatically when
//    cargo-ndk drives the link, so the resulting `libpockethle_jni.so` ends
//    up with `__clear_cache` as an *undefined dynamic symbol*. With BIND_NOW
//    on (Android default), `dlopen()` then fails on the user's device with
//    `cannot locate symbol "__clear_cache"`, `System.loadLibrary` throws
//    `UnsatisfiedLinkError`, and the app crashes the moment it starts —
//    which is exactly the failure mode we hit. The fix below appends the
//    matching `libclang_rt.builtins` archive to the link so `__clear_cache`
//    (and any other builtin libcall) is satisfied statically.
//
// `libm.a` is shipped by NDK r26 so it does not need a stub.
//
// On non-Android targets this build script is a no-op.

use std::env;
use std::fs;
use std::path::PathBuf;

#[path = "src/android_link.rs"]
mod android_link;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/android_link.rs");
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_ROOT");

    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("-linux-android") {
        return;
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let stub_dir = out_dir.join("android-link-stubs");
    fs::create_dir_all(&stub_dir).expect("create android-link-stubs dir");

    // (1) Empty `libpthread.a` so the upstream `-lpthread` is satisfied;
    // pthread symbols themselves resolve against bionic at load time.
    let empty_archive: &[u8] = b"!<arch>\n";
    let pthread_path = stub_dir.join("libpthread.a");
    fs::write(&pthread_path, empty_archive).expect("write libpthread.a stub");
    println!("cargo:rustc-link-search=native={}", stub_dir.display());

    // (2) Link `libclang_rt.builtins-<arch>-android.a` so `__clear_cache`
    // (called from QEMU's TCG cache-flush after codegen) and other compiler
    // builtins are resolved statically. Without this the .so ships with
    // `__clear_cache` as an undefined dynamic symbol that bionic does not
    // export, and the app crashes on `System.loadLibrary`.
    if let Some(builtins) = android_link::find_clang_rt_builtins(&target) {
        // Pass the archive directly to the link command. Using a full path
        // (instead of `-l<name>` plus `-L<dir>`) avoids any clash with
        // search-order quirks in `cargo-ndk`'s linker wrapper.
        println!("cargo:rustc-link-arg={}", builtins.display());
    } else {
        println!(
            "cargo:warning=pocket-android-jni: could not locate \
             libclang_rt.builtins for target {target}; \
             set ANDROID_NDK_HOME (or ANDROID_NDK_ROOT) to your NDK root. \
             Android JIT (unicorn) will fail to dlopen without this archive."
        );
    }
}
