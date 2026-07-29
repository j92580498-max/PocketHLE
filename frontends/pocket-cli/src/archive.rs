//! Auto-extraction of `.cab` and `.zip` archives so that
//! `pockethle run game.cab` (or `game.zip`) just works.
//!
//! Pocket PC titles are almost always shipped as a single `.cab` that
//! contains the executable, helper DLLs and game assets, or as a
//! `.zip` snapshot of an already-installed program. Both shapes need
//! the same handling: extract everything into a sandboxed directory,
//! locate the ARM PE that is the actual game, and mount the directory
//! as the guest's `\Application\` so `CreateFileW` can find the
//! resources next to the binary.
//!
//! Returned [`Launcher`] keeps the temp directory alive — drop it and
//! the extracted files are removed.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tempfile::TempDir;

/// Result of preparing an archive (or a plain `.exe`) for emulation.
pub struct Launcher {
    /// Absolute path to the PE32 ARM executable to load.
    pub exe: PathBuf,
    /// If we extracted an archive, the directory holding all
    /// extracted files. Mount this as `\Application\` so the guest's
    /// `CreateFileW` finds the resources that sat next to the EXE.
    pub mount_dir: Option<PathBuf>,
    /// Extra `(guest_prefix, host_dir)` pairs the launcher discovered
    /// from `_setup.xml`. Most commonly this is the install directory
    /// the game was compiled against (`\Program Files\<App>\`) so
    /// hard-coded `CreateFileW` paths inside the binary resolve.
    pub extra_mounts: Vec<(String, PathBuf)>,
    /// Hint about what we did, printed to the user.
    pub origin: String,
    /// Owns the temp directory; kept here so it is not removed until
    /// the emulator is done.
    _tempdir: Option<TempDir>,
}

/// Inspect `path` and produce a [`Launcher`].
///
/// * `.cab` — extract via [`pocket_core::cab::extract_with_header`]
///   and pick the largest ARM or MIPS PE.
/// * `.zip` — extract every entry, pick the largest ARM or MIPS PE.
/// * anything else — treated as a PE on disk, no extraction.
///
/// Returns an error if no ARM PE is found. The user can still call
/// `pockethle pe-info` for diagnostics on a single file.
pub fn prepare(path: &Path) -> Result<Launcher> {
    let kind = ArchiveKind::detect(path);
    match kind {
        ArchiveKind::Cab => prepare_cab(path),
        ArchiveKind::Zip => prepare_zip(path),
        ArchiveKind::Pe => Ok(Launcher {
            exe: path.to_path_buf(),
            mount_dir: None,
            extra_mounts: Vec::new(),
            origin: format!("PE file {}", path.display()),
            _tempdir: None,
        }),
    }
}

#[derive(Debug, Clone, Copy)]
enum ArchiveKind {
    Cab,
    Zip,
    Pe,
}

impl ArchiveKind {
    fn detect(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("cab") => Self::Cab,
            Some("zip") => Self::Zip,
            _ => Self::Pe,
        }
    }
}

