//! Microbenchmark for the Unicorn ARM JIT.
//!
//! Runs a tight `subs r0, #1; bne -1` loop with no code hooks and no
//! memory I/O, so the only thing being measured is QEMU's translation
//! cache + dispatch overhead. Used to decide whether the FPS ceiling
//! we hit on real games is the JIT's pure throughput or something
//! higher up the stack (hooks, MMU callbacks, dispatcher round-trips).
//!
//! Marked `#[ignore]` so it does not run in regular `cargo test`
//! (the loop runs ~200 M instructions and takes ~1–2 s on a fast
//! host). Run with:
//!
//!     cargo test --release -p pocket-cpu --features unicorn \
//!         --test jit_microbench -- --ignored --nocapture

#![cfg(feature = "unicorn")]

use std::time::Instant;
use unicorn_engine::unicorn_const::{Arch, Mode, Prot};
use unicorn_engine::{RegisterARM, Unicorn};

const CODE_BASE: u64 = 0x1000;
const DATA_BASE: u64 = 0x2000;
const ITERATIONS: u64 = 100_000_000;

fn run_loop(label: &str, code: &[u8], iters: u64, install_hook: bool) {
    let mut uc = Unicorn::new(Arch::ARM, Mode::LITTLE_ENDIAN).expect("create Unicorn");
    uc.mem_map(CODE_BASE, 0x1000, Prot::ALL)
        .expect("map code page");
    uc.mem_map(DATA_BASE, 0x1000, Prot::READ | Prot::WRITE)
        .expect("map data page");
    uc.mem_write(CODE_BASE, code).expect("write code");

    if install_hook {
        // Install code hooks just outside the loop so the loop body
        // is not directly hooked, but Unicorn still has to consider
        // the hook list at TB-translation time (this matches the
        // shape of our real workload, which has thousands of code
        // hooks for IAT thunks living in a different page).
        for off in 0..16u32 {
            let _ = uc.add_code_hook(
                CODE_BASE + 0x800 + (off as u64) * 4,
                CODE_BASE + 0x800 + (off as u64) * 4 + 3,
                |_uc: &mut Unicorn<()>, _addr, _size| {},
            );
        }
    }

    let until = CODE_BASE + code.len() as u64;
    // Warm-up so the TB is cached.
    uc.reg_write(RegisterARM::R0, 1_000_000).expect("warmup");
    uc.reg_write(RegisterARM::SP, DATA_BASE + 0x100)
        .expect("sp");
    uc.emu_start(CODE_BASE, until, 0, 0).expect("warmup");

    uc.reg_write(RegisterARM::R0, iters).expect("counter");
    uc.reg_write(RegisterARM::SP, DATA_BASE + 0x100)
        .expect("sp");
    let t0 = Instant::now();
    uc.emu_start(CODE_BASE, until, 0, 0).expect("emu_start");
    let elapsed = t0.elapsed();
    // The caller passes `iters` for the loop counter; instructions
    // executed per iteration is encoded in the test name's prefix
    // and reported alongside.
    eprintln!(
        "[{label}] {iters} iters in {elapsed:?}, \
         loop tail-rate = {:.1} M iter/s",
        iters as f64 / elapsed.as_secs_f64() / 1.0e6,
    );
}

#[test]
#[ignore]
fn jit_pure_loop() {
    // subs r0, r0, #1 / bne -1   — 2 instr/iter, no memory.
    let code: [u8; 8] = [0x01, 0x00, 0x50, 0xE2, 0xFD, 0xFF, 0xFF, 0x1A];
    run_loop("pure-2instr", &code, ITERATIONS, false);
}

#[test]
#[ignore]
fn jit_loop_with_load_store() {
    // ldr r1,[sp] / str r1,[sp] / subs r0,r0,#1 / bne -3
    //   4 instr/iter, 2 mem ops/iter. Models the typical hot
    //   game loop which does 1–2 mem ops per ALU op.
    let code: [u8; 16] = [
        0x00, 0x10, 0x9D, 0xE5, // ldr r1, [sp]
        0x00, 0x10, 0x8D, 0xE5, // str r1, [sp]
        0x01, 0x00, 0x50, 0xE2, // subs r0, r0, #1
        0xFB, 0xFF, 0xFF, 0x1A, // bne -3
    ];
    run_loop("ld+st-4instr", &code, ITERATIONS / 2, false);
}

#[test]
#[ignore]
fn jit_pure_loop_with_hooks() {
    // Same as `jit_pure_loop`, but with 16 code hooks installed in
    // a different region. Tells us whether the hook list itself is
    // dragging the JIT down even on TBs that don't overlap any
    // hook.
    let code: [u8; 8] = [0x01, 0x00, 0x50, 0xE2, 0xFD, 0xFF, 0xFF, 0x1A];
    run_loop("pure-2instr+hooks", &code, ITERATIONS, true);
}
