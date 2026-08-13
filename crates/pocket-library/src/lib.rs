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

pub mod save_data;

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
    #[error(
        "`{0}` is a support library, not a game — import the .exe instead \
         and any DLLs next to it are picked up automatically"
    )]
    IsLibrary(String),
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
    /// Every guest directory the CAB installs files into.
    #[serde(default)]
    pub install_dirs: Vec<String>,
    /// WinCE directory used by the game for save data, if the CAB records one.
    #[serde(default)]
    pub save_prefix: Option<String>,
    /// Registry values installed by the CAB before the first launch.
    #[serde(default)]
    pub registry: Vec<pocket_cab::SetupRegistryValue>,
    /// Best-effort UNIX timestamp of when the game was imported.
    #[serde(default)]
    pub imported_at: i64,
    /// Per-game runtime settings.
    #[serde(default)]
    pub settings: GameSettings,
    /// Launcher icon extracted from the executable's `RT_GROUP_ICON`
    /// resource, stored as a PNG relative to
    /// `<library_root>/games/<id>/`. `None` when the binary ships no
    /// icon, in which case frontends fall back to a placeholder.
    #[serde(default)]
    pub icon: Option<PathBuf>,
    /// Support libraries that were copied in alongside the executable,
    /// relative to `<library_root>/games/<id>/`.
    ///
    /// Purely informational — the emulator finds these by name at
    /// `LoadLibraryW` time because they sit in the same directory as
    /// the executable. Recorded so the launcher can tell the user that
    /// importing one `.exe` also brought in its satellite DLLs, and so
    /// a game directory stays self-describing.
    #[serde(default)]
    pub companions: Vec<PathBuf>,
}

/// Whether this entry is the Alien Hominid Gizmondo build.
///
/// The ZIP repack has no CAB install manifest, so the launcher must recreate
/// the SD-card paths that the executable uses when it opens its data files.
pub fn is_alien_hominid_gizmondo(entry: &GameEntry) -> bool {
    entry
        .display_name
        .to_ascii_lowercase()
        .contains("alien hominid")
        || entry
            .source_cab
            .to_ascii_lowercase()
            .contains("alien-hominid")
        || entry.source_cab.to_ascii_lowercase().contains("gizmondo")
}

impl GameEntry {
    /// Path to this game's directory, relative to the library root.
    pub fn relative_dir(&self) -> PathBuf {
        PathBuf::from("games").join(&self.id)
    }

    /// Absolute path of the extracted launcher icon, if the executable
    /// carried one.
    pub fn icon_path(&self, library_root: &Path) -> Option<PathBuf> {
        let icon = self.icon.as_ref()?;
        Some(library_root.join(self.relative_dir()).join(icon))
    }

    /// Absolute path to the directory holding the extracted cab.
    pub fn extracted_dir(&self, library_root: &Path) -> PathBuf {
        library_root.join(self.relative_dir()).join("extracted")
    }

    pub fn save_dir(&self, library_root: &Path) -> PathBuf {
        save_data::save_dir_for(library_root, &self.id)
    }

    /// Absolute path to the main executable.
    pub fn executable_path(&self, library_root: &Path) -> PathBuf {
        library_root
            .join(self.relative_dir())
            .join(&self.executable)
    }

    /// Guest directory the installer would have written this game to,
    /// normalised to exactly one trailing backslash (for example
    /// `"\\Program Files\\EA\\Spore v1.0.4\\"`).
    ///
    /// Games that keep their assets in one archive open it by absolute
    /// path -- Spore Origins asks for `<install dir>\data.vfs` -- so the
    /// extracted directory has to be mounted there as well as under the
    /// generic `\Program Files\` prefix. Without it the open fails and
    /// the game calls through the handle it never managed to store.
    pub fn guest_install_prefix(&self) -> Option<String> {
        let raw = self.install_dir.as_deref()?.trim();
        let trimmed = raw.trim_end_matches('\\');
        if trimmed.is_empty() {
            return None;
        }
        Some(format!("{trimmed}\\"))
    }

    /// Path `GetModuleFileNameW` should report for this game, derived
    /// from [`Self::guest_install_prefix`] and the executable's file
    /// name. Relative guest paths resolve against its directory.
    pub fn guest_exe_path(&self) -> Option<String> {
        let prefix = self.guest_install_prefix()?;
        let name = self.executable.file_name()?.to_str()?;
        Some(format!("{prefix}{name}"))
    }

    pub fn guest_save_prefix(&self) -> Option<String> {
        self.save_prefix
            .clone()
            .or_else(|| self.guest_install_prefix())
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
    /// Emulated display geometry. Smartphone builds are shipped for a
    /// specific screen and query it once at start-up, so a landscape
    /// title rendered into a portrait framebuffer comes out clipped.
    #[serde(default)]
    pub screen: ScreenPref,
}

impl Default for GameSettings {
    fn default() -> Self {
        Self {
            cpu_backend: CpuBackendPref::default(),
            max_slices: default_max_slices(),
            instructions_per_slice: default_instructions_per_slice(),
            halt_on_unimplemented: false,
            screen: ScreenPref::default(),
        }
    }
}

/// Emulated display geometry for one game.
///
/// A GAPI game asks the OS for the screen size once (`GetSystemMetrics`
/// / `GXGetDisplayProperties`) and lays its whole HUD out around the
/// answer, so this has to be right before the first frame. The default
/// is the classic 240x320 Pocket PC portrait panel; Smartphone ports
/// built for a landscape device — Asphalt 2 3D's Motorola Q9 build, for
/// one — need 320x240 or their menus and speedometer are cut off at the
/// right edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScreenPref {
    /// 240x320 portrait (Pocket PC / most Smartphone builds).
    #[default]
    Portrait,
    /// 320x240 landscape (Motorola Q / other landscape Smartphones).
    Landscape,
    /// 176x220 — the small Smartphone panel.
    SmallPortrait,
    /// 480x800 WVGA Windows Phone display.
    Wvga,
    /// 480x320 — the Handheld PC / H/PC landscape panel.
    ///
    /// Windows CE's bundled applets (Solitaire among them) are H/PC
    /// class and lay their UI out against this geometry. Launching
    /// them at the 240x320 Pocket PC default leaves the window
    /// clipped, so they need their own entry rather than being
    /// squeezed into `Landscape`.
    Hpc,
}

