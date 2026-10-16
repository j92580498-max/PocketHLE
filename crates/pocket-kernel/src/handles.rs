//! Process namespaces and system-wide object identities for owned handles.
use crate::vfs::VfsObject;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HandleObject {
    Thread(usize),
    CurrentProcess,
    ChildProcess(u32),
    ChildThread(u32),
    Event(u32),
    Semaphore(u32),
    Mutex(u32),
    File(u32),
    Device(u32),
    RemoteThread(u32, usize),
    RemoteProcess(u32),
    RemoteFile(u32, u32),
    RemoteDevice(u32, u32),
}
#[derive(Clone, Copy, Default)]
pub struct ThreadUpdate {
    pub suspend_count: Option<u32>,
    pub terminate: Option<u32>,
}
#[derive(Clone, Copy)]
struct ThreadControl {
    tid: u32,
    suspend_count: u32,
}
#[derive(Default)]
struct Domain {
    api_gate: Arc<Mutex<()>>,
    next_tid: u32,
    threads: HashMap<(u32, usize), ThreadControl>,
    updates: HashMap<(u32, usize), ThreadUpdate>,
    process_kills: HashMap<u32, u32>,
    held_start: HashSet<u32>,
    entries: HashMap<(u32, u32), Option<HandleObject>>,
    names: HashMap<String, HandleObject>,
    next: u32,
    next_pid: u32,
    active: HashSet<u32>,
    exits: HashMap<(u32, Option<usize>), u32>,
    deferred_process_exits: HashSet<u32>,
    child_ids: HashMap<u32, u32>,
    files: HashMap<HandleObject, VfsObject>,
    imports: HashMap<(u32, u32), VfsObject>,
}
#[derive(Clone)]
pub struct HandleTable {
    domain: Arc<Mutex<Domain>>,
    pid: u32,
}
impl Default for HandleTable {
    fn default() -> Self {
        let mut domain = Domain {
            next: 0xd2000000,
            next_pid: 2,
            next_tid: 2,
            ..Domain::default()
        };
        domain.active.insert(1);
        domain.threads.insert(
            (1, 0),
            ThreadControl {
                tid: 1,
                suspend_count: 0,
            },
        );
        Self {
            domain: Arc::new(Mutex::new(domain)),
            pid: 1,
        }
    }
}
impl HandleTable {
    pub fn process_id(&self) -> u32 {
        self.pid
    }
    pub fn execution_gate(&self) -> Arc<Mutex<()>> {
        self.domain.lock().unwrap().api_gate.clone()
    }
    pub fn allocate_thread_id(&self) -> Option<u32> {
        let mut d = self.domain.lock().unwrap();
        let tid = d.next_tid;
        d.next_tid = tid.checked_add(1)?;
        Some(tid)
    }
    pub fn register_thread(&self, index: usize, count: u32, tid: u32) {
        self.domain
            .lock()
            .unwrap()
            .threads
            .entry((self.pid, index))
            .or_insert(ThreadControl {
                tid,
                suspend_count: count,
            });
    }
    pub fn thread_id(&self, index: usize) -> Option<u32> {
        self.domain
            .lock()
            .unwrap()
            .threads
            .get(&(self.pid, index))
            .map(|t| t.tid)
    }
    pub fn set_thread_count(&self, index: usize, count: u32) {
        if let Some(t) = self
            .domain
            .lock()
            .unwrap()
            .threads
            .get_mut(&(self.pid, index))
        {
            t.suspend_count = count;
        }
    }
    pub fn change_remote_suspend(&self, pid: u32, index: usize, suspend: bool) -> Result<u32, u32> {
        let mut d = self.domain.lock().unwrap();
        if !d.active.contains(&pid) || d.exits.contains_key(&(pid, Some(index))) {
            return Err(6);
        }
        let t = d.threads.get_mut(&(pid, index)).ok_or(6u32)?;
        let previous = t.suspend_count;
        if suspend && previous == 127 {
            return Err(156);
        }
        t.suspend_count = if suspend {
            previous + 1
        } else {
            previous.saturating_sub(1)
        };
        let count = t.suspend_count;
        d.updates.entry((pid, index)).or_default().suspend_count = Some(count);
        Ok(previous)
    }
    pub fn terminate_remote_thread(&self, pid: u32, index: usize, code: u32) -> bool {
        let mut d = self.domain.lock().unwrap();
        if !d.active.contains(&pid) || !d.threads.contains_key(&(pid, index)) {
            return false;
        }
        if !d.exits.contains_key(&(pid, Some(index))) {
            d.updates
                .entry((pid, index))
                .or_default()
                .terminate
                .get_or_insert(code);
        }
        true
    }
    pub fn terminate_remote_process(&self, pid: u32, code: u32) -> bool {
        let mut d = self.domain.lock().unwrap();
        if !d.active.contains(&pid) {
            return false;
        }
        d.process_kills.entry(pid).or_insert(code);
        true
    }
    pub fn take_controls(&self) -> (Option<u32>, Vec<(usize, ThreadUpdate)>) {
        let mut d = self.domain.lock().unwrap();
        let code = d.process_kills.remove(&self.pid);
        let keys: Vec<_> = d
            .updates
            .keys()
            .filter(|(pid, _)| *pid == self.pid)
            .copied()
            .collect();
        let updates = keys
            .into_iter()
            .map(|key| (key.1, d.updates.remove(&key).unwrap()))
            .collect();
        (code, updates)
    }
    pub fn hold_start(&self) {
        self.domain.lock().unwrap().held_start.insert(self.pid);
    }
    pub fn allow_start(&self) {
        self.domain.lock().unwrap().held_start.remove(&self.pid);
    }
    pub fn start_allowed(&self) -> bool {
        !self.domain.lock().unwrap().held_start.contains(&self.pid)
    }

