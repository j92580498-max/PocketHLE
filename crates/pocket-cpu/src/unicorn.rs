//! `unicorn-engine`-backed CPU.
//!
//! Compiled only with `--features unicorn`. Apart from the build cost,
//! this is the authoritative ARM backend used at runtime.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ::unicorn_engine::unicorn_const::{
    Arch as UcArch, HookType, MemType, Mode, Prot as UcProt, TlbEntry, TlbType,
};
use ::unicorn_engine::{RegisterARM, RegisterMIPS, Unicorn};

use crate::{regs::ArmReg, Arch, Cpu, CpuError, Prot, StopReason};

/// Which guest pages exist, and which of them the CPU is allowed to
/// fetch instructions from.
///
/// Shared with the virtual-TLB fill hook installed by
/// [`UnicornCpu::new_for_arch`], which is the only thing that answers
/// "may this page be read / written / executed" once we take Unicorn
/// off its architectural page-table walk.
#[derive(Default)]
struct GuestMap {
    /// `(start, end_exclusive, executable)` for every successful
    /// [`Cpu::map_region`], kept sorted by `start`.
    regions: Vec<(u64, u64, bool)>,
    /// Pages promoted to executable at run time because the guest was
    /// caught fetching from them even though their mapping is not
    /// executable, kept sorted.
    promoted_exec: Vec<u64>,
}

impl GuestMap {
    fn insert(&mut self, start: u64, len: u64, exec: bool) {
        let entry = (start, start.saturating_add(len), exec);
        let at = self.regions.partition_point(|r| r.0 < start);
        self.regions.insert(at, entry);
    }

    /// `Some(executable)` when `addr` falls inside a mapped region.
    fn region_for(&self, addr: u64) -> Option<bool> {
        let at = self.regions.partition_point(|r| r.0 <= addr);
        self.regions[..at]
            .iter()
            .rev()
            .find(|r| addr < r.1)
            .map(|r| r.2)
    }

    fn is_promoted_exec(&self, page: u64) -> bool {
        !self.promoted_exec.is_empty() && self.promoted_exec.binary_search(&page).is_ok()
    }

    fn promote_exec(&mut self, page: u64) {
        if let Err(at) = self.promoted_exec.binary_search(&page) {
            self.promoted_exec.insert(at, page);
        }
    }
}

pub struct UnicornCpu {
    uc: Unicorn<'static, ()>,
    last_hook: Rc<RefCell<Option<u32>>>,
    hook_skip_once: Rc<RefCell<Option<u32>>>,
    /// Address of the last invalid memory access seen by the
    /// `mem_invalid` hook, recorded so crash reports can name the
    /// faulting address instead of just the access kind.
    last_fault: Rc<RefCell<Option<(String, u64)>>>,
    stop_requested: Rc<RefCell<bool>>,
    /// Wall-clock deadline for the current slice, or `None` when the
    /// slice watchdog is disabled. Read by the block hook.
    slice_deadline: Rc<Cell<Option<Instant>>>,
    /// Set by the block hook when it stopped emulation because
    /// `slice_deadline` passed, so `run_until_hook` can tell a watchdog
    /// stop apart from an explicit one.
    slice_expired: Rc<Cell<bool>>,
    /// When `POCKETHLE_DUMP_MEM` ranges were last written, for rate
    /// limiting.
    last_dump: Option<Instant>,
    /// Mapping table backing the virtual-TLB hook; `None` when the
    /// architectural TLB is in use and Unicorn resolves pages itself.
    guest_map: Option<Rc<RefCell<GuestMap>>>,
    arch: Arch,
    mips_status: u32,
}