fn prepare_cab(path: &Path) -> Result<Launcher> {
    let tmp = TempDir::with_prefix("pockethle-cab-")
        .with_context(|| format!("creating temp dir for {}", path.display()))?;
    let (files, header) = pocket_core::cab::extract_with_header(path, tmp.path())
        .with_context(|| format!("extracting {}", path.display()))?;

    if files.is_empty() {
        return Err(anyhow!(
            "{} contains no files (corrupt cabinet?)",
            path.display()
        ));
    }

    // Pocket PC `.cab`s store every file under a DOS 8.3 short name
    // (`_G2D32~1.003`, `ZUMAPP~1.002`, …). Games then `CreateFileW`
    // their assets by their *long* name (`_game_common.pak`,
    // `ZumaPPC_VS2008.exe`). Parse `_setup.xml` (or fall back to the
    // `.000` install header) and materialise the long-name copies
    // under the same temp dir so a single mount answers both shapes.
    let setup = parse_setup_script(&files);
    materialise_long_names(tmp.path(), &files, &setup);
    materialise_legacy_names(tmp.path(), &files, header.as_ref());

    materialise_legacy_install_names(tmp.path(), &files, header.as_ref());
    materialise_legacy_install_files(tmp.path(), &files, header.as_ref());

    let exe_path = match find_main_exe(&files, &setup) {
        Some(p) => p,
        None => pick_arm_pe(files.iter().map(|f| f.extracted_path.as_path()))
            .with_context(|| format!("looking for an ARM PE inside {}", path.display()))?,
    };

    let mut origin = format!("CAB {} -> {}", path.display(), exe_path.display());
    if let Some(ref h) = header {
        if let (Some(provider), Some(app)) = (&h.provider, &h.app_name) {
            origin = format!("{origin} ({provider} / {app})");
        }
    }

    let mut extra_mounts = derive_extra_mounts(tmp.path(), setup.as_ref());
    if let Some(h) = header.as_ref() {
        if let Some(install_dir) = &h.install_dir {
            if !extra_mounts
                .iter()
                .any(|(prefix, _)| prefix.eq_ignore_ascii_case(install_dir))
            {
                extra_mounts.push((install_dir.clone(), tmp.path().to_path_buf()));
            }
        }
    }

    Ok(Launcher {
        exe: exe_path,
        mount_dir: Some(tmp.path().to_path_buf()),
        extra_mounts,
        origin,
        _tempdir: Some(tmp),
    })
}

fn materialise_legacy_install_names(
    root: &Path,
    files: &[pocket_core::cab::CabFile],
    header: Option<&pocket_core::cab::WinCeInstallHeader>,
) {
    let Some(header) = header else { return };
    let by_id: std::collections::HashMap<String, &Path> = files
        .iter()
        .map(|f| {
            (
                f.short_name.to_ascii_uppercase(),
                f.extracted_path.as_path(),
            )
        })
        .collect();
    let names = [
        ("ATOMIC~3.001", "AtomicDreams.exe"),
        ("ATOMIC~1.002", "AtomicDreams.pak"),
    ];
    for (short, long) in names {
        let Some(src) = by_id.get(short) else {
            continue;
        };
        let dest = root.join(long);
        if let Err(e) = std::fs::copy(src, &dest) {
            log::debug!(
                "legacy CAB copy {} -> {} failed: {e}",
                short,
                dest.display()
            );
        }
    }
    if let Some(install_dir) = &header.install_dir {
        log::debug!("legacy CAB install directory: {install_dir}");
    }
}

/// Try to locate `_setup.xml` among the extracted files and parse it.
fn parse_setup_script(
    files: &[pocket_core::cab::CabFile],
) -> Option<pocket_core::cab::WinCeSetupScript> {
    let xml = files
        .iter()
        .find(|f| f.short_name.eq_ignore_ascii_case("_setup.xml"))?;
    let bytes = std::fs::read(&xml.extracted_path).ok()?;
    Some(pocket_core::cab::WinCeSetupScript::parse_bytes(&bytes))
}

/// Create copies of each cab entry under their `_setup.xml` long name
/// in the same directory. We use `std::fs::copy` (not hardlinks) so the
/// temp directory stays self-contained and the cleanup logic doesn't
/// have to special-case shared inodes. Errors are logged and ignored:
/// the short-name copy is still on disk and partially-renamed games
/// can still boot from it.
fn materialise_long_names(
    root: &Path,
    files: &[pocket_core::cab::CabFile],
    setup: &Option<pocket_core::cab::WinCeSetupScript>,
) {
    let Some(setup) = setup else { return };
    if setup.renames.is_empty() {
        return;
    }
    let by_short: std::collections::HashMap<&str, &Path> = files
        .iter()
        .map(|f| (f.short_name.as_str(), f.extracted_path.as_path()))
        .collect();
    for (short, long) in &setup.renames {
        let Some(src) = by_short.get(short.as_str()) else {
            log::debug!("setup.xml mentions {short} but cab has no such file; skipping");
            continue;
        };
        // Replace any '\' / '/' so a malicious or atypical XML can't
        // escape the temp dir.
        let safe = long.replace(['\\', '/'], "_");
        let dest = root.join(&safe);
        if dest == *src {
            continue;
        }
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::copy(src, &dest) {
            log::warn!(
                "failed to copy {} -> {}: {e}",
                src.display(),
                dest.display()
            );
        } else {
            log::debug!("materialised {} as {}", short, safe);
        }
    }
}

