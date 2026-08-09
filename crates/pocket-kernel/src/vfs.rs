//! Tiny virtual file system that backs `coredll`'s file APIs.
//!
//! Goals:
//!
//! * Map a single host directory to the WinCE root `\` (or any
//!   configurable mount prefix). All guest paths under that prefix
//!   are resolved against the host directory; everything else fails
//!   with `ERROR_PATH_NOT_FOUND`.
//! * Hand out integer "handles" so the dispatcher can store them in
//!   guest registers without needing to push raw [`std::fs::File`]
//!   objects into the emulator's address space.
//! * Reject any path that tries to escape the mount root via `..`.
//!
//! What it explicitly does NOT do:
//!
//! * Real WinCE attribute / security model.
//! * Asynchronous I/O.
//! * Memory-mapped files.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

/// `INVALID_HANDLE_VALUE` from `<windows.h>`.
pub const INVALID_HANDLE_VALUE: u32 = 0xFFFF_FFFF;

/// First handle handed out. Picked to be obviously not a small Win32
/// pseudo-handle and not collide with the GDI fake-handle range.
const HANDLE_BASE: u32 = 0x4000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    ReadWrite,
}

#[derive(Debug)]
pub struct OpenFile {
    pub host_path: PathBuf,
    pub access: Access,
    pub file: File,
}

#[derive(Debug, Clone)]
struct Mount {
    prefix: String,
    host_dir: PathBuf,
    read_only: bool,
}

/// Mount-point + open-handle table.
pub struct Vfs {
    mounts: Vec<Mount>,
    handles: HashMap<u32, OpenFile>,
    next_handle: u32,
    /// Directory relative guest paths are resolved against.
    ///
    /// Windows CE has no per-process working directory, but Pocket PC
    /// games regularly pass `".\\*.pdb"` or a bare file name and expect
    /// the file next to their executable (Astraware's Bejeweled
    /// enumerates its PalmOS-derived `.pdb` resources that way and calls
    /// `ExitProcess(0x42)` when the search comes up empty). Point this at
    /// the module's install directory so those lookups land there.
    default_dir: String,
}

impl Default for Vfs {
    fn default() -> Self {
        Self::new()
    }
}

impl Vfs {
    pub fn new() -> Self {
        Self {
            mounts: Vec::new(),
            handles: HashMap::new(),
            next_handle: HANDLE_BASE,
            default_dir: "\\".to_string(),
        }
    }

    /// Set the directory bare / `.`-relative guest paths resolve
    /// against. Pass the directory of the running module.
    pub fn set_default_dir(&mut self, guest_dir: &str) {
        let mut d = guest_dir.replace('/', "\\");
        if !d.starts_with('\\') {
            d.insert(0, '\\');
        }
        if !d.ends_with('\\') {
            d.push('\\');
        }
        self.default_dir = d;
    }

    /// Expand a guest path to an absolute one: strip `.\` prefixes and
    /// anchor anything that is not already rooted at [`Self::default_dir`].
    fn absolute(&self, guest_path: &str) -> String {
        let mut p = guest_path.replace('/', "\\");
        while let Some(rest) = p.strip_prefix(".\\") {
            p = rest.to_string();
        }
        if p == "." {
            p = String::new();
        }
        if p.starts_with('\\') {
            p
        } else {
            format!("{}{p}", self.default_dir)
        }
    }

    /// Mount `host_dir` at `guest_prefix`. The prefix is matched
    /// case-insensitively and accepts both `\` and `/` separators.
    pub fn mount(&mut self, guest_prefix: &str, host_dir: impl Into<PathBuf>) {
        self.mount_with_options(guest_prefix, host_dir, false);
    }

    /// Mount a host directory as read-only guest storage.
    pub fn mount_read_only(&mut self, guest_prefix: &str, host_dir: impl Into<PathBuf>) {
        self.mount_with_options(guest_prefix, host_dir, true);
    }

    pub fn mount_save_dir(&mut self, guest_prefix: &str, host_dir: impl Into<PathBuf>) {
        let host_dir = host_dir.into();
        if let Err(error) = std::fs::create_dir_all(&host_dir) {
            log::warn!("vfs.mount_save_dir({host_dir:?}) could not create directory: {error}");
        }
        self.mount_with_options(guest_prefix, host_dir, false);
    }

