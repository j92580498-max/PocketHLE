#![allow(clippy::chunks_exact_to_as_chunks)]
//! Linux command-line frontend for PocketHLE.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use pocket_core::Emulator;

mod archive;
mod managed;

#[derive(Parser, Debug)]
#[command(
    name = "pockethle",
    version,
    about = "High-level Windows Mobile / Pocket PC emulator (CLI frontend)",
    long_about = None
)]
struct Cli {
    /// Logging verbosity (-v info, -vv debug, -vvv trace).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

#[allow(clippy::large_enum_variant)]
#[derive(Subcommand, Debug)]
enum Command {
    /// Print info about a PE32 file (game .exe extracted from a CAB).
    PeInfo { path: PathBuf },
    /// Extract every file from a Windows Mobile `.CAB` into a directory.
    UnpackCab { cab: PathBuf, out_dir: PathBuf },
    /// Extract a CAB and print info about the largest PE inside.
    InspectCab {
        cab: PathBuf,
        /// Optional output directory (defaults to a temp dir under
        /// `$XDG_CACHE_HOME/pockethle`).
        #[arg(short, long)]
        out_dir: Option<PathBuf>,
    },
    /// Import a `.CAB` / `.ZIP` / `.exe` into the launcher library so
    /// the desktop and Android frontends can see it.
    ///
    /// Same code path the GUI's "Import" button uses, which makes it
    /// possible to set a library up from a script or a headless host.
    Import {
        /// Archive or executable to import.
        path: PathBuf,
        /// Library root. Defaults to `POCKETHLE_LIBRARY`, else
        /// `<documents>/PocketHLE`.
        #[arg(long)]
        library: Option<PathBuf>,
    },
    /// Render a deterministic test pattern through the framebuffer
    /// and GDI subsystems and write the result as a PPM. This proves
    /// the rendering substrate is wired without needing a full game
    /// to actually reach `WinMain`.
    RenderDemo {
        /// Path to write the generated PPM (defaults to `./demo.ppm`).
        #[arg(short, long, default_value = "demo.ppm")]
        out: PathBuf,
    },
    /// Run a PE file in the emulator.
    ///
    /// A Pocket PC `.exe`, `.cab`, or `.zip` may be supplied directly.
    /// ARM PE files use `--cpu unicorn`; MIPS Pocket PC PE files use
    /// `--cpu mips`. Archives are auto-extracted and mounted at
    /// `\\Application\\` so the guest can load its resources.
    /// `--cpu stub` is available for trace-only analysis.
    Run {
        path: PathBuf,
        /// CPU backend.
        #[arg(long, default_value = DEFAULT_CPU_BACKEND)]
        cpu: CpuBackend,
        /// Halt as soon as the guest calls an unimplemented API.
        #[arg(long, default_value_t = false)]
        halt_on_unimplemented: bool,
        /// Maximum number of host-resumed slices (each slice can
        /// run up to `--instructions-per-slice` instructions).
        ///
        /// Real PPC2003 games typically need a few hundred thousand
        /// slices to finish their CRT init, build their soft-float
        /// lookup tables and load bitmap resources before the first
        /// `WM_PAINT` is delivered, so the default is high enough
        /// that `pockethle run game.cab` produces visible output
        /// out of the box. Pass a smaller value for fast smoke
        /// tests, or `0` for no upper bound.
        #[arg(long, default_value_t = 50_000_000)]
        max_slices: u64,
        #[arg(long, default_value_t = 1_000_000)]
        instructions_per_slice: u64,
        /// Write a JSON-lines trace of every dispatched API call to
        /// the given file. Useful for diffing runs and for offline
        /// analysis (`jq`, etc.).
        #[arg(long)]
        trace_json: Option<PathBuf>,
        /// Mount a host directory as the WinCE `\Application\` root.
        /// `CreateFileW` requests inside that prefix are translated
        /// to host paths.
        #[arg(long)]
        rom_dir: Option<PathBuf>,
        /// Mount the host directory at a custom guest prefix instead
        /// of `\Application\` (e.g. `--rom-prefix \\Storage\\`).
        #[arg(long, default_value = "\\Application\\")]
        rom_prefix: String,
        /// Guest path `GetModuleFileNameW` reports for the running
        /// image. Games derive asset paths from it, so running a bare
        /// `.exe` (outside its CAB) usually needs this to match the
        /// on-device install path.
        #[arg(long)]
        module_path: Option<String>,
        /// Record everything the game sends to `waveOut*` /
        /// `PlaySound` into a 16-bit PCM WAV file. Works with no host
        /// audio device, which makes it the way to check a game's
        /// sound in CI or over SSH.
        #[arg(long, value_name = "FILE")]
        dump_audio_to: Option<PathBuf>,
        /// Open a host window and render the framebuffer live.
        /// Requires the `display` cargo feature.
        #[arg(long, default_value_t = false)]
        display: bool,
        /// Periodically write the framebuffer as PPM files into the
        /// given directory (one file per emit). Works in any
        /// environment, no extra dependencies.
        #[arg(long)]
        dump_frames_to: Option<PathBuf>,
        /// Only write every Nth changed frame when `--dump-frames-to`
        /// is set. A GDI title bumps the frame counter once per blit,
        /// so a plain dump fills the disk long before the game leaves
        /// its splash screen; `--dump-frame-stride 50` keeps the run
        /// observable without that. `--max-frames` still counts files
        /// written, not frames rendered.
        #[arg(long, value_name = "N", default_value_t = 1)]
        dump_frame_stride: u64,
        /// Stop emulation after this many distinct rendered frames.
        /// Combined with `--dump-frames-to`, gives a deterministic
        /// way to capture proof-of-rendering screenshots.
        #[arg(long, default_value_t = 0)]
        max_frames: u64,
        /// Queue synthetic taps. Repeat the option to exercise several
        /// on-screen buttons in one run. Coordinates use the emulated
        /// framebuffer, which is 240x320 unless `--screen` says
        /// otherwise. Prefix with `<FRAME>:` to deliver the tap once
        /// that many frames have been rendered instead of before the
        /// first game message — e.g. `--tap 1200:160,90` clicks a menu
        /// entry that only exists after the splash.
        #[arg(long, value_name = "[FRAME:]X,Y")]
        tap: Vec<String>,
        /// Queue a virtual-key press. Repeat the option for a sequence,
        /// using names such as enter, up, left, right, down, a, b, c,
        /// start, 0-9, or a hexadecimal VK code such as 0x25. `a` /
        /// `select` and `start` are the device face buttons reported by
        /// `GXGetDefaultKeys`, which is what GAPI games compare against;
        /// `enter` is delivered as the A button for those titles and as
        /// `VK_RETURN` for everything else. Accepts the same
        /// `<FRAME>:` prefix as `--tap`.
        #[arg(long, value_name = "[FRAME:]KEY")]
        key: Vec<String>,
        /// Patch raw bytes into the guest image before execution.
        /// Format: `<hex_addr>=<hex_bytes>`, e.g.
        /// `--patch 0x000247dc=00000ae0` will overwrite four bytes at
        /// VA 0x247dc with `0x00 0x00 0x0a 0xe0`. May be passed
        /// multiple times. Used to bypass hostile static initializers
        /// in legacy CRTs.
        #[arg(long, value_name = "ADDR=HEX")]
        patch: Vec<String>,
        /// Add an instruction-level breakpoint at the given guest VA.
        /// When the CPU reaches it, PocketHLE dumps the full register
        /// state and halts. Used to diagnose where unexpected control
        /// flow comes from. May be passed multiple times.
        #[arg(long, value_name = "VA")]
        watch: Vec<String>,
        /// Override the synthetic `WM_PAINT` message budget. After this
        /// many `GetMessage` / `PeekMessage` calls the dispatcher posts
        /// `WM_QUIT` and the game shuts down. `0` means unlimited.
        /// Default: 240.
        #[arg(long, default_value_t = 240)]
        message_budget: u64,
        /// Emulated screen geometry, `<WIDTH>x<HEIGHT>`. The default
        /// 240x320 is the Pocket PC portrait LCD. Windows Mobile
        /// Smartphone titles (e.g. the Motorola Q9 build of Asphalt 2)
        /// need the landscape `320x240` instead — they size their back
        /// buffer from `GetSystemMetrics`, so a portrait screen makes
        /// them render wider than the framebuffer and get clipped.
        /// Archives that identify the device they shipped on — a
        /// Gizmondo card image, say — already come up on its screen, so
        /// this only has to be passed to override that.
        #[arg(long, value_name = "WxH")]
        screen: Option<String>,
        /// Run a managed x86 .NET Compact Framework image through a
        /// compatible host runtime. This is the practical fallback for
        /// WinForms applications until CLR HLE is implemented.
        #[arg(long)]
        managed_runtime: Option<PathBuf>,
        /// Directory containing compatibility assemblies such as
        /// Microsoft.VisualBasic.dll.
        #[arg(long)]
        managed_runtime_path: Option<PathBuf>,
        /// Seconds to keep a managed application alive before stopping it.
        #[arg(long, default_value_t = 5)]
        managed_duration: u64,
    },
}

#[derive(Clone, Debug, clap::ValueEnum)]
enum CpuBackend {
    Stub,
    #[cfg(feature = "unicorn")]
    Unicorn,
    #[cfg(feature = "unicorn")]
    Mips,
}

/// CPU backend used when `--cpu` is not specified. We pick `unicorn`
/// when the binary is compiled with that feature so that the default
/// `pockethle run …` invocation actually runs guest ARM code; the
/// user can still pass `--cpu stub` for trace-only analysis.
#[cfg(feature = "unicorn")]
const DEFAULT_CPU_BACKEND: &str = "unicorn";
#[cfg(not(feature = "unicorn"))]
const DEFAULT_CPU_BACKEND: &str = "stub";

fn main() -> Result<()> {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();

    match cli.command {
        Command::PeInfo { path } => cmd_pe_info(&path),
        Command::UnpackCab { cab, out_dir } => cmd_unpack_cab(&cab, &out_dir),
        Command::InspectCab { cab, out_dir } => cmd_inspect_cab(&cab, out_dir.as_deref()),
        Command::Import { path, library } => cmd_import(&path, library),
        Command::RenderDemo { out } => cmd_render_demo(&out),
        Command::Run {
            path,
            cpu,
            halt_on_unimplemented,
            max_slices,
            instructions_per_slice,
            trace_json,
            rom_dir,
            rom_prefix,
            module_path,
            display,
            dump_audio_to,
            dump_frames_to,
            dump_frame_stride,
            max_frames,
            tap,
            key,
            patch,
            watch,
            message_budget,
            screen,
            managed_runtime,
            managed_runtime_path,
            managed_duration,
        } => cmd_run(
            &path,
            cpu,
            halt_on_unimplemented,
            max_slices,
            instructions_per_slice,
            trace_json.as_deref(),
            rom_dir.as_deref(),
            &rom_prefix,
            module_path.as_deref(),
            display,
            dump_audio_to.as_deref(),
            dump_frames_to.as_deref(),
            dump_frame_stride,
            max_frames,
            &tap,
            &key,
            &patch,
            &watch,
            message_budget,
            screen.as_deref(),
            managed_runtime.as_deref(),
            managed_runtime_path.as_deref(),
            managed_duration,
        ),
    }
}

fn cmd_pe_info(path: &std::path::Path) -> Result<()> {
    let img = pocket_core::pe::load_file(path).context("loading PE")?;
    println!("Source: {}", img.source_path);
    println!(
        "Machine: 0x{:04x} ({})  Subsystem: {}",
        img.machine,
        img.machine_name(),
        img.subsystem
    );
    println!(
        "ImageBase: 0x{:08x}   SizeOfImage: 0x{:x}   EntryPoint: 0x{:08x}",
        img.image_base,
        img.size_of_image,
        img.entry_va()
    );
    if let Some(runtime) = img.managed_runtime.as_deref() {
        println!("Managed image: CLR metadata {runtime} (requires .NET Compact Framework)");
    }
    println!("Sections:");
    for s in &img.sections {
        println!(
            "  {:>8}  va=0x{:08x}  size=0x{:06x}  flags=0x{:08x}{}{}{}",
            s.name,
            img.image_base + s.virtual_address,
            s.virtual_size,
            s.characteristics,
            if s.is_readable() { " R" } else { "" },
            if s.is_writable() { " W" } else { "" },
            if s.is_executable() { " X" } else { "" },
        );
    }
    println!("Imports:");
    for (dll, syms) in pocket_core::pe::imports_by_dll(&img) {
        println!("  {} ({} symbols)", dll, syms.len());
        for s in syms {
            let mut display = s.binding.to_string_short();
            if let pocket_core::pe::ImportBinding::Ordinal(o) = &s.binding {
                if let Some(name) = pocket_core::winceapi::resolve_ordinal(&dll, *o) {
                    display = format!("{name} (ord {o})");
                }
            }
            println!("    iat=0x{:08x}  {}", s.iat_va, display);
        }
    }
    Ok(())
}

fn cmd_unpack_cab(cab: &std::path::Path, out_dir: &std::path::Path) -> Result<()> {
    let entries = pocket_core::cab::extract_all(cab, out_dir).context("extracting cab")?;
    println!("Extracted {} files to {}", entries.len(), out_dir.display());
    for e in entries {
        println!(
            "  {:>14}  {:>8} bytes  {}",
            e.short_name,
            e.size,
            e.extracted_path.display()
        );
    }
    Ok(())
}

fn cmd_inspect_cab(cab: &std::path::Path, out_dir: Option<&std::path::Path>) -> Result<()> {
    let dest = match out_dir {
        Some(p) => p.to_path_buf(),
        None => {
            let base = std::env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    let home = std::env::var_os("HOME").unwrap_or_default();
                    PathBuf::from(home).join(".cache")
                });
            base.join("pockethle").join("cab-extracted")
        }
    };
    std::fs::create_dir_all(&dest)?;
    let (files, header) = pocket_core::cab::extract_with_header(cab, &dest)?;
    if let Some(h) = &header {
        println!(
            "Install header: provider={:?}, app_name={:?}",
            h.provider, h.app_name
        );
    }
    println!("Files ({}):", files.len());
    let mut largest: Option<&pocket_core::cab::CabFile> = None;
    for f in &files {
        println!("  {:>14}  {:>8} bytes", f.short_name, f.size);
        if largest.map(|l| l.size).unwrap_or(0) < f.size {
            largest = Some(f);
        }
    }
    if let Some(big) = largest {
        println!("\nLargest file: {}", big.short_name);
        cmd_pe_info(&big.extracted_path)?;
    }
    let setup = files
        .iter()
        .find(|f| f.short_name.eq_ignore_ascii_case("_setup.xml"))
        .and_then(|f| std::fs::read(&f.extracted_path).ok())
        .map(|bytes| pocket_core::cab::WinCeSetupScript::parse_bytes(&bytes));
    if let Some(setup) = setup {
        println!(
            "Setup: install_dir={:?}, install_root={:?}, shortcut={:?}, renames={}",
            setup.install_dir,
            setup.install_root(),
            setup.shortcut_target,
            setup.renames.len()
        );
        for (short, long) in setup.renames.iter().filter(|(_, long)| {
            long.to_ascii_lowercase().ends_with(".exe")
                || long.to_ascii_lowercase().ends_with(".dll")
        }) {
            println!("  payload: {short} -> {long}");
        }
    }
    Ok(())
}

