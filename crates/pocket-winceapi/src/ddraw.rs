use pocket_cpu::Prot;
use pocket_kernel::{DispatchOutcome, KernelError, SYNTHETIC_FRAMEBUFFER_BASE};

use crate::{CallCtx, WinCeDispatcher};

const FAKE_DDRAW: u32 = 0xDEAD_DD01;
const FAKE_SURFACE: u32 = 0xDEAD_DD02;
const FAKE_PALETTE: u32 = 0xDEAD_DD03;
const FAKE_MODULE_HANDLE: u32 = 0x1000_0003;

/// `sizeof(DDSURFACEDESC)` as Windows CE declares it — 108 bytes, not the
/// desktop 124. The two headers disagree about the struct as well as the
/// vtables; see [`surface_desc_bytes`].
const DDSURFACEDESC_SIZE: u32 = 108;

/// `IDirectDraw`, in the order Windows CE's `ddraw.h` declares it.
///
/// Windows CE's DirectDraw is *not* desktop DirectDraw with a few methods
/// stubbed out. The methods it does not implement are absent from the
/// vtable entirely, so every slot after the first gap shifts down, and a
/// guest calling by slot lands on a different function than the desktop
/// header says. Against the Windows Mobile 5.0 SDK header, CE drops
/// `Compact`, `DuplicateSurface` and `Initialize`, and appends five
/// methods after `WaitForVerticalBlank`.
///
/// Digital Chocolate's Tower Bloxx is what this cost. With the desktop
/// order its `CreateClipper` (CE slot 3) ran `Compact`, so the clipper
/// out-parameter was never written; the game then called
/// `clipper->lpVtbl->SetHWnd` on a NULL pointer and died at
/// `pc=0x00088d58` before its first frame. `CreateSurface` (CE slot 5)
/// ran `CreatePalette` and `SetCooperativeLevel` (CE slot 17) ran
/// `GetVerticalBlankStatus` in the same run.
const DDRAW_METHODS: [&str; 25] = [
    "ddraw_qi",
    "ddraw_add_ref",
    "ddraw_release",
    "ddraw_create_clipper",
    "ddraw_create_palette",
    "ddraw_create_surface",
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
    "ddraw_restore_display_mode",
    "ddraw_set_cooperative_level",
    "ddraw_set_display_mode",
    "ddraw_wait_for_vertical_blank",
    "ddraw_get_available_vid_mem",
    "ddraw_get_surface_from_dc",
    "ddraw_restore_all_surfaces",
    "ddraw_test_cooperative_level",
    "ddraw_get_device_identifier",
];

/// `IDirectDrawPalette` on Windows CE — `Initialize` is absent, because CE
/// DirectDraw has no `CoCreateInstance` path for an interface to be
/// initialized after the fact.
const PALETTE_METHODS: [&str; 6] = [
    "palette_qi",
    "palette_add_ref",
    "palette_release",
    "palette_get_caps",
    "palette_get_entries",
    "palette_set_entries",
];

/// `IDirectDrawClipper` on Windows CE — again without `Initialize`, which
/// puts `SetHWnd` at slot 7 rather than the desktop's slot 8.
const CLIPPER_METHODS: [&str; 8] = [
    "clipper_qi",
    "clipper_add_ref",
    "clipper_release",
    "clipper_get_clip_list",
    "clipper_get_hwnd",
    "clipper_is_clip_list_changed",
    "clipper_set_clip_list",
    "clipper_set_hwnd",
];