impl UnicornCpu {
    /// Write out any ranges named by `POCKETHLE_DUMP_MEM`.
    ///
    /// Driven from the slice loop rather than at exit, because a guest
    /// that ends by faulting never reaches a tidy shutdown -- the run
    /// we care about is precisely the one that dies.
    ///
    /// Each range is rewritten rather than captured once. Writing once
    /// is the obvious design and it is wrong here: the heap is mapped
    /// from process start, so the very first read succeeds and returns
    /// a page of zeroes long before a loader has decompressed anything
    /// into it. Overwriting means whatever is on disk when the guest
    /// stops is the last state it had, which is what a post-mortem
    /// wants. Rate limited so this is a file write per second, not per
    /// slice.
    fn dump_requested_memory(&mut self) {
        let specs = dump_mem_specs();
        if specs.is_empty() {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.last_dump {
            if now.duration_since(last) < Duration::from_millis(1000) {
                return;
            }
        }
        self.last_dump = Some(now);
        for (addr, len, path) in specs.iter() {
            let mut buf = vec![0u8; *len as usize];
            if self.uc.mem_read(*addr, &mut buf).is_err() {
                continue;
            }
            if buf.iter().all(|b| *b == 0) {
                // Nothing there yet. Do not clobber a good earlier
                // capture with a page of zeroes.
                continue;
            }
            match std::fs::write(path, &buf) {
                Ok(()) => log::debug!(
                    "dumped 0x{addr:08x}..+0x{len:x} to {path} ({} bytes)",
                    buf.len()
                ),
                Err(e) => log::warn!("could not write {path}: {e}"),
            }
        }
    }

    pub fn new() -> Result<Self, CpuError> {
        Self::new_for_arch(Arch::Arm)
    }

    pub fn new_for_arch(arch: Arch) -> Result<Self, CpuError> {
        let (uc_arch, mode) = match arch {
            Arch::Arm => (UcArch::ARM, Mode::LITTLE_ENDIAN),
            Arch::Mips => (UcArch::MIPS, Mode::MIPS32 | Mode::LITTLE_ENDIAN),
        };
        let mut uc = Unicorn::new(uc_arch, mode)
            .map_err(|e| CpuError::Backend(format!("Unicorn::new failed: {e:?}")))?;
        if arch == Arch::Arm {
            let _ = uc.reg_write(RegisterARM::FPEXC, 0x4000_0000);
            let _ = uc.reg_write(RegisterARM::C1_C0_2, 0x00F0_0000);
        }
        let last_fault: Rc<RefCell<Option<(String, u64)>>> = Rc::new(RefCell::new(None));
        let guest_map = install_virtual_tlb(&mut uc, &last_fault);
        if guest_map.is_none() {
            // Architectural TLB: Unicorn resolves pages itself, so the
            // only way to learn the faulting address is the
            // invalid-access hook.
            let sink = last_fault.clone();
            let _ = uc.add_mem_hook(
                ::unicorn_engine::unicorn_const::HookType::MEM_INVALID,
                0,
                u64::MAX,
                move |_uc, kind, addr, size, _value| {
                    *sink.borrow_mut() = Some((format!("{kind:?} size={size}"), addr));
                    false
                },
            );
        }
        if let Ok(spec) = std::env::var("POCKETHLE_WATCH_MEM") {
            let parse = |t: &str| u64::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok();
            let want_value = std::env::var("POCKETHLE_WATCH_VAL")
                .ok()
                .and_then(|v| parse(&v));
            for token in spec.split(',') {
                let (lo, hi) = match token.split_once('-') {
                    Some((a, b)) => match (parse(a), parse(b)) {
                        (Some(a), Some(b)) => (a, b),
                        _ => continue,
                    },
                    None => match parse(token) {
                        Some(a) => (a, a + 3),
                        None => continue,
                    },
                };
                let _ = uc.add_mem_hook(
                    ::unicorn_engine::unicorn_const::HookType::MEM_WRITE,
                    lo,
                    hi,
                    move |uc, _kind, a, size, value| {
                        if let Some(want) = want_value {
                            if value as u64 != want {
                                return true;
                            }
                        }
                        let pc = uc.reg_read(RegisterARM::PC).unwrap_or(0);
                        eprintln!(
                            "[watch-mem] write 0x{a:08x} size={size} value=0x{value:08x} pc=0x{pc:08x}"
                        );
                        true
                    },
                );
            }
        }
        let slice_deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let slice_expired: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Slice watchdog. Only installed when the feature is actually
        // switched on, because a block hook spanning the whole address
        // space stops QEMU chaining translation blocks and that costs
        // throughput on every guest instruction, watchdog or not.
        //
        // This replaces passing a `timeout` to `uc_emu_start`, which
        // looks free and is not: unicorn arms it by spawning a QEMU
        // timer thread *per call*, and slices here end on an IAT thunk
        // hook thousands of times a second. Thousands of OS thread
        // creations per second dominated everything else and dropped a
        // real game from ~12 FPS to ~2.4.
        //
        // The clock is only read once every 1024 blocks. A slice that
        // needs interrupting is spinning for many milliseconds, so
        // granularity is irrelevant, whereas an `Instant::now()` on
        // every block would put a timer read in the hot path.
        // Data write-watchpoint.
        //
        // `--watch` installs a *code* breakpoint, which cannot answer
        // the question that keeps coming up when a guest calls through
        // a null field: who was supposed to write it? This does --
        // `POCKETHLE_WATCH_MEM=0x502d35b0` (optionally `ADDR:LEN`,
        // default 4 bytes) logs every write into that range with the
        // guest `PC` that made it and the value stored. Nothing logged
        // by the time the field is used means nothing ever wrote it,
        // which is just as informative.
        if let Some((begin, end)) = watch_mem_range() {
            log::info!("memory write-watch armed on 0x{begin:08x}..=0x{end:08x}");
            let _ = uc.add_mem_hook(
                HookType::MEM_WRITE,
                begin,
                end,
                move |uc, _mem_type, address, size, value| {
                    let pc = uc.reg_read(RegisterARM::PC).unwrap_or(0);
                    log::info!(
                        "WATCH write 0x{address:08x} size={size} value=0x{value:08x} \
                         from pc=0x{pc:08x}"
                    );
                    true
                },
            );
        }
        if slice_watchdog_ms() > 0 {
            let deadline = slice_deadline.clone();
            let expired = slice_expired.clone();
            let blocks = Cell::new(0u64);
            let _ = uc.add_block_hook(0, u64::MAX, move |uc, _addr, _size| {
                let n = blocks.get().wrapping_add(1);
                blocks.set(n);
                if !n.is_multiple_of(1024) {
                    return;
                }
                if let Some(limit) = deadline.get() {
                    if Instant::now() >= limit {
                        expired.set(true);
                        deadline.set(None);
                        let _ = uc.emu_stop();
                    }
                }
            });
        }
        Ok(Self {
            uc,
            arch,
            last_fault,
            guest_map,
            last_hook: Rc::new(RefCell::new(None)),
            hook_skip_once: Rc::new(RefCell::new(None)),
            stop_requested: Rc::new(RefCell::new(false)),
            slice_deadline,
            slice_expired,
            last_dump: None,
            mips_status: 0,
        })
    }
}

/// Serve the softmmu from our own mapping table instead of the guest's
/// page tables, and return that table.
///
/// This is the single biggest win available to a software-rendered
/// Pocket PC game. PocketHLE loads WinCE images flat and never builds
/// ARM page tables, so guest code runs with the MMU off — and QEMU's
/// MMU-disabled path hands back `PAGE_READ | PAGE_WRITE | PAGE_EXEC`
/// for *every* page. A page QEMU believes is executable keeps its
/// `TLB_NOTDIRTY` bit forever, because `notdirty_write()` only clears
/// it when the TLB entry has no code address; and a store to a
/// `TLB_NOTDIRTY` page abandons the TCG inline fast path for a C
/// helper. `jit_store_tlb_variants` in `tests/jit_microbench.rs`
/// measures the difference: 38.1 ns per guest store with the
/// architectural TLB, 3.6 ns with this one.
///
/// Zuma paints its 800x480 frame with plain `STR` instructions rather
/// than a memcpy, so that 10x store penalty *was* its frame budget.
/// Marking data pages non-executable is what lets QEMU clear
/// `TLB_NOTDIRTY` and inline them.
///
/// The same measurement shows the fast path also requires that no
/// memory hook covers the address (`uc_mem_hook_installed()` is the
/// third condition in `notdirty_write()`), which is why the caller only
/// installs the `MEM_INVALID` hook when this returns `None`: fault
/// addresses are recorded here instead, and with the access type.
///
/// Set `POCKETHLE_CPU_TLB=1` to keep Unicorn's architectural TLB, which
/// restores the pre-optimisation behaviour exactly.
fn install_virtual_tlb(
    uc: &mut Unicorn<'static, ()>,
    last_fault: &Rc<RefCell<Option<(String, u64)>>>,
) -> Option<Rc<RefCell<GuestMap>>> {
    if std::env::var_os("POCKETHLE_CPU_TLB").is_some() {
        return None;
    }
    if uc.ctl_set_tlb_type(TlbType::VIRTUAL).is_err() {
        return None;
    }
    let map: Rc<RefCell<GuestMap>> = Rc::new(RefCell::new(GuestMap::default()));
    let regions = map.clone();
    let sink = last_fault.clone();
    // `begin > end` is Unicorn's "every address" bound check. The
    // address handed to the callback is already page-aligned.
    let installed = uc.add_tlb_hook(1, 0, move |_uc, page, kind| {
        let mut map = regions.borrow_mut();
        let Some(region_exec) = map.region_for(page) else {
            *sink.borrow_mut() = Some((format!("{kind:?} unmapped"), page));
            return None;
        };
        // Report read/write for anything mapped, matching what the
        // MMU-disabled ARM walk used to grant: Unicorn still enforces
        // the region's own `UC_PROT_*` bits in its store/load helper,
        // and a page we hand back as non-executable stays on the slow
        // helper path anyway whenever it is genuinely read-only.
        let mut perms = UcProt::READ | UcProt::WRITE;
        if region_exec || map.is_promoted_exec(page) {
            perms |= UcProt::EXEC;
        } else if kind == MemType::FETCH {
            // A guest running code out of a page it mapped as data —
            // a runtime-built trampoline — has to keep working.
            // Remembering the promotion is also what keeps QEMU
            // invalidating that page's translations on later writes:
            // an entry with no code address would let stores go
            // inline and leave stale translated code behind.
            map.promote_exec(page);
            perms |= UcProt::EXEC;
        }
        Some(TlbEntry { paddr: page, perms })
    });
    if installed.is_err() {
        // Without the hook the virtual TLB would derive permissions
        // from the access type alone, so a page used for both loads
        // and stores would refill on every access. Go back to the
        // architectural TLB instead.
        let _ = uc.ctl_set_tlb_type(TlbType::CPU);
        return None;
    }
    Some(map)
}

fn map_prot(p: Prot) -> UcProt {
    let mut m = UcProt::NONE;
    if p.contains(Prot::READ) {
        m |= UcProt::READ;
    }
    if p.contains(Prot::WRITE) {
        m |= UcProt::WRITE;
    }
    if p.contains(Prot::EXEC) {
        m |= UcProt::EXEC;
    }
    m
}

fn map_arm_reg(r: ArmReg) -> RegisterARM {
    use ArmReg::*;
    match r {
        R0 => RegisterARM::R0,
        R1 => RegisterARM::R1,
        R2 => RegisterARM::R2,
        R3 => RegisterARM::R3,
        R4 => RegisterARM::R4,
        R5 => RegisterARM::R5,
        R6 => RegisterARM::R6,
        R7 => RegisterARM::R7,
        R8 => RegisterARM::R8,
        R9 => RegisterARM::R9,
        R10 => RegisterARM::R10,
        R11 => RegisterARM::R11,
        R12 => RegisterARM::R12,
        Sp => RegisterARM::SP,
        Lr => RegisterARM::LR,
        Pc => RegisterARM::PC,
        Cpsr => RegisterARM::CPSR,
    }
}

fn map_mips_reg(r: ArmReg) -> RegisterMIPS {
    use ArmReg::*;
    match r {
        R0 => RegisterMIPS::A0,
        R1 => RegisterMIPS::A1,
        R2 => RegisterMIPS::A2,
        R3 => RegisterMIPS::A3,
        R4 => RegisterMIPS::S0,
        R5 => RegisterMIPS::S1,
        R6 => RegisterMIPS::S2,
        R7 => RegisterMIPS::S3,
        R8 => RegisterMIPS::S4,
        R9 => RegisterMIPS::S5,
        R10 => RegisterMIPS::S6,
        R11 => RegisterMIPS::S7,
        R12 => RegisterMIPS::GP,
        Sp => RegisterMIPS::SP,
        Lr => RegisterMIPS::RA,
        Pc => RegisterMIPS::PC,
        Cpsr => RegisterMIPS::DSPCARRY,
    }
}

/// Optional per-slice wall-clock watchdog, in microseconds.
///
/// How long a single slice may run before the watchdog interrupts it,
/// in milliseconds. `0` disables it entirely.
///
/// Defaults to [`DEFAULT_SLICE_TIMEOUT_MS`], because a disabled
/// watchdog is not a neutral choice: it means any guest that loops
/// without calling an API freezes the emulator outright, with no way
/// back short of killing the process.
///
/// We deliberately do **not** bound a slice by an instruction *count*:
/// passing a non-zero `count` to `uc_emu_start` makes Unicorn install
/// an internal per-instruction hook that disables QEMU's
/// translation-block chaining, which costs roughly 5-10x throughput on
/// tight guest loops. We also no longer pass `uc_emu_start`'s
/// `timeout`, which spawns a QEMU timer thread on every call -- see the
/// block hook in `new_for_arch`.
///
/// The thunk code hooks already end a slice on every WinCE API call, so
/// the host frame hook and stop requests get a turn on any normal game
/// frame. The watchdog only matters for a guest that loops forever
/// without ever calling an API -- which real games do: Rayman
/// Ultimate's wait-for-button-release loop polls its own pad state and
/// nothing else, and without a preemptive slice boundary the emulator
/// can never regain control to deliver the `WM_KEYUP` that would end
/// it. Set `POCKETHLE_SLICE_TIMEOUT_MS` to override, including to `0`
/// to turn the watchdog off.
/// Parse `POCKETHLE_DUMP_MEM` into a list of guest ranges to dump.
///
/// Format: `ADDR:LEN[=PATH][,ADDR:LEN[=PATH]]...`, addresses in hex
/// with or without `0x`. `PATH` defaults to `dump-<addr>.bin` in the
/// working directory.
///
/// This exists because the interesting code is not always on disk.
/// A loader that decompresses and relocates an image into the heap --
/// Airplay's `.s3e` being the case in point -- leaves you with live
/// addresses and no file to disassemble against. Register traces can
/// walk back through callers one frame at a time, but eventually the
/// question is "what does this branch actually test", and only the
/// bytes answer that.
fn dump_mem_specs() -> &'static [(u64, u64, String)] {
    static CACHED: OnceLock<Vec<(u64, u64, String)>> = OnceLock::new();
    CACHED.get_or_init(|| {
        let Ok(spec) = std::env::var("POCKETHLE_DUMP_MEM") else {
            return Vec::new();
        };
        let parse = |v: &str| -> Option<u64> {
            let v = v.trim();
            match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => v.parse::<u64>().ok(),
            }
        };
        let mut out = Vec::new();
        for entry in spec.split(',') {
            let (range, path) = match entry.split_once('=') {
                Some((r, p)) => (r, p.trim().to_string()),
                None => (entry, String::new()),
            };
            let Some((addr, len)) = range.split_once(':') else {
                log::warn!("POCKETHLE_DUMP_MEM: expected ADDR:LEN in {entry:?}");
                continue;
            };
            let (Some(addr), Some(len)) = (parse(addr), parse(len)) else {
                log::warn!("POCKETHLE_DUMP_MEM: bad address or length in {entry:?}");
                continue;
            };
            let path = if path.is_empty() {
                format!("dump-{addr:08x}.bin")
            } else {
                path
            };
            out.push((addr, len, path));
        }
        out
    })
}

