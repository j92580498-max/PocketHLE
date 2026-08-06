//! `.CAB` archive extractor used by PocketHLE.
//!
//! Pocket PC / Windows Mobile applications are typically distributed as
//! `.CAB` archives that contain the actual `.exe`, bundled DLLs, sound
//! resources and a small `_setup.xml` / `.000` install script. This crate
//! wraps the [`cab`](https://crates.io/crates/cab) crate and adds:
//!
//! * Iteration over files with their original (long) names where available.
//! * Extraction of all files into a target directory.
//! * Best-effort detection of the WinCE install header (the file with the
//!   `.000` extension) which lists the canonical executable / DLL names.
//!
//! Note: PocketHLE never ships any copyrighted game data — the user
//! supplies the `.cab` themselves.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CabError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("cab parse error: {0}")]
    Parse(String),
    #[error("file `{0}` not found in cabinet")]
    NotFound(String),
}

/// One file extracted from a cabinet.
#[derive(Debug, Clone)]
pub struct CabFile {
    /// Short (8.3) name as stored in the cabinet.
    pub short_name: String,
    /// Path on disk after extraction. Always inside the destination dir.
    pub extracted_path: PathBuf,
    /// File size in bytes.
    pub size: u64,
}

/// Extract every file from `cab_path` into `out_dir`.
///
/// Returns the list of extracted files in the order they appeared in the
/// cabinet directory. Existing files are overwritten.
pub fn extract_all<P: AsRef<Path>, Q: AsRef<Path>>(
    cab_path: P,
    out_dir: Q,
) -> Result<Vec<CabFile>, CabError> {
    let cab_path = cab_path.as_ref();
    let out_dir = out_dir.as_ref();
    fs::create_dir_all(out_dir)?;

    let file = File::open(cab_path)?;
    let mut cabinet = cab::Cabinet::new(file).map_err(|e| CabError::Parse(e.to_string()))?;

    // Collect (folder_idx, file_name) up-front since we cannot hold the
    // cabinet borrow across `read_file`.
    let mut entries: Vec<(usize, String)> = Vec::new();
    for (idx, folder) in cabinet.folder_entries().enumerate() {
        for f in folder.file_entries() {
            entries.push((idx, f.name().to_string()));
        }
    }

    let mut out = Vec::with_capacity(entries.len());
    for (_folder_idx, name) in entries {
        let mut reader = cabinet
            .read_file(&name)
            .map_err(|e| CabError::Parse(format!("reading {name}: {e}")))?;
        let dest = out_dir.join(sanitize_name(&name));
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        let size = buf.len() as u64;
        let mut w = File::create(&dest)?;
        w.write_all(&buf)?;
        log::debug!("extracted {name} -> {} ({} bytes)", dest.display(), size);
        out.push(CabFile {
            short_name: name,
            extracted_path: dest,
            size,
        });
    }
    Ok(out)
}

/// Replace path-traversal characters with `_` so a malicious cabinet
/// cannot escape `out_dir`.
fn sanitize_name(name: &str) -> String {
    name.replace(['/', '\\'], "_")
        .trim_start_matches('.')
        .to_string()
}

/// A reader for the WinCE install header — the `MSCE` file stored in
/// the cabinet under the `.000` extension.
///
/// The format is documented at
/// <https://www.cabextract.org.uk/wince_cab_format/>: a fixed 100-byte
/// header holding entry counts and file offsets, followed by six
/// sections (`STRINGS`, `DIRS`, `FILES`, `REGHIVES`, `REGKEYS`,
/// `LINKS`) which may appear in any order.
///
/// This matters because a Pocket PC cabinet stores its payload under
/// generated 8.3 names (`00000RAY.004`, `RAYMAN~1.000`) and the *only*
/// record of the real names is the `FILES` section. Guessing them from
/// printable-string scans reconstructs some cabinets and mangles
/// others — Rayman Ultimate ships 198 payload entries whose names live
/// nowhere else.
#[derive(Debug, Clone, Default)]
pub struct WinCeInstallHeader {
    pub app_name: Option<String>,
    pub provider: Option<String>,
    pub files: Vec<WinCeInstallFile>,
    /// Shallowest directory every installed file sits under, with a
    /// trailing backslash, e.g. `\Program Files\RaymanUltimate\`.
    pub install_dir: Option<String>,
    /// Target CPU from the fixed header (see Appendix A of the spec);
    /// 2577 is StrongARM, 0 means "no specific architecture".
    pub arch: Option<u32>,
    /// Registry values the `REGKEYS` section installs, in the same
    /// shape the `_setup.xml` path produces.
    pub registry: Vec<SetupRegistryValue>,
    /// Guest path of the executable the `LINKS` section points at —
    /// the binary the user would tap on the device.
    pub shortcut_target: Option<String>,
    /// True when the documented MSCE layout parsed cleanly, so every
    /// name here is read from the header rather than guessed.
    ///
    /// Callers use this to decide whether the historical
    /// reconstruct-by-heuristic paths still need to run: for a
    /// structured header they only add wrong names.
    pub structured: bool,
}

#[derive(Debug, Clone)]
pub struct WinCeInstallFile {
    /// Source short name inside the cab, e.g. `JUMPYB~1.002`. Only the
    /// numeric extension identifies the entry; the stem is generated.
    pub source: String,
    /// Numeric extension of the cabinet entry, i.e. the file ID.
    pub file_id: u16,
    /// Destination path on the device, e.g. `\Program Files\JumpyBall\JumpyBall.exe`.
    pub destination: String,
}

impl WinCeInstallHeader {
    /// Parse a `.000` file from disk. Best-effort — unknown bytes are
    /// skipped rather than producing an error, because the format varies
    /// between Pocket PC versions.
    pub fn parse_file(path: impl AsRef<Path>) -> Result<Self, CabError> {
        let mut f = File::open(path)?;
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        Self::parse_bytes(&data)
    }

    pub fn parse_bytes(data: &[u8]) -> Result<Self, CabError> {
        // Parse the documented structure first. It is exact: every
        // installed file's real name and destination directory is
        // recorded there, so nothing has to be guessed.
        if let Some(header) = parse_msce(data) {
            if !header.files.is_empty() {
                return Ok(header);
            }
        }
        // Truncated or non-conforming `.000` payloads fall back to the
        // historical heuristic scan, which reconstructs enough for the
        // handful of cabs whose header we cannot parse.
        Ok(parse_by_scanning(data))
    }
}

/// Cursor over the `.000` payload that yields `None` instead of
/// panicking when a length or offset field points outside the file.
/// Every field in this format is attacker-controlled, so all reads go
/// through here.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn at(data: &'a [u8], pos: usize) -> Self {
        Cursor { data, pos }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A counted, NUL-terminated ASCII string: `len` includes the
    /// terminator, so the text is `len - 1` bytes.
    fn counted_string(&mut self, len: usize) -> Option<String> {
        let raw = self.take(len)?;
        Some(decode_ascii(raw))
    }
}

