//! `unicorn-engine`-backed CPU.
//!
//! Compiled only with `--features unicorn`. Apart from the build cost,
//! this is the authoritative ARM backend used at runtime.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use ::unicorn_engine::unicorn_const::{
    uc_error as UcError, Arch as UcArch, MemType, Mode, Prot as UcProt, TlbEntry, TlbType,
};
use ::unicorn_engine::{RegisterARM, RegisterMIPS, UcHookId, Unicorn};

use crate::{regs::ArmReg, Arch, Cpu, CpuError, Prot, StopReason};

const GUEST_PAGE_SIZE: u64 = 0x1000;

#[path = "slice_timer.rs"]
mod slice_timer;

/// Which guest pages exist, and which of them the CPU is allowed to
/// fetch instructions from.
///
/// Shared with the virtual-TLB fill hook installed by
/// [`UnicornCpu::new_for_arch`], which is the only thing that answers
/// "may this page be read / written / executed" once we take Unicorn
/// off its architectural page-table walk.
#[derive(Default)]
struct GuestMap {
    /// `(start, end_exclusive, protection)` for every mapped region.
    regions: Vec<(u64, u64, Prot)>,
    /// Pages promoted to executable at run time because the guest was
    /// caught fetching from them even though their mapping is not
    /// executable, kept sorted.
    promoted_exec: Vec<u64>,
    /// Explicit VirtualAlloc permissions take precedence over compatibility
    /// promotion of old heap/code arenas. Latest range overrides earlier ones.
    protected: Vec<(u64, u64, Prot)>,
}

impl GuestMap {
    fn insert(&mut self, start: u64, len: u64, prot: Prot) {
        let entry = (start, start.saturating_add(len), prot);
        let at = self.regions.partition_point(|r| r.0 < start);
        self.regions.insert(at, entry);
    }

    fn remove(&mut self, start: u64, len: u64) {
        let end = start + len;
        self.regions = self
            .regions
            .iter()
            .flat_map(|&(lo, hi, prot)| {
                let mut parts = Vec::new();
                if hi <= start || lo >= end {
                    parts.push((lo, hi, prot));
                } else {
                    if lo < start {
                        parts.push((lo, start, prot));
                    }
                    if hi > end {
                        parts.push((end, hi, prot));
                    }
                }
                parts
            })
            .collect();
        self.protected = self
            .protected
            .iter()
            .flat_map(|&(lo, hi, prot)| {
                let mut parts = Vec::new();
                if hi <= start || lo >= end {
                    parts.push((lo, hi, prot));
                } else {
                    if lo < start {
                        parts.push((lo, start, prot));
                    }
                    if hi > end {
                        parts.push((end, hi, prot));
                    }
                }
                parts
            })
            .collect();
        self.promoted_exec
            .retain(|&page| page < start || page >= end);
    }
    fn region_for(&self, addr: u64) -> Option<Prot> {
        let at = self.regions.partition_point(|r| r.0 <= addr);
        self.regions[..at]
            .iter()
            .rev()
            .find(|r| addr < r.1)
            .map(|r| r.2)
    }