/// Parse `POCKETHLE_WATCH_MEM` into an inclusive guest address range.
///
/// Accepts `0xADDR` (four bytes, the common case for a pointer field)
/// or `0xADDR:LEN`. Returns `None` when unset or unparseable, in which
/// case no hook is installed and there is no cost.
fn watch_mem_range() -> Option<(u64, u64)> {
    static CACHED: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let spec = std::env::var("POCKETHLE_WATCH_MEM").ok()?;
        let spec = spec.trim();
        let (addr, len) = match spec.split_once(':') {
            Some((a, l)) => (a.trim(), l.trim()),
            None => (spec, "4"),
        };
        let parse = |v: &str| -> Option<u64> {
            let v = v.trim();
            match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => v.parse::<u64>().ok(),
            }
        };
        let addr = parse(addr)?;
        let len = parse(len).filter(|l| *l > 0).unwrap_or(4);
        Some((addr, addr + len - 1))
    })
}

/// Parse `POCKETHLE_WATCH_HOST` into an inclusive guest address range.
///
/// Same syntax as `POCKETHLE_WATCH_MEM` (`0xADDR` or `0xADDR:LEN`), but
/// applied to host-side writes through `write_mem` rather than guest
/// stores. Unset means no cost.
fn watch_host_range() -> Option<(u64, u64)> {
    static CACHED: OnceLock<Option<(u64, u64)>> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let spec = std::env::var("POCKETHLE_WATCH_HOST").ok()?;
        let spec = spec.trim();
        let (addr, len) = match spec.split_once(':') {
            Some((a, l)) => (a.trim(), l.trim()),
            None => (spec, "4"),
        };
        let parse = |v: &str| -> Option<u64> {
            let v = v.trim();
            match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => v.parse::<u64>().ok(),
            }
        };
        let addr = parse(addr)?;
        let len = parse(len).filter(|l| *l > 0).unwrap_or(4);
        Some((addr, addr + len - 1))
    })
}