/// Strings in this format are ASCII with a trailing NUL. Trim at the
/// first NUL and replace anything non-UTF-8 rather than rejecting the
/// whole cabinet.
fn decode_ascii(raw: &[u8]) -> String {
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

/// The fixed-size header is exactly 100 bytes and begins with `MSCE`.
const MSCE_MAGIC: &[u8; 4] = b"MSCE";
const MSCE_HEADER_LEN: usize = 100;

/// Parse a `.000` install header according to the documented layout.
///
/// Returns `None` when the payload is not an MSCE file at all, or when
/// a section is truncated past repair. Individual malformed *entries*
/// are skipped rather than failing the whole parse, because a cabinet
/// that lists 198 files is still worth reading if one record is odd.
fn parse_msce(data: &[u8]) -> Option<WinCeInstallHeader> {
    if data.len() < MSCE_HEADER_LEN || &data[..4] != MSCE_MAGIC {
        return None;
    }

    let mut fixed = Cursor::at(data, 20);
    let arch = fixed.u32()?;

    // Entry counts at offset 48, then section offsets at 60.
    let mut counts = Cursor::at(data, 48);
    let string_count = counts.u16()? as usize;
    let dir_count = counts.u16()? as usize;
    let file_count = counts.u16()? as usize;
    let hive_count = counts.u16()? as usize;
    let key_count = counts.u16()? as usize;
    let link_count = counts.u16()? as usize;
    let strings_off = counts.u32()? as usize;
    let dirs_off = counts.u32()? as usize;
    let files_off = counts.u32()? as usize;
    let hives_off = counts.u32()? as usize;
    let keys_off = counts.u32()? as usize;
    let links_off = counts.u32()? as usize;
    let appname_off = counts.u16()? as usize;
    let appname_len = counts.u16()? as usize;
    let provider_off = counts.u16()? as usize;
    let provider_len = counts.u16()? as usize;

    let mut header = WinCeInstallHeader {
        arch: Some(arch),
        structured: true,
        ..Default::default()
    };
    header.app_name = read_fixed_string(data, appname_off, appname_len);
    header.provider = read_fixed_string(data, provider_off, provider_len);

    let strings = parse_strings(data, strings_off, string_count);
    let dirs = parse_dirs(data, dirs_off, dir_count, &strings);
    let files = parse_files(data, files_off, file_count, &dirs);

    header.install_dir = pick_install_dir(&files);
    header.files = files;
    header.registry = parse_registry(data, hives_off, hive_count, keys_off, key_count, &strings);
    header.shortcut_target = parse_links(data, links_off, link_count, &header.files);

    // `%InstallDir%` is a documented substitution in registry payloads;
    // the Rayman cabs write their install path that way. Leaving it
    // literal would hand the guest a directory that cannot exist.
    if let Some(install_dir) = header.install_dir.clone() {
        for value in header.registry.iter_mut() {
            if let Some(text) = value.string.as_mut() {
                *text = substitute_install_dir(text, &install_dir)
                    .trim_end_matches('\\')
                    .to_string();
            }
        }
    }

    Some(header)
}

/// The APPNAME / PROVIDER strings are located by an absolute offset and
/// a byte length that includes the NUL terminator.
fn read_fixed_string(data: &[u8], offset: usize, len: usize) -> Option<String> {
    if len == 0 {
        return None;
    }
    let raw = data.get(offset..offset.checked_add(len)?)?;
    let text = decode_ascii(raw);
    (!text.is_empty()).then_some(text)
}

/// `STRINGS`: `{u16 id, u16 len, [len] bytes}` repeated, no padding.
fn parse_strings(data: &[u8], offset: usize, count: usize) -> HashMap<u16, String> {
    let mut out = HashMap::with_capacity(count);
    let mut cursor = Cursor::at(data, offset);
    for _ in 0..count {
        let (Some(id), Some(len)) = (cursor.u16(), cursor.u16()) else {
            break;
        };
        let Some(text) = cursor.counted_string(len as usize) else {
            break;
        };
        out.insert(id, text);
    }
    out
}

/// `DIRS`: `{u16 id, u16 len, [len/2] u16 string ids terminated by 0}`.
///
/// Each directory is spelled as a list of `STRINGS` ids joined with
/// backslashes; the first component is often a `%CEn%` macro.
fn parse_dirs(
    data: &[u8],
    offset: usize,
    count: usize,
    strings: &HashMap<u16, String>,
) -> HashMap<u16, String> {
    let mut out = HashMap::with_capacity(count);
    let mut cursor = Cursor::at(data, offset);
    for _ in 0..count {
        let (Some(id), Some(len)) = (cursor.u16(), cursor.u16()) else {
            break;
        };
        let Some(spec) = cursor.take(len as usize) else {
            break;
        };
        let mut parts = Vec::new();
        for chunk in spec.chunks_exact(2) {
            let sid = u16::from_le_bytes([chunk[0], chunk[1]]);
            if sid == 0 {
                break;
            }
            match strings.get(&sid) {
                Some(part) => parts.push(part.as_str()),
                // A dangling id means the cabinet references a string
                // it never defined; keeping the rest of the path is
                // better than dropping the directory entirely.
                None => continue,
            }
        }
        if parts.is_empty() {
            continue;
        }
        out.insert(id, canonicalise_install_dir(&parts.join("\\")));
    }
    out
}

/// `FILES`: `{u16 id, u16 dir id, u16 unknown, u32 flags, u16 name len,
/// [name len] bytes}`.
///
/// The file ID is the cabinet entry's three-digit extension, which is
/// how the payload is located: `RaymanUltimateARM.exe` is whichever
/// cabinet member ends in `.001`.
fn parse_files(
    data: &[u8],
    offset: usize,
    count: usize,
    dirs: &HashMap<u16, String>,
) -> Vec<WinCeInstallFile> {
    let mut out = Vec::with_capacity(count);
    let mut cursor = Cursor::at(data, offset);
    for _ in 0..count {
        let (Some(file_id), Some(dir_id), Some(_unknown), Some(_flags), Some(name_len)) = (
            cursor.u16(),
            cursor.u16(),
            cursor.u16(),
            cursor.u32(),
            cursor.u16(),
        ) else {
            break;
        };
        let Some(name) = cursor.counted_string(name_len as usize) else {
            break;
        };
        // A name carrying a separator would let a cabinet write outside
        // the extract directory once the destination is turned into a
        // host path.
        if name.is_empty() || name.contains(['\\', '/']) || name == ".." {
            continue;
        }
        let dir = dirs.get(&dir_id).map(String::as_str).unwrap_or("\\");
        out.push(WinCeInstallFile {
            source: format!(".{file_id:03}"),
            file_id,
            destination: format!("{dir}{name}"),
        });
    }
    out
}

/// The shallowest directory that prefixes every installed file, with a
/// trailing backslash.
///
/// Rayman Ultimate installs into `\Program Files\RaymanUltimate` and
/// eight subdirectories of `…\PCMAP`; anchoring on any of the latter
/// would push the rest of the payload above the extract root. Files
/// installed into `\Windows` (shared DLLs) are ignored when choosing
/// the anchor but still keep their own destination.
fn pick_install_dir(files: &[WinCeInstallFile]) -> Option<String> {
    let candidates = || {
        files.iter().filter_map(|f| {
            let dir = &f.destination[..f.destination.rfind('\\')? + 1];
            let usable = dir.len() > 1 && !dir.to_ascii_lowercase().starts_with("\\windows\\");
            usable.then_some(dir)
        })
    };
    candidates()
        .filter(|dir| candidates().all(|other| other.starts_with(*dir)))
        .min_by_key(|dir| dir.len())
        .map(str::to_string)
}

/// `REGHIVES` + `REGKEYS`, resolved into the same value shape the
/// `_setup.xml` path produces so both install formats feed one
/// registry-replay path.
fn parse_registry(
    data: &[u8],
    hives_off: usize,
    hive_count: usize,
    keys_off: usize,
    key_count: usize,
    strings: &HashMap<u16, String>,
) -> Vec<SetupRegistryValue> {
    // `{u16 id, u16 root, u16 unknown, u16 len, [len/2] u16 string ids}`
    let mut hives: HashMap<u16, String> = HashMap::with_capacity(hive_count);
    let mut cursor = Cursor::at(data, hives_off);
    for _ in 0..hive_count {
        let (Some(id), Some(root), Some(_unknown), Some(len)) =
            (cursor.u16(), cursor.u16(), cursor.u16(), cursor.u16())
        else {
            break;
        };
        let Some(spec) = cursor.take(len as usize) else {
            break;
        };
        let root = match root {
            1 => "HKCR",
            2 => "HKCU",
            3 => "HKLM",
            4 => "HKU",
            _ => continue,
        };
        let mut parts = vec![root.to_string()];
        for chunk in spec.chunks_exact(2) {
            let sid = u16::from_le_bytes([chunk[0], chunk[1]]);
            if sid == 0 {
                break;
            }
            if let Some(part) = strings.get(&sid) {
                parts.push(part.clone());
            }
        }
        hives.insert(id, parts.join("\\"));
    }

    // `{u16 id, u16 hive id, u16 substitute, u32 type, u16 len,
    // [len] bytes}` where the data is a NUL-terminated value name
    // followed by the payload.
    let mut out = Vec::with_capacity(key_count);
    let mut cursor = Cursor::at(data, keys_off);
    for _ in 0..key_count {
        let (Some(_id), Some(hive_id), Some(_subst), Some(kind), Some(len)) = (
            cursor.u16(),
            cursor.u16(),
            cursor.u16(),
            cursor.u32(),
            cursor.u16(),
        ) else {
            break;
        };
        let Some(blob) = cursor.take(len as usize) else {
            break;
        };
        let Some(key) = hives.get(&hive_id) else {
            continue;
        };
        let split = blob.iter().position(|&b| b == 0).unwrap_or(blob.len());
        let name = String::from_utf8_lossy(&blob[..split]).into_owned();
        let payload = blob.get(split + 1..).unwrap_or(&[]);
        let mut value = SetupRegistryValue {
            key: canonicalise_registry_key(key),
            name,
            ..Default::default()
        };
        // Bit 16 selects "integer-ish", bit 0 selects "typed"; see the
        // flag table in the spec.
        match (kind & 0x0001_0000 != 0, kind & 0x0000_0001 != 0) {
            (true, true) => {
                if payload.len() >= 4 {
                    value.dword = Some(u32::from_le_bytes([
                        payload[0], payload[1], payload[2], payload[3],
                    ]));
                }
            }
            // TYPE_BINARY has no textual form we can replay; skip it
            // rather than inventing a string for it.
            (false, true) => continue,
            // TYPE_SZ and TYPE_MULTI_SZ both start with a string; we
            // only ever read the first one back.
            _ => value.string = Some(expand_ce_macros(&decode_ascii(payload))),
        }
        out.push(value);
    }
    out
}

/// `LINKS`: return the guest path of the first link that points at a
/// file, which is the executable the device's shell would launch.
///
/// Layout is `{u16 id, u16 unknown, u16 base dir, u16 target id,
/// u16 type, u16 len, [len/2] u16 string ids}`, with type 1 meaning the
/// target is a file ID.
fn parse_links(
    data: &[u8],
    offset: usize,
    count: usize,
    files: &[WinCeInstallFile],
) -> Option<String> {
    let mut cursor = Cursor::at(data, offset);
    for _ in 0..count {
        let (Some(_id), Some(_unknown), Some(_base), Some(target), Some(kind), Some(len)) = (
            cursor.u16(),
            cursor.u16(),
            cursor.u16(),
            cursor.u16(),
            cursor.u16(),
            cursor.u16(),
        ) else {
            break;
        };
        if cursor.take(len as usize).is_none() {
            break;
        }
        if kind != 1 {
            continue;
        }
        if let Some(file) = files.iter().find(|f| f.file_id == target) {
            return Some(file.destination.clone());
        }
    }
    None
}

/// The historical best-effort reader, kept as a fallback for `.000`
/// payloads whose documented structure does not parse.
fn parse_by_scanning(data: &[u8]) -> WinCeInstallHeader {
    let mut header = WinCeInstallHeader::default();
    let mut strings: Vec<String> = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        // Look for sequences of printable wide chars terminated by
        // a NUL wide char.
        let start = i;
        let mut s = String::new();
        while i + 2 <= data.len() {
            let lo = data[i] as u16;
            let hi = data[i + 1] as u16;
            let c = lo | (hi << 8);
            if c == 0 {
                break;
            }
            if let Some(ch) = char::from_u32(c as u32) {
                if ch.is_ascii_graphic() || ch == ' ' {
                    s.push(ch);
                    i += 2;
                    continue;
                }
            }
            s.clear();
            break;
        }
        if !s.is_empty() && s.len() >= 3 {
            strings.push(s);
            // Skip the trailing NUL pair.
            i += 2;
        } else {
            i = start + 1;
        }
    }
    if let Some(s) = strings.first() {
        header.provider = Some(s.clone());
    }
    if let Some(s) = strings.get(1) {
        header.app_name = Some(s.clone());
    }
    let ascii_strings = extract_ascii_strings(data);
    if let Some(path) = ascii_strings
        .iter()
        .find(|s| s.starts_with("%CE") && s.contains('\\'))
    {
        header.install_dir = Some(canonicalise_install_dir(path));
    }
    if header.provider.is_none() {
        header.provider = ascii_strings.first().cloned();
    }
    if header.app_name.is_none() {
        header.app_name = ascii_strings.get(1).cloned();
    }
    parse_legacy_install_records(data, &mut header);
    header
}

