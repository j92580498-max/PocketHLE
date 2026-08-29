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
    /// Screen geometry the recognised layout implies, when it identifies
    /// the device the game shipped on. `None` leaves the emulator's
    /// Pocket PC default alone.
    ///
    /// This is a fact about the hardware, not a preference: a GAPI or
    /// GL ES title reads the display size once at start-up and lays its
    /// whole scene out around it, so the geometry has to be right before
    /// the game runs rather than being something the user discovers.
    pub native_screen: Option<(u32, u32)>,
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
            native_screen: None,
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
        && data.windows(4).any(|window| window == b"PK\x03\x04")
}

fn is_windows_mobile_cab_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".ppc_arm.cab") || name.ends_with(".2577.cab") || name.ends_with("_arm.cab")
}

fn extract_installshield_sfx_cab(source: &Path, destination: &Path) -> Result<()> {
    let header = source.with_extension("hdr");
    let hdr = std::fs::read(&header)
        .with_context(|| format!("reading InstallShield header {}", header.display()))?;
    let cab = std::fs::read(source)
        .with_context(|| format!("reading InstallShield cabinet {}", source.display()))?;
    if hdr.len() < 0x200 || cab.len() < 0x3c || &cab[..4] != b"ISc(" {
        return Err(anyhow!("unsupported InstallShield cabinet layout"));
    }
    let header_descriptor_offset = u32::from_le_bytes(hdr[0x0c..0x10].try_into().unwrap()) as usize;
    let header_file_table_offset = u32::from_le_bytes(
        hdr[header_descriptor_offset + 0x0c..header_descriptor_offset + 0x10]
            .try_into()
            .unwrap(),
    ) as usize;
    let header_table = header_descriptor_offset
        .checked_add(header_file_table_offset)
        .ok_or_else(|| anyhow!("InstallShield header table overflow"))?;
    let read_u32 = |data: &[u8], offset: usize, label: &str| -> Result<usize> {
        let bytes = data
            .get(offset..offset + 4)
            .ok_or_else(|| anyhow!("truncated InstallShield {label}"))?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
    };
    let directory_count = read_u32(&hdr, header_descriptor_offset + 0x1c, "directory count")?;
    let file_count = read_u32(&hdr, header_descriptor_offset + 0x28, "file count")?;
    let table = header_table;
    let offsets_end = table
        .checked_add(
            directory_count
                .checked_add(file_count)
                .and_then(|count| count.checked_mul(4))
                .ok_or_else(|| anyhow!("InstallShield file table overflow"))?,
        )
        .ok_or_else(|| anyhow!("InstallShield file table overflow"))?;
    if offsets_end > hdr.len() || file_count == 0 || directory_count == 0 {
        return Err(anyhow!("invalid InstallShield file table"));
    }

    for index in 0..file_count {
        let offset = table
            .checked_add(
                directory_count
                    .checked_add(index)
                    .and_then(|value| value.checked_mul(4))
                    .ok_or_else(|| anyhow!("InstallShield file table overflow"))?,
            )
            .ok_or_else(|| anyhow!("InstallShield file table overflow"))?;
        let file_offset = read_u32(&hdr, offset, "file descriptor offset")?;
        let descriptor = header_table
            .checked_add(file_offset)
            .ok_or_else(|| anyhow!("InstallShield file descriptor overflow"))?;
        let descriptor_end = descriptor
            .checked_add(0x3a)
            .ok_or_else(|| anyhow!("InstallShield file descriptor overflow"))?;
        if descriptor_end > hdr.len() {
            return Err(anyhow!("truncated InstallShield file descriptor"));
        }
        let flags = u16::from_le_bytes(hdr[descriptor + 8..descriptor + 10].try_into().unwrap());
        let expanded_size = read_u32(&hdr, descriptor + 0x0a, "expanded size")?;
        let compressed_size = read_u32(&hdr, descriptor + 0x0e, "compressed size")?;
        let data_offset = read_u32(&hdr, descriptor + 0x26, "data offset")?;
        if flags & 0x0008 != 0 || expanded_size == 0 || compressed_size == 0 || data_offset == 0 {
            continue;
        }
        let data_end = data_offset
            .checked_add(compressed_size)
            .ok_or_else(|| anyhow!("InstallShield payload overflow"))?;
        let compressed = cab
            .get(data_offset..data_end)
            .ok_or_else(|| anyhow!("InstallShield payload is truncated"))?;
        let mut out = Vec::with_capacity(expanded_size);
        let mut cursor = 0usize;
        while out.len() < expanded_size {
            let chunk_len_bytes = compressed
                .get(cursor..cursor + 2)
                .ok_or_else(|| anyhow!("truncated InstallShield compressed chunk"))?;
            let chunk_len = u16::from_le_bytes(chunk_len_bytes.try_into().unwrap()) as usize;
            cursor = cursor
                .checked_add(2)
                .ok_or_else(|| anyhow!("InstallShield compressed chunk overflow"))?;
            let chunk_end = cursor
                .checked_add(chunk_len)
                .ok_or_else(|| anyhow!("InstallShield compressed chunk overflow"))?;
            let chunk = compressed
                .get(cursor..chunk_end)
                .ok_or_else(|| anyhow!("truncated InstallShield compressed data"))?;
            cursor = chunk_end;
            let remaining = expanded_size - out.len();
            let mut decoded = vec![0u8; remaining.min(64 * 1024)];
            let mut decoder = flate2::Decompress::new(false);
            let status = decoder
                .decompress(chunk, &mut decoded, flate2::FlushDecompress::Sync)
                .map_err(|error| anyhow!("InstallShield decompression failed: {error}"))?;
            let written = decoder.total_out() as usize;
            if written == 0 || !matches!(status, flate2::Status::Ok | flate2::Status::StreamEnd) {
                return Err(anyhow!("InstallShield compressed chunk produced no data"));
            }
            out.extend_from_slice(&decoded[..written.min(decoded.len())]);
        }
        if out.len() != expanded_size {
            return Err(anyhow!("InstallShield decompressed size mismatch"));
        }
        std::fs::write(destination, out)?;
        return Ok(());
    }
    Err(anyhow!("InstallShield payload has no valid files"))
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
    let cab_path = if let Some(data_z_path) = data_z_path {
        let mut archive = unshield::Archive::new(File::open(&data_z_path)?)
            .with_context(|| format!("opening InstallShield data.z {}", data_z_path.display()))?;
        let cab_name = archive
            .list()
            .map(|entry| entry.path.clone())
            .find(|name| is_windows_mobile_cab_name(name))
            .or_else(|| {
                archive
                    .list()
                    .map(|entry| entry.path.clone())
                    .find(|name| name.to_ascii_lowercase().ends_with(".cab"))
            })
            .ok_or_else(|| anyhow!("{} contains no Windows Mobile CAB", data_z_path.display()))?;
        let cab_bytes = archive
            .load(&cab_name)
            .with_context(|| format!("extracting {cab_name} from data.z"))?;
        let cab_path = tmp.path().join("pocket-bass-pro.CAB");
        std::fs::write(&cab_path, cab_bytes)?;
        cab_path
    } else {
        let package = ["data1.cab", "data.cab"]
            .iter()
            .map(|name| tmp.path().join(name))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| anyhow!("{} has no InstallShield payload", path.display()))?;
        let raw_cab = tmp.path().join("pocket-bass-pro.CAB");
        extract_installshield_sfx_cab(&package, &raw_cab)?;
        raw_cab
    };
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
    extract_payload_archives(tmp.path())
        .with_context(|| format!("extracting nested game archives from {}", path.display()))?;

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

    // The shortcut names the file the *installer* would have left there,
    // which for a cabinet shipping one build per 3D chip is whichever
    // one its setup DLL renamed into place. The guest keeps seeing that
    // name — on the device it *is* that file — while we run the build
    // behind it.
    let guest_exe_path = guest_exe_path(&exe_path, setup.as_ref(), header.as_ref());
    let exe_path = pocket_library::accelerated_renderer_build(&exe_path).unwrap_or(exe_path);

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
        native_screen: None,
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

