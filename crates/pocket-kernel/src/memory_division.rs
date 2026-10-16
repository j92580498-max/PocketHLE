//! Gizmondo device RAM, independent of guest virtual address arenas.
//! The SDK v1.5 section 2.1 supplies the nominal 44 MiB divisible pool
//! and 30% storage / 70% program boot split. These are device settings,
//! not a measurement of a particular ROM or of host RAM.
use std::sync::{Arc, Mutex};

pub const CE_PAGE_SIZE: u32 = 4096;
pub const SYSMEM_CHANGED: u32 = 0;
pub const SYSMEM_FAILED: u32 = 3;

pub fn pages(bytes: u64) -> u32 {
    bytes.div_ceil(CE_PAGE_SIZE as u64).min(u32::MAX as u64) as u32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RamSnapshot {
    pub total_pages: u32,
    pub store_pages: u32,
    pub program_used: u32,
    pub store_used: u32,
}
impl RamSnapshot {
    pub fn ram_pages(self) -> u32 {
        self.total_pages - self.store_pages
    }
    pub fn program_free(self) -> u32 {
        self.ram_pages().saturating_sub(self.program_used)
    }
    pub fn store_free(self) -> u32 {
        self.store_pages.saturating_sub(self.store_used)
    }
}

/// Clones refer to the SAME device, including its RAM files. In particular,
/// a suspended launcher still consumes program pages while a child executes.
#[derive(Clone, Debug)]
pub struct MemoryDivision {
    state: Arc<Mutex<RamSnapshot>>,
    pub(crate) files: Arc<Mutex<crate::vfs::RamStore>>,
    pub(crate) registry: Arc<Mutex<crate::registry::RegistryStore>>,
}
impl PartialEq for MemoryDivision {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state) || self.snapshot() == other.snapshot()
    }
}
impl Eq for MemoryDivision {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeError {
    InvalidSize,
    InUse,
}

impl MemoryDivision {
    pub fn new(divisible_bytes: u32, store_pages: u32) -> Option<Self> {
        if divisible_bytes == 0 || divisible_bytes % CE_PAGE_SIZE != 0 {
            return None;
        }
        let total_pages = divisible_bytes / CE_PAGE_SIZE;
        if store_pages >= total_pages {
            return None;
        }
        Some(Self {
            state: Arc::new(Mutex::new(RamSnapshot {
                total_pages,
                store_pages,
                program_used: 0,
                store_used: 0,
            })),
            files: Arc::new(Mutex::new(crate::vfs::RamStore::default())),
            registry: Arc::new(Mutex::new(crate::registry::RegistryStore::default())),
        })
    }
    pub fn gizmondo_sdk_default() -> Self {
        let total_pages = 44 * 1024 * 1024 / CE_PAGE_SIZE;
        Self::new(total_pages * CE_PAGE_SIZE, total_pages * 30 / 100).unwrap()
    }
    pub(crate) fn same_device(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
    pub fn snapshot(&self) -> RamSnapshot {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn store_pages(&self) -> u32 {
        self.snapshot().store_pages
    }
    pub fn ram_pages(&self) -> u32 {
        self.snapshot().ram_pages()
    }
    pub fn page_size(&self) -> u32 {
        CE_PAGE_SIZE
    }

    /// Only unoccupied pages may cross the boundary. HLE pages are movable,
    /// so an accepted change is effective immediately and needs no reboot.
    pub fn resize(&self, store_pages: u32) -> Result<(), ResizeError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // CE's object store has a 32 KiB minimum (pOEMCalcFSPages docs).
        if store_pages < 8 || store_pages >= state.total_pages {
            return Err(ResizeError::InvalidSize);
        }
        if store_pages < state.store_used || state.total_pages - store_pages < state.program_used {
            return Err(ResizeError::InUse);
        }
        state.store_pages = store_pages;
        Ok(())
    }
    pub(crate) fn acquire_program(&self, count: u32) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if count > state.program_free() {
            return false;
        }
        state.program_used += count;
        true
    }
    pub(crate) fn release_program(&self, count: u32) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert!(count <= state.program_used);
        state.program_used = state.program_used.saturating_sub(count);
    }
    pub(crate) fn acquire_store(&self, count: u32) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if count > state.store_free() {
            return false;
        }
        state.store_used += count;
        true
    }
    pub(crate) fn store_charge(&self, count: u32) -> Option<StoreCharge> {
        self.acquire_store(count).then(|| StoreCharge {
            state: Arc::downgrade(&self.state),
            count,
        })
    }
}
impl Default for MemoryDivision {
    fn default() -> Self {
        Self::gizmondo_sdk_default()
    }
}

/// A file's pages remain occupied after deletion while an open handle refers
/// to it. Weak ownership prevents the device's file table forming a cycle.
#[derive(Debug)]
pub(crate) struct StoreCharge {
    state: std::sync::Weak<Mutex<RamSnapshot>>,
    count: u32,
}
impl StoreCharge {
    pub fn resize(&mut self, count: u32) -> bool {
        let Some(state) = self.state.upgrade() else {
            return false;
        };
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        if count > self.count && count - self.count > state.store_free() {
            return false;
        }
        state.store_used = state.store_used - self.count + count;
        self.count = count;
        true
    }
}
impl Drop for StoreCharge {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.store_used -= self.count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory_division_sdk_profile_conserves_the_documented_pool() {
        let d = MemoryDivision::default();
        assert_eq!(
            (d.store_pages() + d.ram_pages()) * d.page_size(),
            44 * 1024 * 1024
        );
        assert_eq!(d.store_pages(), 3379);
        assert_eq!(d.ram_pages(), 7885);
    }
    #[test]
    fn memory_division_rejects_invalid_configuration() {
        assert!(MemoryDivision::new(0, 0).is_none());
        assert!(MemoryDivision::new(4097, 0).is_none());
        assert!(MemoryDivision::new(4096, 1).is_none());
    }
    #[test]
    fn memory_division_resize_is_shared_and_transactional() {
        let d = MemoryDivision::new(64 * CE_PAGE_SIZE, 32).unwrap();
        let child = d.clone();
        assert!(child.acquire_program(20));
        let file = d.store_charge(10).unwrap();
        assert_eq!(d.resize(45), Err(ResizeError::InUse));
        assert_eq!(d.resize(9), Err(ResizeError::InUse));
        assert_eq!(d.store_pages(), 32);
        d.resize(40).unwrap();
        assert_eq!(child.ram_pages(), 24);
        assert!(!child.acquire_program(5));
        drop(file);
        child.release_program(20);
        d.resize(8).unwrap();
        assert_eq!(d.snapshot().store_used, 0);
    }
}