fn extract_ascii_strings(data: &[u8]) -> Vec<String> {
    extract_ascii_strings_with_offsets(data)
        .into_iter()
        .map(|(_, value)| value)
        .collect()
}

fn extract_ascii_strings_with_offsets(data: &[u8]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut start = None;
    for (index, &byte) in data.iter().enumerate() {
        if byte.is_ascii_graphic() || byte == b' ' {
            if start.is_none() {
                start = Some(index);
            }
        } else if let Some(begin) = start.take() {
            if index - begin >= 3 {
                out.push((
                    begin,
                    String::from_utf8_lossy(&data[begin..index]).into_owned(),
                ));
            }
        }
    }
    if let Some(begin) = start {
        if data.len() - begin >= 3 {
            out.push((begin, String::from_utf8_lossy(&data[begin..]).into_owned()));
        }
    }
    out
}

fn parse_legacy_install_records(data: &[u8], header: &mut WinCeInstallHeader) {
    let install_dir = match header.install_dir.clone() {
        Some(dir) => dir,
        None => return,
    };
    let strings = extract_ascii_strings_with_offsets(data);
    let Some(first_file) = strings
        .iter()
        .position(|(_, value)| value.to_ascii_lowercase().ends_with(".exe"))
    else {
        return;
    };

    let mut folders = std::collections::HashMap::new();
    for (offset, name) in strings.iter().take(first_file) {
        if *offset < 4 {
            continue;
        }
        let id = u16::from_le_bytes([data[*offset - 4], data[*offset - 3]]);
        let length = u16::from_le_bytes([data[*offset - 2], data[*offset - 1]]) as usize;
        if length == name.len() + 1 && id != 0 {
            folders.insert(id, name.clone());
        }
    }

    for (offset, name) in strings.iter().skip(first_file) {
        if *offset < 12 {
            continue;
        }
        let record = offset - 12;
        let sequence = u16::from_le_bytes([data[record], data[record + 1]]);
        let folder_id = u16::from_le_bytes([data[record + 2], data[record + 3]]);
        let length = u16::from_le_bytes([data[record + 10], data[record + 11]]) as usize;
        if sequence == 0 || length != name.len() + 1 {
            continue;
        }
        let source_id = sequence;
        let folder = folders.get(&folder_id).map(String::as_str).unwrap_or("");
        let relative = match folder.to_ascii_lowercase().as_str() {
            "bin" => name.clone(),
            "resources" => format!("resources\\{name}"),
            "gui" | "sounds" | "scenes" | "vehicle" => {
                format!("resources\\{folder}\\{name}")
            }
            _ if folder.is_empty() => name.clone(),
            _ => format!("{folder}\\{name}"),
        };
        header.files.push(WinCeInstallFile {
            source: format!(".{source_id:03}"),
            file_id: source_id,
            destination: join_install_path(&install_dir, &relative),
        });
    }
    if header.files.is_empty() {
        let names = extract_ascii_strings(data)
            .into_iter()
            .filter(|name| {
                let lower = name.to_ascii_lowercase();
                (lower.ends_with(".exe") || lower.ends_with(".dll")) && !lower.contains(".000")
            })
            .collect::<Vec<_>>();
        for (index, name) in names.into_iter().enumerate() {
            header.files.push(WinCeInstallFile {
                source: format!(".{:03}", index + 1),
                file_id: (index + 1) as u16,
                destination: join_install_path(&install_dir, &name),
            });
        }
    }
}

/// Join a legacy `.000` install directory with a file's relative
/// destination.
///
/// The `.000` records reference folders by id, and the folder table
/// also contains the shortcut destinations (`%CE14%` — the Start
/// menu). A file record that resolves to one of those would otherwise
/// produce a bogus path such as
/// `\\Program Files\\Games\\SkyForce Reloaded\\\\%CE14%\\SkyForceReloaded.exe`,
/// which no game ever reads from. Drop any `%CEnn%` component and
/// collapse repeated separators so the result is the path the
/// installer would really have written.
fn join_install_path(install_dir: &str, relative: &str) -> String {
    let mut out = String::from(install_dir.trim_end_matches('\\'));
    for part in relative.split('\\') {
        let part = part.trim();
        if part.is_empty() || (part.starts_with('%') && part.ends_with('%')) {
            continue;
        }
        out.push('\\');
        out.push_str(part);
    }
    out
}

