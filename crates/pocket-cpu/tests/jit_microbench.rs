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

/// Cost of one host round-trip — the thing PocketHLE pays for every
/// emulated WinCE API call, because each IAT thunk carries a code hook
/// that calls `emu_stop`.
///
/// The guest loop is `bl thunk; b loop` with `bx lr` at `thunk`, so a
/// round-trip executes three guest instructions and one `emu_start` /
/// `emu_stop` pair. Subtracting the pure-JIT rate measured by
/// [`jit_pure_loop`] leaves the fixed re-entry cost, which is what
/// decides whether a hot import is worth turning into a native thunk
/// (strategy 1) or a dispatcher handler (strategy 3).
///
/// `hooks` is swept because Unicorn has to consider every registered
/// code hook when it translates a block, and a real game installs one
/// per import.
fn run_round_trips(label: &str, hooks: u32, round_trips: u64) {
    let mut uc = Unicorn::new(Arch::ARM, Mode::LITTLE_ENDIAN).expect("create Unicorn");
    uc.mem_map(CODE_BASE, 0x1000, Prot::ALL)
        .expect("map code page");
    uc.mem_map(DATA_BASE, 0x1000, Prot::READ | Prot::WRITE)
        .expect("map data page");
    // 0x000: bl  0x100   (thunk)
    // 0x004: b   0x000
    // 0x100: bx  lr
    // imm24 = (target - insn_addr - 8) / 4.
    let bl: u32 = 0xEB00_0000 | (((0x100 - 8) / 4) as u32 & 0x00FF_FFFF);
    let b_back: u32 = 0xEA00_0000 | ((-3i32) as u32 & 0x00FF_FFFF);
    uc.mem_write(CODE_BASE, &bl.to_le_bytes()).expect("bl");
    uc.mem_write(CODE_BASE + 4, &b_back.to_le_bytes())
        .expect("b");
    uc.mem_write(CODE_BASE + 0x100, &0xE12F_FF1Eu32.to_le_bytes())
        .expect("bx lr");

    // One hook per "import", the last of which is the thunk the loop
    // actually calls, so the sweep measures list length rather than
    // which entry matches.
    let stopped = std::rc::Rc::new(std::cell::RefCell::new(false));
    for index in 0..hooks {
        let va = if index + 1 == hooks {
            CODE_BASE + 0x100
        } else {
            CODE_BASE + 0x200 + u64::from(index) * 4
        };
        let flag = stopped.clone();
        uc.add_code_hook(va, va, move |uc: &mut Unicorn<()>, _addr, _size| {
            *flag.borrow_mut() = true;
            let _ = uc.emu_stop();
        })
        .expect("add_code_hook");
    }

    uc.reg_write(RegisterARM::SP, DATA_BASE + 0x100)
        .expect("sp");
    let mut pc = CODE_BASE;
    // Warm-up: translate both blocks before the clock starts.
    for _ in 0..64 {
        uc.emu_start(pc, 0, 0, 0).expect("warmup");
        pc = uc.reg_read(RegisterARM::LR).expect("lr");
    }
    let t0 = Instant::now();
    for _ in 0..round_trips {
        uc.emu_start(pc, 0, 0, 0).expect("emu_start");
        pc = uc.reg_read(RegisterARM::LR).expect("lr");
    }
    let elapsed = t0.elapsed();
    eprintln!(
        "[{label}] {hooks} hooks, {round_trips} round-trips in {elapsed:?} = \
         {:.0} ns/round-trip",
        elapsed.as_nanos() as f64 / round_trips as f64,
    );
}