/// `IDirectDrawSurface` on Windows CE.
///
/// CE drops `AddAttachedSurface`, `BltBatch`, `BltFast`,
/// `DeleteAttachedSurface`, `GetAttachedSurface`, `Initialize` and
/// `UpdateOverlayDisplay`, and appends `GetDDInterface` and `AlphaBlt`.
/// The shift matters most for `Lock` (CE slot 19, desktop 25) and
/// `Unlock` (CE slot 26, desktop 32) — the two calls that actually move
/// pixels, and therefore the reason a mis-ordered table renders nothing
/// at all rather than rendering something wrong.
const SURFACE_METHODS: [&str; 31] = [
    "surface_qi",
    "surface_add_ref",
    "surface_release",
    "surface_add_overlay_dirty",
    "surface_blt",
    "surface_enum_attached",
    "surface_enum_overlay",
    "surface_flip",
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
    "surface_update_overlay_z_order",
    "surface_get_dd_interface",
    "surface_alpha_blt",
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
            "ddraw_create_palette" => ddraw_create_palette,
            "ddraw_create_clipper" => ddraw_create_clipper,
            "ddraw_set_cooperative_level" => ddraw_set_cooperative_level,
            "ddraw_get_caps"
            | "ddraw_get_fourcc_codes"
            | "ddraw_get_monitor_frequency"
            | "ddraw_restore_display_mode"
            | "ddraw_restore_all_surfaces"
            | "ddraw_test_cooperative_level"
            | "ddraw_get_device_identifier"
            | "ddraw_set_display_mode" => ddraw_ok,
            "ddraw_get_display_mode" => ddraw_get_display_mode,
            "ddraw_get_vertical_blank_status" => ddraw_get_vertical_blank_status,
            "ddraw_get_scan_line" => ddraw_get_scan_line,
            "ddraw_enum_surfaces" => ddraw_enum_surfaces,
            "ddraw_enum_display_modes" => ddraw_enum_display_modes,
            "ddraw_get_gdi_surface" => ddraw_get_gdi_surface,
            "ddraw_get_surface_from_dc" => ddraw_get_surface_from_dc,
            "ddraw_get_available_vid_mem" => ddraw_get_available_vid_mem,
            "palette_qi" => palette_qi,
            "palette_add_ref" => add_ref,
            "palette_release" => release,
            "palette_get_caps" | "palette_get_entries" | "palette_set_entries" => palette_ok,
            "clipper_qi" => clipper_qi,
            "clipper_add_ref" => add_ref,
            "clipper_release" => release,
            "clipper_set_hwnd" => clipper_set_hwnd,
            _ if name.starts_with("clipper_") => clipper_ok,
            "surface_qi" => surface_qi,
            "surface_add_ref" => add_ref,
            "surface_release" => release,
            "surface_blt" | "surface_alpha_blt" => surface_blt,
            "surface_get_blt_status" | "surface_get_flip_status" => surface_status,
            "surface_get_pixel_format" => surface_get_pixel_format,
            "surface_get_color_key" => surface_get_color_key,
            "surface_get_surface_desc" => surface_desc,
            "surface_get_dc" => surface_get_dc,
            "surface_is_lost" => surface_is_lost,
            "surface_lock" => surface_lock,
            "surface_unlock" => surface_unlock,
            "surface_flip" => surface_flip,
            "surface_get_dd_interface" => surface_get_dd_interface,
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
    alloc_object_with(ctx, vtable, tag, &[])
}

/// COM objects here are `[vtable_ptr, ..private words]` in guest heap.
///
/// A surface keeps its own geometry and pixel pointer in those private
/// words, which is what lets `Blt` be a real copy between two distinct
/// surfaces rather than a no-op — see [`SurfaceRecord`].
fn alloc_object_with(
    ctx: &mut CallCtx<'_>,
    vtable: &[&str],
    tag: u32,
    private: &[u32],
) -> Result<u32, KernelError> {
    let table = ctx.kernel.heap.alloc(vtable.len() as u32 * 4).unwrap_or(0);
    let object = ctx
        .kernel
        .heap
        .alloc(4 + private.len() as u32 * 4)
        .unwrap_or(0);
    if table == 0 || object == 0 {
        return Ok(0);
    }
    write_vtable(ctx, table, vtable)?;
    ctx.cpu.write_mem(object, &table.to_le_bytes())?;
    for (i, word) in private.iter().enumerate() {
        ctx.cpu
            .write_mem(object + 4 + i as u32 * 4, &word.to_le_bytes())?;
    }
    log::debug!("allocated DirectDraw object {tag:#x} at {object:#x}");
    Ok(object)
}