/// Add a game to the launcher library used by the GUI frontends.
fn cmd_import(path: &std::path::Path, library: Option<PathBuf>) -> Result<()> {
    let root = library.unwrap_or_else(pocket_library::default_library_root);
    let mut lib = pocket_library::Library::open(&root)
        .with_context(|| format!("opening library at {}", root.display()))?;
    let entry = lib
        .import_any(path)
        .with_context(|| format!("importing {}", path.display()))?;
    println!(
        "Imported \"{}\" as id {} into {}",
        entry.display_name,
        entry.id,
        root.display()
    );
    println!("  executable: {}", entry.executable.display());
    Ok(())
}

fn cmd_render_demo(out_path: &std::path::Path) -> Result<()> {
    use pocket_core::kernel::framebuffer::{pack_rgb565, FB_HEIGHT, FB_WIDTH};
    use pocket_core::kernel::gdi::{Bitmap, Surface};
    use pocket_core::kernel::Framebuffer;

    let mut fb = Framebuffer::default();

    // Sky gradient (top half) directly via the framebuffer primitive.
    for y in 0..(FB_HEIGHT as i32 / 2) {
        let t = y as u32;
        let r = 0x40 + t / 2;
        let g = 0x80 + t / 3;
        let b = 0xff_u32.saturating_sub(t);
        let pixel = pack_rgb565(r as u8, g as u8, b as u8);
        for x in 0..FB_WIDTH as i32 {
            fb.put_pixel(x, y, pixel);
        }
    }
    // Ground via fill_rect — same path GDI `FillRect` exercises.
    Surface::Screen(&mut fb).fill_rect(
        0,
        FB_HEIGHT as i32 / 2,
        FB_WIDTH as i32,
        FB_HEIGHT as i32 / 2,
        pack_rgb565(0x4a, 0x35, 0x1f),
    );

    // Off-screen 32×32 ball drawn in a memory bitmap, then blitted —
    // the same code path GDI `BitBlt` exercises.
    let ball_w = 32u32;
    let ball_h = 32u32;
    let mut ball = Bitmap::new(ball_w, ball_h);
    for y in 0..ball_h as i32 {
        for x in 0..ball_w as i32 {
            let dx = x - 16;
            let dy = y - 16;
            let d2 = dx * dx + dy * dy;
            let pixel = if d2 <= 15 * 15 {
                pack_rgb565(0xff, 0xd0, 0x20)
            } else {
                pack_rgb565(0, 0, 0)
            };
            let off = (y as u32 * ball_w + x as u32) as usize * 2;
            ball.pixels[off..off + 2].copy_from_slice(&pixel.to_le_bytes());
        }
    }
    let ball_pixels = ball.pixels.clone();
    Surface::Screen(&mut fb).blit_from_bytes(
        100,
        140,
        0,
        0,
        ball_w as i32,
        ball_h as i32,
        &ball_pixels,
        ball_w,
        ball_h,
    );

    // Red border via stroke_rect.
    Surface::Screen(&mut fb).stroke_rect(
        0,
        0,
        FB_WIDTH as i32,
        FB_HEIGHT as i32,
        pack_rgb565(0xff, 0, 0),
    );

    let ppm = fb.snapshot_ppm();
    std::fs::write(out_path, &ppm).context("writing PPM")?;
    println!(
        "Wrote {}×{} demo PPM to {}",
        FB_WIDTH,
        FB_HEIGHT,
        out_path.display()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_run(
    path: &std::path::Path,
    backend: CpuBackend,
    halt_on_unimplemented: bool,
    max_slices: u64,
    instructions_per_slice: u64,
    trace_json: Option<&std::path::Path>,
    rom_dir: Option<&std::path::Path>,
    rom_prefix: &str,
    module_path: Option<&str>,
    display: bool,
    dump_audio_to: Option<&std::path::Path>,
    dump_frames_to: Option<&std::path::Path>,
    dump_frame_stride: u64,
    max_frames: u64,
    taps: &[String],
    keys: &[String],
    patches: &[String],
    watches: &[String],
    message_budget: u64,
    screen: Option<&str>,
    managed_runtime: Option<&std::path::Path>,
    managed_runtime_path: Option<&std::path::Path>,
    managed_duration: u64,
) -> Result<()> {
    let screen = screen.map(parse_screen_size).transpose()?;
    let mut emu = match backend {
        CpuBackend::Stub => Emulator::with_stub_cpu(),
        #[cfg(feature = "unicorn")]
        CpuBackend::Unicorn => Emulator::with_unicorn_cpu()?,
        #[cfg(feature = "unicorn")]
        CpuBackend::Mips => Emulator::with_unicorn_cpu_for_arch(pocket_core::cpu::Arch::Mips)?,
    };
    emu.set_halt_on_unimplemented(halt_on_unimplemented);
    emu.max_slices = max_slices;
    emu.instruction_budget_per_slice = instructions_per_slice;
    if let Some(p) = trace_json {
        let f = std::fs::File::create(p)
            .with_context(|| format!("creating trace file {}", p.display()))?;
        emu.set_trace_sink(Box::new(std::io::BufWriter::new(f)));
        println!("Tracing API calls to {} (JSON lines)", p.display());
    }

    // Resolve `path` into the actual PE to load. For `.cab` / `.zip`
    // archives this auto-extracts into a temp dir held alive by
    // `_launcher` for the duration of cmd_run.
    let _launcher = archive::prepare(path)
        .with_context(|| format!("preparing {} for execution", path.display()))?;
    println!("{}", _launcher.origin);

    let image = pocket_core::pe::load_file(&_launcher.exe).context("loading PE")?;
    println!(
        "Loaded {} ({} machine), {} sections, {} imports{}",
        image.source_path,
        image.machine_name(),
        image.sections.len(),
        image.imports.len(),
        image
            .managed_runtime
            .as_deref()
            .map(|v| format!(", CLR runtime {v}"))
            .unwrap_or_default()
    );
    if image.managed_runtime.is_some() {
        let result = managed::run(
            &_launcher.exe,
            _launcher.mount_dir.as_deref(),
            managed_runtime,
            managed_runtime_path,
            managed_duration,
            taps,
            keys,
            dump_frames_to,
        )?;
        if result.terminated_by_timeout {
            println!(
                "Managed application stopped after {} seconds",
                managed_duration
            );
        }
        return Ok(());
    }
    emu.load_pe(&_launcher.exe)?;
    // An explicit `--screen` wins; otherwise a launcher that recognised
    // the device the game shipped on picks the geometry, because the
    // game reads it during start-up and cannot be told later.
    if let Some((w, h)) = screen.or(_launcher.native_screen) {
        emu.set_screen_size(w, h);
        println!("Emulated display set to {w}x{h}");
    }
    if let Some(path) = dump_audio_to {
        emu.capture_audio_to(path)
            .with_context(|| format!("opening audio capture {}", path.display()))?;
        println!("Recording guest audio to {}", path.display());
    }
    if let Some(dir) = rom_dir {
        emu.mount_dir(rom_prefix, dir);
        println!(
            "Mounted host directory {} at guest prefix {:?}",
            dir.display(),
            rom_prefix
        );
    } else if let Some(dir) = _launcher.mount_dir.as_deref() {
        emu.mount_dir(rom_prefix, dir);
        println!(
            "Auto-mounted extracted dir {} at guest prefix {:?}",
            dir.display(),
            rom_prefix
        );
    }
    for (prefix, dir) in &_launcher.extra_mounts {
        emu.mount_dir(prefix, dir);
        println!(
            "Auto-mounted extracted dir {} at guest prefix {:?}",
            dir.display(),
            prefix
        );
    }
    if let Some(save_prefix) = &_launcher.save_prefix {
        let save_dir = pocket_library::save_data::save_dir_for(
            &pocket_library::default_library_root(),
            &archive::save_id(path),
        );
        emu.mount_save_dir(save_prefix, &save_dir);
        println!(
            "Persistent save data: {} -> {save_prefix:?}",
            save_dir.display()
        );
    }
    let effective_module_path = module_path.or(_launcher.guest_exe_path.as_deref());
    if let Some(path) = effective_module_path {
        emu.set_module_path(path);
        println!("GetModuleFileNameW will report {path:?}");
        // Relative guest paths resolve against the executable's
        // directory: Astraware titles enumerate `.\*.pdb` next to the
        // binary, and a bare `\` default sends the search to the
        // device root where nothing is mounted.
        if let Some(cut) = path.rfind('\\') {
            let dir = &path[..cut + 1];
            emu.set_default_dir(dir);
            println!("Relative guest paths resolve against {dir:?}");
        }
    }
    // Replay the cabinet's `_setup.xml` registry section. A real
    // Pocket PC installer writes these values before the game's first
    // launch, and titles read them back to locate their own data —
    // Astraware Bejeweled calls `ExitProcess(0x42)` when
    // `HKLM\SOFTWARE\Apps\Astraware Bejeweled\SaveDir` is missing.
    for entry in &_launcher.registry {
        let value = if let Some(text) = entry.string.as_deref() {
            pocket_core::kernel::registry::RegistryValue::Sz(text.to_string())
        } else if let Some(number) = entry.dword {
            pocket_core::kernel::registry::RegistryValue::Dword(number)
        } else {
            continue;
        };
        println!(
            "Installed registry value {}\\{} from the cabinet's install script",
            entry.key, entry.name
        );
        emu.set_registry_value(&entry.key, &entry.name, value);
    }
    let has_save_dir = _launcher
        .registry
        .iter()
        .any(|entry| entry.name.eq_ignore_ascii_case("SaveDir"));
    if !has_save_dir {
        if let Some(install_dir) = _launcher
            .registry
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case("InstallDir"))
            .and_then(|entry| entry.string.clone())
        {
            println!(
                "CAB has no SaveDir; using InstallDir {install_dir:?} for the app registry fallback"
            );
            emu.set_registry_value(
                r"HKLM\SOFTWARE\Apps\Astraware Cubis",
                "SaveDir",
                pocket_core::kernel::registry::RegistryValue::Sz(install_dir),
            );
        }
    }
    emu.set_synthetic_message_budget(message_budget);
    let (fb_w, fb_h) = emu
        .process()
        .map(|p| (p.state.framebuffer.width, p.state.framebuffer.height))
        .unwrap_or((
            pocket_core::kernel::FB_WIDTH,
            pocket_core::kernel::FB_HEIGHT,
        ));
    let mut scheduled: Vec<(u64, pocket_core::kernel::InputEvent)> = Vec::new();
    for tap in taps {
        let (at_frame, body) = split_frame_prefix(tap);
        let (x, y) = body
            .split_once(',')
            .with_context(|| format!("invalid --tap {tap:?}; expected [FRAME:]X,Y"))?;
        let x = x
            .trim()
            .parse::<u16>()
            .with_context(|| format!("invalid tap x in {tap:?}"))?;
        let y = y
            .trim()
            .parse::<u16>()
            .with_context(|| format!("invalid tap y in {tap:?}"))?;
        if x >= fb_w as u16 || y >= fb_h as u16 {
            anyhow::bail!("tap {tap:?} is outside the {fb_w}x{fb_h} framebuffer");
        }
        let down = pocket_core::kernel::InputEvent::PointerDown { x, y };
        let up = pocket_core::kernel::InputEvent::PointerUp { x, y };
        match at_frame {
            Some(f) => {
                scheduled.push((f, down));
                scheduled.push((f + INPUT_HOLD_FRAMES, up));
                println!("Scheduled synthetic tap at ({x},{y}) for frame {f}");
            }
            None => {
                if let Some(process) = emu.process_mut() {
                    process.state.pending_input.push_back(down);
                    process.state.pending_input.push_back(up);
                }
                println!("Queued synthetic tap at ({x},{y})");
            }
        }
    }
    for key in keys {
        let (at_frame, body) = split_frame_prefix(key);
        let vk = parse_virtual_key(body)
            .with_context(|| format!("invalid --key {key:?}; use enter, arrows, a/b/c, or 0xNN"))?;
        let down = pocket_core::kernel::InputEvent::KeyDown { vk };
        let up = pocket_core::kernel::InputEvent::KeyUp { vk };
        match at_frame {
            Some(f) => {
                scheduled.push((f, down));
                scheduled.push((f + INPUT_HOLD_FRAMES, up));
                println!("Scheduled synthetic key {body} (VK 0x{vk:02x}) for frame {f}");
            }
            None => {
                if let Some(process) = emu.process_mut() {
                    process.state.pending_input.push_back(down);
                    // Real hardware always follows a press with a
                    // release; a game that latches on WM_KEYUP never
                    // sees the input otherwise.
                    process.state.pending_input.push_back(up);
                }
                println!("Queued synthetic key {body} (VK 0x{vk:02x})");
            }
        }
    }
    scheduled.sort_by_key(|(f, _)| *f);
    for spec in patches {
        let (addr_str, hex_str) = spec
            .split_once('=')
            .with_context(|| format!("invalid --patch spec {spec:?}; expected ADDR=HEX"))?;
        let addr_str = addr_str.trim_start_matches("0x");
        let addr = u32::from_str_radix(addr_str, 16)
            .with_context(|| format!("invalid hex address in --patch {spec:?}"))?;
        let hex_str = hex_str.trim_start_matches("0x");
        if hex_str.len() % 2 != 0 {
            anyhow::bail!("invalid --patch hex bytes (odd length) in {spec:?}");
        }
        let mut bytes = Vec::with_capacity(hex_str.len() / 2);
        for chunk in hex_str.as_bytes().chunks(2) {
            let s = std::str::from_utf8(chunk).unwrap();
            bytes.push(
                u8::from_str_radix(s, 16)
                    .with_context(|| format!("invalid hex byte {s:?} in --patch {spec:?}"))?,
            );
        }
        emu.write_guest_memory(addr, &bytes)
            .with_context(|| format!("applying --patch {spec:?}"))?;
        println!("Patched {} bytes at guest VA 0x{:08x}", bytes.len(), addr);
    }
    for spec in watches {
        let s = spec.trim_start_matches("0x");
        let va = u32::from_str_radix(s, 16)
            .with_context(|| format!("invalid hex VA in --watch {spec:?}"))?;
        emu.add_code_hook(va)
            .with_context(|| format!("installing --watch breakpoint at 0x{va:08x}"))?;
        println!("Installed watch breakpoint at guest VA 0x{:08x}", va);
    }
    println!(
        "Registered API stubs: {}",
        emu.dispatcher().registered_count()
    );

    let mut hooks: Vec<Box<dyn pocket_core::kernel::FrameHook>> = Vec::new();
    if !scheduled.is_empty() {
        // Ahead of the frame dumper so an input scheduled for frame N
        // is queued before the guest renders past it.
        hooks.push(Box::new(ScheduledInputHook::new(scheduled)));
    }
    if max_frames > 0 && dump_frames_to.is_none() {
        // `--max-frames` used to be honoured only by the frame dumper,
        // so capping a run meant also paying for a PPM write per
        // frame — 2.6 ms of it at 800x480, which is most of a frame
        // budget and makes the cap useless for measuring anything.
        hooks.push(Box::new(FrameLimitHook::new(max_frames)));
    }
    if let Some(dir) = dump_frames_to {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating frame dump dir {}", dir.display()))?;
        let dir = dir.to_path_buf();
        hooks.push(Box::new(DumpFrameHook::new(
            dir,
            max_frames,
            dump_frame_stride,
        )));
        println!(
            "Dumping framebuffer snapshots to {}",
            dump_frames_to.unwrap().display()
        );
    }
    if display {
        #[cfg(feature = "display")]
        {
            hooks.push(Box::new(display_window::DisplayHook::new(fb_w, fb_h)?));
            println!("Display window opened (close it to exit emulator).");
        }
        #[cfg(not(feature = "display"))]
        {
            anyhow::bail!(
                "--display requires building pockethle with `--features display` (minifb)."
            );
        }
    }

    emu.start_audio();
    let run_result = if hooks.is_empty() {
        emu.run()
    } else {
        let mut combined = MultiHook { hooks };
        emu.run_with_hook(&mut combined)
    };
    emu.stop_audio();

    if let Some(p) = emu.process() {
        let ppm = p.state.framebuffer.snapshot_ppm();
        let final_path = std::path::PathBuf::from("/tmp/pockethle-final.ppm");
        if let Err(e) = std::fs::write(&final_path, &ppm) {
            eprintln!("warn: could not write {} ({e})", final_path.display());
        } else {
            println!(
                "Final framebuffer snapshot written to {} ({} bytes, frame_counter={})",
                final_path.display(),
                ppm.len(),
                p.state.framebuffer.frame_counter,
            );
        }
    }

    run_result?;
    println!("Emulator exited cleanly.");
    Ok(())
}

/// How many rendered frames a scheduled press is held before the
/// matching release is delivered. A press and release in the same
/// frame can be coalesced by a game that samples key state once per
/// tick, so give it a few frames of overlap.
const INPUT_HOLD_FRAMES: u64 = 3;

/// Split an optional `<FRAME>:` prefix off a `--tap` / `--key` value.
/// Returns `(Some(frame), rest)` when the value is scheduled, and
/// `(None, whole)` when it should be queued before the first message.
///
/// A hex VK code such as `0x25` is not a frame prefix, and neither is
/// anything whose prefix is not all digits, so `--key 0x25` and
/// `--key enter` keep working unchanged.
fn split_frame_prefix(value: &str) -> (Option<u64>, &str) {
    match value.split_once(':') {
        Some((head, rest)) if !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()) => {
            match head.parse::<u64>() {
                Ok(f) => (Some(f), rest.trim()),
                Err(_) => (None, value),
            }
        }
        _ => (None, value),
    }
}