fn extract_payload_archives(root: &Path) -> Result<()> {
    let archives: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
        })
        .collect();

    for archive_path in archives {
        let file = File::open(&archive_path)?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading nested archive {}", archive_path.display()))?;
        let mut extracted = 0usize;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index)?;
            let Some(relative) = entry.enclosed_name().map(Path::to_path_buf) else {
                continue;
            };
            if relative.as_os_str().is_empty() {
                continue;
            }
            let destination = root.join(relative);
            if entry.is_dir() {
                std::fs::create_dir_all(&destination)?;
                continue;
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut output = File::create(&destination)?;
            std::io::copy(&mut entry, &mut output)?;
            extracted += 1;
        }
        log::debug!(
            "extracted {} files from nested payload {}",
            extracted,
            archive_path.display()
        );
    }
    Ok(())
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
    let mut nested_archives = Vec::new();

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
        if written
            .last()
            .and_then(|path| path.extension())
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
        {
            nested_archives.push(written.last().unwrap().clone());
        }
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

    for nested in nested_archives {
        extract_payload_archives(nested.parent().unwrap_or(tmp.path()))?;
    }

    let exe_path = pick_entrypoint_pe(written.iter().map(PathBuf::as_path)).with_context(|| {
        format!(
            "no launchable PE found in {}: {} contains no supported game entry point",
            path.display(),
            path.file_name().unwrap_or_default().to_string_lossy(),
        )
    })?;

    let origin = format!("ZIP {} -> {}", path.display(), exe_path.display());
    let gizmondo_layout = is_gizmondo_layout(&written, &exe_path);
    let mut extra_mounts = vec![(
        "\\Program Files\\Game\\".to_string(),
        tmp.path().to_path_buf(),
    )];
    let guest_exe_path = if gizmondo_layout {
        extra_mounts.push(("\\SD Card\\".to_string(), tmp.path().to_path_buf()));
        // The guest path has to name the *real* executable, not a fixed
        // title: it is what relative opens resolve against. Ball Busters
        // asks for `GZGA200045\vsdata.cfl`, so it only finds the container
        // if it believes it is running from `\SD Card\Ball Busters.exe`.
        exe_path
            .file_name()
            .map(|name| format!("\\SD Card\\{}", name.to_string_lossy()))
    } else {
        None
    };
    Ok(Launcher {
        exe: exe_path,
        mount_dir: Some(tmp.path().to_path_buf()),
        extra_mounts,
        guest_exe_path,
        registry: Vec::new(),
        save_prefix: None,
        native_screen: gizmondo_layout.then_some(GIZMONDO_SCREEN),
        origin,
        _tempdir: Some(tmp),
    })
}