    fn canonical(&self, object: HandleObject, domain: &Domain) -> HandleObject {
        match object {
            HandleObject::Thread(index) => HandleObject::RemoteThread(self.pid, index),
            HandleObject::CurrentProcess => HandleObject::RemoteProcess(self.pid),
            HandleObject::ChildProcess(key) => domain
                .child_ids
                .get(&key)
                .map_or(object, |&pid| HandleObject::RemoteProcess(pid)),
            HandleObject::ChildThread(key) => domain
                .child_ids
                .get(&key)
                .map_or(object, |&pid| HandleObject::RemoteThread(pid, 0)),
            HandleObject::File(key) => HandleObject::RemoteFile(self.pid, key),
            HandleObject::Device(key) => HandleObject::RemoteDevice(self.pid, key),
            _ => object,
        }
    }
    fn local(&self, object: HandleObject) -> HandleObject {
        match object {
            HandleObject::RemoteThread(pid, index) if pid == self.pid => {
                HandleObject::Thread(index)
            }
            HandleObject::RemoteProcess(pid) if pid == self.pid => HandleObject::CurrentProcess,
            HandleObject::RemoteFile(pid, key) if pid == self.pid => HandleObject::File(key),
            HandleObject::RemoteDevice(pid, key) if pid == self.pid => HandleObject::Device(key),
            _ => object,
        }
    }
    pub fn named(&self, name: &str) -> Option<HandleObject> {
        self.domain
            .lock()
            .unwrap()
            .names
            .get(name)
            .copied()
            .map(|o| self.local(o))
    }
    pub fn name(&mut self, name: String, object: HandleObject) {
        let mut d = self.domain.lock().unwrap();
        let object = self.canonical(object, &d);
        d.names.insert(name, object);
    }
    pub fn get(&self, handle: u32) -> Option<HandleObject> {
        self.get_in(self.pid, handle)
    }
    pub fn get_in(&self, pid: u32, handle: u32) -> Option<HandleObject> {
        self.domain
            .lock()
            .unwrap()
            .entries
            .get(&(pid, handle))
            .copied()
            .flatten()
            .map(|o| self.local(o))
    }
    pub fn contains(&self, handle: u32) -> bool {
        self.domain
            .lock()
            .unwrap()
            .entries
            .contains_key(&(self.pid, handle))
    }
    pub fn known_elsewhere(&self, handle: u32) -> bool {
        self.domain
            .lock()
            .unwrap()
            .entries
            .keys()
            .any(|(p, h)| *p != self.pid && *h == handle)
    }
    pub fn is_closed(&self, handle: u32) -> bool {
        self.domain
            .lock()
            .unwrap()
            .entries
            .get(&(self.pid, handle))
            .is_some_and(|o| o.is_none())
    }
    pub fn is_empty(&self) -> bool {
        !self
            .domain
            .lock()
            .unwrap()
            .entries
            .keys()
            .any(|(p, _)| *p == self.pid)
    }
    pub fn bind(&mut self, handle: u32, object: HandleObject) {
        self.bind_in(self.pid, handle, object);
    }
    pub fn bind_in(&mut self, pid: u32, handle: u32, object: HandleObject) {
        let mut d = self.domain.lock().unwrap();
        let object = self.canonical(object, &d);
        d.entries.entry((pid, handle)).or_insert(Some(object));
    }
    pub fn reserve(&mut self) -> Option<u32> {
        let mut d = self.domain.lock().unwrap();
        loop {
            let handle = d.next;
            d.next = d.next.checked_add(1)?;
            if !d.entries.keys().any(|(_, h)| *h == handle) {
                return Some(handle);
            }
        }
    }
    pub fn duplicate(&mut self, object: HandleObject) -> Option<u32> {
        let handle = self.reserve()?;
        self.bind(handle, object);
        Some(handle)
    }
    pub fn close(&mut self, handle: u32) -> Option<(HandleObject, bool)> {
        self.close_in(self.pid, handle)
    }
    pub fn close_in(&mut self, pid: u32, handle: u32) -> Option<(HandleObject, bool)> {
        let mut d = self.domain.lock().unwrap();
        let object = d.entries.get_mut(&(pid, handle))?.take()?;
        d.imports.remove(&(pid, handle));
        let last = !d.entries.values().any(|o| *o == Some(object));
        if last {
            d.names.retain(|_, o| *o != object);
            d.files.remove(&object);
        }
        Some((self.local(object), last))
    }
    pub fn invalidate_files(&mut self) {
        let handles: Vec<_> = self
            .owned()
            .into_iter()
            .filter_map(|(h, o)| {
                matches!(o, HandleObject::File(_) | HandleObject::RemoteFile(..)).then_some(h)
            })
            .collect();
        for h in handles {
            self.close(h);
        }
    }
    pub fn owned(&self) -> Vec<(u32, HandleObject)> {
        self.domain
            .lock()
            .unwrap()
            .entries
            .iter()
            .filter_map(|(&(p, h), &o)| {
                (p == self.pid)
                    .then(|| o.map(|o| (h, self.local(o))))
                    .flatten()
            })
            .collect()
    }
    pub fn new_child(&mut self) -> Option<(u32, HandleTable)> {
        let mut d = self.domain.lock().unwrap();
        let pid = d.next_pid;
        d.next_pid = pid.checked_add(1)?;
        let handle = 0xd1000000u32.checked_add(pid.checked_mul(2)?)?;
        if handle >= 0xd2000000 {
            return None;
        }
        let tid = d.next_tid;
        d.next_tid = tid.checked_add(1)?;
        d.threads.insert(
            (pid, 0),
            ThreadControl {
                tid,
                suspend_count: 0,
            },
        );
        d.active.insert(pid);
        d.child_ids.insert(handle, pid);
        Some((
            handle,
            Self {
                domain: self.domain.clone(),
                pid,
            },
        ))
    }
    pub fn child(&self, handle: u32) -> Option<Self> {
        let pid = *self.domain.lock().unwrap().child_ids.get(&handle)?;
        Some(Self {
            domain: self.domain.clone(),
            pid,
        })
    }
    pub fn child_id(&self, handle: u32) -> Option<u32> {
        self.domain.lock().unwrap().child_ids.get(&handle).copied()
    }
    pub fn focus_after_exit(&self, preferred: u32) -> u32 {
        let d = self.domain.lock().unwrap();
        if d.active.contains(&preferred) {
            preferred
        } else {
            d.active.iter().copied().min().unwrap_or(preferred)
        }
    }
    pub fn is_active(&self, pid: u32) -> bool {
        self.domain.lock().unwrap().active.contains(&pid)
    }
    pub fn set_exit(&self, thread: Option<usize>, code: u32) {
        self.domain
            .lock()
            .unwrap()
            .exits
            .insert((self.pid, thread), code);
    }
    pub fn exit_code(&self, pid: u32, thread: Option<usize>) -> Option<u32> {
        let d = self.domain.lock().unwrap();
        if thread.is_none() && d.deferred_process_exits.contains(&pid) {
            return None;
        }
        d.exits.get(&(pid, thread)).copied()
    }
    /// A host-run child must not wake process waiters before its CPU image and
    /// heap have been dropped. Thread exit remains independently observable.
    pub fn defer_process_exit(&self) {
        self.domain
            .lock()
            .unwrap()
            .deferred_process_exits
            .insert(self.pid);
    }
    /// Called by the host after dropping the child's emulator and private RAM.
    pub fn complete_process_exit(&self, code: u32) {
        let mut d = self.domain.lock().unwrap();
        d.exits.insert((self.pid, None), code);
        d.deferred_process_exits.remove(&self.pid);
    }
    pub fn mark_inactive(&self) {
        let mut d = self.domain.lock().unwrap();
        d.active.remove(&self.pid);
        d.held_start.remove(&self.pid);
        d.process_kills.remove(&self.pid);
        d.updates.retain(|(pid, _), _| *pid != self.pid);
    }
    pub fn export_file(&self, object: HandleObject, file: VfsObject) {
        let mut d = self.domain.lock().unwrap();
        let object = self.canonical(object, &d);
        d.files.entry(object).or_insert(file);
    }
    pub fn file(&self, object: HandleObject) -> Option<VfsObject> {
        let d = self.domain.lock().unwrap();
        d.files.get(&self.canonical(object, &d)).cloned()
    }
    pub fn queue_file(&self, pid: u32, handle: u32, file: VfsObject) {
        self.domain
            .lock()
            .unwrap()
            .imports
            .insert((pid, handle), file);
    }
    pub fn take_files(&self) -> Vec<(u32, VfsObject)> {
        let mut d = self.domain.lock().unwrap();
        let keys: Vec<_> = d
            .imports
            .keys()
            .filter(|(p, _)| *p == self.pid)
            .copied()
            .collect();
        keys.into_iter()
            .map(|key| (key.1, d.imports.remove(&key).unwrap()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_wait_stays_pending_until_host_releases_child_ram() {
        use crate::{memory_division::MemoryDivision, Heap};
        let mut parent = HandleTable::default();
        let (_, child) = parent.new_child().unwrap();
        let pid = child.process_id();
        let ram = MemoryDivision::new(32 * 4096, 16).unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        child.defer_process_exit();
        let budget = ram.clone();
        let worker = std::thread::spawn(move || {
            let mut heap = Heap::new(0x100000, 0x40000);
            assert!(heap.attach_ram(Some(budget)));
            heap.alloc(4 * 4096 - 8).unwrap();
            child.set_exit(Some(0), 74);
            child.set_exit(None, 74);
            child.mark_inactive();
            ready_tx.send(()).unwrap();
            // Force the race window: guest exit has happened but host cleanup
            // has deliberately not refunded the child's four pages yet.
            release_rx.recv().unwrap();
            drop(heap);
            child.complete_process_exit(74);
        });
        ready_rx.recv().unwrap();
        assert_eq!(ram.snapshot().program_used, 4);
        assert_eq!(parent.exit_code(pid, Some(0)), Some(74));
        assert_eq!(parent.exit_code(pid, None), None);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(parent.exit_code(pid, None), Some(74));
        assert_eq!(ram.snapshot().program_used, 0);
    }
    #[test]
    fn handles_share_identity_until_the_last_reference_closes() {
        let mut table = HandleTable::default();
        let object = HandleObject::Event(0xdeade001);
        table.bind(0xdeade001, object);
        let alias = table.duplicate(object).unwrap();
        assert!(!(0xd1000000..0xd2000000).contains(&alias));
        assert_eq!(table.close(0xdeade001), Some((object, false)));
        assert_eq!(table.get(alias), Some(object));
        assert_eq!(table.close(alias), Some((object, true)));
        assert_eq!(table.close(alias), None);
        assert!(table.is_closed(alias));
    }
    #[test]
    fn cross_process_namespaces_share_identity_and_last_reference() {
        let mut parent = HandleTable::default();
        let (_, mut child) = parent.new_child().unwrap();
        parent.bind(11, HandleObject::Thread(0));
        let object = child.get_in(parent.process_id(), 11).unwrap();
        assert_eq!(object, HandleObject::RemoteThread(parent.process_id(), 0));
        child.bind(12, object);
        assert_eq!(parent.get(12), None);
        assert_eq!(parent.close(11), Some((HandleObject::Thread(0), false)));
        parent.set_exit(Some(0), 7);
        assert_eq!(child.exit_code(parent.process_id(), Some(0)), Some(7));
        assert_eq!(child.close(12), Some((object, true)));
    }
    #[test]
    fn process_primary_and_worker_ids_are_unique_in_the_shared_domain() {
        let mut root = HandleTable::default();
        let first = root.allocate_thread_id().unwrap();
        root.register_thread(1, 0, first);
        let (_, child) = root.new_child().unwrap();
        let worker = child.allocate_thread_id().unwrap();
        child.register_thread(1, 1, worker);
        assert_eq!(root.thread_id(0), Some(1));
        let ids = [1, first, child.thread_id(0).unwrap(), worker];
        assert_eq!(ids.into_iter().collect::<HashSet<_>>().len(), 4);
    }
    #[test]
    fn remote_suspend_counts_limit_and_termination_are_owned_requests() {
        let mut root = HandleTable::default();
        let (_, child) = root.new_child().unwrap();
        let pid = child.process_id();
        for count in 0..127 {
            assert_eq!(root.change_remote_suspend(pid, 0, true), Ok(count));
        }
        assert_eq!(root.change_remote_suspend(pid, 0, true), Err(156));
        assert_eq!(root.change_remote_suspend(pid, 0, false), Ok(127));
        assert!(root.terminate_remote_thread(pid, 0, 33));
        let (kill, updates) = child.take_controls();
        assert_eq!(kill, None);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].1.suspend_count, Some(126));
        assert_eq!(updates[0].1.terminate, Some(33));
        child.set_exit(Some(0), 33);
        assert_eq!(root.change_remote_suspend(pid, 0, false), Err(6));
        assert!(root.terminate_remote_process(pid, 77));
        assert_eq!(child.take_controls().0, Some(77));
        child.mark_inactive();
        assert!(!root.terminate_remote_process(pid, 99));
    }
}
