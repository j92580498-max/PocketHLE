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

/// A tiny, format-tolerant reader for the WinCE install header (`.000`
/// file). The full format is described in the SDK header `cefiles.h`,
/// but for our purposes we only need a few fields:
///
/// * an offset table to the installer strings (app name, provider, etc.)
/// * a list of files referenced by short id (`.001`, `.002`, ...) along
///   with the install destination path on the device.
///
/// We expose only the safe, validated subset.
#[derive(Debug, Clone, Default)]
pub struct WinCeInstallHeader {
    pub app_name: Option<String>,
    pub provider: Option<String>,
    pub files: Vec<WinCeInstallFile>,
    pub install_dir: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WinCeInstallFile {
    /// Source short name inside the cab, e.g. `JUMPYB~1.002`.
    pub source: String,
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
        // The header always starts with the magic 'MSCE' (0x4543534D LE)
        // followed by a series of word-aligned offset tables. Different
        // Pocket PC versions emit different fields, so we just scan for
        // printable UTF-16LE strings and keep the first two as the
        // (provider, app_name) pair, which is the order Microsoft's
        // CabWiz uses.
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
        if let Some(path) = ascii_strings.iter().find(|s| s.starts_with("%CE")) {
            header.install_dir = Some(canonicalise_install_dir(path));
        }
        if header.provider.is_none() {
            header.provider = ascii_strings.first().cloned();
        }
        if header.app_name.is_none() {
            header.app_name = ascii_strings.get(1).cloned();
        }
        parse_legacy_install_records(data, &mut header);
        Ok(header)
    }
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
        let metadata = offset.saturating_add(name.len() + 1);
        if metadata + 4 > data.len() {
            continue;
        }
        let id = u16::from_le_bytes([data[metadata], data[metadata + 1]]);
        let length = u16::from_le_bytes([data[metadata + 2], data[metadata + 3]]) as usize;
        if length == name.len() && id != 0 {
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
        if sequence == 0 || length != name.len() {
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
            destination: format!("{install_dir}\\{relative}"),
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
                destination: format!("{install_dir}{name}"),
            });
        }
    }
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
                script.install_dir = Some(canonicalise_install_dir(&name));
            } else if let Some(name) = parm_value(line, "AppName") {
                script.app_name = Some(name);
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
        let mut current_long: Option<String> = None;
        for raw in s.split(['\n', '>']) {
            let line = raw.trim();
            if let Some(t) = type_attribute(line) {
                if !structural.contains(&t.as_str()) && !t.starts_with('%') {
                    current_long = Some(t);
                }
            }
            if let Some(short) = parm_value(line, "Source") {
                if let Some(long) = current_long.take() {
                    script.renames.push((short, long));
                }
            }
        }

        script
    }
}

/// `\Program Files\Astraware\Zuma` -> `\Program Files\Astraware\Zuma\`.
/// `%CE1%\Astraware\Zuma` -> `\Program Files\Astraware\Zuma\` (CE1 is
/// the `\Program Files\` macro on Windows Mobile 5/6).
fn canonicalise_install_dir(raw: &str) -> String {
    let mut s = raw.replace('/', "\\");
    for (macro_name, replacement) in [
        ("%CE1%", "\\Program Files"),
        ("%CE2%", "\\Windows"),
        ("%CE11%", "\\Start Menu\\Programs"),
    ] {
        s = s.replace(macro_name, replacement);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_traversal() {
        // Leading dots are trimmed, then path separators are
        // replaced with `_`.
        assert_eq!(sanitize_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_name("JUMPYB~1.002"), "JUMPYB~1.002");
        assert_eq!(sanitize_name("a\\b/c"), "a_b_c");
    }

    #[test]
    fn parse_empty_header() {
        let h = WinCeInstallHeader::parse_bytes(&[]).unwrap();
        assert!(h.app_name.is_none());
        assert!(h.files.is_empty());
    }
}