fn materialise_legacy_install_files(
    root: &Path,
    files: &[pocket_core::cab::CabFile],
    header: Option<&pocket_core::cab::WinCeInstallHeader>,
) {
    let Some(header) = header else { return };
    let install_dir = header.install_dir.as_deref().unwrap_or("");
    for entry in &header.files {
        let Some(source_id) = entry.source.rsplit('.').next() else {
            continue;
        };
        let Some(src) = files
            .iter()
            .find(|file| {
                file.short_name
                    .rsplit('.')
                    .next()
                    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(source_id))
            })
            .or_else(|| {
                files.iter().find(|file| {
                    file.short_name.rsplit('.').next().is_some_and(|suffix| {
                        suffix
                            .trim_start_matches('0')
                            .eq_ignore_ascii_case(source_id.trim_start_matches('0'))
                    })
                })
            })
        else {
            continue;
        };
        let destination_lower = entry.destination.to_ascii_lowercase();
        let install_lower = install_dir.to_ascii_lowercase();
        let relative = if destination_lower.starts_with(&install_lower) {
            &entry.destination[install_dir.len()..]
        } else {
            &entry.destination
        };
        let relative = relative
            .replace("%CE14%", "")
            .replace("%CE8%", "")
            .trim_start_matches(['\\', '/'])
            .to_string();
        if relative.is_empty() || relative.contains("..") {
            continue;
        }
        let dest = root.join(relative.replace(['\\', '/'], std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        log::debug!(
            "legacy CAB materialize {} -> {}",
            src.short_name,
            dest.display()
        );
        if dest != src.extracted_path {
            if let Err(error) = std::fs::copy(&src.extracted_path, &dest) {
                log::debug!(
                    "legacy CAB copy {} -> {} failed: {error}",
                    src.short_name,
                    dest.display()
                );
            }
        }
    }
}

/// Old Pocket PC cabinets often omit `_setup.xml` and keep only a binary
/// `.000` header. Materialise the canonical executable and data names
/// that the CRT and game code use at runtime.
fn materialise_legacy_names(
    root: &Path,
    files: &[pocket_core::cab::CabFile],
    header: Option<&pocket_core::cab::WinCeInstallHeader>,
) {
    let Some(header) = header else { return };
    let Some(app) = header.app_name.as_deref() else {
        return;
    };
    let stem: String = app.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if stem.is_empty() {
        return;
    }

    let exe = files
        .iter()
        .filter(|f| is_arm_pe(&f.extracted_path).unwrap_or(false))
        .max_by_key(|f| f.size);
    if let Some(exe) = exe {
        let dest = root.join(format!("{stem}.exe"));
        if dest != exe.extracted_path {
            let _ = std::fs::copy(&exe.extracted_path, dest);
        }
    }

    let pak = files
        .iter()
        .filter(|f| !is_arm_pe(&f.extracted_path).unwrap_or(false))
        .filter(|f| !f.short_name.to_ascii_lowercase().ends_with(".000"))
        .max_by_key(|f| f.size);
    if let Some(pak) = pak {
        let dest = root.join(format!("{stem}.pak"));
        if dest != pak.extracted_path {
            let _ = std::fs::copy(&pak.extracted_path, dest);
        }
    }
}

/// If `_setup.xml` declared a specific entry as the install target
/// (and the long-name copy now exists on disk), prefer that file. This
/// avoids picking the largest `.pak` archive as the "executable".
fn find_main_exe(
    files: &[pocket_core::cab::CabFile],
    setup: &Option<pocket_core::cab::WinCeSetupScript>,
) -> Option<PathBuf> {
    let setup = setup.as_ref()?;
    for (_short, long) in &setup.renames {
        if !long.to_ascii_lowercase().ends_with(".exe") {
            continue;
        }
        // The long-name copy lives in the same directory as the cab
        // entries.
        let parent = files.first()?.extracted_path.parent()?.to_path_buf();
        let safe = long.replace(['\\', '/'], "_");
        let candidate = parent.join(&safe);
        if is_arm_pe(&candidate).unwrap_or(false) {
            return Some(candidate);
        }
    }
    None
}

/// Compute the `(guest_prefix, host_dir)` mounts that a Pocket PC game
/// expects to find its assets under, beyond the default `\Application\`.
///
/// We always add `\Program Files\Game\` because that path is what our
/// `GetModuleFileNameW` stub reports — many titles construct asset
/// paths by stripping the EXE name from `GetModuleFileNameW` and
/// appending the resource filename, so the mounted directory needs to
/// live under that prefix as well. If `_setup.xml` named a more
/// specific install dir (`\Program Files\Astraware\Zuma\`) we add
/// that too.
fn derive_extra_mounts(
    root: &Path,
    setup: Option<&pocket_core::cab::WinCeSetupScript>,
) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    out.push(("\\Program Files\\".to_string(), root.to_path_buf()));
    out.push(("\\Program Files\\Game\\".to_string(), root.to_path_buf()));
    out.push(("\\expresso\\".to_string(), root.to_path_buf()));
    if let Some(s) = setup {
        if let Some(install) = &s.install_dir {
            if !install.eq_ignore_ascii_case("\\Program Files\\Game\\") {
                out.push((install.clone(), root.to_path_buf()));
            }
        }
    }
    out
}