/// Same round-trip, but with the whole thunk pool covered by a single
/// ranged code hook instead of one hook per thunk VA.
///
/// PocketHLE registers a hook for every import *and* every dynamic
/// export it can serve through `GetProcAddress` (~5 000 for
/// `coredll.dll` alone), so if Unicorn's per-`emu_start` cost grows
/// with the length of the hook list, that list is the frame budget.
fn run_round_trips_ranged(label: &str, round_trips: u64) {
    let mut uc = Unicorn::new(Arch::ARM, Mode::LITTLE_ENDIAN).expect("create Unicorn");
    uc.mem_map(CODE_BASE, 0x1000, Prot::ALL)
        .expect("map code page");
    uc.mem_map(DATA_BASE, 0x1000, Prot::READ | Prot::WRITE)
        .expect("map data page");
    // `bl` from 0x000 to 0x100: imm24 = (target - insn_addr - 8) / 4.
    let bl: u32 = 0xEB00_0000 | (((0x100 - 8) / 4) as u32 & 0x00FF_FFFF);
    let b_back: u32 = 0xEA00_0000 | ((-3i32) as u32 & 0x00FF_FFFF);
    uc.mem_write(CODE_BASE, &bl.to_le_bytes()).expect("bl");
    uc.mem_write(CODE_BASE + 4, &b_back.to_le_bytes())
        .expect("b");
    uc.mem_write(CODE_BASE + 0x100, &0xE12F_FF1Eu32.to_le_bytes())
        .expect("bx lr");
    uc.add_code_hook(
        CODE_BASE + 0x100,
        CODE_BASE + 0xfff,
        move |uc: &mut Unicorn<()>, _addr, _size| {
            let _ = uc.emu_stop();
        },
    )
    .expect("add_code_hook");
    uc.reg_write(RegisterARM::SP, DATA_BASE + 0x100)
        .expect("sp");
    let mut pc = CODE_BASE;
    for _ in 0..64 {
        uc.emu_start(pc, 0, 0, 0).expect("warmup");
        pc = uc.reg_read(RegisterARM::LR).expect("lr");
    }
    let t0 = Instant::now();
    for _ in 0..round_trips {
        uc.emu_start(pc, 0, 0, 0).expect("emu_start");
        pc = uc.reg_read(RegisterARM::LR).expect("lr");
    }
    let elapsed = t0.elapsed();
    eprintln!(
        "[{label}] 1 ranged hook, {round_trips} round-trips in {elapsed:?} = \
         {:.0} ns/round-trip",
        elapsed.as_nanos() as f64 / round_trips as f64,
    );
}

#[test]
#[ignore]
fn jit_slice_round_trip_cost() {
    for hooks in [1u32, 16, 256, 1024, 5166] {
        run_round_trips("round-trip", hooks, 200_000);
    }
    run_round_trips_ranged("round-trip-ranged", 200_000);
}

/// Cost of the framebuffer read-back that the presentation path does:
/// `uc_mem_read` of a whole 16 bpp screen, plus the `memcmp` against
/// the host copy that decides whether the frame changed.
#[test]
#[ignore]
fn mem_read_framebuffer_cost() {
    for (w, h) in [(240u64, 320u64), (800, 480)] {
        let bytes = w * h * 2;
        let pages = (bytes + 0xfff) & !0xfff;
        let mut uc = Unicorn::new(Arch::ARM, Mode::LITTLE_ENDIAN).expect("create Unicorn");
        uc.mem_map(DATA_BASE, pages, Prot::READ | Prot::WRITE)
            .expect("map fb");
        let mut host = vec![0u8; bytes as usize];
        let mut shadow = vec![0u8; bytes as usize];
        let reads = 2_000u64;
        let t0 = Instant::now();
        for _ in 0..reads {
            uc.mem_read(DATA_BASE, &mut host).expect("mem_read");
            if host != shadow {
                shadow.copy_from_slice(&host);
            }
        }
        let elapsed = t0.elapsed();
        eprintln!(
            "[fb-readback] {w}x{h} ({bytes} B) {reads} reads in {elapsed:?} = {:.0} ns/read",
            elapsed.as_nanos() as f64 / reads as f64,
        );
    }
}