/// Parse a `--screen WIDTHxHEIGHT` argument, e.g. `320x240`.
fn parse_screen_size(value: &str) -> anyhow::Result<(u32, u32)> {
    let (w, h) = value
        .trim()
        .split_once(['x', 'X'])
        .with_context(|| format!("invalid --screen {value:?}; expected WIDTHxHEIGHT"))?;
    let w: u32 = w
        .trim()
        .parse()
        .with_context(|| format!("invalid screen width in {value:?}"))?;
    let h: u32 = h
        .trim()
        .parse()
        .with_context(|| format!("invalid screen height in {value:?}"))?;
    if w == 0 || h == 0 {
        anyhow::bail!("invalid --screen {value:?}; dimensions must be non-zero");
    }
    Ok((w, h))
}

/// Resolve a `--key` name to a virtual-key code.
///
/// The face-button names (`a`, `b`, `c`, `start`) resolve to the codes
/// `pocket_kernel::gapi` hands the guest through `GXGetDefaultKeys`,
/// which is what a GAPI title such as Asphalt 2 3D actually listens
/// for — spelling `a` as the letter `A` (0x41) meant the confirm button
/// could not be pressed from the CLI at all. `enter` stays the real
/// `VK_RETURN`; the message pump rewrites it to `vkA` for GAPI guests,
/// so it confirms menus in either kind of title.
fn parse_virtual_key(value: &str) -> anyhow::Result<u16> {
    use pocket_core::kernel::gapi;
    let normalized = value.trim().to_ascii_lowercase();
    let vk = match normalized.as_str() {
        "up" | "arrowup" => gapi::VK_UP,
        "down" | "arrowdown" => gapi::VK_DOWN,
        "left" | "arrowleft" => gapi::VK_LEFT,
        "right" | "arrowright" => gapi::VK_RIGHT,
        "enter" | "return" => gapi::VK_RETURN,
        "space" => 0x20,
        "escape" | "esc" | "back" => 0x1b,
        "a" | "action" | "select" | "ok" | "fire" => gapi::VK_A,
        "b" => gapi::VK_B,
        "c" => gapi::VK_C,
        "start" => gapi::VK_START,
        "soft1" => 0xc1,
        "soft2" => 0xc2,
        "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
            0x30 + u16::from(normalized.as_bytes()[0] - b'0')
        }
        _ if normalized.starts_with("0x") => u16::from_str_radix(&normalized[2..], 16)?,
        _ => anyhow::bail!("unknown virtual key name"),
    };
    Ok(vk)
}