    fn mount_with_options(
        &mut self,
        guest_prefix: &str,
        host_dir: impl Into<PathBuf>,
        read_only: bool,
    ) {
        let mut p = guest_prefix.replace('\\', "/").to_ascii_lowercase();
        if !p.starts_with('/') {
            p.insert(0, '/');
        }
        while p.contains("//") {
            p = p.replace("//", "/");
        }
        if !p.ends_with('/') {
            p.push('/');
        }
        self.mounts.push(Mount {
            prefix: p,
            host_dir: host_dir.into(),
            read_only,
        });
    }

    pub fn mount_count(&self) -> usize {
        self.mounts.len()
    }

    pub fn mounts_snapshot(&self) -> Vec<(String, PathBuf)> {
        self.mounts
            .iter()
            .map(|m| (m.prefix.clone(), m.host_dir.clone()))
            .collect()
    }

    fn normalise_guest_path(&self, guest_path: &str) -> String {
        let absolute = self.absolute(guest_path);
        let mut normalised = absolute.replace('\\', "/").to_ascii_lowercase();
        while normalised.contains("//") {
            normalised = normalised.replace("//", "/");
        }
        if normalised.starts_with('/') {
            normalised
        } else {
            format!("/{normalised}")
        }
    }

    fn matching_mounts<'a>(&'a self, guest_path: &str) -> Vec<&'a Mount> {
        let normalised = self.normalise_guest_path(guest_path);
        let mut mounts: Vec<_> = self
            .mounts
            .iter()
            .filter(|mount| {
                let root = mount.prefix.trim_end_matches('/');
                normalised == root || normalised.starts_with(&mount.prefix)
            })
            .collect();
        mounts.sort_by_key(|mount| std::cmp::Reverse(mount.prefix.len()));
        mounts
    }

    fn host_path_for_mount(&self, mount: &Mount, normalised: &str) -> Option<PathBuf> {
        let root = mount.prefix.trim_end_matches('/');
        if normalised == root {
            return Some(mount.host_dir.clone());
        }
        let rel = &normalised[mount.prefix.len()..];
        let mut p = mount.host_dir.clone();
        for comp in Path::new(rel).components() {
            match comp {
                Component::Normal(n) => {
                    let wanted = n.to_string_lossy();
                    let exact = p.join(n);
                    if exact.exists() {
                        p = exact;
                    } else if let Ok(entries) = std::fs::read_dir(&p) {
                        if let Some(entry) = entries.flatten().find(|entry| {
                            entry
                                .file_name()
                                .to_string_lossy()
                                .eq_ignore_ascii_case(&wanted)
                        }) {
                            p = entry.path();
                        } else {
                            p = exact;
                        }
                    } else {
                        p = exact;
                    }
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    log::warn!("vfs.resolve: refusing escape via {normalised:?}");
                    return None;
                }
            }
        }
        Some(p)
    }

    fn find_basename_recursive(root: &Path, wanted: &str) -> Option<PathBuf> {
        let mut pending = vec![(root.to_path_buf(), 0usize)];
        while let Some((dir, depth)) = pending.pop() {
            let entries = std::fs::read_dir(&dir).ok()?;
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                if name.to_string_lossy().eq_ignore_ascii_case(wanted) {
                    if path.is_file() {
                        return Some(path);
                    }
                    continue;
                }
                if depth < 16 && path.is_dir() {
                    pending.push((path, depth + 1));
                }
            }
        }
        None
    }

    /// Translate a guest path to a host path. Existing files fall back
    /// through broader mounts, allowing a writable save overlay to sit
    /// above a read-only extracted game directory.
    pub fn resolve(&self, guest_path: &str) -> Option<PathBuf> {
        let normalised = self.normalise_guest_path(guest_path);
        let mounts = self.matching_mounts(&normalised);
        let mut fallback = None;
        let basename = Path::new(&normalised)
            .file_name()
            .map(|name| name.to_string_lossy());
        for mount in mounts {
            let path = self.host_path_for_mount(mount, &normalised)?;
            if fallback.is_none() {
                fallback = Some(path.clone());
            }
            if path.exists() {
                return Some(path);
            }
            let root = mount.prefix.trim_end_matches('/');
            if normalised != root {
                if let Some(name) = basename.as_deref() {
                    if let Some(found) = Self::find_basename_recursive(&mount.host_dir, name) {
                        log::debug!("vfs.resolve: basename fallback {normalised:?} -> {found:?}");
                        return Some(found);
                    }
                }
            }
        }
        fallback
    }

    /// List a guest directory. Returns `(name, size, is_dir)` for
    /// every entry, sorted case-insensitively by name, or `None` when
    /// the directory does not resolve to a mounted host directory.
    ///
    /// Backs `FindFirstFileW` / `FindNextFileW`, which Pocket PC games
    /// use to discover their own data files (Astraware titles enumerate
    /// `*.pdb` resource databases next to the executable).
    pub fn list_dir(&self, guest_dir: &str) -> Option<Vec<(String, u64, bool)>> {
        let normalised = self.normalise_guest_path(guest_dir);
        let mut merged = std::collections::BTreeMap::new();
        for mount in self.matching_mounts(&normalised) {
            let Some(host) = self.host_path_for_mount(mount, &normalised) else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(host) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                let name = entry.file_name().to_string_lossy().to_string();
                merged.entry(name.to_ascii_lowercase()).or_insert((
                    name,
                    meta.len(),
                    meta.is_dir(),
                ));
            }
        }
        (!merged.is_empty()).then(|| merged.into_values().collect())
    }

    /// Create a guest directory through the writable mount that owns it.
    pub fn create_dir(&self, guest_path: &str) -> bool {
        let normalised = self.normalise_guest_path(guest_path);
        let Some(mount) = self
            .matching_mounts(&normalised)
            .into_iter()
            .find(|mount| !mount.read_only)
        else {
            return false;
        };
        let Some(path) = self.host_path_for_mount(mount, &normalised) else {
            return false;
        };
        std::fs::create_dir_all(path).is_ok()
    }

    /// Remove a guest file from the writable mount that owns it.
    pub fn delete_file(&self, guest_path: &str) -> bool {
        let normalised = self.normalise_guest_path(guest_path);
        let Some(mount) = self
            .matching_mounts(&normalised)
            .into_iter()
            .find(|mount| !mount.read_only)
        else {
            return false;
        };
        let Some(path) = self.host_path_for_mount(mount, &normalised) else {
            return false;
        };
        std::fs::remove_file(path).is_ok()
    }

    /// Rename a guest file within the writable mount that owns it.
    pub fn move_file(&self, from: &str, to: &str) -> bool {
        let from_normalised = self.normalise_guest_path(from);
        let to_normalised = self.normalise_guest_path(to);
        let Some(mount) = self
            .matching_mounts(&from_normalised)
            .into_iter()
            .find(|mount| !mount.read_only)
        else {
            return false;
        };
        let Some(source) = self.host_path_for_mount(mount, &from_normalised) else {
            return false;
        };
        let root = mount.prefix.trim_end_matches('/');
        if to_normalised != root && !to_normalised.starts_with(&mount.prefix) {
            return false;
        }
        let Some(destination) = self.host_path_for_mount(mount, &to_normalised) else {
            return false;
        };
        if let Some(parent) = destination.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::rename(source, destination).is_ok()
    }

    /// Open a host file behind a guest path. Returns the handle id.
    pub fn open(&mut self, guest_path: &str, access: Access, create: bool) -> Option<u32> {
        let normalised = self.normalise_guest_path(guest_path);
        let host_path = if matches!(access, Access::Read) {
            self.resolve(guest_path)?
        } else {
            let mount = self
                .matching_mounts(&normalised)
                .into_iter()
                .find(|mount| !mount.read_only)?;
            let path = self.host_path_for_mount(mount, &normalised)?;
            if create {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
            }
            path
        };
        let mut opts = OpenOptions::new();
        match access {
            Access::Read => {
                opts.read(true);
            }
            Access::Write => {
                opts.write(true);
                if create {
                    opts.create(true).truncate(true);
                }
            }
            Access::ReadWrite => {
                opts.read(true).write(true);
                if create {
                    opts.create(true);
                }
            }
        }
        if create {
            if let Some(parent) = host_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let file = match opts.open(&host_path) {
            Ok(f) => f,
            Err(e) => {
                log::trace!("vfs.open({guest_path:?}) -> host {host_path:?} failed: {e}");
                return None;
            }
        };
        let h = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(
            h,
            OpenFile {
                host_path,
                access,
                file,
            },
        );
        Some(h)
    }

    pub fn read(&mut self, handle: u32, buf: &mut [u8]) -> Option<usize> {
        let of = self.handles.get_mut(&handle)?;
        of.file.read(buf).ok()
    }

    pub fn write(&mut self, handle: u32, buf: &[u8]) -> Option<usize> {
        let of = self.handles.get_mut(&handle)?;
        of.file.write(buf).ok()
    }

    pub fn size(&mut self, handle: u32) -> Option<u64> {
        let of = self.handles.get_mut(&handle)?;
        of.file.metadata().ok().map(|m| m.len())
    }

    pub fn seek(&mut self, handle: u32, offset: i64, whence: SeekKind) -> Option<u64> {
        let of = self.handles.get_mut(&handle)?;
        let from = match whence {
            SeekKind::Begin => SeekFrom::Start(offset.max(0) as u64),
            SeekKind::Current => SeekFrom::Current(offset),
            SeekKind::End => SeekFrom::End(offset),
        };
        of.file.seek(from).ok()
    }

    pub fn flush(&mut self, handle: u32) -> std::io::Result<()> {
        self.handles
            .get_mut(&handle)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid handle"))?
            .file
            .flush()
    }

    pub fn close(&mut self, handle: u32) -> bool {
        self.handles.remove(&handle).is_some()
    }

    /// Flush and close every open handle, returning how many were closed.
    ///
    /// This backs the CRT's `_fcloseall`. The handle table does not
    /// separate CRT streams from Win32 `CreateFile` handles, so this
    /// closes both — which is what the process teardown this runs as part
    /// of would do anyway: CeGCC's `crt3.c` calls `_fcloseall` on its way
    /// into `ExitProcess`, and nothing reads a handle after that.
    pub fn close_all(&mut self) -> usize {
        for of in self.handles.values_mut() {
            let _ = of.file.flush();
        }
        let n = self.handles.len();
        self.handles.clear();
        n
    }

    pub fn is_open(&self, handle: u32) -> bool {
        self.handles.contains_key(&handle)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SeekKind {
    Begin,
    Current,
    End,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_resolves_guest_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
        let mut v = Vfs::new();
        v.mount("\\Application\\", dir.path());
        let p = v.resolve("\\Application\\hello.txt").unwrap();
        assert!(p.ends_with("hello.txt"));
        assert!(v.resolve("\\Other\\thing.txt").is_none());
    }

    #[test]
    fn refuses_parent_dir_escape() {
        let dir = tempfile::tempdir().unwrap();
        let mut v = Vfs::new();
        v.mount("\\App\\", dir.path());
        assert!(v.resolve("\\App\\..\\..\\etc\\passwd").is_none());
    }

    #[test]
    fn read_only_mount_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"abcdef").unwrap();
        let mut v = Vfs::new();
        v.mount_read_only("\\Rom\\", dir.path());
        assert!(v.open("\\Rom\\data.bin", Access::Read, false).is_some());
        assert!(v.open("\\Rom\\new.bin", Access::Write, true).is_none());
    }

    #[test]
    fn writable_open_creates_nested_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let mut v = Vfs::new();
        v.mount("\\Save\\", dir.path());
        let h = v
            .open(
                "\\Save\\My Documents\\My Saved Games\\settings.pdb",
                Access::Write,
                true,
            )
            .unwrap();
        let host_path = v
            .resolve("\\Save\\My Documents\\My Saved Games\\settings.pdb")
            .unwrap();
        assert!(host_path.ends_with("my documents/my saved games/settings.pdb"));
        assert_eq!(v.write(h, b"settings"), Some(8));
        v.flush(h).unwrap();
        v.close(h);
        assert_eq!(std::fs::read(host_path).unwrap(), b"settings");
    }

    #[test]
    fn writable_mount_supports_atomic_save_operations() {
        let dir = tempfile::tempdir().unwrap();
        let mut v = Vfs::new();
        v.mount("\\Save\\", dir.path());
        assert!(v.create_dir("\\Save\\nested"));
        let h = v
            .open("\\Save\\nested\\old.dat", Access::Write, true)
            .unwrap();
        assert_eq!(v.write(h, b"save"), Some(4));
        v.flush(h).unwrap();
        v.close(h);
        assert!(v.move_file("\\Save\\nested\\old.dat", "\\Save\\nested\\new.dat"));
        assert!(v.delete_file("\\Save\\nested\\new.dat"));
        assert!(!v.move_file("\\Save\\nested\\missing.dat", "\\Other\\escape.dat"));
    }

    #[test]
    fn open_read_close_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"abcdef").unwrap();
        let mut v = Vfs::new();
        v.mount("\\App\\", dir.path());
        let h = v.open("\\App\\data.bin", Access::Read, false).unwrap();
        let mut buf = [0u8; 6];
        assert_eq!(v.read(h, &mut buf), Some(6));
        assert_eq!(&buf, b"abcdef");
        assert!(v.close(h));
        assert!(!v.is_open(h));
    }
}
