use std::time::Instant;

use pocket_cpu::Cpu;

use crate::Heap;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CorruptionOptions {
    pub enabled: bool,
    pub interval_frames: u32,
    pub bytes_per_burst: u32,
    pub max_offset: Option<u32>,
    pub seed: u64,
}

impl Default for CorruptionOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_frames: 400,
            bytes_per_burst: 1,
            max_offset: None,
            seed: 0x6a09_e667_f3bc_c909,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9e37_79b9_7f4a_7c15,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

pub struct Corruptor {
    options: CorruptionOptions,
    rng: Rng,
    ticks_since_burst: u64,
    total_corrupted: u64,
    last_log: Option<Instant>,
}

impl Default for Corruptor {
    fn default() -> Self {
        Self::new(CorruptionOptions {
            enabled: false,
            ..CorruptionOptions::default()
        })
    }
}

impl Corruptor {
    pub fn new(options: CorruptionOptions) -> Self {
        Self {
            rng: Rng::new(options.seed),
            options,
            ticks_since_burst: 0,
            total_corrupted: 0,
            last_log: None,
        }
    }

    pub fn options(&self) -> CorruptionOptions {
        self.options
    }

    pub fn total_corrupted(&self) -> u64 {
        self.total_corrupted
    }

    pub fn tick(&mut self, cpu: &mut dyn Cpu, heap: &Heap) {
        if !self.options.enabled {
            return;
        }
        self.ticks_since_burst = self.ticks_since_burst.saturating_add(1);
        if self.ticks_since_burst < u64::from(self.options.interval_frames.max(1)) {
            return;
        }
        self.ticks_since_burst = 0;
        self.blast(cpu, heap);
    }

    fn blast(&mut self, cpu: &mut dyn Cpu, heap: &Heap) {
        let allocations = heap.live_allocations();
        let ranges: Vec<(u32, u32)> = allocations
            .into_iter()
            .filter_map(|(base, size)| {
                let eligible = match self.options.max_offset {
                    Some(limit) if limit > 0 => size.min(limit),
                    _ => size,
                };
                (eligible > 0).then_some((base, eligible))
            })
            .collect();
        let total_bytes: u64 = ranges.iter().map(|(_, size)| u64::from(*size)).sum();
        if total_bytes == 0 {
            return;
        }

        let mut corrupted = 0u64;
        for _ in 0..self.options.bytes_per_burst.max(1) {
            let mut pick = self.rng.below(total_bytes);
            let mut address = None;
            for &(base, size) in &ranges {
                let size = u64::from(size);
                if pick < size {
                    address = Some(base.wrapping_add(pick as u32));
                    break;
                }
                pick -= size;
            }
            let Some(address) = address else { continue };
            let value = self.rng.next_u64() as u8;
            if cpu.write_mem(address, &[value]).is_ok() {
                corrupted = corrupted.saturating_add(1);
            }
        }
        self.total_corrupted = self.total_corrupted.saturating_add(corrupted);
        if corrupted > 0 && self.last_log.map_or(true, |t| t.elapsed().as_secs() >= 1) {
            log::debug!(
                "corrupted {corrupted} guest byte(s) across {} live allocation(s); {} total",
                ranges.len(),
                self.total_corrupted
            );
            self.last_log = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{stub::StubCpu, Cpu, Prot};

    fn setup(size: u32) -> (StubCpu, Heap, u32) {
        let mut cpu = StubCpu::new();
        cpu.map_region(0x5000_0000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let mut heap = Heap::new(0x5000_0000, 0x1000);
        let ptr = heap.alloc(size).unwrap();
        cpu.write_mem(ptr, &vec![0xaa; size as usize]).unwrap();
        (cpu, heap, ptr)
    }

    #[test]
    fn deterministic_for_seed() {
        let mut a = Rng::new(12345);
        let mut b = Rng::new(12345);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn disabled_does_nothing() {
        let (mut cpu, heap, ptr) = setup(256);
        let mut corruptor = Corruptor::new(CorruptionOptions {
            enabled: false,
            interval_frames: 1,
            ..CorruptionOptions::default()
        });
        corruptor.tick(&mut cpu, &heap);
        assert_eq!(cpu.read_mem(ptr, 256).unwrap(), vec![0xaa; 256]);
        assert_eq!(corruptor.total_corrupted(), 0);
    }

    #[test]
    fn interval_gates_burst() {
        let (mut cpu, heap, ptr) = setup(256);
        let mut corruptor = Corruptor::new(CorruptionOptions {
            interval_frames: 3,
            ..CorruptionOptions::default()
        });
        corruptor.tick(&mut cpu, &heap);
        corruptor.tick(&mut cpu, &heap);
        assert_eq!(corruptor.total_corrupted(), 0);
        corruptor.tick(&mut cpu, &heap);
        assert_eq!(corruptor.total_corrupted(), 1);
        assert_ne!(cpu.read_mem(ptr, 256).unwrap(), vec![0xaa; 256]);
    }

    #[test]
    fn max_offset_stays_inside_prefix() {
        let (mut cpu, heap, ptr) = setup(256);
        let mut corruptor = Corruptor::new(CorruptionOptions {
            interval_frames: 1,
            max_offset: Some(4),
            ..CorruptionOptions::default()
        });
        corruptor.tick(&mut cpu, &heap);
        let bytes = cpu.read_mem(ptr, 256).unwrap();
        assert!(bytes[..4].iter().any(|&byte| byte != 0xaa));
        assert!(bytes[4..].iter().all(|&byte| byte == 0xaa));
    }
}