#[cfg(test)]
mod key_name_tests {
    use super::parse_virtual_key;
    use pocket_core::kernel::gapi;

    #[test]
    fn face_buttons_use_the_gapi_codes() {
        assert_eq!(parse_virtual_key("a").unwrap(), gapi::VK_A);
        assert_eq!(parse_virtual_key("select").unwrap(), gapi::VK_A);
        assert_eq!(parse_virtual_key("start").unwrap(), gapi::VK_START);
        assert_eq!(parse_virtual_key("enter").unwrap(), gapi::VK_RETURN);
        assert_eq!(parse_virtual_key("5").unwrap(), 0x35);
        assert_eq!(parse_virtual_key("0x28").unwrap(), gapi::VK_DOWN);
        assert!(parse_virtual_key("nope").is_err());
    }
}

// ----- frame hooks -----

struct MultiHook {
    hooks: Vec<Box<dyn pocket_core::kernel::FrameHook>>,
}

impl pocket_core::kernel::FrameHook for MultiHook {
    fn on_frame(
        &mut self,
        state: &mut pocket_core::kernel::KernelState,
    ) -> pocket_core::kernel::FrameAction {
        let mut action = pocket_core::kernel::FrameAction::Continue;
        for h in self.hooks.iter_mut() {
            if h.on_frame(state) == pocket_core::kernel::FrameAction::Stop {
                action = pocket_core::kernel::FrameAction::Stop;
            }
        }
        action
    }
}

