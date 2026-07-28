//! Native ARM/VFP thunks for pure-arithmetic and pure-memory CRT
//! helpers.
//!
//! Pocket PC binaries are built with the soft-float ABI, so any
//! floating-point op turns into a call to a `coredll.dll` helper
//! (`__adds`, `__divs`, `__stoi`, …). Likewise the C runtime ships
//! `memcpy` / `memset` / `strlen` / … as separate exports rather
//! than inlining them. On real hardware those helpers are tiny
//! hand-written routines; under HLE we used to route every call
//! through the Rust dispatcher (one `emu_stop` → handler →
//! `emu_start` per call). For a game like Derby that calls
//! `memcpy` ~1 100 times per second that overhead alone is a
//! double-digit percentage of the wall-clock budget.
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
//! ABI notes (matches the Microsoft / RVCT soft-float runtime and
//! AAPCS for everything else):
//! * f32 ops take `a` in r0 (and `b` in r1 for binary), return in r0;
//! * f64 ops take `a` packed in (r0, r1) (lo word first) and `b` in
//!   (r2, r3); the result is returned in (r0, r1);
//! * `__rt_sdiv` / `__rt_udiv` take `divisor` in r0 and `dividend`
//!   in r1, returning the quotient in r0 and the remainder in r1;
//! * `memcpy` / `memset` / `strcpy` / `wcscpy` return their
//!   destination pointer (r0 unchanged across the call);
//! * `strlen` / `wcslen` / `lstrlen*` return the length in r0;
//! * `strcmp` / `lstrcmpA` / `wcscmp` / `lstrcmpW` return signed
//!   `(*a) - (*b)` at the first difference;
//! * `GetTickCount` returns the current tick (ms since the first
//!   call within the host process) by reading the host-managed
//!   cell at [`TICK_PAGE_VA`]; the run loop refreshes that cell
//!   between every emulator slice via
//!   [`refresh_tick_page`].
//!
//! Note that `lstrlenA` / `lstrcpyA` etc. are byte-exact aliases of
//! their `str*` counterparts on Pocket PC — they're entry points
//! into the same code in `coredll.dll` — so we share the assembled
//! image rather than copy-pasting it.

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

/// Base address of the host-managed "tick page" — a single 4 KiB
/// guest-readable page whose first u32 always holds the current
/// `GetTickCount` result. Updated by the run loop's frame hook so
/// guest code can read it with a plain `LDR`. Chosen just below
/// [`super::THUNK_REGION_BASE`] so it sits inside the same logical
/// "synthetic kernel" address neighbourhood.
pub const TICK_PAGE_VA: u32 = 0x6FFF_F000;
/// Offset within [`TICK_PAGE_VA`] of the live tick value (u32 LE).
pub const TICK_VALUE_OFFSET: u32 = 0;

// `memcpy` / `memset` are intentionally NOT hand-rolled here. We
// tried it (a 32-byte byte-by-byte loop using `ldrb`/`strb`) and
// the JIT'd loop ran *slower* than the dispatcher path on Derby,
// because the dispatcher has access to host-vectorised
// `memcpy` / `Vec::resize_with` and only pays an FFI round-trip
// once per call — for several-hundred-byte BitBlt scanline copies
// the host SIMD memcpy beats any 4-instr-per-byte ARM loop we can
// fit in the 32-byte slot. Leaving these on the dispatcher path.

/// Build a thunk that does `mov r0, #imm; bx lr`. Used to patch
/// IAT entries whose dispatcher handler is a pure constant-return
/// stub (`zero_returning` / `one_returning`). The JIT then executes
/// these calls inline with zero callback overhead — same shape as
/// the arithmetic CRT thunks above, just simpler.
///
/// Supports any unsigned 8-bit immediate (0..=255). For ARM, the
/// encoding `MOV r0, #imm` packs the rotated-immediate in bits 0..11
/// of `0xE3A0_0000`; with no rotation we can fit any 8-bit value
/// directly.
const fn const_return_thunk(imm: u32) -> [u32; 8] {
    // `mov r0, #imm` -- AArch32 immediate form `e3 a0 00 XX`.
    let mov_r0_imm = 0xE3A0_0000 | (imm & 0xFF);
    pad(&[mov_r0_imm, BX_LR])
}

/// Look up the native thunk for a handler that returns a fixed
/// `u32` constant in `r0` (with no other side effects). Returns
/// `None` if `value` doesn't fit in an 8-bit ARM mov immediate —
/// the caller should fall back to the regular dispatcher path in
/// that case.
pub fn constant_return_thunk(value: u32) -> Option<[u32; 8]> {
    if value > 0xFF {
        return None;
    }
    Some(const_return_thunk(value))
}

