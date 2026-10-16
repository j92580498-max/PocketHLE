//! File contracts shared by all CPUs of one emulated device.
use super::*;
use std::sync::Weak;
#[derive(Clone, Default)]
pub struct VfsShared(Arc<Mutex<Shared>>);
#[derive(Default)]
struct Shared {
    leases: HashMap<String, Vec<Weak<Lease>>>,
    attributes: HashMap<String, u32>,
    bluetooth: crate::bluetooth::Service,
    camera: crate::camera::Service,
    gps: crate::gps::Service,
    vibration: crate::vibration::Service,
}
#[derive(Debug)]
pub(super) struct Lease {
    pub access: u32,
    share: u32,
    pub(super) volume: Option<(PathBuf, u64)>,
}
#[derive(Debug, PartialEq, Eq)]
pub struct OpenResult {
    pub handle: u32,
    pub existed: bool,
}
fn io_error(e: &std::io::Error) -> u32 {
    match e.kind() {
        std::io::ErrorKind::NotFound => 2,
        std::io::ErrorKind::PermissionDenied => 5,
        std::io::ErrorKind::AlreadyExists => 183,
        std::io::ErrorKind::DirectoryNotEmpty => 145,
        std::io::ErrorKind::InvalidInput => 87,
        _ => 29,
    }
}
fn used(root: &Path) -> Result<u64, u32> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(root).map_err(|e| io_error(&e))? {
        let e = entry.map_err(|e| io_error(&e))?;
        let m = e.path().symlink_metadata().map_err(|e| io_error(&e))?;
        if m.is_symlink() {
            continue;
        }
        total = total.saturating_add(if m.is_dir() {
            used(&e.path())?
        } else {
            m.len()
        });
    }
    Ok(total)
}
fn busy(state: &mut Shared, key: &str) -> bool {
    state.leases.retain(|_, v| {
        v.retain(|l| l.strong_count() != 0);
        !v.is_empty()
    });
    state.leases.get(key).is_some_and(|v| !v.is_empty())
}
impl Vfs {
    /// Copy through guest file contracts, including sharing and volume quota.
    pub fn copy_file(&mut self, from: &str, to: &str, fail_if_exists: bool) -> Result<(), u32> {
        let source = self.open_file(from, 1, 1, 3, false, 0)?.handle;
        let result = (|| {
            let size = self.size(source).ok_or(30u32)?;
            let dest_key = self.file_key(to, true)?;
            let source_key = if let Some(file) = self.handles.get(&source) {
                file.lock()
                    .map_err(|_| 30u32)?
                    .host_path
                    .canonicalize()
                    .map_err(|e| io_error(&e))?
                    .to_string_lossy()
                    .to_ascii_lowercase()
            } else {
                self.file_key(from, false)?
            };
            if source_key == dest_key {
                return Err(87);
            }
            if fail_if_exists && self.attributes(to).is_ok() {
                return Err(80);
            }
            if !self.is_ram_path(to) {
                let (host, mount) = self.exact_path(to, true)?;
                if let Some((root, total)) = Self::quota(mount) {
                    let old = host.metadata().map(|m| m.len()).unwrap_or(0);
                    if size.saturating_sub(old) > total.saturating_sub(used(&root)?) {
                        return Err(112);
                    }
                }
            }
            let attrs = self.attributes(from)? & 0x27;
            let dest = self
                .open_file(to, 2, 0, if fail_if_exists { 1 } else { 2 }, false, 0)?
                .handle;
            let copied = (|| {
                let mut buffer = [0u8; 16384];
                loop {
                    let count = self.read(source, &mut buffer).ok_or(30u32)?;
                    if count == 0 {
                        break;
                    }
                    let mut written = 0;
                    while written < count {
                        let n = self.write(dest, &buffer[written..count]).ok_or(112u32)?;
                        if n == 0 {
                            return Err(29);
                        }
                        written += n;
                    }
                }
                self.flush(dest).map_err(|e| io_error(&e))
            })();
            self.close(dest);
            if copied.is_err() && fail_if_exists {
                let _ = self.delete(to);
            }
            copied?;
            self.set_attributes(to, attrs)
        })();
        self.close(source);
        result
    }
    pub fn shared_context(&self) -> VfsShared {
        self.shared.clone()
    }
    pub fn attach_shared_context(&mut self, shared: VfsShared) {
        self.bluetooth.service = shared.0.lock().unwrap().bluetooth.clone();
        self.shared = shared;
    }
    pub fn vibration_service(&self) -> crate::vibration::Service {
        self.shared.0.lock().unwrap().vibration.clone()
    }
    pub fn set_vibration_service(&self, service: crate::vibration::Service) {
        self.shared.0.lock().unwrap().vibration = service;
    }
    pub fn gps_service(&self) -> crate::gps::Service {
        self.shared.0.lock().unwrap().gps.clone()
    }
    pub fn set_gps_service(&self, service: crate::gps::Service) {
        self.shared.0.lock().unwrap().gps = service;
    }
    pub fn camera_service(&self) -> crate::camera::Service {
        self.shared.0.lock().unwrap().camera.clone()
    }
    pub fn set_camera_service(&self, service: crate::camera::Service) {
        self.shared.0.lock().unwrap().camera = service;
    }
    pub(super) fn bluetooth_service(&self) -> crate::bluetooth::Service {
        self.shared.0.lock().unwrap().bluetooth.clone()
    }
    pub(super) fn set_shared_bluetooth_service(&self, service: crate::bluetooth::Service) {
        self.shared.0.lock().unwrap().bluetooth = service;
    }
    fn exact_path(&self, path: &str, write: bool) -> Result<(PathBuf, &Mount), u32> {
        let normal = self.normalise_guest_path(path);
        let mounts = self.matching_mounts(&normal);
        let first = *mounts.first().ok_or(3u32)?;
        // A more specific read-only mount cannot be bypassed through a broader writable root.
        let mount = if write {
            mounts
                .into_iter()
                .find(|m| m.prefix == first.prefix && !m.read_only)
                .ok_or(5u32)?
        } else {
            first
        };
        let host = self.host_path_for_mount(mount, &normal).ok_or(5u32)?;
        let root = mount.host_dir.canonicalize().map_err(|e| io_error(&e))?;
        let ancestor = host
            .ancestors()
            .find(|p| p.exists())
            .ok_or(3u32)?
            .canonicalize()
            .map_err(|e| io_error(&e))?;
        if !ancestor.starts_with(root) {
            return Err(5);
        }
        Ok((host, mount))
    }
    fn file_key(&self, path: &str, write: bool) -> Result<String, u32> {
        if self.is_ram_path(path) {
            return self.ram_key(path).map(|k| format!("ram:{k}")).ok_or(87);
        }
        let (host, _) = self.exact_path(path, write)?;
        let resolved = if host.exists() {
            host.canonicalize().map_err(|e| io_error(&e))?
        } else {
            let parent = host
                .parent()
                .ok_or(3u32)?
                .canonicalize()
                .map_err(|_| 3u32)?;
            parent.join(host.file_name().ok_or(87u32)?)
        };
        Ok(resolved.to_string_lossy().to_ascii_lowercase())
    }
    fn quota(mount: &Mount) -> Option<(PathBuf, u64)> {
        (mount.prefix == "/flash disk/").then(|| (mount.host_dir.clone(), 32 * 1024 * 1024))
    }
    /// Legacy internal callers retain their create-parent convenience; guest APIs use open_file.
    pub fn open(&mut self, path: &str, access: Access, create: bool) -> Option<u32> {
        if create && !self.is_ram_path(path) {
            if let Ok((host, _)) = self.exact_path(path, true) {
                if let Some(p) = host.parent() {
                    std::fs::create_dir_all(p).ok();
                }
            }
        }
        let bits = match access {
            Access::Read => 1,
            Access::Write => 2,
            Access::ReadWrite => 3,
        };
        let disposition = if create {
            if access == Access::Write {
                2
            } else {
                4
            }
        } else {
            3
        };
        self.open_file(path, bits, 3, disposition, false, 0)
            .ok()
            .map(|r| r.handle)
    }
    pub fn open_file(
        &mut self,
        path: &str,
        access: u32,
        share: u32,
        disposition: u32,
        append: bool,
        attributes: u32,
    ) -> Result<OpenResult, u32> {
        if path.is_empty() || access > 3 || share & !3 != 0 || !(1..=5).contains(&disposition) {
            return Err(87);
        }
        let normal = self.normalise_guest_path(path);
        if normal.rsplit('/').next() == Some("bth1:") {
            if disposition != 3 {
                return Err(87);
            }
            let mut state = self.shared.0.lock().unwrap();
            let key = "bluetooth:BTH1";
            busy(&mut state, key);
            if state.leases.get(key).is_some_and(|leases| {
                leases
                    .iter()
                    .filter_map(Weak::upgrade)
                    .any(|l| access & !l.share != 0 || l.access & !share != 0)
            }) {
                return Err(32);
            }
            let lease = Arc::new(Lease {
                access,
                share,
                volume: None,
            });
            let handle = self.next_handle;
            self.next_handle = self.next_handle.checked_add(1).ok_or(8u32)?;
            self.bluetooth_driver_handles.insert(
                handle,
                Arc::new(BluetoothDriverOpen {
                    service: state.bluetooth.clone(),
                    access,
                    _lease: lease.clone(),
                }),
            );
            state
                .leases
                .entry(key.into())
                .or_default()
                .push(Arc::downgrade(&lease));
            return Ok(OpenResult {
                handle,
                existed: true,
            });
        }
        if normal.rsplit('/').next() == Some("vib1:") {
            if disposition != 3 {
                return Err(87);
            }
            let mut state = self.shared.0.lock().unwrap();
            let key = "vibration:VIB1";
            busy(&mut state, key);
            if state.leases.get(key).is_some_and(|leases| {
                leases
                    .iter()
                    .filter_map(Weak::upgrade)
                    .any(|l| access & !l.share != 0 || l.access & !share != 0)
            }) {
                return Err(32);
            }
            let service = state.vibration.clone();
            let lease = Arc::new(Lease {
                access,
                share,
                volume: None,
            });
            let handle = self.next_handle;
            self.next_handle = self.next_handle.checked_add(1).ok_or(8u32)?;
            self.vibration_handles.insert(
                handle,
                Arc::new(VibrationOpen {
                    service,
                    access,
                    _lease: lease.clone(),
                }),
            );
            state
                .leases
                .entry(key.into())
                .or_default()
                .push(Arc::downgrade(&lease));
            return Ok(OpenResult {
                handle,
                existed: true,
            });
        }
        if normal.rsplit('/').next() == Some("gps1:") {
            if disposition != 3 {
                return Err(87);
            }
            let mut state = self.shared.0.lock().unwrap();
            let key = "gps:GPS1";
            busy(&mut state, key);
            if state.leases.get(key).is_some_and(|leases| {
                leases.iter().filter_map(Weak::upgrade).any(|l| {
                    access & !l.share != 0
                        || l.access & !share != 0
                        || (access & 2 != 0 && l.access & 2 != 0)
                })
            }) {
                return Err(32);
            }
            let device = state.gps.open()?;
            let lease = Arc::new(Lease {
                access,
                share,
                volume: None,
            });
            let handle = self.next_handle;
            self.next_handle = self.next_handle.checked_add(1).ok_or(8u32)?;
            self.gps_handles.insert(
                handle,
                Arc::new(GpsOpen {
                    device,
                    access,
                    _lease: lease.clone(),
                }),
            );
            state
                .leases
                .entry(key.into())
                .or_default()
                .push(Arc::downgrade(&lease));
            return Ok(OpenResult {
                handle,
                existed: true,
            });
        }
        if normal.rsplit('/').next() == Some("cam1:") {
            if disposition != 3 {
                return Err(87);
            }
            let mut state = self.shared.0.lock().unwrap();
            let key = "camera:CAM1";
            busy(&mut state, key);
            if state.leases.get(key).is_some_and(|leases| {
                leases
                    .iter()
                    .filter_map(Weak::upgrade)
                    .any(|l| access & !l.share != 0 || l.access & !share != 0)
            }) {
                return Err(32);
            }
            let device = state.camera.open()?;
            let lease = Arc::new(Lease {
                access,
                share,
                volume: None,
            });
            let handle = self.next_handle;
            self.next_handle = self.next_handle.checked_add(1).ok_or(8u32)?;
            self.camera_handles.insert(
                handle,
                Arc::new(CameraOpen {
                    device,
                    access,
                    _lease: lease.clone(),
                }),
            );
            state
                .leases
                .entry(key.into())
                .or_default()
                .push(Arc::downgrade(&lease));
            return Ok(OpenResult {
                handle,
                existed: true,
            });
        }

        if let Some(index) = normal
            .rsplit('/')
            .next()
            .and_then(|s| s.strip_prefix("com"))
            .and_then(|s| s.strip_suffix(':'))
            .and_then(|s| s.parse::<u32>().ok())
        {
            if disposition != 3 {
                return Err(87);
            }
            let (registration, port) = self.bluetooth.service.registered_port(index).ok_or(2u32)?;
            let key = format!("bluetooth:COM{index}:{registration}");
            let mut state = self.shared.0.lock().unwrap();
            busy(&mut state, &key);
            if state.leases.get(&key).is_some_and(|leases| {
                leases
                    .iter()
                    .filter_map(Weak::upgrade)
                    .any(|l| access & !l.share != 0 || l.access & !share != 0)
            }) {
                return Err(32);
            }
            let lease = Arc::new(Lease {
                access,
                share,
                volume: None,
            });
            let handle = self.next_handle;
            self.next_handle = self.next_handle.checked_add(1).ok_or(8u32)?;
            self.bluetooth_handles.insert(
                handle,
                Arc::new(BluetoothOpen {
                    port,
                    access,
                    _lease: lease.clone(),
                }),
            );
            state
                .leases
                .entry(key)
                .or_default()
                .push(Arc::downgrade(&lease));
            return Ok(OpenResult {
                handle,
                existed: true,
            });
        }
        let device = normal
            .rsplit('/')
            .next()
            .is_some_and(|p| p == "mas1:" || p == "reg1:" || p == "vol:");
        if device {
            if disposition != 3 {
                return Err(87);
            }
            let a = match access {
                2 => Access::Write,
                3 => Access::ReadWrite,
                _ => Access::Read,
            };
            return self
                .open_legacy(path, a, false)
                .map(|handle| OpenResult {
                    handle,
                    existed: true,
                })
                .ok_or(2);
        }
        if disposition == 5 && access & 2 == 0 {
            return Err(87);
        }
        let mut host = None;
        let mut volume = None;
        let mut read_only = false;
        let key = if self.is_ram_path(path) {
            self.file_key(path, false)?
        } else {
            let (exact, mount) = self.exact_path(
                path,
                access & 2 != 0 || disposition == 1 || disposition == 2,
            )?;
            volume = Self::quota(mount);
            read_only = mount.read_only;
            // Preserve the validated wrapped-install fallback for reads only.
            let target = if access == 1 && disposition == 3 {
                self.resolve(path).unwrap_or(exact)
            } else {
                exact
            };
            let root = mount.host_dir.canonicalize().map_err(|e| io_error(&e))?;
            let ancestor = target
                .ancestors()
                .find(|p| p.exists())
                .ok_or(3u32)?
                .canonicalize()
                .map_err(|e| io_error(&e))?;
            if !ancestor.starts_with(&root)
                && !(access == 1
                    && disposition == 3
                    && self.matching_mounts(path).iter().any(|m| {
                        m.host_dir
                            .canonicalize()
                            .is_ok_and(|root| ancestor.starts_with(root))
                    }))
            {
                return Err(5);
            }
            let k = if target.exists() {
                target.canonicalize().map_err(|e| io_error(&e))?
            } else {
                target
                    .parent()
                    .ok_or(3u32)?
                    .canonicalize()
                    .map_err(|_| 3u32)?
                    .join(target.file_name().ok_or(87u32)?)
            }
            .to_string_lossy()
            .to_ascii_lowercase();
            host = Some(target);
            k
        };
        let shared = self.shared.clone();
        let mut state = shared.0.lock().unwrap();
        busy(&mut state, &key);
        if state.leases.get(&key).is_some_and(|leases| {
            leases
                .iter()
                .filter_map(Weak::upgrade)
                .any(|l| access & !l.share != 0 || l.access & !share != 0)
        }) {
            return Err(32);
        }
        let existed = if let Some(p) = &host {
            p.exists()
        } else {
            self.ram_file_size(path).is_some()
        };
        if !existed && read_only {
            return Err(5);
        }
        if disposition == 1 && existed {
            return Err(80);
        }
        if (disposition == 3 || disposition == 5) && !existed {
            return Err(2);
        }
        if access & 2 != 0 && state.attributes.get(&key).is_some_and(|a| a & 1 != 0) {
            return Err(5);
        }
        let a = match access {
            2 => Access::Write,
            3 => Access::ReadWrite,
            _ => Access::Read,
        };
        let lease = Arc::new(Lease {
            access,
            share,
            volume,
        });
        let h = if let Some(path) = host {
            if path.is_dir() {
                return Err(5);
            }
            let mut options = OpenOptions::new();
            options
                .read(access & 1 != 0 || access == 0)
                .write(access & 2 != 0);
            match disposition {
                1 => {
                    options.create_new(true);
                }
                2 => {
                    options.create(true).truncate(true);
                }
                4 => {
                    options.create(true);
                }
                5 => {
                    options.truncate(true);
                }
                _ => {}
            }
            // Creating a read-only handle still creates the file, without granting guest write access.
            if access & 2 == 0
                && !existed
                && (disposition == 1 || disposition == 2 || disposition == 4)
            {
                let mut create = OpenOptions::new();
                create.write(true).create_new(true);
                create.open(&path).map_err(|e| io_error(&e))?;
                options.create(false).create_new(false).truncate(false);
            }
            if disposition == 2 && existed && access & 2 == 0 {
                return Err(5);
            }
            let file = options.open(&path).map_err(|e| io_error(&e))?;
            let handle = self.next_handle;
            self.next_handle = self.next_handle.checked_add(1).ok_or(8u32)?;
            self.handles.insert(
                handle,
                Arc::new(Mutex::new(OpenFile {
                    host_path: path,
                    access: a,
                    file,
                    text_mode: false,
                    lease: Some(lease.clone()),
                    append,
                })),
            );
            handle
        } else {
            let ram = self.ram.as_ref().unwrap().clone();
            let rk = self.ram_key(path).ok_or(87u32)?;
            let store = ram.files.lock().unwrap();
            if store.dirs.contains_key(&rk) {
                return Err(5);
            }
            let parent = rk.rsplit_once('/').map(|p| p.0).unwrap_or("");
            if !parent.is_empty() && !store.dirs.contains_key(parent) {
                return Err(3);
            }
            drop(store);
            // open_ram already implements charging and transactional allocation.
            let h = self
                .open_ram(path, if !existed { Access::ReadWrite } else { a }, !existed)
                .ok_or(112u32)?;
            let open = self.ram_handles.get(&h).unwrap().clone();
            let mut f = open.lock().unwrap();
            f.access = a;
            f.lease = Some(lease.clone());
            f.append = append;
            if (disposition == 2 || disposition == 5) && !f.file.lock().unwrap().set_len(0) {
                drop(f);
                self.close(h);
                return Err(112);
            }
            h
        };
        if !existed {
            state
                .attributes
                .insert(key.clone(), (attributes & 0x27) | 0x20);
        }
        state
            .leases
            .entry(key)
            .or_default()
            .push(Arc::downgrade(&lease));
        if append {
            self.seek(h, 0, SeekKind::End);
        }
        Ok(OpenResult { handle: h, existed })
    }
    pub(super) fn lease(&self, h: u32) -> Option<Arc<Lease>> {
        if let Some(f) = self.handles.get(&h) {
            return f.lock().ok()?.lease.clone();
        }
        self.ram_handles.get(&h)?.lock().ok()?.lease.clone()
    }
    fn growth_allowed(&self, h: u32, size: u64) -> bool {
        let Some(l) = self.lease(h) else {
            return true;
        };
        if l.access & 2 == 0 {
            return false;
        }
        let Some((root, total)) = &l.volume else {
            return true;
        };
        let old = self
            .handles
            .get(&h)
            .and_then(|f| f.lock().ok()?.file.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0);
        size <= old || used(root).is_ok_and(|u| size - old <= total.saturating_sub(u))
    }
    pub fn write(&mut self, h: u32, data: &[u8]) -> Option<usize> {
        let shared = self.shared.clone();
        let _gate = shared.0.lock().unwrap();
        let end = if let Some(f) = self.handles.get(&h) {
            let mut f = f.lock().ok()?;
            let pos = if f.append {
                f.file.metadata().ok()?.len()
            } else {
                f.file.stream_position().ok()?
            };
            pos.checked_add(data.len() as u64)?
        } else {
            0
        };
        if !self.growth_allowed(h, end) {
            return None;
        }
        self.write_raw(h, data)
    }
    pub fn set_end_of_file(&mut self, h: u32) -> bool {
        let shared = self.shared.clone();
        let _gate = shared.0.lock().unwrap();
        let size = if let Some(f) = self.handles.get(&h) {
            let Ok(mut f) = f.lock() else {
                return false;
            };
            let Ok(p) = f.file.stream_position() else {
                return false;
            };
            p
        } else {
            0
        };
        self.growth_allowed(h, size) && self.set_end_of_file_raw(h)
    }
    pub fn disk_space(&self, path: &str) -> Result<(u64, u64), u32> {
        if self.is_ram_path(path) {
            if path != "\\" && path != "/" && self.list_dir(path).is_none() {
                return Err(3);
            }
            let ram = self.ram.as_ref().unwrap();
            let s = ram.snapshot();
            return Ok((
                s.store_pages as u64 * ram.page_size() as u64,
                s.store_free() as u64 * ram.page_size() as u64,
            ));
        }
        let (p, m) = self.exact_path(path, false)?;
        if !p.is_dir() {
            return Err(3);
        }
        let occupied = used(&m.host_dir)?;
        let total = if let Some((_, total)) = Self::quota(m) {
            total
        } else {
            occupied
                .max(64 * 1024 * 1024)
                .checked_next_power_of_two()
                .unwrap_or(u64::MAX)
        };
        Ok((total, total.saturating_sub(occupied)))
    }
    pub fn attributes(&self, path: &str) -> Result<u32, u32> {
        if self.is_ram_path(path) {
            let key = self.file_key(path, false)?;
            if self.ram_file_size(path).is_some() {
                return Ok(self
                    .shared
                    .0
                    .lock()
                    .unwrap()
                    .attributes
                    .get(&key)
                    .copied()
                    .unwrap_or(0x20));
            }
            return self.list_dir(path).is_some().then_some(0x10).ok_or(2);
        }
        let (exact, m) = self.exact_path(path, false)?;
        let p = self.resolve(path).unwrap_or(exact);
        let meta = p.metadata().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound && !p.parent().is_some_and(Path::is_dir) {
                3
            } else {
                io_error(&e)
            }
        })?;
        let key = p
            .canonicalize()
            .map_err(|e| io_error(&e))?
            .to_string_lossy()
            .to_ascii_lowercase();
        let flags = self
            .shared
            .0
            .lock()
            .unwrap()
            .attributes
            .get(&key)
            .copied()
            .unwrap_or(if meta.is_dir() { 0x10 } else { 0x80 });
        Ok(flags
            | if m.read_only || meta.permissions().readonly() {
                1
            } else {
                0
            }
            | if meta.is_dir() { 0x10 } else { 0 })
    }
    pub fn set_attributes(&self, path: &str, flags: u32) -> Result<(), u32> {
        if flags & !(0x27 | 0x80) != 0 {
            return Err(87);
        }
        self.attributes(path)?;
        let key = self.file_key(path, true)?;
        self.shared.0.lock().unwrap().attributes.insert(
            key,
            if flags & 0x27 == 0 {
                0x80
            } else {
                flags & 0x27
            },
        );
        Ok(())
    }
    pub fn create_directory(&self, path: &str) -> Result<(), u32> {
        if self.attributes(path).is_ok() {
            return Err(183);
        }
        if !self.is_ram_path(path) {
            let (p, _) = self.exact_path(path, true)?;
            if !p.parent().is_some_and(Path::is_dir) {
                return Err(3);
            }
        }
        if self.create_dir_raw(path) {
            Ok(())
        } else {
            Err(if self.is_ram_path(path) { 112 } else { 5 })
        }
    }
    pub fn create_dir(&self, path: &str) -> bool {
        self.create_directory(path).is_ok()
    }
    pub fn delete(&self, path: &str) -> Result<(), u32> {
        let attrs = self.attributes(path)?;
        if attrs & 0x11 != 0 {
            return Err(5);
        }
        let key = self.file_key(path, true)?;
        let shared = self.shared.clone();
        let mut s = shared.0.lock().unwrap();
        if busy(&mut s, &key) {
            return Err(32);
        }
        if !self.delete_file_raw(path) {
            return Err(5);
        }
        s.attributes.remove(&key);
        Ok(())
    }
    pub fn delete_file(&self, path: &str) -> bool {
        self.delete(path).is_ok()
    }
    pub fn remove_directory(&self, path: &str) -> Result<(), u32> {
        if self.attributes(path)? & 0x10 == 0 {
            return Err(267);
        }
        if self.list_dir(path).is_some_and(|v| !v.is_empty()) {
            return Err(145);
        }
        if self.is_ram_path(path) {
            return self.remove_ram_dir(path).then_some(()).ok_or(5);
        }
        let (p, m) = self.exact_path(path, true)?;
        if p == m.host_dir {
            return Err(5);
        }
        std::fs::remove_dir(p).map_err(|e| io_error(&e))
    }
    pub fn rename(&self, from: &str, to: &str) -> Result<(), u32> {
        self.attributes(from)?;
        if self.attributes(to).is_ok() {
            return Err(183);
        }
        let source = self.file_key(from, true)?;
        let dest = self.file_key(to, true)?;
        let shared = self.shared.clone();
        let mut s = shared.0.lock().unwrap();
        if busy(&mut s, &source)
            || s.leases.iter().any(|(key, v)| {
                key.starts_with(&format!("{source}/")) && v.iter().any(|l| l.strong_count() != 0)
            })
        {
            return Err(32);
        }
        if !self.move_file_raw(from, to) {
            return Err(5);
        }
        if let Some(a) = s.attributes.remove(&source) {
            s.attributes.insert(dest, a);
        }
        Ok(())
    }
    pub fn move_file(&self, from: &str, to: &str) -> bool {
        self.rename(from, to).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mounted(dir: &Path) -> Vfs {
        let mut v = Vfs::new();
        v.mount_save_dir("\\Flash Disk\\", dir);
        v
    }
    #[test]
    fn copy_preserves_data_and_enforces_same_file_sharing_and_existing_destination() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("source"), vec![0x5a; 40000]).unwrap();
        let mut v = mounted(d.path());
        let from = "\\Flash Disk\\source";
        let to = "\\Flash Disk\\copy";
        v.copy_file(from, to, true).unwrap();
        assert_eq!(
            std::fs::read(d.path().join("copy")).unwrap(),
            vec![0x5a; 40000]
        );
        assert_eq!(v.copy_file(from, to, true), Err(80));
        assert_eq!(v.copy_file(from, from, false), Err(87));
        let h = v.open_file(to, 1, 1, 3, false, 0).unwrap().handle;
        assert_eq!(v.copy_file(from, to, false), Err(32));
        v.close(h);
        std::fs::write(d.path().join("source"), b"short").unwrap();
        v.copy_file(from, to, false).unwrap();
        assert_eq!(std::fs::read(d.path().join("copy")).unwrap(), b"short");
        assert!(v.open_handles().is_empty());
    }

    #[test]
    fn copy_between_ram_and_flash_and_quota_failure_preserves_old_destination() {
        let d = tempfile::tempdir().unwrap();
        let mut v = mounted(d.path());
        v.attach_ram(Some(crate::memory_division::MemoryDivision::default()));
        v.create_directory("/copytest").unwrap();
        let h = v
            .open_file("/copytest/source", 3, 3, 1, false, 0)
            .unwrap()
            .handle;
        assert_eq!(v.write(h, b"ram data"), Some(8));
        v.close(h);
        v.copy_file("/copytest/source", "\\Flash Disk\\save", true)
            .unwrap();
        v.copy_file("\\Flash Disk\\save", "/copytest/back", true)
            .unwrap();
        let h = v
            .open_file("/copytest/back", 1, 3, 3, false, 0)
            .unwrap()
            .handle;
        let mut b = [0; 8];
        assert_eq!(v.read(h, &mut b), Some(8));
        assert_eq!(&b, b"ram data");
        v.close(h);
        let large = std::fs::File::create(d.path().join("large")).unwrap();
        large.set_len(32 * 1024 * 1024).unwrap();
        assert_eq!(
            v.copy_file("\\Flash Disk\\large", "\\Flash Disk\\save", false),
            Err(112)
        );
        assert_eq!(std::fs::read(d.path().join("save")).unwrap(), b"ram data");
        assert!(v.open_handles().is_empty());
    }
    #[test]
    fn creation_dispositions_preserve_or_truncate_on_host_and_ram() {
        let d = tempfile::tempdir().unwrap();
        for ram in [false, true] {
            let mut v = mounted(d.path());
            if ram {
                v = Vfs::new();
                v.attach_ram(Some(crate::memory_division::MemoryDivision::default()));
                v.create_dir("/test");
            }
            let path = if ram {
                "/test/save"
            } else {
                "\\Flash Disk\\save"
            };
            let h = v.open_file(path, 3, 3, 1, false, 0).unwrap();
            assert!(!h.existed);
            assert_eq!(v.write(h.handle, b"SAVE"), Some(4));
            v.close(h.handle);
            assert_eq!(v.open_file(path, 2, 3, 1, false, 0), Err(80));
            let h = v.open_file(path, 2, 3, 4, false, 0).unwrap();
            assert!(h.existed);
            assert_eq!(v.size(h.handle), Some(4));
            v.close(h.handle);
            let h = v.open_file(path, 3, 3, 2, false, 0).unwrap();
            assert_eq!(v.size(h.handle), Some(0));
            assert_eq!(v.write(h.handle, b"NEW"), Some(3));
            v.close(h.handle);
            let h = v.open_file(path, 2, 3, 5, false, 0).unwrap();
            assert_eq!(v.size(h.handle), Some(0));
            v.close(h.handle);
            assert!(v.delete_file(path));
            assert_eq!(v.open_file(path, 3, 3, 5, false, 0), Err(2));
        }
    }
    #[test]
    fn share_restrictions_survive_duplicates_and_cross_process_close() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("save"), b"data").unwrap();
        let mut a = mounted(d.path());
        let mut b = mounted(d.path());
        b.attach_shared_context(a.shared_context());
        let h = a
            .open_file("\\Flash Disk\\save", 1, 0, 3, false, 0)
            .unwrap()
            .handle;
        assert_eq!(
            b.open_file("\\Flash Disk\\save", 1, 3, 3, false, 0),
            Err(32)
        );
        b.import_handle(99, a.export_handle(h).unwrap());
        a.close(h);
        assert_eq!(a.delete("\\Flash Disk\\save"), Err(32));
        b.close(99);
        let h = a
            .open_file("\\Flash Disk\\save", 1, 1, 3, false, 0)
            .unwrap()
            .handle;
        assert_eq!(
            b.open_file("\\Flash Disk\\save", 2, 3, 3, false, 0),
            Err(32)
        );
        assert_eq!(
            b.open_file("\\Flash Disk\\save", 1, 0, 3, false, 0),
            Err(32)
        );
        let h2 = b
            .open_file("\\Flash Disk\\save", 1, 1, 3, false, 0)
            .unwrap()
            .handle;
        a.close(h);
        b.close(h2);
        assert!(a.delete_file("\\Flash Disk\\save"));
    }
    #[test]
    fn append_mode_always_writes_at_end_after_seek() {
        let d = tempfile::tempdir().unwrap();
        let mut v = mounted(d.path());
        let h = v
            .open_file("\\Flash Disk\\save", 3, 3, 4, true, 0)
            .unwrap()
            .handle;
        assert_eq!(v.write(h, b"ONE"), Some(3));
        v.seek(h, 0, SeekKind::Begin);
        assert_eq!(v.write(h, b"TWO"), Some(3));
        v.seek(h, 0, SeekKind::Begin);
        let mut buf = [0; 6];
        assert_eq!(v.read(h, &mut buf), Some(6));
        assert_eq!(&buf, b"ONETWO");
    }
    #[test]
    fn flash_quota_refunds_truncation_and_refuses_overflow_atomically() {
        let d = tempfile::tempdir().unwrap();
        let mut v = mounted(d.path());
        let before = v.disk_space("\\Flash Disk\\").unwrap();
        assert_eq!(before, (32 * 1024 * 1024, 32 * 1024 * 1024));
        let h = v
            .open_file("\\Flash Disk\\large", 3, 3, 1, false, 0)
            .unwrap()
            .handle;
        v.seek(h, before.1 as i64 + 1, SeekKind::Begin);
        assert!(!v.set_end_of_file(h));
        assert_eq!(v.size(h), Some(0));
        v.seek(h, before.1 as i64, SeekKind::Begin);
        assert!(v.set_end_of_file(h));
        assert_eq!(v.disk_space("\\Flash Disk\\").unwrap().1, 0);
        assert_eq!(v.write(h, b"X"), None);
        assert_eq!(v.size(h), Some(before.1));
        v.seek(h, 0, SeekKind::Begin);
        assert!(v.set_end_of_file(h));
        assert_eq!(v.disk_space("\\Flash Disk\\").unwrap(), before);
    }
    #[test]
    fn directory_removal_and_rename_do_not_succeed_fictitiously() {
        let d = tempfile::tempdir().unwrap();
        let mut v = mounted(d.path());
        assert_eq!(v.create_directory("\\Flash Disk\\missing\\child"), Err(3));
        v.create_directory("\\Flash Disk\\scratch").unwrap();
        assert_eq!(v.create_directory("\\Flash Disk\\scratch"), Err(183));
        let h = v
            .open_file("\\Flash Disk\\scratch\\a", 2, 3, 1, false, 0)
            .unwrap()
            .handle;
        assert_eq!(v.remove_directory("\\Flash Disk\\scratch"), Err(145));
        assert_eq!(
            v.rename("\\Flash Disk\\scratch\\a", "\\Flash Disk\\scratch\\b"),
            Err(32)
        );
        v.close(h);
        std::fs::write(d.path().join("scratch/b"), b"DEST").unwrap();
        assert_eq!(
            v.rename("\\Flash Disk\\scratch\\a", "\\Flash Disk\\scratch\\b"),
            Err(183)
        );
        assert_eq!(std::fs::read(d.path().join("scratch/b")).unwrap(), b"DEST");
        v.delete("\\Flash Disk\\scratch\\a").unwrap();
        v.delete("\\Flash Disk\\scratch\\b").unwrap();
        v.remove_directory("\\Flash Disk\\scratch").unwrap();
        assert!(!d.path().join("scratch").exists());
    }
    #[test]
    fn protected_mount_cannot_be_written_through_broader_root() {
        let broad = tempfile::tempdir().unwrap();
        let card = tempfile::tempdir().unwrap();
        let mut v = Vfs::new();
        v.mount_save_dir("\\", broad.path());
        v.mount_read_only("\\SD Card\\", card.path());
        assert_eq!(v.open_file("\\SD Card\\new", 2, 3, 2, false, 0), Err(5));
        assert_eq!(v.create_directory("\\SD Card\\dir"), Err(5));
    }
    #[test]
    fn readonly_attributes_and_query_access_are_enforced() {
        let d = tempfile::tempdir().unwrap();
        let mut v = mounted(d.path());
        let path = "\\Flash Disk\\save";
        let h = v.open_file(path, 3, 3, 1, false, 0).unwrap().handle;
        v.close(h);
        v.set_attributes(path, 3).unwrap();
        assert_eq!(v.attributes(path).unwrap() & 3, 3);
        assert_eq!(v.open_file(path, 2, 3, 3, false, 0), Err(5));
        assert_eq!(v.delete(path), Err(5));
        let h = v.open_file(path, 0, 3, 3, false, 0).unwrap().handle;
        assert_eq!(v.read(h, &mut [0; 1]), None);
        assert_eq!(v.write(h, b"X"), None);
        v.close(h);
        v.set_attributes(path, 0x80).unwrap();
        v.delete(path).unwrap();
    }
    #[test]
    fn negative_seek_preserves_position_on_host() {
        let d = tempfile::tempdir().unwrap();
        let mut v = mounted(d.path());
        let h = v
            .open_file("\\Flash Disk\\save", 3, 3, 1, false, 0)
            .unwrap()
            .handle;
        v.seek(h, 10, SeekKind::Begin);
        assert_eq!(v.seek(h, -1, SeekKind::Begin), None);
        assert_eq!(v.seek(h, 0, SeekKind::Current), Some(10));
    }
}