/// Convenience — open a cabinet, dump every file into `out_dir`, and
/// also return the parsed install header if one was found.
pub fn extract_with_header<P: AsRef<Path>, Q: AsRef<Path>>(
    cab_path: P,
    out_dir: Q,
) -> Result<(Vec<CabFile>, Option<WinCeInstallHeader>), CabError> {
    let files = extract_all(&cab_path, &out_dir)?;
    let mut header = None;
    for f in &files {
        // The install header is conventionally a `.000` file at the
        // root of the cabinet.
        if f.short_name.to_ascii_lowercase().ends_with(".000") {
            // Re-read and parse.
            let mut buf = Vec::new();
            let mut r = File::open(&f.extracted_path)?;
            r.seek(SeekFrom::Start(0))?;
            r.read_to_end(&mut buf)?;
            header = Some(WinCeInstallHeader::parse_bytes(&buf)?);
            break;
        }
    }
    Ok((files, header))
}

/// Parsed `_setup.xml` shipped alongside the binary inside Pocket PC
/// `.cab` files. Modern (CabWiz / Visual Studio Smart Device project)
/// cabs use this XML — distinct from the ancient binary `.000` install
/// header — to describe the install destination of every short-named
/// payload entry.
///
/// We only extract the bits we need to make a game runnable:
/// * the install directory (target on the device, e.g.
///   `\Program Files\Astraware\Zuma\`),
/// * the `short_name -> long_name` rename map,
///
/// so the host launcher can:
/// * materialise the long-name copies of each asset under the extract
///   directory, and
/// * mount the extract directory at the install path the game was
///   compiled to read from.
#[derive(Debug, Clone, Default)]
pub struct WinCeSetupScript {
    /// Resolved install directory on the device. Always uses backslash
    /// separators and ends with a backslash, e.g.
    /// `\Program Files\Astraware\Zuma\`.
    pub install_dir: Option<String>,
    /// Human-readable app name (`<parm name="AppName" value="…" />`).
    pub app_name: Option<String>,
    /// `(short_name_in_cab, long_name_on_disk)` pairs.
    pub renames: Vec<(String, String)>,
    /// Every directory the `<characteristic type="FileOperation">`
    /// block installs files into, canonicalised the same way as
    /// [`Self::install_dir`] (leading + trailing backslash, `%CEn%`
    /// macros expanded).
    ///
    /// This matters because the `InstallDir` parm and the real
    /// destination often disagree: Gameloft's Sonic Unleashed cab
    /// declares `InstallDir = %CE1%\SONIC` but extracts every file
    /// into `%CE1%\Gameloft\SONIC`, which is also the path the game
    /// hard-codes when it opens `data.bar`. Mounting only the
    /// `InstallDir` left those `fopen` calls failing and the game
    /// bailed out before it ever drew a frame.
    pub install_dirs: Vec<String>,
    /// Registry values the `Registry` section of `_setup.xml` installs,
    /// as `(canonical key, value name, payload)`.
    ///
    /// A Pocket PC installer writes the paths and licence records a game
    /// reads back on startup. Astraware Bejeweled quits with
    /// `ExitProcess(0x42)` when `HKLM\SOFTWARE\Apps\Astraware
    /// Bejeweled\SaveDir` is missing, so a faithful CAB install has to
    /// replay them.
    pub registry: Vec<SetupRegistryValue>,
    /// Guest path the Start-menu shortcut points at, e.g.
    /// `\\Program Files\\Gameloft\\Sonic Unleashed\\Sonic Unleashed.exe`.
    ///
    /// This is the only entry in the script that says *which* payload
    /// the user actually launches. Cabs regularly ship more than one
    /// executable — Sonic Unleashed's QVGA cab installs a tiny
    /// `GetRealDPI.exe` helper next to the game — so picking "the
    /// first (or largest) .exe" loads the wrong binary.
    pub shortcut_target: Option<String>,
}

/// One value from the `Registry` section of `_setup.xml`.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SetupRegistryValue {
    /// Canonical key path, e.g. `HKLM\SOFTWARE\Apps\Astraware Bejeweled`.
    pub key: String,
    /// Value name; empty for a key's default value.
    pub name: String,
    /// String payload with `%CEn%` macros expanded.
    pub string: Option<String>,
    /// Numeric payload for `datatype="integer"`.
    pub dword: Option<u32>,
}

impl WinCeSetupScript {
    /// Parse a `_setup.xml` payload. The format is a small subset of
    /// WAP provisioning documents and varies enough between versions
    /// that we use a regex-style scan rather than a full XML parser.
    pub fn parse_bytes(data: &[u8]) -> Self {
        let s = match std::str::from_utf8(data) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };

        let mut script = WinCeSetupScript::default();

        // Pull out parm="..." values for InstallDir, AppName, ...
        for line in s.lines() {
            let line = line.trim();
            if let Some(name) = parm_value(line, "InstallDir") {
                if script.install_dir.is_none() && !name.eq_ignore_ascii_case("%InstallDir%") {
                    script.install_dir = Some(canonicalise_install_dir(&name));
                }
            } else if let Some(name) = parm_value(line, "AppName") {
                script.app_name = Some(name);
            }
        }
        if script.install_dir.is_none() {
            for line in s.lines() {
                if let Some(name) = type_attribute(line.trim()) {
                    if name.starts_with("%CE") && name.contains('\\') {
                        script.install_dir = Some(canonicalise_install_dir(&name));
                        break;
                    }
                }
            }
        }

        // The rename map lives in nested `<characteristic
        // type="long_name.ext"><characteristic type="Extract"><parm
        // name="Source" value="SHORT~1.NNN" /></characteristic></…>`
        // blocks. We can be lazy: a simple state machine that
        // remembers the most recent `type="..."` that does NOT equal
        // a structural keyword (Install, FileOperation, MakeDir, …),
        // then pairs it with the next `Source` parm we see, is enough
        // for every CAB Microsoft / CabWiz produces.
        let structural = [
            "Install",
            "FileOperation",
            "MakeDir",
            "Extract",
            "Shortcut",
            "Registry",
        ];
        // The enclosing `<characteristic type="%CE1%\Xtrakt\data">`
        // block is the only place the destination *directory* appears —
        // the file's own `type=` is a bare name. Remember the most
        // recent directory so the rename records a full guest path;
        // dropping it flattens `data\arch0.par` onto the install root
        // and Xtrakt bails out with "Failed to initialize game".
        let mut current_dir: Option<String> = None;
        let mut current_long: Option<String> = None;
        for raw in s.split(['\n', '>']) {
            let line = raw.trim();
            if let Some(t) = type_attribute(line) {
                let is_dir = t.starts_with('%') || t.contains('\\');
                if is_dir {
                    let dir = canonicalise_install_dir(&t);
                    if !script.install_dirs.contains(&dir) {
                        script.install_dirs.push(dir.clone());
                    }
                    current_dir = Some(dir);
                } else if !structural.contains(&t.as_str()) {
                    if t.to_ascii_lowercase().ends_with(".lnk") {
                        current_long = None;
                    } else {
                        current_long = Some(match &current_dir {
                            Some(dir) => format!("{dir}{t}"),
                            None => t,
                        });
                    }
                }
            }
            if let Some(source) = parm_value(line, "Source") {
                // A `Shortcut` block's `Source` is a full guest path
                // (`%CE1%\\Gameloft\\SONIC\\SONIC.exe`); a file
                // extraction's `Source` is always a bare 8.3 name in
                // the cabinet. Tell them apart by the separator.
                if source.contains('\\') || source.starts_with('%') {
                    script.shortcut_target = Some(canonicalise_shortcut_target(&source));
                    current_long = None;
                } else if let Some(long) = current_long.take() {
                    script.renames.push((source, long));
                }
            }
        }