/// Marginal cost of a single guest load and a single guest store.
///
/// [`jit_pure_loop`] shows the ALU path runs at ~1.4 ns/instruction,
/// while [`jit_loop_with_load_store`] drops to ~10 ns/instruction as
/// soon as two memory accesses join the loop — so memory is where a
/// software-rendered game's frame budget goes. Splitting loads from
/// stores says whether the cost is the softmmu TLB lookup (both should
/// be equal) or QEMU's dirty-page bookkeeping on writes (stores much
/// worse), which decides whether remapping the guest's frame buffer
/// differently could help at all.
#[test]
#[ignore]
fn jit_load_store_split() {
    // ldr r1,[sp] / subs r0,r0,#1 / bne -2
    let load_only: [u8; 12] = [
        0x00, 0x10, 0x9D, 0xE5, // ldr r1, [sp]
        0x01, 0x00, 0x50, 0xE2, // subs r0, r0, #1
        0xFC, 0xFF, 0xFF, 0x1A, // bne -2
    ];
    run_loop("1-load-3instr", &load_only, ITERATIONS / 2, false);
    // str r1,[sp] / subs r0,r0,#1 / bne -2
    let store_only: [u8; 12] = [
        0x00, 0x10, 0x8D, 0xE5, // str r1, [sp]
        0x01, 0x00, 0x50, 0xE2, // subs r0, r0, #1
        0xFC, 0xFF, 0xFF, 0x1A, // bne -2
    ];
    run_loop("1-store-3instr", &store_only, ITERATIONS / 2, false);
    // Four loads per iteration: divides out the loop overhead.
    let four_loads: [u8; 24] = [
        0x00, 0x10, 0x9D, 0xE5, // ldr r1, [sp]
        0x04, 0x20, 0x9D, 0xE5, // ldr r2, [sp, #4]
        0x08, 0x30, 0x9D, 0xE5, // ldr r3, [sp, #8]
        0x0C, 0x40, 0x9D, 0xE5, // ldr r4, [sp, #12]
        0x01, 0x00, 0x50, 0xE2, // subs r0, r0, #1
        0xF9, 0xFF, 0xFF, 0x1A, // bne -5
    ];
    run_loop("4-loads-6instr", &four_loads, ITERATIONS / 4, false);
}

/// Same load/store loop, but with the always-on `MEM_INVALID` hook that
/// [`crate::unicorn::UnicornCpu`] installs so it can report the faulting
/// address on a crash. If Unicorn compiles a helper call into every
/// access once any memory hook exists, that hook alone would be the
/// software-rendering frame budget — this test is how we tell.
#[test]
#[ignore]
fn jit_load_store_with_mem_invalid_hook() {
    let code: [u8; 16] = [
        0x00, 0x10, 0x9D, 0xE5, // ldr r1, [sp]
        0x00, 0x10, 0x8D, 0xE5, // str r1, [sp]
        0x01, 0x00, 0x50, 0xE2, // subs r0, r0, #1
        0xFB, 0xFF, 0xFF, 0x1A, // bne -3
    ];
    let mut uc = Unicorn::new(Arch::ARM, Mode::LITTLE_ENDIAN).expect("create Unicorn");
    uc.mem_map(CODE_BASE, 0x1000, Prot::ALL).expect("map code");
    uc.mem_map(DATA_BASE, 0x1000, Prot::READ | Prot::WRITE)
        .expect("map data");
    uc.mem_write(CODE_BASE, &code).expect("write code");
    uc.add_mem_hook(
        unicorn_engine::unicorn_const::HookType::MEM_INVALID,
        0,
        u64::MAX,
        |_uc: &mut Unicorn<()>, _kind, _addr, _size, _value| false,
    )
    .expect("mem hook");
    let until = CODE_BASE + code.len() as u64;
    let iters = ITERATIONS / 2;
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
    eprintln!(
        "[ld+st-4instr+mem_invalid_hook] {iters} iters in {elapsed:?}, \
         loop tail-rate = {:.1} M iter/s",
        iters as f64 / elapsed.as_secs_f64() / 1.0e6,
    );
}

/// Four stores per iteration to the same page.
///
/// Tells apart "QEMU takes the clean-RAM slow path once per page and
/// then runs at full speed" from "every single store goes through the
/// C helper". The second is what a software-rendered game would feel,
/// because its blitter is one long run of stores.
#[test]
#[ignore]
fn jit_four_stores() {
    let four_stores: [u8; 24] = [
        0x00, 0x10, 0x8D, 0xE5, // str r1, [sp]
        0x04, 0x10, 0x8D, 0xE5, // str r1, [sp, #4]
        0x08, 0x10, 0x8D, 0xE5, // str r1, [sp, #8]
        0x0C, 0x10, 0x8D, 0xE5, // str r1, [sp, #12]
        0x01, 0x00, 0x50, 0xE2, // subs r0, r0, #1
        0xF9, 0xFF, 0xFF, 0x1A, // bne -5
    ];
    run_loop("4-stores-6instr", &four_stores, ITERATIONS / 8, false);
}

