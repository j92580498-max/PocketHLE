//! Game library and persistent configuration shared by all GUI
//! frontends (the egui desktop launcher and the Android launcher).
//!
//! The library lives on disk under a single root directory:
//!
//! ```text
//! <library_root>/
//!     library.json     # registry of installed games (this crate)
//!     config.json      # global launcher settings (this crate)
//!     games/
//!         <id>/
//!             game.json    # per-game manifest (this crate)
//!             extracted/   # files extracted from the imported .CAB
//! ```
//!
//! `<library_root>` is platform-specific:
//!
//! * Linux/Windows: a path supplied by the desktop frontend, typically
//!   the user's `Documents/PocketHLE` folder.
//! * Android: `Context.getExternalFilesDir(null)` or any other path the
//!   Java side hands across JNI.
//!
//! The crate is designed to be `Send + Sync` so the Android side can
//! call into it from any thread without locking.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors returned by [`Library`] operations.
#[derive(Debug, Error)]
pub enum LibraryError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("cab error: {0}")]
    Cab(#[from] pocket_cab::CabError),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("pe error: {0}")]
    Pe(String),
    #[error("game with id `{0}` not found")]
    NotFound(String),
    #[error("the cabinet does not contain any ARM PE32 executable")]
    NoExecutable,
    #[error("invalid game id `{0}`")]
    InvalidId(String),
    #[error("file `{0}` is not an ARM PE32 executable")]
    NotArmPe(String),
}

fn default_schema_version() -> u32 {
    1
}

/// One installed game entry. Stored both inline in `library.json`
/// (for fast listing) and as a separate `game.json` inside the game
/// directory (for per-game settings and so the directory is
/// self-describing if a user copies it around).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameEntry {
    /// Stable identifier, derived from the source cab name. Used as
    /// the directory name. Always a-z 0-9 _ - .
    pub id: String,
    /// Human-readable name shown in the launcher.
    pub display_name: String,
    /// Optional one-line subtitle (provider / publisher).
    #[serde(default)]
    pub provider: Option<String>,
    /// Path to the main ARM `.exe` inside the extracted directory.
    /// Stored relative to `<library_root>/games/<id>/`.
    pub executable: PathBuf,
    /// Source cab basename, kept for display purposes.
    pub source_cab: String,
    /// WinCE installation directory recorded by the CAB, if available.
    #[serde(default)]
    pub install_dir: Option<String>,
    /// Best-effort UNIX timestamp of when the game was imported.
    #[serde(default)]
    pub imported_at: i64,
    /// Per-game runtime settings.
    #[serde(default)]
    pub settings: GameSettings,
}

impl GameEntry {
    /// Path to this game's directory, relative to the library root.
    pub fn relative_dir(&self) -> PathBuf {
        PathBuf::from("games").join(&self.id)
    }

    /// Absolute path to the directory holding the extracted cab.
    pub fn extracted_dir(&self, library_root: &Path) -> PathBuf {
        library_root.join(self.relative_dir()).join("extracted")
    }

    /// Absolute path to the main executable.
    pub fn executable_path(&self, library_root: &Path) -> PathBuf {
        library_root
            .join(self.relative_dir())
            .join(&self.executable)
    }
}

/// Runtime settings stored per game.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameSettings {
    /// Which CPU backend the user prefers for this game.
    #[serde(default)]
    pub cpu_backend: CpuBackendPref,
    /// Maximum number of host-resumed slices per run.
    #[serde(default = "default_max_slices")]
    pub max_slices: u64,
    /// Instructions per slice budget passed to the CPU.
    #[serde(default = "default_instructions_per_slice")]
    pub instructions_per_slice: u64,
    /// If true, the run loop halts as soon as an unimplemented API
    /// is encountered (great for debugging).
    #[serde(default)]
    pub halt_on_unimplemented: bool,
}

impl Default for GameSettings {
    fn default() -> Self {
        Self {
            cpu_backend: CpuBackendPref::default(),
            max_slices: default_max_slices(),
            instructions_per_slice: default_instructions_per_slice(),
            halt_on_unimplemented: false,
        }
    }
}

