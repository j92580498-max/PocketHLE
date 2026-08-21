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

use rmp3::{DecoderOwned, Frame};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

/// `INVALID_HANDLE_VALUE` from `<windows.h>`.
pub const INVALID_HANDLE_VALUE: u32 = 0xFFFF_FFFF;

/// First handle handed out. Picked to be obviously not a small Win32
/// pseudo-handle and not collide with the GDI fake-handle range.
const HANDLE_BASE: u32 = 0x4000_0000;

/// Windows CE exposes every mounted volume as a `Vol:` pseudo-file
/// inside it, so `CreateFileW("\\SD Card\\Vol:")` yields a handle that
/// `DeviceIoControl` accepts for storage queries. It is not a byte
/// stream — nothing reads or writes it.
const VOLUME_SPECIAL_FILE: &str = "vol:";

/// The Gizmondo's hardware MP3 decoder, exposed by Windows CE as the
/// stream device `MAS1:` (the Micronas MAS chip behind the console's
/// audio). A title plays music by opening it, configuring it with a
/// `DeviceIoControl`, and then writing MP3 frames to it.
///
/// It has to open even though nothing here decodes MP3, because a
/// missing device is not a case these games handle: Ball Busters builds
/// its music player by opening `MAS1:` first and the file second, and on
/// failure leaves the player zeroed — then calls it anyway on the next
/// loading tick and dereferences a NULL stream. Accepting the device and
/// swallowing the frames is what lets the game past its loading screen.
const MP3_DECODER_DEVICE: &str = "mas1:";

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

#[derive(Debug)]
struct Mp3DecoderState {
    bytes_seen: u64,
    encoded: Vec<u8>,
    decoded_offset: usize,
    pcm: Vec<i16>,
    sample_rate: u32,
    channels: u16,
    started: bool,
    paused: bool,
    volume: u32,
}

/// A handle opened on a volume's `Vol:` pseudo-file instead of on a
/// regular file. Carries the mount it names so `DeviceIoControl` can
/// report that volume's size and free space.
#[derive(Debug, Clone)]
pub struct OpenVolume {
    /// Guest mount prefix, lower-cased with `/` separators (`/sd card/`).
    pub prefix: String,
    /// Host directory backing the volume.
    pub host_dir: PathBuf,
    /// Whether the mount refuses writes.
    pub read_only: bool,
}

impl OpenVolume {
    /// The card's serial number, as the storage driver would report it.
    ///
    /// Real removable media carries one, and a guest that asks for it and
    /// gets zero concludes there is no card in the slot. Gizmondo titles
    /// do exactly that during startup, so a volume has to have a serial
    /// for one to boot at all.
    ///
    /// A Gizmondo card states its own serial: alongside the game
    /// directory it carries a four-byte marker file with the same name as
    /// that directory (`\SD Card\GZGA200045\GZGA200045`), holding the
    /// serial of the card the title was published on. Reporting that
    /// value is what makes the card in the slot *be* the card the content
    /// was written for, which is the situation the game is checking for.
    /// Any other volume gets a serial derived from its own host path:
    /// arbitrary, but stable across runs, which is all an unrelated guest
    /// can reasonably expect of one.
    pub fn serial(&self) -> u32 {
        if let Some(declared) = self.declared_serial() {
            return declared;
        }
        // FNV-1a over the host path, forced non-zero.
        let mut hash: u32 = 0x811c_9dc5;
        for byte in self.host_dir.to_string_lossy().as_bytes() {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        hash | 1
    }

    /// The serial this volume's own contents declare, if it carries a
    /// game-directory marker file. See [`Self::serial`].
    fn declared_serial(&self) -> Option<u32> {
        let entries = std::fs::read_dir(&self.host_dir).ok()?;
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name();
            let marker = entry.path().join(&name);
            let Ok(bytes) = std::fs::read(&marker) else {
                continue;
            };
            if let Ok(four) = <[u8; 4]>::try_from(bytes.as_slice()) {
                let serial = u32::from_le_bytes(four);
                log::debug!(
                    "volume {:?} declares serial {serial} in {marker:?}",
                    self.prefix
                );
                return Some(serial);
            }
        }
        None
    }
}

#[derive(Debug, Clone)]
struct Mount {
    prefix: String,
    /// `prefix` with its original capitalisation. Enumerating a parent
    /// directory reports a nested mount point by name, and a guest that
    /// mounted `\SD Card\` should see `SD Card` back rather than the
    /// lower-cased form matching uses internally.
    display_prefix: String,
    host_dir: PathBuf,
    read_only: bool,
}

/// The Gizmondo registration service device exposed as `REG1:`.
const REGISTRATION_SERVICE_DEVICE: &str = "reg1:";

