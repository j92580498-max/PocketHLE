//! Physical image pages share the same device budget as heaps and stacks.
use crate::memory_division::MemoryDivision;
use pocket_cpu::image_pages::ImagePageBudget;
use std::sync::Mutex;
#[derive(Default, Debug)]
pub struct ImageMemory {
    state: Mutex<State>,
}
#[derive(Default, Debug)]
struct State {
    pages: u32,
    ram: Option<MemoryDivision>,
}
impl ImageMemory {
    pub fn pages(&self) -> u32 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).pages
    }
    pub fn attach_ram(&self, ram: Option<MemoryDivision>) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .ram
            .as_ref()
            .zip(ram.as_ref())
            .is_some_and(|(old, next)| old.same_device(next))
        {
            return true;
        }
        if let Some(next) = &ram {
            if !next.acquire_program(state.pages) {
                return false;
            }
        }
        if let Some(old) = &state.ram {
            old.release_program(state.pages);
        }
        state.ram = ram;
        true
    }
}
impl ImagePageBudget for ImageMemory {
    fn acquire(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .ram
            .as_ref()
            .is_some_and(|ram| !ram.acquire_program(1))
        {
            return false;
        }
        state.pages += 1;
        true
    }
    fn release(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert!(state.pages > 0);
        state.pages -= 1;
        if let Some(ram) = &state.ram {
            ram.release_program(1);
        }
    }
}
impl Drop for ImageMemory {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        if let Some(ram) = &state.ram {
            ram.release_program(state.pages);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{stub::StubCpu, Cpu, Prot};
    #[test]
    fn paged_images_and_heap_share_budget_and_profile_changes_are_atomic() {
        let ram = MemoryDivision::new(12 * 4096, 8).unwrap();
        let mut heap = crate::Heap::new(0x50000000, 0x10000);
        let mut cpu = StubCpu::new();
        cpu.map_image_region(
            0x30000000,
            6 * 4096,
            Prot::ALL,
            vec![17],
            heap.image_page_budget(),
        )
        .unwrap();
        assert_eq!(cpu.read_u8(0x30000000).unwrap(), 17);
        assert_eq!(heap.program_pages(), 1);
        assert!(heap.attach_ram(Some(ram.clone())));
        assert_eq!(ram.snapshot().program_used, 1);
        let allocation = heap.alloc(4096).unwrap(); // data plus allocation header: 2 pages
        assert_eq!(ram.snapshot().program_used, 3);
        cpu.read_u8(0x30001000).unwrap();
        assert_eq!(ram.snapshot().program_used, 4);
        assert!(matches!(
            cpu.read_u8(0x30002000),
            Err(pocket_cpu::CpuError::ImageOutOfMemory { .. })
        ));
        assert_eq!(ram.snapshot().program_used, 4);
        assert!(heap.attach_ram(Some(ram.clone()))); // no transient double charging
        heap.free(allocation);
        cpu.read_u8(0x30002000).unwrap();
        assert_eq!(ram.snapshot().program_used, 3);
        let small = MemoryDivision::new(10 * 4096, 8).unwrap();
        assert!(!heap.attach_ram(Some(small.clone())));
        assert_eq!(ram.snapshot().program_used, 3);
        assert_eq!(small.snapshot().program_used, 0);
        cpu.unmap_region(0x30000000, 6 * 4096).unwrap();
        assert_eq!(heap.program_pages(), 0);
        assert_eq!(ram.snapshot().program_used, 0);
    }
}
