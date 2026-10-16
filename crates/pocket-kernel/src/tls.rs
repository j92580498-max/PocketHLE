//! Save the active CE KData TLS window at guest thread switches.
use crate::{Cpu, KernelError, KernelState, TLS_SLOT_COUNT, USER_KDATA_TLS_ARRAY_VA};

impl KernelState {
    fn tls_thread_finished(&self, thread: usize) -> bool {
        if thread == 0 {
            self.main_thread.exit_code.is_some()
        } else {
            self.threads.get(thread - 1).is_some_and(|t| t.finished)
        }
    }

    pub fn sync_thread_tls(&mut self, cpu: &mut dyn Cpu) -> Result<(), KernelError> {
        if self.tls_owner == self.current_thread {
            return Ok(());
        }
        let raw = cpu.read_mem(USER_KDATA_TLS_ARRAY_VA, TLS_SLOT_COUNT * 4)?;
        let mut outgoing = [0u32; TLS_SLOT_COUNT as usize];
        for (value, bytes) in outgoing.iter_mut().zip(raw.chunks_exact(4)) {
            *value = u32::from_le_bytes(bytes.try_into().unwrap());
        }
        let incoming = self
            .thread_tls
            .get(&self.current_thread)
            .copied()
            .unwrap_or([0; TLS_SLOT_COUNT as usize]);
        let mut bytes = [0u8; TLS_SLOT_COUNT as usize * 4];
        for (value, dst) in incoming.iter().zip(bytes.chunks_exact_mut(4)) {
            dst.copy_from_slice(&value.to_le_bytes());
        }
        // Publish ownership only after the target window has been restored.
        cpu.write_mem(USER_KDATA_TLS_ARRAY_VA, &bytes)?;
        if !self.tls_thread_finished(self.tls_owner) {
            self.thread_tls.insert(self.tls_owner, outgoing);
        }
        self.thread_tls.remove(&self.current_thread);
        self.tls_owner = self.current_thread;
        Ok(())
    }

    pub fn clear_tls_slot(&mut self, cpu: &mut dyn Cpu, slot: u32) -> Result<(), KernelError> {
        // Slot values are opaque pointers: clear storage, never free pointees.
        cpu.write_mem(USER_KDATA_TLS_ARRAY_VA + slot * 4, &[0; 4])?;
        for values in self.thread_tls.values_mut() {
            values[slot as usize] = 0;
        }
        Ok(())
    }

    pub(crate) fn reclaim_finished_tls(&mut self) {
        let main_finished = self.main_thread.exit_code.is_some();
        let threads = &self.threads;
        self.thread_tls.retain(|&thread, _| {
            if thread == 0 {
                !main_finished
            } else {
                !threads.get(thread - 1).is_some_and(|t| t.finished)
            }
        });
    }
}