impl ScreenPref {
    pub fn label(self) -> &'static str {
        match self {
            ScreenPref::Portrait => "240x320 (portrait)",
            ScreenPref::Landscape => "320x240 (landscape)",
            ScreenPref::SmallPortrait => "176x220 (small)",
            ScreenPref::Wvga => "480x800 (WVGA)",
            ScreenPref::Hpc => "480x320 (Handheld PC)",
        }
    }

    /// `(width, height)` in pixels.
    pub fn size(self) -> (u32, u32) {
        match self {
            ScreenPref::Portrait => (240, 320),
            ScreenPref::Landscape => (320, 240),
            ScreenPref::SmallPortrait => (176, 220),
            ScreenPref::Wvga => (480, 800),
            ScreenPref::Hpc => (480, 320),
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
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default = "default_fullscreen_mode")]
    pub fullscreen_mode: String,
    #[serde(default = "default_orientation")]
    pub orientation: String,
}

fn default_show_fps() -> bool {
    true
}

fn default_orientation() -> String {
    "auto".to_string()
}

fn default_fullscreen_mode() -> String {
    "with_controls".to_string()
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            default_cpu_backend: CpuBackendPref::default(),
            verbosity: 1,
            last_import_dir: None,
            show_fps: default_show_fps(),
            fullscreen: false,
            fullscreen_mode: default_fullscreen_mode(),
            orientation: default_orientation(),
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
        for (index, companion) in find_resource_companion_cabs(cab_path)
            .into_iter()
            .enumerate()
        {
            import_resource_companion_cab(&game_dir, &extracted_dir, index, &companion);
        }
        // A cabinet stores its payload under generated 8.3 names and keeps
        // the real destination names in `_setup.xml`. The game only ever
        // opens the long ones -- Asphalt 2 3D wants `light.bar` next to
        // `Asphalt2_SPV_C600.exe` -- so recreate them before picking an
        // entry point. Without this a title that runs fine through
        // `pockethle run` fails to load its data from the library.
        let long_names = pocket_cab::materialise_setup_names(&extracted_dir, &files);
        // Cabs predating `_setup.xml` keep the same mapping in their
        // binary `.000` header instead. Rayman Ultimate records all 198
        // of its payload names there and nowhere else, so a library
        // import that skips this step ends up with a directory of
        // `00000RAY.004`-style names the game cannot open.
        let structured_header = header.as_ref().filter(|h| h.structured);
        if let Some(h) = structured_header {
            pocket_cab::materialise_install_header_names(&extracted_dir, &files, h);
        } else {
            materialise_legacy_assets(
                &extracted_dir,
                &files,
                header.as_ref().and_then(|h| h.app_name.as_deref()),
            );
        }

        let setup = files
            .iter()
            .find(|f| f.short_name.eq_ignore_ascii_case("_setup.xml"))
            .and_then(|f| fs::read(&f.extracted_path).ok())
            .map(|bytes| pocket_cab::WinCeSetupScript::parse_bytes(&bytes));
        let materialised_exe = setup
            .as_ref()
            .and_then(|script| {
                let install_root = script.install_root();
                let by_long_path = |long: &str| {
                    let relative = script.relative_destination(long, install_root.as_deref())?;
                    let candidate = relative
                        .split('\\')
                        .filter(|s| !s.is_empty())
                        .fold(extracted_dir.to_path_buf(), |acc, seg| acc.join(seg));
                    (is_guest_exe(&candidate)).then_some(candidate)
                };
                if let Some(target) = &script.shortcut_target {
                    if target.to_ascii_lowercase().ends_with(".exe") {
                        if let Some(path) = by_long_path(target) {
                            return Some(path);
                        }
                    }
                }
                script
                    .renames
                    .iter()
                    .filter(|(_, long)| long.to_ascii_lowercase().ends_with(".exe"))
                    .filter_map(|(_, long)| by_long_path(long))
                    .max_by_key(|path| fs::metadata(path).map(|m| m.len()).unwrap_or(0))
            })
            .or_else(|| {
                // `.000`-only cabs: the LINKS section names the binary
                // the device's shell would launch, which beats picking
                // the largest PE when a cab ships helper executables.
                let header = structured_header?;
                let by_dest = |dest: &str| {
                    header
                        .host_path(&extracted_dir, dest)
                        .filter(|p| is_guest_exe(p))
                };
                if let Some(target) = header
                    .shortcut_target
                    .as_deref()
                    .filter(|t| t.to_ascii_lowercase().ends_with(".exe"))
                {
                    if let Some(path) = by_dest(target) {
                        return Some(path);
                    }
                }
                header
                    .files
                    .iter()
                    .filter(|e| e.destination.to_ascii_lowercase().ends_with(".exe"))
                    .filter_map(|e| by_dest(&e.destination))
                    .max_by_key(|path| fs::metadata(path).map(|m| m.len()).unwrap_or(0))
            });
        let (mut exe_abs, _) = if let Some(path) = materialised_exe {
            (
                path.clone(),
                fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
            )
        } else {
            // Legacy cabinets without setup.xml do not provide long-name
            // mappings, so retain the size-based PE fallback.
            let mut best: Option<(PathBuf, u64)> = None;
            for f in &files {
                let lower = f.short_name.to_ascii_lowercase();
                if lower.ends_with(".000") || lower.ends_with(".dll") {
                    continue;
                }
                if !is_pe_file(&f.extracted_path) {
                    continue;
                }
                // A cabinet renames its payload to 8.3 names, so the
                // extension check above misses a satellite library
                // stored as `PEGCAR~1.002`. The COFF DLL bit does not
                // lie. A cabinet that ships *only* libraries still
                // falls through to `NoExecutable` below.
                if is_guest_dll(&f.extracted_path) {
                    continue;
                }
                if best.as_ref().map(|(_, sz)| *sz).unwrap_or(0) < f.size {
                    best = Some((f.extracted_path.clone(), f.size));
                }
            }
            best.ok_or(LibraryError::NoExecutable)?
        };
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

        let setup_app_name = setup.as_ref().and_then(|script| script.app_name.clone());
        let display_name = setup_app_name
            .or_else(|| header.as_ref().and_then(|h| h.app_name.clone()))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| pretty_id(&id));
        let provider = header.as_ref().and_then(|h| h.provider.clone());

        let entry = GameEntry {
            id: id.clone(),
            display_name,
            provider,
            executable,
            source_cab,
            install_dir: setup_install_dir(&files)
                .or_else(|| header.as_ref().and_then(|h| h.install_dir.clone()))
                .or_else(|| infer_install_dir(&files)),
            install_dirs: setup_install_dirs(&files),
            save_prefix: setup_save_dir(&files),
            registry: setup_registry(&files),
            imported_at: now_unix_seconds(),
            settings: GameSettings {
                cpu_backend: self.config.default_cpu_backend,
                screen: guess_screen(
                    &exe_abs,
                    &files,
                    header.as_ref().and_then(|h| h.app_name.as_deref()),
                ),
                ..GameSettings::default()
            },
            icon: extract_icon_png(&game_dir, &exe_abs),
            companions: record_extracted_libraries(&extracted_dir, &game_dir),
        };

        self.commit_entry(&id, entry)
    }

    /// Import a standalone ARM PE32 `.exe` (or any extensionless PE)
    /// into the library. The file is copied verbatim into
    /// `<library_root>/games/<id>/extracted/<exe-basename>` so the
    /// rest of the launcher pipeline can treat it identically to a
    /// CAB-extracted game.
    ///
    /// Any guest DLLs sitting next to the executable are copied in as
    /// well. A fair number of titles split their artwork or engine into
    /// a satellite library and pull it in with `LoadLibraryW` at
    /// runtime — the CE shell's Solitaire keeps every card face in
    /// `pegcards.dll` — and the emulator resolves those by name in the
    /// executable's own directory, exactly as a real device would. Copy
    /// only the executable and the game loads but cannot draw.
    ///
    /// The user-supplied .exe is checked for an ARM machine type
    /// before being accepted; a mistakenly-imported x86 desktop
    /// build returns [`LibraryError::NotArmPe`] rather than silently
    /// crashing the emulator with "guest jumped to unmapped address"
    /// later on. A DLL handed in as the entry point is rejected with
    /// [`LibraryError::IsLibrary`]: it is a real file the game needs,
    /// but it has no entry point to start.
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
        let Some(pe) = sniff_pe(exe_path).ok().flatten() else {
            return Err(LibraryError::NotArmPe(source_name));
        };
        if !pe.is_supported_guest() {
            return Err(LibraryError::NotArmPe(source_name));
        }
        if pe.is_dll() {
            return Err(LibraryError::IsLibrary(source_name));
        }

        let game_dir = self.root.join("games").join(&id);
        if game_dir.exists() {
            fs::remove_dir_all(&game_dir)?;
        }
        let extracted_dir = game_dir.join("extracted");
        fs::create_dir_all(&extracted_dir)?;
        let dest_exe = extracted_dir.join(&source_name);
        fs::copy(exe_path, &dest_exe)?;
        let companions = copy_sibling_libraries(exe_path, &extracted_dir, &game_dir);

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
            install_dirs: Vec::new(),
            save_prefix: None,
            registry: Vec::new(),
            imported_at: now_unix_seconds(),
            settings: GameSettings {
                cpu_backend: self.config.default_cpu_backend,
                ..GameSettings::default()
            },
            icon: extract_icon_png(&game_dir, &dest_exe),
            companions,
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
        let nested_cabs: Vec<PathBuf> = written
            .iter()
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("cab"))
            })
            .cloned()
            .collect();
        if !nested_cabs.is_empty() {
            let main_cab = nested_cabs
                .iter()
                .find(|path| {
                    let stem = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    !stem.contains("res") && !stem.contains("resource")
                })
                .cloned()
                .unwrap_or_else(|| nested_cabs[0].clone());
            log::info!(
                "zip {} contains nested cabinets; importing {} with its companion cabinets",
                zip_path.display(),
                main_cab.display(),
            );
            let nested_dir = self.root.join("games").join(format!(".{id}-nested-cabs"));
            fs::create_dir_all(&nested_dir)?;
            let staged_main =
                nested_dir.join(main_cab.file_name().ok_or(LibraryError::NoExecutable)?);
            for cab in &nested_cabs {
                let name = cab.file_name().ok_or(LibraryError::NoExecutable)?;
                fs::copy(cab, nested_dir.join(name))?;
            }
            fs::remove_dir_all(&game_dir)?;
            let result = self.import_cab(&staged_main);
            let _ = fs::remove_dir_all(&nested_dir);
            return result;
        }

        // Find the largest ARM PE inside the extraction. Satellite
        // libraries are skipped: a resource-only DLL can easily be the
        // largest PE in the archive — it is where the artwork lives —
        // and it has no entry point to start.
        let mut best: Option<(PathBuf, u64)> = None;
        for path in &written {
            let Ok(meta) = fs::metadata(path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            if !is_guest_exe(path) {
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

        let is_gizmondo = source_name.to_ascii_lowercase().contains("alien-hominid")
            || source_name.to_ascii_lowercase().contains("gizmondo")
            || written.iter().any(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().eq_ignore_ascii_case("Sky.bmp"))
                    .unwrap_or(false)
            });
        let entry = GameEntry {
            id: id.clone(),
            display_name: if is_gizmondo {
                "Alien Hominid".to_string()
            } else {
                pretty_id(&id)
            },
            provider: None,
            executable,
            source_cab: source_name,
            install_dir: None,
            install_dirs: Vec::new(),
            save_prefix: None,
            registry: Vec::new(),
            imported_at: now_unix_seconds(),
            settings: GameSettings {
                cpu_backend: self.config.default_cpu_backend,
                ..GameSettings::default()
            },
            icon: extract_icon_png(&game_dir, &exe_abs),
            companions: record_extracted_libraries(&extracted_dir, &game_dir),
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

/// Number of satellite libraries we are willing to pull in next to a
/// hand-picked `.exe`. A game ships a handful; a user who points the
/// importer at a system directory should not have it copied wholesale.
const MAX_SIBLING_LIBRARIES: usize = 16;

/// Copy the guest support libraries sitting next to `exe_path` into
/// `dest_dir`, so `LoadLibraryW` finds them at run time.
///
/// Returns the copied files as paths relative to `game_dir`, for
/// [`GameEntry::companions`].
///
/// The filter is deliberately narrow: only ARM/MIPS PE images with
/// `IMAGE_FILE_DLL` set, only in the executable's own directory, never
/// recursing. That is enough for the satellite-DLL layout games
/// actually ship (`solitare.exe` + `pegcards.dll` in one folder) while
/// keeping an import from a busy Downloads folder from dragging in
/// unrelated binaries. Host x86 DLLs are skipped because they fail the
/// machine check.
fn copy_sibling_libraries(exe_path: &Path, dest_dir: &Path, game_dir: &Path) -> Vec<PathBuf> {
    let Some(src_dir) = exe_path.parent() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(src_dir) else {
        return Vec::new();
    };
    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p != exe_path)
        .filter(|p| p.is_file())
        .filter(|p| is_guest_dll(p))
        .collect();
    // Stable order so a re-import produces the same manifest.
    candidates.sort();

    let mut copied = Vec::new();
    for src in candidates.iter().take(MAX_SIBLING_LIBRARIES) {
        let Some(name) = src.file_name() else {
            continue;
        };
        let dest = dest_dir.join(name);
        if let Err(e) = fs::copy(src, &dest) {
            log::warn!("could not copy support library {}: {e}", src.display());
            continue;
        }
        log::info!("imported support library {}", src.display());
        copied.push(
            dest.strip_prefix(game_dir)
                .map(Path::to_path_buf)
                .unwrap_or(dest),
        );
    }
    if candidates.len() > MAX_SIBLING_LIBRARIES {
        log::warn!(
            "{} has {} sibling DLLs; imported the first {MAX_SIBLING_LIBRARIES}",
            src_dir.display(),
            candidates.len(),
        );
    }
    copied
}

/// List the guest DLLs already sitting in a CAB/ZIP extraction, as
/// paths relative to `game_dir`.
///
/// Unlike [`copy_sibling_libraries`] this copies nothing: an archive
/// import has already written every payload file into `extracted_dir`,
/// so the libraries are where the emulator will look for them. We only
/// record them for [`GameEntry::companions`].
fn record_extracted_libraries(extracted_dir: &Path, game_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(extracted_dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_guest_dll(p))
        .filter_map(|p| p.strip_prefix(game_dir).map(Path::to_path_buf).ok())
        .collect();
    found.sort();
    found
}

fn import_resource_companion_cab(
    game_dir: &Path,
    extracted_dir: &Path,
    index: usize,
    companion: &Path,
) {
    let staging = game_dir.join(format!(".resource-cab-{index}"));
    if fs::create_dir_all(&staging).is_err() {
        return;
    }
    match pocket_cab::extract_with_header(companion, &staging) {
        Ok((companion_files, companion_header)) => {
            if let Some(header) = companion_header.as_ref().filter(|h| h.structured) {
                pocket_cab::materialise_install_header_names(
                    extracted_dir,
                    &companion_files,
                    header,
                );
            } else {
                materialise_legacy_assets(
                    extracted_dir,
                    &companion_files,
                    companion_header
                        .as_ref()
                        .and_then(|h| h.app_name.as_deref()),
                );
            }
            log::info!(
                "imported companion resource cabinet {}",
                companion.display()
            );
        }
        Err(error) => {
            log::warn!(
                "could not import companion resource cabinet {}: {error}",
                companion.display()
            );
        }
    }
    let _ = fs::remove_dir_all(staging);
}

fn find_resource_companion_cabs(source: &Path) -> Vec<PathBuf> {
    let Some(parent) = source.parent() else {
        return Vec::new();
    };
    let source_stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let family = source_stem.split(['-', '_']).next().unwrap_or(&source_stem);
    let mut matches: Vec<PathBuf> = fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path != source)
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("cab"))
        })
        .filter(|path| {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            stem.split(['-', '_'])
                .next()
                .is_some_and(|candidate| candidate == family)
                && (stem.contains("res") || stem.contains("resource"))
        })
        .collect();
    matches.sort();
    matches
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