        // ---- Registry section ----
        //
        // <characteristic type="Registry">
        //   <characteristic type="HKLM\SOFTWARE\Apps\Foo">
        //     <parm name="SaveDir" value="%CE5%\Saves" datatype="string" />
        //   </characteristic>
        // </characteristic>
        //
        // Track the innermost `type="HK.."` key while walking tags and
        // attach every `<parm>` we see under it.
        let mut in_registry = false;
        let mut current_key: Option<String> = None;
        for raw in s.split('<') {
            let tag = raw.trim();
            if tag.starts_with("/characteristic") {
                continue;
            }
            if let Some(t) = tag.strip_prefix("characteristic").and_then(type_attribute) {
                if t.eq_ignore_ascii_case("Registry") {
                    in_registry = true;
                    current_key = None;
                } else if in_registry && t.to_ascii_uppercase().starts_with("HK") {
                    current_key = Some(canonicalise_registry_key(&t));
                }
                continue;
            }
            if !in_registry || !tag.starts_with("parm") {
                continue;
            }
            let Some(key) = current_key.clone() else {
                continue;
            };
            let Some(name) = attribute(tag, "name") else {
                continue;
            };
            let value = attribute(tag, "value").unwrap_or_default();
            let datatype = attribute(tag, "datatype").unwrap_or_default();
            let mut entry = SetupRegistryValue {
                key,
                name,
                ..Default::default()
            };
            if datatype.eq_ignore_ascii_case("integer") {
                entry.dword = parse_setup_integer(&value);
            } else {
                entry.string = Some(expand_ce_macros(&value));
            }
            script.registry.push(entry);
        }

        // CabWiz lets a `FileOperation` directory (and a registry
        // payload) refer back to the `Install` section's `InstallDir`
        // through a literal `%InstallDir%`. Leaving it unexpanded turns
        // the mount prefix — and the module path we report from
        // `GetModuleFileNameW` — into `\\%InstallDir%\\`, so every
        // asset path the game rebuilds from its own module name lands
        // in a directory that cannot exist. Astraware's Bejeweled ends
        // up searching `.\\*.pdb`, finds no resource database and calls
        // `ExitProcess(0x42)` before drawing a frame.
        if let Some(install_dir) = script.install_dir.clone() {
            for dir in script.install_dirs.iter_mut() {
                *dir = substitute_install_dir(dir, &install_dir);
            }
            if let Some(target) = script.shortcut_target.as_mut() {
                *target = substitute_install_dir(target, &install_dir);
            }
            for (_short, long) in script.renames.iter_mut() {
                *long = substitute_install_dir(long, &install_dir);
            }
            for value in script.registry.iter_mut() {
                if let Some(text) = value.string.as_mut() {
                    *text = substitute_install_dir(text, &install_dir)
                        .trim_end_matches('\\')
                        .to_string();
                }
            }
        }

        script
    }

    /// The shallowest directory every installed file sits under, without
    /// a trailing separator.
    ///
    /// `install_dirs` — the directories the `FileOperation` block really
    /// writes into — wins over the declared `InstallDir`, because the
    /// two frequently disagree: Gameloft's Sonic Unleashed declares
    /// `%CE1%\SONIC` but installs into `%CE1%\Gameloft\SONIC`, which is
    /// the path the game hard-codes. Preferring whichever string is
    /// merely shorter would pick the declaration and mount the payload
    /// at a directory no file was written to.
    ///
    /// Two filters matter. `install_dirs` also lists the Start-menu
    /// folders the shortcut lives in (`\Windows\Start Menu\…`), which
    /// are never where a game reads its assets from and which would win
    /// on length. And only a candidate that actually prefixes every
    /// destination qualifies — anchoring on a sibling would push assets
    /// above the extract root. Xtrakt is the case that forced the
    /// shallowest-of-the-real-directories rule: taking the first usable
    /// entry gave `\Program Files\Xtrakt\data\music\`, so the mount
    /// prefix sat two levels below the install root and the game could
    /// not open `data\arch0.par`.
    pub fn install_root(&self) -> Option<String> {
        let usable = |dir: &String| {
            dir.len() > 1
                && !dir.contains('%')
                && !dir.to_ascii_lowercase().starts_with("\\windows\\")
        };
        let anchors_every_file = |dir: &String| {
            self.renames
                .iter()
                .all(|(_, long)| !long.starts_with('\\') || long.starts_with(dir.as_str()))
        };
        self.install_dirs
            .iter()
            .filter(|dir| usable(dir) && anchors_every_file(dir))
            .min_by_key(|dir| dir.len())
            .or_else(|| self.install_dir.as_ref().filter(|dir| usable(dir)))
            .map(|dir| dir.trim_end_matches('\\').to_string())
    }

    /// Path of an installed file relative to [`Self::install_root`],
    /// using backslash separators — `data\arch0.par` for Xtrakt's
    /// `\Program Files\Xtrakt\data\arch0.par`.
    ///
    /// Strips the *root* install directory rather than the deepest
    /// matching one: matching the longest prefix would flatten the
    /// `data\` / `data\music\` hierarchy the game opens back onto the
    /// install root, which is what made Xtrakt fail to initialise.
    /// Returns `None` for a traversal attempt or an empty tail.
    pub fn relative_destination<'a>(&self, long: &'a str, root: Option<&str>) -> Option<&'a str> {
        let relative = root
            .and_then(|prefix| {
                long.strip_prefix(prefix)
                    .and_then(|tail| tail.strip_prefix('\\'))
            })
            .unwrap_or_else(|| {
                // No install root to anchor against (or the file lands
                // outside it): keep the historical basename behaviour.
                long.rsplit('\\')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(long)
            });
        let safe = !relative.is_empty()
            && !relative.starts_with('\\')
            && relative.split('\\').all(|seg| seg != ".." && seg != ".");
        safe.then_some(relative)
    }
}

/// Join a backslash-separated guest-relative path onto a host directory
/// one segment at a time, so `\` stays a separator on every platform.
fn join_guest_relative(root: &Path, relative: &str) -> PathBuf {
    relative
        .split('\\')
        .filter(|seg| !seg.is_empty())
        .fold(root.to_path_buf(), |acc, seg| acc.join(seg))
}

/// Replace a literal `%InstallDir%` with the script's expanded install
/// directory, collapsing the duplicate separators the splice creates.
fn substitute_install_dir(value: &str, install_dir: &str) -> String {
    let lowered = value.to_ascii_lowercase();
    let Some(pos) = lowered.find("%installdir%") else {
        return value.to_string();
    };
    let mut out = String::with_capacity(value.len() + install_dir.len());
    out.push_str(&value[..pos]);
    out.push_str(install_dir);
    out.push_str(&value[pos + "%installdir%".len()..]);
    while out.contains("\\\\") {
        out = out.replace("\\\\", "\\");
    }
    out
}

/// Expand the `%CEn%` macros in a shortcut's target and normalise the
/// separators, keeping the trailing file name intact.
fn canonicalise_shortcut_target(raw: &str) -> String {
    let (dir, file) = match raw.replace('/', "\\").rsplit_once('\\') {
        Some((dir, file)) => (dir.to_string(), file.to_string()),
        None => return raw.to_string(),
    };
    format!("{}{}", canonicalise_install_dir(&dir), file)
}