/// Marks a surface object as ours, so a `Blt` that is handed a pointer
/// from somewhere else falls back instead of reading garbage geometry.
const SURFACE_MAGIC: u32 = 0x5048_5346;

/// What a surface object carries past its vtable pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceRecord {
    pixels: u32,
    width: u32,
    height: u32,
    pitch: u32,
    primary: bool,
}

impl SurfaceRecord {
    fn private_words(&self) -> [u32; 6] {
        [
            SURFACE_MAGIC,
            self.pixels,
            self.width,
            self.height,
            self.pitch,
            u32::from(self.primary),
        ]
    }
}

fn surface_record(ctx: &mut CallCtx<'_>, object: u32) -> Option<SurfaceRecord> {
    if object == 0 {
        return None;
    }
    let word = |ctx: &mut CallCtx<'_>, i: u32| ctx.cpu.read_u32_le(object + 4 + i * 4).ok();
    if word(ctx, 0)? != SURFACE_MAGIC {
        return None;
    }
    Some(SurfaceRecord {
        pixels: word(ctx, 1)?,
        width: word(ctx, 2)?,
        height: word(ctx, 3)?,
        pitch: word(ctx, 4)?,
        primary: word(ctx, 5)? != 0,
    })
}

fn this_surface(ctx: &mut CallCtx<'_>) -> Result<Option<SurfaceRecord>, KernelError> {
    let object = ctx.arg_u32(0)?;
    Ok(surface_record(ctx, object))
}

/// The panel itself, for surfaces we could not give private storage to.
fn panel_record(ctx: &CallCtx<'_>) -> SurfaceRecord {
    SurfaceRecord {
        pixels: SYNTHETIC_FRAMEBUFFER_BASE,
        width: ctx.kernel.framebuffer.width,
        height: ctx.kernel.framebuffer.height,
        pitch: ctx.kernel.framebuffer.stride_bytes(),
        primary: true,
    }
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

/// `SetCooperativeLevel` is where a Windows CE game announces it is about
/// to draw, and CE has no `Initialize` slot to do it in instead, so this
/// is the point the synthetic framebuffer has to exist by.
fn ddraw_set_cooperative_level(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_framebuffer(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn create_surface_at(ctx: &mut CallCtx<'_>, out: u32) -> Result<DispatchOutcome, KernelError> {
    create_surface_described(ctx, out, panel_record(ctx))
}

fn create_surface_described(
    ctx: &mut CallCtx<'_>,
    out: u32,
    record: SurfaceRecord,
) -> Result<DispatchOutcome, KernelError> {
    ensure_framebuffer(ctx)?;
    let object = alloc_object_with(ctx, &SURFACE_METHODS, FAKE_SURFACE, &record.private_words())?;
    if out != 0 {
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(if object != 0 {
        0
    } else {
        0x8000_4005
    }))
}

/// Read the `DDSURFACEDESC` a game passed to `CreateSurface` and give the
/// surface storage of its own.
///
/// Handing every surface the panel's own mapping made `Blt` a no-op that
/// happened to look right: a game drawing into its back buffer was really
/// drawing on the screen. Tower Bloxx shows why that is not enough — it
/// clears the back buffer with a `DDBLT_COLORFILL` every frame and only
/// then draws, so with one shared buffer the clear wiped the picture and
/// the present put nothing back, and the frame counter stopped at 1.
fn surface_from_desc(ctx: &mut CallCtx<'_>, desc: u32) -> SurfaceRecord {
    let panel = panel_record(ctx);
    if desc == 0 {
        return panel;
    }
    let word = |ctx: &mut CallCtx<'_>, offset: u32| ctx.cpu.read_u32_le(desc + offset).unwrap_or(0);
    if word(ctx, 0) != DDSURFACEDESC_SIZE {
        return panel;
    }
    let flags = word(ctx, 4);
    let caps = if flags & 0x0000_0001 != 0 {
        word(ctx, 100)
    } else {
        0
    };
    if caps & 0x0000_0040 != 0 {
        // DDSCAPS_PRIMARYSURFACE
        return panel;
    }
    let width = if flags & 0x0000_0004 != 0 {
        word(ctx, 12)
    } else {
        panel.width
    };
    let height = if flags & 0x0000_0002 != 0 {
        word(ctx, 8)
    } else {
        panel.height
    };
    if width == 0 || height == 0 {
        return panel;
    }
    let pitch = width * 2;
    match ctx.kernel.heap.alloc(pitch * height) {
        Some(pixels) if pixels != 0 => SurfaceRecord {
            pixels,
            width,
            height,
            pitch,
            primary: false,
        },
        // Out of heap: aliasing the panel is worse than a private
        // buffer but better than a surface with nowhere to draw.
        _ => panel,
    }
}

fn ddraw_create_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let desc = ctx.arg_u32(1)?;
    let out = ctx.arg_u32(2)?;
    let record = surface_from_desc(ctx, desc);
    let outcome = create_surface_described(ctx, out, record)?;
    // A game that asked for a specific off-screen size expects the
    // descriptor it passed in to come back filled with the pitch and the
    // surface pointer it will draw through.
    if desc != 0 {
        let size = ctx.cpu.read_u32_le(desc).unwrap_or(0);
        if size == DDSURFACEDESC_SIZE {
            write_surface_desc(ctx, desc, SYNTHETIC_FRAMEBUFFER_BASE)?;
        }
    }
    Ok(outcome)
}

/// Slot 8 is `FlipToGDISurface`, which takes no arguments at all.
///
/// A cabinet built against a vtable we have not seen still occasionally
/// aims its `CreateSurface` here; a descriptor-shaped `r1` and an
/// out-pointer in `r2` are not something the real method is ever called
/// with, so treating that shape as a surface creation costs nothing and
/// keeps those titles booting.
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
        log::debug!("DirectDraw slot 8 used as CreateSurface(desc=0x{desc:08x}, out=0x{out:08x})");
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

fn ddraw_get_vertical_blank_status(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
    }
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

fn ddraw_get_gdi_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
    create_surface_at(ctx, out)
}

fn ddraw_get_surface_from_dc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(2)?;
    create_surface_at(ctx, out)
}

