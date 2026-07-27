//! Linux command-line frontend for PocketHLE.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use pocket_core::Emulator;

mod archive;

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
    /// `path` may be:
    ///   * a Pocket PC `.exe` (PE32 ARM) — loaded directly,
    ///   * a `.cab` — auto-extracted into a temp dir; the largest ARM
    ///     PE inside is launched and the extracted dir is mounted as
    ///     `\Application\` so the guest can `CreateFileW` its own
    ///     resources,
    ///   * a `.zip` — auto-extracted, first ARM PE is launched.
    ///
    /// With the default `unicorn` CPU backend the real ARM core runs
    /// the guest. With `--cpu stub` PocketHLE only logs API requests
    /// (useful for dry-run analysis).
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
        /// Open a host window and render the framebuffer live.
        /// Requires the `display` cargo feature.
        #[arg(long, default_value_t = false)]
        display: bool,
        /// Periodically write the framebuffer as PPM files into the
        /// given directory (one file per emit). Works in any
        /// environment, no extra dependencies.
        #[arg(long)]
        dump_frames_to: Option<PathBuf>,
        /// Stop emulation after this many distinct rendered frames.
        /// Combined with `--dump-frames-to`, gives a deterministic
        /// way to capture proof-of-rendering screenshots.
        #[arg(long, default_value_t = 0)]
        max_frames: u64,
        /// Queue one synthetic tap before the first game message.
        #[arg(long, value_name = "X,Y")]
        tap: Option<String>,
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
    },
}

#[derive(Clone, Debug, clap::ValueEnum)]
enum CpuBackend {
    Stub,
    #[cfg(feature = "unicorn")]
    Unicorn,
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
            display,
            dump_frames_to,
            max_frames,
            tap,
            patch,
            watch,
            message_budget,
        } => cmd_run(
            &path,
            cpu,
            halt_on_unimplemented,
            max_slices,
            instructions_per_slice,
            trace_json.as_deref(),
            rom_dir.as_deref(),
            &rom_prefix,
            display,
            dump_frames_to.as_deref(),
            max_frames,
            tap.as_deref(),
            &patch,
            &watch,
            message_budget,
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
        println!(
            "\nLargest file is {}, treating as the game executable.",
            big.short_name
        );
        cmd_pe_info(&big.extracted_path)?;
    }
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
    display: bool,
    dump_frames_to: Option<&std::path::Path>,
    max_frames: u64,
    tap: Option<&str>,
    patches: &[String],
    watches: &[String],
    message_budget: u64,
) -> Result<()> {
    let mut emu = match backend {
        CpuBackend::Stub => Emulator::with_stub_cpu(),
        #[cfg(feature = "unicorn")]
        CpuBackend::Unicorn => Emulator::with_unicorn_cpu()?,
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

    let summary = {
        let p = emu.load_pe(&_launcher.exe)?;
        format!(
            "Loaded {} ({} machine), {} sections, {} imports",
            p.image.source_path,
            p.image.machine_name(),
            p.image.sections.len(),
            p.image.imports.len()
        )
    };
    println!("{summary}");
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
    emu.set_synthetic_message_budget(message_budget);
    if let Some(tap) = tap {
        let (x, y) = tap
            .split_once(',')
            .with_context(|| format!("invalid --tap {tap:?}; expected X,Y"))?;
        let x = x
            .trim()
            .parse::<u16>()
            .with_context(|| format!("invalid tap x in {tap:?}"))?;
        let y = y
            .trim()
            .parse::<u16>()
            .with_context(|| format!("invalid tap y in {tap:?}"))?;
        if let Some(process) = emu.process_mut() {
            process
                .state
                .pending_input
                .push_back(pocket_core::kernel::InputEvent::PointerDown { x, y });
            process
                .state
                .pending_input
                .push_back(pocket_core::kernel::InputEvent::PointerUp { x, y });
        }
        println!("Queued synthetic tap at ({x},{y})");
    }
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
    if let Some(dir) = dump_frames_to {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating frame dump dir {}", dir.display()))?;
        let dir = dir.to_path_buf();
        hooks.push(Box::new(DumpFrameHook::new(dir, max_frames)));
        println!(
            "Dumping framebuffer snapshots to {}",
            dump_frames_to.unwrap().display()
        );
    }
    if display {
        #[cfg(feature = "display")]
        {
            hooks.push(Box::new(display_window::DisplayHook::new()?));
            println!("Display window opened (close it to exit emulator).");
        }
        #[cfg(not(feature = "display"))]
        {
            anyhow::bail!(
                "--display requires building pockethle with `--features display` (minifb)."
            );
        }
    }

    let run_result = if hooks.is_empty() {
        emu.run()
    } else {
        let mut combined = MultiHook { hooks };
        emu.run_with_hook(&mut combined)
    };

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

struct DumpFrameHook {
    dir: PathBuf,
    last_dumped_frame: u64,
    written: u64,
    max_frames: u64,
}

impl DumpFrameHook {
    fn new(dir: PathBuf, max_frames: u64) -> Self {
        Self {
            dir,
            last_dumped_frame: 0,
            written: 0,
            max_frames,
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
        last_frame: u64,
        last_emit_at: Option<Instant>,
        ticks_since_poll: u64,
    }

    impl DisplayHook {
        pub fn new() -> Result<Self> {
            let w = pocket_core::kernel::FB_WIDTH as usize;
            let h = pocket_core::kernel::FB_HEIGHT as usize;
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
                let w = pocket_core::kernel::FB_WIDTH as usize;
                let h = pocket_core::kernel::FB_HEIGHT as usize;
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
