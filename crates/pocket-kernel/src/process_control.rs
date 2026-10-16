//! Cross-process requests are applied only by the owning CPU at a slice boundary.
use crate::{Cpu, KernelError, KernelState, THREAD_SCHEDULER_IDLE_VA};
use pocket_cpu::regs::ArmReg;
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
impl KernelState {
    /// True means forced process exit; no guest detach callbacks may run.
    pub fn apply_process_controls(
        &mut self,
        cpu: &mut dyn Cpu,
        pc: &mut u32,
    ) -> Result<bool, KernelError> {
        let (kill, updates) = self.object_handles.take_controls();
        if let Some(code) = kill {
            self.record_process_exit(code);
            self.reclaim_finished_stacks(cpu)?;
            return Ok(true);
        }
        if updates.is_empty() {
            return Ok(false);
        }
        let forced_thread_exit = updates.iter().any(|(_, u)| u.terminate.is_some());
        for (index, update) in updates {
            if index == 0 {
                if self.main_thread.exit_code.is_some() {
                    continue;
                }
                if let Some(count) = update.suspend_count {
                    self.main_thread.suspend_count = count;
                }
                if let Some(code) = update.terminate {
                    self.main_thread.exit_code = Some(code);
                    self.main_thread.last_exit_code = Some(code);
                    self.main_thread.saved_regs = None;
                    self.message_frames.remove(&0);
                    self.object_handles.set_exit(Some(0), code);
                }
            } else if let Some(thread) = self.threads.get_mut(index - 1) {
                if thread.finished {
                    continue;
                }
                if let Some(count) = update.suspend_count {
                    thread.suspend_count = count;
                    thread.started = count == 0;
                }
                if let Some(code) = update.terminate {
                    thread.finished = true;
                    thread.exit_code = Some(code);
                    thread.dll_thread_detach_delivered = true;
                    self.main_thread.last_exit_code = Some(code);
                    self.message_frames.remove(&index);
                    self.object_handles.set_exit(Some(index), code);
                }
            }
            if update.terminate.is_some()
                && self
                    .dll_notification_frame
                    .as_ref()
                    .is_some_and(|f| f.thread == index)
            {
                self.dll_notification_frame = None;
            }
        }
        let owner = self.current_thread;
        let (dead, suspended) = if owner == 0 {
            (
                self.main_thread.exit_code.is_some(),
                self.main_thread.suspend_count != 0,
            )
        } else {
            let t = &self.threads[owner - 1];
            (t.finished, t.suspend_count != 0)
        };
        if (dead || suspended) && *pc != THREAD_SCHEDULER_IDLE_VA {
            let mut saved = [0; 17];
            if !dead {
                for (i, reg) in REGS.iter().enumerate() {
                    saved[i] = cpu.read_reg(*reg)?;
                }
                saved[15] = *pc;
            }
            if owner == 0 {
                if !dead {
                    self.main_thread.saved_regs = Some(saved);
                }
                *pc = THREAD_SCHEDULER_IDLE_VA;
            } else {
                let thread = &mut self.threads[owner - 1];
                if !dead {
                    thread.worker_regs = saved;
                    thread.worker_saved = true;
                }
                let main = thread.saved_regs;
                self.guest_fpscr.insert(owner, cpu.read_fpscr()?);
                cpu.write_fpscr(self.guest_fpscr.get(&0).copied().unwrap_or(0))?;
                self.current_thread = 0;
                if self.main_thread.exit_code.is_none()
                    && self.main_thread.suspend_count == 0
                    && self.main_thread.saved_regs.is_none()
                {
                    for (i, reg) in REGS.iter().enumerate() {
                        cpu.write_reg(*reg, main[i])?;
                    }
                    *pc = main[15];
                } else {
                    *pc = THREAD_SCHEDULER_IDLE_VA;
                }
            }
        }
        self.reclaim_finished_stacks(cpu)?;
        if forced_thread_exit
            && self.main_thread.exit_code.is_some()
            && self.threads.iter().all(|t| t.finished)
            && self.process_exit_code.is_none()
        {
            let code = self
                .main_thread
                .last_exit_code
                .unwrap_or(self.main_thread.exit_code.unwrap());
            self.record_process_exit(code);
            return Ok(true);
        }
        Ok(false)
    }
}
