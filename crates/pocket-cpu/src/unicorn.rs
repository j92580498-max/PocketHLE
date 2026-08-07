//! `unicorn-engine`-backed CPU.
//!
//! Compiled only with `--features unicorn`. Apart from the build cost,
//! this is the authoritative ARM backend used at runtime.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;

use ::unicorn_engine::unicorn_const::{Arch as UcArch, Mode, Prot as UcProt};
use ::unicorn_engine::{RegisterARM, RegisterMIPS, Unicorn};

use crate::{regs::ArmReg, Arch, Cpu, CpuError, Prot, StopReason};

pub struct UnicornCpu {
    uc: Unicorn<'static, ()>,
    last_hook: Rc<RefCell<Option<u32>>>,
    /// Address of the last invalid memory access seen by the
    /// `mem_invalid` hook, recorded so crash reports can name the
    /// faulting address instead of just the access kind.
    last_fault: Rc<RefCell<Option<(String, u64)>>>,
    stop_requested: Rc<RefCell<bool>>,
    arch: Arch,
    mips_status: u32,
}

impl UnicornCpu {
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
        {
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
        Ok(Self {
            uc,
            arch,
            last_fault,
            last_hook: Rc::new(RefCell::new(None)),
            stop_requested: Rc::new(RefCell::new(false)),
            mips_status: 0,
        })
    }
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

impl Cpu for UnicornCpu {
    fn arch(&self) -> Arch {
        self.arch
    }

    fn map_region(&mut self, va: u32, size: u32, prot: Prot) -> Result<(), CpuError> {
        self.uc
            .mem_map(va as u64, size as u64, map_prot(prot))
            .map_err(|e| CpuError::Backend(format!("mem_map: {e:?}")))
    }

    fn write_mem(&mut self, va: u32, data: &[u8]) -> Result<(), CpuError> {
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

    fn add_code_hook(&mut self, va: u32) -> Result<(), CpuError> {
        let last = self.last_hook.clone();
        let stop = self.stop_requested.clone();
        let cb = move |uc: &mut Unicorn<'_, ()>, _addr: u64, _size: u32| {
            *last.borrow_mut() = Some(va);
            *stop.borrow_mut() = true;
            let _ = uc.emu_stop();
        };
        self.uc
            .add_code_hook(va as u64, va as u64, cb)
            .map(|_| ())
            .map_err(|e| CpuError::Backend(format!("add_code_hook: {e:?}")))
    }

    fn run_until_hook(
        &mut self,
        start_va: u32,
        _max_instructions: u64,
    ) -> Result<StopReason, CpuError> {
        *self.last_hook.borrow_mut() = None;
        *self.stop_requested.borrow_mut() = false;
        // IMPORTANT: run with `count = 0` (no instruction limit) so the
        // QEMU TCG keeps chaining translation blocks at full speed. A
        // non-zero `count` would silently install a per-instruction
        // counting hook and tank throughput ~5-10x. Slices are instead
        // ended by the IAT-thunk code hooks (which call `emu_stop` on
        // every emulated API call) and, optionally, by a wall-clock
        // watchdog for pathological API-free loops.
        let r = self.uc.emu_start(
            start_va as u64,
            0,                   // until = 0 → run until stopped
            slice_watchdog_us(), // timeout (us); 0 = no timeout
            0,                   // count = 0 → keep TB chaining (do NOT pass a limit)
        );
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
