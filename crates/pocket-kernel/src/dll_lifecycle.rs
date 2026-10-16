//! Serialized guest DllMain notifications, with the interrupted context retained.
use crate::{KernelError, KernelState, THREAD_SCHEDULER_IDLE_VA};
use pocket_cpu::{regs::ArmReg, Cpu};

pub const DLL_NOTIFICATION_RETURN_VA: u32 = 0xf000_0008;
const REGS: [ArmReg; 17] = [
    ArmReg::R0,
    ArmReg::R1,
    ArmReg::R2,
    ArmReg::R3,
    ArmReg::R4,
    ArmReg::R5,
    ArmReg::R6,
    ArmReg::R7,
    ArmReg::R8,
    ArmReg::R9,
    ArmReg::R10,
    ArmReg::R11,
    ArmReg::R12,
    ArmReg::Sp,
    ArmReg::Lr,
    ArmReg::Pc,
    ArmReg::Cpsr,
];
#[derive(Clone, Debug)]
pub enum DllContinuation {
    Resume(u32),
    FinishMain(u32),
    ExitProcess(u32),
}
#[derive(Clone, Debug)]
pub struct DllNotificationFrame {
    pub thread: usize,
    pub reason: u32,
    pub continuation: DllContinuation,
    regs: [u32; 17],
    fpscr: u32,
    error: Option<u32>,
    pending: Vec<(u32, u32)>,
}
impl KernelState {
    /// No retroactive THREAD_ATTACH: the snapshot is taken when a new thread starts.
    pub fn begin_dll_notifications(
        &mut self,
        cpu: &mut dyn Cpu,
        reason: u32,
        continuation: DllContinuation,
    ) -> Result<Option<u32>, KernelError> {
        let mut pending: Vec<_> = self
            .modules
            .iter()
            .filter(|module| {
                module.attached
                    && module.image_entry != 0
                    && !self
                        .module_attach_frames
                        .iter()
                        .any(|frame| frame.base == module.base && !frame.detaching)
            })
            .map(|module| (module.base, module.image_entry))
            .collect();
        // pop() yields load order on attach, reverse load order on detach.
        if reason == 2 {
            pending.reverse();
        }
        if pending.is_empty() {
            return Ok(None);
        }
        if self.dll_notification_frame.is_some() {
            return Err(KernelError::Loader(
                "overlapping DLL lifecycle notifications".into(),
            ));
        }
        let mut regs = [0; 17];
        for (i, reg) in REGS.iter().enumerate() {
            regs[i] = cpu.read_reg(*reg)?;
        }
        self.dll_notification_frame = Some(DllNotificationFrame {
            thread: self.current_thread,
            reason,
            continuation,
            regs,
            fpscr: cpu.read_fpscr()?,
            error: self.thread_last_errors.get(&self.current_thread).copied(),
            pending,
        });
        self.next_dll_notification(cpu)
    }
    pub fn next_dll_notification(&mut self, cpu: &mut dyn Cpu) -> Result<Option<u32>, KernelError> {
        let Some(frame) = self.dll_notification_frame.as_mut() else {
            return Err(KernelError::Loader(
                "DLL notification return without a pending callback".into(),
            ));
        };
        if frame.thread != self.current_thread || cpu.read_reg(ArmReg::Sp)? != frame.regs[13] {
            return Err(KernelError::Loader(
                "DLL notification returned in the wrong context".into(),
            ));
        }
        while let Some((base, entry)) = frame.pending.pop() {
            // A callback must not jump through a module already unloaded by another callback.
            if !self
                .modules
                .iter()
                .any(|module| module.base == base && module.image_entry == entry)
            {
                continue;
            }
            cpu.write_reg(ArmReg::R0, base)?;
            cpu.write_reg(ArmReg::R1, frame.reason)?;
            cpu.write_reg(ArmReg::R2, u32::from(frame.reason == 0))?;
            cpu.write_reg(ArmReg::Lr, DLL_NOTIFICATION_RETURN_VA)?;
            return Ok(Some(entry));
        }
        let frame = self.dll_notification_frame.take().unwrap();
        for (i, reg) in REGS.iter().enumerate() {
            cpu.write_reg(*reg, frame.regs[i])?;
        }
        cpu.write_fpscr(frame.fpscr)?;
        match frame.error {
            Some(error) => {
                self.thread_last_errors.insert(frame.thread, error);
            }
            None => {
                self.thread_last_errors.remove(&frame.thread);
            }
        }
        match frame.continuation {
            DllContinuation::Resume(pc) => Ok(Some(pc)),
            DllContinuation::FinishMain(code) => {
                self.main_thread.last_exit_code = Some(code);
                self.main_thread.exit_code = Some(code);
                self.main_thread.saved_regs = None;
                self.message_frames.remove(&0);
                self.reclaim_finished_stacks(cpu)?;
                Ok(Some(THREAD_SCHEDULER_IDLE_VA))
            }
            DllContinuation::ExitProcess(code) => {
                self.finish_dll_process_exit(cpu, code)?;
                Ok(None)
            }
        }
    }
    pub fn finish_dll_process_exit(
        &mut self,
        cpu: &mut dyn Cpu,
        code: u32,
    ) -> Result<(), KernelError> {
        // Keep DLLs and the calling stack resident until all callbacks have returned.
        while let Some(base) = self.modules.last().map(|module| module.base) {
            self.unload_runtime_module(cpu, base)?;
        }
        self.record_process_exit(code);
        log::info!("process exited normally with code 0x{code:08x}");
        self.reclaim_finished_stacks(cpu)
    }
    /// Imported DLLs have no explicit reference of their own. Cycles are kept
    /// alive by a loaded root, and become collectible together when it closes.
    pub fn unreachable_runtime_modules(&self) -> Vec<u32> {
        let mut reachable = std::collections::HashSet::new();
        let mut pending: Vec<_> = self
            .modules
            .iter()
            .filter(|module| module.refcount != 0)
            .map(|module| module.base)
            .collect();
        while let Some(base) = pending.pop() {
            if !reachable.insert(base) {
                continue;
            }
            if let Some(module) = self.modules.iter().find(|module| module.base == base) {
                pending.extend(module.dependencies.iter().copied());
                pending.extend(module.satellites.iter().copied());
            }
        }
        self.modules
            .iter()
            .filter(|module| !reachable.contains(&module.base))
            .map(|module| module.base)
            .collect()
    }
    pub fn unload_runtime_module(
        &mut self,
        cpu: &mut dyn Cpu,
        base: u32,
    ) -> Result<(), KernelError> {
        let Some(index) = self.modules.iter().position(|module| module.handle == base) else {
            return Ok(());
        };
        while let Some(&(start, size)) = self.modules[index].resident_regions.last() {
            if size != 0 {
                cpu.unmap_region(start, size)?;
            }
            self.heap.release_resident(start, size);
            self.modules[index].resident_regions.pop();
        }
        let satellites = self.modules.remove(index).satellites;
        for satellite in satellites {
            if let Some(module) = self
                .modules
                .iter_mut()
                .find(|module| module.handle == satellite)
            {
                if module.refcount > 1 {
                    module.refcount -= 1;
                } else {
                    self.unload_runtime_module(cpu, satellite)?;
                }
            }
        }
        self.dynamic_exports.remove(&base);
        self.runtime_thunks
            .retain(|address, _| *address < base || *address >= base + crate::MODULE_REGION_STRIDE);
        self.free_module_bases.push(base);
        Ok(())
    }
}
