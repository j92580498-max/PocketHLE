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
use pocket_kernel::SYNTHETIC_FRAMEBUFFER_BASE;
use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

/// Synthetic framebuffer base address. Mapped lazily by
/// [`gx_open_display`]. The value is chosen well above the thunk
/// pool so it cannot collide with normal allocations.
pub const SYNTHETIC_FB_BASE: u32 = SYNTHETIC_FRAMEBUFFER_BASE;
/// Default screen geometry. The *live* geometry comes from
/// [`pocket_kernel::Framebuffer`], which the frontend may resize (a
/// landscape smartphone title wants 320×240, not the Pocket PC
/// 240×320 portrait default); these constants only describe the
/// out-of-the-box configuration.
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
    d.register_constant(dll, "?GXCloseDisplay@@YAHXZ", 1, gx_close_display);
    d.register_handler(dll, "?GXBeginDraw@@YAPAXXZ", gx_begin_draw);
    d.register_handler(dll, "?GXEndDraw@@YAHXZ", gx_end_draw);
    d.register_handler(dll, "?GXSuspend@@YAHXZ", gx_suspend);
    d.register_constant(dll, "?GXSuspend@@YAHXZ", 1, gx_suspend);
    d.register_handler(dll, "?GXResume@@YAHXZ", gx_resume);
    d.register_constant(dll, "?GXResume@@YAHXZ", 1, gx_resume);
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
    d.register_handler(dll, "?GXSetViewport@@YAHKKKK@Z", gx_set_viewport);
}

/// Round `size` up to the next multiple of `0x1000` so we can mmap
/// it as whole pages.
const fn page_align_up(size: u32) -> u32 {
    (size + 0xfff) & !0xfff
}

fn guest_pitch_bytes(ctx: &CallCtx<'_>) -> u32 {
    if ctx
        .kernel
        .module_path
        .to_ascii_lowercase()
        .contains("pocket bass pro")
    {
        512
    } else {
        ctx.kernel.framebuffer.stride_bytes()
    }
}

fn ensure_fb_mapped(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    if ctx.kernel.fb_mapped {
        return Ok(());
    }
    let bytes = page_align_up(
        guest_pitch_bytes(ctx)
            .saturating_mul(ctx.kernel.framebuffer.height)
            .max(ctx.kernel.framebuffer.byte_size()),
    );
    ctx.cpu
        .map_region(SYNTHETIC_FB_BASE, bytes, Prot::READ | Prot::WRITE)?;
    ctx.cpu
        .write_mem(SYNTHETIC_FB_BASE, &ctx.kernel.framebuffer.pixels)?;
    ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
    ctx.kernel.fb_mapped = true;
    Ok(())
}