/// `GetAvailableVidMem(LPDDSCAPS, LPDWORD total, LPDWORD free)`.
///
/// Zero free bytes is a real answer on a real device, and a game that
/// believes it reports out of memory instead of allocating a surface, so
/// report the panel's own size as available.
fn ddraw_get_available_vid_mem(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let total = ctx.arg_u32(2)?;
    let free = ctx.arg_u32(3)?;
    let bytes = ctx.kernel.framebuffer.byte_size().saturating_mul(4);
    if total != 0 {
        ctx.cpu.write_mem(total, &bytes.to_le_bytes())?;
    }
    if free != 0 {
        ctx.cpu.write_mem(free, &bytes.to_le_bytes())?;
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

fn surface_status(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `DDPIXELFORMAT` for the emulated RGB565 panel, as CE lays it out: an
/// eight-DWORD structure with the alpha mask last.
fn pixel_format_bytes() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[0..4].copy_from_slice(&32u32.to_le_bytes()); // dwSize
    bytes[4..8].copy_from_slice(&0x40u32.to_le_bytes()); // dwFlags = DDPF_RGB
    bytes[12..16].copy_from_slice(&16u32.to_le_bytes()); // dwRGBBitCount
    bytes[16..20].copy_from_slice(&0xf800u32.to_le_bytes()); // dwRBitMask
    bytes[20..24].copy_from_slice(&0x07e0u32.to_le_bytes()); // dwGBitMask
    bytes[24..28].copy_from_slice(&0x001fu32.to_le_bytes()); // dwBBitMask
    bytes
}

/// `DDSURFACEDESC` for the emulated panel, in the Windows CE layout.
///
/// CE's struct is 108 bytes and carries an `lXPitch` (bytes to the next
/// pixel to the right) that the desktop struct does not have, which moves
/// `lpSurface` to offset 32 and the pixel format to offset 68. Writing
/// the desktop offsets here put the surface pointer four bytes past where
/// the guest reads it, and the pixel format four bytes past that: Tower
/// Bloxx locked the primary surface, read a NULL `lpSurface` and drew
/// nothing for the whole run.
fn surface_desc_bytes(width: u32, height: u32, pitch: u32, surface: u32) -> [u8; 108] {
    let mut bytes = [0u8; 108];
    let flags = 0x0000_0001 // DDSD_CAPS
        | 0x0000_0002 // DDSD_HEIGHT
        | 0x0000_0004 // DDSD_WIDTH
        | 0x0000_0008 // DDSD_PITCH
        | 0x0000_0010 // DDSD_XPITCH
        | 0x0000_0800 // DDSD_LPSURFACE
        | 0x0000_1000 // DDSD_PIXELFORMAT
        | 0x0008_0000u32; // DDSD_SURFACESIZE
    bytes[0..4].copy_from_slice(&DDSURFACEDESC_SIZE.to_le_bytes());
    bytes[4..8].copy_from_slice(&flags.to_le_bytes());
    bytes[8..12].copy_from_slice(&height.to_le_bytes());
    bytes[12..16].copy_from_slice(&width.to_le_bytes());
    bytes[16..20].copy_from_slice(&pitch.to_le_bytes()); // lPitch
    bytes[20..24].copy_from_slice(&2u32.to_le_bytes()); // lXPitch
    bytes[32..36].copy_from_slice(&surface.to_le_bytes()); // lpSurface
    bytes[68..100].copy_from_slice(&pixel_format_bytes()); // ddpfPixelFormat
    bytes[100..104].copy_from_slice(&0x40u32.to_le_bytes()); // DDSCAPS_PRIMARYSURFACE
    bytes[104..108].copy_from_slice(&(pitch.saturating_mul(height)).to_le_bytes());
    bytes
}

fn write_surface_desc(ctx: &mut CallCtx<'_>, desc: u32, surface: u32) -> Result<(), KernelError> {
    let record = panel_record(ctx);
    write_record_desc(
        ctx,
        desc,
        SurfaceRecord {
            pixels: surface,
            ..record
        },
    )
}

fn write_record_desc(
    ctx: &mut CallCtx<'_>,
    desc: u32,
    record: SurfaceRecord,
) -> Result<(), KernelError> {
    if desc == 0 {
        return Ok(());
    }
    let bytes = surface_desc_bytes(record.width, record.height, record.pitch, record.pixels);
    ctx.cpu.write_mem(desc, &bytes)?;
    Ok(())
}

/// `GetColorKey(DWORD dwFlags, LPDDCOLORKEY lpDDColorKey)` — the key is
/// the *second* argument, so it lands in `r2`.
fn surface_get_color_key(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let color_key = ctx.arg_u32(2)?;
    if color_key != 0 {
        ctx.cpu.write_mem(color_key, &[0; 8])?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_get_pixel_format(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
    if out != 0 {
        let bytes = pixel_format_bytes();
        ctx.cpu.write_mem(out, &bytes)?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_desc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let desc = ctx.arg_u32(1)?;
    let record = this_surface(ctx)?.unwrap_or_else(|| panel_record(ctx));
    if desc != 0 {
        write_record_desc(ctx, desc, record)?;
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
    let record = this_surface(ctx)?.unwrap_or_else(|| panel_record(ctx));
    if desc != 0 {
        write_record_desc(ctx, desc, record)?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// Read the guest's writes back out of the mapping and publish them.
///
/// This is one of the two presentation points for a DirectDraw title, so
/// it is also where `frame_counter` moves — and only when the pixels
/// actually changed, per invariant 10.
fn publish_framebuffer(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    ensure_framebuffer(ctx)?;
    let mut pixels = vec![0u8; ctx.kernel.framebuffer.pixels.len()];
    ctx.cpu
        .read_mem_into(SYNTHETIC_FRAMEBUFFER_BASE, &mut pixels)?;
    if pixels != ctx.kernel.framebuffer.pixels {
        ctx.kernel.framebuffer.pixels.copy_from_slice(&pixels);
        ctx.kernel.framebuffer.mark_dirty();
    }
    Ok(())
}

fn surface_unlock(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if this_surface(ctx)?.is_none_or(|record| record.primary) {
        publish_framebuffer(ctx)?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `Blt(LPRECT dst, LPDIRECTDRAWSURFACE src, LPRECT srcRect, DWORD flags,
/// LPDDBLTFX fx)` — the last two arrive on the stack.
///
/// Two shapes matter. `DDBLT_COLORFILL` with a NULL source clears a
/// rectangle to `fx->dwFillColor`, which is how a game starts its frame;
/// everything else is a surface-to-surface copy, which is how it ends
/// one. Scaling is not implemented: the copy takes the overlap of the
/// two rectangles, which is what a CE game asking for a straight present
/// gets anyway.
fn surface_blt(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dest_rect = ctx.arg_u32(1)?;
    let source = ctx.arg_u32(2)?;
    let source_rect = ctx.arg_u32(3)?;
    let flags = ctx.arg_u32(4)?;
    let fx = ctx.arg_u32(5)?;
    let Some(dest) = this_surface(ctx)? else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let dest_area = read_rect(ctx, dest_rect, dest);
    if flags & 0x0000_0400 != 0 {
        // DDBLT_COLORFILL. dwFillColor is the third DWORD of CE's
        // DDBLTFX, after dwSize and dwROP.
        let colour = if fx != 0 {
            ctx.cpu.read_u32_le(fx + 8).unwrap_or(0) as u16
        } else {
            0
        };
        fill_rect(ctx, dest, dest_area, colour)?;
    } else if let Some(src) = surface_record(ctx, source) {
        let source_area = read_rect(ctx, source_rect, src);
        copy_rect(ctx, src, source_area, dest, dest_area)?;
    }
    if dest.primary {
        publish_framebuffer(ctx)?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// A `RECT` clamped to a surface, or the whole surface when NULL.
fn read_rect(ctx: &mut CallCtx<'_>, rect: u32, surface: SurfaceRecord) -> (u32, u32, u32, u32) {
    let whole = (0, 0, surface.width, surface.height);
    if rect == 0 {
        return whole;
    }
    let mut bytes = [0u8; 16];
    if ctx.cpu.read_mem_into(rect, &mut bytes).is_err() {
        return whole;
    }
    let word = |i: usize| i32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
    let left = word(0).clamp(0, surface.width as i32) as u32;
    let top = word(1).clamp(0, surface.height as i32) as u32;
    let right = word(2).clamp(left as i32, surface.width as i32) as u32;
    let bottom = word(3).clamp(top as i32, surface.height as i32) as u32;
    (left, top, right, bottom)
}

fn fill_rect(
    ctx: &mut CallCtx<'_>,
    surface: SurfaceRecord,
    (left, top, right, bottom): (u32, u32, u32, u32),
    colour: u16,
) -> Result<(), KernelError> {
    if right <= left || bottom <= top {
        return Ok(());
    }
    let row = vec![colour.to_le_bytes(); (right - left) as usize].concat();
    for y in top..bottom {
        ctx.cpu
            .write_mem(surface.pixels + y * surface.pitch + left * 2, &row)?;
    }
    Ok(())
}

fn copy_rect(
    ctx: &mut CallCtx<'_>,
    src: SurfaceRecord,
    (sl, st, sr, sb): (u32, u32, u32, u32),
    dest: SurfaceRecord,
    (dl, dt, dr, db): (u32, u32, u32, u32),
) -> Result<(), KernelError> {
    if src.pixels == dest.pixels && (sl, st) == (dl, dt) {
        return Ok(());
    }
    let width = (sr.saturating_sub(sl)).min(dr.saturating_sub(dl));
    let height = (sb.saturating_sub(st)).min(db.saturating_sub(dt));
    if width == 0 || height == 0 {
        return Ok(());
    }
    let mut row = vec![0u8; width as usize * 2];
    for y in 0..height {
        ctx.cpu
            .read_mem_into(src.pixels + (st + y) * src.pitch + sl * 2, &mut row)?;
        ctx.cpu
            .write_mem(dest.pixels + (dt + y) * dest.pitch + dl * 2, &row)?;
    }
    Ok(())
}

/// A game that double-buffers presents with `Flip` rather than by
/// unlocking, and every surface we hand out aliases the one synthetic
/// framebuffer, so the flip is already done — it just has to be
/// published. Without this a flipping title draws into the mapping and
/// never announces a frame.
fn surface_flip(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if let (Some(dest), Some(src)) = (this_surface(ctx)?, {
        let other = ctx.arg_u32(1)?;
        surface_record(ctx, other)
    }) {
        let area = (0, 0, dest.width, dest.height);
        copy_rect(ctx, src, (0, 0, src.width, src.height), dest, area)?;
    }
    publish_framebuffer(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

#[cfg(test)]
mod tests {
    use super::{
        pixel_format_bytes, surface_desc_bytes, CLIPPER_METHODS, DDRAW_METHODS, PALETTE_METHODS,
        SURFACE_METHODS,
    };

    fn slot(table: &[&str], name: &str) -> usize {
        table.iter().position(|entry| *entry == name).unwrap()
    }

    /// Windows CE's `ddraw.h`, not the desktop one. Tower Bloxx calls
    /// every one of these by slot; the desktop order sends them
    /// elsewhere and the game faults before its first frame.
    #[test]
    fn the_vtables_follow_the_windows_ce_header() {
        assert_eq!(slot(&DDRAW_METHODS, "ddraw_create_clipper"), 3);
        assert_eq!(slot(&DDRAW_METHODS, "ddraw_create_surface"), 5);
        assert_eq!(slot(&DDRAW_METHODS, "ddraw_set_cooperative_level"), 17);
        assert_eq!(slot(&CLIPPER_METHODS, "clipper_set_hwnd"), 7);
        assert_eq!(slot(&SURFACE_METHODS, "surface_lock"), 19);
        assert_eq!(slot(&SURFACE_METHODS, "surface_unlock"), 26);
        assert_eq!(slot(&SURFACE_METHODS, "surface_blt"), 4);
        assert_eq!(slot(&PALETTE_METHODS, "palette_set_entries"), 5);
        // CE has no Compact, DuplicateSurface or Initialize anywhere.
        for table in [
            DDRAW_METHODS.as_slice(),
            PALETTE_METHODS.as_slice(),
            CLIPPER_METHODS.as_slice(),
            SURFACE_METHODS.as_slice(),
        ] {
            assert!(!table.iter().any(|name| name.ends_with("_initialize")));
        }
        assert!(!DDRAW_METHODS.contains(&"ddraw_compact"));
        assert!(!DDRAW_METHODS.contains(&"ddraw_duplicate_surface"));
    }

    #[test]
    fn the_surface_descriptor_uses_the_windows_ce_field_offsets() {
        let bytes = surface_desc_bytes(240, 320, 480, 0x7800_0000);
        let word =
            |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!(word(0), 108, "CE sizeof(DDSURFACEDESC)");
        assert_eq!(word(8), 320, "dwHeight");
        assert_eq!(word(12), 240, "dwWidth");
        assert_eq!(word(16), 480, "lPitch");
        assert_eq!(word(20), 2, "lXPitch — CE only, and what shifts lpSurface");
        assert_eq!(word(32), 0x7800_0000, "lpSurface");
        assert_eq!(word(68), 32, "ddpfPixelFormat.dwSize");
        assert_eq!(word(72), 0x40, "DDPF_RGB");
        assert_eq!(word(80), 16, "dwRGBBitCount");
        assert_eq!(word(84), 0xf800);
        assert_eq!(word(88), 0x07e0);
        assert_eq!(word(92), 0x001f);
        assert_eq!(word(104), 480 * 320, "dwSurfaceSize");
    }

    #[test]
    fn the_pixel_format_describes_rgb565() {
        let bytes = pixel_format_bytes();
        let word =
            |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!(word(0), 32);
        assert_eq!(word(4), 0x40);
        assert_eq!(word(12), 16);
        assert_eq!((word(16), word(20), word(24)), (0xf800, 0x07e0, 0x001f));
    }
}
