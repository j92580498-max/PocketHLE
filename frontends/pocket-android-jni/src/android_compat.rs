//! Compatibility entry points for the 32-bit Android 4 linker.
//!
//! Recent Rust and NDK runtimes reference a handful of bionic symbols that
//! were only added to the 32-bit ABI in API 21. HTC Desire C ships API 15.
//! The implementations below use API-1 syscalls or older libc calls, so the
//! shared library can be loaded on both old and current Android releases.
//! This module is intentionally limited to the legacy ABI; current Android
//! builds keep the normal platform implementations.

#[cfg(all(target_os = "android", target_arch = "arm"))]
mod arm32 {
    use core::ffi::{c_char, c_int, c_long, c_void};

    #[repr(C)]
    pub struct Timespec {
        tv_sec: c_long,
        tv_nsec: c_long,
    }

    unsafe extern "C" {
        fn nanosleep(request: *const Timespec, remaining: *mut Timespec) -> c_int;
        fn syscall(number: c_long, _: ...) -> c_long;
    }

    const MAP_FAILED: isize = -1;
    const PAGE_SIZE: i64 = 4096;
    const __NR_MMAP2: c_long = 192;
    const __NR_OPENAT: c_long = 322;
    const __NR_NEWFSTATAT: c_long = 327;

    #[no_mangle]
    pub unsafe extern "C" fn clock_nanosleep(
        _clock_id: c_int,
        _flags: c_int,
        request: *const Timespec,
        remaining: *mut Timespec,
    ) -> c_int {
        nanosleep(request, remaining)
    }

    #[no_mangle]
    pub unsafe extern "C" fn mmap64(
        address: *mut c_void,
        length: usize,
        protection: c_int,
        flags: c_int,
        file: c_int,
        offset: i64,
    ) -> *mut c_void {
        if offset < 0 || offset % PAGE_SIZE != 0 {
            return MAP_FAILED as *mut c_void;
        }
        let page_offset = offset / PAGE_SIZE;
        syscall(
            __NR_MMAP2,
            address,
            length,
            protection,
            flags,
            file,
            page_offset,
        ) as *mut c_void
    }

    #[no_mangle]
    pub unsafe extern "C" fn openat(
        directory: c_int,
        path: *const c_char,
        flags: c_int,
        mode: c_int,
    ) -> c_int {
        syscall(__NR_OPENAT, directory, path, flags, mode) as c_int
    }

    #[no_mangle]
    pub unsafe extern "C" fn fstatat(
        directory: c_int,
        path: *const c_char,
        stat: *mut c_void,
        flags: c_int,
    ) -> c_int {
        syscall(__NR_NEWFSTATAT, directory, path, stat, flags) as c_int
    }

    #[no_mangle]
    pub unsafe extern "C" fn fdopendir(_file: c_int) -> *mut c_void {
        core::ptr::null_mut()
    }

    #[no_mangle]
    pub unsafe extern "C" fn dirfd(_directory: *mut c_void) -> c_int {
        -1
    }
}
