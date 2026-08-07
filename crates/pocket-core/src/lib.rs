//! Top-level emulator object that the frontends drive.
//!
//! Usage:
//!
//! ```no_run
//! use pocket_core::Emulator;
//! let mut emu = Emulator::with_stub_cpu();
//! emu.load_pe("/path/to/JumpyBallPPC.exe").unwrap();
//! emu.run().unwrap();
//! ```
//!
//! When compiled with `--features unicorn`, [`Emulator::with_unicorn_cpu`]
//! provides a fully working ARM backend.

use std::path::Path;

use anyhow::{Context, Result};

use pocket_cpu::{stub::StubCpu, Cpu};
use pocket_kernel::{run_main_loop, run_main_loop_with_hook, FrameHook, Process};
use pocket_winceapi::{resolve_ordinal, WinCeDispatcher};

pub use pocket_cab as cab;
pub use pocket_cpu as cpu;
pub use pocket_kernel as kernel;
pub use pocket_pe as pe;
pub use pocket_winceapi as winceapi;

pub struct Emulator {
    cpu: Box<dyn Cpu>,
    process: Option<Process>,
    dispatcher: WinCeDispatcher,
    /// Screen geometry requested by the frontend, remembered so
    /// [`Emulator::set_screen_size`] works whether it is called before
    /// or after [`Emulator::load_pe`]. See that method for why.
    requested_screen: Option<(u32, u32)>,
    pub instruction_budget_per_slice: u64,
    pub max_slices: u64,
}

impl Emulator {
    pub fn with_stub_cpu() -> Self {
        Self {
            cpu: Box::new(StubCpu::new()),
            process: None,
            dispatcher: WinCeDispatcher::new(),
            requested_screen: None,
            instruction_budget_per_slice: 1_000_000,
            max_slices: 1024,
        }
    }

    /// Build with the unicorn-engine backed CPU. Requires the
    /// `unicorn` Cargo feature.
    #[cfg(feature = "unicorn")]
    pub fn with_unicorn_cpu() -> Result<Self> {
        Self::with_unicorn_cpu_for_arch(pocket_cpu::Arch::Arm)
    }

    #[cfg(feature = "unicorn")]
    pub fn with_unicorn_cpu_for_arch(arch: pocket_cpu::Arch) -> Result<Self> {
        let cpu = pocket_cpu::unicorn::UnicornCpu::new_for_arch(arch)
            .context("creating unicorn-engine CPU instance")?;
        Ok(Self {
            cpu: Box::new(cpu),
            process: None,
            dispatcher: WinCeDispatcher::new(),
            requested_screen: None,
            instruction_budget_per_slice: 1_000_000,
            max_slices: 1024,
        })
    }

    /// Halt the emulator the first time an unimplemented API is hit.
    /// Useful for the tracing CLI mode.
    pub fn set_halt_on_unimplemented(&mut self, halt: bool) {
        self.dispatcher.halt_on_unimplemented = halt;
    }

    /// Forward every dispatched API call as JSON-lines to `sink`.
    pub fn set_trace_sink(&mut self, sink: Box<dyn std::io::Write + Send>) {
        self.dispatcher.set_trace_sink(sink);
    }

    /// Load and map a PE file into the emulator. Existing process
    /// state is replaced.
    pub fn load_pe(&mut self, path: impl AsRef<Path>) -> Result<&Process> {
        let image = pe::load_file(path).context("loading PE")?;
        let process = Process::map_into(
            image,
            self.cpu.as_mut(),
            &|dll, ord| resolve_ordinal(dll, ord),
            &self.dispatcher,
        )
        .context("mapping image into CPU")?;
        self.process = Some(process);
        // A frontend that sized the screen before loading gets its
        // request honoured here rather than silently dropped.
        if let Some((w, h)) = self.requested_screen {
            self.apply_screen_size(w, h);
        }
        Ok(self.process.as_ref().unwrap())
    }

    /// Run until the emulator halts. Returns the number of slices
    /// consumed.
    pub fn run(&mut self) -> Result<()> {
        let process = self
            .process
            .as_mut()
            .context("no PE loaded — call load_pe() first")?;
        run_main_loop(
            self.cpu.as_mut(),
            process,
            &mut self.dispatcher,
            self.instruction_budget_per_slice,
            self.max_slices,
        )
        .context("main emulator loop")
    }