/// Per-run override of the slice watchdog, set by a frontend before it
/// starts a game. `-1` means "not set".
///
/// An `AtomicI64` rather than a `OnceLock` because the desktop launcher
/// runs several games in one process: whatever the first game wanted
/// would otherwise be frozen in for the rest of the session, which is
/// exactly the bug this whole setting exists to avoid.
static SLICE_WATCHDOG_OVERRIDE: AtomicI64 = AtomicI64::new(-1);

/// Set (or with `None`, clear) the slice watchdog for subsequent runs.
///
/// Takes precedence over `POCKETHLE_SLICE_TIMEOUT_MS`, because a
/// per-game setting the user chose deliberately should not be silently
/// beaten by a variable left in their environment months ago. Set it
/// once per launch; it applies until changed.
pub fn set_slice_watchdog_ms(ms: Option<u64>) {
    SLICE_WATCHDOG_OVERRIDE.store(ms.map_or(-1, |v| v as i64), Ordering::Relaxed);
}

fn slice_watchdog_ms() -> u64 {
    let over = SLICE_WATCHDOG_OVERRIDE.load(Ordering::Relaxed);
    if over >= 0 {
        return over as u64;
    }
    static CACHED: OnceLock<u64> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("POCKETHLE_SLICE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_SLICE_TIMEOUT_MS)
    })
}

