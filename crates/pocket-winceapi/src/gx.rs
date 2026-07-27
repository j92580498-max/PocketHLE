//! Microsoft GAPI (Game API) — `gx.dll`.
//!
//! GAPI exposes nine functions that give a Pocket PC game direct
//! access to the framebuffer. The interface is small and very stable
//! across devices: `GXOpenDisplay` once, `GXBeginDraw` to obtain a
//! pointer to the back-buffer, write pixels, `GXEndDraw` to flush.
//!
//! PocketHLE backs this with the same software [`Framebuffer`] that
//! the GDI handlers paint into. We map an extra page-aligned region
//! at [`SYNTHETIC_FB_BASE`] in the guest VA space lazily, on the
//! first call to `GXOpenDisplay`, so the guest can write pixels
//! through that pointer; `GXEndDraw` then copies them back into the
//! host-visible [`pocket_kernel::Framebuffer`].

use pocket_cpu::Prot;
use pocket_kernel::framebuffer::FB_BYTES;
use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

/// Synthetic framebuffer base address. Mapped lazily by
/// [`gx_open_display`]. The value is chosen well above the thunk
/// pool so it cannot collide with normal allocations.
pub const SYNTHETIC_FB_BASE: u32 = 0x7800_0000;
pub const SCREEN_WIDTH: u32 = pocket_kernel::framebuffer::FB_WIDTH;
pub const SCREEN_HEIGHT: u32 = pocket_kernel::framebuffer::FB_HEIGHT;
/// 16 bpp framebuffer, default Pocket PC depth.
pub const SCREEN_BPP: u32 = pocket_kernel::framebuffer::FB_BPP;

pub fn register(d: &mut WinCeDispatcher) {
    let dll = "gx.dll";
    // The names below are the *demangled* C++ names used in the
    // import directory of the JumpyBall PE. We strip mangling for
    // dispatch lookup.
    d.register_handler(dll, "?GXOpenDisplay@@YAHPAUHWND__@@K@Z", gx_open_display);
    d.register_handler(dll, "?GXCloseDisplay@@YAHXZ", gx_close_display);
    d.register_handler(dll, "?GXBeginDraw@@YAPAXXZ", gx_begin_draw);
    d.register_handler(dll, "?GXEndDraw@@YAHXZ", gx_end_draw);
    d.register_handler(dll, "?GXSuspend@@YAHXZ", gx_suspend);
    d.register_handler(dll, "?GXResume@@YAHXZ", gx_resume);
    d.register_handler(dll, "?GXOpenInput@@YAHXZ", gx_open_input);
    d.register_handler(dll, "?GXCloseInput@@YAHXZ", gx_close_input);
    d.register_handler(
        dll,
        "?GXGetDefaultKeys@@YA?AUGXKeyList@@H@Z",
        gx_get_default_keys,
    );
    d.register_handler(
        dll,
        "?GXGetDisplayProperties@@YA?AUGXDisplayProperties@@XZ",
        gx_get_display_properties,
    );
    // `BOOL GXIsDisplayDRAMBuffer()` — Pocket Derby Day asks whether
    // the framebuffer pointer returned by `GXBeginDraw` lives in the
    // device's "DRAM" (i.e. is a regular RAM-backed surface) or VRAM
    // (some hardware framebuffer that doesn't tolerate dword writes).
    // Our synthetic buffer at [`SYNTHETIC_FB_BASE`] is plain RAM, so
    // we always answer "yes" — that's the answer Derby expects on
    // Pocket PC 2003 devices and the path it has the most testing on.
    d.register_handler(
        dll,
        "?GXIsDisplayDRAMBuffer@@YAHXZ",
        gx_is_display_dram_buffer,
    );
}

/// Round `size` up to the next multiple of `0x1000` so we can mmap
/// it as whole pages.
const fn page_align_up(size: u32) -> u32 {
    (size + 0xfff) & !0xfff
}

fn ensure_fb_mapped(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    if ctx.kernel.fb_mapped {
        return Ok(());
    }
    let bytes = page_align_up(FB_BYTES);
    ctx.cpu
        .map_region(SYNTHETIC_FB_BASE, bytes, Prot::READ | Prot::WRITE)?;
    ctx.cpu
        .write_mem(SYNTHETIC_FB_BASE, &ctx.kernel.framebuffer.pixels)?;
    ctx.kernel.fb_mapped = true;
    Ok(())
}