fn default_max_slices() -> u64 {
    // Real PPC2003 games typically need a few hundred thousand
    // slices to finish their CRT init / soft-float lookup tables /
    // bitmap loading before the first WM_PAINT is delivered, and
    // millions more to clear the splash and reach gameplay.
    // 1024 was effectively a smoke test, not a game launcher: a
    // freshly imported game timed out long before the title
    // screen and looked frozen in the GUI. 50 million is enough
    // to land on the JumpyBall main menu in roughly ten seconds
    // on a modern x86 machine.
    50_000_000
}

fn default_instructions_per_slice() -> u64 {
    1_000_000
}

/// User preference for the CPU backend. `Unicorn` is the only backend
/// that actually executes ARM code — `Stub` is trace-only and cannot
/// run a real game. Both frontends default to `Unicorn` and only fall
/// back to `Stub` when the user explicitly picks it (e.g. for API
/// tracing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CpuBackendPref {
    Stub,
    #[default]
    Unicorn,
}

impl CpuBackendPref {
    pub fn label(self) -> &'static str {
        match self {
            CpuBackendPref::Stub => "Stub (trace-only)",
            CpuBackendPref::Unicorn => "Unicorn (ARM)",
        }
    }
}

/// Persistent global configuration. Lives at
/// `<library_root>/config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Default CPU backend used when importing a new game.
    #[serde(default)]
    pub default_cpu_backend: CpuBackendPref,
    /// Default verbosity level (0..=3).
    #[serde(default)]
    pub verbosity: u8,
    /// Last folder the user picked a `.cab` from. Used to remember
    /// the file dialog start directory.
    #[serde(default)]
    pub last_import_dir: Option<PathBuf>,
    /// Render a j2me-loader-style FPS counter on top of the
    /// in-game framebuffer. Defaults to `true` so a freshly
    /// installed launcher shows the counter out of the box; users
    /// who find it distracting can switch it off in launcher
    /// settings.
    #[serde(default = "default_show_fps")]
    pub show_fps: bool,
}

fn default_show_fps() -> bool {
    true
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            default_cpu_backend: CpuBackendPref::default(),
            verbosity: 1,
            last_import_dir: None,
            show_fps: default_show_fps(),
        }
    }
}

/// Top-level handle to an on-disk PocketHLE library.
///
/// Cheap to clone: the only state is the root path and the in-memory
/// registry; mutations are written through to disk immediately.
#[derive(Debug, Clone)]
pub struct Library {
    root: PathBuf,
    library: LibraryFile,
    config: LauncherConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LibraryFile {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    #[serde(default)]
    games: Vec<GameEntry>,
}

/// Resolve the default library root, in the order the desktop
/// launcher has always used:
///
/// 1. the `POCKETHLE_LIBRARY` environment variable,
/// 2. `<documents>/PocketHLE` (e.g. `~/Documents/PocketHLE`),
/// 3. `<data_dir>/PocketHLE/library` (XDG / `%APPDATA%`),
/// 4. `./pockethle-library` as a last resort.
///
/// Lives here rather than in a frontend so the CLI's `import` command
/// and the GUI always agree on which library they are touching.
pub fn default_library_root() -> PathBuf {
    if let Some(p) = std::env::var_os("POCKETHLE_LIBRARY") {
        return PathBuf::from(p);
    }
    if let Some(dirs) = directories::UserDirs::new() {
        if let Some(docs) = dirs.document_dir() {
            return docs.join("PocketHLE");
        }
    }
    if let Some(dirs) = directories::ProjectDirs::from("ai", "PocketHLE", "PocketHLE") {
        return dirs.data_dir().join("library");
    }
    PathBuf::from("./pockethle-library")
}

impl Library {
    /// Open the library rooted at `root`, creating the directory and
    /// default `library.json` / `config.json` if they don't exist.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, LibraryError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        fs::create_dir_all(root.join("games"))?;