/// Does Unicorn's *virtual* TLB restore the TCG inline store fast path?
///
/// QEMU only lets a store use the inline fast path once `TLB_NOTDIRTY`
/// has been cleared for that page, and `notdirty_write()` refuses to
/// clear it while either (a) the TLB entry claims the page is
/// executable or (b) any memory hook covers the address. With the ARM
/// MMU disabled — which is how PocketHLE runs, since WinCE images are
/// loaded flat with no page tables — `get_phys_addr()` hands back
/// `PAGE_READ|PAGE_WRITE|PAGE_EXEC` for *every* page, so (a) is always
/// true and every guest store pays a ~42 ns helper call. A software
/// renderer is one long run of stores, so that is the whole frame
/// budget.
///
/// `UC_TLB_VIRTUAL` replaces the architectural walk with a callback, so
/// we can hand back "read+write, not executable" for data pages. This
/// sweep measures the two blockers independently before we rely on
/// either.
fn run_store_variant(label: &str, mem_invalid_hook: bool, virtual_tlb: bool) {
    // str r1,[sp] ×4 / subs r0,r0,#1 / bne -5
    let four_stores: [u8; 24] = [
        0x00, 0x10, 0x8D, 0xE5, // str r1, [sp]
        0x04, 0x10, 0x8D, 0xE5, // str r1, [sp, #4]
        0x08, 0x10, 0x8D, 0xE5, // str r1, [sp, #8]
        0x0C, 0x10, 0x8D, 0xE5, // str r1, [sp, #12]
        0x01, 0x00, 0x50, 0xE2, // subs r0, r0, #1
        0xF9, 0xFF, 0xFF, 0x1A, // bne -5
    ];
    let mut uc = Unicorn::new(Arch::ARM, Mode::LITTLE_ENDIAN).expect("create Unicorn");
    uc.mem_map(CODE_BASE, 0x1000, Prot::ALL).expect("map code");
    uc.mem_map(DATA_BASE, 0x1000, Prot::READ | Prot::WRITE)
        .expect("map data");
    uc.mem_write(CODE_BASE, &four_stores).expect("write code");
    if mem_invalid_hook {
        uc.add_mem_hook(
            unicorn_engine::unicorn_const::HookType::MEM_INVALID,
            0,
            u64::MAX,
            |_uc: &mut Unicorn<()>, _kind, _addr, _size, _value| false,
        )
        .expect("mem hook");
    }
    if virtual_tlb {
        uc.ctl_set_tlb_type(unicorn_engine::unicorn_const::TlbType::VIRTUAL)
            .expect("tlb type");
        // begin > end means "every address" to Unicorn's bound check.
        uc.add_tlb_hook(1, 0, |_uc: &mut Unicorn<()>, addr, _kind| {
            let perms = if addr & !0xfff == CODE_BASE {
                Prot::ALL
            } else {
                // The point of the whole exercise: no EXEC on data
                // pages, so `tlbe->addr_code` stays -1.
                Prot::READ | Prot::WRITE
            };
            Some(unicorn_engine::unicorn_const::TlbEntry { paddr: addr, perms })
        })
        .expect("tlb hook");
    }
    let until = CODE_BASE + four_stores.len() as u64;
    let iters = ITERATIONS / 8;
    uc.reg_write(RegisterARM::R0, 100_000).expect("warmup");
    uc.reg_write(RegisterARM::SP, DATA_BASE + 0x100)
        .expect("sp");
    uc.emu_start(CODE_BASE, until, 0, 0).expect("warmup");
    uc.reg_write(RegisterARM::R0, iters).expect("counter");
    uc.reg_write(RegisterARM::SP, DATA_BASE + 0x100)
        .expect("sp");
    let t0 = Instant::now();
    uc.emu_start(CODE_BASE, until, 0, 0).expect("emu_start");
    let elapsed = t0.elapsed();
    eprintln!(
        "[{label}] {iters} iters in {elapsed:?} = {:.1} M iter/s, {:.1} ns/store",
        iters as f64 / elapsed.as_secs_f64() / 1.0e6,
        elapsed.as_nanos() as f64 / (iters * 4) as f64,
    );
}

#[test]
#[ignore]
fn jit_store_tlb_variants() {
    run_store_variant("4-stores/cpu-tlb+mem_invalid", true, false);
    run_store_variant("4-stores/cpu-tlb", false, false);
    run_store_variant("4-stores/vtlb+mem_invalid", true, true);
    run_store_variant("4-stores/vtlb", false, true);
}