fn gx_open_display(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_fb_mapped(ctx)?;
    log::info!(
        "GXOpenDisplay() -> 1 (FB at 0x{:08x}, {}×{}×{}bpp)",
        SYNTHETIC_FB_BASE,
        SCREEN_WIDTH,
        SCREEN_HEIGHT,
        SCREEN_BPP
    );
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gx_close_display(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gx_begin_draw(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_fb_mapped(ctx)?;
    // Push the current host framebuffer state into the guest mapping
    // so the guest sees what was previously painted (e.g. a partial
    // background). Skip the 150 KiB write_mem when nothing on the
    // host side has touched the framebuffer since our last EndDraw —
    // i.e. the guest is the sole producer of pixels (the JumpyBall
    // hot loop). `gx_last_pushed_counter` is bumped at end-of-frame;
    // any GDI handler that paints into the host fb calls
    // `mark_dirty()`, which advances `frame_counter`, so a mismatch
    // here means somebody else dirtied the host fb and we have to
    // re-prime the guest mapping.
    let host_counter = ctx.kernel.framebuffer.frame_counter;
    if host_counter != ctx.kernel.gx_last_pushed_counter {
        ctx.cpu
            .write_mem(SYNTHETIC_FB_BASE, &ctx.kernel.framebuffer.pixels)?;
    }
    log::trace!("GXBeginDraw() -> 0x{:08x}", SYNTHETIC_FB_BASE);
    Ok(DispatchOutcome::ReturnedR0(SYNTHETIC_FB_BASE))
}

fn gx_end_draw(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.fb_mapped {
        let fb_len = FB_BYTES as usize;
        // Reuse a single host-side scratch buffer for the per-frame
        // 150 KiB readback. Allocating a fresh `Vec<u8>` here every
        // frame churned ~9 MiB/s during JumpyBall play and showed up
        // as a measurable slice of the dispatcher's time budget.
        if ctx.kernel.gx_readback_scratch.len() != fb_len {
            ctx.kernel.gx_readback_scratch.resize(fb_len, 0);
        }
        ctx.cpu
            .read_mem_into(SYNTHETIC_FB_BASE, &mut ctx.kernel.gx_readback_scratch)?;
        ctx.kernel
            .framebuffer
            .pixels
            .copy_from_slice(&ctx.kernel.gx_readback_scratch);
        ctx.kernel.framebuffer.mark_dirty();
        // Snapshot the new counter *after* mark_dirty so the next
        // BeginDraw can detect "nothing else touched the host fb in
        // the meantime" and skip the redundant push.
        ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gx_suspend(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `GXResume()` — called by the foreground app when the OS sends it
/// `WM_ACTIVATE`. We're always foreground, so success is the only
/// answer. Without this stub Zuma exits its message-pump after the
/// first `WM_ACTIVATE` because GAPI returns `0`.
fn gx_resume(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gx_open_input(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gx_close_input(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gx_get_default_keys(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // The function returns a `GXKeyList` value via a hidden pointer
    // passed in r0 (sret on ARM AAPCS). The struct holds 8 key
    // entries of `{SHORT vkXxx; POINT ptXxx;}` — 12 bytes each
    // (with 2 bytes of padding before the 4-aligned POINT) for a
    // total of `0x60` bytes. Writing past that is exactly what was
    // smashing Expresso's saved LR on the way out of GXOpenInput.
    //
    // Real Pocket PC devices fill this with the standard hardware
    // mapping: D-pad up/down/left/right + the central "action"
    // button + three soft keys. Returning all-zero (i.e. "vk = 0")
    // tells games every key is unmapped, which is why JumpyBall and
    // similar PPC titles never advance past the title screen under
    // PocketHLE — their menu logic short-circuits when the key list
    // is degenerate. We return the canonical Windows Mobile defaults
    // matching the PPC2003 SDK header `gx.h` order:
    //   vkUp, vkDown, vkLeft, vkRight, vkA (action), vkB, vkC, vkStart.
    let sret = ctx.arg_u32(0)?;
    // Win32 virtual-key codes — documented in winuser.h.
    const VK_UP: u16 = 0x26;
    const VK_DOWN: u16 = 0x28;
    const VK_LEFT: u16 = 0x25;
    const VK_RIGHT: u16 = 0x27;
    const VK_RETURN: u16 = 0x0D; // Start / center button.
    const VK_A: u16 = 0x41;
    const VK_B: u16 = 0x42;
    const VK_C: u16 = 0x43;
    let mut buf = Vec::with_capacity(0x60);
    let entries: [(u16, u32, u32); 8] = [
        (VK_UP, 0, 0),
        (VK_DOWN, 0, 0),
        (VK_LEFT, 0, 0),
        (VK_RIGHT, 0, 0),
        (VK_A, 0, 0),
        (VK_B, 0, 0),
        (VK_C, 0, 0),
        (VK_RETURN, 0, 0),
    ];
    for (vk, x, y) in entries {
        buf.extend_from_slice(&vk.to_le_bytes()); // 2 bytes
        buf.extend_from_slice(&[0u8, 0u8]); // 2 bytes padding
        buf.extend_from_slice(&x.to_le_bytes()); // 4 bytes (POINT.x)
        buf.extend_from_slice(&y.to_le_bytes()); // 4 bytes (POINT.y)
    }
    debug_assert_eq!(buf.len(), 0x60);
    ctx.cpu.write_mem(sret, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(sret))
}

fn gx_is_display_dram_buffer(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gx_get_display_properties(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // Returns GXDisplayProperties { cxWidth, cyHeight, cbxPitch, cbyPitch, cBPP, ffFormat }.
    let sret = ctx.arg_u32(0)?;
    let mut buf = Vec::with_capacity(24);
    buf.extend_from_slice(&SCREEN_WIDTH.to_le_bytes());
    buf.extend_from_slice(&SCREEN_HEIGHT.to_le_bytes());
    buf.extend_from_slice(&(SCREEN_BPP / 8).to_le_bytes());
    buf.extend_from_slice(&(SCREEN_WIDTH * SCREEN_BPP / 8).to_le_bytes());
    buf.extend_from_slice(&SCREEN_BPP.to_le_bytes());
    // ffFormat = kfDirect | kfDirect565
    buf.extend_from_slice(&0x40_0010u32.to_le_bytes());
    ctx.cpu.write_mem(sret, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(sret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu};
    use pocket_kernel::framebuffer::FB_BYTES;
    use pocket_kernel::{vfs::Vfs, Framebuffer, GdiState, Heap, KernelState, Thunk};
    use pocket_pe::ImportBinding;

    fn fresh_kernel() -> KernelState {
        use pocket_kernel::audio::{AudioEngine, GuestFormat};
        KernelState {
            heap: Heap::new(0x5000_0000, 0x10000),
            vfs: Vfs::new(),
            framebuffer: Framebuffer::default(),
            gdi: GdiState::new(),
            resources: vec![],
            image_base: 0,
            fb_mapped: false,
            gx_readback_scratch: Vec::new(),
            mem_op_scratch: Vec::new(),
            mem_op_scratch_b: Vec::new(),
            bit_blt_src_scratch: Vec::new(),
            dib_sync_scratch: Vec::new(),
            dib_decode_scratch: Vec::new(),
            gx_last_pushed_counter: 0,
            synthetic_message_count: 0,
            synthetic_message_budget: 240,
            wnd_proc: 0,
            synthetic_timer_id: 0,
            synthetic_timer_interval_ms: 16,
            synthetic_timer_next_ms: 0,
            synthetic_paint_next_ms: 0,
            synthetic_create_sent: false,
            pending_input: std::collections::VecDeque::new(),
            pressed_keys: [false; 256],
            should_stop: false,
            tls_slots_used: 0,
            vector_iter_frames: std::collections::HashMap::new(),
            security_cookie: 0,
            audio: AudioEngine::new(),
            wave_out_format: GuestFormat::default(),
            menus: std::collections::HashMap::new(),
            next_menu_handle: 0xDEAD_2000,
            sub_menus: std::collections::HashMap::new(),
        }
    }

    fn dummy_thunk() -> Thunk {
        Thunk {
            thunk_va: 0x70000000,
            iat_va: 0x4000_0000,
            dll: "gx.dll".to_string(),
            binding: ImportBinding::Ordinal(0),
            friendly_name: None,
        }
    }

    #[test]
    fn open_display_maps_fb_region() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let t = dummy_thunk();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        let r = gx_open_display(&mut c).unwrap();
        assert_eq!(r, DispatchOutcome::ReturnedR0(1));
        assert!(c.kernel.fb_mapped);
        // Region must be readable.
        let bytes = c.cpu.read_mem(SYNTHETIC_FB_BASE, 4).unwrap();
        assert_eq!(bytes.len(), 4);
    }

    #[test]
    fn end_draw_copies_guest_pixels_to_host_framebuffer() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let t = dummy_thunk();
        // Open display + begin draw to map the region.
        {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            gx_open_display(&mut c).unwrap();
            assert_eq!(
                gx_begin_draw(&mut c).unwrap(),
                DispatchOutcome::ReturnedR0(SYNTHETIC_FB_BASE)
            );
        }
        // Guest writes a magenta pixel at (0,0): RGB565 0xF81F (LE: 1F F8).
        cpu.write_mem(SYNTHETIC_FB_BASE, &[0x1f, 0xf8]).unwrap();
        // Set sp to a sane value so arg_u32 doesn't trip.
        cpu.write_reg(ArmReg::Sp, 0x4000).unwrap();
        let pre_counter;
        {
            let mut c = CallCtx {
                cpu: &mut cpu,
                thunk: &t,
                kernel: &mut kernel,
            };
            pre_counter = c.kernel.framebuffer.frame_counter;
            assert_eq!(gx_end_draw(&mut c).unwrap(), DispatchOutcome::ReturnedR0(1));
        }
        // The host framebuffer must have observed those pixels and
        // bumped its dirty counter.
        assert_eq!(kernel.framebuffer.pixels[0], 0x1f);
        assert_eq!(kernel.framebuffer.pixels[1], 0xf8);
        assert!(kernel.framebuffer.frame_counter > pre_counter);
        assert_eq!(kernel.framebuffer.pixels.len(), FB_BYTES as usize);
    }
}