        let library = read_or_default::<LibraryFile>(&root.join("library.json"))?;
        let config = read_or_default::<LauncherConfig>(&root.join("config.json"))?;
        let mut this = Self {
            root,
            library,
            config,
        };
        this.migrate_legacy_entries();
        Ok(this)
    }

    /// Upgrade game entries and global config persisted by older
    /// versions of PocketHLE so they are usable under the current
    /// defaults.
    ///
    /// * `cpu_backend = Stub` is bumped to `Unicorn`. Stub is
    ///   trace-only — it never executes ARM instructions, so any
    ///   pre-#8 game crashes with "guest jumped to unmapped address
    ///   0x00000000" out of the box, which is exactly the failure
    ///   mode users hit before this migration existed.
    /// * `max_slices <= 1_048_576` is bumped to the current default
    ///   (50M). The legacy 1024 default was effectively a smoke
    ///   test and timed out long before the title screen.
    /// * The global `default_cpu_backend` is also flipped from
    ///   `Stub` to `Unicorn` so freshly imported games pick a
    ///   backend that actually runs.
    ///
    /// Both the in-memory state and the on-disk `game.json` /
    /// `library.json` / `config.json` are updated so subsequent
    /// launches see the migrated values. Migration is silent when
    /// no changes are needed.
    fn migrate_legacy_entries(&mut self) {
        let mut library_changed = false;
        for game in self.library.games.iter_mut() {
            let mut migrated = false;
            if game.settings.cpu_backend == CpuBackendPref::Stub {
                game.settings.cpu_backend = CpuBackendPref::Unicorn;
                migrated = true;
            }
            if game.settings.max_slices <= 1_048_576 {
                game.settings.max_slices = default_max_slices();
                migrated = true;
            }
            if migrated {
                library_changed = true;
                let manifest = self.root.join("games").join(&game.id).join("game.json");
                if let Err(e) = write_json(&manifest, game) {
                    log::warn!("could not migrate {}: {e}", manifest.display());
                }
                log::info!(
                    "migrated legacy game entry {}: backend=Unicorn, max_slices={}",
                    game.id,
                    game.settings.max_slices,
                );
            }
        }
        let mut config_changed = false;
        if self.config.default_cpu_backend == CpuBackendPref::Stub {
            self.config.default_cpu_backend = CpuBackendPref::Unicorn;
            config_changed = true;
        }
        if library_changed {
            if let Err(e) = write_json(&self.root.join("library.json"), &self.library) {
                log::warn!("could not save migrated library.json: {e}");
            }
        }
        if config_changed {
            if let Err(e) = write_json(&self.root.join("config.json"), &self.config) {
                log::warn!("could not save migrated config.json: {e}");
            }
        }
    }

    /// Library root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// All known games, sorted by display name.
    pub fn games(&self) -> &[GameEntry] {
        &self.library.games
    }

    /// Look up one game by id.
    pub fn get(&self, id: &str) -> Option<&GameEntry> {
        self.library.games.iter().find(|g| g.id == id)
    }