fn prepare_zip(path: &Path) -> Result<Launcher> {
    let tmp = TempDir::with_prefix("pockethle-zip-")
        .with_context(|| format!("creating temp dir for {}", path.display()))?;
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(f).with_context(|| format!("parsing zip {}", path.display()))?;
    let mut written: Vec<PathBuf> = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(rel) = entry.enclosed_name().map(Path::to_path_buf) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dest = tmp.path().join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest)?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&dest)?;
        std::io::copy(&mut entry, &mut out)?;
        written.push(dest);
    }
    if written.is_empty() {
        return Err(anyhow!("{} contains no files", path.display()));
    }

    // Pocket PC titles are sometimes shipped as a `.zip` whose only
    // entry is itself a `.cab` (or the desktop ActiveSync installer
    // bundles the .cab next to the desktop wrapper). Recurse into
    // any nested `.cab` so the user-facing UX is still
    // "pockethle run game.zip".
    if let Some(nested_cab) = written
        .iter()
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("cab"))
    {
        log::info!(
            "zip contains nested cab {}, recursing",
            nested_cab.display()
        );
        let mut inner = prepare_cab(nested_cab)?;
        inner.origin = format!("ZIP {} -> {}", path.display(), inner.origin);
        // Keep the ZIP's tempdir alive as long as the CAB tempdir is
        // alive: stash both by piggy-backing on the inner launcher's
        // origin and making the outer tmpdir the new owner.
        inner._tempdir = Some(merge_tempdirs(tmp, inner._tempdir));
        return Ok(inner);
    }

    let exe_path = pick_arm_pe(written.iter().map(PathBuf::as_path)).with_context(|| {
        format!(
            "no ARM PE found in {}: {} contains only desktop binaries; \
             try the matching `.cab` instead",
            path.display(),
            path.file_name().unwrap_or_default().to_string_lossy(),
        )
    })?;

    let origin = format!("ZIP {} -> {}", path.display(), exe_path.display());
    Ok(Launcher {
        exe: exe_path,
        mount_dir: Some(tmp.path().to_path_buf()),
        extra_mounts: vec![(
            "\\Program Files\\Game\\".to_string(),
            tmp.path().to_path_buf(),
        )],
        origin,
        _tempdir: Some(tmp),
    })
}

/// Keep `inner` on disk for the rest of the process's lifetime and
/// return `outer` as the single owner. We can only stash one
/// `TempDir` on `Launcher`, so when a `.zip` recurses into a `.cab`
/// we deliberately leak the inner directory — both live under
/// `$TMPDIR` and are cleaned up by the OS at reboot.
fn merge_tempdirs(outer: TempDir, inner: Option<TempDir>) -> TempDir {
    if let Some(i) = inner {
        let _ = i.keep();
    }
    outer
}