    /// Like [`Self::run`], but routes the framebuffer through
    /// `frame_hook` once per dispatch slice.
    pub fn run_with_hook(&mut self, frame_hook: &mut dyn FrameHook) -> Result<()> {
        let process = self
            .process
            .as_mut()
            .context("no PE loaded — call load_pe() first")?;
        run_main_loop_with_hook(
            self.cpu.as_mut(),
            process,
            &mut self.dispatcher,
            self.instruction_budget_per_slice,
            self.max_slices,
            Some(frame_hook),
        )
        .context("main emulator loop")
    }

    pub fn process(&self) -> Option<&Process> {
        self.process.as_ref()
    }

    pub fn process_mut(&mut self) -> Option<&mut Process> {
        self.process.as_mut()
    }

    pub fn dispatcher(&self) -> &WinCeDispatcher {
        &self.dispatcher
    }

    /// Write raw bytes into emulated guest memory. Useful for patching
    /// the loaded image (e.g. NOP'ing out a hostile static-init call)
    /// before [`Self::run`] is invoked.
    pub fn write_guest_memory(&mut self, addr: u32, bytes: &[u8]) -> Result<()> {
        self.cpu
            .write_mem(addr, bytes)
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Install an instruction-level code hook at the given guest VA.
    /// Used by `--watch` for diagnostic breakpoints. When the CPU
    /// reaches the address, the kernel run loop will dump registers
    /// and halt cleanly.
    pub fn add_code_hook(&mut self, va: u32) -> Result<()> {
        self.cpu.add_code_hook(va).map_err(|e| anyhow::anyhow!(e))
    }

    /// Mount a host directory at a guest WinCE path. Useful for
    /// satisfying `CreateFileW` requests once the PE is loaded.
    pub fn mount_dir(&mut self, guest_prefix: &str, host_dir: impl Into<std::path::PathBuf>) {
        if let Some(p) = self.process.as_mut() {
            p.state.vfs.mount(guest_prefix, host_dir);
        } else {
            log::warn!("mount_dir called before load_pe; ignored");
        }
    }

    pub fn mount_read_only_dir(
        &mut self,
        guest_prefix: &str,
        host_dir: impl Into<std::path::PathBuf>,
    ) {
        if let Some(p) = self.process.as_mut() {
            p.state.vfs.mount_read_only(guest_prefix, host_dir);
        } else {
            log::warn!("mount_read_only_dir called before load_pe; ignored");
        }
    }

    pub fn mount_save_dir(&mut self, guest_prefix: &str, host_dir: impl Into<std::path::PathBuf>) {
        if let Some(p) = self.process.as_mut() {
            p.state.vfs.mount_save_dir(guest_prefix, host_dir);
        } else {
            log::warn!("mount_save_dir called before load_pe; ignored");
        }
    }

    /// Set the guest path `GetModuleFileNameW` reports for the running
    /// executable.
    ///
    /// Pocket PC titles routinely locate their assets *relative to
    /// their own module path*: they call `GetModuleFileNameW`, subtract
    /// the length of a hard-coded `L"<Game>.exe"` literal, and append
    /// the asset name. Reporting a generic placeholder therefore breaks
    /// them, so frontends pass the path the installer would have used
    /// on a real device.
    pub fn set_module_path(&mut self, path: impl Into<String>) {
        let path = path.into();
        if let Some(p) = self.process.as_mut() {
            p.state.module_path = path;
        } else {
            log::warn!("set_module_path called before load_pe; ignored");
        }
    }

    /// Override how many synthetic `WM_PAINT` messages the dispatcher
    /// will hand out before posting `WM_QUIT`. Pass `0` for unlimited
    /// (the message loop will keep running until another path halts
    /// the emulator).
    pub fn set_synthetic_message_budget(&mut self, budget: u64) {
        if let Some(p) = self.process.as_mut() {
            p.state.synthetic_message_budget = budget;
        }
    }

    /// Pre-seed a registry value, as a cabinet's `_setup.xml`
    /// `<characteristic type="Registry">` block would have on install.
    ///
    /// Pocket PC games read their save directory (and sometimes their
    /// licence record) back out of the registry the installer wrote;
    /// Astraware's Bejeweled calls `ExitProcess(0x42)` when
    /// `HKLM\SOFTWARE\Apps\Astraware Bejeweled\SaveDir` is missing.
    pub fn set_registry_value(
        &mut self,
        key: &str,
        name: &str,
        value: pocket_kernel::registry::RegistryValue,
    ) {
        if let Some(p) = self.process.as_mut() {
            p.state.registry.set_value(key, name, value);
        }
    }
    /// Set the directory relative guest paths resolve against.
    ///
    /// Windows CE has no per-process working directory, but games ship
    /// relative paths anyway (`FindFirstFile(".\\*.pdb")` in Astraware's
    /// Bejeweled). Anchoring them at the executable's install directory
    /// is what those titles expect, because that is where the shell
    /// launched them from.
    pub fn set_default_dir(&mut self, guest_dir: &str) {
        if let Some(p) = self.process.as_mut() {
            p.state.vfs.set_default_dir(guest_dir);
        }
    }

    /// Tee every PCM sample the guest submits into a 16-bit WAV file.
    ///
    /// Headless machines (CI, containers) have no audio device, so the
    /// only way to check that a game really produces sound is to
    /// record what it sent. Must be called after `load_pe`.
    pub fn capture_audio_to(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        match self.process.as_mut() {
            Some(p) => p.state.audio.capture_to(path),
            None => {
                log::warn!("capture_audio_to called before load_pe; ignored");
                Ok(())
            }
        }
    }

    /// A pull handle for hosts that play the guest's PCM themselves.
    ///
    /// The Android frontend uses this: it has no cpal backend, so the
    /// JNI layer drains the tap on a helper thread and writes into a
    /// Java `AudioTrack`. Returns `None` before `load_pe`.
    pub fn audio_tap(&self) -> Option<pocket_kernel::AudioTap> {
        self.process.as_ref().map(|p| p.state.audio.tap())
    }

    /// Resize the emulated display. Call it any time before
    /// [`Self::run`] — either side of [`Self::load_pe`] works, because
    /// a request made before the process exists is remembered and
    /// applied by `load_pe`. It must precede `run`, since the guest
    /// reads the geometry once during start-up (`GetSystemMetrics`,
    /// `GetDeviceCaps`, `GXGetDisplayProperties`) and sizes its back
    /// buffer from it.
    ///
    /// The ordering used to matter and silently didn't hold: the
    /// desktop launcher sized the screen before loading, so every game
    /// ran at the default no matter which resolution the user picked in
    /// the GUI. Deferring instead of dropping the request keeps that
    /// class of bug from coming back through a new frontend.
    ///
    /// The default is the Pocket PC 240×320 portrait LCD. Windows
    /// Mobile Smartphone titles — e.g. the Motorola Q9 build of
    /// Asphalt 2 — expect a 320×240 landscape screen and will
    /// otherwise blit a surface that our framebuffer silently clips.
    pub fn set_screen_size(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            log::warn!("set_screen_size({width}x{height}) ignored: zero dimension");
            return;
        }
        self.requested_screen = Some((width, height));
        if self.process.is_some() {
            self.apply_screen_size(width, height);
        }
    }