    /// Mutable access to one game by id.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut GameEntry> {
        self.library.games.iter_mut().find(|g| g.id == id)
    }

    /// Persist the current `library.json` and `config.json` to disk.
    pub fn save(&self) -> Result<(), LibraryError> {
        write_json(&self.root.join("library.json"), &self.library)?;
        write_json(&self.root.join("config.json"), &self.config)?;
        Ok(())
    }

    /// Read-only view of the global launcher config.
    pub fn config(&self) -> &LauncherConfig {
        &self.config
    }

    /// Mutable view of the global launcher config. The caller is
    /// responsible for calling [`Library::save`] when finished.
    pub fn config_mut(&mut self) -> &mut LauncherConfig {
        &mut self.config
    }

    /// Import a Pocket PC `.CAB` into the library.
    ///
    /// Returns the freshly created [`GameEntry`]. The cab is extracted
    /// into `<library_root>/games/<id>/extracted/`, where `<id>` is
    /// derived from the source cab's filename. Existing entries with
    /// the same id are replaced (the directory is wiped first).
    pub fn import_cab(&mut self, cab_path: impl AsRef<Path>) -> Result<&GameEntry, LibraryError> {
        let cab_path = cab_path.as_ref();
        let source_cab = cab_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown.cab".to_string());
        let id = sanitize_id(cab_path.file_stem().map(|s| s.to_string_lossy()).as_deref());
        if id.is_empty() {
            return Err(LibraryError::InvalidId(source_cab));
        }
        let game_dir = self.root.join("games").join(&id);
        if game_dir.exists() {
            fs::remove_dir_all(&game_dir)?;
        }
        let extracted_dir = game_dir.join("extracted");
        fs::create_dir_all(&extracted_dir)?;

        let (files, header) = pocket_cab::extract_with_header(cab_path, &extracted_dir)?;
        // A cabinet stores its payload under generated 8.3 names and keeps
        // the real destination names in `_setup.xml`. The game only ever
        // opens the long ones -- Asphalt 2 3D wants `light.bar` next to
        // `Asphalt2_SPV_C600.exe` -- so recreate them before picking an
        // entry point. Without this a title that runs fine through
        // `pockethle run` fails to load its data from the library.
        let long_names = pocket_cab::materialise_setup_names(&extracted_dir, &files);
        materialise_legacy_assets(
            &extracted_dir,
            &files,
            header.as_ref().and_then(|h| h.app_name.as_deref()),
        );

        // Pick the largest ARM PE32 executable as the entry point.
        let mut best: Option<(PathBuf, u64)> = None;
        for f in &files {
            let lower = f.short_name.to_ascii_lowercase();
            // Skip obvious non-executables. WinCE installer files like
            // `.000` headers must never be treated as the game .exe.
            if lower.ends_with(".000") || lower.ends_with(".dll") {
                continue;
            }
            // Cheap PE check — read the first bytes and look for "MZ".
            if !is_pe_file(&f.extracted_path) {
                continue;
            }
            if best.as_ref().map(|(_, sz)| *sz).unwrap_or(0) < f.size {
                best = Some((f.extracted_path.clone(), f.size));
            }
        }
        let (mut exe_abs, _) = best.ok_or(LibraryError::NoExecutable)?;
        // Prefer the long name `_setup.xml` asked for: that is what the
        // installer would have written on the device, and it is the name
        // `GetModuleFileNameW` reports to the game.
        if let Some((_, long)) = long_names.iter().find(|(short, _)| *short == exe_abs) {
            exe_abs = long.clone();
        }
        let executable = exe_abs
            .strip_prefix(&game_dir)
            .map(|p| p.to_path_buf())
            .unwrap_or(exe_abs.clone());

        let display_name = header
            .as_ref()
            .and_then(|h| h.app_name.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| pretty_id(&id));
        let provider = header.as_ref().and_then(|h| h.provider.clone());

        let entry = GameEntry {
            id: id.clone(),
            display_name,
            provider,
            executable,
            source_cab,
            install_dir: header
                .as_ref()
                .and_then(|h| h.install_dir.clone())
                .or_else(|| infer_install_dir(&files)),
            imported_at: now_unix_seconds(),
            settings: GameSettings {
                cpu_backend: self.config.default_cpu_backend,
                ..GameSettings::default()
            },
        };

        self.commit_entry(&id, entry)
    }

    /// Import a standalone ARM PE32 `.exe` (or any extensionless PE)
    /// into the library. The file is copied verbatim into
    /// `<library_root>/games/<id>/extracted/<exe-basename>` so the
    /// rest of the launcher pipeline can treat it identically to a
    /// CAB-extracted game.
    ///
    /// The user-supplied .exe is checked for an ARM machine type
    /// before being accepted; a mistakenly-imported x86 desktop
    /// build returns [`LibraryError::NotArmPe`] rather than silently
    /// crashing the emulator with "guest jumped to unmapped address"
    /// later on.
    pub fn import_exe(&mut self, exe_path: impl AsRef<Path>) -> Result<&GameEntry, LibraryError> {
        let exe_path = exe_path.as_ref();
        let source_name = exe_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown.exe".to_string());
        let id = sanitize_id(exe_path.file_stem().map(|s| s.to_string_lossy()).as_deref());
        if id.is_empty() {
            return Err(LibraryError::InvalidId(source_name));
        }
        if !is_arm_pe(exe_path).unwrap_or(false) {
            return Err(LibraryError::NotArmPe(source_name));
        }

        let game_dir = self.root.join("games").join(&id);
        if game_dir.exists() {
            fs::remove_dir_all(&game_dir)?;
        }
        let extracted_dir = game_dir.join("extracted");
        fs::create_dir_all(&extracted_dir)?;
        let dest_exe = extracted_dir.join(&source_name);
        fs::copy(exe_path, &dest_exe)?;

        let executable = dest_exe
            .strip_prefix(&game_dir)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| dest_exe.clone());

        let entry = GameEntry {
            id: id.clone(),
            display_name: pretty_id(&id),
            provider: None,
            executable,
            source_cab: source_name,
            install_dir: None,
            imported_at: now_unix_seconds(),
            settings: GameSettings {
                cpu_backend: self.config.default_cpu_backend,
                ..GameSettings::default()
            },
        };

        self.commit_entry(&id, entry)
    }

    /// Import a `.zip` archive (typically a community repack of a
    /// PocketPC game) into the library.
    ///
    /// Every entry is extracted into
    /// `<library_root>/games/<id>/extracted/`. If the zip turns out
    /// to contain a nested `.cab`, we transparently recurse via
    /// [`Library::import_cab`] on the extracted CAB so the user gets
    /// the proper `app_name` / `provider` from the cabinet header
    /// rather than a stem-derived placeholder. Otherwise the largest
    /// ARM PE32 inside the archive is picked as the entry point.
    pub fn import_zip(&mut self, zip_path: impl AsRef<Path>) -> Result<&GameEntry, LibraryError> {
        let zip_path = zip_path.as_ref();
        let source_name = zip_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown.zip".to_string());
        let id = sanitize_id(zip_path.file_stem().map(|s| s.to_string_lossy()).as_deref());
        if id.is_empty() {
            return Err(LibraryError::InvalidId(source_name));
        }

        let game_dir = self.root.join("games").join(&id);
        if game_dir.exists() {
            fs::remove_dir_all(&game_dir)?;
        }
        let extracted_dir = game_dir.join("extracted");
        fs::create_dir_all(&extracted_dir)?;

        // Extract every entry next to the would-be executable.
        let f = fs::File::open(zip_path)?;
        let mut archive = zip::ZipArchive::new(f)?;
        let mut written: Vec<PathBuf> = Vec::with_capacity(archive.len());
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let Some(rel) = entry.enclosed_name().map(Path::to_path_buf) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            let dest = extracted_dir.join(&rel);
            if entry.is_dir() {
                fs::create_dir_all(&dest)?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = fs::File::create(&dest)?;
            std::io::copy(&mut entry, &mut out)?;
            written.push(dest);
        }
        if written.is_empty() {
            return Err(LibraryError::NoExecutable);
        }

        // ZIPs that are really just installer wrappers around a CAB —
        // recurse so we get the proper PocketPC display metadata
        // instead of a stem-derived placeholder.
        if let Some(nested_cab) = written
            .iter()
            .find(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("cab"))
                    .unwrap_or(false)
            })
            .cloned()
        {
            log::info!(
                "zip {} contains nested cab {}, recursing",
                zip_path.display(),
                nested_cab.display(),
            );
            // Wipe the partial extraction so the CAB import starts
            // from a clean directory layout — the nested cab content
            // is what we actually want as the per-game tree.
            let nested_cab_temp = self.root.join("games").join(format!(".{id}-nested.cab"));
            if let Some(parent) = nested_cab_temp.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&nested_cab, &nested_cab_temp)?;
            fs::remove_dir_all(&game_dir)?;
            let result = self.import_cab(&nested_cab_temp);
            let _ = fs::remove_file(&nested_cab_temp);
            return result;
        }

        // Find the largest ARM PE inside the extraction.
        let mut best: Option<(PathBuf, u64)> = None;
        for path in &written {
            let Ok(meta) = fs::metadata(path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            if !is_arm_pe(path).unwrap_or(false) {
                continue;
            }
            if best.as_ref().map(|(_, sz)| *sz).unwrap_or(0) < meta.len() {
                best = Some((path.clone(), meta.len()));
            }
        }
        let (exe_abs, _) = best.ok_or(LibraryError::NoExecutable)?;
        let executable = exe_abs
            .strip_prefix(&game_dir)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| exe_abs.clone());

        let entry = GameEntry {
            id: id.clone(),
            display_name: pretty_id(&id),
            provider: None,
            executable,
            source_cab: source_name,
            install_dir: None,
            imported_at: now_unix_seconds(),
            settings: GameSettings {
                cpu_backend: self.config.default_cpu_backend,
                ..GameSettings::default()
            },
        };

        self.commit_entry(&id, entry)
    }

    /// Import whatever `path` points at, picking the right importer
    /// from its extension: `.cab`, `.zip`, or otherwise a raw ARM
    /// PE32 executable. This is the entry point the launcher UI and
    /// the CLI's `import` subcommand both use, so a game imported from
    /// a script behaves exactly like one added through the GUI.
    pub fn import_any(&mut self, path: impl AsRef<Path>) -> Result<&GameEntry, LibraryError> {
        let path = path.as_ref();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("cab") => self.import_cab(path),
            Some("zip") => self.import_zip(path),
            _ => self.import_exe(path),
        }
    }

    /// Persist `entry` and return a stable reference. Replaces any
    /// existing game with the same id.
    fn commit_entry(&mut self, id: &str, entry: GameEntry) -> Result<&GameEntry, LibraryError> {
        let game_dir = self.root.join("games").join(id);
        // Persist a per-game manifest so the game directory is
        // self-describing.
        write_json(&game_dir.join("game.json"), &entry)?;

        // Replace any existing entry with the same id.
        self.library.games.retain(|g| g.id != id);
        self.library.games.push(entry);
        self.library
            .games
            .sort_by(|a, b| a.display_name.cmp(&b.display_name));
        self.save()?;

        // Return a stable reference.
        Ok(self
            .library
            .games
            .iter()
            .find(|g| g.id == id)
            .expect("just inserted"))
    }

    /// Remove a game and its on-disk files.
    pub fn remove(&mut self, id: &str) -> Result<(), LibraryError> {
        let game_dir = self.root.join("games").join(id);
        if game_dir.exists() {
            fs::remove_dir_all(&game_dir)?;
        }
        self.library.games.retain(|g| g.id != id);
        self.save()
    }

    /// Update the per-game settings and save the library.
    pub fn update_settings(
        &mut self,
        id: &str,
        settings: GameSettings,
    ) -> Result<(), LibraryError> {
        let game = self
            .get_mut(id)
            .ok_or_else(|| LibraryError::NotFound(id.to_string()))?;
        game.settings = settings;
        let cloned = game.clone();
        let game_dir = self.root.join("games").join(id);
        write_json(&game_dir.join("game.json"), &cloned)?;
        self.save()
    }
}

