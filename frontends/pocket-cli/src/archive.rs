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
    /// Guest path the installer would have written the executable to,
    /// e.g. `\Program Files\Games\SkyForce Reloaded\SkyForceReloaded.exe`.
    ///
    /// Games commonly rebuild their asset paths from
    /// `GetModuleFileNameW` by subtracting the length of a hard-coded
    /// `L"<Game>.exe"` literal, so the reported module path has to have
    /// the real file name — a generic placeholder truncates the
    /// directory mid-component.
    pub guest_exe_path: Option<String>,
    /// Registry values the cabinet's `_setup.xml` installs.
    ///
    /// A Pocket PC installer writes these before the game ever runs, and
    /// titles read them back to find their own data: Astraware Bejeweled
    /// looks up `HKLM\SOFTWARE\Apps\Astraware Bejeweled\SaveDir` and
    /// calls `ExitProcess(0x42)` when the value is missing.
    pub registry: Vec<pocket_core::cab::SetupRegistryValue>,
    /// Guest directory used by the game for persistent data, when the
    /// cabinet records a `SaveDir` registry value.
    pub save_prefix: Option<String>,
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
        ArchiveKind::InstallShieldSfx => prepare_installshield_sfx(path),
        ArchiveKind::Pe => Ok(Launcher {
            exe: path.to_path_buf(),
            mount_dir: None,
            extra_mounts: Vec::new(),
            guest_exe_path: None,
            registry: Vec::new(),
            save_prefix: None,
            origin: format!("PE file {}", path.display()),
            _tempdir: None,
        }),
    }
}

#[derive(Debug, Clone, Copy)]
enum ArchiveKind {
    Cab,
    Zip,
    InstallShieldSfx,
    Pe,
}

impl ArchiveKind {
    fn detect(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(ext.as_deref(), Some("exe")) && is_installshield_sfx(path) {
            return Self::InstallShieldSfx;
        }
        match ext.as_deref() {
            Some("cab") => Self::Cab,
            Some("zip") => Self::Zip,
            _ => Self::Pe,
        }
    }
}

fn is_installshield_sfx(path: &Path) -> bool {
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    data.windows(8).any(|window| window == b"_winzip_")
        && data.windows(4).any(|window| window == b"\x13\x5d\x65\x8c")
}