/// Slice ceiling used when `POCKETHLE_SLICE_TIMEOUT_MS` is unset.
///
/// Roughly one 60 Hz frame. The stalls this exists to break are
/// permanent, so the exact value only trades interruption granularity
/// against how often legitimate long stretches of guest compute get
/// chopped up -- it does not need to be tight. Measured at ~18 FPS on
/// Rayman Ultimate, against ~12 before any of this work.
const DEFAULT_SLICE_TIMEOUT_MS: u64 = 16;

impl Cpu for UnicornCpu {
    fn arch(&self) -> Arch {
        self.arch
    }

    fn map_region(&mut self, va: u32, size: u32, prot: Prot) -> Result<(), CpuError> {
        self.uc
            .mem_map(va as u64, size as u64, map_prot(prot))
            .map_err(|e| CpuError::Backend(format!("mem_map: {e:?}")))?;
        if let Some(map) = &self.guest_map {
            map.borrow_mut()
                .insert(va as u64, size as u64, prot.contains(Prot::EXEC));
            // Pages inside the new region may already have a
            // "fault" verdict cached from a probe, so drop the TLB.
            // Mappings are created at load time and by VirtualAlloc,
            // never on a hot path.
            let _ = self.uc.ctl_flush_tlb();
        }
        Ok(())
    }