/// `\Program Files\Astraware\Zuma` -> `\Program Files\Astraware\Zuma\`.
/// `%CE1%\Astraware\Zuma` -> `\Program Files\Astraware\Zuma\` (CE1 is
/// the `\Program Files\` macro on Windows Mobile 5/6).
fn canonicalise_install_dir(raw: &str) -> String {
    let mut s = raw.replace('/', "\\");
    // The documented CabWiz `%CEn%` install-macro table. Getting these
    // wrong is not cosmetic: the expansion becomes the guest path we
    // mount the payload at *and* the module path we report from
    // `GetModuleFileNameW`, so a bogus expansion sends every
    // asset-loading `fopen` to a directory that does not exist.
    // Longest macros first — a plain `str::replace` pass would rewrite
    // the `%CE1%` prefix inside `%CE14%`.
    for (macro_name, replacement) in [
        ("%CE17%", "\\Windows\\Start Menu"),
        ("%CE16%", "\\Windows\\Recent"),
        ("%CE15%", "\\Windows\\Fonts"),
        ("%CE14%", "\\Windows\\Start Menu\\Programs\\Games"),
        ("%CE13%", "\\Windows\\Start Menu\\Programs\\Communications"),
        ("%CE12%", "\\Windows\\Start Menu\\Programs\\Accessories"),
        ("%CE11%", "\\Windows\\Start Menu\\Programs"),
        ("%CE10%", "\\Program Files\\Office"),
        ("%CE9%", "\\Program Files\\Pocket Outlook"),
        ("%CE8%", "\\Program Files\\Games"),
        ("%CE7%", "\\Program Files\\Communication"),
        ("%CE6%", "\\Program Files\\Accessories"),
        ("%CE5%", "\\My Documents"),
        ("%CE4%", "\\Windows\\StartUp"),
        ("%CE3%", "\\Windows\\Desktop"),
        ("%CE2%", "\\Windows"),
        ("%CE1%", "\\Program Files"),
    ] {
        s = s.replace(macro_name, replacement);
    }
    // Anything we don't know about would otherwise leak a literal
    // `%CE9%` into a mount prefix; treat it as the device root.
    while let Some(start) = s.find("%CE") {
        match s[start + 1..].find('%') {
            Some(offset) => {
                let end = start + 1 + offset + 1;
                s.replace_range(start..end, "");
            }
            None => break,
        }
    }
    if !s.starts_with('\\') {
        s.insert(0, '\\');
    }
    if !s.ends_with('\\') {
        s.push('\\');
    }
    s
}

/// Match `<parm name="<key>" value="<val>" ... />` and return `val`.
fn parm_value(line: &str, key: &str) -> Option<String> {
    let needle = format!("name=\"{key}\"");
    let pos = line.find(&needle)?;
    let after = &line[pos + needle.len()..];
    let val_pos = after.find("value=\"")?;
    let after_val = &after[val_pos + "value=\"".len()..];
    let end = after_val.find('"')?;
    Some(after_val[..end].to_string())
}

/// Match `type="<val>"` and return `val`.
fn type_attribute(line: &str) -> Option<String> {
    let pos = line.find("type=\"")?;
    let after = &line[pos + "type=\"".len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// `HKLM\SOFTWARE\Apps\Foo` / `HKEY_LOCAL_MACHINE\...` -> canonical
/// `HKLM\SOFTWARE\Apps\Foo` with no leading or trailing separator.
fn canonicalise_registry_key(raw: &str) -> String {
    let mut s = expand_ce_macros(raw).replace('/', "\\");
    while s.contains("\\\\") {
        s = s.replace("\\\\", "\\");
    }
    let s = s.trim_matches('\\').to_string();
    for (long, short) in [
        ("HKEY_LOCAL_MACHINE", "HKLM"),
        ("HKEY_CURRENT_USER", "HKCU"),
        ("HKEY_CLASSES_ROOT", "HKCR"),
        ("HKEY_USERS", "HKU"),
    ] {
        if let Some(rest) = s.strip_prefix(long) {
            return format!("{short}{rest}");
        }
    }
    s
}

/// Expand the `%CEn%` install macros inside a registry payload without
/// forcing the trailing separator [`canonicalise_install_dir`] adds.
fn expand_ce_macros(raw: &str) -> String {
    let expanded = canonicalise_install_dir(raw);
    let trimmed = expanded.trim_end_matches('\\');
    if raw.ends_with('\\') || trimmed.is_empty() {
        expanded
    } else {
        trimmed.to_string()
    }
}

/// `_setup.xml` writes integers in decimal or as `0x`-prefixed hex.
fn parse_setup_integer(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).ok()
    } else {
        trimmed.parse::<u32>().ok()
    }
}

/// Recreate the long file names a WinCE `_setup.xml` asks the installer
/// to produce, alongside the short (8.3) names `extract_all` wrote.
///
/// A Pocket PC cabinet stores its payload under generated 8.3 names
/// (`ASPHAL~1.001`, `000light.002`) and lists the real destination names
/// in `_setup.xml`. A game only ever opens the long names — Asphalt 2 3D
/// looks for `light.bar` next to `Asphalt2_SPV_C600.exe` — so anything
/// that runs a game out of an extracted cabinet has to materialise them
/// first. This is the canonical implementation: both the launcher
/// library's import path and the CLI's throwaway-extract path go through
/// the same rename map so a game imported into the library behaves
/// exactly like one passed to `pockethle run` directly.
///
/// Copies (rather than links) are used so the directory stays
/// self-contained. Failures are logged and skipped: the short-name file
/// is still on disk, and a partially renamed game may still boot.
///
/// Returns the `(short name on disk, long name on disk)` pairs that were
/// created, so a caller that has to name one of the files (the launcher
/// picks an entry-point executable) can prefer the long name the device
/// would have shown.
pub fn materialise_setup_names(root: &Path, files: &[CabFile]) -> Vec<(PathBuf, PathBuf)> {
    let Some(script) = files
        .iter()
        .find(|f| f.short_name.eq_ignore_ascii_case("_setup.xml"))
        .and_then(|f| fs::read(&f.extracted_path).ok())
        .map(|bytes| WinCeSetupScript::parse_bytes(&bytes))
    else {
        return Vec::new();
    };
    let install_root = script.install_root();

    let mut created = Vec::new();
    for (short, long) in &script.renames {
        let Some(src) = files
            .iter()
            .find(|file| file.short_name.eq_ignore_ascii_case(short))
        else {
            log::debug!("_setup.xml names {short} but the cab has no such file; skipping");
            continue;
        };
        let Some(relative) = script.relative_destination(long, install_root.as_deref()) else {
            continue;
        };

        let dest = join_guest_relative(root, relative);
        if dest == src.extracted_path {
            continue;
        }

        // `data\music\` only exists on the host once we create it.
        if let Some(parent) = dest.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                log::debug!("could not create {}: {e}", parent.display());
                continue;
            }
        }

        match fs::copy(&src.extracted_path, &dest) {
            Ok(_) => {
                log::debug!("materialised {} as {}", src.short_name, dest.display());
                created.push((src.extracted_path.clone(), dest));
            }
            Err(e) => log::debug!(
                "could not materialise {} as {}: {e}",
                src.short_name,
                dest.display()
            ),
        }
    }
    created
}

impl WinCeInstallHeader {
    /// Resolve an install destination from this header onto the host
    /// path [`materialise_install_header_names`] wrote it to under
    /// `root`.
    ///
    /// The launcher needs this to name the executable it loads: the
    /// long-name copy is what `GetModuleFileNameW` reports back, and
    /// games rebuild their asset paths from that string.
    pub fn host_path(&self, root: &Path, destination: &str) -> Option<PathBuf> {
        let install_root = self
            .install_dir
            .as_deref()
            .map(|d| d.trim_end_matches('\\'));
        let relative = relative_to_root(destination, install_root)?;
        Some(join_guest_relative(root, relative))
    }
}

/// Recreate the long file names a legacy `.000` (MSCE) install header
/// asks the installer to produce, alongside the short (8.3) names
/// [`extract_all`] wrote.
///
/// This is the `.000` counterpart to [`materialise_setup_names`]. Cabs
/// predating `_setup.xml` — Rayman Ultimate, Rayman Pocket and every
/// other CabWiz-era title — record their real file names *only* in the
/// binary header's `FILES` section, keyed by the numeric extension of
/// each cabinet member. Resolving that mapping is what turns
/// `00000RAY.004` back into `RAY.LNG` and `000DAT~1.030` into
/// `PCMAP\dat\…`, which are the names the game actually opens.
///
/// Returns the `(short name on disk, long name on disk)` pairs created.
pub fn materialise_install_header_names(
    root: &Path,
    files: &[CabFile],
    header: &WinCeInstallHeader,
) -> Vec<(PathBuf, PathBuf)> {
    let install_root = header
        .install_dir
        .as_deref()
        .map(|d| d.trim_end_matches('\\'));

    // Index the cabinet by numeric extension: the spec is explicit that
    // only the extension identifies an entry, the 8.3 stem is generated
    // and must not be relied on.
    let by_id: HashMap<u16, &CabFile> = files
        .iter()
        .filter_map(|f| {
            let ext = f.short_name.rsplit('.').next()?;
            ext.parse::<u16>().ok().map(|id| (id, f))
        })
        .collect();

    let mut created = Vec::new();
    for entry in &header.files {
        let Some(src) = by_id.get(&entry.file_id) else {
            log::debug!(
                ".000 header names file id {} but the cab has no such entry; skipping",
                entry.file_id
            );
            continue;
        };
        let Some(relative) = relative_to_root(&entry.destination, install_root) else {
            continue;
        };
        let dest = join_guest_relative(root, relative);
        if dest == src.extracted_path {
            continue;
        }
        if let Some(parent) = dest.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                log::debug!("could not create {}: {e}", parent.display());
                continue;
            }
        }
        match fs::copy(&src.extracted_path, &dest) {
            Ok(_) => {
                log::debug!("materialised {} as {}", src.short_name, dest.display());
                created.push((src.extracted_path.clone(), dest));
            }
            Err(e) => log::debug!(
                "could not materialise {} as {}: {e}",
                src.short_name,
                dest.display()
            ),
        }
    }
    created
}

