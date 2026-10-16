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

use rmp3::{Frame, RawDecoder, MAX_SAMPLES_PER_FRAME};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

#[path = "vfs_contract.rs"]
mod contracts;
pub use contracts::{OpenResult, VfsShared};

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
const MAX_FILE_MAPPING_BYTES: u64 = 64 * 1024 * 1024;

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
    /// Whether the CRT stream was opened in text mode (`fopen` without
    /// `b`). Only the stdio layer in `pocket-winceapi` consults this —
    /// `CreateFileW` has no text mode on Windows CE, so handles opened
    /// there are always binary.
    pub text_mode: bool,
    lease: Option<Arc<contracts::Lease>>,
    append: bool,
}

#[derive(Debug)]
struct FileMapping {
    host_path: Option<PathBuf>,
    anonymous_bytes: Option<Vec<u8>>,
    size: u64,
    writable: bool,
    source_file_handle: Option<u32>,
    open_views: usize,
    closed: bool,
}

// MPEG Layer III frame size, used to retain a split frame until the next
// compressed buffer arrives. Passing an incomplete tail to minimp3 lets its
// garbage scan consume it and reset the reservoir, causing clicks/lost frames.
fn mp3_frame_bytes(header: &[u8]) -> Option<usize> {
    if header.len() < 4 || header[0] != 0xff || header[1] & 0xe0 != 0xe0 {
        return None;
    }
    let version = (header[1] >> 3) & 3;
    if version == 1 || (header[1] >> 1) & 3 != 1 {
        return None;
    }
    let index = (header[2] >> 4) as usize;
    let rate_index = ((header[2] >> 2) & 3) as usize;
    if index == 0 || index == 15 || rate_index == 3 {
        return None;
    }
    let rates = [44100usize, 48000, 32000];
    let bitrate = if version == 3 {
        [
            0usize, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
        ][index]
    } else {
        [
            0usize, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
        ][index]
    };
    let rate = rates[rate_index]
        / if version == 3 {
            1
        } else if version == 2 {
            2
        } else {
            4
        };
    let coefficient = if version == 3 { 144000 } else { 72000 };
    Some(coefficient * bitrate / rate + ((header[2] >> 1) & 1) as usize)
}

struct Mp3DecoderState {
    stream_key: u32,
    decoder: RawDecoder,
    callback_event: u32,
    pending: VecDeque<(u64, u64)>, // cumulative PCM samples / compressed bytes
    completed_samples: u64,
    completed_bytes: u64,
    played_bytes: u64,
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

impl std::fmt::Debug for Mp3DecoderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mp3DecoderState")
            .field("bytes_seen", &self.bytes_seen)
            .field("pending", &self.pending)
            .field("callback_event", &self.callback_event)
            .finish_non_exhaustive()
    }
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
    /// Gizmondo's card marker is not a raw serial: the same-named file in
    /// a GZGA directory (or directly in a flattened game root) stores
    /// `numeric_id - serial - 1`, modulo 2^32.
    /// Decode it so all titles from one card report the same volume identity.
    /// Other volumes keep a stable serial derived from their host path.
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
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if !kind.is_dir() && !kind.is_file() {
                continue;
            }
            let name = entry.file_name();
            let upper = name.to_string_lossy().to_ascii_uppercase();
            let Some(digits) = upper.strip_prefix("GZGA") else {
                continue;
            };
            if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let Ok(numeric_id) = digits.parse::<u32>() else {
                continue;
            };
            // Independent imports flatten the card directory. The same serial
            // marker then sits directly beside autorun.exe (Toy Golf and other
            // SDK titles still validate it against the reported volume serial).
            let marker = if kind.is_dir() {
                entry.path().join(&name)
            } else {
                entry.path()
            };
            let Ok(bytes) = std::fs::read(&marker) else {
                continue;
            };
            if let Ok(four) = <[u8; 4]>::try_from(bytes.as_slice()) {
                let encoded = u32::from_le_bytes(four);
                let serial = numeric_id.wrapping_sub(encoded).wrapping_sub(1);
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
    /// Read-only mounts are a session snapshot for recursive fallback.
    /// Exact lookups remain live; writable mounts never use this index.
    fallback_files: RefCell<Option<Vec<PathBuf>>>,
}

/// The Gizmondo registration service device exposed as `REG1:`.
const REGISTRATION_SERVICE_DEVICE: &str = "reg1:";

/// RAM-backed root files, shared by all processes on one emulated device.
#[derive(Debug, Default)]
pub(crate) struct RamStore {
    files: HashMap<String, Arc<Mutex<RamFile>>>,
    dirs: HashMap<String, crate::memory_division::StoreCharge>,
}
#[derive(Debug)]
struct RamFile {
    data: Vec<u8>,
    charge: crate::memory_division::StoreCharge,
    name_bytes: u64,
}
impl RamFile {
    fn set_len(&mut self, len: usize) -> bool {
        let Some(bytes) = (len as u64).checked_add(self.name_bytes) else {
            return false;
        };
        let pages = crate::memory_division::pages(bytes);
        if !self.charge.resize(pages) {
            return false;
        }
        if len > self.data.len() && self.data.try_reserve(len - self.data.len()).is_err() {
            self.charge.resize(crate::memory_division::pages(
                self.data.len() as u64 + self.name_bytes,
            ));
            return false;
        }
        self.data.resize(len, 0);
        true
    }
}
#[derive(Debug)]
struct OpenRamFile {
    file: Arc<Mutex<RamFile>>,
    position: u64,
    access: Access,
    text_mode: bool,
    lease: Option<Arc<contracts::Lease>>,
    append: bool,
}

static NEXT_MAS_STREAM: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0xd3000000);

/// Cloneable open description for transfer between process namespaces.
#[derive(Clone)]
pub struct VfsObject(VfsObjectInner, Option<(u32, crate::audio::MasPlayback)>);
#[derive(Clone)]
enum VfsObjectInner {
    Bluetooth(Arc<BluetoothOpen>),
    BluetoothDriver(Arc<BluetoothDriverOpen>),
    Camera(Arc<CameraOpen>),
    Gps(Arc<GpsOpen>),
    Vibration(Arc<VibrationOpen>),
    File(Arc<Mutex<OpenFile>>),
    Ram(Arc<Mutex<OpenRamFile>>),
    Volume(OpenVolume),
    Registration,
    Decoder(Arc<Mutex<Mp3DecoderState>>),
}
impl VfsObject {
    pub fn with_mas(mut self, key: u32, playback: crate::audio::MasPlayback) -> Self {
        self.1 = Some((key, playback));
        self
    }
    pub fn mas(&self) -> Option<(u32, crate::audio::MasPlayback)> {
        self.1.clone()
    }
}

/// Mount-point + open-handle table.
pub struct VibrationOpen {
    pub service: crate::vibration::Service,
    pub access: u32,
    _lease: Arc<contracts::Lease>,
}
pub struct GpsOpen {
    pub device: Arc<Mutex<crate::gps::Device>>,
    pub access: u32,
    _lease: Arc<contracts::Lease>,
}
pub struct CameraOpen {
    pub device: Arc<Mutex<crate::camera::Device>>,
    pub access: u32,
    _lease: Arc<contracts::Lease>,
}
pub struct BluetoothDriverOpen {
    pub service: crate::bluetooth::Service,
    pub access: u32,
    _lease: Arc<contracts::Lease>,
}
pub struct BluetoothOpen {
    pub port: Arc<crate::bluetooth::Port>,
    pub access: u32,
    _lease: Arc<contracts::Lease>,
}

/// Mount-point + open-handle table.
pub struct Vfs {
    pub bluetooth: crate::bluetooth::State,
    bluetooth_handles: HashMap<u32, Arc<BluetoothOpen>>,
    bluetooth_driver_handles: HashMap<u32, Arc<BluetoothDriverOpen>>,
    camera_handles: HashMap<u32, Arc<CameraOpen>>,
    gps_handles: HashMap<u32, Arc<GpsOpen>>,
    vibration_handles: HashMap<u32, Arc<VibrationOpen>>,
    pub camera_deadlines: HashMap<(usize, u32, u32), std::time::Instant>,
    mounts: Vec<Mount>,
    shared: VfsShared,
    ram: Option<crate::memory_division::MemoryDivision>,
    ram_handles: HashMap<u32, Arc<Mutex<OpenRamFile>>>,
    handles: HashMap<u32, Arc<Mutex<OpenFile>>>,
    file_mappings: HashMap<u32, FileMapping>,
    mapping_file_handles: HashSet<u32>,
    /// Handles opened on the `MAS1:` MP3 decoder.
    decoders: HashMap<u32, Arc<Mutex<Mp3DecoderState>>>,
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
    /// Process-local opt-in: other games on a demo card must not inherit Colors aliases.
    colors_language_extensions: &'static [&'static str],
}

impl Default for Vfs {
    fn default() -> Self {
        Self::new()
    }
}