/// Delivers `--tap` / `--key` values that carry a `<FRAME>:` prefix
/// once the guest has actually rendered that many frames. Queueing
/// them up front is no use for anything past the title screen: the
/// game drains its message queue long before the menu it belongs to
/// exists.
struct ScheduledInputHook {
    /// Sorted by frame, drained from the front.
    pending: std::collections::VecDeque<(u64, pocket_core::kernel::InputEvent)>,
    frames_seen: u64,
    last_counter: u64,
    /// When the guest last pushed a new frame. A menu that is waiting
    /// for a key press stops redrawing, so frame-indexed input would
    /// never come due; after `STALL` of real time without a new frame
    /// we release the next queued event instead.
    last_frame_at: std::time::Instant,
}

impl ScheduledInputHook {
    /// How long the frame counter may stand still before we assume the
    /// guest is idling on a menu and deliver the next queued event.
    const STALL: std::time::Duration = std::time::Duration::from_secs(3);

    fn new(mut events: Vec<(u64, pocket_core::kernel::InputEvent)>) -> Self {
        events.sort_by_key(|(f, _)| *f);
        Self {
            pending: events.into(),
            frames_seen: 0,
            last_counter: 0,
            last_frame_at: std::time::Instant::now(),
        }
    }
}

impl pocket_core::kernel::FrameHook for ScheduledInputHook {
    fn on_frame(
        &mut self,
        state: &mut pocket_core::kernel::KernelState,
    ) -> pocket_core::kernel::FrameAction {
        let counter = state.framebuffer.frame_counter;
        if counter != self.last_counter {
            self.last_counter = counter;
            self.frames_seen += 1;
            self.last_frame_at = std::time::Instant::now();
        }
        let stalled = self.frames_seen > 0 && self.last_frame_at.elapsed() >= Self::STALL;
        while let Some((at, _)) = self.pending.front() {
            if *at > self.frames_seen && !stalled {
                break;
            }
            if *at > self.frames_seen {
                log::info!(
                    "frame counter stalled at {} for {:?}; releasing queued input early",
                    self.frames_seen,
                    Self::STALL
                );
                self.last_frame_at = std::time::Instant::now();
            }
            let (at, ev) = self.pending.pop_front().expect("front just checked");
            log::info!(
                "delivering scheduled input {ev:?} at frame {}",
                self.frames_seen
            );
            state.pending_input.push_back(ev);
            if at > self.frames_seen {
                // Released early because of a stall: hand over one
                // event per stall window so menu keys don't all land
                // in the same frame.
                break;
            }
        }
        pocket_core::kernel::FrameAction::Continue
    }
}