/// Install directory the cabinet's `_setup.xml` declares.
///
/// `_setup.xml` is the authoritative source: it is the script a real
/// installer follows, so it names the directory the game's own code was
/// compiled to read from. The `.000` install header is only a fallback
/// for legacy cabinets that ship no script, and guessing from the
/// executable's file name is a last resort.
///
/// `install_dirs` (every directory the `FileOperation` block writes
/// into) is preferred over the declared `InstallDir` because the two
/// frequently disagree: Gameloft's Sonic Unleashed declares
/// `%CE1%\SONIC` but installs into `%CE1%\Gameloft\SONIC`, which is
/// the path the game hard-codes. This mirrors how `pockethle run <cab>`
/// resolves the same cabinet, so a title behaves identically whether it
/// is launched from a file or from the library.
fn setup_save_dir(files: &[pocket_cab::CabFile]) -> Option<String> {
    let setup = files
        .iter()
        .find(|f| f.short_name.eq_ignore_ascii_case("_setup.xml"))?;
    let data = fs::read(&setup.extracted_path).ok()?;
    pocket_cab::WinCeSetupScript::parse_bytes(&data)
        .registry
        .into_iter()
        .find(|value| value.name.eq_ignore_ascii_case("SaveDir"))
        .and_then(|value| value.string)
        .map(|value| {
            let mut path = value.replace('/', "\\");
            if !path.starts_with('\\') {
                path.insert(0, '\\');
            }
            if !path.ends_with('\\') {
                path.push('\\');
            }
            path
        })
}