fn materialise_legacy_assets(root: &Path, files: &[pocket_cab::CabFile], app_name: Option<&str>) {
    let by_short: std::collections::HashMap<String, &Path> = files
        .iter()
        .map(|f| {
            (
                f.short_name.to_ascii_uppercase(),
                f.extracted_path.as_path(),
            )
        })
        .collect();

    let known_names = [
        ("ATOMIC~3.001", "AtomicDreams.exe"),
        ("ATOMIC~1.002", "AtomicDreams.pak"),
    ];
    let mut copied_known = false;
    for (short, long) in known_names {
        if let Some(src) = by_short.get(short) {
            let _ = fs::copy(src, root.join(long));
            copied_known = true;
        }
    }
    if copied_known {
        return;
    }

    let arm = files
        .iter()
        .filter(|f| is_arm_pe(&f.extracted_path).unwrap_or(false))
        .max_by_key(|f| f.size);
    let data = files
        .iter()
        .filter(|f| !is_arm_pe(&f.extracted_path).unwrap_or(false))
        .filter(|f| !f.short_name.to_ascii_lowercase().ends_with(".000"))
        .max_by_key(|f| f.size);
    let Some(arm) = arm else { return };
    let stem: String = app_name
        .unwrap_or("Game")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let stem = if stem.is_empty() { "Game" } else { &stem };
    let _ = fs::copy(&arm.extracted_path, root.join(format!("{stem}.exe")));
    if let Some(data) = data {
        let _ = fs::copy(&data.extracted_path, root.join(format!("{stem}.pak")));
    }
}