    fn write_mem(&mut self, va: u32, data: &[u8]) -> Result<(), CpuError> {
        // Host-side writes bypass the guest CPU entirely, so
        // POCKETHLE_WATCH_MEM cannot see them. This mirrors that watch
        // for writes made by emulator code (patched memcpy, file reads,
        // loader fixups) so a stray host write onto guest memory can be
        // attributed instead of merely observed after the fact.
        if let Some((begin, end)) = watch_host_range() {
            let lo = va as u64;
            let hi = lo + data.len() as u64 - 1;
            if lo <= end && hi >= begin {
                let preview: Vec<String> =
                    data.iter().take(16).map(|b| format!("{b:02x}")).collect();
                log::info!(
                    "WATCH host write 0x{va:08x} len={} [{}{}]\n{}",
                    data.len(),
                    preview.join(" "),
                    if data.len() > 16 { " ..." } else { "" },
                    std::backtrace::Backtrace::force_capture()
                );
            }
        }
        self.uc
            .mem_write(va as u64, data)
            .map_err(|e| CpuError::Backend(format!("mem_write: {e:?}")))
    }

    fn read_mem(&mut self, va: u32, len: u32) -> Result<Vec<u8>, CpuError> {
        let mut out = vec![0u8; len as usize];
        self.uc
            .mem_read(va as u64, &mut out)
            .map_err(|e| CpuError::Backend(format!("mem_read: {e:?}")))?;
        Ok(out)
    }

