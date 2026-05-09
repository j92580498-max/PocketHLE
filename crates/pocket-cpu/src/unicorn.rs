//! `unicorn-engine`-backed CPU.
//!
//! Compiled only with `--features unicorn`. Apart from the build cost,
//! this is the authoritative ARM backend used at runtime.

use std::cell::RefCell;
use std::rc::Rc;

use ::unicorn_engine::unicorn_const::{Arch as UcArch, Mode, Prot as UcProt};
use ::unicorn_engine::{RegisterARM, Unicorn};

use crate::{regs::ArmReg, Arch, Cpu, CpuError, Prot, StopReason};

pub struct UnicornCpu {
    uc: Unicorn<'static, ()>,
    last_hook: Rc<RefCell<Option<u32>>>,
    stop_requested: Rc<RefCell<bool>>,
}

impl UnicornCpu {
    pub fn new() -> Result<Self, CpuError> {
        let mut uc = Unicorn::new(UcArch::ARM, Mode::LITTLE_ENDIAN)
            .map_err(|e| CpuError::Backend(format!("Unicorn::new failed: {e:?}")))?;
        // Enable VFP. Pocket PC binaries are built with the soft-float
        // ABI, so the *guest* code never touches VFP registers — but
        // we patch IAT thunks for `__adds` / `__divs` / etc. with
        // native VFP instructions, and those need VFP turned on.
        // Setting `FPEXC.EN = 1` and CPACR's CP10/CP11 access bits
        // mirrors what real ARMv7 firmware does at boot.
        let _ = uc.reg_write(RegisterARM::FPEXC, 0x4000_0000);
        let _ = uc.reg_write(RegisterARM::C1_C0_2, 0x00F0_0000);
        Ok(Self {
            uc,
            last_hook: Rc::new(RefCell::new(None)),
            stop_requested: Rc::new(RefCell::new(false)),
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

fn map_reg(r: ArmReg) -> RegisterARM {
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

impl Cpu for UnicornCpu {
    fn arch(&self) -> Arch {
        Arch::Arm
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
        self.uc
            .reg_read(map_reg(reg))
            .map(|v| v as u32)
            .map_err(|e| CpuError::Backend(format!("reg_read: {e:?}")))
    }

    fn write_reg(&mut self, reg: ArmReg, value: u32) -> Result<(), CpuError> {
        self.uc
            .reg_write(map_reg(reg), value as u64)
            .map_err(|e| CpuError::Backend(format!("reg_write: {e:?}")))
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
        max_instructions: u64,
    ) -> Result<StopReason, CpuError> {
        *self.last_hook.borrow_mut() = None;
        *self.stop_requested.borrow_mut() = false;
        let r = self.uc.emu_start(
            start_va as u64,
            0, // until = 0 → run until stopped or limit
            0, // timeout (us); 0 = no timeout
            max_instructions as usize,
        );
        if let Some(addr) = *self.last_hook.borrow() {
            return Ok(StopReason::Hook(addr));
        }
        match r {
            Ok(()) => Ok(StopReason::InstructionLimit),
            Err(e) => Err(CpuError::Backend(format!("emu_start: {e:?}"))),
        }
    }

    fn request_stop(&mut self) {
        *self.stop_requested.borrow_mut() = true;
        let _ = self.uc.emu_stop();
    }
}