fn infer_install_dir(files: &[pocket_cab::CabFile]) -> Option<String> {
    let has_asphalt_exe = files.iter().any(|file| {
        file.short_name.eq_ignore_ascii_case("ASPHAL~1.001")
            && is_arm_pe(&file.extracted_path).unwrap_or(false)
    });
    has_asphalt_exe.then(|| "\\Program Files\\Asphalt 2 3D\\".to_string())
}

fn read_or_default<T>(path: &Path) -> Result<T, LibraryError>
where
    T: Default + serde::de::DeserializeOwned,
{
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<T>(&bytes) {
            Ok(v) => Ok(v),
            Err(e) => {
                log::warn!(
                    "could not parse {}: {e}; falling back to default",
                    path.display()
                );
                Ok(T::default())
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(LibraryError::Io(e)),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), LibraryError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    let temp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("manifest.json")
    ));
    fs::write(&temp, bytes)?;
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(())
}

fn sanitize_id(stem: Option<&str>) -> String {
    let raw = stem.unwrap_or("game");
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

fn pretty_id(id: &str) -> String {
    let cleaned = id.replace(['_', '-', '.'], " ");
    let mut out = String::with_capacity(cleaned.len());
    let mut new_word = true;
    for ch in cleaned.chars() {
        if ch == ' ' {
            new_word = true;
            out.push(' ');
        } else if new_word {
            out.extend(ch.to_uppercase());
            new_word = false;
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

fn is_pe_file(path: &Path) -> bool {
    let mut head = [0u8; 2];
    match fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head)) {
        Ok(()) => &head == b"MZ",
        Err(_) => false,
    }
}

/// `IMAGE_FILE_MACHINE_*` constants from the PE/COFF spec. Mirrors
/// the values in `pocket-cli`'s archive helper — we deliberately read
/// the raw bytes here so the hot import-time scan doesn't have to
/// fully parse every PE.
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

/// Cheap PE header sniff: is `path` an ARM or little-endian MIPS PE32.
///
/// Returns `Ok(false)` for non-PE / short / non-ARM files so the
/// caller can scan a whole directory without having to discriminate
/// between "isn't an ARM PE" and "actually failed I/O".
fn is_arm_pe(path: &Path) -> std::io::Result<bool> {
    let mut f = fs::File::open(path)?;
    let mut head = [0u8; 0x40];
    let n = f.read(&mut head)?;
    if n < 0x40 {
        return Ok(false);
    }
    if &head[0..2] != b"MZ" {
        return Ok(false);
    }
    let lfanew = u32::from_le_bytes(head[0x3c..0x40].try_into().unwrap()) as u64;
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

fn now_unix_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "pockethle-library-test-{}-{}",
            name,
            now_unix_seconds()
        ));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn open_creates_layout() {
        let root = tmpdir("layout");
        let _lib = Library::open(&root).unwrap();
        assert!(root.join("games").is_dir());
        assert!(!root.join("library.json").exists() || root.join("library.json").is_file());
    }

    #[test]
    fn save_and_reload_round_trips() {
        let root = tmpdir("roundtrip");
        let mut lib = Library::open(&root).unwrap();
        lib.config_mut().verbosity = 2;
        lib.config_mut().show_fps = false;
        lib.save().unwrap();

        let lib2 = Library::open(&root).unwrap();
        assert_eq!(lib2.config().verbosity, 2);
        assert!(!lib2.config().show_fps);
    }

    #[test]
    fn show_fps_defaults_to_true() {
        // A brand-new library that has never been saved should
        // come back with the FPS overlay enabled.
        let root = tmpdir("showfps_default");
        let lib = Library::open(&root).unwrap();
        assert!(lib.config().show_fps);

        // A pre-existing config.json that pre-dates the show_fps
        // field must also default to enabled when re-opened.
        let root2 = tmpdir("showfps_legacy");
        fs::create_dir_all(&root2).unwrap();
        let legacy_config = serde_json::json!({
            "schema_version": 1,
            "default_cpu_backend": "unicorn",
            "verbosity": 1,
            "last_import_dir": null,
        });
        fs::write(
            root2.join("config.json"),
            serde_json::to_vec_pretty(&legacy_config).unwrap(),
        )
        .unwrap();
        let lib2 = Library::open(&root2).unwrap();
        assert!(lib2.config().show_fps);
    }

    #[test]
    fn sanitize_id_strips_garbage() {
        assert_eq!(sanitize_id(Some("JumpyBall PPC")), "jumpyball_ppc");
        assert_eq!(sanitize_id(Some("../../etc/passwd")), "etcpasswd");
        assert_eq!(sanitize_id(Some("")), "game");
        assert_eq!(sanitize_id(None), "game");
    }

    #[test]
    fn pretty_id_titlecases_words() {
        assert_eq!(pretty_id("jumpy_ball"), "Jumpy Ball");
        assert_eq!(pretty_id("foo-bar.baz"), "Foo Bar Baz");
    }

    #[test]
    fn legacy_stub_entries_are_migrated_to_unicorn() {
        let root = tmpdir("migrate");
        fs::create_dir_all(root.join("games").join("legacy")).unwrap();
        // Pre-#8 game.json shape: backend = stub, max_slices = 1024.
        let legacy_json = serde_json::json!({
            "id": "legacy",
            "display_name": "Legacy Game",
            "provider": null,
            "executable": "extracted/LEGACY.exe",
            "source_cab": "legacy.cab",
            "imported_at": 0,
            "settings": {
                "cpu_backend": "stub",
                "max_slices": 1024,
                "instructions_per_slice": 1_000_000,
                "halt_on_unimplemented": false,
            },
        });
        fs::write(
            root.join("games").join("legacy").join("game.json"),
            serde_json::to_vec_pretty(&legacy_json).unwrap(),
        )
        .unwrap();
        let library_file = serde_json::json!({
            "schema_version": 1,
            "games": [legacy_json],
        });
        fs::write(
            root.join("library.json"),
            serde_json::to_vec_pretty(&library_file).unwrap(),
        )
        .unwrap();
        // Legacy global config that defaults new games to Stub.
        let legacy_config = serde_json::json!({
            "schema_version": 1,
            "default_cpu_backend": "stub",
            "verbosity": 1,
            "last_import_dir": null,
        });
        fs::write(
            root.join("config.json"),
            serde_json::to_vec_pretty(&legacy_config).unwrap(),
        )
        .unwrap();

        let lib = Library::open(&root).unwrap();
        let game = lib.get("legacy").unwrap();
        assert_eq!(game.settings.cpu_backend, CpuBackendPref::Unicorn);
        assert_eq!(game.settings.max_slices, default_max_slices());
        assert_eq!(lib.config().default_cpu_backend, CpuBackendPref::Unicorn);

        // The migration must also be persisted to disk so the next
        // open() doesn't have to redo the work.
        let lib2 = Library::open(&root).unwrap();
        let game2 = lib2.get("legacy").unwrap();
        assert_eq!(game2.settings.cpu_backend, CpuBackendPref::Unicorn);
        assert_eq!(game2.settings.max_slices, default_max_slices());
    }
}
