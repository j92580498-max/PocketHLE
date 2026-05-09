//! Native ARM/VFP thunks for pure-arithmetic CRT helpers.
//!
//! Pocket PC binaries are built with the soft-float ABI, so any
//! floating-point op turns into a call to a `coredll.dll` helper
//! (`__adds`, `__divs`, `__stoi`, …). On real hardware those helpers
//! are tiny hand-written software-float routines; under HLE we used
//! to route every call through the Rust dispatcher (one
//! `emu_stop` → handler → `emu_start` per call), and a soft-float-
//! heavy game like JumpyBall ended up spending ~99 % of its CPU
//! time on dispatcher round-trips instead of actual gameplay.
//!
//! This module provides native ARM/VFP machine code that performs
//! the same operation in a single JIT-translated basic block. The
//! `pocket-cpu::UnicornCpu` boot path enables VFP for us
//! (FPEXC.EN=1, CPACR.CP10/11), so VFP instructions become a few
//! native SSE/AVX ops on the host. Hardware integer divide
//! (`SDIV`/`UDIV`) is supported on Unicorn's default ARMv7-A model.
//!
//! Each thunk is exactly [`THUNK_STRIDE`](super::THUNK_STRIDE) bytes
//! (32) — every slot in the synthetic IAT region is padded to that
//! stride. Unused slots are filled with `bx lr` so an accidental
//! fall-through returns harmlessly.
//!
//! ABI notes (matches the Microsoft / RVCT soft-float runtime):
//! * f32 ops take `a` in r0 (and `b` in r1 for binary), return in r0;
//! * f64 ops take `a` packed in (r0, r1) (lo word first) and `b` in
//!   (r2, r3); the result is returned in (r0, r1);
//! * `__rt_sdiv` / `__rt_udiv` take `divisor` in r0 and `dividend`
//!   in r1, returning the quotient in r0 and the remainder in r1.

/// `bx lr` (ARM, little endian). Used both as the trailing return
/// instruction of every thunk and as harmless padding for the
/// otherwise-unused tail of each 32-byte slot.
const BX_LR: u32 = 0xE12F_FF1E;

/// Build an 8-word thunk image, padding with `bx lr` after the
/// supplied instructions.
const fn pad(words: &[u32]) -> [u32; 8] {
    let mut out = [BX_LR; 8];
    let mut i = 0;
    while i < words.len() {
        out[i] = words[i];
        i += 1;
    }
    out
}

/// Look up the native thunk for a `coredll.dll` symbol. Returns
/// `None` if the symbol isn't one of the pure-arithmetic helpers we
/// can JIT directly — the kernel falls back to the regular hooked
/// dispatcher path for those.
pub fn native_thunk_for(dll: &str, name: &str) -> Option<[u32; 8]> {
    if !dll.eq_ignore_ascii_case("coredll.dll") {
        return None;
    }
    Some(match name {
        // ---- 32-bit hardware integer divide -----------------------------
        // sdiv ip, r1, r0 / mls r1, ip, r0, r1 / mov r0, ip / bx lr
        "__rt_sdiv" => pad(&[0xE71C_F011, 0xE061_109C, 0xE1A0_000C, BX_LR]),
        "__rt_udiv" => pad(&[0xE73C_F011, 0xE061_109C, 0xE1A0_000C, BX_LR]),

        // ---- f32 binary arithmetic --------------------------------------
        // vmov s0,r0 / vmov s1,r1 / vOP.f32 s0,s0,s1 / vmov r0,s0 / bx lr
        "__adds" => pad(&[0xEE00_0A10, 0xEE00_1A90, 0xEE30_0A20, 0xEE10_0A10, BX_LR]),
        "__subs" => pad(&[0xEE00_0A10, 0xEE00_1A90, 0xEE30_0A60, 0xEE10_0A10, BX_LR]),
        "__muls" => pad(&[0xEE00_0A10, 0xEE00_1A90, 0xEE20_0A20, 0xEE10_0A10, BX_LR]),
        "__divs" => pad(&[0xEE00_0A10, 0xEE00_1A90, 0xEE80_0A20, 0xEE10_0A10, BX_LR]),
        "__negs" => pad(&[0xEE00_0A10, 0xEEB1_0A40, 0xEE10_0A10, BX_LR]),

        // ---- f32 conversions --------------------------------------------
        "__stoi" => pad(&[0xEE00_0A10, 0xEEBD_0AC0, 0xEE10_0A10, BX_LR]),
        "__stou" => pad(&[0xEE00_0A10, 0xEEBC_0AC0, 0xEE10_0A10, BX_LR]),
        "__itos" => pad(&[0xEE00_0A10, 0xEEB8_0AC0, 0xEE10_0A10, BX_LR]),
        "__utos" => pad(&[0xEE00_0A10, 0xEEB8_0A40, 0xEE10_0A10, BX_LR]),

        // ---- f64 binary arithmetic --------------------------------------
        // vmov d0,r0,r1 / vmov d1,r2,r3 / vOP.f64 d0,d0,d1 / vmov r0,r1,d0 / bx lr
        "__addd" => pad(&[0xEC41_0B10, 0xEC43_2B11, 0xEE30_0B01, 0xEC51_0B10, BX_LR]),
        "__subd" => pad(&[0xEC41_0B10, 0xEC43_2B11, 0xEE30_0B41, 0xEC51_0B10, BX_LR]),
        "__muld" => pad(&[0xEC41_0B10, 0xEC43_2B11, 0xEE20_0B01, 0xEC51_0B10, BX_LR]),
        "__divd" => pad(&[0xEC41_0B10, 0xEC43_2B11, 0xEE80_0B01, 0xEC51_0B10, BX_LR]),
        "__negd" => pad(&[0xEC41_0B10, 0xEEB1_0B40, 0xEC51_0B10, BX_LR]),

        // ---- f32 ↔ f64 / int conversions --------------------------------
        "__stod" => pad(&[0xEE00_0A10, 0xEEB7_0AC0, 0xEC51_0B10, BX_LR]),
        "__dtos" => pad(&[0xEC41_0B10, 0xEEB7_0BC0, 0xEE10_0A10, BX_LR]),
        "__itod" => pad(&[0xEE00_0A10, 0xEEB8_0BC0, 0xEC51_0B10, BX_LR]),
        "__utod" => pad(&[0xEE00_0A10, 0xEEB8_0B40, 0xEC51_0B10, BX_LR]),
        "__dtoi" => pad(&[0xEC41_0B10, 0xEEBD_0BC0, 0xEE10_0A10, BX_LR]),
        "__dtou" => pad(&[0xEC41_0B10, 0xEEBC_0BC0, 0xEE10_0A10, BX_LR]),

        _ => return None,
    })
}

/// Convert an instruction array into a flat little-endian byte
/// buffer suitable for `cpu.write_mem`.
pub fn thunk_bytes(words: &[u32; 8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, w) in words.iter().enumerate() {
        let bytes = w.to_le_bytes();
        out[i * 4] = bytes[0];
        out[i * 4 + 1] = bytes[1];
        out[i * 4 + 2] = bytes[2];
        out[i * 4 + 3] = bytes[3];
    }
    out
}