/// Mount-point + open-handle table.
pub struct Vfs {
    mounts: Vec<Mount>,
    handles: HashMap<u32, OpenFile>,
    /// Handles opened on the `MAS1:` MP3 decoder.
    decoders: HashMap<u32, Mp3DecoderState>,
    /// Handles opened on a `Vol:` pseudo-file. Kept apart from
    /// `handles` because they have no backing [`File`] — a volume
    /// handle only ever reaches `DeviceIoControl` and `CloseHandle`.
    volumes: HashMap<u32, OpenVolume>,
    next_handle: u32,
    /// Handles opened on the Gizmondo registration service device.
    registration: std::collections::HashSet<u32>,
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
            volumes: HashMap::new(),
            decoders: HashMap::new(),
            next_handle: HANDLE_BASE,
            registration: std::collections::HashSet::new(),
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
        // The same shape with the capitalisation kept. `to_ascii_lowercase`
        // preserves byte length, so the two strings stay index-compatible
        // and a slice taken from one can be taken from the other.
        let mut display = guest_prefix.replace('\\', "/");
        if !display.starts_with('/') {
            display.insert(0, '/');
        }
        while display.contains("//") {
            display = display.replace("//", "/");
        }
        if !display.ends_with('/') {
            display.push('/');
        }
        self.mounts.push(Mount {
            prefix: p,
            display_prefix: display,
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
        let is_device_path = Path::new(&normalised)
            .file_name()
            .map(|name| name.to_string_lossy().ends_with(':'))
            .unwrap_or(false);
        if is_device_path {
            return None;
        }
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
        // A mount point nested below this directory is a real directory to
        // the guest even though no host directory contains it. Windows CE
        // has no drive letters: a storage card *is* a directory in the
        // object-store root, so `\` must enumerate `\SD Card` or the card
        // does not exist as far as the guest is concerned. Ball Busters
        // runs from the card and watches `\` with
        // `FindFirstChangeNotificationW` to notice it being pulled; with an
        // empty root the watch could not be set up and the game sat on its
        // "SD card removed" screen.
        let dir_prefix = if normalised.ends_with('/') {
            normalised.clone()
        } else {
            format!("{normalised}/")
        };
        for mount in &self.mounts {
            let Some(rel) = mount.prefix.strip_prefix(&dir_prefix) else {
                continue;
            };
            let Some(child) = rel.split('/').find(|part| !part.is_empty()) else {
                continue;
            };
            let display = &mount.display_prefix[dir_prefix.len()..][..child.len()];
            merged
                .entry(child.to_string())
                .or_insert((display.to_string(), 0, true));
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

    /// The mount a `Vol:` pseudo-path names, if any.
    ///
    /// `\SD Card\Vol:` resolves to the mount at `\SD Card\`; the volume
    /// exists exactly when that mount does, which is what makes a failed
    /// open mean "no card in the slot" on a real device.
    fn volume_mount(&self, normalised: &str) -> Option<&Mount> {
        let parent = normalised.strip_suffix(VOLUME_SPECIAL_FILE)?;
        let root = parent.trim_end_matches('/');
        self.mounts
            .iter()
            .filter(|mount| mount.prefix.trim_end_matches('/') == root)
            .max_by_key(|mount| usize::from(!mount.read_only))
    }

    /// Open a host file behind a guest path. Returns the handle id.
    pub fn open(&mut self, guest_path: &str, access: Access, create: bool) -> Option<u32> {
        let normalised = self.normalise_guest_path(guest_path);
        // The MP3 decoder is a bare device name, not a path under any
        // mount, so it cannot be found by resolving against the
        // filesystem and has to be recognised first. Match on the last
        // component: CE device names are global, and normalising a
        // relative one like `MAS1:` prefixes it with the module's
        // directory (`/sd card/mas1:`).
        if normalised
            .rsplit('/')
            .next()
            .is_some_and(|leaf| leaf == MP3_DECODER_DEVICE)
        {
            let h = self.next_handle;
            self.next_handle += 1;
            log::debug!("vfs.open({guest_path:?}) -> MP3 decoder handle 0x{h:08x}");
            self.decoders.insert(
                h,
                Mp3DecoderState {
                    bytes_seen: 0,
                    encoded: Vec::new(),
                    decoded_offset: 0,
                    pcm: Vec::new(),
                    sample_rate: 0,
                    channels: 0,
                    started: false,
                    paused: false,
                    volume: 0xFFFF_FFFF,
                },
            );
            return Some(h);
        }
        // Gizmondo titles open REG1: to query the device registration
        // service before creating their first window. HLE has no licensing
        // service, but the device must exist so the title can continue.
        if normalised.rsplit('/').next() == Some(REGISTRATION_SERVICE_DEVICE) {
            let h = self.next_handle;
            self.next_handle += 1;
            self.registration.insert(h);
            log::debug!("vfs.open({guest_path:?}) -> registration service handle 0x{h:08x}");
            return Some(h);
        }
        // A volume handle has to be checked for before the regular file
        // path: `Vol:` is not a file, so `resolve` would fall through to
        // its recursive basename search and then fail to open the result.
        if let Some(mount) = self.volume_mount(&normalised) {
            let volume = OpenVolume {
                prefix: mount.prefix.clone(),
                host_dir: mount.host_dir.clone(),
                read_only: mount.read_only,
            };
            let h = self.next_handle;
            self.next_handle += 1;
            log::debug!("vfs.open({guest_path:?}) -> volume handle 0x{h:08x} on {volume:?}");
            self.volumes.insert(h, volume);
            return Some(h);
        }
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
        if self.registration.contains(&handle) {
            buf.fill(0);
            return Some(0);
        }
        let of = self.handles.get_mut(&handle)?;
        of.file.read(buf).ok()
    }

    pub fn write(&mut self, handle: u32, buf: &[u8]) -> Option<usize> {
        if self.registration.contains(&handle) {
            return Some(buf.len());
        }
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
            || self.volumes.remove(&handle).is_some()
            || self.decoders.remove(&handle).is_some()
            || self.registration.remove(&handle)
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
        self.volumes.clear();
        self.decoders.clear();
        self.registration.clear();
        n
    }

    pub fn is_open(&self, handle: u32) -> bool {
        self.handles.contains_key(&handle)
    }

    /// Whether `handle` came from opening the Gizmondo registration service.
    pub fn is_registration_service(&self, handle: u32) -> bool {
        self.registration.contains(&handle)
    }

    /// The volume a handle was opened on, when it came from a `Vol:`
    /// pseudo-path. `None` for regular files, which is what lets
    /// `DeviceIoControl` tell a storage query from a nonsense one.
    pub fn volume(&self, handle: u32) -> Option<&OpenVolume> {
        self.volumes.get(&handle)
    }

    /// Whether `handle` came from opening the `MAS1:` MP3 decoder.
    pub fn is_mp3_decoder(&self, handle: u32) -> bool {
        self.decoders.contains_key(&handle)
    }

    pub fn feed_mp3_decoder_data(&mut self, handle: u32, data: &[u8]) -> Option<u64> {
        let state = self.decoders.get_mut(&handle)?;
        state.bytes_seen = state.bytes_seen.saturating_add(data.len() as u64);
        state.encoded.extend_from_slice(data);
        if state.paused {
            return Some(state.bytes_seen);
        }

        let tail = state.encoded[state.decoded_offset..].to_vec();
        let mut decoder = DecoderOwned::new(tail);
        let mut decoded_any = false;
        while let Some(frame) = decoder.next() {
            if let Frame::Audio(audio) = frame {
                if state.sample_rate == 0 {
                    state.sample_rate = audio.sample_rate();
                    state.channels = audio.channels();
                }
                state.pcm.extend_from_slice(audio.samples());
                decoded_any = true;
            }
        }
        state.decoded_offset = state.decoded_offset.saturating_add(decoder.position());
        if state.decoded_offset > 1 << 20 {
            state.encoded.drain(..state.decoded_offset);
            state.decoded_offset = 0;
        }
        state.started |= decoded_any;
        Some(state.bytes_seen)
    }

    pub fn feed_mp3_decoder(&mut self, handle: u32, len: u64) -> Option<u64> {
        let state = self.decoders.get_mut(&handle)?;
        state.bytes_seen = state.bytes_seen.saturating_add(len);
        Some(state.bytes_seen)
    }

    pub fn stop_mp3_decoder(&mut self, handle: u32) {
        if let Some(state) = self.decoders.get_mut(&handle) {
            state.encoded.clear();
            state.decoded_offset = 0;
            state.started = false;
            state.paused = false;
            state.pcm.clear();
        }
    }

    pub fn pause_mp3_decoder(&mut self, handle: u32, paused: bool) {
        if let Some(state) = self.decoders.get_mut(&handle) {
            state.paused = paused;
        }
    }

    pub fn set_mp3_decoder_volume(&mut self, handle: u32, volume: u32) {
        if let Some(state) = self.decoders.get_mut(&handle) {
            state.volume = volume;
        }
    }

    pub fn mp3_decoder_reply(&self, handle: u32, code: u32, len: usize) -> Vec<u8> {
        let Some(state) = self.decoders.get(&handle) else {
            return vec![0; len];
        };
        let value = if code == 0x001d_1030 {
            1
        } else if code == 0x001d_1010 {
            if state.paused {
                5
            } else if state.started {
                4
            } else {
                0
            }
        } else if code == 0x001d_1018 {
            state.bytes_seen.min(u32::MAX as u64) as u32
        } else if code == 0x001d_1020 {
            state.volume
        } else {
            0
        };
        let mut reply = vec![0; len];
        if len >= 4 {
            reply[..4].copy_from_slice(&value.to_le_bytes());
        }
        reply
    }

    pub fn take_mp3_decoder_pcm(&mut self, handle: u32) -> Option<(u32, u16, Vec<i16>)> {
        let state = self.decoders.get_mut(&handle)?;
        if state.pcm.is_empty() || state.sample_rate == 0 || state.channels == 0 {
            return None;
        }
        Some((
            state.sample_rate,
            state.channels,
            std::mem::take(&mut state.pcm),
        ))
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

    /// `\SD Card\Vol:` is a handle on the volume, not a file. It must
    /// open even though no such host file exists, and it must not land
    /// in the file table — a volume handle only ever reaches
    /// `DeviceIoControl` and `CloseHandle`.
    #[test]
    fn volume_pseudo_file_opens_without_a_backing_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut v = Vfs::new();
        v.mount("\\SD Card\\", dir.path());

        let h = v.open("\\SD Card\\Vol:", Access::Read, false).unwrap();
        assert!(v.volume(h).is_some(), "handle should name a volume");
        assert!(!v.is_open(h), "a volume is not an open file");
        assert_eq!(v.volume(h).unwrap().prefix, "/sd card/");
        // Case is irrelevant on Windows CE, and the file does not exist
        // on the host either way.
        assert!(v.open("\\sd card\\vol:", Access::Read, false).is_some());
        assert!(v.close(h));
        assert!(v.volume(h).is_none());
    }

    /// A Gizmondo card states its own serial in a four-byte marker file
    /// named after the game directory that holds it. Reporting that
    /// value is what makes the card in the slot be the card the title
    /// was published on — Ball Busters checks and refuses to boot
    /// otherwise.
    #[test]
    fn volume_serial_comes_from_the_gizmondo_card_marker() {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("GZGA200045");
        std::fs::create_dir(&game).unwrap();
        std::fs::write(game.join("GZGA200045"), 200_045u32.to_le_bytes()).unwrap();

        let mut v = Vfs::new();
        v.mount("\\SD Card\\", dir.path());
        let h = v.open("\\SD Card\\Vol:", Access::Read, false).unwrap();
        assert_eq!(v.volume(h).unwrap().serial(), 200_045);
    }

    /// A volume with no marker still needs a serial: zero reads as "no
    /// card in the slot". It also has to be the same value next run, or
    /// a guest that remembers which card it saw sees a new one.
    #[test]
    fn volume_without_a_marker_has_a_stable_nonzero_serial() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loose.txt"), b"x").unwrap();
        let mut v = Vfs::new();
        v.mount("\\Storage Card\\", dir.path());
        let h = v.open("\\Storage Card\\Vol:", Access::Read, false).unwrap();
        let first = v.volume(h).unwrap().serial();
        assert_ne!(first, 0);

        let mut again = Vfs::new();
        again.mount("\\Storage Card\\", dir.path());
        let h2 = again
            .open("\\Storage Card\\Vol:", Access::Read, false)
            .unwrap();
        assert_eq!(again.volume(h2).unwrap().serial(), first);
    }

    /// The `MAS1:` MP3 decoder is a stream device, not a file. It has to
    /// open with no mount behind it and accept written frames, because a
    /// title that fails to open it leaves its music player zeroed and
    /// then uses it anyway.
    #[test]
    fn mp3_decoder_device_opens_and_swallows_frames() {
        let mut v = Vfs::new();
        let h = v.open("MAS1:", Access::Write, false).unwrap();
        assert!(v.is_mp3_decoder(h));
        assert!(!v.is_open(h), "the decoder is not a file");
        assert!(v.volume(h).is_none());
        assert_eq!(v.feed_mp3_decoder(h, 417), Some(417));
        assert_eq!(v.feed_mp3_decoder(h, 417), Some(834));
        // Case-insensitive, like every other path here.
        assert!(v.open("mas1:", Access::Write, false).is_some());
        assert!(v.close(h));
        assert!(!v.is_mp3_decoder(h));
        assert_eq!(v.feed_mp3_decoder(h, 417), None);
    }

    /// Windows CE has no drive letters: a storage card is a directory in
    /// the object-store root. Enumerating `\` therefore has to report
    /// the mount point, with the capitalisation the mount was made with,
    /// or the card does not exist as far as the guest is concerned.
    #[test]
    fn root_enumeration_reports_nested_mount_points() {
        let dir = tempfile::tempdir().unwrap();
        let mut v = Vfs::new();
        v.mount("\\SD Card\\", dir.path());
        let entries = v.list_dir("\\").expect("the root is always a directory");
        assert!(
            entries
                .iter()
                .any(|(name, _, is_dir)| name == "SD Card" && *is_dir),
            "root should list the card: {entries:?}"
        );
    }
}
