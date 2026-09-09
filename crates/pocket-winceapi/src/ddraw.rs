use pocket_cpu::Prot;
use pocket_kernel::{DispatchOutcome, KernelError, SYNTHETIC_FRAMEBUFFER_BASE};

use crate::{CallCtx, WinCeDispatcher};

const FAKE_DDRAW: u32 = 0xDEAD_DD01;
const FAKE_SURFACE: u32 = 0xDEAD_DD02;
const FAKE_PALETTE: u32 = 0xDEAD_DD03;
const FAKE_MODULE_HANDLE: u32 = 0x1000_0003;

const DDRAW_METHODS: [&str; 23] = [
    "ddraw_qi",
    "ddraw_add_ref",
    "ddraw_release",
    "ddraw_compact",
    "ddraw_create_clipper",
    "ddraw_create_palette",
    "ddraw_create_surface",
    "ddraw_duplicate_surface",
    "ddraw_enum_display_modes",
    "ddraw_enum_surfaces",
    "ddraw_flip_to_gdi",
    "ddraw_get_caps",
    "ddraw_get_display_mode",
    "ddraw_get_fourcc_codes",
    "ddraw_get_gdi_surface",
    "ddraw_get_monitor_frequency",
    "ddraw_get_scan_line",
    "ddraw_get_vertical_blank_status",
    "ddraw_initialize",
    "ddraw_restore_display_mode",
    "ddraw_set_cooperative_level",
    "ddraw_set_display_mode",
    "ddraw_wait_for_vertical_blank",
];

const PALETTE_METHODS: [&str; 7] = [
    "palette_qi",
    "palette_add_ref",
    "palette_release",
    "palette_get_caps",
    "palette_get_entries",
    "palette_initialize",
    "palette_set_entries",
];

const CLIPPER_METHODS: [&str; 9] = [
    "clipper_qi",
    "clipper_add_ref",
    "clipper_release",
    "clipper_get_clip_list",
    "clipper_get_hwnd",
    "clipper_initialize",
    "clipper_is_clip_list_changed",
    "clipper_set_clip_list",
    "clipper_set_hwnd",
];

const SURFACE_METHODS: [&str; 40] = [
    "surface_qi",
    "surface_add_ref",
    "surface_release",
    "surface_add_attached",
    "surface_add_overlay_dirty",
    "surface_blt",
    "surface_blt_batch",
    "surface_blt_fast",
    "surface_delete_attached",
    "surface_enum_attached",
    "surface_enum_overlay",
    "surface_flip",
    "surface_get_attached",
    "surface_get_blt_status",
    "surface_get_caps",
    "surface_get_clipper",
    "surface_get_color_key",
    "surface_get_dc",
    "surface_get_flip_status",
    "surface_get_overlay_position",
    "surface_get_palette",
    "surface_get_pixel_format",
    "surface_get_surface_desc",
    "surface_initialize",
    "surface_is_lost",
    "surface_lock",
    "surface_release_dc",
    "surface_restore",
    "surface_set_clipper",
    "surface_set_color_key",
    "surface_set_overlay_position",
    "surface_set_palette",
    "surface_unlock",
    "surface_update_overlay",
    "surface_update_overlay_display",
    "surface_update_overlay_z_order",
    "surface_get_dd_interface",
    "surface_page_lock",
    "surface_page_unlock",
    "surface_set_surface_desc",
];

