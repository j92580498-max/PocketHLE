//! Deferred image pages. Backing bytes are discarded after the first page-in.
use crate::{CpuError, Prot};
use std::{collections::BTreeMap, sync::Arc};

/// The kernel owns device RAM policy; CPU backends only acquire/release pages.
pub trait ImagePageBudget {
    fn acquire(&self) -> bool;
    fn release(&self);
}

pub(crate) struct ImagePage {
    pub bytes: Vec<u8>,
    pub prot: Prot,
    pub resident: bool,
    pub budget: Arc<dyn ImagePageBudget>,
}
impl Drop for ImagePage {
    fn drop(&mut self) {
        if self.resident {
            self.budget.release();
        }
    }
}
#[derive(Default)]
pub(crate) struct ImagePages {
    pub pages: BTreeMap<u32, ImagePage>,
}
impl ImagePages {
    pub fn reserve(
        &mut self,
        va: u32,
        size: u32,
        prot: Prot,
        bytes: Vec<u8>,
        budget: Arc<dyn ImagePageBudget>,
    ) -> Result<(), CpuError> {
        let end = va
            .checked_add(size)
            .ok_or(CpuError::BadMemory { va, size })?;
        if size == 0
            || !va.is_multiple_of(4096)
            || !size.is_multiple_of(4096)
            || bytes.len() > size as usize
            || (va..end)
                .step_by(4096)
                .any(|page| self.pages.contains_key(&page))
        {
            return Err(CpuError::BadMemory { va, size });
        }
        for page in (va..end).step_by(4096) {
            let start = (page - va) as usize;
            let data = if start < bytes.len() {
                bytes[start..(start + 4096).min(bytes.len())].to_vec()
            } else {
                vec![]
            };
            self.pages.insert(
                page,
                ImagePage {
                    bytes: data,
                    prot,
                    resident: false,
                    budget: budget.clone(),
                },
            );
        }
        Ok(())
    }
    pub fn range(va: u32, len: u32) -> Result<impl Iterator<Item = u32>, CpuError> {
        let (first, end) = if len == 0 {
            (0, 0)
        } else {
            let last = va
                .checked_add(len - 1)
                .ok_or(CpuError::BadMemory { va, size: len })?;
            (va / 4096, last / 4096 + 1)
        };
        Ok((first..end).map(|page| page * 4096))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    pub struct Budget {
        pub used: AtomicU32,
        pub limit: AtomicU32,
    }
    impl Budget {
        pub fn new(limit: u32) -> Arc<Self> {
            Arc::new(Self {
                used: AtomicU32::new(0),
                limit: AtomicU32::new(limit),
            })
        }
        pub fn count(&self) -> u32 {
            self.used.load(Ordering::SeqCst)
        }
    }
    impl ImagePageBudget for Budget {
        fn acquire(&self) -> bool {
            self.used
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n < self.limit.load(Ordering::SeqCst)).then_some(n + 1)
                })
                .is_ok()
        }
        fn release(&self) {
            assert!(self.used.fetch_sub(1, Ordering::SeqCst) > 0);
        }
    }
    fn host_accesses(cpu: &mut dyn crate::Cpu) {
        let budget = Budget::new(8);
        let mut data = vec![0; 0x3000];
        data[0] = 17;
        data[0x1fff] = 23;
        assert!(cpu
            .map_image_region(0x1000, 0x5000, Prot::ALL, data, budget.clone())
            .unwrap());
        assert_eq!(budget.count(), 0);
        assert_eq!(cpu.read_u8(0x1000).unwrap(), 17);
        assert_eq!(budget.count(), 1);
        assert_eq!(cpu.read_u8(0x1000).unwrap(), 17);
        assert_eq!(budget.count(), 1);
        cpu.write_mem(0x1ffe, &[1, 2, 3, 4]).unwrap();
        assert_eq!(cpu.read_mem(0x1ffe, 4).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(budget.count(), 2);
        assert_eq!(cpu.read_u8(0x5000).unwrap(), 0);
        assert_eq!(budget.count(), 3);
        assert!(cpu.map_region(0x4000, 4096, Prot::ALL).is_err());
        assert!(cpu.unmap_region(0x1000, 0x6000).is_err()); // atomic validation
        assert_eq!(cpu.read_u8(0x1000).unwrap(), 17);
        cpu.unmap_region(0x1000, 0x5000).unwrap();
        assert_eq!(budget.count(), 0);
        assert!(cpu.read_u8(0x1000).is_err());
        cpu.map_image_region(0x1000, 4096, Prot::ALL, vec![29], budget.clone())
            .unwrap();
        assert_eq!(cpu.read_u8(0x1000).unwrap(), 29);
        cpu.unmap_region(0x1000, 4096).unwrap();
        assert_eq!(budget.count(), 0);
    }
    fn out_of_ram(cpu: &mut dyn crate::Cpu) {
        let budget = Budget::new(1);
        cpu.map_image_region(0x1000, 0x3000, Prot::ALL, vec![7], budget.clone())
            .unwrap();
        assert_eq!(cpu.read_u8(0x1000).unwrap(), 7);
        assert!(cpu.read_u8(0x2000).is_err());
        assert!(cpu.read_u8(0x2000).is_err());
        assert_eq!(budget.count(), 1);
        budget.limit.store(2, Ordering::SeqCst);
        assert_eq!(cpu.read_u8(0x2000).unwrap(), 0);
        assert_eq!(budget.count(), 2);
        cpu.unmap_region(0x1000, 0x3000).unwrap();
        assert_eq!(budget.count(), 0);
    }
    #[test]
    fn stub_pages_only_host_accesses_and_discards_unloaded_backing() {
        host_accesses(&mut crate::stub::StubCpu::new());
    }
    #[test]
    fn stub_page_in_failure_is_retryable_and_refunds_exactly() {
        out_of_ram(&mut crate::stub::StubCpu::new());
    }
    #[cfg(feature = "unicorn")]
    #[test]
    fn unicorn_pages_only_host_accesses_and_discards_unloaded_backing() {
        host_accesses(&mut crate::unicorn::UnicornCpu::new().unwrap());
    }
    #[cfg(feature = "unicorn")]
    #[test]
    fn unicorn_page_in_failure_is_retryable_and_refunds_exactly() {
        out_of_ram(&mut crate::unicorn::UnicornCpu::new().unwrap());
    }
    #[cfg(feature = "unicorn")]
    #[test]
    fn guest_fetch_and_store_page_in_without_hle_calls() {
        use crate::{regs::ArmReg, Cpu, StopReason};
        let mut cpu = crate::unicorn::UnicornCpu::new().unwrap();
        let budget = Budget::new(8);
        let words = [0xe59f1008u32, 0xe3a0004d, 0xe5810000, 0xe12fff1e, 0x2000];
        let code = words.into_iter().flat_map(u32::to_le_bytes).collect();
        cpu.map_image_region(0x1000, 0x3000, Prot::ALL, code, budget.clone())
            .unwrap();
        cpu.map_region(0x9000, 4096, Prot::ALL).unwrap();
        cpu.add_code_hook(0x9000).unwrap();
        cpu.write_reg(ArmReg::Lr, 0x9000).unwrap();
        assert_eq!(budget.count(), 0);
        assert_eq!(
            cpu.run_until_hook(0x1000, 100).unwrap(),
            StopReason::Hook(0x9000)
        );
        assert_eq!(budget.count(), 2); // third page has never been touched
        assert_eq!(cpu.read_u32_le(0x2000).unwrap(), 77);
        cpu.unmap_region(0x1000, 0x3000).unwrap();
        assert_eq!(budget.count(), 0);
    }
    fn api_permissions(cpu: &mut dyn crate::Cpu) {
        let budget = Budget::new(8);
        cpu.map_image_region(0x1000, 4096, Prot::READ, vec![7], budget.clone())
            .unwrap();
        assert!(cpu.check_guest_access(0x1000, 4096, Prot::READ).is_ok());
        assert!(cpu.check_guest_access(0x1000, 1, Prot::WRITE).is_err());
        assert_eq!(budget.count(), 0);
        cpu.map_region(0x4000, 8192, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.protect_region(0x5000, 4096, Prot::READ).unwrap();
        assert!(cpu.check_guest_access(0x4fff, 2, Prot::WRITE).is_err());
        assert!(cpu.check_guest_access(0x4fff, 2, Prot::READ).is_ok());
        assert!(cpu.check_guest_access(0x6000, 1, Prot::READ).is_err());
        assert!(cpu.check_guest_access(u32::MAX, 2, Prot::WRITE).is_err());
        // Loader access is deliberately unaffected by guest permissions.
        cpu.write_mem(0x5000, b"loader").unwrap();
        assert_eq!(cpu.read_mem(0x5000, 6).unwrap(), b"loader");
        cpu.unmap_region(0x1000, 4096).unwrap();
        cpu.unmap_region(0x4000, 8192).unwrap();
        assert_eq!(budget.count(), 0);
    }
    #[test]
    fn stub_guest_api_permissions_do_not_commit_cold_pages() {
        api_permissions(&mut crate::stub::StubCpu::new());
    }
    #[cfg(feature = "unicorn")]
    #[test]
    fn unicorn_guest_api_permissions_do_not_commit_cold_pages() {
        api_permissions(&mut crate::unicorn::UnicornCpu::new().unwrap());
    }
}
