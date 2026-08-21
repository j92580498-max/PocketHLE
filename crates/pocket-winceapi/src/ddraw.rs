use pocket_cpu::Prot;
use pocket_kernel::{DispatchOutcome, KernelError, SYNTHETIC_FRAMEBUFFER_BASE};

use crate::{CallCtx, WinCeDispatcher};

const FAKE_DDRAW: u32 = 0xDEAD_DD01;
const FAKE_SURFACE: u32 = 0xDEAD_DD02;
const FAKE_PALETTE: u32 = 0xDEAD_DD03;
const FAKE_MODULE_HANDLE: u32 = 0x1000_0003;

const DDRAW_METHODS: [&str; 22] = [
    "ddraw_qi",
    "ddraw_add_ref",
    "ddraw_release",
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
            "surface_unlock" => surface_unlock,
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

fn create_surface_at(
    ctx: &mut CallCtx<'_>,
    out: u32,
) -> Result<DispatchOutcome, KernelError> {
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
    let looks_like_surface_desc = desc != 0
        && out != 0
        && matches!(ctx.cpu.read_u32_le(desc), Ok(108 | 124));
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
    let mut bytes = [0u8; 124];
    bytes[0..4].copy_from_slice(&124u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&0x0000_100fu32.to_le_bytes());
    bytes[8..12].copy_from_slice(&ctx.kernel.framebuffer.height.to_le_bytes());
    bytes[12..16].copy_from_slice(&ctx.kernel.framebuffer.width.to_le_bytes());
    bytes[16..20].copy_from_slice(&ctx.kernel.framebuffer.stride_bytes().to_le_bytes());
    bytes[36..40].copy_from_slice(&surface.to_le_bytes());
    bytes[56..60].copy_from_slice(&32u32.to_le_bytes());
    bytes[60..64].copy_from_slice(&0x40u32.to_le_bytes());
    bytes[68..72].copy_from_slice(&16u32.to_le_bytes());
    bytes[72..76].copy_from_slice(&0xf800u32.to_le_bytes());
    bytes[76..80].copy_from_slice(&0x07e0u32.to_le_bytes());
    bytes[80..84].copy_from_slice(&0x001fu32.to_le_bytes());
    ctx.cpu.write_mem(desc, &bytes)?;
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
    if desc != 0 && !(0x5000_0000..0x5f00_0000).contains(&desc) {
        write_surface_desc(ctx, desc, SYNTHETIC_FRAMEBUFFER_BASE)?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_get_dc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
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

fn ensure_framebuffer(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    if !ctx.kernel.fb_mapped {
        let size = pocket_cpu::round_up_to_page(ctx.kernel.framebuffer.byte_size());
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
    if desc != 0 {
        write_surface_desc(ctx, desc, SYNTHETIC_FRAMEBUFFER_BASE)?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_unlock(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_framebuffer(ctx)?;
    let mut pixels = vec![0u8; ctx.kernel.framebuffer.pixels.len()];
    ctx.cpu
        .read_mem_into(SYNTHETIC_FRAMEBUFFER_BASE, &mut pixels)?;
    if pixels != ctx.kernel.framebuffer.pixels {
        ctx.kernel.framebuffer.pixels.copy_from_slice(&pixels);
        ctx.kernel.framebuffer.mark_dirty();
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}