/// Stop the run after a number of rendered frames, without doing
/// anything else per frame.
///
/// Deliberately counts *changed* frames rather than presentation
/// calls, matching what `--max-frames` promises and what
/// `frame_counter` means (invariant 10: it moves only when the pixels
/// change).
struct FrameLimitHook {
    max_frames: u64,
    last_seen: u64,
    frames: u64,
}

impl FrameLimitHook {
    fn new(max_frames: u64) -> Self {
        Self {
            max_frames,
            last_seen: 0,
            frames: 0,
        }
    }
}

impl pocket_core::kernel::FrameHook for FrameLimitHook {
    fn on_frame(
        &mut self,
        state: &mut pocket_core::kernel::KernelState,
    ) -> pocket_core::kernel::FrameAction {
        let counter = state.framebuffer.frame_counter;
        if counter == self.last_seen {
            return pocket_core::kernel::FrameAction::Continue;
        }
        self.last_seen = counter;
        self.frames += 1;
        if self.frames >= self.max_frames {
            pocket_core::kernel::FrameAction::Stop
        } else {
            pocket_core::kernel::FrameAction::Continue
        }
    }
}

struct DumpFrameHook {
    dir: PathBuf,
    last_dumped_frame: u64,
    /// Changed frames observed so far, whether or not they were
    /// written. Drives the `stride` decision.
    seen: u64,
    written: u64,
    max_frames: u64,
    stride: u64,
    saw_non_black: bool,
}