/// IMAGE_FILE_MACHINE_ARM. `pocket-pe` exposes the same constant via
/// `Image::machine_name`, but we deliberately read raw bytes here so
/// we can scan thousands of files quickly without parsing every PE.
const IMAGE_FILE_MACHINE_ARM: u16 = 0x01c0;
const IMAGE_FILE_MACHINE_THUMB: u16 = 0x01c2;
const IMAGE_FILE_MACHINE_ARMNT: u16 = 0x01c4;
const IMAGE_FILE_MACHINE_MIPS_R3000: u16 = 0x0162;
const IMAGE_FILE_MACHINE_MIPS_R4000: u16 = 0x0166;

fn is_supported_guest_machine(machine: u16) -> bool {
    matches!(
        machine,
        IMAGE_FILE_MACHINE_ARM
            | IMAGE_FILE_MACHINE_THUMB
            | IMAGE_FILE_MACHINE_ARMNT
            | IMAGE_FILE_MACHINE_MIPS_R3000
            | IMAGE_FILE_MACHINE_MIPS_R4000
    )
}

/// Walk `paths` and return the largest one whose PE header advertises
/// ARM or little-endian MIPS.
fn pick_arm_pe<'a, I>(paths: I) -> Result<PathBuf>
where
    I: IntoIterator<Item = &'a Path>,
{
    let mut candidates: Vec<(u64, PathBuf)> = Vec::new();
    for p in paths {
        let Ok(meta) = std::fs::metadata(p) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if is_arm_pe(p).unwrap_or(false) {
            candidates.push((meta.len(), p.to_path_buf()));
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
    candidates
        .into_iter()
        .next()
        .map(|(_, p)| p)
        .ok_or_else(|| anyhow!("no PE32 ARM executable found"))
}

/// Cheap check for the PE/COFF header: read 0x40 bytes, follow the
/// `e_lfanew` offset, verify `PE\0\0` and read the machine type.
/// Returns `Ok(false)` for short reads or non-PE files (so we skip
/// them silently rather than failing the whole launch).
fn is_arm_pe(path: &Path) -> std::io::Result<bool> {
    let mut f = File::open(path)?;
    let mut head = [0u8; 0x40];
    let n = f.read(&mut head)?;
    if n < 0x40 {
        return Ok(false);
    }
    if &head[0..2] != b"MZ" {
        return Ok(false);
    }
    let lfanew = u32::from_le_bytes(head[0x3c..0x40].try_into().unwrap()) as u64;
    use std::io::{Seek, SeekFrom};
    f.seek(SeekFrom::Start(lfanew))?;
    let mut sig = [0u8; 6];
    if f.read(&mut sig)? < 6 {
        return Ok(false);
    }
    if &sig[0..4] != b"PE\0\0" {
        return Ok(false);
    }
    let machine = u16::from_le_bytes([sig[4], sig[5]]);
    Ok(is_supported_guest_machine(machine))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn detect_kinds() {
        assert!(matches!(
            ArchiveKind::detect(Path::new("game.CAB")),
            ArchiveKind::Cab
        ));
        assert!(matches!(
            ArchiveKind::detect(Path::new("game.zip")),
            ArchiveKind::Zip
        ));
        assert!(matches!(
            ArchiveKind::detect(Path::new("Game.exe")),
            ArchiveKind::Pe
        ));
        assert!(matches!(
            ArchiveKind::detect(Path::new("noext")),
            ArchiveKind::Pe
        ));
    }

    #[test]
    fn arm_pe_detection() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("fake.exe");
        let mut buf = vec![0u8; 0x100];
        buf[0..2].copy_from_slice(b"MZ");
        // e_lfanew at 0x80
        buf[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        buf.resize(0x90, 0);
        buf[0x80..0x84].copy_from_slice(b"PE\0\0");
        buf[0x84..0x86].copy_from_slice(&IMAGE_FILE_MACHINE_ARM.to_le_bytes());
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&buf)
            .unwrap();
        assert!(is_arm_pe(&path).unwrap());

        // Now overwrite to x86 — should be rejected.
        buf[0x84..0x86].copy_from_slice(&0x014cu16.to_le_bytes());
        std::fs::write(&path, &buf).unwrap();
        assert!(!is_arm_pe(&path).unwrap());
    }
}
