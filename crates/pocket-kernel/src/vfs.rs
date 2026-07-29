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

    fn matching_mount(&self, guest_path: &str) -> Option<&Mount> {
        let mut normalised = guest_path.replace('\\', "/").to_ascii_lowercase();
        while normalised.contains("//") {
            normalised = normalised.replace("//", "/");
        }
        let with_leading = if normalised.starts_with('/') {
            normalised
        } else {
            format!("/{normalised}")
        };
        self.mounts
            .iter()
            .filter(|mount| with_leading.starts_with(&mount.prefix))
            .max_by_key(|mount| mount.prefix.len())
    }

    /// Translate a guest path to a host path. Returns `None` if no
    /// mount matches or the path tries to escape the mount root.
    pub fn resolve(&self, guest_path: &str) -> Option<PathBuf> {
        let mut normalised = guest_path.replace('\\', "/").to_ascii_lowercase();
        while normalised.contains("//") {
            normalised = normalised.replace("//", "/");
        }
        let with_leading = if normalised.starts_with('/') {
            normalised
        } else {
            format!("/{normalised}")
        };
        let mount = self.matching_mount(&with_leading)?;
        let rel = &with_leading[mount.prefix.len()..];
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
                    log::warn!("vfs.resolve: refusing escape via {guest_path:?}");
                    return None;
                }
            }
        }
        Some(p)
    }

    /// Open a host file behind a guest path. Returns the handle id.
    pub fn open(&mut self, guest_path: &str, access: Access, create: bool) -> Option<u32> {
        let mount = self.matching_mount(guest_path)?;
        if mount.read_only && !matches!(access, Access::Read) {
            log::debug!("vfs.open: refusing write to read-only mount {guest_path:?}");
            return None;
        }
        let host_path = self.resolve(guest_path)?;
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

    pub fn close(&mut self, handle: u32) -> bool {
        self.handles.remove(&handle).is_some()
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