impl DumpFrameHook {
    fn new(dir: PathBuf, max_frames: u64, stride: u64) -> Self {
        Self {
            dir,
            last_dumped_frame: 0,
            seen: 0,
            written: 0,
            max_frames,
            stride: stride.max(1),
            saw_non_black: false,
        }
    }
}

impl pocket_core::kernel::FrameHook for DumpFrameHook {
    fn on_frame(
        &mut self,
        state: &mut pocket_core::kernel::KernelState,
    ) -> pocket_core::kernel::FrameAction {
        let counter = state.framebuffer.frame_counter;
        if counter == self.last_dumped_frame {
            return pocket_core::kernel::FrameAction::Continue;
        }
        self.last_dumped_frame = counter;
        if state.framebuffer.is_all_black() && self.saw_non_black {
            return pocket_core::kernel::FrameAction::Continue;
        }
        self.saw_non_black = !state.framebuffer.is_all_black();
        let index = self.seen;
        self.seen += 1;
        if !index.is_multiple_of(self.stride) {
            return pocket_core::kernel::FrameAction::Continue;
        }
        let path = self.dir.join(format!("frame_{:06}.ppm", self.written));
        let ppm = state.framebuffer.snapshot_ppm();
        if let Err(e) = std::fs::write(&path, ppm) {
            log::warn!("failed to write {}: {e}", path.display());
            return pocket_core::kernel::FrameAction::Continue;
        }
        log::info!("wrote {}", path.display());
        self.written += 1;
        if self.max_frames > 0 && self.written >= self.max_frames {
            return pocket_core::kernel::FrameAction::Stop;
        }
        pocket_core::kernel::FrameAction::Continue
    }
}