    fn read_mem_into(&mut self, va: u32, dst: &mut [u8]) -> Result<(), CpuError> {
        // Bypass the default `read_mem` -> Vec allocation: feed
        // unicorn's `mem_read` the caller's buffer directly. Used by
        // the per-frame GAPI flush (~150 KiB).
        self.uc
            .mem_read(va as u64, dst)
            .map_err(|e| CpuError::Backend(format!("mem_read: {e:?}")))
    }

    fn read_reg(&mut self, reg: ArmReg) -> Result<u32, CpuError> {
        if self.arch == Arch::Mips && reg == ArmReg::Cpsr {
            return Ok(self.mips_status);
        }
        let value = match self.arch {
            Arch::Arm => self.uc.reg_read(map_arm_reg(reg)),
            Arch::Mips => self.uc.reg_read(map_mips_reg(reg)),
        };
        value
            .map(|v| v as u32)
            .map_err(|e| CpuError::Backend(format!("reg_read: {e:?}")))
    }

    fn write_reg(&mut self, reg: ArmReg, value: u32) -> Result<(), CpuError> {
        if self.arch == Arch::Mips && reg == ArmReg::Cpsr {
            self.mips_status = value;
            return Ok(());
        }
        let result = match self.arch {
            Arch::Arm => self.uc.reg_write(map_arm_reg(reg), value as u64),
            Arch::Mips => self.uc.reg_write(map_mips_reg(reg), value as u64),
        };
        result.map_err(|e| CpuError::Backend(format!("reg_write: {e:?}")))
    }

    fn read_return(&mut self) -> Result<u32, CpuError> {
        if self.arch == Arch::Mips {
            return self
                .uc
                .reg_read(RegisterMIPS::V0)
                .map(|v| v as u32)
                .map_err(|e| CpuError::Backend(format!("reg_read: {e:?}")));
        }
        self.read_reg(ArmReg::R0)
    }

    fn write_return(&mut self, value: u32) -> Result<(), CpuError> {
        if self.arch == Arch::Mips {
            return self
                .uc
                .reg_write(RegisterMIPS::V0, value as u64)
                .map_err(|e| CpuError::Backend(format!("reg_write: {e:?}")));
        }
        self.write_reg(ArmReg::R0, value)
    }

    fn write_return_pair(&mut self, first: u32, second: u32) -> Result<(), CpuError> {
        if self.arch == Arch::Mips {
            self.uc
                .reg_write(RegisterMIPS::V0, first as u64)
                .map_err(|e| CpuError::Backend(format!("reg_write: {e:?}")))?;
            return self
                .uc
                .reg_write(RegisterMIPS::V1, second as u64)
                .map_err(|e| CpuError::Backend(format!("reg_write: {e:?}")));
        }
        self.write_return(first)?;
        self.write_reg(ArmReg::R1, second)
    }

    fn set_hook_skip_once(&mut self, va: u32) {
        *self.hook_skip_once.borrow_mut() = Some(va);
    }

    fn add_code_hook(&mut self, va: u32) -> Result<(), CpuError> {
        self.add_code_hook_range(va, va)
    }