/// Hand-rolled `strlen(s)` — returns count in bytes, exclusive of NUL.
const STRLEN: [u32; 8] = [
    0xE1A0_1000, // mov r1, r0
    0xE4D1_2001, // ldrb r2, [r1], #1
    0xE352_0000, // cmp r2, #0
    0x1AFF_FFFC, // bne -4 (back to ldrb)
    0xE041_0000, // sub r0, r1, r0
    0xE240_0001, // sub r0, r0, #1
    BX_LR,
    BX_LR,
];

/// Hand-rolled `wcslen(s)` — returns count in u16 chars.
const WCSLEN: [u32; 8] = [
    0xE1A0_1000, // mov r1, r0
    0xE0D1_20B2, // ldrh r2, [r1], #2
    0xE352_0000, // cmp r2, #0
    0x1AFF_FFFC, // bne -4 (back to ldrh)
    0xE041_0000, // sub r0, r1, r0
    0xE240_0002, // sub r0, r0, #2
    0xE1A0_00A0, // lsr r0, r0, #1
    BX_LR,
];

/// Hand-rolled `strcpy(dst, src)` — copies through-and-including NUL,
/// returns the original `dst`.
const STRCPY: [u32; 8] = [
    0xE92D_4005, // push {r0, r2, lr}
    0xE4D1_2001, // ldrb r2, [r1], #1
    0xE4C0_2001, // strb r2, [r0], #1
    0xE352_0000, // cmp r2, #0
    0x1AFF_FFFB, // bne -5 (back to ldrb)
    0xE8BD_8005, // pop {r0, r2, pc}
    BX_LR,
    BX_LR,
];

/// Hand-rolled `wcscpy(dst, src)` — wide-char strcpy.
const WCSCPY: [u32; 8] = [
    0xE92D_4005, // push {r0, r2, lr}
    0xE0D1_20B2, // ldrh r2, [r1], #2
    0xE0C0_20B2, // strh r2, [r0], #2
    0xE352_0000, // cmp r2, #0
    0x1AFF_FFFB, // bne -5 (back to ldrh)
    0xE8BD_8005, // pop {r0, r2, pc}
    BX_LR,
    BX_LR,
];

/// Hand-rolled `strcmp(a, b)` — returns signed diff at first
/// mismatch, 0 if equal.
const STRCMP: [u32; 8] = [
    0xE4D0_2001, // ldrb r2, [r0], #1
    0xE4D1_3001, // ldrb r3, [r1], #1
    0xE052_C003, // subs r12, r2, r3
    0x1A00_0001, // bne +1  (skip the trailing cmp)
    0xE352_0000, // cmp r2, #0
    0x1AFF_FFF9, // bne -7 (back to first ldrb)
    0xE1A0_000C, // mov r0, r12
    BX_LR,
];

/// Hand-rolled `wcscmp(a, b)` — wide-char strcmp.
const WCSCMP: [u32; 8] = [
    0xE0D0_20B2, // ldrh r2, [r0], #2
    0xE0D1_30B2, // ldrh r3, [r1], #2
    0xE052_C003, // subs r12, r2, r3
    0x1A00_0001, // bne +1
    0xE352_0000, // cmp r2, #0
    0x1AFF_FFF9, // bne -7
    0xE1A0_000C, // mov r0, r12
    BX_LR,
];

const TOLOWER: [u32; 8] = [
    0xE350_0000,
    0x0A00_0004,
    0xE350_0041,
    0x3A00_0002,
    0xE350_005A,
    0x8A00_0000,
    0xE280_0020,
    BX_LR,
];

/// Hand-rolled `GetTickCount()` — reads the host-managed tick cell
/// at [`TICK_PAGE_VA`] + [`TICK_VALUE_OFFSET`] (= 0x6FFFF000) and
/// returns it.
///
/// The value is refreshed by the kernel run loop on every frame
/// hook, which fires after every dispatched WinCE call — so the
/// granularity matches what the dispatcher path used to provide
/// (the previous Rust handler also read host time once per call).
const GET_TICK_COUNT: [u32; 8] = [
    // movw r0, #0xF000  -- imm16 in {imm4=0xF, imm12=0x000}
    0xE30F_0000,
    // movt r0, #0x6FFF  -- imm16 in {imm4=0x6, imm12=0xFFF}
    0xE346_0FFF,
    0xE590_0000, // ldr r0, [r0]
    BX_LR,
    BX_LR,
    BX_LR,
    BX_LR,
    BX_LR,
];