pub fn register(d: &mut WinCeDispatcher) {
    d.register_handler("ddraw.dll", "DirectDrawCreate", direct_draw_create);
    d.register_handler("coredll.dll", "DirectDrawCreate", direct_draw_create);
    for name in DDRAW_METHODS
        .iter()
        .chain(PALETTE_METHODS.iter())
        .chain(CLIPPER_METHODS.iter())
        .chain(SURFACE_METHODS.iter())
    {
        let handler = match *name {
            "ddraw_qi" => ddraw_qi,
            "ddraw_add_ref" => add_ref,
            "ddraw_release" => release,
            "ddraw_compact" => ddraw_compact,
            "ddraw_create_surface" => ddraw_create_surface,
            "ddraw_flip_to_gdi" => ddraw_flip_to_gdi_or_create_surface,
            "ddraw_initialize" => ddraw_initialize,
            "ddraw_create_palette" => ddraw_create_palette,
            "ddraw_create_clipper" => ddraw_create_clipper,
            "ddraw_get_caps"
            | "ddraw_get_fourcc_codes"
            | "ddraw_get_gdi_surface"
            | "ddraw_get_monitor_frequency"
            | "ddraw_restore_display_mode"
            | "ddraw_set_cooperative_level"
            | "ddraw_set_display_mode" => ddraw_ok,
            "ddraw_get_display_mode" => ddraw_get_display_mode,
            "ddraw_get_vertical_blank_status" => ddraw_get_vertical_blank_status,
            "ddraw_get_scan_line" => ddraw_get_scan_line,
            "ddraw_enum_surfaces" => ddraw_enum_surfaces,
            "ddraw_enum_display_modes" => ddraw_enum_display_modes,
            "palette_qi" => palette_qi,
            "palette_add_ref" => add_ref,
            "palette_release" => release,
            "palette_get_caps" | "palette_get_entries" => palette_ok,
            "palette_initialize" | "palette_set_entries" => palette_ok,
            "clipper_qi" => clipper_qi,
            "clipper_add_ref" => add_ref,
            "clipper_release" => release,
            "clipper_set_hwnd" => clipper_set_hwnd,
            _ if name.starts_with("clipper_") => clipper_ok,
            "surface_qi" => surface_qi,
            "surface_add_ref" => add_ref,
            "surface_release" => release,
            "surface_get_attached" => surface_get_attached,
            "surface_get_blt_status" | "surface_get_flip_status" => surface_status,
            "surface_get_caps" | "surface_get_pixel_format" => surface_ok,
            "surface_get_color_key" => surface_get_color_key,
            "surface_get_surface_desc" => surface_desc,
            "surface_get_dc" => surface_get_dc,
            "surface_is_lost" => surface_is_lost,
            "surface_lock" => surface_lock,
            // Slot 19 in PocketHLE's table is named after the desktop
            // IDirectDrawSurface layout, where it is GetOverlayPosition.
            // Asphalt 4 (and, presumably, other Pocket PC titles built
            // against the CE ddraw.h) calls this slot with Lock's exact
            // signature -- (this, lpDestRect=NULL, lpDDSurfaceDesc,
            // dwFlags, hEvent) -- and then reads the surface pointer
            // back out of the descriptor it passed. The CE surface
            // interface is trimmed relative to the desktop one, so the
            // slot numbering does not line up. Route it to Lock; a real
            // GetOverlayPosition call is a no-op here anyway, so the
            // worst case for a title that genuinely wanted overlay
            // coordinates is the same no-op it got before.
            "surface_get_overlay_position" => surface_lock,
            "surface_unlock" => surface_unlock,
            // Slot 26, `ReleaseDC` in the desktop naming, is the other
            // half of the same shift. Desktop `Lock` is slot 25 and the
            // CE guests call it at 19 -- six lower. Desktop `Unlock` is
            // slot 32, and 32 - 6 = 26, which lands exactly here. NFS
            // Undercover confirms the pairing: across a whole session it
            // calls slot 19 once and slot 26 once, and nothing else.
            //
            // This matters more than a missing no-op, because
            // `surface_unlock` is the only thing that marks the
            // framebuffer dirty. Routed to the generic `surface_ok`, a
            // game locks the surface, renders into it, "unlocks", and
            // never presents a single frame -- which looks from outside
            // exactly like a game that renders nothing at all.
            //
            // A genuine `ReleaseDC` after `GetDC` also wants the same
            // treatment (publish what was drawn), so this is the right
            // behaviour for both readings of the slot.
            "surface_release_dc" => surface_unlock,
            "surface_get_dd_interface" => surface_get_dd_interface,
            "surface_page_lock" | "surface_page_unlock" | "surface_set_surface_desc" => surface_ok,
            _ if name.starts_with("surface_") => surface_ok,
            _ => ddraw_ok,
        };
        d.register_handler("coredll.dll", name, handler);
    }
}

fn dynamic_address(ctx: &CallCtx<'_>, name: &str) -> u32 {
    ctx.kernel
        .dynamic_exports
        .get(&FAKE_MODULE_HANDLE)
        .and_then(|m| m.get(name).copied())
        .or_else(|| {
            ctx.kernel
                .dynamic_exports
                .get(&0x1000_0000)
                .and_then(|m| m.get(name).copied())
        })
        .unwrap_or(0)
}

fn write_vtable(ctx: &mut CallCtx<'_>, ptr: u32, names: &[&str]) -> Result<(), KernelError> {
    for (i, name) in names.iter().enumerate() {
        let address = dynamic_address(ctx, name);
        log::debug!("DirectDraw vtable[{i}] {name} -> 0x{address:08x}");
        ctx.cpu
            .write_mem(ptr + i as u32 * 4, &address.to_le_bytes())?;
    }
    Ok(())
}