    /// Screen geometry the guest will see, once [`Self::load_pe`] has
    /// run. Reflects the live framebuffer, so it also covers a game
    /// that resized the display itself.
    pub fn screen_size(&self) -> Option<(u32, u32)> {
        let fb = &self.process.as_ref()?.state.framebuffer;
        Some((fb.width, fb.height))
    }

    /// Resize the live framebuffer. Only valid with a process loaded.
    fn apply_screen_size(&mut self, width: u32, height: u32) {
        let Some(p) = self.process.as_mut() else {
            return;
        };
        p.state.framebuffer = pocket_kernel::Framebuffer::new(width, height);
        // The GAPI mapping is sized from the framebuffer, so drop it;
        // the next GXOpenDisplay/GXBeginDraw re-maps at the new size.
        p.state.fb_mapped = false;
        p.state.gx_readback_scratch.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_emulator_constructs() {
        let _ = Emulator::with_stub_cpu();
    }

    #[test]
    fn a_screen_size_set_before_load_is_remembered() {
        // The desktop launcher sizes the screen before it loads the PE.
        // That used to be silently dropped, so every game ran at the
        // 240x320 default however the user configured it. The request
        // has to survive until there is a process to apply it to.
        let mut emu = Emulator::with_stub_cpu();
        assert_eq!(emu.screen_size(), None, "no process yet");
        emu.set_screen_size(480, 320);
        assert_eq!(
            emu.requested_screen,
            Some((480, 320)),
            "request must be kept for load_pe to apply"
        );
    }

    #[test]
    fn a_zero_dimension_screen_size_is_refused() {
        // Never leave a zero-sized framebuffer behind: a later
        // load_pe must not apply it either.
        let mut emu = Emulator::with_stub_cpu();
        emu.set_screen_size(480, 0);
        assert_eq!(emu.requested_screen, None);
        emu.set_screen_size(0, 320);
        assert_eq!(emu.requested_screen, None);
    }
}