/// Look up the native thunk for a `coredll.dll` symbol. Returns
/// `None` if the symbol isn't one of the helpers we can JIT directly
/// — the kernel falls back to the regular hooked dispatcher path
/// for those.
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

        // ---- short string helpers ----------------------------------------
        // `strlen` / `strcpy` / `strcmp` are typically called on short
        // strings (HUD labels, file paths). For inputs under ~300 bytes
        // a byte-by-byte ARM loop translated by Unicorn beats the
        // dispatcher round-trip even though the dispatcher itself
        // could call into the host's vectorised libc.
        //
        // The bulk-memory exports (`memcpy` / `memset` / `memmove`)
        // are intentionally NOT on this list: in Derby they're called
        // with several-KiB buffers (DIB blits etc.), and the host's
        // SIMD memcpy reached through the dispatcher is faster than
        // any ARM-mode byte loop we could fit in 32 bytes — we
        // measured a small regression when we tried it, so they're
        // left on the dispatcher path.
        "strlen" => STRLEN,
        "wcslen" | "lstrlenW" => WCSLEN,
        "lstrlenA" => STRLEN,
        "strcpy" | "lstrcpyA" => STRCPY,
        "wcscpy" | "lstrcpyW" => WCSCPY,
        "strcmp" | "lstrcmpA" => STRCMP,
        "wcscmp" | "lstrcmpW" => WCSCMP,
        "tolower" => TOLOWER,

        // ---- host-managed tick page --------------------------------------
        // Reads `*(u32*)0x6FFFF000`, which the run loop refreshes
        // before every dispatched WinCE call. See [`TICK_PAGE_VA`].
        "GetTickCount" => GET_TICK_COUNT,

        _ => return None,
    })
}

/// Refresh the tick cell at [`TICK_PAGE_VA`] with the current
/// monotonic time in milliseconds since the first call. Called by
/// the run loop's frame hook so the [`GET_TICK_COUNT`] thunk's
/// plain `LDR` always sees a fresh value.
///
/// Errors are swallowed: a failing `write_mem` here would mean the
/// page wasn't mapped, which is purely a kernel bug — and the
/// previous `coredll::get_tick_count` dispatcher implementation
/// also kept running silently when its underlying `SystemTime` call
/// failed, so we preserve that behaviour.
pub fn refresh_tick_page(cpu: &mut dyn crate::Cpu) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static START_MS: AtomicU64 = AtomicU64::new(0);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut start = START_MS.load(Ordering::Relaxed);
    if start == 0 {
        // First-call seeding. `compare_exchange` so a concurrent
        // first-call (only possible across multiple `Process`es,
        // which the loader does not currently support but might in
        // future) doesn't race the `now - start` calculation below.
        let _ = START_MS.compare_exchange(0, now_ms, Ordering::Relaxed, Ordering::Relaxed);
        start = START_MS.load(Ordering::Relaxed);
    }
    let delta = (now_ms - start) as u32;
    let _ = cpu.write_mem(TICK_PAGE_VA + TICK_VALUE_OFFSET, &delta.to_le_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_return_thunk_emits_mov_bx_lr() {
        let words = constant_return_thunk(0).expect("0 is encodable");
        // First instruction: `mov r0, #0`.
        assert_eq!(words[0], 0xE3A0_0000);
        // Second instruction: `bx lr`.
        assert_eq!(words[1], 0xE12F_FF1E);
        // Trailing slots are `bx lr` padding.
        for w in &words[2..] {
            assert_eq!(*w, 0xE12F_FF1E);
        }
    }

    #[test]
    fn constant_return_thunk_for_one_returns_imm1() {
        let words = constant_return_thunk(1).expect("1 is encodable");
        assert_eq!(words[0], 0xE3A0_0001);
        assert_eq!(words[1], 0xE12F_FF1E);
    }

    #[test]
    fn constant_return_thunk_for_max_imm8() {
        let words = constant_return_thunk(0xFF).expect("0xFF is encodable");
        assert_eq!(words[0], 0xE3A0_00FF);
    }

    #[test]
    fn constant_return_thunk_rejects_unencodable() {
        // 0x100 doesn't fit in the unrotated ARM 8-bit mov-immediate
        // slot — the caller should fall back to the dispatcher.
        assert!(constant_return_thunk(0x100).is_none());
        assert!(constant_return_thunk(0xFFFF_FFFF).is_none());
    }
}