fn prepare_installshield_sfx(path: &Path) -> Result<Launcher> {
    let tmp = TempDir::with_prefix("pockethle-sfx-")
        .with_context(|| format!("creating temp dir for {}", path.display()))?;
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut outer = zip::ZipArchive::new(file)
        .with_context(|| format!("reading WinZip self-extractor {}", path.display()))?;
    let mut data_z_path = None;
    for index in 0..outer.len() {
        let mut entry = outer.by_index(index)?;
        let Some(name) = entry.enclosed_name().map(Path::to_path_buf) else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        let destination = tmp.path().join(&name);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&destination)?;
        std::io::copy(&mut entry, &mut output)?;
        if name
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("data.z"))
        {
            data_z_path = Some(destination);
        }
    }
    let data_z_path =
        data_z_path.ok_or_else(|| anyhow!("{} has no data.z payload", path.display()))?;
    let mut archive = unshield::Archive::new(File::open(&data_z_path)?)
        .with_context(|| format!("opening InstallShield data.z from {}", path.display()))?;
    let cab_name = archive
        .list()
        .map(|entry| entry.path.clone())
        .find(|name| name.to_ascii_lowercase().ends_with("pacman.ppc_arm.cab"))
        .or_else(|| {
            archive
                .list()
                .map(|entry| entry.path.clone())
                .find(|name| name.to_ascii_lowercase().ends_with("_arm.cab"))
        })
        .ok_or_else(|| {
            anyhow!(
                "{} contains no ARM Windows Mobile CAB",
                data_z_path.display()
            )
        })?;
    let cab_bytes = archive
        .load(&cab_name)
        .with_context(|| format!("extracting {cab_name} from data.z"))?;
    let cab_path = tmp.path().join("pacman.PPC_ARM.CAB");
    std::fs::write(&cab_path, cab_bytes)?;
    let mut launcher = prepare_cab(&cab_path)?;
    launcher.origin = format!(
        "InstallShield SFX {} -> {}",
        path.display(),
        launcher.origin
    );
    launcher._tempdir = Some(merge_tempdirs(tmp, launcher._tempdir));
    Ok(launcher)
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
    // `ZumaPPC_VS2008.exe`). Parse `_setup.xml` (or the binary `.000`
    // install header) and materialise the long-name copies under the
    // same temp dir so a single mount answers both shapes.
    let setup = parse_setup_script(&files);
    materialise_long_names(tmp.path(), &files, &setup);

    // A `.000` header that parsed as a real MSCE file names every
    // payload exactly, so the reconstruct-by-guesswork paths below only
    // add wrong names. They stay for headers we could not parse.
    let structured = header.as_ref().is_some_and(|h| h.structured);
    if structured {
        if let Some(h) = header.as_ref() {
            pocket_core::cab::materialise_install_header_names(tmp.path(), &files, h);
        }
    } else {
        materialise_legacy_names(tmp.path(), &files, header.as_ref());
        if setup.is_none() {
            materialise_legacy_install_names(tmp.path(), &files, header.as_ref());
            materialise_legacy_install_files(tmp.path(), &files, header.as_ref());
        }
    }

    let exe_path = match find_main_exe(&files, &setup, header.as_ref()) {
        Some(p) => p,
        None => pick_entrypoint_pe(files.iter().map(|f| f.extracted_path.as_path()))
            .with_context(|| format!("looking for a launchable PE inside {}", path.display()))?,
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

    let guest_exe_path = guest_exe_path(&exe_path, setup.as_ref(), header.as_ref());
    // Both install formats write registry values the game reads back on
    // startup; take whichever one this cabinet carries.
    let registry = match setup.as_ref() {
        Some(script) => script.registry.clone(),
        None => header
            .as_ref()
            .map(|h| h.registry.clone())
            .unwrap_or_default(),
    };
    let save_prefix = registry
        .iter()
        .find(|value| value.name.eq_ignore_ascii_case("SaveDir"))
        .and_then(|value| value.string.clone());

    Ok(Launcher {
        exe: exe_path,
        mount_dir: Some(tmp.path().to_path_buf()),
        extra_mounts,
        guest_exe_path,
        registry,
        save_prefix,
        origin,
        _tempdir: Some(tmp),
    })
}