/// The Gizmondo's LCD: 320x240, landscape.
///
/// Every card image runs on the same panel, and the titles size
/// themselves from it. Sticky Balls asks GL ES for a viewport the size
/// of the display, so at the emulator's 240x320 Pocket PC default it
/// rendered a portrait slice of a landscape scene with the HUD off the
/// edge of the screen; Ball Busters lays its menus out the same way.
const GIZMONDO_SCREEN: (u32, u32) = (320, 240);

/// A Gizmondo card image is recognised by its catalogue-ID pair: a
/// directory named `GZxx######` holding a file of the *same* name, which
/// is where the title reads the card's serial from. `GZGA200045` is Ball
/// Busters, `GZGA200014` Sticky Balls, `GZGA200036` Carmageddon.
///
/// This used to test for `Alien Hominid.exe` next to a `Sky.bmp`, which
/// recognised exactly one title. Every other Gizmondo dump fell through
/// to the plain `\Program Files\Game\` mount, and because these games
/// address their assets *relative* to the executable, that is not a
/// cosmetic difference: Ball Busters resolved `GZGA200045\vsdata.cfl`
/// against `\Program Files\Game\`, never found the container holding
/// every texture and scene in the game, and sat on an empty loading bar.
fn is_gizmondo_layout(written: &[PathBuf], exe_path: &Path) -> bool {
    has_gizmondo_title_id_pair(written) || is_alien_hominid_layout(written, exe_path)
}

fn has_gizmondo_title_id_pair(written: &[PathBuf]) -> bool {
    written.iter().any(|entry| {
        let Some(name) = entry.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        is_gizmondo_title_id(name)
            && entry
                .parent()
                .and_then(Path::file_name)
                .and_then(|n| n.to_str())
                .is_some_and(|parent| parent.eq_ignore_ascii_case(name))
    })
}

/// The original signature, kept because it is the one dump confirmed to
/// need it: Alien Hominid is recognised by name rather than by a
/// catalogue-ID pair, and no dump of it is on hand to check whether it
/// ships one.
fn is_alien_hominid_layout(written: &[PathBuf], exe_path: &Path) -> bool {
    let exe_name = exe_path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    exe_name == "alien hominid.exe"
        && written.iter().any(|entry| {
            entry
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("Sky.bmp"))
        })
}