fn setup_registry(files: &[pocket_cab::CabFile]) -> Vec<pocket_cab::SetupRegistryValue> {
    let Some(setup) = files
        .iter()
        .find(|f| f.short_name.eq_ignore_ascii_case("_setup.xml"))
    else {
        return Vec::new();
    };
    fs::read(&setup.extracted_path)
        .ok()
        .map(|data| pocket_cab::WinCeSetupScript::parse_bytes(&data).registry)
        .unwrap_or_default()
}

fn setup_install_dirs(files: &[pocket_cab::CabFile]) -> Vec<String> {
    let Some(setup) = files
        .iter()
        .find(|f| f.short_name.eq_ignore_ascii_case("_setup.xml"))
    else {
        return Vec::new();
    };
    let Ok(data) = fs::read(&setup.extracted_path) else {
        return Vec::new();
    };
    pocket_cab::WinCeSetupScript::parse_bytes(&data).install_dirs
}

fn setup_install_dir(files: &[pocket_cab::CabFile]) -> Option<String> {
    let setup = files
        .iter()
        .find(|f| f.short_name.eq_ignore_ascii_case("_setup.xml"))?;
    let data = fs::read(&setup.extracted_path).ok()?;
    // `install_root` strips the trailing separator; the manifest stores
    // the directory form every other guest path is built against.
    let root = pocket_cab::WinCeSetupScript::parse_bytes(&data).install_root()?;
    Some(format!("{root}\\"))
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

/// `IMAGE_FILE_DLL` in the COFF characteristics word. The only
/// reliable way to tell a library from an executable: a WinCE cabinet
/// stores its payload under generated 8.3 names, so the `.dll`
/// extension is frequently not there to read.
const IMAGE_FILE_DLL: u16 = 0x2000;

/// The COFF header fields we care about at import time.
#[derive(Debug, Clone, Copy)]
struct PeSniff {
    machine: u16,
    characteristics: u16,
}

impl PeSniff {
    fn is_supported_guest(self) -> bool {
        is_supported_guest_machine(self.machine)
    }

    fn is_dll(self) -> bool {
        (self.characteristics & IMAGE_FILE_DLL) != 0
    }
}

/// Cheap PE header sniff: read the COFF machine and characteristics
/// without parsing the whole image.
///
/// Returns `Ok(None)` for non-PE / short files so the caller can scan
/// a whole directory without having to discriminate between "isn't a
/// PE" and "actually failed I/O".
fn sniff_pe(path: &Path) -> std::io::Result<Option<PeSniff>> {
    let mut f = fs::File::open(path)?;
    let mut head = [0u8; 0x40];
    let n = f.read(&mut head)?;
    if n < 0x40 {
        return Ok(None);
    }
    if &head[0..2] != b"MZ" {
        return Ok(None);
    }
    let lfanew = u32::from_le_bytes(head[0x3c..0x40].try_into().unwrap()) as u64;
    f.seek(SeekFrom::Start(lfanew))?;
    // PE signature (4) + machine (2) + sections (2) + timestamp (4)
    // + symbol table (8) + optional header size (2) + characteristics (2).
    let mut coff = [0u8; 24];
    if f.read(&mut coff)? < 24 {
        return Ok(None);
    }
    if &coff[0..4] != b"PE\0\0" {
        return Ok(None);
    }
    Ok(Some(PeSniff {
        machine: u16::from_le_bytes([coff[4], coff[5]]),
        characteristics: u16::from_le_bytes([coff[22], coff[23]]),
    }))
}

/// Cheap PE header sniff: is `path` an ARM or little-endian MIPS PE32.
///
/// Returns `Ok(false)` for non-PE / short / non-ARM files so the
/// caller can scan a whole directory without having to discriminate
/// between "isn't an ARM PE" and "actually failed I/O".
fn is_arm_pe(path: &Path) -> std::io::Result<bool> {
    Ok(sniff_pe(path)?.is_some_and(PeSniff::is_supported_guest))
}

/// Is `path` a guest PE that can serve as a process entry point —
/// i.e. an ARM/MIPS image that is *not* a DLL?
///
/// A resource-only satellite library (Solitaire's `pegcards.dll` is
/// 132 KiB of card artwork with zero exports) looks exactly like a
/// game to a size-ranked scan, so every entry-point search has to
/// filter on this rather than on the file extension.
fn is_guest_exe(path: &Path) -> bool {
    sniff_pe(path)
        .ok()
        .flatten()
        .is_some_and(|pe| pe.is_supported_guest() && !pe.is_dll())
}

/// Is `path` a guest DLL — an ARM/MIPS PE with `IMAGE_FILE_DLL` set?
fn is_guest_dll(path: &Path) -> bool {
    sniff_pe(path)
        .ok()
        .flatten()
        .is_some_and(|pe| pe.is_supported_guest() && pe.is_dll())
}

fn now_unix_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Extract the executable's Windows icon and store it next to the game
/// as `icon.png`, returning the path relative to the game directory.
///
/// Every frontend can then show the game's real icon the way
/// j2me-loader shows a MIDlet icon, instead of a generic placeholder.
/// A missing or unreadable icon is not an import error - we return
/// `None` and let the UI fall back.
fn extract_icon_png(game_dir: &Path, exe_abs: &Path) -> Option<PathBuf> {
    let bytes = fs::read(exe_abs).ok()?;
    let icon = pocket_pe::icon_from_pe_bytes(&bytes).ok().flatten()?;
    let path = game_dir.join("icon.png");
    let file = fs::File::create(&path).ok()?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), icon.width, icon.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    if writer.write_image_data(&icon.rgba).is_err() {
        let _ = fs::remove_file(&path);
        return None;
    }
    drop(writer);
    log::info!(
        "extracted {}x{} icon from {}",
        icon.width,
        icon.height,
        exe_abs.display()
    );
    Some(PathBuf::from("icon.png"))
}