#[cfg(feature = "display")]
mod display_window {
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use minifb::{Window, WindowOptions};
    use pocket_core::kernel::{FrameAction, FrameHook, KernelState};

    const DISPLAY_UPDATE_INTERVAL: Duration = Duration::from_millis(16);
    const EVENT_POLL_INTERVAL_SLICES: u64 = 100_000;

    pub struct DisplayHook {
        window: Window,
        buffer: Vec<u32>,
        width: usize,
        height: usize,
        last_frame: u64,
        last_emit_at: Option<Instant>,
        ticks_since_poll: u64,
    }

    impl DisplayHook {
        pub fn new(width: u32, height: u32) -> Result<Self> {
            let w = width as usize;
            let h = height as usize;
            let window = Window::new(
                "PocketHLE",
                w,
                h,
                WindowOptions {
                    resize: true,
                    scale: minifb::Scale::X2,
                    ..WindowOptions::default()
                },
            )
            .context("opening minifb window")?;
            // No `set_target_fps`: minifb's FPS cap blocks every
            // `update_with_buffer` call to ~16 ms, but `on_frame`
            // is invoked *per emulator slice*. With the cap on,
            // the entire run loop gets throttled to ~60 slices/s,
            // which is several orders of magnitude below what a
            // real PPC2003 game needs to reach its first WM_PAINT.
            // Frame pacing comes from the guest's own GAPI flips.
            Ok(Self {
                window,
                buffer: vec![0; w * h],
                width: w,
                height: h,
                last_frame: 0,
                last_emit_at: None,
                ticks_since_poll: 0,
            })
        }
    }

    impl FrameHook for DisplayHook {
        fn on_frame(&mut self, state: &mut KernelState) -> FrameAction {
            if !self.window.is_open() {
                return FrameAction::Stop;
            }

            let counter = state.framebuffer.frame_counter;
            let dirty = counter != self.last_frame;
            let due = self
                .last_emit_at
                .map(|t| t.elapsed() >= DISPLAY_UPDATE_INTERVAL)
                .unwrap_or(true);

            if dirty && due {
                self.last_frame = counter;
                self.last_emit_at = Some(Instant::now());
                for (src, px) in state
                    .framebuffer
                    .pixels
                    .chunks_exact(2)
                    .zip(self.buffer.iter_mut())
                {
                    *px = rgb565_to_minifb(u16::from_le_bytes([src[0], src[1]]));
                }
                let w = self.width;
                let h = self.height;
                let _ = self.window.update_with_buffer(&self.buffer, w, h);
                self.ticks_since_poll = 0;
            } else {
                self.ticks_since_poll = self.ticks_since_poll.saturating_add(1);
                if self.ticks_since_poll >= EVENT_POLL_INTERVAL_SLICES {
                    self.ticks_since_poll = 0;
                    self.window.update();
                }
            }
            FrameAction::Continue
        }
    }

    fn rgb565_to_minifb(p: u16) -> u32 {
        let r5 = ((p >> 11) & 0x1f) as u32;
        let g6 = ((p >> 5) & 0x3f) as u32;
        let b5 = (p & 0x1f) as u32;
        let r = (r5 << 3) | (r5 >> 2);
        let g = (g6 << 2) | (g6 >> 4);
        let b = (b5 << 3) | (b5 >> 2);
        (r << 16) | (g << 8) | b
    }
}