    fn add_instruction_hook(&mut self, va: u32) -> Result<(), CpuError> {
        self.add_code_hook(va)
    }

    fn add_code_hook_range(&mut self, lo: u32, hi: u32) -> Result<(), CpuError> {
        let last = self.last_hook.clone();
        let stop = self.stop_requested.clone();
        let skip = self.hook_skip_once.clone();
        // Report the address that actually trapped, not the range's
        // bounds: with one hook covering a whole run of thunk slots
        // the run loop identifies the import by the trapping PC.
        let cb = move |uc: &mut Unicorn<'_, ()>, addr: u64, _size: u32| {
            // One-shot suppression, so a tracer can log this address
            // and let the instruction execute on re-entry.
            if *skip.borrow() == Some(addr as u32) {
                *skip.borrow_mut() = None;
                return;
            }
            *last.borrow_mut() = Some(addr as u32);
            *stop.borrow_mut() = true;
            let _ = uc.emu_stop();
        };
        self.uc
            .add_code_hook(lo as u64, hi as u64, cb)
            .map(|_| ())
            .map_err(|e| CpuError::Backend(format!("add_code_hook: {e:?}")))
    }

    fn run_until_hook(
        &mut self,
        start_va: u32,
        _max_instructions: u64,
    ) -> Result<StopReason, CpuError> {
        self.dump_requested_memory();
        *self.last_hook.borrow_mut() = None;
        *self.stop_requested.borrow_mut() = false;
        self.slice_expired.set(false);
        let watchdog_ms = slice_watchdog_ms();
        self.slice_deadline.set(if watchdog_ms > 0 {
            Some(Instant::now() + Duration::from_millis(watchdog_ms))
        } else {
            None
        });
        // IMPORTANT: run with `count = 0` (no instruction limit) so the
        // QEMU TCG keeps chaining translation blocks at full speed. A
        // non-zero `count` would silently install a per-instruction
        // counting hook and tank throughput ~5-10x. Slices are instead
        // ended by the IAT-thunk code hooks (which call `emu_stop` on
        // every emulated API call) and, optionally, by a wall-clock
        // watchdog for pathological API-free loops.
        // `start_va` still carries its ARM/Thumb interworking bit in
        // bit 0, and that is deliberate: unicorn latches the execution
        // state from the `PC` write that `uc_emu_start` performs
        // (`env->thumb = value & 1` in `unicorn_arm.c`), so an odd
        // address is how Thumb gets selected. Do not mask it here, and
        // do not try to drive the mode from `CPSR` instead -- that
        // route does not survive the round trip.
        let r = self.uc.emu_start(
            start_va as u64,
            0, // until = 0 → run until stopped
            0, // timeout = 0 → never spawn a per-call QEMU timer thread
            0, // count = 0 → keep TB chaining (do NOT pass a limit)
        );
        self.slice_deadline.set(None);
        if let Some(addr) = *self.last_hook.borrow() {
            return Ok(StopReason::Hook(addr));
        }
        match r {
            // No hook fired: either an explicit stop was requested from
            // another thread/hook, or the watchdog timeout elapsed.
            // Both are benign slice boundaries — the caller refreshes
            // state and resumes from the current PC.
            Ok(()) => {
                if self.slice_expired.get() {
                    // The watchdog cut the slice short. Reported as
                    // `InstructionLimit` because that is what the
                    // caller already treats as "a slice ended without
                    // the guest calling anything", which is exactly
                    // what happened.
                    Ok(StopReason::InstructionLimit)
                } else if *self.stop_requested.borrow() {
                    Ok(StopReason::Requested)
                } else {
                    Ok(StopReason::InstructionLimit)
                }
            }
            Err(e) => {
                if let Some((kind, addr)) = self.last_fault.borrow().clone() {
                    Err(CpuError::Backend(format!(
                        "emu_start: {e:?} ({kind}) at guest address 0x{addr:08x}"
                    )))
                } else {
                    Err(CpuError::Backend(format!("emu_start: {e:?}")))
                }
            }
        }
    }

    fn request_stop(&mut self) {
        *self.stop_requested.borrow_mut() = true;
        let _ = self.uc.emu_stop();
    }
}