    fn effective_prot(&self, addr: u64) -> Option<Prot> {
        self.protected
            .iter()
            .rev()
            .find(|&&(start, end, _)| addr >= start && addr < end)
            .map(|&(_, _, prot)| prot)
            .or_else(|| self.region_for(addr))
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
    // Field drop order matters: join the stop timer before closing Unicorn.
    scheduler_timer: Option<slice_timer::SliceTimer>,
    uc: Unicorn<'static, ()>,
    last_hook: Rc<RefCell<Option<u32>>>,
    /// Address of the last invalid memory access, recorded so crash
    /// reports can name the faulting address instead of just the
    /// access kind.
    last_fault: Rc<RefCell<Option<(String, u64)>>>,
    stop_requested: Rc<RefCell<bool>>,
    /// Mapping table backing the virtual-TLB hook; `None` when the
    /// architectural TLB is in use and Unicorn resolves pages itself.
    guest_map: Option<Rc<RefCell<GuestMap>>>,
    memory_map: Rc<RefCell<GuestMap>>,
    arch: Arch,
    mips_status: u32,
    code_hooks: Vec<(u32, u32, UcHookId)>,
    inline_arithmetic: std::collections::HashMap<u32, crate::arithmetic64::Arithmetic64>,
    images: Rc<RefCell<crate::image_pages::ImagePages>>,
    scheduler_timeout_us: u64,
}

impl UnicornCpu {
    fn promote_exec_page(&mut self, page: u64, prot: Prot) -> Result<(), CpuError> {
        if prot.contains(Prot::EXEC) {
            return Ok(());
        }
        materialize_images(
            &mut self.uc,
            &self.images,
            page as u32,
            GUEST_PAGE_SIZE as u32,
        )?;
        self.uc
            .mem_protect(page, GUEST_PAGE_SIZE, map_prot(prot | Prot::EXEC))
            .map_err(|error| CpuError::Backend(format!("mem_protect: {error:?}")))?;
        self.memory_map.borrow_mut().promote_exec(page);
        Ok(())
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
        let images = Rc::new(RefCell::new(crate::image_pages::ImagePages::default()));
        let memory_map = Rc::new(RefCell::new(GuestMap::default()));
        let guest_map = install_virtual_tlb(&mut uc, &last_fault, &images, memory_map.clone());
        if guest_map.is_none() {
            // Architectural TLB: Unicorn resolves pages itself, so the
            // only way to learn the faulting address is the
            // invalid-access hook.
            let sink = last_fault.clone();
            let pending = images.clone();
            let _ = uc.add_mem_hook(
                ::unicorn_engine::unicorn_const::HookType::MEM_INVALID,
                0,
                u64::MAX,
                move |uc, kind, addr, size, _value| {
                    if matches!(
                        kind,
                        MemType::READ_UNMAPPED | MemType::WRITE_UNMAPPED | MemType::FETCH_UNMAPPED
                    ) {
                        match materialize_images(uc, &pending, addr as u32, size as u32) {
                            Ok(true) => return true,
                            Err(error) => {
                                *sink.borrow_mut() = Some((error.to_string(), addr));
                                return false;
                            }
                            Ok(false) => {}
                        }
                    }
                    *sink.borrow_mut() = Some((format!("{kind:?} size={size}"), addr));
                    false
                },
            );
        }
        Ok(Self {
            uc,
            arch,
            last_fault,
            guest_map,
            memory_map,
            last_hook: Rc::new(RefCell::new(None)),
            stop_requested: Rc::new(RefCell::new(false)),
            mips_status: 0,
            code_hooks: Vec::new(),
            inline_arithmetic: std::collections::HashMap::new(),
            images,
            scheduler_timeout_us: 0,
            scheduler_timer: None,
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
    images: &Rc<RefCell<crate::image_pages::ImagePages>>,
    map: Rc<RefCell<GuestMap>>,
) -> Option<Rc<RefCell<GuestMap>>> {
    if std::env::var_os("POCKETHLE_CPU_TLB").is_some() {
        return None;
    }
    if uc.ctl_set_tlb_type(TlbType::VIRTUAL).is_err() {
        return None;
    }
    let regions = map.clone();
    let sink = last_fault.clone();
    // `begin > end` is Unicorn's "every address" bound check. The
    // address handed to the callback is already page-aligned.
    let pending = images.clone();
    let installed = uc.add_tlb_hook(1, 0, move |uc, page, kind| {
        if let Err(error) = materialize_images(uc, &pending, page as u32, 1) {
            *sink.borrow_mut() = Some((error.to_string(), page));
            return None;
        }
        let mut map = regions.borrow_mut();
        let Some(region_prot) = map.region_for(page) else {
            *sink.borrow_mut() = Some((format!("{kind:?} unmapped"), page));
            return None;
        };
        if let Some(&(_, _, prot)) = map
            .protected
            .iter()
            .rev()
            .find(|&&(start, end, _)| page >= start && page < end)
        {
            let required = match kind {
                MemType::FETCH => Prot::EXEC,
                MemType::WRITE => Prot::WRITE,
                _ => Prot::READ,
            };
            if !prot.contains(required) {
                *sink.borrow_mut() = Some((format!("{kind:?} protected"), page));
                return None;
            }
            return Some(TlbEntry {
                paddr: page,
                perms: map_prot(prot),
            });
        }
        // Report read/write for anything mapped, matching what the
        // MMU-disabled ARM walk used to grant: Unicorn still enforces
        // the region's own `UC_PROT_*` bits in its store/load helper,
        // and a page we hand back as non-executable stays on the slow
        // helper path anyway whenever it is genuinely read-only.
        let mut perms = UcProt::READ | UcProt::WRITE;
        if region_prot.contains(Prot::EXEC) || map.is_promoted_exec(page) {
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

/// Called only by a TLB miss, an invalid access, or an HLE host access.
/// No per-instruction/read/write hook is installed on the normal fast path.
fn materialize_images(
    uc: &mut Unicorn<'_, ()>,
    images: &Rc<RefCell<crate::image_pages::ImagePages>>,
    va: u32,
    len: u32,
) -> Result<bool, CpuError> {
    let mut found = false;
    for address in crate::image_pages::ImagePages::range(va, len)? {
        // Unicorn's host mem_write can itself refill the virtual TLB. Never
        // keep a RefCell borrow across a backend call that may reenter a hook.
        let image = {
            let mut pending = images.borrow_mut();
            if pending
                .pages
                .get(&address)
                .is_some_and(|image| image.resident)
            {
                found = true;
                continue;
            }
            pending.pages.remove(&address)
        };
        let Some(mut image) = image else {
            continue;
        };
        found = true;
        if !image.budget.acquire() {
            images.borrow_mut().pages.insert(address, image);
            return Err(CpuError::ImageOutOfMemory { va: address });
        }
        let result = uc.mem_map(address as u64, 4096, map_prot(image.prot));
        if let Err(error) = result {
            image.budget.release();
            images.borrow_mut().pages.insert(address, image);
            return Err(CpuError::Backend(format!(
                "image page-in mem_map: {error:?}"
            )));
        }
        if !image.bytes.is_empty() {
            if let Err(error) = uc.mem_write(address as u64, &image.bytes) {
                let _ = uc.mem_unmap(address as u64, 4096);
                image.budget.release();
                images.borrow_mut().pages.insert(address, image);
                return Err(CpuError::Backend(format!(
                    "image page-in mem_write: {error:?}"
                )));
            }
        }
        image.resident = true;
        image.bytes = Vec::new();
        images.borrow_mut().pages.insert(address, image);
    }
    Ok(found)
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
/// Returns `0` (disabled) by default. We deliberately do **not** bound
/// a slice by an instruction *count*: passing a non-zero `count` to
/// `uc_emu_start` makes Unicorn install an internal per-instruction
/// hook that disables QEMU's translation-block chaining, which costs
/// roughly 5-10x throughput on tight guest loops (this is exactly why
/// the JIT microbenchmark — which calls `emu_start(.., 0, 0)` — runs
/// far faster than real games used to). The thunk code hooks already
/// stop emulation on every WinCE API call, so the host frame hook and
/// stop requests still get a turn on any normal game frame. The
/// watchdog is only a safety net for a pathological guest that loops
/// forever without ever calling an API; set
/// `POCKETHLE_SLICE_TIMEOUT_MS` to enable it.
fn slice_watchdog_us() -> u64 {
    static CACHED: OnceLock<u64> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("POCKETHLE_SLICE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|ms| ms.saturating_mul(1000))
            .unwrap_or(0)
    })
}

impl UnicornCpu {
    fn promote_fetch_page(&mut self) -> Result<bool, CpuError> {
        if self.arch != Arch::Arm {
            return Ok(false);
        }
        let pc = self.read_reg(ArmReg::Pc)?;
        let page = u64::from(pc) & !(GUEST_PAGE_SIZE - 1);
        let prot = {
            let map = self.memory_map.borrow();
            let Some(prot) = map.effective_prot(page) else {
                return Ok(false);
            };
            let protected = map
                .protected
                .iter()
                .rev()
                .any(|&(start, end, _)| page >= start && page < end);
            if prot.contains(Prot::EXEC) || protected {
                return Ok(false);
            }
            prot
        };
        self.promote_exec_page(page, prot)?;
        self.uc
            .ctl_remove_cache(page, page + GUEST_PAGE_SIZE)
            .map_err(|error| CpuError::Backend(format!("ctl_remove_cache: {error:?}")))?;
        self.uc
            .ctl_flush_tlb()
            .map_err(|error| CpuError::Backend(format!("ctl_flush_tlb: {error:?}")))?;
        Ok(true)
    }

    fn run_once(
        &mut self,
        start_va: u32,
        scheduler_us: u64,
        watchdog_us: u64,
        retry_fetch_fault: bool,
    ) -> Result<StopReason, CpuError> {
        *self.last_hook.borrow_mut() = None;
        *self.last_fault.borrow_mut() = None;
        *self.stop_requested.borrow_mut() = false;
        // IMPORTANT: run with `count = 0` (no instruction limit) so the
        // QEMU TCG keeps chaining translation blocks at full speed. A
        // non-zero `count` would silently install a per-instruction
        // counting hook and tank throughput ~5-10x. Slices are instead
        // ended by the IAT-thunk code hooks (which call `emu_stop` on
        // every emulated API call) and the reusable scheduler timer for
        // API-free loops. A separate diagnostic watchdog remains opt-in.
        if scheduler_us != 0 {
            if let Some(timer) = &self.scheduler_timer {
                timer.arm(scheduler_us);
            }
        }
        let r = self.uc.emu_start(
            start_va as u64,
            0,           // until = 0 → run until stopped
            watchdog_us, // remaining explicit diagnostic watchdog
            0,           // count = 0 → keep TB chaining (do NOT pass a limit)
        );
        if let Some(timer) = &self.scheduler_timer {
            timer.disarm();
        }
        if let Some(addr) = *self.last_hook.borrow() {
            return Ok(StopReason::Hook(addr));
        }
        match r {
            // No hook fired: either an explicit stop was requested from
            // another thread/hook, or the watchdog timeout elapsed.
            // Both are benign slice boundaries — the caller refreshes
            // state and resumes from the current PC.
            Ok(()) => {
                if *self.stop_requested.borrow() {
                    Ok(StopReason::Requested)
                } else {
                    Ok(StopReason::InstructionLimit)
                }
            }
            Err(e) => {
                if e == UcError::FETCH_PROT
                    && retry_fetch_fault
                    && self.last_fault.borrow().is_none()
                    && self.promote_fetch_page()?
                {
                    let pc = self.read_reg(ArmReg::Pc)?;
                    return self.run_once(pc, scheduler_us, watchdog_us, false);
                }
                let fault = self.last_fault.borrow().clone();
                if let Some((kind, addr)) = fault {
                    Err(CpuError::Backend(format!(
                        "emu_start: {e:?} ({kind}) at guest address 0x{addr:08x}; pc=0x{:08x} lr=0x{:08x} sp=0x{:08x} r0=0x{:08x} r1=0x{:08x} r2=0x{:08x} r3=0x{:08x}",
                        self.read_reg(ArmReg::Pc).unwrap_or(0),
                        self.read_reg(ArmReg::Lr).unwrap_or(0),
                        self.read_reg(ArmReg::Sp).unwrap_or(0),
                        self.read_reg(ArmReg::R0).unwrap_or(0),
                        self.read_reg(ArmReg::R1).unwrap_or(0),
                        self.read_reg(ArmReg::R2).unwrap_or(0),
                        self.read_reg(ArmReg::R3).unwrap_or(0),
                    )))
                } else {
                    Err(CpuError::Backend(format!("emu_start: {e:?}")))
                }
            }
        }
    }
}

impl Cpu for UnicornCpu {
    fn arch(&self) -> Arch {
        self.arch
    }

    fn install_inline_arithmetic64(
        &mut self,
        va: u32,
        operation: crate::arithmetic64::Arithmetic64,
    ) -> Result<bool, CpuError> {
        if self.arch != Arch::Arm {
            return Ok(false);
        }
        self.add_code_hook(va)?;
        self.inline_arithmetic.insert(va, operation);
        Ok(true)
    }

    fn supports_image_paging(&self) -> bool {
        self.arch == Arch::Arm
    }

    fn map_image_region(
        &mut self,
        va: u32,
        size: u32,
        prot: Prot,
        bytes: Vec<u8>,
        budget: std::sync::Arc<dyn crate::image_pages::ImagePageBudget>,
    ) -> Result<bool, CpuError> {
        if self.arch != Arch::Arm {
            self.map_region(va, size, prot)?;
            self.write_mem(va, &bytes)?;
            return Ok(false);
        }
        let end = u64::from(va) + u64::from(size);
        self.inline_arithmetic
            .retain(|address, _| *address < va || u64::from(*address) >= end);
        let regions = self
            .uc
            .mem_regions()
            .map_err(|e| CpuError::Backend(format!("mem_regions: {e:?}")))?;
        if regions
            .iter()
            .any(|region| region.begin < end && region.end >= u64::from(va))
        {
            return Err(CpuError::BadMemory { va, size });
        }
        self.images
            .borrow_mut()
            .reserve(va, size, prot, bytes, budget)?;
        self.memory_map
            .borrow_mut()
            .insert(va as u64, size as u64, prot);
        if self.guest_map.is_some() {
            let _ = self.uc.ctl_flush_tlb();
        }
        Ok(true)
    }

    fn map_region(&mut self, va: u32, size: u32, prot: Prot) -> Result<(), CpuError> {
        if self
            .images
            .borrow()
            .pages
            .range(va..va.saturating_add(size))
            .next()
            .is_some()
        {
            return Err(CpuError::BadMemory { va, size });
        }
        self.uc
            .mem_map(va as u64, size as u64, map_prot(prot))
            .map_err(|e| CpuError::Backend(format!("mem_map: {e:?}")))?;
        self.memory_map
            .borrow_mut()
            .insert(va as u64, size as u64, prot);
        if self.guest_map.is_some() {
            let _ = self.uc.ctl_flush_tlb();
        }
        Ok(())
    }

    fn unmap_region(&mut self, va: u32, size: u32) -> Result<(), CpuError> {
        let end_va = va
            .checked_add(size)
            .ok_or(CpuError::BadMemory { va, size })?;
        let lazy = self
            .images
            .borrow()
            .pages
            .range(va..end_va)
            .next()
            .is_some();
        if lazy {
            if va % 4096 != 0
                || size % 4096 != 0
                || (va..end_va)
                    .step_by(4096)
                    .any(|page| !self.images.borrow().pages.contains_key(&page))
            {
                return Err(CpuError::BadMemory { va, size });
            }
            for page in (va..end_va).step_by(4096) {
                let image = self.images.borrow_mut().pages.remove(&page).unwrap();
                if image.resident {
                    if let Err(error) = self.uc.mem_unmap(page as u64, 4096) {
                        self.images.borrow_mut().pages.insert(page, image);
                        return Err(CpuError::Backend(format!("mem_unmap image: {error:?}")));
                    }
                }
                // Drop refunds a committed page; cold backing has no RAM charge.
            }
        } else {
            self.uc
                .mem_unmap(va as u64, size as u64)
                .map_err(|e| CpuError::Backend(format!("mem_unmap: {e:?}")))?;
        }
        self.memory_map.borrow_mut().remove(va as u64, size as u64);
        let end = u64::from(va) + u64::from(size);
        let mut index = 0;
        while index < self.code_hooks.len() {
            let (lo, hi, id) = self.code_hooks[index];
            if lo >= va && u64::from(hi) < end {
                self.uc
                    .remove_hook(id)
                    .map_err(|e| CpuError::Backend(format!("remove_hook: {e:?}")))?;
                self.code_hooks.remove(index);
            } else {
                index += 1;
            }
        }
        let _ = self.uc.ctl_remove_cache(u64::from(va), end - 1);
        let _ = self.uc.ctl_flush_tlb();
        Ok(())
    }
    fn protect_region(&mut self, va: u32, size: u32, prot: Prot) -> Result<(), CpuError> {
        materialize_images(&mut self.uc, &self.images, va, size)?;
        self.uc
            .mem_protect(va as u64, size as u64, map_prot(prot))
            .map_err(|error| CpuError::Backend(format!("mem_protect: {error:?}")))?;
        self.memory_map
            .borrow_mut()
            .protected
            .push((va as u64, va as u64 + size as u64, prot));
        let _ = self.uc.ctl_flush_tlb();
        Ok(())
    }

    fn check_guest_access(&self, va: u32, len: u32, required: Prot) -> Result<(), CpuError> {
        let regions = self
            .uc
            .mem_regions()
            .map_err(|e| CpuError::Backend(format!("mem_regions: {e:?}")))?;
        // Bindgen's C enum underlying type differs between Windows and Unix.
        let images = self.images.borrow();
        let permissions = map_prot(required).0 as u32;
        for page in crate::image_pages::ImagePages::range(va, len)? {
            let allowed = if let Some(r) = regions
                .iter()
                .find(|r| r.begin <= page as u64 && r.end >= page as u64)
            {
                (r.perms as u32) & permissions == permissions
            } else {
                images
                    .pages
                    .get(&page)
                    .is_some_and(|p| !p.resident && p.prot.contains(required))
            };
            if !allowed {
                return Err(CpuError::BadMemory { va, size: len });
            }
        }
        Ok(())
    }
    fn write_mem(&mut self, va: u32, data: &[u8]) -> Result<(), CpuError> {
        materialize_images(&mut self.uc, &self.images, va, data.len() as u32)?;
        self.uc
            .mem_write(va as u64, data)
            .map_err(|e| CpuError::Backend(format!("mem_write: {e:?}")))
    }

    fn read_mem(&mut self, va: u32, len: u32) -> Result<Vec<u8>, CpuError> {
        materialize_images(&mut self.uc, &self.images, va, len)?;
        let mut out = vec![0u8; len as usize];
        self.uc
            .mem_read(va as u64, &mut out)
            .map_err(|e| CpuError::Backend(format!("mem_read: {e:?}")))?;
        Ok(out)
    }

    fn flush_instruction_cache(&mut self, va: u32, size: u32) -> Result<(), CpuError> {
        if va == 0 || self.arch != Arch::Arm {
            self.uc
                .ctl_flush_tb()
                .map_err(|error| CpuError::Backend(format!("ctl_flush_tb: {error:?}")))?;
            self.uc
                .ctl_flush_tlb()
                .map_err(|error| CpuError::Backend(format!("ctl_flush_tlb: {error:?}")))?;
            return Ok(());
        }
        if size == 0 {
            return Ok(());
        }
        let start = u64::from(va);
        let end = start
            .checked_add(u64::from(size))
            .filter(|end| *end <= u64::from(u32::MAX) + 1)
            .ok_or(CpuError::BadMemory { va, size })?;
        let first_page = start & !(GUEST_PAGE_SIZE - 1);
        let page_end = end
            .checked_add(GUEST_PAGE_SIZE - 1)
            .ok_or(CpuError::BadMemory { va, size })?
            & !(GUEST_PAGE_SIZE - 1);
        let pages = {
            let map = self.memory_map.borrow();
            let mut pages = Vec::new();
            let mut page = first_page;
            while page < page_end {
                let prot = map.effective_prot(page).ok_or(CpuError::BadMemory {
                    va: page as u32,
                    size: GUEST_PAGE_SIZE as u32,
                })?;
                pages.push((page, prot));
                page += GUEST_PAGE_SIZE;
            }
            pages
        };
        for (page, prot) in pages {
            self.promote_exec_page(page, prot)?;
        }
        self.uc
            .ctl_remove_cache(start, end)
            .map_err(|error| CpuError::Backend(format!("ctl_remove_cache: {error:?}")))?;
        self.uc
            .ctl_flush_tlb()
            .map_err(|error| CpuError::Backend(format!("ctl_flush_tlb: {error:?}")))?;
        Ok(())
    }

    fn read_mem_into(&mut self, va: u32, dst: &mut [u8]) -> Result<(), CpuError> {
        materialize_images(&mut self.uc, &self.images, va, dst.len() as u32)?;
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

    fn read_fpscr(&mut self) -> Result<u32, CpuError> {
        if self.arch != Arch::Arm {
            return Ok(0);
        }
        self.uc
            .reg_read(RegisterARM::FPSCR)
            .map(|v| v as u32)
            .map_err(|e| CpuError::Backend(format!("FPSCR read: {e:?}")))
    }
    fn write_fpscr(&mut self, value: u32) -> Result<(), CpuError> {
        if self.arch != Arch::Arm {
            return Ok(());
        }
        self.uc
            .reg_write(RegisterARM::FPSCR, u64::from(value))
            .map_err(|e| CpuError::Backend(format!("FPSCR write: {e:?}")))
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

    fn add_code_hook(&mut self, va: u32) -> Result<(), CpuError> {
        self.add_code_hook_range(va, va)
    }

    fn add_register_trace(&mut self, va: u32, label: &'static str) -> Result<(), CpuError> {
        if self.arch != Arch::Arm {
            return Err(CpuError::Unsupported("ARM register trace"));
        }
        let cb = move |uc: &mut Unicorn<'_, ()>, addr: u64, _size: u32| {
            let r0 = uc.reg_read(RegisterARM::R0).unwrap_or(0);
            let r1 = uc.reg_read(RegisterARM::R1).unwrap_or(0);
            let r4 = uc.reg_read(RegisterARM::R4).unwrap_or(0);
            let r6 = uc.reg_read(RegisterARM::R6).unwrap_or(0);
            // Warning level keeps an explicitly installed diagnostic visible
            // with the Android launcher's default log verbosity.
            log::warn!("ARM trace {label} pc=0x{addr:08x} r0=0x{r0:08x} r1=0x{r1:08x} r4=0x{r4:08x} r6=0x{r6:08x}");
        };
        let id = self
            .uc
            .add_code_hook(va as u64, va as u64, cb)
            .map_err(|e| CpuError::Backend(format!("register trace: {e:?}")))?;
        self.code_hooks.push((va, va, id));
        Ok(())
    }

    fn add_instruction_hook(&mut self, va: u32) -> Result<(), CpuError> {
        self.add_code_hook(va)
    }

    fn add_code_hook_range(&mut self, lo: u32, hi: u32) -> Result<(), CpuError> {
        let last = self.last_hook.clone();
        let stop = self.stop_requested.clone();
        // Report the address that actually trapped, not the range's
        // bounds: with one hook covering a whole run of thunk slots
        // the run loop identifies the import by the trapping PC.
        let cb = move |uc: &mut Unicorn<'_, ()>, addr: u64, _size: u32| {
            *last.borrow_mut() = Some(addr as u32);
            *stop.borrow_mut() = true;
            let _ = uc.emu_stop();
        };
        let id = self
            .uc
            .add_code_hook(lo as u64, hi as u64, cb)
            .map_err(|e| CpuError::Backend(format!("add_code_hook: {e:?}")))?;
        self.code_hooks.push((lo, hi, id));
        Ok(())
    }

    fn set_slice_timeout_us(&mut self, timeout_us: u64) -> Result<(), CpuError> {
        if timeout_us == self.scheduler_timeout_us {
            return Ok(());
        }
        if timeout_us != 0 && self.scheduler_timer.is_none() {
            self.scheduler_timer = Some(
                slice_timer::SliceTimer::new(self.uc.get_handle() as usize)
                    .map_err(|e| CpuError::Backend(format!("CPU slice timer: {e}")))?,
            );
        }
        self.scheduler_timeout_us = timeout_us;
        if let Some(timer) = &self.scheduler_timer {
            timer.set_enabled(timeout_us != 0);
        }
        Ok(())
    }

    fn run_until_hook(
        &mut self,
        start_va: u32,
        _max_instructions: u64,
    ) -> Result<StopReason, CpuError> {
        if self.inline_arithmetic.is_empty() {
            return self.run_once(
                start_va,
                self.scheduler_timeout_us,
                slice_watchdog_us(),
                true,
            );
        }
        let started = std::time::Instant::now();
        let scheduler = self.scheduler_timeout_us;
        let watchdog = slice_watchdog_us();
        let mut current = start_va;
        let mut consumed = false;
        loop {
            let elapsed = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            if consumed
                && ((scheduler != 0 && elapsed >= scheduler)
                    || (watchdog != 0 && elapsed >= watchdog))
            {
                return Ok(StopReason::InstructionLimit);
            }
            let remaining = |limit: u64| {
                if limit == 0 {
                    0
                } else {
                    limit.saturating_sub(elapsed).max(1)
                }
            };
            let stop = self.run_once(current, remaining(scheduler), remaining(watchdog), true)?;
            let StopReason::Hook(address) = stop else {
                return Ok(stop);
            };
            let Some(operation) = self.inline_arithmetic.get(&address).copied() else {
                return Ok(stop);
            };
            // Read/write registers only while emulation is stopped. Pure helpers
            // stay inside this CPU slice, retaining one fixed wall-clock deadline.
            let n = u64::from(self.read_reg(ArmReg::R0)?)
                | (u64::from(self.read_reg(ArmReg::R1)?) << 32);
            let d = u64::from(self.read_reg(ArmReg::R2)?)
                | (u64::from(self.read_reg(ArmReg::R3)?) << 32);
            let value = operation.evaluate(n, d);
            self.write_return_pair(value as u32, (value >> 32) as u32)?;
            // The padded next word is bx lr. Execute it natively so Thumb returns
            // and an interrupt between result publication and return stay exact.
            current = address + 4;
            self.write_reg(ArmReg::Pc, current)?;
            consumed = true;
        }
    }

    fn request_stop(&mut self) {
        *self.stop_requested.borrow_mut() = true;
        let _ = self.uc.emu_stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_arithmetic64_matches_wide_integer_oracle_without_kernel_dispatch_or_clobbering() {
        use crate::arithmetic64::Arithmetic64;
        let mut cpu = UnicornCpu::new_for_arch(Arch::Arm).unwrap();
        cpu.map_region(0x1000, 0x1000, Prot::ALL).unwrap();
        cpu.write_mem(0x1000, &0xe12fff1eu32.to_le_bytes()).unwrap();
        cpu.add_code_hook(0x1100).unwrap();
        let operations = [
            Arithmetic64::SignedDivide,
            Arithmetic64::UnsignedDivide,
            Arithmetic64::SignedRemainder,
            Arithmetic64::UnsignedRemainder,
        ];
        let edges = [
            0,
            1,
            2,
            u32::MAX as u64,
            1u64 << 32,
            i64::MAX as u64,
            i64::MIN as u64,
            u64::MAX - 1,
            u64::MAX,
        ];
        let mut cases: Vec<_> = edges
            .iter()
            .flat_map(|&a| edges.iter().map(move |&b| (a, b)))
            .collect();
        let mut seed = 0xa3d7b9c1e2f40567u64;
        for _ in 0..2000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let a = seed;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            cases.push((a, seed));
        }
        for (index, operation) in operations.into_iter().enumerate() {
            let thunk = 0x1200 + index as u32 * 32;
            cpu.write_mem(thunk, &[0x1e, 0xff, 0x2f, 0xe1, 0x1e, 0xff, 0x2f, 0xe1])
                .unwrap();
            assert!(cpu.install_inline_arithmetic64(thunk, operation).unwrap());
            for &(n, d) in &cases {
                cpu.write_reg(ArmReg::R0, n as u32).unwrap();
                cpu.write_reg(ArmReg::R1, (n >> 32) as u32).unwrap();
                cpu.write_reg(ArmReg::R2, d as u32).unwrap();
                cpu.write_reg(ArmReg::R3, (d >> 32) as u32).unwrap();
                cpu.write_reg(ArmReg::R4, 0xdeadbeef).unwrap();
                cpu.write_reg(ArmReg::R12, 0xcafebabe).unwrap();
                cpu.write_reg(ArmReg::Sp, 0x1f00).unwrap();
                cpu.write_reg(ArmReg::Lr, 0x1100).unwrap();
                let flags = (cpu.read_reg(ArmReg::Cpsr).unwrap() & 0x0fffffff) | 0xa0000000;
                cpu.write_reg(ArmReg::Cpsr, flags).unwrap();
                cpu.write_fpscr(0x02000000).unwrap();
                assert_eq!(
                    cpu.run_until_hook(thunk, 100).unwrap(),
                    StopReason::Hook(0x1100)
                );
                let actual = u64::from(cpu.read_reg(ArmReg::R0).unwrap())
                    | (u64::from(cpu.read_reg(ArmReg::R1).unwrap()) << 32);
                let expected = if d == 0 {
                    0
                } else {
                    match operation {
                        Arithmetic64::SignedDivide => {
                            ((n as i64 as i128) / (d as i64 as i128)) as u64
                        }
                        Arithmetic64::SignedRemainder => {
                            ((n as i64 as i128) % (d as i64 as i128)) as u64
                        }
                        Arithmetic64::UnsignedDivide => (u128::from(n) / u128::from(d)) as u64,
                        Arithmetic64::UnsignedRemainder => (u128::from(n) % u128::from(d)) as u64,
                    }
                };
                assert_eq!(actual, expected, "{operation:?} {n:#x} {d:#x}");
                assert_eq!(cpu.read_reg(ArmReg::R2).unwrap(), d as u32);
                assert_eq!(cpu.read_reg(ArmReg::R3).unwrap(), (d >> 32) as u32);
                assert_eq!(cpu.read_reg(ArmReg::R4).unwrap(), 0xdeadbeef);
                assert_eq!(cpu.read_reg(ArmReg::R12).unwrap(), 0xcafebabe);
                assert_eq!(cpu.read_reg(ArmReg::Sp).unwrap(), 0x1f00);
                assert_eq!(cpu.read_reg(ArmReg::Lr).unwrap(), 0x1100);
                assert_eq!(cpu.read_reg(ArmReg::Cpsr).unwrap(), flags);
                assert_eq!(cpu.read_fpscr().unwrap(), 0x02000000);
            }
        }
    }

    #[test]
    fn inline_arithmetic_hooks_are_removed_with_their_mapping() {
        let mut cpu = UnicornCpu::new_for_arch(Arch::Arm).unwrap();
        cpu.map_region(0x1000, 0x1000, Prot::ALL).unwrap();
        cpu.install_inline_arithmetic64(0x1000, crate::arithmetic64::Arithmetic64::SignedDivide)
            .unwrap();
        cpu.unmap_region(0x1000, 0x1000).unwrap();
        cpu.map_region(0x1000, 0x1000, Prot::ALL).unwrap();
        cpu.write_mem(0x1000, &0xe12fff1eu32.to_le_bytes()).unwrap();
        cpu.add_code_hook(0x1100).unwrap();
        cpu.write_reg(ArmReg::R0, 17).unwrap();
        cpu.write_reg(ArmReg::R1, 0).unwrap();
        cpu.write_reg(ArmReg::R2, 3).unwrap();
        cpu.write_reg(ArmReg::R3, 0).unwrap();
        cpu.write_reg(ArmReg::Lr, 0x1100).unwrap();
        assert_eq!(
            cpu.run_until_hook(0x1000, 100).unwrap(),
            StopReason::Hook(0x1100)
        );
        assert_eq!(cpu.read_reg(ArmReg::R0).unwrap(), 17);
    }

    #[test]
    fn inline_arithmetic_returns_to_thumb_and_remains_preemptible() {
        use crate::arithmetic64::Arithmetic64;
        let mut cpu = UnicornCpu::new_for_arch(Arch::Arm).unwrap();
        cpu.map_region(0x1000, 0x1000, Prot::ALL).unwrap();
        cpu.write_mem(0x1000, &0x47a0u16.to_le_bytes()).unwrap(); // Thumb blx r4
        cpu.write_mem(0x1200, &[0x1e, 0xff, 0x2f, 0xe1, 0x1e, 0xff, 0x2f, 0xe1])
            .unwrap();
        cpu.install_inline_arithmetic64(0x1200, Arithmetic64::SignedDivide)
            .unwrap();
        cpu.add_code_hook(0x1002).unwrap();
        cpu.write_reg(ArmReg::R0, (-123i64) as u32).unwrap();
        cpu.write_reg(ArmReg::R1, u32::MAX).unwrap();
        cpu.write_reg(ArmReg::R2, 5).unwrap();
        cpu.write_reg(ArmReg::R3, 0).unwrap();
        cpu.write_reg(ArmReg::R4, 0x1200).unwrap();
        assert_eq!(
            cpu.run_until_hook(0x1001, 100).unwrap(),
            StopReason::Hook(0x1002)
        );
        assert_eq!(cpu.read_reg(ArmReg::R0).unwrap(), (-24i32) as u32);
        assert_eq!(cpu.read_reg(ArmReg::R1).unwrap(), u32::MAX);
        assert_ne!(cpu.read_reg(ArmReg::Cpsr).unwrap() & 0x20, 0);
        // No stopping API hook inside this ARM loop; the scheduler still gets control.
        for (offset, word) in [0xe1a0e00fu32, 0xe12fff14, 0xeafffffc]
            .into_iter()
            .enumerate()
        {
            cpu.write_mem(0x1400 + offset as u32 * 4, &word.to_le_bytes())
                .unwrap();
        }
        cpu.set_slice_timeout_us(1000).unwrap();
        assert_eq!(
            cpu.run_until_hook(0x1400, 0).unwrap(),
            StopReason::InstructionLimit
        );
    }

    #[test]
    fn wall_clock_preemption_resumes_an_api_free_poll_without_changing_guest_state() {
        let mut cpu = UnicornCpu::new_for_arch(Arch::Arm).unwrap();
        cpu.map_region(0x1000, 0x1000, Prot::ALL).unwrap();
        cpu.map_region(0x2000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        // Poll a host-refreshed cell, like native GetTickCount. No API or
        // instruction hook occurs inside the loop; only completion is hooked.
        let words = [
            0xe59f1014u32, // ldr r1,[pc,#20] -> literal 0x2000
            0xe5910000,    // ldr r0,[r1]
            0xe3500000,    // cmp r0,#0
            0x0afffffc,    // beq 0x1004
            0xe3a0702a,    // mov r7,#42
            0xea000039,    // b 0x1100
            0xe12fff1e,
            0x2000,
        ];
        let bytes: Vec<_> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        cpu.write_mem(0x1000, &bytes).unwrap();
        cpu.write_mem(0x2000, &0u32.to_le_bytes()).unwrap();
        cpu.write_reg(ArmReg::R7, 17).unwrap();
        cpu.add_code_hook(0x1100).unwrap();
        cpu.set_slice_timeout_us(20_000).unwrap();
        assert_eq!(
            cpu.run_until_hook(0x1000, 0).unwrap(),
            StopReason::InstructionLimit
        );
        assert_eq!(cpu.read_reg(ArmReg::R7).unwrap(), 17);
        let pc = cpu.read_reg(ArmReg::Pc).unwrap();
        cpu.write_mem(0x2000, &1u32.to_le_bytes()).unwrap();
        assert_eq!(cpu.run_until_hook(pc, 0).unwrap(), StopReason::Hook(0x1100));
        assert_eq!(cpu.read_reg(ArmReg::R7).unwrap(), 42);
        cpu.set_slice_timeout_us(0).unwrap();
    }

    #[test]
    fn register_trace_observes_without_stopping_or_changing_arm_execution() {
        let mut cpu = UnicornCpu::new_for_arch(Arch::Arm).unwrap();
        cpu.map_region(0x1000, 0x1000, Prot::ALL).unwrap();
        // mov r0,#1; add r0,r0,#1; bx lr
        let words = [0xe3a00001u32, 0xe2800001, 0xe12fff1e];
        cpu.write_mem(
            0x1000,
            &words
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        cpu.add_register_trace(0x1004, "test-observation").unwrap();
        cpu.add_code_hook(0x1100).unwrap();
        cpu.write_reg(ArmReg::Lr, 0x1100).unwrap();
        assert_eq!(
            cpu.run_until_hook(0x1000, 100).unwrap(),
            StopReason::Hook(0x1100)
        );
        assert_eq!(cpu.read_reg(ArmReg::R0).unwrap(), 2);
    }

    #[test]
    fn unmap_and_remap_do_not_keep_old_hooks_or_translated_arm_code() {
        let mut cpu = UnicornCpu::new_for_arch(Arch::Arm).unwrap();
        cpu.map_region(0x1000, 0x1000, Prot::ALL).unwrap();
        cpu.map_region(0x3000, 0x1000, Prot::ALL).unwrap();
        cpu.write_mem(0x1000, &[0x01, 0x00, 0xa0, 0xe3, 0x3d, 0x00, 0x00, 0xea])
            .unwrap();
        cpu.write_mem(0x1100, &0xe12fff1eu32.to_le_bytes()).unwrap();
        cpu.add_code_hook(0x1100).unwrap();
        cpu.add_code_hook(0x3000).unwrap();
        cpu.write_reg(ArmReg::Lr, 0x3000).unwrap();
        assert_eq!(
            cpu.run_until_hook(0x1000, 100).unwrap(),
            StopReason::Hook(0x1100)
        );
        assert_eq!(cpu.read_reg(ArmReg::R0).unwrap(), 1);
        cpu.unmap_region(0x1000, 0x1000).unwrap();
        assert!(cpu.read_mem(0x1000, 1).is_err());
        cpu.map_region(0x1000, 0x1000, Prot::ALL).unwrap();
        cpu.write_mem(0x1000, &[0x02, 0x00, 0xa0, 0xe3, 0x3d, 0x00, 0x00, 0xea])
            .unwrap();
        cpu.write_mem(0x1100, &0xe12fff1eu32.to_le_bytes()).unwrap();
        assert_eq!(
            cpu.run_until_hook(0x1000, 100).unwrap(),
            StopReason::Hook(0x3000)
        );
        assert_eq!(cpu.read_reg(ArmReg::R0).unwrap(), 2);
    }

    #[test]
    fn unmapping_removes_permission_and_execution_promotion_history() {
        let mut map = GuestMap::default();
        map.insert(0x1000, 0x3000, Prot::READ | Prot::WRITE);
        map.protected.push((0x1000, 0x4000, Prot::READ));
        map.promote_exec(0x1000);
        map.promote_exec(0x2000);
        map.promote_exec(0x3000);
        map.remove(0x2000, 0x1000);
        assert_eq!(map.region_for(0x2000), None);
        assert!(!map.is_promoted_exec(0x2000));
        assert_eq!(
            map.protected,
            vec![(0x1000, 0x2000, Prot::READ), (0x3000, 0x4000, Prot::READ)]
        );
        map.insert(0x2000, 0x1000, Prot::READ | Prot::EXEC);
        assert_eq!(map.region_for(0x2000), Some(Prot::READ | Prot::EXEC));
    }
}