fn alloc_object(ctx: &mut CallCtx<'_>, vtable: &[&str], tag: u32) -> Result<u32, KernelError> {
    let table = ctx.kernel.heap.alloc(vtable.len() as u32 * 4).unwrap_or(0);
    let object = ctx.kernel.heap.alloc(4).unwrap_or(0);
    if table == 0 || object == 0 {
        return Ok(0);
    }
    write_vtable(ctx, table, vtable)?;
    ctx.cpu.write_mem(object, &table.to_le_bytes())?;
    log::debug!("allocated DirectDraw object {tag:#x} at {object:#x}");
    Ok(object)
}

fn direct_draw_create(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
    let object = alloc_object(ctx, &DDRAW_METHODS, FAKE_DDRAW)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(if object != 0 {
        0
    } else {
        0x8000_4005
    }))
}

fn ddraw_create_clipper(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(2)?;
    let object = alloc_object(ctx, &CLIPPER_METHODS, 0xDEAD_DD04)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(if object != 0 {
        0
    } else {
        0x8000_4005
    }))
}

fn ddraw_qi(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(2)?;
    if out != 0 {
        let object = ctx.arg_u32(0)?;
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ddraw_create_palette(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(2)?;
    let object = alloc_object(ctx, &PALETTE_METHODS, FAKE_PALETTE)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(if object != 0 {
        0
    } else {
        0x8000_4005
    }))
}

fn ddraw_initialize(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_framebuffer(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn create_surface_at(ctx: &mut CallCtx<'_>, out: u32) -> Result<DispatchOutcome, KernelError> {
    let object = alloc_object(ctx, &SURFACE_METHODS, FAKE_SURFACE)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(if object != 0 {
        0
    } else {
        0x8000_4005
    }))
}

fn ddraw_create_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _desc = ctx.arg_u32(1)?;
    let out = ctx.arg_u32(2)?;
    create_surface_at(ctx, out)
}

fn ddraw_flip_to_gdi_or_create_surface(
    ctx: &mut CallCtx<'_>,
) -> Result<DispatchOutcome, KernelError> {
    let desc = ctx.arg_u32(1)?;
    let out = ctx.arg_u32(2)?;
    let size = ctx.cpu.read_u32_le(desc).unwrap_or(0);
    let looks_like_surface_desc = desc != 0
        && out != 0
        && (matches!(size, 0x6c | 0x7c)
            || ((0x5fff_0000..0x6000_0000).contains(&desc)
                && (0x5fff_0000..0x6000_0000).contains(&out)));
    if looks_like_surface_desc {
        log::debug!(
            "DirectDraw compact vtable slot 9 used as CreateSurface(desc=0x{desc:08x}, out=0x{out:08x})"
        );
        return create_surface_at(ctx, out);
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn clipper_qi(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(2)?;
    if out != 0 {
        let object = ctx.arg_u32(0)?;
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn clipper_ok(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn clipper_set_hwnd(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn palette_qi(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(2)?;
    if out != 0 {
        let object = ctx.arg_u32(0)?;
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn palette_ok(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ddraw_get_vertical_blank_status(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ddraw_get_scan_line(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _ = ctx.arg_u32(1)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ddraw_enum_surfaces(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ddraw_enum_display_modes(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ddraw_get_display_mode(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let desc = ctx.arg_u32(1)?;
    if desc != 0 && !(0x5000_0000..0x5f00_0000).contains(&desc) {
        write_surface_desc(ctx, desc, SYNTHETIC_FRAMEBUFFER_BASE)?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ddraw_ok(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ddraw_compact(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_ok(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn add_ref(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn release(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_qi(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(2)?;
    if out != 0 {
        let object = ctx.arg_u32(0)?;
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_get_attached(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &FAKE_SURFACE.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_status(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn write_surface_desc(ctx: &mut CallCtx<'_>, desc: u32, surface: u32) -> Result<(), KernelError> {
    if desc == 0 {
        return Ok(());
    }
    // `dwSize` of 124 means this is a DDSURFACEDESC2, whose layout is:
    //   0   dwSize            28  dwAlphaBitDepth   72  ddpfPixelFormat (32 B)
    //   4   dwFlags           32  dwReserved       104  ddsCaps2 (16 B)
    //   8   dwHeight          36  lpSurface        120  dwTextureStage
    //  12   dwWidth           40  ddckCKDestOverlay
    //  16   lPitch            48  ddckCKDestBlt
    //  20   dwBackBufferCount 56  ddckCKSrcOverlay
    //  24   dwMipMapCount     64  ddckCKSrcBlt
    // Honour the caller's `dwSize`.
    //
    // DirectDraw's contract is that the *caller* sets `dwSize` and the
    // callee fills at most that many bytes. Writing a fixed 124
    // overruns any guest using the smaller layout -- and these
    // descriptors are almost always stack locals, so the overrun lands
    // in the caller's live frame. NFS Undercover passed a descriptor at
    // `0x5ffff8c0` with `SP=0x5ffff938`: 124 bytes reached
    // `0x5ffff93c`, four bytes past `SP`, and the function returned
    // through a smashed `LR` into its own stack.
    //
    // Clamp to what the caller declared, floor it at the offsets we
    // must fill for Lock to be useful (`lpSurface` at 32 and 36), and
    // leave the caller's own `dwSize` value intact rather than
    // rewriting it to 124.
    let declared =
        u32::from_le_bytes(ctx.cpu.read_mem(desc, 4)?.try_into().unwrap_or([0u8; 4])) as usize;
    let size = if (40..=124).contains(&declared) {
        declared
    } else {
        // Nothing sane declared: fall back to the larger layout, which
        // is what the previous unconditional write assumed.
        124
    };
    let mut bytes = [0u8; 124];
    bytes[0..4].copy_from_slice(&(size as u32).to_le_bytes());
    // DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PITCH
    //   | DDSD_LPSURFACE (0x800) | DDSD_PIXELFORMAT (0x1000).
    //
    // DDSD_LPSURFACE was missing here. We were filling in `lpSurface`
    // at offset 36 correctly, but never telling the caller the field
    // was valid -- and a guest that checks the flag before trusting
    // the pointer (which is the documented contract for Lock) reads
    // the flag as clear, ignores our pointer, and ends up blitting
    // against a base of zero. Asphalt 4 does exactly that: every
    // scanline of its splash blit lands at `0 + row_offset`, which is
    // why the destinations we saw walking up from 0x0000 in 0x1e0
    // steps looked like plain offsets rather than addresses.
    bytes[4..8].copy_from_slice(&0x0000_180fu32.to_le_bytes());
    bytes[8..12].copy_from_slice(&ctx.kernel.framebuffer.height.to_le_bytes());
    bytes[12..16].copy_from_slice(&ctx.kernel.framebuffer.width.to_le_bytes());
    bytes[16..20].copy_from_slice(&ctx.kernel.framebuffer.stride_bytes().to_le_bytes());
    // lpSurface: written at BOTH 32 and 36.
    //
    // The DirectX 1-3 era DDSURFACEDESC has no dwReserved member, so
    // lpSurface sits at 32; the later layout inserts dwReserved and
    // pushes it to 36. Asphalt 4 demonstrably reads it at 32 (it
    // passes a descriptor at sb+0x164 and then loads its blit base
    // from sb+0x184, i.e. desc+32), while the 108-byte descriptor it
    // memsets matches the *later* layout's size. Rather than bet on
    // one reading, fill both slots -- offset 36 is only a color-key
    // field in the earlier layout, and a Lock output descriptor is
    // the caller's scratch buffer either way, so the redundant write
    // is harmless.
    bytes[32..36].copy_from_slice(&surface.to_le_bytes());
    bytes[36..40].copy_from_slice(&surface.to_le_bytes());
    // ddpfPixelFormat starts at 72, not 56. The four DDCOLORKEY
    // members before it are 8 bytes each and span 40..72, so the old
    // base wrote the whole pixel-format block over ddckCKSrcOverlay
    // and ddckCKSrcBlt, leaving the real pixel format all zeroes.
    bytes[72..76].copy_from_slice(&32u32.to_le_bytes()); // dwSize
    bytes[76..80].copy_from_slice(&0x40u32.to_le_bytes()); // DDPF_RGB
    bytes[84..88].copy_from_slice(&16u32.to_le_bytes()); // dwRGBBitCount
    bytes[88..92].copy_from_slice(&0xf800u32.to_le_bytes()); // R mask
    bytes[92..96].copy_from_slice(&0x07e0u32.to_le_bytes()); // G mask
    bytes[96..100].copy_from_slice(&0x001fu32.to_le_bytes()); // B mask
    log::debug!("write_surface_desc: desc=0x{desc:08x} declared={declared} writing {size} bytes");
    ctx.cpu.write_mem(desc, &bytes[..size])?;
    Ok(())
}

fn surface_get_color_key(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let color_key = ctx.arg_u32(3)?;
    if color_key != 0 {
        ctx.cpu.write_mem(color_key, &[0; 8])?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_desc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let desc = ctx.arg_u32(1)?;
    // NOTE: this guard skips the write entirely for any `desc` in
    // 0x50000000..0x5f000000 -- which is the heap range in this
    // environment. A guest that hands us a heap-allocated
    // DDSURFACEDESC therefore gets nothing written, leaving
    // `lpSurface` at whatever was already there (usually zero).
    // Logging both branches so we can see which one a given title
    // actually takes before deciding whether the guard is load-bearing.
    let skipped = desc != 0 && (0x5000_0000..0x5f00_0000).contains(&desc);
    log::debug!("surface_desc: desc=0x{desc:08x} skipped_by_heap_guard={skipped}");
    if desc != 0 && !(0x5000_0000..0x5f00_0000).contains(&desc) {
        write_surface_desc(ctx, desc, SYNTHETIC_FRAMEBUFFER_BASE)?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_get_dc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
    log::debug!("surface_get_dc: out=0x{out:08x} -> hdc=0xdead1001");
    if out != 0 {
        ctx.cpu.write_mem(out, &0xDEAD_1001u32.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_get_dd_interface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &0xDEAD_DD01u32.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// Size of the synthetic framebuffer mapping.
///
/// `ensure_framebuffer` maps this region exactly once, but the
/// framebuffer's own dimensions can grow afterwards -- Asphalt 4
/// starts at 240x320 and then presents a 242x402 surface, which is
/// larger than the originally mapped 0x26000 bytes. The guest holds
/// the base pointer it got from Lock and blits straight past the end
/// of the old mapping, and every scanline past the boundary fails
/// with WRITE_UNMAPPED. Rather than track the mapped size and remap
/// on every resize, reserve a region comfortably larger than any
/// plausible Pocket PC surface (4 MB covers e.g. 640x480 at 32bpp
/// several times over) so a resize can never outgrow it. The pages
/// are only backed as they're touched, so the reservation is cheap.
const FRAMEBUFFER_REGION_SIZE: u32 = 0x40_0000;

fn ensure_framebuffer(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    if !ctx.kernel.fb_mapped {
        let needed = pocket_cpu::round_up_to_page(ctx.kernel.framebuffer.byte_size());
        let size = needed.max(FRAMEBUFFER_REGION_SIZE);
        ctx.cpu
            .map_region(SYNTHETIC_FRAMEBUFFER_BASE, size, Prot::READ | Prot::WRITE)?;
        ctx.cpu
            .write_mem(SYNTHETIC_FRAMEBUFFER_BASE, &ctx.kernel.framebuffer.pixels)?;
        ctx.kernel.fb_mapped = true;
    }
    Ok(())
}

fn surface_is_lost(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_lock(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_framebuffer(ctx)?;
    let desc = ctx.arg_u32(2)?;
    // Diagnostic: we have never actually confirmed that the guest
    // calls Lock at all on the frame it blits. If these lines never
    // appear, the guest is getting its destination base from some
    // other route entirely (GetDC, or the CreateDIBSection bits) and
    // no amount of correctness work on the surface desc can affect
    // its blit destination.
    log::debug!(
        "surface_lock: desc=0x{desc:08x} -> lpSurface=0x{base:08x}",
        base = SYNTHETIC_FRAMEBUFFER_BASE
    );
    if desc != 0 {
        write_surface_desc(ctx, desc, SYNTHETIC_FRAMEBUFFER_BASE)?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_unlock(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_framebuffer(ctx)?;
    // Read straight into the framebuffer's own storage.
    //
    // This used to allocate a fresh full-frame `Vec` per call, read
    // into it, compare all of it against the framebuffer, and then
    // copy all of it across on a difference -- four passes over
    // ~192 KB (240x400x2) plus a heap allocation, every unlock. At a
    // couple of unlocks per frame that was the single largest cost in
    // the frame, and the compare almost never paid off: a game only
    // unlocks after it has drawn, so the contents have essentially
    // always changed. Read once, mark dirty unconditionally.
    //
    // `cpu` and `kernel` are separate borrows of `CallCtx`, so
    // reading guest memory directly into the framebuffer is fine.
    let base = SYNTHETIC_FRAMEBUFFER_BASE;
    ctx.cpu
        .read_mem_into(base, &mut ctx.kernel.framebuffer.pixels)?;
    ctx.kernel.framebuffer.mark_dirty();
    Ok(DispatchOutcome::ReturnedR0(0))
}