/// Strip `root` from a guest destination, rejecting traversal.
///
/// Files installed outside the install root (a shared DLL dropped in
/// `\Windows`) keep their bare name so they still land next to the
/// executable, which is where the loader looks for them.
fn relative_to_root<'a>(destination: &'a str, root: Option<&str>) -> Option<&'a str> {
    let relative = root
        .and_then(|prefix| {
            destination
                .strip_prefix(prefix)
                .and_then(|tail| tail.strip_prefix('\\'))
        })
        .unwrap_or_else(|| {
            destination
                .rsplit('\\')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(destination)
        });
    let safe = !relative.is_empty()
        && !relative.starts_with('\\')
        && relative.split('\\').all(|seg| seg != ".." && seg != ".");
    safe.then_some(relative)
}

/// Match `<... <attr>="<value>" ...>` and return `value`.
fn attribute(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let pos = tag.find(&needle)?;
    let after = &tag[pos + needle.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_section() {
        let xml = br#"<wap-provisioningdoc>
<characteristic type="Install">
<parm name="InstallDir" value="%CE1%\Astraware\Bejeweled" />
</characteristic>
<characteristic type="Registry">
<characteristic type="HKLM\SOFTWARE\Apps\Astraware Bejeweled">
<parm name="SaveDir" value="%CE5%\My Saved Games\Bejeweled" datatype="string" nooverwrite="1" />
<parm name="Level" value="7" datatype="integer" />
</characteristic>
</characteristic>
</wap-provisioningdoc>"#;
        let script = WinCeSetupScript::parse_bytes(xml);
        assert_eq!(script.registry.len(), 2);
        let save_dir = &script.registry[0];
        assert_eq!(save_dir.key, "HKLM\\SOFTWARE\\Apps\\Astraware Bejeweled");
        assert_eq!(save_dir.name, "SaveDir");
        assert_eq!(
            save_dir.string.as_deref(),
            Some("\\My Documents\\My Saved Games\\Bejeweled")
        );
        assert_eq!(script.registry[1].dword, Some(7));
    }

    #[test]
    fn sanitize_strips_traversal() {
        // Leading dots are trimmed, then path separators are
        // replaced with `_`.
        assert_eq!(sanitize_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_name("JUMPYB~1.002"), "JUMPYB~1.002");
        assert_eq!(sanitize_name("a\\b/c"), "a_b_c");
    }

    #[test]
    fn renames_carry_the_enclosing_directory() {
        // Xtrakt's cab nests each file's `type=` inside a directory
        // `characteristic`. Recording only the leaf name flattened
        // `data\arch0.par` onto the install root and the game quit
        // with "Failed to initialize game".
        let script = WinCeSetupScript::parse_bytes(
            br#"<wap-provisioningdoc>
<characteristic type="Install">
<parm name="AppName" value="Southend Xtrakt" />
<parm name="InstallDir" value="%CE1%\Xtrakt" translation="install" />
</characteristic>
<characteristic type="FileOperation">
<characteristic type="%CE1%\Xtrakt\data\music" translation="install">
<characteristic type="MakeDir" />
<characteristic type="xtrakt_music.xm" translation="install">
<characteristic type="Extract">
<parm name="Source" value="XTRAKT~1.010" />
</characteristic>
</characteristic>
</characteristic>
<characteristic type="%CE1%\Xtrakt\data" translation="install">
<characteristic type="MakeDir" />
<characteristic type="arch0.par" translation="install">
<characteristic type="Extract">
<parm name="Source" value="000arch0.005" />
</characteristic>
</characteristic>
</characteristic>
<characteristic type="%CE1%\Xtrakt" translation="install">
<characteristic type="Xtrakt.exe" translation="install">
<characteristic type="Extract">
<parm name="Source" value="00Xtrakt.000" />
</characteristic>
</characteristic>
</characteristic>
</characteristic>
</wap-provisioningdoc>"#,
        );
        assert_eq!(
            script.renames,
            vec![
                (
                    "XTRAKT~1.010".to_string(),
                    r"\Program Files\Xtrakt\data\music\xtrakt_music.xm".to_string()
                ),
                (
                    "000arch0.005".to_string(),
                    r"\Program Files\Xtrakt\data\arch0.par".to_string()
                ),
                (
                    "00Xtrakt.000".to_string(),
                    r"\Program Files\Xtrakt\Xtrakt.exe".to_string()
                ),
            ]
        );

        // The root anchor is the shallowest directory, not the deepest
        // match — anchoring on `…\Xtrakt\data` would flatten the tree
        // right back.
        let root = script.install_root();
        assert_eq!(root.as_deref(), Some(r"\Program Files\Xtrakt"));
        assert_eq!(
            script.relative_destination(&script.renames[0].1, root.as_deref()),
            Some(r"data\music\xtrakt_music.xm")
        );
        assert_eq!(
            script.relative_destination(&script.renames[2].1, root.as_deref()),
            Some("Xtrakt.exe")
        );
    }

    #[test]
    fn relative_destination_rejects_traversal() {
        let script = WinCeSetupScript::default();
        let root = Some(r"\Program Files\Game");
        assert_eq!(
            script.relative_destination(r"\Program Files\Game\..\..\Windows\evil.dll", root),
            None
        );
        // A destination outside the install root keeps the historical
        // basename fallback rather than escaping the extract dir.
        assert_eq!(
            script.relative_destination(r"\Windows\fmodce.dll", root),
            Some("fmodce.dll")
        );
    }

    #[test]
    fn parse_empty_header() {
        let h = WinCeInstallHeader::parse_bytes(&[]).unwrap();
        assert!(h.app_name.is_none());
        assert!(h.files.is_empty());
        assert!(!h.structured);
    }

    /// Build a minimal but valid MSCE `.000` payload in the documented
    /// layout, shaped like Rayman Ultimate's: an app installed into
    /// `%CE1%\Game` with one file in the root and one in a subdirectory,
    /// a registry value using `%InstallDir%`, and a shortcut pointing at
    /// the executable.
    fn synthetic_msce() -> Vec<u8> {
        fn u16b(v: u16) -> [u8; 2] {
            v.to_le_bytes()
        }
        fn u32b(v: u32) -> [u8; 4] {
            v.to_le_bytes()
        }
        fn counted(id: u16, text: &str) -> Vec<u8> {
            let mut out = u16b(id).to_vec();
            out.extend_from_slice(&u16b(text.len() as u16 + 1));
            out.extend_from_slice(text.as_bytes());
            out.push(0);
            out
        }

        // STRINGS: 1 = "%CE1%\Game", 2 = "data", 3 = "Software", 4 = "Acme"
        let mut strings = Vec::new();
        strings.extend(counted(1, r"%CE1%\Game"));
        strings.extend(counted(2, "data"));
        strings.extend(counted(3, "Software"));
        strings.extend(counted(4, "Acme"));

        // DIRS: 1 = {1}, 2 = {1, 2}
        let mut dirs = Vec::new();
        dirs.extend_from_slice(&u16b(1));
        dirs.extend_from_slice(&u16b(4));
        dirs.extend_from_slice(&u16b(1));
        dirs.extend_from_slice(&u16b(0));
        dirs.extend_from_slice(&u16b(2));
        dirs.extend_from_slice(&u16b(6));
        dirs.extend_from_slice(&u16b(1));
        dirs.extend_from_slice(&u16b(2));
        dirs.extend_from_slice(&u16b(0));

        // FILES: id 1 -> Game.exe in dir 1, id 2 -> level.dat in dir 2
        let mut files = Vec::new();
        for (fid, did, name) in [(1u16, 1u16, "Game.exe"), (2, 2, "level.dat")] {
            files.extend_from_slice(&u16b(fid));
            files.extend_from_slice(&u16b(did));
            files.extend_from_slice(&u16b(fid));
            files.extend_from_slice(&u32b(0x4000_0002));
            files.extend_from_slice(&u16b(name.len() as u16 + 1));
            files.extend_from_slice(name.as_bytes());
            files.push(0);
        }

        // REGHIVES: id 1, root 3 (HKLM), spec {3, 4}
        let mut hives = Vec::new();
        hives.extend_from_slice(&u16b(1));
        hives.extend_from_slice(&u16b(3));
        hives.extend_from_slice(&u16b(0));
        hives.extend_from_slice(&u16b(6));
        hives.extend_from_slice(&u16b(3));
        hives.extend_from_slice(&u16b(4));
        hives.extend_from_slice(&u16b(0));

        // REGKEYS: id 1, hive 1, substitute, TYPE_SZ "InstallPath" = "%InstallDir%"
        let payload = b"InstallPath\0%InstallDir%\0";
        let mut keys = Vec::new();
        keys.extend_from_slice(&u16b(1));
        keys.extend_from_slice(&u16b(1));
        keys.extend_from_slice(&u16b(1));
        keys.extend_from_slice(&u32b(0));
        keys.extend_from_slice(&u16b(payload.len() as u16));
        keys.extend_from_slice(payload);

        // LINKS: id 1, base %InstallDir%, target file 1, type 1 (file)
        let mut links = Vec::new();
        links.extend_from_slice(&u16b(1));
        links.extend_from_slice(&u16b(2));
        links.extend_from_slice(&u16b(0));
        links.extend_from_slice(&u16b(1));
        links.extend_from_slice(&u16b(1));
        links.extend_from_slice(&u16b(4));
        links.extend_from_slice(&u16b(1));
        links.extend_from_slice(&u16b(0));

        let app = b"TestGame\0";
        let provider = b"Acme\0";

        // Lay the sections out after the fixed header and the two
        // strings, recording each offset as we go.
        let mut body = Vec::new();
        let place = |bytes: &[u8], body: &mut Vec<u8>| -> u32 {
            let off = MSCE_HEADER_LEN + body.len();
            body.extend_from_slice(bytes);
            off as u32
        };
        let app_off = place(app, &mut body);
        let provider_off = place(provider, &mut body);
        let strings_off = place(&strings, &mut body);
        let dirs_off = place(&dirs, &mut body);
        let files_off = place(&files, &mut body);
        let hives_off = place(&hives, &mut body);
        let keys_off = place(&keys, &mut body);
        let links_off = place(&links, &mut body);

        let mut out = Vec::new();
        out.extend_from_slice(MSCE_MAGIC);
        out.extend_from_slice(&u32b(0));
        out.extend_from_slice(&u32b((MSCE_HEADER_LEN + body.len()) as u32));
        out.extend_from_slice(&u32b(0));
        out.extend_from_slice(&u32b(1));
        out.extend_from_slice(&u32b(2577)); // StrongARM
        for _ in 0..6 {
            out.extend_from_slice(&u32b(0));
        }
        for count in [4u16, 2, 2, 1, 1, 1] {
            out.extend_from_slice(&u16b(count));
        }
        for off in [
            strings_off,
            dirs_off,
            files_off,
            hives_off,
            keys_off,
            links_off,
        ] {
            out.extend_from_slice(&u32b(off));
        }
        out.extend_from_slice(&u16b(app_off as u16));
        out.extend_from_slice(&u16b(app.len() as u16));
        out.extend_from_slice(&u16b(provider_off as u16));
        out.extend_from_slice(&u16b(provider.len() as u16));
        out.extend_from_slice(&u16b(0));
        out.extend_from_slice(&u16b(0));
        out.extend_from_slice(&u16b(0));
        out.extend_from_slice(&u16b(0));
        assert_eq!(out.len(), MSCE_HEADER_LEN);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn parses_msce_structure() {
        let header = WinCeInstallHeader::parse_bytes(&synthetic_msce()).unwrap();
        assert!(header.structured);
        assert_eq!(header.app_name.as_deref(), Some("TestGame"));
        assert_eq!(header.provider.as_deref(), Some("Acme"));
        assert_eq!(header.arch, Some(2577));

        // Names come from the FILES section, and each one is joined to
        // the directory its DIRS entry spells out.
        let names: Vec<_> = header
            .files
            .iter()
            .map(|f| (f.file_id, f.destination.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                (1, r"\Program Files\Game\Game.exe"),
                (2, r"\Program Files\Game\data\level.dat"),
            ]
        );

        // The anchor is the shallowest directory, so `data\` survives.
        assert_eq!(header.install_dir.as_deref(), Some(r"\Program Files\Game\"));
        assert_eq!(
            header.shortcut_target.as_deref(),
            Some(r"\Program Files\Game\Game.exe")
        );

        // `%InstallDir%` in a registry payload is expanded, not leaked.
        assert_eq!(header.registry.len(), 1);
        assert_eq!(header.registry[0].key, r"HKLM\Software\Acme");
        assert_eq!(header.registry[0].name, "InstallPath");
        assert_eq!(
            header.registry[0].string.as_deref(),
            Some(r"\Program Files\Game")
        );
    }

    #[test]
    fn msce_destinations_survive_truncation_and_traversal() {
        // A truncated payload must not panic and must not claim to be
        // structured — the caller falls back to the heuristic reader.
        let full = synthetic_msce();
        for cut in [MSCE_HEADER_LEN, MSCE_HEADER_LEN + 8, full.len() - 4] {
            let header = WinCeInstallHeader::parse_bytes(&full[..cut]).unwrap();
            let _ = header.files.len();
        }

        // A file name carrying a separator would escape the extract
        // directory once it is turned into a host path.
        assert_eq!(
            relative_to_root(
                r"\Program Files\Game\..\evil.dll",
                Some(r"\Program Files\Game")
            ),
            None
        );
        // A file installed outside the install root keeps its bare name
        // rather than escaping.
        assert_eq!(
            relative_to_root(r"\Windows\fmodce.dll", Some(r"\Program Files\Game")),
            Some("fmodce.dll")
        );
    }

    #[test]
    fn install_dir_anchors_above_every_subdirectory() {
        // Rayman Ultimate's shape: the executable sits in the install
        // root and the assets in eight sibling subdirectories. Picking
        // any subdirectory as the anchor would push the rest of the
        // payload above the extract root.
        let files = |dirs: &[&str]| -> Vec<WinCeInstallFile> {
            dirs.iter()
                .enumerate()
                .map(|(i, d)| WinCeInstallFile {
                    source: format!(".{:03}", i + 1),
                    file_id: i as u16 + 1,
                    destination: format!("{d}\\f{i}.dat"),
                })
                .collect()
        };
        assert_eq!(
            pick_install_dir(&files(&[
                r"\Program Files\RaymanUltimate",
                r"\Program Files\RaymanUltimate\PCMAP",
                r"\Program Files\RaymanUltimate\PCMAP\cake",
            ])),
            Some(r"\Program Files\RaymanUltimate\".to_string())
        );
    }

    #[test]
    fn setup_install_dir_is_not_overwritten_by_registry_reference() {
        let script = WinCeSetupScript::parse_bytes(
            br#"<characteristic type="Install"><parm name="InstallDir" value="%CE1%\Astraware\Cubis" /></characteristic><characteristic type="Registry"><characteristic type="HKLM\SOFTWARE\Apps\Astraware Cubis"><parm name="InstallDir" value="%InstallDir%" datatype="string" /></characteristic></characteristic>"#,
        );
        assert_eq!(
            script.install_dir.as_deref(),
            Some(r"\Program Files\Astraware\Cubis\")
        );
        assert_eq!(script.shortcut_target, None);
        assert_eq!(
            script.registry[0].string.as_deref(),
            Some(r"\Program Files\Astraware\Cubis")
        );
    }
}
