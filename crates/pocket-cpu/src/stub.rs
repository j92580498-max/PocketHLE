//! In-process software-only CPU stub.
//!
//! It does not interpret any instructions. Memory and registers are
//! tracked so that loader / kernel layers can be unit-tested without
//! pulling in `unicorn-engine`.

use std::collections::BTreeMap;

use crate::{regs::ArmReg, Arch, Cpu, CpuError, Prot, StopReason};

/// Per-page state. A page is `0x1000` bytes.
#[derive(Debug, Default, Clone)]
struct Page {
    bytes: Vec<u8>,
    #[allow(dead_code)]
    prot: Prot,
}

const PAGE_SIZE: u32 = 0x1000;

#[derive(Default)]
pub struct StubCpu {
    pages: BTreeMap<u32, Page>,
    regs: [u32; 17],
    hooks: Vec<u32>,
    /// Inclusive stop-on-execute ranges from [`Cpu::add_code_hook_range`].
    /// The stub never interprets instructions, so these are only
    /// recorded so loader tests can assert that the thunk pool was
    /// covered.
    hook_ranges: Vec<(u32, u32)>,
    stop_requested: bool,
}

impl StubCpu {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Cpu for StubCpu {
    fn arch(&self) -> Arch {
        Arch::Arm
    }

    fn map_region(&mut self, va: u32, size: u32, prot: Prot) -> Result<(), CpuError> {
        let mut p = va & !(PAGE_SIZE - 1);
        let end = va.saturating_add(size);
        while p < end {
            self.pages.entry(p).or_insert_with(|| Page {
                bytes: vec![0; PAGE_SIZE as usize],
                prot,
            });
            p = p.saturating_add(PAGE_SIZE);
            if p == 0 {
                break; // wraparound
            }
        }
        Ok(())
    }

    fn write_mem(&mut self, va: u32, data: &[u8]) -> Result<(), CpuError> {
        let mut cur = va;
        for byte in data {
            let page_va = cur & !(PAGE_SIZE - 1);
            let page = self
                .pages
                .get_mut(&page_va)
                .ok_or(CpuError::BadMemory { va: cur, size: 1 })?;
            let off = (cur - page_va) as usize;
            page.bytes[off] = *byte;
            cur = cur.wrapping_add(1);
        }
        Ok(())
    }

    fn read_mem(&mut self, va: u32, len: u32) -> Result<Vec<u8>, CpuError> {
        let mut out = Vec::with_capacity(len as usize);
        for i in 0..len {
            let cur = va.wrapping_add(i);
            let page_va = cur & !(PAGE_SIZE - 1);
            let page = self
                .pages
                .get(&page_va)
                .ok_or(CpuError::BadMemory { va: cur, size: 1 })?;
            out.push(page.bytes[(cur - page_va) as usize]);
        }
        Ok(out)
    }

    fn read_reg(&mut self, reg: ArmReg) -> Result<u32, CpuError> {
        Ok(self.regs[reg as usize])
    }

    fn write_reg(&mut self, reg: ArmReg, value: u32) -> Result<(), CpuError> {
        self.regs[reg as usize] = value;
        Ok(())
    }

    fn add_code_hook(&mut self, va: u32) -> Result<(), CpuError> {
        self.hooks.push(va);
        Ok(())
    }

    fn add_code_hook_range(&mut self, lo: u32, hi: u32) -> Result<(), CpuError> {
        self.hook_ranges.push((lo, hi));
        Ok(())
    }

    fn run_until_hook(
        &mut self,
        start_va: u32,
        _max_instructions: u64,
    ) -> Result<StopReason, CpuError> {
        // The stub doesn't interpret instructions; instead it pretends
        // we executed straight to the first registered hook (if any)
        // so that loader-level integration tests can still flow.
        self.regs[ArmReg::Pc as usize] = start_va;
        // A ranged hook stands for every slot inside it, so its low
        // bound is the first hook of that range. Ranges and single
        // addresses are both registered by the loader, so pick
        // whichever came first to keep the pre-range behaviour.
        let first = self
            .hooks
            .first()
            .copied()
            .into_iter()
            .chain(self.hook_ranges.first().map(|&(lo, _)| lo))
            .min();
        if let Some(h) = first {
            self.regs[ArmReg::Pc as usize] = h;
            return Ok(StopReason::Hook(h));
        }
        Ok(StopReason::InstructionLimit)
    }

    fn request_stop(&mut self) {
        self.stop_requested = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_write_read() {
        let mut cpu = StubCpu::new();
        cpu.map_region(0x1000, 0x1000, Prot::ALL).unwrap();
        cpu.write_mem(0x1234, &[1, 2, 3]).unwrap();
        let v = cpu.read_mem(0x1234, 3).unwrap();
        assert_eq!(v, vec![1, 2, 3]);
    }

    #[test]
    fn registers_round_trip() {
        let mut cpu = StubCpu::new();
        cpu.write_reg(ArmReg::R0, 0xdead_beef).unwrap();
        assert_eq!(cpu.read_reg(ArmReg::R0).unwrap(), 0xdead_beef);
    }
}