/// Reconstruct the on-device path of the executable we are about to
/// run: `<install dir>\<long exe name>`.
///
/// The long name comes from the materialised file we picked (the
/// long-name copies are written with `\` replaced by `_`), the
/// directory from the `_setup.xml` shortcut target when it names one,
/// otherwise from the install directories the script declares.
fn guest_exe_path(
    exe_path: &Path,
    setup: Option<&pocket_core::cab::WinCeSetupScript>,
    header: Option<&pocket_core::cab::WinCeInstallHeader>,
) -> Option<String> {
    let name = exe_path.file_name()?.to_str()?.to_string();

    if let Some(setup) = setup {
        // A shortcut target is already a full guest path; trust it when
        // it points at the binary we chose.
        if let Some(target) = &setup.shortcut_target {
            let target_name = target.rsplit(['\\', '/']).next().unwrap_or(target);
            if target_name.eq_ignore_ascii_case(&name) {
                return Some(target.clone());
            }
        }
        let dir = setup
            .install_dirs
            .iter()
            .chain(setup.install_dir.iter())
            .find(|dir| dir.len() > 1)?;
        return Some(format!("{}{}", dir, name));
    }

    // Legacy `.000` cabs: the install records already carry the full
    // on-device destination of every payload.
    let header = header?;
    let matches_name = |dest: &str| {
        dest.rsplit(['\\', '/'])
            .next()
            .is_some_and(|leaf| leaf.eq_ignore_ascii_case(&name))
    };
    if let Some(target) = header
        .shortcut_target
        .as_deref()
        .filter(|t| matches_name(t))
    {
        return Some(target.to_string());
    }
    if let Some(dest) = header
        .files
        .iter()
        .map(|entry| entry.destination.as_str())
        .find(|dest| matches_name(dest))
    {
        return Some(dest.to_string());
    }
    let dir = header.install_dir.as_ref()?;
    Some(format!("{dir}{name}"))
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
    let install_root = setup.install_root();
    for (short, long) in &setup.renames {
        let source_suffix = short.rsplit('.').next().unwrap_or(short);
        let Some(src) = files
            .iter()
            .find(|file| {
                file.short_name.eq_ignore_ascii_case(short)
                    || file
                        .short_name
                        .rsplit('.')
                        .next()
                        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(source_suffix))
            })
            .map(|file| file.extracted_path.as_path())
        else {
            log::debug!("setup.xml mentions {short} but cab has no such file; skipping");
            continue;
        };
        // `long` is now a full guest path; extract the relative tail
        // so the directory hierarchy survives extraction.
        let Some(relative) = setup.relative_destination(long, install_root.as_deref()) else {
            continue;
        };
        let dest = relative
            .split('\\')
            .filter(|s| !s.is_empty())
            .fold(root.to_path_buf(), |acc, seg| acc.join(seg));
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
            log::debug!("materialised {} as {}", short, dest.display());
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
        log::debug!(
            "legacy CAB destination {} source {}",
            entry.destination,
            entry.source
        );
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
            match std::fs::copy(&src.extracted_path, &dest) {
                Ok(_) => log::debug!(
                    "legacy CAB copied {} exists={}",
                    dest.display(),
                    dest.exists()
                ),
                Err(error) => log::debug!(
                    "legacy CAB copy {} -> {} failed: {error}",
                    src.short_name,
                    dest.display()
                ),
            }
        }
        let basename = relative.replace(['\\', '/'], std::path::MAIN_SEPARATOR_STR);
        let basename = Path::new(&basename).file_name().map(|name| name.to_owned());
        if let Some(basename) = basename {
            let alias = root.join(basename);
            if alias != src.extracted_path && alias != dest {
                match std::fs::copy(&src.extracted_path, &alias) {
                    Ok(_) => log::debug!(
                        "legacy CAB root alias {} -> {}",
                        src.short_name,
                        alias.display()
                    ),
                    Err(error) => {
                        log::debug!("legacy CAB root alias {} failed: {error}", src.short_name)
                    }
                }
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
    header: Option<&pocket_core::cab::WinCeInstallHeader>,
) -> Option<PathBuf> {
    let parent = files.first()?.extracted_path.parent()?.to_path_buf();
    let Some(setup) = setup.as_ref() else {
        // Ancient `.000`-header cabs (Rayman Ultimate, SkyForce
        // Reloaded, JumpyBall) have no `_setup.xml`. Their install
        // records still name every payload, and the long-name copies
        // are already on disk, so resolve them through the header.
        // Loading the long-name copy (rather than the `00RAYMA~1.001`
        // short name) is what lets `GetModuleFileNameW` report a path
        // the game recognises.
        let header = header?;

        // The shortcut the installer would have put on the Start menu
        // names the binary the user actually launches. Trust it first:
        // cabs regularly ship helper executables next to the game.
        if let Some(target) = &header.shortcut_target {
            if target.to_ascii_lowercase().ends_with(".exe") {
                if let Some(path) = header
                    .host_path(&parent, target)
                    .filter(|p| is_arm_pe(p).unwrap_or(false))
                {
                    return Some(path);
                }
            }
        }

        return header
            .files
            .iter()
            .filter(|entry| entry.destination.to_ascii_lowercase().ends_with(".exe"))
            .filter_map(|entry| header.host_path(&parent, &entry.destination))
            .filter(|path| is_arm_pe(path).unwrap_or(false))
            .max_by_key(|path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
    };
    let install_root = setup.install_root();
    // `renames` now carries full guest paths, so the on-disk copy sits
    // at the same relative offset `materialise_long_names` wrote it to.
    let materialised = |long: &str| -> Option<PathBuf> {
        let relative = setup.relative_destination(long, install_root.as_deref())?;
        let candidate = relative
            .split('\\')
            .filter(|s| !s.is_empty())
            .fold(parent.to_path_buf(), |acc, seg| acc.join(seg));
        is_entrypoint_candidate(&candidate).then_some(candidate)
    };

    // The Start-menu shortcut names the executable the user launches.
    // Trust it before anything else: cabs often install helper
    // binaries (Sonic Unleashed ships a `GetRealDPI.exe` probe) that
    // are perfectly valid ARM PEs but exit immediately.
    if let Some(target) = &setup.shortcut_target {
        if target.to_ascii_lowercase().ends_with(".exe") {
            if let Some(path) = materialised(target) {
                return Some(path);
            }
        }
    }

    // Otherwise take the biggest `.exe` the script installs — helper
    // probes are tiny next to a real game binary.
    setup
        .renames
        .iter()
        .filter(|(_short, long)| long.to_ascii_lowercase().ends_with(".exe"))
        .filter_map(|(_short, long)| materialised(long))
        .max_by_key(|path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
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
        // Every directory `_setup.xml` installs into, plus the
        // declared `InstallDir`. The two frequently differ (Sonic
        // Unleashed: `InstallDir = \Program Files\SONIC` but the files
        // land in `\Program Files\Gameloft\SONIC`, which is the path
        // the game hard-codes for `data.bar`), so mount both.
        for dir in s
            .install_dirs
            .iter()
            .chain(s.install_dir.iter())
            .filter(|dir| dir.len() > 1)
        {
            if !out
                .iter()
                .any(|(prefix, _)| prefix.eq_ignore_ascii_case(dir))
            {
                out.push((dir.clone(), root.to_path_buf()));
            }
        }
    }
    out
}

pub fn save_id(path: &Path) -> String {
    let raw = path
        .file_stem()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '_' | '-' | '.') {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('.').trim_matches('_').trim_matches('-');
    if trimmed.is_empty() {
        "game".to_string()
    } else {
        trimmed.to_string()
    }
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

    let exe_path = pick_entrypoint_pe(written.iter().map(PathBuf::as_path)).with_context(|| {
        format!(
            "no launchable PE found in {}: {} contains no supported game entry point",
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
        guest_exe_path: None,
        registry: Vec::new(),
        save_prefix: None,
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

/// Walk `paths` and return the largest PE that PocketHLE can identify as
/// a process entry point. Native ARM/MIPS images are handled by the HLE;
/// managed WinCE images are retained so the loader can report the missing
/// .NET Compact Framework runtime instead of claiming the cabinet is empty.
fn pick_entrypoint_pe<'a, I>(paths: I) -> Result<PathBuf>
where
    I: IntoIterator<Item = &'a Path>,
{
    let mut candidates: Vec<(u64, PathBuf)> = Vec::new();
    for p in paths {
        let Ok(meta) = std::fs::metadata(p) else {
            continue;
        };
        if !meta.is_file() || !is_entrypoint_candidate(p) {
            continue;
        }
        candidates.push((meta.len(), p.to_path_buf()));
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
    candidates
        .into_iter()
        .next()
        .map(|(_, p)| p)
        .ok_or_else(|| anyhow!("no launchable PE executable found"))
}

/// Cheap check for the PE/COFF header: read 0x40 bytes, follow the
/// `e_lfanew` offset, verify `PE\0\0` and read the machine type.
/// Returns `Ok(false)` for short reads or non-PE files (so we skip
/// them silently rather than failing the whole launch).
fn pe_header_fields(path: &Path) -> std::io::Result<Option<(u16, u16)>> {
    let mut f = File::open(path)?;
    let mut head = [0u8; 0x40];
    if f.read(&mut head)? < head.len() || &head[0..2] != b"MZ" {
        return Ok(None);
    }
    let lfanew = u32::from_le_bytes(head[0x3c..0x40].try_into().unwrap()) as u64;
    use std::io::{Seek, SeekFrom};
    f.seek(SeekFrom::Start(lfanew))?;
    let mut coff = [0u8; 24];
    if f.read(&mut coff)? < coff.len() || &coff[0..4] != b"PE\0\0" {
        return Ok(None);
    }
    Ok(Some((
        u16::from_le_bytes([coff[4], coff[5]]),
        u16::from_le_bytes([coff[22], coff[23]]),
    )))
}

fn is_arm_pe(path: &Path) -> std::io::Result<bool> {
    Ok(pe_header_fields(path)?.is_some_and(|(machine, _)| is_supported_guest_machine(machine)))
}

fn is_entrypoint_candidate(path: &Path) -> bool {
    let Ok(image) = pocket_core::pe::load_file(path) else {
        return false;
    };
    (is_supported_guest_machine(image.machine) || image.managed_runtime.is_some())
        && !is_guest_dll(path)
}

fn is_guest_dll(path: &Path) -> bool {
    pe_header_fields(path)
        .ok()
        .flatten()
        .is_some_and(|(_, characteristics)| characteristics & 0x2000 != 0)
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
        buf.resize(0x98, 0);
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