/// `GZ` + two letters + six digits, e.g. `GZGA200045`. Case-insensitive:
/// the games spell it upper-case in their own path building, but the ZIP
/// entries are lower-case.
fn is_gizmondo_title_id(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 10
        && bytes[..2].eq_ignore_ascii_case(b"GZ")
        && bytes[2..4].iter().all(|b| b.is_ascii_alphabetic())
        && bytes[4..].iter().all(|b| b.is_ascii_digit())
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
    fn detects_winzip_installshield_sfx_with_zip_payload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("game.exe");
        let mut data = b"MZ_winzip_".to_vec();
        data.extend_from_slice(b"PK\x03\x04");
        std::fs::write(&path, data).unwrap();
        assert!(matches!(
            ArchiveKind::detect(&path),
            ArchiveKind::InstallShieldSfx
        ));
    }

    #[test]
    fn recognises_pocket_pc_installshield_cab_names() {
        assert!(is_windows_mobile_cab_name(
            "Device Installation Files\\Pocket Bass Pro.2577.CAB"
        ));
        assert!(is_windows_mobile_cab_name("Game.ppc_arm.cab"));
        assert!(!is_windows_mobile_cab_name("Help Files\\Manual.cab"));
    }

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

    /// The smallest byte sequence [`pe_header_fields`] accepts as an ARM
    /// PE: an `MZ` stub whose `e_lfanew` points at a `PE\0\0` signature
    /// and a machine type.
    fn arm_pe_header_stub() -> Vec<u8> {
        let mut buf = vec![0u8; 0x100];
        buf[0..2].copy_from_slice(b"MZ");
        // e_lfanew at 0x80
        buf[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        buf.resize(0x98, 0);
        buf[0x80..0x84].copy_from_slice(b"PE\0\0");
        buf[0x84..0x86].copy_from_slice(&IMAGE_FILE_MACHINE_ARM.to_le_bytes());
        buf
    }

    /// A whole ARM PE32 image, sectionless but complete enough for
    /// `pocket_core::pe::load_file` to parse — which is what picking an
    /// entry point out of an archive goes through, unlike the cheap
    /// header peek above.
    fn arm_pe_image() -> Vec<u8> {
        const OH: usize = 0x98;
        let mut buf = arm_pe_header_stub();
        // COFF: no sections, and a 0xe0-byte PE32 optional header.
        buf[0x86..0x88].copy_from_slice(&0u16.to_le_bytes());
        buf[0x94..0x96].copy_from_slice(&0xe0u16.to_le_bytes());
        // Characteristics: executable image, 32-bit, not a DLL.
        buf[0x96..0x98].copy_from_slice(&0x0102u16.to_le_bytes());
        buf.resize(OH + 0xe0, 0);
        let put = |buf: &mut Vec<u8>, off: usize, v: u32| {
            buf[OH + off..OH + off + 4].copy_from_slice(&v.to_le_bytes());
        };
        buf[OH..OH + 2].copy_from_slice(&0x010bu16.to_le_bytes()); // PE32 magic
        put(&mut buf, 0x10, 0x1000); // entry point
        put(&mut buf, 0x14, 0x1000); // base of code
        put(&mut buf, 0x1c, 0x0001_0000); // image base
        put(&mut buf, 0x20, 0x1000); // section alignment
        put(&mut buf, 0x24, 0x200); // file alignment
        put(&mut buf, 0x38, 0x2000); // size of image
        put(&mut buf, 0x3c, 0x200); // size of headers
        buf[OH + 0x44..OH + 0x46].copy_from_slice(&9u16.to_le_bytes()); // WINCE_GUI
        put(&mut buf, 0x5c, 16); // data directory count
        buf
    }

    #[test]
    fn arm_pe_detection() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("fake.exe");
        let mut buf = arm_pe_header_stub();
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

    /// Build a `.zip` holding `entries` as `(name, contents)` pairs.
    fn zip_with(dir: &Path, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join(name);
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for (entry, contents) in entries {
            zip.start_file(*entry, zip::write::FileOptions::default())
                .unwrap();
            zip.write_all(contents).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    /// A Gizmondo card has to come up on the Gizmondo's screen without
    /// the user knowing to ask for it. The device is 320x240 landscape
    /// and the games size themselves from the display, so at the Pocket
    /// PC default Sticky Balls rendered a portrait slice of a landscape
    /// scene and Ball Busters' menus ran off the edge.
    #[test]
    fn a_gizmondo_card_comes_up_on_the_devices_landscape_screen() {
        let dir = TempDir::new().unwrap();
        let card = zip_with(
            dir.path(),
            "card.zip",
            &[
                ("GZGA200014/GZGA200014", b"card serial"),
                ("Sticky Balls.exe", &arm_pe_image()),
            ],
        );
        assert_eq!(prepare(&card).unwrap().native_screen, Some((320, 240)));
    }

    /// The other side of it: a plain Pocket PC zip says nothing about
    /// the device, so it keeps the emulator's portrait default rather
    /// than being turned sideways.
    #[test]
    fn a_pocket_pc_zip_keeps_the_default_screen() {
        let dir = TempDir::new().unwrap();
        let game = zip_with(
            dir.path(),
            "game.zip",
            &[("Game.exe", &arm_pe_image()), ("data.dat", b"assets")],
        );
        assert_eq!(prepare(&game).unwrap().native_screen, None);
    }

    /// The catalogue-ID pair is the signature, and it has to work for
    /// titles nobody special-cased: this failed for every Gizmondo game
    /// except Alien Hominid, which left their relative asset opens
    /// resolving against `\Program Files\Game\`.
    #[test]
    fn gizmondo_layout_detects_any_title_id_directory() {
        // Ball Busters, Sticky Balls, Carmageddon — none named here.
        for id in ["gzga200045", "gzga200014", "GZGA200036"] {
            let entries = vec![
                PathBuf::from(format!("/tmp/pockethle/{id}/{id}")),
                PathBuf::from("/tmp/pockethle/game.exe"),
            ];
            assert!(
                is_gizmondo_layout(&entries, Path::new("/tmp/pockethle/game.exe")),
                "{id} should be recognised as a Gizmondo card layout"
            );
        }
    }

    #[test]
    fn gizmondo_layout_needs_the_marker_inside_its_own_directory() {
        // The ID directory alone is not enough — the serial file inside it
        // carrying the same name is what the title actually reads.
        assert!(!is_gizmondo_layout(
            &[PathBuf::from("/tmp/pockethle/gzga200045/vsdata.cfl")],
            Path::new("/tmp/pockethle/game.exe")
        ));
        // A same-named file somewhere else is not the pair either.
        assert!(!is_gizmondo_layout(
            &[PathBuf::from("/tmp/pockethle/data/gzga200045")],
            Path::new("/tmp/pockethle/game.exe")
        ));
        assert!(!is_gizmondo_layout(
            &[PathBuf::from("/tmp/pockethle/game.exe")],
            Path::new("/tmp/pockethle/game.exe")
        ));
    }

    /// The Alien Hominid signature predates the catalogue-ID one and is
    /// the only dump confirmed to need it, so it has to keep working.
    #[test]
    fn gizmondo_layout_still_accepts_alien_hominid_by_name() {
        let entries = vec![
            PathBuf::from("/tmp/pockethle/Data/Sky.bmp"),
            PathBuf::from("/tmp/pockethle/Alien Hominid.exe"),
        ];
        assert!(is_gizmondo_layout(
            &entries,
            Path::new("/tmp/pockethle/Alien Hominid.exe")
        ));
        assert!(!is_gizmondo_layout(
            &entries,
            Path::new("/tmp/pockethle/Autorun.exe")
        ));
    }

    #[test]
    fn gizmondo_title_id_shape_is_gz_two_letters_six_digits() {
        assert!(is_gizmondo_title_id("gzga200045"));
        assert!(is_gizmondo_title_id("GZGA200045"));
        assert!(!is_gizmondo_title_id("gzga20004"), "too short");
        assert!(!is_gizmondo_title_id("gzga2000456"), "too long");
        assert!(!is_gizmondo_title_id("gz1a200045"), "digits in the prefix");
        assert!(!is_gizmondo_title_id("gzga20004a"), "letter in the number");
        assert!(!is_gizmondo_title_id("xxga200045"), "wrong prefix");
    }
}