/// Same sweep for *loads*, and for the load/blend/store shape a
/// software sprite blitter actually runs.
///
/// [`jit_store_tlb_variants`] proved the virtual TLB is what lets a
/// guest store use the TCG inline fast path. Loads never carried
/// `TLB_NOTDIRTY`, so the open question is whether they were already
/// inline — if they were, the remaining per-pixel cost in a game like
/// Zuma is genuine guest work and no amount of MMU plumbing will move
/// it; if they were not, there is a second win the same size as the
/// first.
fn run_mem_variant(label: &str, code: &[u8], accesses: u64, mem_invalid_hook: bool, vtlb: bool) {
    let mut uc = Unicorn::new(Arch::ARM, Mode::LITTLE_ENDIAN).expect("create Unicorn");
    uc.mem_map(CODE_BASE, 0x1000, Prot::ALL).expect("map code");
    uc.mem_map(DATA_BASE, 0x1000, Prot::READ | Prot::WRITE)
        .expect("map data");
    uc.mem_write(CODE_BASE, code).expect("write code");
    if mem_invalid_hook {
        uc.add_mem_hook(
            unicorn_engine::unicorn_const::HookType::MEM_INVALID,
            0,
            u64::MAX,
            |_uc: &mut Unicorn<()>, _kind, _addr, _size, _value| false,
        )
        .expect("mem hook");
    }
    if vtlb {
        uc.ctl_set_tlb_type(unicorn_engine::unicorn_const::TlbType::VIRTUAL)
            .expect("tlb type");
        uc.add_tlb_hook(1, 0, |_uc: &mut Unicorn<()>, addr, _kind| {
            let perms = if addr & !0xfff == CODE_BASE {
                Prot::ALL
            } else {
                Prot::READ | Prot::WRITE
            };
            Some(unicorn_engine::unicorn_const::TlbEntry { paddr: addr, perms })
        })
        .expect("tlb hook");
    }
    let until = CODE_BASE + code.len() as u64;
    let iters = ITERATIONS / 8;
    uc.reg_write(RegisterARM::R0, 100_000).expect("warmup");
    uc.reg_write(RegisterARM::SP, DATA_BASE + 0x100)
        .expect("sp");
    uc.emu_start(CODE_BASE, until, 0, 0).expect("warmup");
    uc.reg_write(RegisterARM::R0, iters).expect("counter");
    uc.reg_write(RegisterARM::SP, DATA_BASE + 0x100)
        .expect("sp");
    let t0 = Instant::now();
    uc.emu_start(CODE_BASE, until, 0, 0).expect("emu_start");
    let elapsed = t0.elapsed();
    eprintln!(
        "[{label}] {iters} iters in {elapsed:?} = {:.1} ns/access",
        elapsed.as_nanos() as f64 / (iters * accesses) as f64,
    );
}

#[test]
#[ignore]
fn jit_load_tlb_variants() {
    // ldr r1..r4 from [sp,#0/4/8/12] / subs / bne -5
    let four_loads: [u8; 24] = [
        0x00, 0x10, 0x9D, 0xE5, 0x04, 0x20, 0x9D, 0xE5, 0x08, 0x30, 0x9D, 0xE5, 0x0C, 0x40, 0x9D,
        0xE5, 0x01, 0x00, 0x50, 0xE2, 0xF9, 0xFF, 0xFF, 0x1A,
    ];
    run_mem_variant("4-loads/cpu-tlb+mem_invalid", &four_loads, 4, true, false);
    run_mem_variant("4-loads/cpu-tlb", &four_loads, 4, false, false);
    run_mem_variant("4-loads/vtlb", &four_loads, 4, false, true);
    // The blitter shape: ldr src / ldr dst / orr / str dst.
    let blend: [u8; 24] = [
        0x00, 0x10, 0x9D, 0xE5, // ldr r1, [sp]
        0x04, 0x20, 0x9D, 0xE5, // ldr r2, [sp, #4]
        0x02, 0x10, 0x81, 0xE1, // orr r1, r1, r2
        0x08, 0x10, 0x8D, 0xE5, // str r1, [sp, #8]
        0x01, 0x00, 0x50, 0xE2, // subs r0, r0, #1
        0xF9, 0xFF, 0xFF, 0x1A, // bne -5
    ];
    run_mem_variant("blend/cpu-tlb+mem_invalid", &blend, 3, true, false);
    run_mem_variant("blend/vtlb", &blend, 3, false, true);
}