/// Guess the display geometry a cabinet was built for.
///
/// Windows Mobile games ship one build per handset and hard-code that
/// handset's screen: the Smartphone edition of Asphalt 2 3D draws a
/// 240x320 portrait screen, while the Motorola Q9 edition draws 320x240
/// landscape and gets clipped on a portrait framebuffer. Nothing in the
/// cabinet states the resolution, but the payload is named after the
/// target device (`Asphalt2_MOTO_Q9.exe`), so match on the handful of
/// device tags that mean "landscape QWERTY Smartphone". Anything we do
/// not recognise stays portrait, and the per-game Settings sheet can
/// override the guess either way.
/// Guess the emulated panel geometry from the file names a cabinet
/// installs.
///
/// Gameloft shipped one binary per handset and put the device in the
/// name — `Asphalt2_SPV_C600.exe` is the 240x320 portrait build,
/// `Asphalt2_MOTO_Q9.exe` the 320x240 landscape one — and a landscape
/// build lays its HUD out past the right edge of a portrait
/// framebuffer, so getting this wrong visibly clips the game. The
/// device tag is only a hint; the per-game Settings sheet is the
/// override.
///
/// `entry_point` is checked first because it is the long name restored
/// from `_setup.xml`; the raw cabinet entries only carry generated 8.3
/// names like `ASPHAL~1.001`, which say nothing about the device.
fn guess_screen(
    entry_point: &Path,
    files: &[pocket_cab::CabFile],
    app_name: Option<&str>,
) -> ScreenPref {
    const LANDSCAPE_TAGS: [&str; 5] = ["moto_q", "motoq", "_q9", "_q8", "_q11"];
    const WVGA_TAGS: [&str; 7] = [
        "wvga", "touch_hd", "hd2", "hd7", "hd_game", "480x800", "asphalt4",
    ];
    let names = std::iter::once(entry_point.to_path_buf())
        .chain(app_name.map(PathBuf::from))
        .chain(files.iter().map(|f| f.extracted_path.clone()))
        .filter_map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_ascii_lowercase())
        });
    for name in names {
        if LANDSCAPE_TAGS.iter().any(|tag| name.contains(tag)) {
            return ScreenPref::Landscape;
        }
        if WVGA_TAGS.iter().any(|tag| name.contains(tag)) {
            return ScreenPref::Wvga;
        }
    }
    ScreenPref::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(name: &str) -> PathBuf {
        // Tests run in parallel and `now_unix_seconds` has one-second
        // granularity, so the timestamp alone is not unique: two tests
        // starting in the same second used to share a directory and
        // read each other's fixture files. A process-wide counter makes
        // every path distinct.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "pockethle-library-test-{}-{}-{}",
            name,
            now_unix_seconds(),
            unique
        ));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn entry_with_install_dir(install_dir: Option<&str>) -> GameEntry {
        GameEntry {
            id: "spore".to_string(),
            display_name: "Spore v1.0.4".to_string(),
            provider: None,
            executable: PathBuf::from("extracted/spore.exe"),
            source_cab: "spore.cab".to_string(),
            install_dir: install_dir.map(str::to_string),
            install_dirs: Vec::new(),
            save_prefix: None,
            registry: Vec::new(),
            imported_at: 0,
            settings: GameSettings::default(),
            icon: None,
            companions: Vec::new(),
        }
    }

    #[test]
    fn setup_xml_install_dir_wins_over_the_binary_header() {
        // A cabinet whose `_setup.xml` installs into a nested vendor
        // directory. The library must follow the script, not the
        // executable's own file name.
        let root = tmpdir("setup_install_dir_vendor_subdir");
        std::fs::create_dir_all(&root).unwrap();
        let xml = root.join("_setup.xml");
        std::fs::write(
            &xml,
            concat!(
                "<wap-provisioningdoc>",
                "<characteristic type=\"Install\">",
                "<parm name=\"InstallDir\" value=\"%CE1%\\SONIC\" />",
                "</characteristic>",
                "<characteristic type=\"FileOperation\">",
                "<characteristic type=\"%CE1%\\Gameloft\\SONIC\" translation=\"install\" />",
                "</characteristic>",
                "</wap-provisioningdoc>",
            ),
        )
        .unwrap();
        let files = vec![pocket_cab::CabFile {
            short_name: "_setup.xml".to_string(),
            extracted_path: xml,
            size: 0,
        }];
        assert_eq!(
            setup_install_dir(&files).as_deref(),
            Some("\\Program Files\\Gameloft\\SONIC\\")
        );
    }

    #[test]
    fn setup_install_dir_is_absent_without_a_script() {
        assert!(setup_install_dir(&[]).is_none());
    }

    #[test]
    fn setup_install_dir_skips_the_start_menu() {
        // Asphalt 2 3D's script makes `%CE2%\\Start Menu` before it
        // makes `%InstallDir%`, so declaration order alone would pick
        // the shell folder the shortcut lives in.
        let root = tmpdir("setup_install_dir_start_menu");
        fs::create_dir_all(&root).unwrap();
        let xml = root.join("_setup.xml");
        fs::write(
            &xml,
            concat!(
                "<wap-provisioningdoc>",
                "<characteristic type=\"Install\">",
                "<parm name=\"InstallDir\" value=\"%CE1%\\Asphalt 2 3D\" />",
                "</characteristic>",
                "<characteristic type=\"FileOperation\">",
                "<characteristic type=\"%CE2%\\Start Menu\">",
                "<characteristic type=\"MakeDir\" />",
                "</characteristic>",
                "<characteristic type=\"%InstallDir%\">",
                "<characteristic type=\"MakeDir\" />",
                "</characteristic>",
                "</characteristic>",
                "</wap-provisioningdoc>",
            ),
        )
        .unwrap();
        let files = vec![pocket_cab::CabFile {
            short_name: "_setup.xml".to_string(),
            extracted_path: xml,
            size: 0,
        }];
        assert_eq!(
            setup_install_dir(&files).as_deref(),
            Some("\\Program Files\\Asphalt 2 3D\\")
        );
    }

    #[test]
    fn guest_paths_follow_the_cabinet_install_dir() {
        let entry = entry_with_install_dir(Some("\\Program Files\\EA\\Spore v1.0.4\\"));
        assert_eq!(
            entry.guest_install_prefix().as_deref(),
            Some("\\Program Files\\EA\\Spore v1.0.4\\")
        );
        assert_eq!(
            entry.guest_exe_path().as_deref(),
            Some("\\Program Files\\EA\\Spore v1.0.4\\spore.exe")
        );
    }

    #[test]
    fn guest_prefix_normalises_a_missing_separator() {
        let entry = entry_with_install_dir(Some("\\Program Files\\Asphalt 2 3D"));
        assert_eq!(
            entry.guest_exe_path().as_deref(),
            Some("\\Program Files\\Asphalt 2 3D\\spore.exe")
        );
    }

    #[test]
    fn guest_paths_are_absent_without_an_install_dir() {
        assert!(entry_with_install_dir(None)
            .guest_install_prefix()
            .is_none());
        assert!(entry_with_install_dir(Some("  "))
            .guest_exe_path()
            .is_none());
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

        lib.config_mut().orientation = "landscape".to_string();
        lib.save().unwrap();
        let lib3 = Library::open(&root).unwrap();
        assert_eq!(lib3.config().orientation, "landscape");
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

    /// Write a PE that is just enough header for [`sniff_pe`]: an `MZ`
    /// stub whose `e_lfanew` points at a COFF header carrying `machine`
    /// and `characteristics`. No sections, no optional header — the
    /// importer's accept/reject decision is made entirely from these
    /// two fields.
    fn write_stub_pe(path: &Path, machine: u16, characteristics: u16) {
        let mut buf = vec![0u8; 0x40];
        buf[0..2].copy_from_slice(b"MZ");
        buf[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        let mut coff = vec![0u8; 24];
        coff[0..4].copy_from_slice(b"PE\0\0");
        coff[4..6].copy_from_slice(&machine.to_le_bytes());
        coff[22..24].copy_from_slice(&characteristics.to_le_bytes());
        buf.extend_from_slice(&coff);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, &buf).unwrap();
    }

    const MACHINE_ARM: u16 = 0x01c0;
    const MACHINE_X86: u16 = 0x014c;

    #[test]
    fn a_dll_handed_in_as_a_game_is_rejected_as_a_library() {
        // The user picks `pegcards.dll` in the import dialog. It is a
        // real ARM PE the game needs, so the machine check passes and
        // the old importer accepted it as a game that then could not
        // start. The DLL characteristic bit has to be what decides.
        let root = tmpdir("import_dll_rejected");
        let src = tmpdir("import_dll_rejected_src");
        fs::create_dir_all(&src).unwrap();
        let dll = src.join("pegcards.dll");
        write_stub_pe(&dll, MACHINE_ARM, IMAGE_FILE_DLL);

        let mut lib = Library::open(&root).unwrap();
        match lib.import_exe(&dll) {
            Err(LibraryError::IsLibrary(name)) => assert_eq!(name, "pegcards.dll"),
            other => panic!("expected IsLibrary, got {other:?}"),
        }
        // Nothing may be left behind for a rejected import.
        assert!(!root.join("games").join("pegcards").exists());
    }

    #[test]
    fn importing_an_exe_brings_its_satellite_libraries_along() {
        // Solitaire's layout: the executable plus the resource-only DLL
        // holding every card face. The emulator resolves the DLL by
        // name in the executable's own directory, so the import has to
        // put them back in one directory or the game draws nothing.
        let root = tmpdir("import_exe_companions");
        let src = tmpdir("import_exe_companions_src");
        fs::create_dir_all(&src).unwrap();
        let exe = src.join("solitare.exe");
        write_stub_pe(&exe, MACHINE_ARM, 0);
        write_stub_pe(&src.join("pegcards.dll"), MACHINE_ARM, IMAGE_FILE_DLL);
        // An unrelated host DLL in the same folder must not come along.
        write_stub_pe(&src.join("msvcrt.dll"), MACHINE_X86, IMAGE_FILE_DLL);

        let mut lib = Library::open(&root).unwrap();
        let entry = lib.import_exe(&exe).unwrap().clone();
        assert_eq!(entry.id, "solitare");
        assert_eq!(
            entry.companions,
            vec![PathBuf::from("extracted/pegcards.dll")]
        );

        let extracted = root.join("games").join("solitare").join("extracted");
        assert!(extracted.join("solitare.exe").is_file());
        assert!(extracted.join("pegcards.dll").is_file());
        assert!(!extracted.join("msvcrt.dll").exists());
    }

    #[test]
    fn an_x86_exe_is_still_rejected_as_the_wrong_architecture() {
        let root = tmpdir("import_x86_rejected");
        let src = tmpdir("import_x86_rejected_src");
        fs::create_dir_all(&src).unwrap();
        let exe = src.join("setup.exe");
        write_stub_pe(&exe, MACHINE_X86, 0);

        let mut lib = Library::open(&root).unwrap();
        assert!(matches!(
            lib.import_exe(&exe),
            Err(LibraryError::NotArmPe(_))
        ));
    }

    #[test]
    fn guest_exe_and_dll_sniffing_disagree_only_on_the_dll_bit() {
        let dir = tmpdir("sniff_dll_bit");
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("game.exe");
        let dll = dir.join("cards.dll");
        write_stub_pe(&exe, MACHINE_ARM, 0);
        write_stub_pe(&dll, MACHINE_ARM, IMAGE_FILE_DLL);

        assert!(is_guest_exe(&exe));
        assert!(!is_guest_dll(&exe));
        assert!(is_guest_dll(&dll));
        // The important half: a satellite DLL must never be picked as
        // an entry point by the archive scans.
        assert!(!is_guest_exe(&dll));

        // A truncated file is neither.
        let stub = dir.join("tiny.exe");
        fs::write(&stub, b"MZ").unwrap();
        assert!(!is_guest_exe(&stub));
        assert!(!is_guest_dll(&stub));
    }

    #[test]
    fn resource_companion_cabs_are_selected_by_game_family() {
        let root = tmpdir("resource_companion_selection");
        fs::create_dir_all(&root).unwrap();
        for name in ["wwp.CAB", "wwp-res.CAB", "wwp-2003.CAB", "other-res.CAB"] {
            fs::write(root.join(name), []).unwrap();
        }
        let found = find_resource_companion_cabs(&root.join("wwp.CAB"));
        assert_eq!(found, vec![root.join("wwp-res.CAB")]);
    }

    #[test]
    fn the_handheld_pc_screen_preference_is_480x320() {
        assert_eq!(ScreenPref::Hpc.size(), (480, 320));
        // Round-trips through the manifest, so an existing library
        // keeps the setting across a restart.
        let json = serde_json::to_string(&ScreenPref::Hpc).unwrap();
        assert_eq!(
            serde_json::from_str::<ScreenPref>(&json).unwrap(),
            ScreenPref::Hpc
        );
    }
}