impl Vfs {
    pub fn new() -> Self {
        let mut vfs = Self {
            bluetooth: Default::default(),
            bluetooth_handles: HashMap::new(),
            bluetooth_driver_handles: HashMap::new(),
            camera_handles: HashMap::new(),
            gps_handles: HashMap::new(),
            vibration_handles: HashMap::new(),
            camera_deadlines: HashMap::new(),
            mounts: Vec::new(),
            shared: VfsShared::default(),
            ram: None,
            ram_handles: HashMap::new(),
            handles: HashMap::new(),
            file_mappings: HashMap::new(),
            mapping_file_handles: HashSet::new(),
            volumes: HashMap::new(),
            decoders: HashMap::new(),
            next_handle: HANDLE_BASE,
            registration: std::collections::HashSet::new(),
            default_dir: "\\".to_string(),
            colors_language_extensions: &[],
        };
        vfs.bluetooth.service = vfs.bluetooth_service();
        vfs
    }

    pub fn attach_ram(&mut self, ram: Option<crate::memory_division::MemoryDivision>) {
        self.ram = ram;
    }
    /// External mounts, including read-only card/ROM overlays, always win.
    pub fn is_ram_path(&self, path: &str) -> bool {
        self.ram.is_some() && self.matching_mounts(path).is_empty()
    }
    pub fn ram_file_size(&self, path: &str) -> Option<u64> {
        if !self.is_ram_path(path) {
            return None;
        }
        let ram = self.ram.as_ref()?;
        let store = ram.files.lock().ok()?;
        let file = store
            .files
            .get(&self.normalise_guest_path(path))?
            .lock()
            .ok()?;
        Some(file.data.len() as u64)
    }
    fn ram_key(&self, path: &str) -> Option<String> {
        let path = self.normalise_guest_path(path);
        if path
            .split('/')
            .any(|part| part == ".." || part == "." || part.contains(':'))
        {
            return None;
        }
        Some(path.trim_end_matches('/').to_string())
    }
    fn open_ram(&mut self, path: &str, access: Access, create: bool) -> Option<u32> {
        let key = self.ram_key(path)?;
        if key.is_empty() {
            return None;
        }
        let ram = self.ram.as_ref()?;
        let mut store = ram.files.lock().ok()?;
        if store.dirs.contains_key(&key) {
            return None;
        }
        let file = if let Some(file) = store.files.get(&key) {
            if create && access == Access::Write && !file.lock().ok()?.set_len(0) {
                return None;
            }
            file.clone()
        } else {
            if !create || access == Access::Read {
                return None;
            }
            let parent = key.rsplit_once('/')?.0;
            if !parent.is_empty() && !store.dirs.contains_key(parent) {
                return None;
            }
            // Charge the encoded pathname and data, not a made-up disk size.
            let name_bytes = (key.encode_utf16().count() as u64 + 1) * 2;
            let charge = ram.store_charge(crate::memory_division::pages(name_bytes))?;
            let file = Arc::new(Mutex::new(RamFile {
                data: Vec::new(),
                charge,
                name_bytes,
            }));
            store.files.insert(key, file.clone());
            file
        };
        drop(store);
        let handle = self.next_handle;
        self.next_handle += 1;
        self.ram_handles.insert(
            handle,
            Arc::new(Mutex::new(OpenRamFile {
                file,
                position: 0,
                access,
                text_mode: false,
                lease: None,
                append: false,
            })),
        );
        Some(handle)
    }
    pub fn read_failure_error(&self, handle: u32) -> u32 {
        if self.lease(handle).is_some_and(|l| l.access & 1 == 0) {
            5
        } else {
            30
        }
    }
    pub fn write_failure_error(&self, handle: u32) -> u32 {
        if let Some(open) = self.ram_handles.get(&handle) {
            if open.lock().is_ok_and(|open| open.access == Access::Read) {
                return 5;
            }
            return 112; // RAM object store full
        }
        if let Some(open) = self.handles.get(&handle) {
            if open.lock().is_ok_and(|open| open.access == Access::Read) {
                return 5;
            }
        }
        if self.lease(handle).is_some_and(|l| l.volume.is_some()) {
            return 112;
        }
        29 // host backing write failed
    }
    /// SetEndOfFile acts on the shared open description, including duplicates.
    fn set_end_of_file_raw(&mut self, handle: u32) -> bool {
        if let Some(open) = self.ram_handles.get(&handle) {
            let Ok(open) = open.lock() else {
                return false;
            };
            if open.access == Access::Read {
                return false;
            }
            let Ok(len) = usize::try_from(open.position) else {
                return false;
            };
            let Ok(mut file) = open.file.lock() else {
                return false;
            };
            return file.set_len(len);
        }
        let Some(open) = self.handles.get(&handle) else {
            return false;
        };
        let Ok(mut open) = open.lock() else {
            return false;
        };
        if open.access == Access::Read {
            return false;
        }
        let Ok(position) = open.file.stream_position() else {
            return false;
        };
        open.file.set_len(position).is_ok()
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
            fallback_files: RefCell::new(None),
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
                Component::ParentDir => {
                    // Chopper Fight runs from `bin/` and opens sibling resources.
                    // Allow this only while it stays inside the mounted install root.
                    if p == mount.host_dir {
                        log::warn!("vfs.resolve: refusing escape via {normalised:?}");
                        return None;
                    }
                    p.pop();
                }
                Component::RootDir | Component::Prefix(_) => {
                    log::warn!("vfs.resolve: refusing escape via {normalised:?}");
                    return None;
                }
            }
        }
        Some(p)
    }

    /// Rank fallback files by matching trailing path components. A basename
    /// alone is safe only if unique; wrappers around extracted archives must
    /// not turn Fonts/font.cmf into Frontend/Font/font.cmf (Jump uses two
    /// incompatible font formats under those names).
    fn collect_fallback_files(root: &Path) -> Vec<PathBuf> {
        let mut pending = vec![(root.to_path_buf(), 0usize)];
        let mut files = Vec::new();
        while let Some((dir, depth)) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    files.push(path);
                } else if depth < 16 && path.is_dir() {
                    pending.push((path, depth + 1));
                }
            }
        }
        files
    }

    fn rank_fallback_files(
        root: &Path,
        wanted: &[&str],
        files: &[PathBuf],
    ) -> (usize, Vec<PathBuf>) {
        let mut best = 0;
        let mut found = Vec::new();
        for path in files {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let score = relative
                .components()
                .rev()
                .zip(wanted.iter().rev())
                .take_while(|(component, expected)| {
                    component
                        .as_os_str()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(expected)
                })
                .count();
            if score > best {
                best = score;
                found.clear();
            }
            if score != 0 && score == best && found.len() < 2 {
                found.push(path.clone());
            }
        }
        (best, found)
    }

    fn find_path_recursive(mount: &Mount, wanted: &[&str]) -> (usize, Vec<PathBuf>) {
        if mount.read_only {
            let mut files = mount.fallback_files.borrow_mut();
            let files = files.get_or_insert_with(|| Self::collect_fallback_files(&mount.host_dir));
            Self::rank_fallback_files(&mount.host_dir, wanted, files)
        } else {
            let files = Self::collect_fallback_files(&mount.host_dir);
            Self::rank_fallback_files(&mount.host_dir, wanted, &files)
        }
    }

    /// Colors only declares English in its binary. Redirect its read-side resource
    /// lookup without editing card files; writes keep the exact guest destination.
    /// Frontends enable this only for the GZGA200015 process, at the next launch.
    pub fn set_colors_language(&mut self, language: &str) {
        self.colors_language_extensions = match language {
            "french" => &["fre", "fra"],
            "german" => &["ger", "deu"],
            "italian" => &["ita"],
            "spanish" => &["spa", "esp"],
            _ => &[],
        };
    }

    /// Translate a guest path to a host path, including the Colors read alias.
    pub fn resolve(&self, guest_path: &str) -> Option<PathBuf> {
        let english = self.resolve_original(guest_path)?;
        if self.colors_language_extensions.is_empty()
            || !english
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("bstrings.eng"))
        {
            return Some(english);
        }
        // Stay beside the resolved English resource. Never borrow a translation
        // from another title or another Colors installation via basename fallback.
        let Some(parent) = english.parent() else {
            return Some(english);
        };
        let Ok(entries) = std::fs::read_dir(parent) else {
            return Some(english);
        };
        let files: Vec<_> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .collect();
        for extension in self.colors_language_extensions {
            let name = format!("bstrings.{extension}");
            let mut matches = files.iter().filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&name)
            });
            if let Some(found) = matches.next() {
                // Case-colliding resources are ambiguous on case-sensitive hosts.
                if matches.next().is_none() {
                    return Some(found.path());
                }
            }
        }
        Some(english)
    }

    /// Existing files fall back through broader mounts, allowing a writable
    /// save overlay to sit above a read-only extracted game directory.
    fn resolve_original(&self, guest_path: &str) -> Option<PathBuf> {
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
        let mut known_parent = false;
        // Exhaust exact paths before considering a fallback from any mount.
        for mount in &mounts {
            let Some(path) = self.host_path_for_mount(mount, &normalised) else {
                continue;
            };
            if fallback.is_none() {
                fallback = Some(path.clone());
            }
            if path.exists() {
                return Some(path);
            }
            known_parent |= path.parent().is_some_and(|parent| parent.is_dir());
            // A missing intermediate component does not make the requested
            // existing directory disappear (e.g. app/app/asset). Keep the
            // basename-only fallback from crossing into a sibling directory.
            known_parent |= path
                .ancestors()
                .skip(1)
                .take_while(|parent| *parent != mount.host_dir.as_path())
                .any(|parent| parent.is_dir());
        }
        let wanted: Vec<_> = normalised
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        let mut best = 0;
        let mut candidates = Vec::new();
        for mount in mounts {
            let (score, found) = Self::find_path_recursive(mount, &wanted);
            if score > best {
                best = score;
                candidates.clear();
            }
            if score != 0 && score == best {
                for path in found {
                    if !candidates.contains(&path) && candidates.len() < 2 {
                        candidates.push(path);
                    }
                }
            }
        }
        // A known directory prefix with a missing leaf must not borrow a file
        // from another game's directory merely because its basename is unique.
        // Keep suffix matches for wrapped install layouts (including Jump).
        if known_parent && best == 1 {
            log::debug!("vfs.resolve: missing file below an existing directory {normalised:?}; refusing basename substitution");
            return fallback;
        }
        if candidates.len() == 1 {
            let path = candidates.pop().unwrap();
            log::debug!("vfs.resolve: path fallback {normalised:?} -> {path:?} ({best} matching components)");
            return Some(path);
        }
        if candidates.len() > 1 {
            log::warn!("vfs.resolve: ambiguous fallback for {normalised:?}; refusing to substitute another asset");
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
        let mut ram_directory = false;
        if self.is_ram_path(guest_dir) {
            if let Some(ram) = &self.ram {
                if let Ok(store) = ram.files.lock() {
                    let key = normalised.trim_end_matches('/');
                    ram_directory = key.is_empty() || store.dirs.contains_key(key);
                    let prefix = format!("{key}/");
                    for (path, file) in &store.files {
                        if let Some(name) = path
                            .strip_prefix(&prefix)
                            .filter(|name| !name.contains('/'))
                        {
                            if let Ok(file) = file.lock() {
                                merged.insert(
                                    name.to_string(),
                                    (name.to_string(), file.data.len() as u64, false),
                                );
                            }
                        }
                    }
                    for path in store.dirs.keys() {
                        if let Some(name) = path
                            .strip_prefix(&prefix)
                            .filter(|name| !name.contains('/'))
                        {
                            merged.insert(name.to_string(), (name.to_string(), 0, true));
                        }
                    }
                }
            }
        }
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
        (ram_directory || !merged.is_empty()).then(|| merged.into_values().collect())
    }

    /// Create a guest directory through the writable mount that owns it.
    fn create_dir_raw(&self, guest_path: &str) -> bool {
        if self.is_ram_path(guest_path) {
            let Some(key) = self.ram_key(guest_path) else {
                return false;
            };
            let ram = self.ram.as_ref().unwrap();
            let Ok(mut store) = ram.files.lock() else {
                return false;
            };
            if key.is_empty() {
                return true;
            }
            if store.files.contains_key(&key) {
                return false;
            }
            if store.dirs.contains_key(&key) {
                return true;
            }
            let parent = key.rsplit_once('/').map(|p| p.0).unwrap_or("");
            if !parent.is_empty() && !store.dirs.contains_key(parent) {
                return false;
            }
            let Some(charge) = ram.store_charge(crate::memory_division::pages(
                (key.encode_utf16().count() as u64 + 1) * 2,
            )) else {
                return false;
            };
            store.dirs.insert(key, charge);
            return true;
        }
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
        std::fs::create_dir(path).is_ok()
    }

    /// Remove a guest file from the writable mount that owns it.
    fn delete_file_raw(&self, guest_path: &str) -> bool {
        if self.is_ram_path(guest_path) {
            let Some(key) = self.ram_key(guest_path) else {
                return false;
            };
            let ram = self.ram.as_ref().unwrap();
            let Ok(mut store) = ram.files.lock() else {
                return false;
            };
            return store.files.remove(&key).is_some();
        }
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

    pub fn remove_ram_dir(&self, guest_path: &str) -> bool {
        if !self.is_ram_path(guest_path) {
            return false;
        }
        let Some(key) = self.ram_key(guest_path) else {
            return false;
        };
        if key.is_empty() {
            return false;
        }
        let Some(ram) = &self.ram else {
            return false;
        };
        let Ok(mut store) = ram.files.lock() else {
            return false;
        };
        let prefix = format!("{key}/");
        if store.files.keys().any(|p| p.starts_with(&prefix))
            || store.dirs.keys().any(|p| p.starts_with(&prefix))
        {
            return false;
        }
        store.dirs.remove(&key).is_some()
    }

    /// Rename a guest file within the writable mount that owns it.
    fn move_file_raw(&self, from: &str, to: &str) -> bool {
        if self.is_ram_path(from) {
            if !self.is_ram_path(to) {
                return false;
            }
            let (Some(from), Some(to)) = (self.ram_key(from), self.ram_key(to)) else {
                return false;
            };
            let ram = self.ram.as_ref().unwrap();
            let Ok(mut store) = ram.files.lock() else {
                return false;
            };
            if store.files.contains_key(&to) || store.dirs.contains_key(&to) {
                return false;
            }
            let parent = to.rsplit_once('/').map(|p| p.0).unwrap_or("");
            if !parent.is_empty() && !store.dirs.contains_key(parent) {
                return false;
            }
            let Some(file) = store.files.get(&from).cloned() else {
                return false;
            };
            {
                let Ok(mut file) = file.lock() else {
                    return false;
                };
                let name_bytes = (to.encode_utf16().count() as u64 + 1) * 2;
                let pages = crate::memory_division::pages(file.data.len() as u64 + name_bytes);
                if !file.charge.resize(pages) {
                    return false;
                }
                file.name_bytes = name_bytes;
            }
            store.files.remove(&from);
            store.files.insert(to, file);
            return true;
        }
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
        if destination.exists() {
            return false;
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
    fn open_legacy(&mut self, guest_path: &str, access: Access, create: bool) -> Option<u32> {
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
                Arc::new(Mutex::new(Mp3DecoderState {
                    stream_key: NEXT_MAS_STREAM.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    decoder: RawDecoder::new(),
                    callback_event: 0,
                    pending: VecDeque::new(),
                    completed_samples: 0,
                    completed_bytes: 0,
                    played_bytes: 0,
                    bytes_seen: 0,
                    encoded: Vec::new(),
                    decoded_offset: 0,
                    pcm: Vec::new(),
                    sample_rate: 0,
                    channels: 0,
                    started: false,
                    paused: false,
                    volume: 0xFFFF_FFFF,
                })),
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
        if self.is_ram_path(guest_path) {
            return self.open_ram(guest_path, access, create);
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
            Arc::new(Mutex::new(OpenFile {
                host_path,
                access,
                file,
                text_mode: false,
                lease: None,
                append: false,
            })),
        );
        Some(h)
    }

    pub fn open_for_mapping(
        &mut self,
        path: &str,
        access: u32,
        share: u32,
        disposition: u32,
        append: bool,
        attributes: u32,
    ) -> Result<OpenResult, u32> {
        let opened = self.open_file(path, access, share, disposition, append, attributes)?;
        if !self.handles.contains_key(&opened.handle) {
            self.close(opened.handle);
            return Err(87);
        }
        self.mapping_file_handles.insert(opened.handle);
        Ok(opened)
    }

    pub fn discard_file_mapping_source(&mut self, file_handle: u32) {
        if self.mapping_file_handles.remove(&file_handle) {
            self.handles.remove(&file_handle);
        }
    }

    pub fn create_file_mapping(
        &mut self,
        file_handle: u32,
        max_size: u64,
        writable: bool,
    ) -> Option<u32> {
        let source_file_handle = self
            .mapping_file_handles
            .remove(&file_handle)
            .then_some(file_handle);
        let backing = (|| {
            if file_handle == INVALID_HANDLE_VALUE {
                if max_size == 0 || max_size > MAX_FILE_MAPPING_BYTES {
                    return None;
                }
                let len = usize::try_from(max_size).ok()?;
                let mut bytes = Vec::new();
                bytes.try_reserve_exact(len).ok()?;
                bytes.resize(len, 0);
                return Some((None, Some(bytes), max_size));
            }

            let open = self.handles.get(&file_handle)?.lock().ok()?;
            let can_read = matches!(open.access, Access::Read | Access::ReadWrite);
            let can_write = matches!(open.access, Access::Write | Access::ReadWrite);
            if !can_read || (writable && !can_write) {
                return None;
            }
            let file_size = open.file.metadata().ok()?.len();
            let size = if max_size == 0 { file_size } else { max_size };
            if size == 0 || size > MAX_FILE_MAPPING_BYTES {
                return None;
            }
            if size > file_size {
                if !writable {
                    return None;
                }
                open.file.set_len(size).ok()?;
            }
            Some((Some(open.host_path.clone()), None, size))
        })();

        let Some((host_path, anonymous_bytes, size)) = backing else {
            if let Some(handle) = source_file_handle {
                self.handles.remove(&handle);
            }
            return None;
        };

        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        self.file_mappings.insert(
            handle,
            FileMapping {
                host_path,
                anonymous_bytes,
                size,
                writable,
                source_file_handle,
                open_views: 0,
                closed: false,
            },
        );
        if let Some(file_handle) = source_file_handle {
            self.handles.remove(&file_handle);
        }
        Some(handle)
    }

    pub fn file_mapping_size(&self, handle: u32) -> Option<u64> {
        let mapping = self.file_mappings.get(&handle)?;
        (!mapping.closed).then_some(mapping.size)
    }

    pub fn read_file_mapping(
        &self,
        handle: u32,
        offset: u64,
        len: u32,
        write_access: bool,
    ) -> Option<Vec<u8>> {
        let mapping = self.file_mappings.get(&handle)?;
        if mapping.closed || (write_access && !mapping.writable) {
            return None;
        }
        if offset.checked_add(u64::from(len))? > mapping.size {
            return None;
        }
        let len = usize::try_from(len).ok()?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).ok()?;
        bytes.resize(len, 0);
        if let Some(anonymous) = &mapping.anonymous_bytes {
            let start = usize::try_from(offset).ok()?;
            let end = start.checked_add(len)?;
            bytes.copy_from_slice(anonymous.get(start..end)?);
        } else {
            let mut file = File::open(mapping.host_path.as_ref()?).ok()?;
            file.seek(SeekFrom::Start(offset)).ok()?;
            file.read_exact(&mut bytes).ok()?;
        }
        Some(bytes)
    }

    pub fn write_file_mapping(&mut self, handle: u32, offset: u64, data: &[u8]) -> bool {
        let path = {
            let Some(mapping) = self.file_mappings.get_mut(&handle) else {
                return false;
            };
            if !mapping.writable || (mapping.closed && mapping.open_views == 0) {
                return false;
            }
            let Some(end) = offset.checked_add(data.len() as u64) else {
                return false;
            };
            if end > mapping.size {
                return false;
            }
            if let Some(anonymous) = &mut mapping.anonymous_bytes {
                let Ok(start) = usize::try_from(offset) else {
                    return false;
                };
                let Some(end) = start.checked_add(data.len()) else {
                    return false;
                };
                let Some(destination) = anonymous.get_mut(start..end) else {
                    return false;
                };
                destination.copy_from_slice(data);
                return true;
            }
            mapping.host_path.clone()
        };
        let Some(path) = path else {
            return false;
        };
        let Ok(mut file) = OpenOptions::new().write(true).open(path) else {
            return false;
        };
        file.seek(SeekFrom::Start(offset)).is_ok() && file.write_all(data).is_ok()
    }

    pub fn add_file_mapping_view(&mut self, handle: u32) -> bool {
        let Some(mapping) = self.file_mappings.get_mut(&handle) else {
            return false;
        };
        if mapping.closed {
            return false;
        }
        mapping.open_views = mapping.open_views.saturating_add(1);
        true
    }

    pub fn release_file_mapping_view(&mut self, handle: u32) -> bool {
        let remove = {
            let Some(mapping) = self.file_mappings.get_mut(&handle) else {
                return false;
            };
            if mapping.open_views == 0 {
                return false;
            }
            mapping.open_views -= 1;
            mapping.closed && mapping.open_views == 0
        };
        if remove {
            self.remove_file_mapping(handle);
        }
        true
    }

    fn remove_file_mapping(&mut self, handle: u32) {
        if let Some(mapping) = self.file_mappings.remove(&handle) {
            if let Some(file_handle) = mapping.source_file_handle {
                self.mapping_file_handles.remove(&file_handle);
                self.handles.remove(&file_handle);
            }
        }
    }

    pub fn read(&mut self, handle: u32, buf: &mut [u8]) -> Option<usize> {
        if let Some(open) = self.ram_handles.get(&handle) {
            let mut open = open.lock().ok()?;
            if open.access == Access::Write
                || open.lease.as_ref().is_some_and(|l| l.access & 1 == 0)
            {
                return None;
            }
            let n = {
                let file = open.file.lock().ok()?;
                let start = usize::try_from(open.position).ok()?.min(file.data.len());
                let n = buf.len().min(file.data.len() - start);
                buf[..n].copy_from_slice(&file.data[start..start + n]);
                n
            };
            open.position += n as u64;
            return Some(n);
        }
        if self.registration.contains(&handle) {
            buf.fill(0);
            return Some(0);
        }
        let mut of = self.handles.get(&handle)?.lock().ok()?;
        if of.lease.as_ref().is_some_and(|l| l.access & 1 == 0) {
            return None;
        }
        of.file.read(buf).ok()
    }

    fn write_raw(&mut self, handle: u32, buf: &[u8]) -> Option<usize> {
        if let Some(open) = self.ram_handles.get(&handle) {
            let mut open = open.lock().ok()?;
            if open.access == Access::Read {
                return None;
            }
            if buf.is_empty() {
                return Some(0);
            }
            if open.append {
                let end = open.file.lock().ok()?.data.len() as u64;
                open.position = end;
            }
            let start = usize::try_from(open.position).ok()?;
            let end = start.checked_add(buf.len())?;
            {
                let mut file = open.file.lock().ok()?;
                if end > file.data.len() && !file.set_len(end) {
                    return None;
                }
                file.data[start..end].copy_from_slice(buf);
            }
            open.position = end as u64;
            return Some(buf.len());
        }
        if self.registration.contains(&handle) {
            return Some(buf.len());
        }
        let mut of = self.handles.get(&handle)?.lock().ok()?;
        if of.append {
            of.file.seek(SeekFrom::End(0)).ok()?;
        }
        of.file.write(buf).ok()
    }

    pub fn size(&mut self, handle: u32) -> Option<u64> {
        if let Some(open) = self.ram_handles.get(&handle) {
            return Some(open.lock().ok()?.file.lock().ok()?.data.len() as u64);
        }
        let of = self.handles.get(&handle)?.lock().ok()?;
        of.file.metadata().ok().map(|m| m.len())
    }

    pub fn seek(&mut self, handle: u32, offset: i64, whence: SeekKind) -> Option<u64> {
        if let Some(open) = self.ram_handles.get(&handle) {
            let mut open = open.lock().ok()?;
            let base = match whence {
                SeekKind::Begin => 0,
                SeekKind::Current => open.position as i128,
                SeekKind::End => open.file.lock().ok()?.data.len() as i128,
            };
            let position = base + offset as i128;
            if !(0..=u64::MAX as i128).contains(&position) {
                return None;
            }
            open.position = position as u64;
            return Some(open.position);
        }
        let mut of = self.handles.get(&handle)?.lock().ok()?;
        let from = match whence {
            SeekKind::Begin => SeekFrom::Start(u64::try_from(offset).ok()?),
            SeekKind::Current => SeekFrom::Current(offset),
            SeekKind::End => SeekFrom::End(offset),
        };
        of.file.seek(from).ok()
    }

    pub fn flush(&mut self, handle: u32) -> std::io::Result<()> {
        if self.ram_handles.contains_key(&handle) {
            return Ok(());
        }
        let file = self.handles.get(&handle).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid handle")
        })?;
        file.lock()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "file lock poisoned"))?
            .file
            .flush()
    }

    /// An opaque shared open description, retaining seek/access/device state.
    pub fn export_handle(&self, handle: u32) -> Option<VfsObject> {
        if let Some(driver) = self.bluetooth_driver_handles.get(&handle) {
            return Some(VfsObject(
                VfsObjectInner::BluetoothDriver(driver.clone()),
                None,
            ));
        }
        if let Some(v) = self.vibration_handles.get(&handle) {
            return Some(VfsObject(VfsObjectInner::Vibration(v.clone()), None));
        }
        if let Some(gps) = self.gps_handles.get(&handle) {
            return Some(VfsObject(VfsObjectInner::Gps(gps.clone()), None));
        }
        if let Some(camera) = self.camera_handles.get(&handle) {
            return Some(VfsObject(VfsObjectInner::Camera(camera.clone()), None));
        }
        if let Some(port) = self.bluetooth_handles.get(&handle) {
            return Some(VfsObject(VfsObjectInner::Bluetooth(port.clone()), None));
        }
        if let Some(file) = self.ram_handles.get(&handle) {
            return Some(VfsObject(VfsObjectInner::Ram(file.clone()), None));
        }
        if let Some(file) = self.handles.get(&handle) {
            return Some(VfsObject(VfsObjectInner::File(file.clone()), None));
        }
        if let Some(volume) = self.volumes.get(&handle) {
            return Some(VfsObject(VfsObjectInner::Volume(volume.clone()), None));
        }
        if let Some(decoder) = self.decoders.get(&handle) {
            return Some(VfsObject(VfsObjectInner::Decoder(decoder.clone()), None));
        }
        self.registration
            .contains(&handle)
            .then_some(VfsObject(VfsObjectInner::Registration, None))
    }
    pub fn import_handle(&mut self, handle: u32, object: VfsObject) -> bool {
        if self.is_handle(handle) {
            return false;
        }
        match object.0 {
            VfsObjectInner::BluetoothDriver(driver) => {
                self.bluetooth_driver_handles.insert(handle, driver);
            }
            VfsObjectInner::Vibration(v) => {
                self.vibration_handles.insert(handle, v);
            }
            VfsObjectInner::Gps(gps) => {
                self.gps_handles.insert(handle, gps);
            }
            VfsObjectInner::Camera(camera) => {
                self.camera_handles.insert(handle, camera);
            }
            VfsObjectInner::Bluetooth(port) => {
                self.bluetooth_handles.insert(handle, port);
            }
            VfsObjectInner::Ram(file) => {
                self.ram_handles.insert(handle, file);
            }
            VfsObjectInner::File(file) => {
                self.handles.insert(handle, file);
            }
            VfsObjectInner::Volume(volume) => {
                self.volumes.insert(handle, volume);
            }
            VfsObjectInner::Decoder(decoder) => {
                self.decoders.insert(handle, decoder);
            }
            VfsObjectInner::Registration => {
                self.registration.insert(handle);
            }
        }
        true
    }
    pub fn is_handle(&self, handle: u32) -> bool {
        self.vibration_handles.contains_key(&handle)
            || self.gps_handles.contains_key(&handle)
            || self.camera_handles.contains_key(&handle)
            || self.bluetooth_handles.contains_key(&handle)
            || self.bluetooth_driver_handles.contains_key(&handle)
            || self.is_open(handle)
            || self.file_mappings.contains_key(&handle)
            || self.volumes.contains_key(&handle)
            || self.decoders.contains_key(&handle)
            || self.registration.contains(&handle)
    }
    pub fn vibration_open(&self, handle: u32) -> Option<Arc<VibrationOpen>> {
        self.vibration_handles.get(&handle).cloned()
    }
    pub fn gps_open(&self, handle: u32) -> Option<Arc<GpsOpen>> {
        self.gps_handles.get(&handle).cloned()
    }
    pub fn camera_open(&self, handle: u32) -> Option<Arc<CameraOpen>> {
        self.camera_handles.get(&handle).cloned()
    }
    pub fn bluetooth_driver_open(&self, handle: u32) -> Option<Arc<BluetoothDriverOpen>> {
        self.bluetooth_driver_handles.get(&handle).cloned()
    }
    pub fn bluetooth_open(&self, handle: u32) -> Option<Arc<BluetoothOpen>> {
        self.bluetooth_handles.get(&handle).cloned()
    }
    pub fn set_bluetooth_service(&mut self, service: crate::bluetooth::Service) {
        self.set_shared_bluetooth_service(service.clone());
        self.bluetooth.service = service;
    }
    pub fn open_handles(&self) -> Vec<u32> {
        self.handles
            .keys()
            .chain(self.vibration_handles.keys())
            .chain(self.gps_handles.keys())
            .chain(self.camera_handles.keys())
            .chain(self.bluetooth_handles.keys())
            .chain(self.bluetooth_driver_handles.keys())
            .chain(self.ram_handles.keys())
            .chain(self.volumes.keys())
            .chain(self.decoders.keys())
            .chain(self.registration.iter())
            .copied()
            .collect()
    }
    pub fn duplicate_file(&mut self, source: u32, target: u32) -> bool {
        let Some(object) = self.export_handle(source) else {
            return false;
        };
        self.import_handle(target, object)
    }

    pub fn close(&mut self, handle: u32) -> bool {
        if self.file_mappings.contains_key(&handle) {
            let remove = {
                let mapping = self.file_mappings.get_mut(&handle).unwrap();
                if mapping.closed {
                    return false;
                }
                mapping.closed = true;
                mapping.open_views == 0
            };
            if remove {
                self.remove_file_mapping(handle);
            }
            return true;
        }
        self.mapping_file_handles.remove(&handle);
        self.vibration_handles.remove(&handle).is_some()
            || self.gps_handles.remove(&handle).is_some()
            || self.camera_handles.remove(&handle).is_some()
            || self.bluetooth_handles.remove(&handle).is_some()
            || self.bluetooth_driver_handles.remove(&handle).is_some()
            || self.ram_handles.remove(&handle).is_some()
            || self.handles.remove(&handle).is_some()
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
            if let Ok(mut file) = of.lock() {
                let _ = file.file.flush();
            }
        }
        let n = self.handles.len() + self.ram_handles.len() + self.file_mappings.len();
        self.ram_handles.clear();
        self.handles.clear();
        self.file_mappings.clear();
        self.mapping_file_handles.clear();
        self.volumes.clear();
        self.decoders.clear();
        self.registration.clear();
        self.bluetooth_handles.clear();
        self.bluetooth_driver_handles.clear();
        self.camera_handles.clear();
        self.gps_handles.clear();
        self.vibration_handles.clear();
        self.camera_deadlines.clear();
        self.bluetooth.lookups.clear();
        self.bluetooth.writes.clear();
        self.bluetooth.sockets.clear();
        self.bluetooth.socket_deadlines.clear();
        self.bluetooth.startups = 0;
        n
    }

    /// Mark a handle as a CRT stream opened in text mode. The stdio
    /// readers use this to apply CRLF -> LF translation the way the real
    /// CRT does; see `crt_fgetws` in `pocket-winceapi`.
    pub fn mark_text_mode(&mut self, handle: u32) {
        if let Some(of) = self.ram_handles.get(&handle) {
            if let Ok(mut file) = of.lock() {
                file.text_mode = true;
            }
        }
        if let Some(of) = self.handles.get(&handle) {
            if let Ok(mut file) = of.lock() {
                file.text_mode = true;
            }
        }
    }

    /// Whether a handle was opened by `fopen`/`_wfopen` in text mode
    /// (no `b` in the mode string). Non-stdio handles are binary.
    pub fn is_text_mode(&self, handle: u32) -> bool {
        self.ram_handles
            .get(&handle)
            .is_some_and(|of| of.lock().is_ok_and(|file| file.text_mode))
            || self
                .handles
                .get(&handle)
                .is_some_and(|of| of.lock().is_ok_and(|file| file.text_mode))
    }

    pub fn is_open(&self, handle: u32) -> bool {
        self.ram_handles.contains_key(&handle) || self.handles.contains_key(&handle)
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
        let decoder = self.decoders.get(&handle)?;
        let mut guard = decoder.lock().ok()?;
        let state = &mut *guard;
        state.bytes_seen = state.bytes_seen.saturating_add(data.len() as u64);
        state.encoded.extend_from_slice(data);
        // Preserve the decoder reservoir/filter history and incomplete frame tail
        // across 32 KB writes. Recreating DecoderOwned per chunk loses both.
        let mut scratch = [0i16; MAX_SAMPLES_PER_FRAME];
        let mut decoded_any = false;
        loop {
            let tail = &state.encoded[state.decoded_offset..];
            if tail.len() < 4 {
                break;
            }
            if tail.starts_with(b"ID3") {
                if tail.len() < 10 {
                    break;
                }
                let size = tail[6..10]
                    .iter()
                    .fold(0usize, |n, &b| (n << 7) | (b & 0x7f) as usize);
                let total = 10
                    + size
                    + if tail[3] == 4 && tail[5] & 0x10 != 0 {
                        10
                    } else {
                        0
                    };
                if tail.len() < total {
                    break;
                }
                state.decoded_offset += total;
                continue;
            }
            let Some(frame_bytes) = mp3_frame_bytes(tail) else {
                state.decoded_offset += 1;
                continue;
            };
            if tail.len() < frame_bytes {
                break;
            }
            if let Some((frame, _)) = state.decoder.next(&tail[..frame_bytes], &mut scratch) {
                if let Frame::Audio(audio) = frame {
                    if state.sample_rate == 0 {
                        state.sample_rate = audio.sample_rate();
                        state.channels = audio.channels();
                    }
                    state.pcm.extend_from_slice(audio.samples());
                    decoded_any = true;
                }
            }
            state.decoded_offset += frame_bytes;
        }
        if state.decoded_offset > 0 {
            let consumed = state.decoded_offset;
            state.encoded.drain(..consumed);
            state.decoded_offset = 0;
        }
        state.started |= decoded_any;
        Some(state.bytes_seen)
    }

    pub fn feed_mp3_decoder(&mut self, handle: u32, len: u64) -> Option<u64> {
        let decoder = self.decoders.get(&handle)?;
        let mut guard = decoder.lock().ok()?;
        let state = &mut *guard;
        state.bytes_seen = state.bytes_seen.saturating_add(len);
        Some(state.bytes_seen)
    }

    pub fn mp3_decoder_handles(&self) -> Vec<u32> {
        let mut seen = std::collections::HashSet::new();
        self.decoders
            .iter()
            .filter_map(|(&handle, state)| {
                seen.insert(state.lock().unwrap().stream_key)
                    .then_some(handle)
            })
            .collect()
    }
    pub fn mp3_stream_key(&self, handle: u32) -> u32 {
        self.decoders
            .get(&handle)
            .map(|v| v.lock().unwrap().stream_key)
            .unwrap_or(handle)
    }

    pub fn mp3_callback_event(&self, handle: u32) -> u32 {
        self.decoders
            .get(&handle)
            .map(|v| v.lock().unwrap().callback_event)
            .unwrap_or(0)
    }

    pub fn start_mp3_decoder(&mut self, handle: u32, callback_event: u32) {
        self.stop_mp3_decoder(handle);
        if let Some(decoder) = self.decoders.get(&handle) {
            let mut guard = decoder.lock().unwrap();
            let v = &mut *guard;
            v.callback_event = callback_event;
            v.started = true;
        }
    }

    pub fn queue_mp3_buffer(&mut self, handle: u32, end_samples: u64) {
        if let Some(decoder) = self.decoders.get(&handle) {
            let mut guard = decoder.lock().unwrap();
            let v = &mut *guard;
            let bytes_seen = v.bytes_seen;
            v.pending.push_back((end_samples, bytes_seen));
        }
    }

    /// Driver buffer swaps are driven by consumed PCM, not submission time.
    pub fn service_mp3_decoder(&mut self, handle: u32, cursor: u64) -> Option<(u32, usize)> {
        let decoder = self.decoders.get(&handle)?;
        let mut guard = decoder.lock().ok()?;
        let v = &mut *guard;
        if !v.started || v.paused {
            return None;
        }
        let mut completed = 0;
        while let Some(&(end_samples, end_bytes)) = v.pending.front() {
            if cursor < end_samples {
                break;
            }
            v.pending.pop_front();
            v.completed_samples = end_samples;
            v.completed_bytes = end_bytes;
            completed += 1;
        }
        v.played_bytes = v.completed_bytes;
        if let Some(&(end_samples, end_bytes)) = v.pending.front() {
            let length = end_samples.saturating_sub(v.completed_samples);
            if length > 0 {
                v.played_bytes += (end_bytes - v.completed_bytes)
                    .saturating_mul(cursor.saturating_sub(v.completed_samples).min(length))
                    / length;
            }
        }
        if completed > 0 {
            Some((v.callback_event, completed))
        } else {
            None
        }
    }

    pub fn stop_mp3_decoder(&mut self, handle: u32) {
        if let Some(decoder) = self.decoders.get(&handle) {
            let mut guard = decoder.lock().unwrap();
            let v = &mut *guard;
            v.decoder = RawDecoder::new();
            v.encoded.clear();
            v.decoded_offset = 0;
            v.bytes_seen = 0;
            v.sample_rate = 0;
            v.channels = 0;
            v.started = false;
            v.paused = false;
            v.pcm.clear();
            v.pending.clear();
            v.completed_samples = 0;
            v.completed_bytes = 0;
            v.played_bytes = 0;
        }
    }

    pub fn mp3_decoder_paused(&self, handle: u32) -> bool {
        self.decoders
            .get(&handle)
            .map(|v| v.lock().unwrap().paused)
            .unwrap_or(false)
    }

    pub fn pause_mp3_decoder(&mut self, handle: u32, paused: bool) {
        if let Some(decoder) = self.decoders.get(&handle) {
            let mut guard = decoder.lock().unwrap();
            let state = &mut *guard;
            state.paused = paused;
        }
    }

    pub fn set_mp3_decoder_volume(&mut self, handle: u32, volume: u32) {
        if let Some(decoder) = self.decoders.get(&handle) {
            let mut guard = decoder.lock().unwrap();
            let state = &mut *guard;
            state.volume = volume;
        }
    }

    pub fn mp3_decoder_reply(&self, handle: u32, code: u32, len: usize) -> Vec<u8> {
        let Some(decoder) = self.decoders.get(&handle) else {
            return vec![0; len];
        };
        let state = decoder.lock().unwrap();
        let value = if code == 0x001d_1030 {
            1
        } else if code == 0x001d_1010 {
            if state.paused {
                5
            } else if state.started && state.pending.is_empty() {
                7 // MASG_DONE, per OEMINC.H
            } else if state.started {
                4
            } else {
                0
            }
        } else if code == 0x001d_1018 {
            state.played_bytes.min(u32::MAX as u64) as u32
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
        let decoder = self.decoders.get(&handle)?;
        let mut guard = decoder.lock().ok()?;
        let state = &mut *guard;
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
    fn colors_language_redirects_reads_and_preserves_original_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bstrings.eng"), b"english").unwrap();
        for (language, extension) in [
            ("french", "fre"),
            ("german", "ger"),
            ("italian", "ita"),
            ("spanish", "spa"),
        ] {
            std::fs::write(dir.path().join(format!("BSTRINGS.{extension}")), language).unwrap();
        }
        let mut vfs = Vfs::new();
        vfs.mount("/card/", dir.path());
        vfs.set_default_dir("/card/");
        for language in [
            "french", "german", "italian", "spanish", "english", "unknown",
        ] {
            vfs.set_colors_language(language);
            let h = vfs.open("BSTRINGS.ENG", Access::Read, false).unwrap();
            let expected = if language == "unknown" {
                "english"
            } else {
                language
            };
            assert_eq!(vfs.size(h), Some(expected.len() as u64));
            let mut buffer = vec![0; expected.len()];
            assert_eq!(vfs.read(h, &mut buffer), Some(expected.len()));
            assert_eq!(buffer, expected.as_bytes());
            assert!(vfs.close(h));
        }
        vfs.set_colors_language("french");
        let h = vfs.open("bstrings.eng", Access::Write, false).unwrap();
        assert_eq!(vfs.write(h, b"ENGLISH"), Some(7));
        assert!(vfs.close(h));
        assert_eq!(
            std::fs::read(dir.path().join("bstrings.eng")).unwrap(),
            b"ENGLISH"
        );
        assert_eq!(
            std::fs::read(dir.path().join("BSTRINGS.fre")).unwrap(),
            b"french"
        );
    }

    #[test]
    fn colors_language_missing_translation_falls_back_without_crossing_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("other")).unwrap();
        let english = dir.path().join("bstrings.eng");
        std::fs::write(&english, b"english").unwrap();
        std::fs::write(dir.path().join("other/bstrings.fre"), b"other").unwrap();
        let mut vfs = Vfs::new();
        vfs.mount_read_only("/card/", dir.path());
        vfs.set_colors_language("french");
        assert_eq!(vfs.resolve("/card/bstrings.eng"), Some(english.clone()));
        let h = vfs.open("/card/bstrings.eng", Access::Read, false).unwrap();
        let mut buffer = [0; 7];
        assert_eq!(vfs.read(h, &mut buffer), Some(7));
        assert_eq!(&buffer, b"english");
        assert!(vfs.close(h));
        // Installing a translation becomes visible without changing the card's English file.
        let french = dir.path().join("bstrings.fre");
        std::fs::write(&french, b"french").unwrap();
        assert_eq!(vfs.resolve("/card/bstrings.eng"), Some(french));
        let mut other_game = Vfs::new();
        other_game.mount_read_only("/card/", dir.path());
        other_game.attach_shared_context(vfs.shared_context());
        assert_eq!(other_game.resolve("/card/bstrings.eng"), Some(english));
    }

    #[cfg(unix)]
    #[test]
    fn colors_language_ambiguous_case_falls_back_to_english() {
        let dir = tempfile::tempdir().unwrap();
        let english = dir.path().join("bstrings.eng");
        std::fs::write(&english, b"english").unwrap();
        std::fs::write(dir.path().join("bstrings.fre"), b"first").unwrap();
        std::fs::write(dir.path().join("BSTRINGS.FRE"), b"second").unwrap();
        let mut vfs = Vfs::new();
        vfs.mount_read_only("/card/", dir.path());
        vfs.set_colors_language("french");
        assert_eq!(vfs.resolve("/card/bstrings.eng"), Some(english));
    }

    #[test]
    fn memory_division_ram_files_share_capacity_and_preserve_failed_writes() {
        let ram = crate::memory_division::MemoryDivision::new(64 * 4096, 8).unwrap();
        let mut parent = Vfs::new();
        parent.attach_ram(Some(ram.clone()));
        assert!(parent.create_dir("/temp"));
        let h = parent.open("/temp/test", Access::ReadWrite, true).unwrap();
        assert_eq!(parent.write(h, b"hello"), Some(5));
        assert!(parent.duplicate_file(h, 0xd2000000));
        assert_eq!(parent.seek(0xd2000000, 0, SeekKind::Begin), Some(0));
        let mut child = Vfs::new();
        child.attach_ram(Some(ram.clone()));
        child.attach_shared_context(parent.shared_context());
        let read = child.open("/temp/test", Access::Read, false).unwrap();
        let mut bytes = [0; 5];
        assert_eq!(child.read(read, &mut bytes), Some(5));
        assert_eq!(&bytes, b"hello");
        let used = ram.snapshot().store_used;
        assert_eq!(parent.write(h, &vec![7; 8 * 4096]), None);
        assert_eq!(ram.snapshot().store_used, used);
        assert_eq!(parent.size(h), Some(5));
        ram.resize(12).unwrap();
        assert_eq!(parent.write(h, &vec![7; 8 * 4096]), Some(8 * 4096));
        assert_eq!(child.size(read), Some(8 * 4096));
        assert!(!parent.delete_file("/temp/test"));
        assert!(ram.resize(8).is_err()); // open descriptions keep data alive
        assert!(parent.close(h));
        assert!(parent.close(0xd2000000));
        assert!(child.close(read));
        assert!(parent.delete_file("/temp/test"));
        assert!(parent.ram_file_size("/temp/test").is_none());
        ram.resize(8).unwrap();
        assert!(parent.remove_ram_dir("/temp"));
        assert_eq!(ram.snapshot().store_used, 0);
    }

    #[test]
    fn memory_division_flash_and_card_are_not_ram_store() {
        let ram = crate::memory_division::MemoryDivision::default();
        let dir = tempfile::tempdir().unwrap();
        let mut vfs = Vfs::new();
        vfs.attach_ram(Some(ram.clone()));
        vfs.mount_save_dir("/Flash Disk", dir.path());
        vfs.mount_read_only("/SD Card", dir.path());
        let h = vfs.open("/Flash Disk/save", Access::Write, true).unwrap();
        assert_eq!(vfs.write(h, &vec![0; 4096]), Some(4096));
        assert_eq!(ram.snapshot().store_used, 0);
        assert!(vfs.open("/SD Card/missing", Access::Write, true).is_none());
        assert!(vfs.open("/../escape", Access::Write, true).is_none());
        assert!(vfs
            .open("/missing/parent/file", Access::Write, true)
            .is_none());
    }

    #[test]
    fn vfs_missing_file_does_not_borrow_from_another_existing_directory() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "pockethle-vfs-missing-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("First")).unwrap();
        std::fs::create_dir_all(root.join("Second")).unwrap();
        let movie = root.join("Second/asset.bin");
        std::fs::write(&movie, b"other game video").unwrap();
        let mut v = Vfs::new();
        v.mount_read_only("\\SD Card\\", &root);
        assert!(v
            .open("\\SD Card\\First\\ASSET.BIN", Access::Read, false)
            .is_none());
        assert!(v
            .open("\\SD Card\\First\\First\\ASSET.BIN", Access::Read, false)
            .is_none());
        assert!(v
            .open("\\SD Card\\First\\missing\\ASSET.BIN", Access::Read, false)
            .is_none());
        assert_eq!(
            v.resolve("\\SD Card\\Second\\ASSET.BIN"),
            Some(movie.clone())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn vfs_read_only_fallback_index_keeps_exact_lookup_priority_and_writable_mounts_live() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "pockethle-vfs-index-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("wrapper/Assets")).unwrap();
        let asset = root.join("wrapper/Assets/item.bin");
        std::fs::write(&asset, b"asset").unwrap();
        let mut v = Vfs::new();
        v.mount_read_only("\\Card\\", &root);
        for _ in 0..2 {
            assert_eq!(v.resolve("\\Card\\Assets\\ITEM.BIN"), Some(asset.clone()));
        }
        assert!(v.mounts[0].fallback_files.borrow().is_some());
        // Exact paths on newly mounted overlays still beat a warm index.
        let overlay = root.join("overlay");
        std::fs::create_dir_all(&overlay).unwrap();
        let exact = overlay.join("item.bin");
        std::fs::write(&exact, b"override").unwrap();
        v.mount_save_dir("\\Card\\Assets\\", &overlay);
        assert_eq!(v.resolve("\\Card\\Assets\\item.bin"), Some(exact));
        let mut writable = Vfs::new();
        writable.mount("\\Card\\", &root);
        assert!(!writable
            .resolve("\\Card\\unknown\\new.bin")
            .unwrap()
            .exists());
        let added = root.join("wrapper/Assets/new.bin");
        std::fs::write(&added, b"new").unwrap();
        assert_eq!(writable.resolve("\\Card\\unknown\\new.bin"), Some(added));
        assert!(writable.mounts[0].fallback_files.borrow().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn vfs_path_fallback_preserves_directories_and_rejects_ambiguous_names() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "pockethle-vfs-suffix-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let correct = root.join("wrapper/Data/Fonts/font.cmf");
        let other = root.join("wrapper/Data/Frontend/Font/font.cmf");
        for (path, bytes) in [
            (&correct, b"game font".as_slice()),
            (&other, b"frontend font".as_slice()),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let mut v = Vfs::new();
        v.mount_read_only("\\SD Card\\", &root);
        let requested = "\\SD Card\\GZGA200035\\Data\\Fonts\\font.cmf";
        assert_eq!(v.resolve(requested), Some(correct.clone()));
        let handle = v.open(requested, Access::Read, false).unwrap();
        let mut bytes = [0; 9];
        assert_eq!(v.read(handle, &mut bytes), Some(9));
        assert_eq!(&bytes, b"game font");
        assert!(v
            .open("\\SD Card\\missing\\font.cmf", Access::Read, false)
            .is_none());
        // An exact path on a broader mount beats a basename fallback on
        // an overlay mounted more specifically.
        let exact = root.join("real/font.cmf");
        std::fs::create_dir_all(exact.parent().unwrap()).unwrap();
        std::fs::write(&exact, b"exact").unwrap();
        v.mount_read_only("\\SD Card\\real\\", root.join("wrapper/Data/Frontend/Font"));
        assert_eq!(v.resolve("\\SD Card\\real\\font.cmf"), Some(other));
        // A missing leaf on that specific overlay must not preempt the
        // existing, complete path on the broader card mount.
        let only = root.join("real/unique.bin");
        std::fs::write(&only, b"exact unique").unwrap();
        std::fs::write(
            root.join("wrapper/Data/Frontend/Font/unique.bin"),
            b"overlay exact",
        )
        .unwrap();
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let nested = empty.join("wrong/unique.bin");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        std::fs::write(&nested, b"wrong basename").unwrap();
        let mut v2 = Vfs::new();
        v2.mount_read_only("\\SD Card\\", &root);
        v2.mount_read_only("\\SD Card\\real\\", &empty);
        assert_eq!(v2.resolve("\\SD Card\\real\\unique.bin"), Some(only));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mas_buffer_swaps_follow_playback_and_report_sdk_states() {
        let mut v = Vfs::new();
        let h = v.open("MAS1:", Access::ReadWrite, false).unwrap();
        v.start_mp3_decoder(h, 123);
        let _ = v.feed_mp3_decoder(h, 32768);
        v.queue_mp3_buffer(h, 100);
        let _ = v.feed_mp3_decoder(h, 32768);
        v.queue_mp3_buffer(h, 200);
        assert_eq!(v.service_mp3_decoder(h, 50), None);
        assert_eq!(
            v.mp3_decoder_reply(h, 0x001d1018, 4),
            16384u32.to_le_bytes()
        );
        assert_eq!(v.service_mp3_decoder(h, 100), Some((123, 1)));
        assert_eq!(v.mp3_decoder_reply(h, 0x001d1010, 4), 4u32.to_le_bytes());
        v.pause_mp3_decoder(h, true);
        assert_eq!(v.service_mp3_decoder(h, 200), None);
        assert_eq!(v.mp3_decoder_reply(h, 0x001d1010, 4), 5u32.to_le_bytes());
        v.pause_mp3_decoder(h, false);
        assert_eq!(v.service_mp3_decoder(h, 200), Some((123, 1)));
        assert_eq!(v.service_mp3_decoder(h, 200), None);
        assert_eq!(v.mp3_decoder_reply(h, 0x001d1010, 4), 7u32.to_le_bytes());
        assert_eq!(
            v.mp3_decoder_reply(h, 0x001d1018, 4),
            65536u32.to_le_bytes()
        );
        v.stop_mp3_decoder(h);
        assert_eq!(v.mp3_decoder_reply(h, 0x001d1010, 4), 0u32.to_le_bytes());
        v.start_mp3_decoder(h, 456);
        assert_eq!(v.mp3_callback_event(h), 456);
        assert_eq!(v.mp3_decoder_reply(h, 0x001d1018, 4), 0u32.to_le_bytes());
    }

    #[test]
    fn mas_chunked_mp3_decode_matches_contiguous_decode() {
        // Generate complete silent MPEG-1 Layer III frames in memory.
        // Cross 32 KB and arbitrary split-frame boundaries without requiring
        // a binary fixture or an encoder on the developer's machine.
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xff, 0xfb, 0x90, 0]);
        let data = frame.repeat(128);
        fn decode(data: &[u8], chunk: usize) -> Vec<i16> {
            let mut v = Vfs::new();
            let h = v.open("MAS1:", Access::ReadWrite, false).unwrap();
            let mut samples = Vec::new();
            for bytes in data.chunks(chunk) {
                let _ = v.feed_mp3_decoder_data(h, bytes);
                if let Some((rate, channels, pcm)) = v.take_mp3_decoder_pcm(h) {
                    assert_eq!((rate, channels), (44100, 2));
                    samples.extend(pcm);
                }
            }
            samples
        }
        let expected = decode(&data, data.len());
        assert!(expected.len() > 44100 * 2);
        assert_eq!(decode(&data, 32768), expected);
        assert_eq!(decode(&data, 701), expected);
    }

    #[test]
    fn vfs_duplicate_file_shares_position_and_survives_independent_close() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("stream.bin"), b"abcdef").unwrap();
        let mut v = Vfs::new();
        v.mount("\\Data\\", dir.path());
        let original = v
            .open("\\Data\\stream.bin", Access::ReadWrite, false)
            .unwrap();
        let alias = 0xd2000000;
        assert!(v.duplicate_file(original, alias));
        let mut bytes = [0u8; 2];
        assert_eq!(v.read(original, &mut bytes), Some(2));
        assert_eq!(&bytes, b"ab");
        assert_eq!(v.read(alias, &mut bytes), Some(2));
        assert_eq!(&bytes, b"cd");
        v.mark_text_mode(original);
        assert!(v.is_text_mode(alias));
        assert!(v.close(original));
        assert!(!v.is_open(original));
        assert_eq!(v.read(alias, &mut bytes), Some(2));
        assert_eq!(&bytes, b"ef");
        assert_eq!(v.seek(alias, 0, SeekKind::Begin), Some(0));
        assert_eq!(v.write(alias, b"XY"), Some(2));
        assert!(v.close(alias));
        assert_eq!(
            std::fs::read(dir.path().join("stream.bin")).unwrap(),
            b"XYcdef"
        );
    }

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
    fn resolves_parent_segments_that_stay_inside_the_mount() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir
            .path()
            .join("resources")
            .join("scenes")
            .join("Level1")
            .join("flyable.properties");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"level=1").unwrap();
        let mut v = Vfs::new();
        v.mount(r"\Program Files\OmniGSoft\Chopper Fight 1.1\", dir.path());
        v.set_default_dir(r"\Program Files\OmniGSoft\Chopper Fight 1.1\bin");
        assert_eq!(
            v.resolve(r"..\resources\scenes\Level1\flyable.properties"),
            Some(file)
        );
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

    #[test]
    fn file_mapping_reads_and_writes_file_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"abcdef").unwrap();
        let mut v = Vfs::new();
        v.mount("\\App\\", dir.path());
        let file = v
            .open_for_mapping("\\App\\data.bin", 3, 3, 3, false, 0)
            .unwrap()
            .handle;
        let mapping = v.create_file_mapping(file, 0, true).unwrap();
        assert_eq!(v.file_mapping_size(mapping), Some(6));
        assert_eq!(
            v.read_file_mapping(mapping, 2, 3, false).as_deref(),
            Some(&b"cde"[..])
        );
        assert!(v.add_file_mapping_view(mapping));
        assert!(v.write_file_mapping(mapping, 1, b"XYZ"));
        assert_eq!(
            v.read_file_mapping(mapping, 0, 6, true).as_deref(),
            Some(&b"aXYZef"[..])
        );
        assert!(v.close(mapping));
        assert!(!v.is_open(file));
        assert!(v.release_file_mapping_view(mapping));
        assert_eq!(std::fs::read(path).unwrap(), b"aXYZef");
    }

    #[test]
    fn anonymous_file_mapping_is_zero_filled_and_bounded() {
        let mut v = Vfs::new();
        let mapping = v
            .create_file_mapping(INVALID_HANDLE_VALUE, 8, true)
            .unwrap();
        assert_eq!(v.read_file_mapping(mapping, 0, 8, true), Some(vec![0; 8]));
        assert!(v.write_file_mapping(mapping, 2, b"xy"));
        assert_eq!(
            v.read_file_mapping(mapping, 0, 8, false),
            Some(vec![0, 0, b'x', b'y', 0, 0, 0, 0])
        );
        assert!(v.close(mapping));
        assert!(v
            .create_file_mapping(INVALID_HANDLE_VALUE, MAX_FILE_MAPPING_BYTES + 1, true)
            .is_none());
    }

    #[test]
    fn readonly_file_mapping_rejects_write_access_and_out_of_bounds_views() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), b"abc").unwrap();
        let mut v = Vfs::new();
        v.mount_read_only("\\App\\", dir.path());
        let file = v
            .open_for_mapping("\\App\\data.bin", 1, 3, 3, false, 0)
            .unwrap()
            .handle;
        assert!(v.create_file_mapping(file, 0, true).is_none());
        assert!(!v.is_open(file));

        let file = v
            .open_for_mapping("\\App\\data.bin", 1, 3, 3, false, 0)
            .unwrap()
            .handle;
        let mapping = v.create_file_mapping(file, 0, false).unwrap();
        assert_eq!(
            v.read_file_mapping(mapping, 1, 2, false),
            Some(b"bc".to_vec())
        );
        assert!(v.read_file_mapping(mapping, 2, 2, false).is_none());
        assert!(!v.write_file_mapping(mapping, 0, b"x"));
        assert!(v.close(mapping));
    }

    #[test]
    fn read_only_file_mapping_cannot_modify_backing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"abc").unwrap();
        let mut v = Vfs::new();
        v.mount_read_only("\\App\\", dir.path());
        let file = v
            .open_for_mapping("\\App\\data.bin", 1, 3, 3, false, 0)
            .unwrap()
            .handle;
        let mapping = v.create_file_mapping(file, 0, false).unwrap();
        assert_eq!(
            v.read_file_mapping(mapping, 0, 3, false),
            Some(b"abc".to_vec())
        );
        assert!(!v.write_file_mapping(mapping, 0, b"xyz"));
        assert!(v.close(mapping));
        assert_eq!(std::fs::read(path).unwrap(), b"abc");
    }

    #[test]
    fn device_paths_do_not_fall_back_to_basename_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ACS1:"), b"not a device").unwrap();
        let mut v = Vfs::new();
        v.mount("\\Application\\", dir.path());

        assert!(v.resolve("\\Application\\missing\\ACS1:").is_none());
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

    #[test]
    fn flattened_toy_golf_marker_reports_the_original_card_serial() {
        let dir = tempfile::tempdir().unwrap();
        // Real Toy Golf marker supplied with the flattened installed game.
        std::fs::write(dir.path().join("GZGA200046"), [0x17, 0x0d, 0x8b, 0x6a]).unwrap();
        let mut v = Vfs::new();
        v.mount_read_only("\\SD Card\\", dir.path());
        v.mount_read_only("\\SD Card\\GZGA200046\\", dir.path());
        let h = v.open("\\SD Card\\Vol:", Access::Read, false).unwrap();
        let expected = 200046u32.wrapping_sub(0x6a8b0d17).wrapping_sub(1);
        assert_eq!(v.volume(h).unwrap().serial(), expected);
        let marker = v.resolve("\\SD Card\\GZGA200046\\GZGA200046").unwrap();
        assert_eq!(std::fs::read(marker).unwrap(), [0x17, 0x0d, 0x8b, 0x6a]);
    }

    #[test]
    fn volume_serial_decodes_gizmondo_markers_across_multiple_titles() {
        let dir = tempfile::tempdir().unwrap();
        let serial = 0xb5f1_0053u32;
        // The identifiers are synthetic; the encoding is the SDK format.
        for id in [100_001u32, 100_002] {
            let name = format!("GZGA{id:06}");
            let game = dir.path().join(&name);
            std::fs::create_dir(&game).unwrap();
            let encoded = id.wrapping_sub(serial).wrapping_sub(1);
            std::fs::write(game.join(&name), encoded.to_le_bytes()).unwrap();
            let card = tempfile::tempdir().unwrap();
            let copy = card.path().join(&name);
            std::fs::create_dir(&copy).unwrap();
            std::fs::write(copy.join(&name), encoded.to_le_bytes()).unwrap();
            let mut single = Vfs::new();
            single.mount_read_only("\\SD Card\\", card.path());
            let handle = single.open("\\SD Card\\Vol:", Access::Read, false).unwrap();
            assert_eq!(single.volume(handle).unwrap().serial(), serial);
        }
        // An unrelated four-byte same-named file is not volume metadata.
        let unrelated = dir.path().join("Assets");
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(unrelated.join("Assets"), 42u32.to_le_bytes()).unwrap();
        let malformed = dir.path().join("GZGAABCDEF");
        std::fs::create_dir(&malformed).unwrap();
        std::fs::write(malformed.join("GZGAABCDEF"), 42u32.to_le_bytes()).unwrap();
        let mut v = Vfs::new();
        v.mount_read_only("\\SD Card\\", dir.path());
        let h = v.open("\\SD Card\\Vol:", Access::Read, false).unwrap();
        assert_eq!(v.volume(h).unwrap().serial(), serial);
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