fn gx_open_display(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_fb_mapped(ctx)?;
    let pc = ctx.cpu.read_reg(pocket_cpu::regs::ArmReg::Pc).unwrap_or(0);
    let lr = ctx.cpu.read_reg(pocket_cpu::regs::ArmReg::Lr).unwrap_or(0);
    log::info!(
        "GXOpenDisplay() -> 1 (FB at 0x{:08x}, {}×{}×{}bpp, pc=0x{:08x}, lr=0x{:08x})",
        SYNTHETIC_FB_BASE,
        ctx.kernel.framebuffer.width,
        ctx.kernel.framebuffer.height,
        ctx.kernel.framebuffer.bpp,
        pc,
        lr
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
    if ctx.kernel.framebuffer.frame_counter != ctx.kernel.gx_last_pushed_counter
        && !ctx.kernel.framebuffer.is_all_black()
    {
        let pitch = guest_pitch_bytes(ctx) as usize;
        let row_bytes = ctx.kernel.framebuffer.stride_bytes() as usize;
        if pitch == row_bytes {
            ctx.cpu
                .write_mem(SYNTHETIC_FB_BASE, &ctx.kernel.framebuffer.pixels)?;
        } else {
            for row in 0..ctx.kernel.framebuffer.height as usize {
                let src = row * row_bytes;
                let dst = SYNTHETIC_FB_BASE + (row * pitch) as u32;
                ctx.cpu
                    .write_mem(dst, &ctx.kernel.framebuffer.pixels[src..src + row_bytes])?;
            }
        }
        ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
    }
    log::trace!("GXBeginDraw() -> 0x{:08x}", SYNTHETIC_FB_BASE);
    Ok(DispatchOutcome::ReturnedR0(SYNTHETIC_FB_BASE))
}

fn gx_end_draw(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if !ctx.kernel.fb_mapped {
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let fb_len = ctx.kernel.framebuffer.pixels.len();
    if ctx.kernel.gx_readback_scratch.len() != fb_len {
        ctx.kernel.gx_readback_scratch.resize(fb_len, 0);
    }
    let pitch = guest_pitch_bytes(ctx) as usize;
    let row_bytes = ctx.kernel.framebuffer.stride_bytes() as usize;
    if pitch == row_bytes {
        ctx.cpu
            .read_mem_into(SYNTHETIC_FB_BASE, &mut ctx.kernel.gx_readback_scratch)?;
    } else {
        for row in 0..ctx.kernel.framebuffer.height as usize {
            let src = SYNTHETIC_FB_BASE + (row * pitch) as u32;
            let dst = row * row_bytes;
            ctx.cpu.read_mem_into(
                src,
                &mut ctx.kernel.gx_readback_scratch[dst..dst + row_bytes],
            )?;
        }
    }
    let signature = sample_signature(
        &ctx.kernel.gx_readback_scratch,
        ctx.kernel.framebuffer.stride_bytes() as usize,
    );
    let changed = ctx.kernel.gx_readback_scratch != ctx.kernel.framebuffer.pixels;
    if changed {
        ctx.kernel
            .framebuffer
            .pixels
            .copy_from_slice(&ctx.kernel.gx_readback_scratch);
        ctx.kernel.framebuffer.mark_dirty();
        ctx.kernel.gx_guest_signature = Some(signature);
        ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn sample_signature(bytes: &[u8], stride_bytes: usize) -> u64 {
    let stride = stride_bytes.max(1);
    let mut hash = 14695981039346656037u64;
    for row in bytes.chunks(stride) {
        for byte in row.iter().step_by(16) {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(1099511628211);
        }
    }
    hash
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
    //   vkUp, vkDown, vkLeft, vkRight, vkA, vkB, vkC, vkStart.
    // The exact codes live in `pocket_kernel::gapi` so that the guest,
    // the frontends and the message pump all agree on which virtual key
    // is the A button.
    let sret = ctx.arg_u32(0)?;
    let buf = pocket_kernel::gapi::default_key_list();
    debug_assert_eq!(buf.len(), 0x60);
    ctx.cpu.write_mem(sret, &buf)?;
    // A guest that reads the key list drives its whole input layer off
    // this table and ignores virtual keys that are not in it, so from
    // here on a host VK_RETURN has to be delivered as `vkA`.
    ctx.kernel.gapi_keys_queried = true;
    Ok(DispatchOutcome::ReturnedR0(sret))
}

fn gx_is_display_dram_buffer(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}
fn gx_set_viewport(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let top = ctx.arg_u32(0)?;
    let height = ctx.arg_u32(1)?;
    let reserved1 = ctx.arg_u32(2)?;
    let reserved2 = ctx.arg_u32(3)?;
    log::debug!(
        "GXSetViewport(top={top}, height={height}, reserved1={reserved1}, reserved2={reserved2}) -> 1"
    );
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gx_get_display_properties(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // GXDisplayProperties is returned by value under the ARM ABI: the
    // hidden structure-return pointer is in r0, while the visible
    // function has no arguments. The import trace confirms that the
    // caller reserves the result at r0 and passes unrelated register
    // values in r1-r3. Treating r0 as an ordinary argument is correct,
    // but keep the pitch fields in the documented units: cbxPitch is
    // bytes per pixel and cbyPitch is bytes per scanline.
    let sret = ctx.arg_u32(0)?;
    let width = ctx.kernel.framebuffer.width;
    let height = ctx.kernel.framebuffer.height;
    let bpp = ctx.kernel.framebuffer.bpp;
    let mut buf = Vec::with_capacity(24);
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(&(bpp / 8).to_le_bytes());
    buf.extend_from_slice(&guest_pitch_bytes(ctx).to_le_bytes());
    buf.extend_from_slice(&bpp.to_le_bytes());
    // kfDirect565 is 0x80 in the WinCE GAPI header. The direct flag
    // is not a pixel-format bit and must not be ORed into ffFormat.
    buf.extend_from_slice(&0x0000_0080u32.to_le_bytes());
    ctx.cpu.write_mem(sret, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(sret))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu};
    use pocket_kernel::framebuffer::FB_BYTES;
    use pocket_kernel::{vfs::Vfs, Framebuffer, GdiState, Heap, KernelState, Thunk};
    use pocket_pe::ImportBinding;

    /// A blank `KernelState`. Shared with the other DLL test modules
    /// (see [`crate::gles`]) so the ~100-field literal lives in one
    /// place.
    pub(crate) fn fresh_kernel() -> KernelState {
        use pocket_kernel::audio::{AudioEngine, GuestFormat};
        KernelState {
            heap: Heap::new(0x5000_0000, 0x10000),
            vfs: Vfs::new(),
            registry: pocket_kernel::registry::Registry::new(),
            find_handles: std::collections::HashMap::new(),
            next_find_handle: 0,
            module_path: "\\Program Files\\Game\\Game.exe".to_string(),
            pending_startup: std::collections::VecDeque::new(),
            framebuffer: Framebuffer::default(),
            gdi: GdiState::new(),
            resources: vec![],
            image_base: 0,
            image_size: 0,
            image_entry: 0,
            dynamic_exports: std::collections::HashMap::new(),
            next_module_handle: 0x1000_0001,
            modules: Vec::new(),
            next_module_base: pocket_kernel::MODULE_REGION_BASE,
            module_search_dirs: Vec::new(),
            fb_mapped: false,
            direct_fb_frames: 0,
            gx_readback_scratch: Vec::new(),
            mem_op_scratch: Vec::new(),
            mem_op_scratch_b: Vec::new(),
            bit_blt_src_scratch: Vec::new(),
            dib_sync_scratch: Vec::new(),
            dib_decode_scratch: Vec::new(),
            gx_last_pushed_counter: 0,
            gx_guest_signature: None,
            gles_strings: std::collections::HashMap::new(),
            synthetic_message_count: 0,
            synthetic_message_budget: 240,
            wnd_proc: 0,
            window_class_procs: std::collections::HashMap::new(),
            window_background: None,
            pending_create: None,
            window_procs: std::collections::HashMap::new(),
            window_userdata: std::collections::HashMap::new(),
            window_classes: std::collections::HashMap::new(),
            window_user_data: 0,
            synthetic_timer_id: 0,
            synthetic_timer_interval_ms: 16,
            synthetic_timer_next_ms: 0,
            synthetic_paint_next_ms: 0,
            synthetic_create_sent: false,
            synthetic_size_sent: false,
            create_frame: None,
            create_stage: pocket_kernel::CreateStage::Idle,
            dialog_frame: None,
            status_bar: None,
            message_frame: None,
            controls: Default::default(),
            pending_input: std::collections::VecDeque::new(),
            gapi_keys_queried: false,
            pending_message: None,
            threads: Vec::new(),
            events: Default::default(),
            semaphores: Default::default(),
            current_thread: 0,
            pressed_keys: [false; 256],
            should_stop: false,
            tls_slots_used: 0,
            vector_iter_stack: Vec::new(),
            qsort_frames: std::collections::HashMap::new(),
            strtok_pos: 0,
            security_cookie: 0,
            audio: AudioEngine::new(),
            wave_out_format: GuestFormat::default(),
            wave_out: Default::default(),
            hss: Default::default(),
            posted_messages: Default::default(),
            msg_queues: std::collections::HashMap::new(),
            next_msg_queue_handle: 0xDEAD_E500,
            menus: std::collections::HashMap::new(),
            next_menu_handle: 0xDEAD_2000,
            sub_menus: std::collections::HashMap::new(),
            modal: None,
            message_box_spins: 0,
            modal_dialog: None,
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
    fn set_viewport_returns_success() {
        let mut cpu = StubCpu::new();
        let mut kernel = fresh_kernel();
        let t = dummy_thunk();
        cpu.write_reg(ArmReg::R0, 0).unwrap();
        cpu.write_reg(ArmReg::R1, 320).unwrap();
        cpu.write_reg(ArmReg::R2, 0).unwrap();
        cpu.write_reg(ArmReg::R3, 0).unwrap();
        let mut c = CallCtx {
            cpu: &mut cpu,
            thunk: &t,
            kernel: &mut kernel,
        };
        assert_eq!(
            gx_set_viewport(&mut c).unwrap(),
            DispatchOutcome::ReturnedR0(1)
        );
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
