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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
                script.install_dir = Some(canonicalise_install_dir(&name));
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
        let mut current_long: Option<String> = None;
        for raw in s.split(['\n', '>']) {
            let line = raw.trim();
            if let Some(t) = type_attribute(line) {
                let is_dir = t.starts_with('%') || t.contains('\\');
                if is_dir {
                    let dir = canonicalise_install_dir(&t);
                    if !script.install_dirs.contains(&dir) {
                        script.install_dirs.push(dir);
                    }
                } else if !structural.contains(&t.as_str()) {
                    current_long = Some(t);
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
                if current_key.is_some() {
                    current_key = None;
                } else {
                    in_registry = false;
                }
                continue;
            }
            if let Some(t) = tag.strip_prefix("characteristic").and_then(type_attribute) {
                if t.eq_ignore_ascii_case("Registry") {
                    in_registry = true;
                    current_key = None;
                } else if in_registry {
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
    fn parse_empty_header() {
        let h = WinCeInstallHeader::parse_bytes(&[]).unwrap();
        assert!(h.app_name.is_none());
        assert!(h.files.is_empty());
    }
}
