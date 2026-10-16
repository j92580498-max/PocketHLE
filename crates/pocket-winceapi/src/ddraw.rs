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

// Older CE DXPAK applications (including SkyStriker) explicitly query
// IDirectDraw4. Its retained slots must not be collapsed into the newer
// Windows Mobile IDirectDraw table above: SetCooperativeLevel is slot 20,
// not 17, and CreateSurface is slot 6, not 5.
const IID_DDRAW4: [u8; 16] = [
    0x9a, 0x50, 0x59, 0x9c, 0xbd, 0x39, 0xd1, 0x11, 0x8c, 0x4a, 0x00, 0xc0, 0x4f, 0xd9, 0x30, 0xc5,
];
const IID_SURFACE4: [u8; 16] = [
    0x30, 0x86, 0x2b, 0x0b, 0x35, 0xad, 0xd0, 0x11, 0x8e, 0xa6, 0x00, 0x60, 0x97, 0x97, 0xea, 0x5b,
];
const IID_SURFACE_CE: [u8; 16] = [
    0xe4, 0x83, 0x0e, 0x0b, 0x7f, 0xf3, 0xd2, 0x11, 0x8b, 0x15, 0x00, 0xc0, 0x4f, 0x68, 0x92, 0x92,
];
const DDRAW4_METHODS: [&str; 28] = [
    "ddraw_qi",
    "ddraw_add_ref",
    "ddraw_release",
    "ddraw_compact",
    "ddraw_create_clipper",
    "ddraw_create_palette",
    "ddraw4_create_surface",
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
    "ddraw_get_available_vid_mem",
    "ddraw_get_surface_from_dc",
    "ddraw_restore_all_surfaces",
    "ddraw_test_cooperative_level",
    "ddraw_get_device_identifier",
];
const SURFACE4_METHODS: [&str; 45] = [
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
    "surface_set_private_data",
    "surface_get_private_data",
    "surface_free_private_data",
    "surface_get_uniqueness_value",
    "surface_change_uniqueness_value",
];

fn requested_iid(ctx: &mut CallCtx<'_>) -> Result<[u8; 16], KernelError> {
    let ptr = ctx.arg_u32(1)?;
    let mut iid = [0u8; 16];
    ctx.cpu.read_mem_into(ptr, &mut iid)?;
    Ok(iid)
}

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

// The CE Surface5 IID is used by both the compact Mobile header and
// the older DXPAK header (Surface4 slots plus AlphaBlt). Keep the compact
// view until its first unambiguous legacy Lock selects the retained slots.
const SURFACE5_METHODS: [&str; 46] = {
    let mut names = ["surface_alpha_blt"; 46];
    let mut i = 0;
    while i < SURFACE4_METHODS.len() {
        names[i] = SURFACE4_METHODS[i];
        i += 1;
    }
    names
};
const SURFACE_COMPAT_METHODS: [&str; 46] = {
    let mut names = SURFACE5_METHODS;
    let mut i = 0;
    while i < SURFACE_METHODS.len() {
        names[i] = SURFACE_METHODS[i];
        i += 1;
    }
    names[25] = "surface_ce_palette_or_legacy_lock";
    names[7] = "surface_ce_flip_or_legacy_blt_fast";
    names
};

pub fn register(d: &mut WinCeDispatcher) {
    d.register_handler("ddraw.dll", "DirectDrawCreate", direct_draw_create);
    d.register_handler("coredll.dll", "DirectDrawCreate", direct_draw_create);
    for name in DDRAW_METHODS
        .iter()
        .chain(DDRAW4_METHODS.iter())
        .chain(SURFACE4_METHODS.iter())
        .chain(PALETTE_METHODS.iter())
        .chain(CLIPPER_METHODS.iter())
        .chain(SURFACE_METHODS.iter())
        .chain(SURFACE_COMPAT_METHODS.iter())
    {
        let handler = match *name {
            "ddraw_qi" => ddraw_qi,
            "ddraw_add_ref" => add_ref,
            "ddraw_release" => release,
            "ddraw_create_surface" => ddraw_create_surface,
            "ddraw4_create_surface" => ddraw4_create_surface,
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
            "ddraw_wait_for_vertical_blank" => ddraw_wait_for_vertical_blank,
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
            "surface_blt_fast" => surface_blt_fast,
            "surface_ce_flip_or_legacy_blt_fast" => surface_ce_flip_or_legacy_blt_fast,
            "surface_get_blt_status" | "surface_get_flip_status" => surface_status,
            "surface_get_pixel_format" => surface_get_pixel_format,
            "surface_get_color_key" => surface_get_color_key,
            "surface_get_surface_desc" => surface_desc,
            "surface_get_dc" => surface_get_dc,
            "surface_is_lost" => surface_is_lost,
            "surface_lock" => surface_lock,
            "surface_ce_palette_or_legacy_lock" => surface_ce_palette_or_legacy_lock,
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
        if table != 0 {
            ctx.kernel.heap.free(table);
        }
        if object != 0 {
            ctx.kernel.heap.free(object);
        }
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
        let object = if requested_iid(ctx)? == IID_DDRAW4 {
            alloc_object(ctx, &DDRAW4_METHODS, FAKE_DDRAW)?
        } else {
            ctx.arg_u32(0)?
        };
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
        if object == 0 {
            return Ok(DispatchOutcome::ReturnedR0(0x8000_000e));
        }
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
    if object == 0 && !record.primary {
        ctx.kernel.heap.free(record.pixels);
    }
    if out != 0 {
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(if object != 0 {
        0
    } else {
        0x8007_000e
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
fn surface_from_desc(ctx: &mut CallCtx<'_>, desc: u32) -> Result<SurfaceRecord, u32> {
    let panel = panel_record(ctx);
    if desc == 0 {
        return Ok(panel);
    }
    let word = |ctx: &mut CallCtx<'_>, offset: u32| ctx.cpu.read_u32_le(desc + offset).unwrap_or(0);
    let size = word(ctx, 0);
    if !matches!(size, DDSURFACEDESC_SIZE | 124) {
        return Ok(panel);
    }
    let flags = word(ctx, 4);
    let caps = if flags & 0x0000_0001 != 0 {
        word(ctx, if size == 124 { 104 } else { 100 })
    } else {
        0
    };
    let primary_cap = if size == 124 {
        0x0000_0200
    } else {
        0x0000_0040
    };
    if caps & primary_cap != 0 {
        // DDSCAPS_PRIMARYSURFACE
        return Ok(panel);
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
        return Ok(panel);
    }
    let pitch = width.checked_mul(2).ok_or(0x8007_0057u32)?;
    let bytes = pitch.checked_mul(height).ok_or(0x8007_0057u32)?;
    match ctx.kernel.heap.alloc(bytes) {
        Some(pixels) if pixels != 0 => Ok(SurfaceRecord {
            pixels,
            width,
            height,
            pitch,
            primary: false,
        }),
        _ => Err(0x8007_000e), // E_OUTOFMEMORY: never alias an off-screen buffer.
    }
}

fn ddraw_create_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let desc = ctx.arg_u32(1)?;
    let out = ctx.arg_u32(2)?;
    let record = match surface_from_desc(ctx, desc) {
        Ok(record) => record,
        Err(error) => {
            if out != 0 {
                ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
            }
            return Ok(DispatchOutcome::ReturnedR0(error));
        }
    };
    let outcome = create_surface_described(ctx, out, record)?;
    // A game that asked for a specific off-screen size expects the
    // descriptor it passed in to come back filled with the pitch and the
    // surface pointer it will draw through.
    if desc != 0 {
        let size = ctx.cpu.read_u32_le(desc).unwrap_or(0);
        if matches!(size, DDSURFACEDESC_SIZE | 124) {
            write_record_desc(ctx, desc, record)?;
        }
    }
    Ok(outcome)
}

fn ddraw4_create_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let desc = ctx.arg_u32(1)?;
    let out = ctx.arg_u32(2)?;
    ensure_framebuffer(ctx)?;
    let record = match surface_from_desc(ctx, desc) {
        Ok(record) => record,
        Err(error) => {
            if out != 0 {
                ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
            }
            return Ok(DispatchOutcome::ReturnedR0(error));
        }
    };
    let object = alloc_object_with(
        ctx,
        &SURFACE4_METHODS,
        FAKE_SURFACE,
        &record.private_words(),
    )?;
    if out != 0 {
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    if object == 0 && !record.primary {
        ctx.kernel.heap.free(record.pixels);
    }
    Ok(DispatchOutcome::ReturnedR0(if object == 0 {
        0x8007_000e
    } else {
        0
    }))
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

/// Keep the same deadline when the scheduler re-enters this thunk.
pub(crate) fn wait_display(
    ctx: &mut CallCtx<'_>,
    blank: Option<bool>,
) -> Result<Option<DispatchOutcome>, KernelError> {
    let key = (
        ctx.kernel.current_thread,
        ctx.thunk.thunk_va,
        ctx.cpu.read_reg(pocket_cpu::regs::ArmReg::Sp)?,
    );
    let now = std::time::Instant::now();
    let clock = &mut ctx.kernel.framebuffer.directdraw;
    let deadline = if let Some(deadline) = clock.pending.get(&key) {
        *deadline
    } else {
        let deadline = match blank {
            Some(end) => clock.blank_deadline(now, end),
            None => {
                if clock
                    .recent_blank
                    .get(&ctx.kernel.current_thread)
                    .is_some_and(|last| {
                        now.saturating_duration_since(*last)
                            < pocket_kernel::framebuffer::DirectDrawTiming::PERIOD
                    })
                {
                    now
                } else {
                    clock.present_deadline(now)
                }
            }
        };
        clock.pending.insert(key, deadline);
        deadline
    };
    if let Some(outcome) = crate::coredll::wait_display_until(ctx, deadline)? {
        return Ok(Some(outcome));
    }
    ctx.kernel.framebuffer.directdraw.pending.remove(&key);
    if blank.is_none() {
        ctx.kernel
            .framebuffer
            .directdraw
            .recent_blank
            .remove(&ctx.kernel.current_thread);
    }
    Ok(None)
}

fn ddraw_wait_for_vertical_blank(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let flags = ctx.arg_u32(1)?;
    if flags != 1 && flags != 4 {
        return Ok(DispatchOutcome::ReturnedR0(0x8007_0057));
    }
    if let Some(outcome) = wait_display(ctx, Some(flags == 4))? {
        return Ok(outcome);
    }
    ctx.kernel
        .framebuffer
        .directdraw
        .recent_blank
        .insert(ctx.kernel.current_thread, std::time::Instant::now());
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ddraw_get_vertical_blank_status(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
    if out != 0 {
        let blank = u32::from(
            ctx.kernel
                .framebuffer
                .directdraw
                .in_blank(std::time::Instant::now()),
        );
        ctx.cpu.write_mem(out, &blank.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn ddraw_get_scan_line(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
    let clock = &ctx.kernel.framebuffer.directdraw;
    let phase = clock.epoch.elapsed().as_nanos()
        % pocket_kernel::framebuffer::DirectDrawTiming::PERIOD.as_nanos();
    let line = (phase * u128::from(ctx.kernel.framebuffer.height)
        / pocket_kernel::framebuffer::DirectDrawTiming::PERIOD.as_nanos()) as u32;
    if out != 0 {
        ctx.cpu.write_mem(out, &line.to_le_bytes())?;
    }
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
    let window = ctx.arg_u32(1)?;
    // Slot 20 in the retained IDirectDraw header is SetCooperativeLevel.
    // Its HWND and flags are not output pointers for GetAvailableVidMem.
    if total != 0 && total & !0x1ff == 0 && ctx.kernel.window_procs.contains_key(&window) {
        return ddraw_set_cooperative_level(ctx);
    }
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
        let this = ctx.arg_u32(0)?;
        let iid = requested_iid(ctx)?;
        let table = if iid == IID_SURFACE_CE {
            let retained = ctx
                .cpu
                .read_u32_le(this)
                .ok()
                .and_then(|table| ctx.cpu.read_u32_le(table + 25 * 4).ok())
                .is_some_and(|address| {
                    address != 0 && address == dynamic_address(ctx, "surface_lock")
                });
            Some(if retained {
                SURFACE_COMPAT_METHODS.as_slice()
            } else {
                SURFACE_METHODS.as_slice()
            })
        } else if iid == IID_SURFACE4 {
            Some(SURFACE4_METHODS.as_slice())
        } else {
            None
        };
        let object = if let (Some(table), Some(record)) = (table, surface_record(ctx, this)) {
            // Interface views share pixel storage and geometry, not a fresh
            // framebuffer. SkyStriker switches Surface4 to the CE interface.
            alloc_object_with(ctx, table, FAKE_SURFACE, &record.private_words())?
        } else {
            this
        };
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
        if object == 0 {
            return Ok(DispatchOutcome::ReturnedR0(0x8000_000e));
        }
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

/// The retained DirectDraw4 ABI uses DDSURFACEDESC2 (124 bytes).
/// Its pointer/pixel format/caps offsets differ from the 108-byte CE struct.
fn surface_desc2_bytes(
    width: u32,
    height: u32,
    pitch: u32,
    surface: u32,
    primary: bool,
) -> [u8; 124] {
    let mut bytes = [0u8; 124];
    let flags = 0x1u32 | 0x2 | 0x4 | 0x8 | 0x800 | 0x1000;
    for (offset, value) in [
        (0, 124u32),
        (4, flags),
        (8, height),
        (12, width),
        (16, pitch),
        (36, surface),
        (104, if primary { 0x200 } else { 0x40 }),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes[72..104].copy_from_slice(&pixel_format_bytes());
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
    let size = ctx.cpu.read_u32_le(desc)?;
    if size == 124 {
        let bytes = surface_desc2_bytes(
            record.width,
            record.height,
            record.pitch,
            record.pixels,
            record.primary,
        );
        ctx.cpu.write_mem(desc, &bytes)?;
    } else {
        let bytes = surface_desc_bytes(record.width, record.height, record.pitch, record.pixels);
        ctx.cpu.write_mem(desc, &bytes)?;
    }
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
    let object = ctx.arg_u32(0)?;
    let mut retained = false;
    // A compact slot-19 Lock resolves the ambiguous view in the other
    // direction. Later SetPalette calls must not reclassify it from stale args.
    if let Ok(table) = ctx.cpu.read_u32_le(object) {
        let ambiguous = dynamic_address(ctx, "surface_ce_palette_or_legacy_lock");
        if ambiguous != 0 && ctx.cpu.read_u32_le(table + 25 * 4).ok() == Some(ambiguous) {
            write_vtable(ctx, table, &SURFACE_METHODS)?;
        }
        let lock = dynamic_address(ctx, "surface_lock");
        retained = lock != 0 && ctx.cpu.read_u32_le(table + 25 * 4).ok() == Some(lock);
    }
    let desc = ctx.arg_u32(2)?;
    let record = this_surface(ctx)?.unwrap_or_else(|| panel_record(ctx));
    if desc != 0 {
        if retained {
            // The retained interface returns DDSURFACEDESC2. Some DXPAK
            // callers pass an uninitialized output structure; its contents
            // must not select the compact header's different field offsets.
            let bytes = surface_desc2_bytes(
                record.width,
                record.height,
                record.pitch,
                record.pixels,
                record.primary,
            );
            ctx.cpu.write_mem(desc, &bytes)?;
        } else {
            write_record_desc(ctx, desc, record)?;
        }
    }
    if record.primary {
        ctx.kernel
            .framebuffer
            .directdraw
            .primary_locks
            .insert(record.pixels);
        // Full-frame writers use Unlock as their presentation boundary.
        // Readback and partial locks must not throttle every read or sprite.
        if ctx.arg_u32(1)? == 0 && ctx.arg_u32(3)? & 0x10 == 0 {
            ctx.kernel
                .framebuffer
                .directdraw
                .paced_primary_locks
                .insert(record.pixels);
        }
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_ce_palette_or_legacy_lock(
    ctx: &mut CallCtx<'_>,
) -> Result<DispatchOutcome, KernelError> {
    let desc = ctx.arg_u32(2)?;
    let flags = ctx.arg_u32(3)?;
    let object = ctx.arg_u32(0)?;
    // SetPalette has no descriptor argument. A writable DDSURFACEDESC2
    // and supported lock flags identify the older header's slot 25.
    let initialized = ctx.cpu.read_u32_le(desc).ok() == Some(124);
    let write_only_output =
        !initialized && ctx.arg_u32(1)? == 0 && flags & 0x20 != 0 && ctx.arg_u32(4)? == 0;
    if flags & !0x1fff == 0
        && (initialized || write_only_output)
        && ctx.cpu.check_guest_access(desc, 124, Prot::WRITE).is_ok()
        && surface_record(ctx, object).is_some()
    {
        let table = ctx.cpu.read_u32_le(object)?;
        write_vtable(ctx, table, &SURFACE5_METHODS)?;
        return surface_lock(ctx);
    }
    surface_ok(ctx)
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
        ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
        ctx.kernel.direct_fb_frames = ctx.kernel.direct_fb_frames.saturating_add(1);
    }
    Ok(())
}

fn surface_unlock(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if this_surface(ctx)?.is_none_or(|record| record.primary) {
        let paced = ctx
            .kernel
            .framebuffer
            .directdraw
            .paced_primary_locks
            .contains(&SYNTHETIC_FRAMEBUFFER_BASE);
        if paced {
            if let Some(outcome) = wait_display(ctx, None)? {
                return Ok(outcome);
            }
        }
        publish_framebuffer(ctx)?;
        let clock = &mut ctx.kernel.framebuffer.directdraw;
        clock.primary_locks.remove(&SYNTHETIC_FRAMEBUFFER_BASE);
        clock
            .paced_primary_locks
            .remove(&SYNTHETIC_FRAMEBUFFER_BASE);
        if paced {
            clock.last_present = Some(std::time::Instant::now());
        }
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

/// Copy an explicit back buffer and publish on the modeled display cadence.
/// DDFLIP_NOVSYNC allows a caller to opt out of the presentation wait.
fn surface_flip(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.arg_u32(2)? & 8 == 0 {
        if let Some(outcome) = wait_display(ctx, None)? {
            return Ok(outcome);
        }
    }
    if let (Some(dest), Some(src)) = (this_surface(ctx)?, {
        let other = ctx.arg_u32(1)?;
        surface_record(ctx, other)
    }) {
        let area = (0, 0, dest.width, dest.height);
        copy_rect(ctx, src, (0, 0, src.width, src.height), dest, area)?;
    }
    publish_framebuffer(ctx)?;
    ctx.kernel.framebuffer.directdraw.last_present = Some(std::time::Instant::now());
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn surface_ce_flip_or_legacy_blt_fast(
    ctx: &mut CallCtx<'_>,
) -> Result<DispatchOutcome, KernelError> {
    let source = ctx.arg_u32(3)?;
    let dest = this_surface(ctx)?;
    let x = ctx.arg_u32(1)?;
    let y = ctx.arg_u32(2)?;
    if surface_record(ctx, source).is_some()
        && dest.is_some_and(|d| x < d.width && y < d.height)
        && ctx.arg_u32(5)? & !0xf == 0
    {
        let object = ctx.arg_u32(0)?;
        let table = ctx.cpu.read_u32_le(object)?;
        write_vtable(ctx, table, &SURFACE5_METHODS)?;
        return surface_blt_fast(ctx);
    }
    surface_flip(ctx)
}

/// Retained BltFast uses DWORD x/y, source, source RECT and flags. Its
/// slot 7 is the compact header's Flip; preserve the argument layouts.
fn surface_blt_fast(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let Some(dest) = this_surface(ctx)? else {
        return Ok(DispatchOutcome::ReturnedR0(0x8007_0057));
    };
    let source = ctx.arg_u32(3)?;
    let Some(src) = surface_record(ctx, source) else {
        return Ok(DispatchOutcome::ReturnedR0(0x8007_0057));
    };
    let x = ctx.arg_u32(1)?;
    let y = ctx.arg_u32(2)?;
    let rect = ctx.arg_u32(4)?;
    let area = read_rect(ctx, rect, src);
    let width = area.2 - area.0;
    let height = area.3 - area.1;
    if x < dest.width && y < dest.height {
        let target = (
            x,
            y,
            x.saturating_add(width).min(dest.width),
            y.saturating_add(height).min(dest.height),
        );
        copy_rect(ctx, src, area, dest, target)?;
        if dest.primary {
            publish_framebuffer(ctx)?;
        }
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

#[cfg(test)]
mod tests {
    use super::{
        pixel_format_bytes, surface_desc_bytes, CLIPPER_METHODS, DDRAW_METHODS, PALETTE_METHODS,
        SURFACE_METHODS,
    };

    #[test]
    fn vertical_blank_retries_preserve_arguments_and_really_wait() {
        use super::*;
        use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu};
        use pocket_kernel::Thunk;
        use pocket_pe::ImportBinding;
        let mut cpu = StubCpu::new();
        let mut kernel = crate::gx::tests::fresh_kernel();
        let thunk = Thunk {
            thunk_va: 0x70001000,
            iat_va: 0,
            dll: "ddraw.dll".into(),
            binding: ImportBinding::Name("ddraw_wait_for_vertical_blank".into()),
            friendly_name: None,
        };
        cpu.write_reg(ArmReg::R0, FAKE_DDRAW).unwrap();
        cpu.write_reg(ArmReg::R1, 1).unwrap();
        let mut ctx = CallCtx {
            cpu: &mut cpu,
            kernel: &mut kernel,
            thunk: &thunk,
        };
        let started = std::time::Instant::now();
        for _ in 0..4 {
            loop {
                match ddraw_wait_for_vertical_blank(&mut ctx).unwrap() {
                    DispatchOutcome::JumpTo(pc) => {
                        assert_eq!(pc, thunk.thunk_va);
                        assert_eq!(ctx.cpu.read_reg(ArmReg::R0).unwrap(), FAKE_DDRAW);
                        assert_eq!(ctx.cpu.read_reg(ArmReg::R1).unwrap(), 1);
                        assert_eq!(ctx.kernel.framebuffer.directdraw.pending.len(), 1);
                    }
                    DispatchOutcome::ReturnedR0(0) => break,
                    other => panic!("unexpected {other:?}"),
                }
            }
            assert!(ctx.kernel.framebuffer.directdraw.pending.is_empty());
        }
        assert!(started.elapsed() >= pocket_kernel::framebuffer::DirectDrawTiming::PERIOD * 3);
        // A completed explicit wait pays for the following presentation once.
        ctx.kernel.framebuffer.directdraw.last_present = Some(std::time::Instant::now());
        assert_eq!(wait_display(&mut ctx, None).unwrap(), None);
        assert!(ctx.kernel.framebuffer.directdraw.recent_blank.is_empty());
        assert!(wait_display(&mut ctx, None).unwrap().is_some());
        ctx.cpu.write_reg(ArmReg::R1, 2).unwrap();
        assert_eq!(
            ddraw_wait_for_vertical_blank(&mut ctx).unwrap(),
            DispatchOutcome::ReturnedR0(0x80070057)
        );
    }

    #[test]
    fn primary_unlock_defers_publication_until_the_display_deadline() {
        use super::*;
        use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu};
        use pocket_kernel::Thunk;
        use pocket_pe::ImportBinding;
        let mut cpu = StubCpu::new();
        let mut kernel = crate::gx::tests::fresh_kernel();
        let thunk = Thunk {
            thunk_va: 0x70002000,
            iat_va: 0,
            dll: "ddraw.dll".into(),
            binding: ImportBinding::Name("surface_unlock".into()),
            friendly_name: None,
        };
        cpu.write_reg(ArmReg::R0, FAKE_SURFACE).unwrap();
        let mut ctx = CallCtx {
            cpu: &mut cpu,
            kernel: &mut kernel,
            thunk: &thunk,
        };
        surface_lock(&mut ctx).unwrap();
        ctx.cpu
            .write_mem(SYNTHETIC_FRAMEBUFFER_BASE, &[0xff, 0xff])
            .unwrap();
        ctx.kernel.framebuffer.directdraw.last_present = Some(std::time::Instant::now());
        let before = ctx.kernel.framebuffer.frame_counter;
        assert_eq!(
            surface_unlock(&mut ctx).unwrap(),
            DispatchOutcome::JumpTo(thunk.thunk_va)
        );
        assert_eq!(ctx.kernel.framebuffer.frame_counter, before);
        assert!(!ctx.kernel.framebuffer.directdraw.primary_locks.is_empty());
        let key = (0, thunk.thunk_va, 0);
        ctx.kernel
            .framebuffer
            .directdraw
            .pending
            .insert(key, std::time::Instant::now());
        assert_eq!(
            surface_unlock(&mut ctx).unwrap(),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(ctx.kernel.framebuffer.frame_counter, before + 1);
        assert!(ctx.kernel.framebuffer.directdraw.primary_locks.is_empty());
        // Static menu presents remain paced; pixel equality is not a clock.
        surface_lock(&mut ctx).unwrap();
        assert_eq!(
            surface_unlock(&mut ctx).unwrap(),
            DispatchOutcome::JumpTo(thunk.thunk_va)
        );
        assert_eq!(ctx.kernel.framebuffer.frame_counter, before + 1);
        // Readback locks are not full-frame presentations.
        ctx.kernel.framebuffer.directdraw.pending.clear();
        ctx.kernel.framebuffer.directdraw.primary_locks.clear();
        ctx.kernel
            .framebuffer
            .directdraw
            .paced_primary_locks
            .clear();
        ctx.cpu.write_reg(ArmReg::R3, 0x10).unwrap();
        surface_lock(&mut ctx).unwrap();
        assert_eq!(
            surface_unlock(&mut ctx).unwrap(),
            DispatchOutcome::ReturnedR0(0)
        );
        ctx.cpu.write_reg(ArmReg::R3, 0).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..4 {
            surface_lock(&mut ctx).unwrap();
            loop {
                match surface_unlock(&mut ctx).unwrap() {
                    DispatchOutcome::JumpTo(pc) => assert_eq!(pc, thunk.thunk_va),
                    DispatchOutcome::ReturnedR0(0) => break,
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
        assert!(started.elapsed() >= pocket_kernel::framebuffer::DirectDrawTiming::PERIOD * 3);
        assert_eq!(ctx.kernel.framebuffer.frame_counter, before + 1);
    }

    #[test]
    fn offscreen_allocation_failure_returns_error_without_aliasing_panel() {
        use super::*;
        use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
        use pocket_kernel::Thunk;
        use pocket_pe::ImportBinding;
        let mut cpu = StubCpu::new();
        let mut kernel = crate::gx::tests::fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.write_mem(0x1000, &124u32.to_le_bytes()).unwrap();
        cpu.write_mem(0x1004, &6u32.to_le_bytes()).unwrap();
        cpu.write_mem(0x1008, &240u32.to_le_bytes()).unwrap();
        cpu.write_mem(0x100c, &320u32.to_le_bytes()).unwrap();
        let thunk = Thunk {
            thunk_va: 0,
            iat_va: 0,
            dll: "ddraw.dll".into(),
            binding: ImportBinding::Name("CreateSurface".into()),
            friendly_name: None,
        };
        for handler in [
            ddraw_create_surface as crate::Handler,
            ddraw4_create_surface,
        ] {
            cpu.write_mem(0x1100, &0x12345678u32.to_le_bytes()).unwrap();
            cpu.write_reg(ArmReg::R1, 0x1000).unwrap();
            cpu.write_reg(ArmReg::R2, 0x1100).unwrap();
            assert_eq!(
                handler(&mut CallCtx {
                    cpu: &mut cpu,
                    kernel: &mut kernel,
                    thunk: &thunk
                })
                .unwrap(),
                DispatchOutcome::ReturnedR0(0x8007_000e)
            );
            assert_eq!(cpu.read_u32_le(0x1100).unwrap(), 0);
        }
        cpu.write_mem(0x100c, &u32::MAX.to_le_bytes()).unwrap();
        assert_eq!(
            ddraw_create_surface(&mut CallCtx {
                cpu: &mut cpu,
                kernel: &mut kernel,
                thunk: &thunk
            })
            .unwrap(),
            DispatchOutcome::ReturnedR0(0x8007_0057)
        );
    }

    #[test]
    fn directdraw4_startup_keeps_ce_interfaces_and_surface_storage_distinct() {
        use super::*;
        use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
        use pocket_kernel::Thunk;
        use pocket_pe::ImportBinding;
        let mut cpu = StubCpu::new();
        let mut kernel = crate::gx::tests::fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.map_region(0x5000_0000, 0x100000, Prot::READ | Prot::WRITE)
            .unwrap();
        kernel.heap = pocket_kernel::Heap::new(0x5000_0000, 0x100000);
        let exports = kernel
            .dynamic_exports
            .entry(FAKE_MODULE_HANDLE)
            .or_default();
        let mut address = 0x7000_1000u32;
        for name in DDRAW_METHODS
            .iter()
            .chain(DDRAW4_METHODS.iter())
            .chain(SURFACE_METHODS.iter())
            .chain(SURFACE4_METHODS.iter())
            .chain(SURFACE_COMPAT_METHODS.iter())
        {
            if !exports.contains_key(*name) {
                exports.insert((*name).into(), address);
                address += 16;
            }
        }
        let t = Thunk {
            thunk_va: 0x7000_0000,
            iat_va: 0x20000,
            dll: "ddraw.dll".into(),
            binding: ImportBinding::Name("DirectDrawCreate".into()),
            friendly_name: Some("DirectDrawCreate".into()),
        };
        let call = |cpu: &mut StubCpu,
                    kernel: &mut pocket_kernel::KernelState,
                    handler: crate::Handler,
                    args: [u32; 4]| {
            for (reg, value) in [ArmReg::R0, ArmReg::R1, ArmReg::R2, ArmReg::R3]
                .into_iter()
                .zip(args)
            {
                cpu.write_reg(reg, value).unwrap();
            }
            assert_eq!(
                handler(&mut CallCtx {
                    cpu,
                    kernel,
                    thunk: &t
                })
                .unwrap(),
                DispatchOutcome::ReturnedR0(0)
            );
        };
        call(&mut cpu, &mut kernel, direct_draw_create, [0, 0x1000, 0, 0]);
        let ce = cpu.read_u32_le(0x1000).unwrap();
        let ce_table = cpu.read_u32_le(ce).unwrap();
        cpu.write_mem(0x1100, &IID_DDRAW4).unwrap();
        call(&mut cpu, &mut kernel, ddraw_qi, [ce, 0x1100, 0x1004, 0]);
        let dd4 = cpu.read_u32_le(0x1004).unwrap();
        let dd4_table = cpu.read_u32_le(dd4).unwrap();
        let cooperative =
            kernel.dynamic_exports[&FAKE_MODULE_HANDLE]["ddraw_set_cooperative_level"];
        assert_eq!(cpu.read_u32_le(ce_table + 17 * 4).unwrap(), cooperative);
        assert_eq!(cpu.read_u32_le(dd4_table + 20 * 4).unwrap(), cooperative);
        assert_eq!(
            cpu.read_u32_le(dd4_table + 6 * 4).unwrap(),
            kernel.dynamic_exports[&FAKE_MODULE_HANDLE]["ddraw4_create_surface"]
        );
        // Replay SkyStriker's primary then 320x240 off-screen creation.
        call(
            &mut cpu,
            &mut kernel,
            ddraw_set_cooperative_level,
            [dd4, 0xdead0001, 8, 0],
        );
        let primary_desc = surface_desc2_bytes(320, 240, 640, 0, true);
        cpu.write_mem(0x1200, &primary_desc).unwrap();
        call(
            &mut cpu,
            &mut kernel,
            ddraw4_create_surface,
            [dd4, 0x1200, 0x1008, 0],
        );
        let primary = cpu.read_u32_le(0x1008).unwrap();
        let offscreen_desc = surface_desc2_bytes(320, 240, 640, 0, false);
        cpu.write_mem(0x1200, &offscreen_desc).unwrap();
        call(
            &mut cpu,
            &mut kernel,
            ddraw4_create_surface,
            [dd4, 0x1200, 0x100c, 0],
        );
        let offscreen = cpu.read_u32_le(0x100c).unwrap();
        cpu.write_mem(0x1100, &IID_SURFACE_CE).unwrap();
        call(
            &mut cpu,
            &mut kernel,
            surface_qi,
            [offscreen, 0x1100, 0x1010, 0],
        );
        let ce_surface = cpu.read_u32_le(0x1010).unwrap();
        let (front, back, view) = {
            let mut ctx = CallCtx {
                cpu: &mut cpu,
                kernel: &mut kernel,
                thunk: &t,
            };
            (
                surface_record(&mut ctx, primary).unwrap(),
                surface_record(&mut ctx, offscreen).unwrap(),
                surface_record(&mut ctx, ce_surface).unwrap(),
            )
        };
        assert!(front.primary);
        assert!(!back.primary);
        assert_eq!(
            back, view,
            "QueryInterface must share the off-screen pixels"
        );
        assert_ne!(front.pixels, back.pixels);
        assert_eq!((back.width, back.height, back.pitch), (320, 240, 640));
        let ce_surface_table = cpu.read_u32_le(ce_surface).unwrap();
        assert_eq!(
            cpu.read_u32_le(ce_surface_table + 19 * 4).unwrap(),
            kernel.dynamic_exports[&FAKE_MODULE_HANDLE]["surface_lock"]
        );
        // Lock must respect both descriptor layouts and their buffer sizes.
        for (size, pointer_offset) in [(108u32, 32u32), (124, 36)] {
            cpu.write_mem(0x1300, &[0xa5; 128]).unwrap();
            cpu.write_mem(0x1300, &size.to_le_bytes()).unwrap();
            call(
                &mut cpu,
                &mut kernel,
                surface_lock,
                [ce_surface, 0, 0x1300, 0],
            );
            assert_eq!(cpu.read_u32_le(0x1300).unwrap(), size);
            assert_eq!(
                cpu.read_u32_le(0x1300 + pointer_offset).unwrap(),
                back.pixels
            );
            assert_eq!(cpu.read_u32_le(0x1300 + size).unwrap(), 0xa5a5a5a5);
        }
        assert_eq!(
            cpu.read_u32_le(ce_surface_table + 25 * 4).unwrap(),
            kernel.dynamic_exports[&FAKE_MODULE_HANDLE]["surface_set_palette"]
        );

        // Replay the older DXPAK Surface5 path through the same IID.
        // A fresh view must select slot 25 Lock / slot 32 Unlock while
        // preserving pixels; the compact view above remains compact.
        call(
            &mut cpu,
            &mut kernel,
            surface_qi,
            [primary, 0x1100, 0x1014, 0],
        );
        let legacy = cpu.read_u32_le(0x1014).unwrap();
        let legacy_table = cpu.read_u32_le(legacy).unwrap();
        cpu.write_mem(0x1300, &surface_desc2_bytes(320, 240, 640, 0, true))
            .unwrap();
        call(
            &mut cpu,
            &mut kernel,
            surface_ce_palette_or_legacy_lock,
            [legacy, 0, 0x1300, 0x21],
        );
        assert_eq!(
            cpu.read_u32_le(legacy_table + 25 * 4).unwrap(),
            kernel.dynamic_exports[&FAKE_MODULE_HANDLE]["surface_lock"]
        );
        assert_eq!(
            cpu.read_u32_le(legacy_table + 32 * 4).unwrap(),
            kernel.dynamic_exports[&FAKE_MODULE_HANDLE]["surface_unlock"]
        );
        assert_eq!(cpu.read_u32_le(0x1324).unwrap(), front.pixels);
        assert_eq!(cpu.read_u32_le(0x1368).unwrap(), 0x200);
        assert!(kernel
            .framebuffer
            .directdraw
            .primary_locks
            .contains(&front.pixels));
        cpu.write_mem(front.pixels, &0xffffu16.to_le_bytes())
            .unwrap();
        let before = kernel.framebuffer.frame_counter;
        call(&mut cpu, &mut kernel, surface_unlock, [legacy, 0, 0, 0]);
        assert_eq!(kernel.framebuffer.frame_counter, before + 1);
        assert!(kernel.framebuffer.directdraw.primary_locks.is_empty());
        assert_eq!(&kernel.framebuffer.pixels[..2], &0xffffu16.to_le_bytes());

        // DXPAK callers may leave the Lock output uninitialized, including
        // on their first call. Select its ABI from the write-only Lock
        // arguments, then keep returning the fixed retained layout.
        call(
            &mut cpu,
            &mut kernel,
            surface_qi,
            [offscreen, 0x1100, 0x1018, 0],
        );
        let uninitialized = cpu.read_u32_le(0x1018).unwrap();
        cpu.write_reg(ArmReg::Sp, 0x1500).unwrap();
        cpu.write_mem(0x1500, &0u32.to_le_bytes()).unwrap();
        cpu.write_mem(0x1504, &0u32.to_le_bytes()).unwrap();
        cpu.write_mem(0x1300, &[0xa5; 128]).unwrap();
        call(
            &mut cpu,
            &mut kernel,
            surface_ce_palette_or_legacy_lock,
            [uninitialized, 0, 0x1300, 0x20],
        );
        assert_eq!(cpu.read_u32_le(0x1300).unwrap(), 124);
        assert_eq!(cpu.read_u32_le(0x1324).unwrap(), back.pixels);
        assert_eq!(cpu.read_u32_le(0x137c).unwrap(), 0xa5a5a5a5);
        cpu.write_mem(0x1300, &[0x5a; 128]).unwrap();
        call(
            &mut cpu,
            &mut kernel,
            surface_lock,
            [uninitialized, 0, 0x1300, 0x20],
        );
        assert_eq!(cpu.read_u32_le(0x1300).unwrap(), 124);
        assert_eq!(cpu.read_u32_le(0x1324).unwrap(), back.pixels);
        assert_eq!(cpu.read_u32_le(0x137c).unwrap(), 0x5a5a5a5a);

        call(
            &mut cpu,
            &mut kernel,
            surface_qi,
            [primary, 0x1100, 0x101c, 0],
        );
        let blit_view = cpu.read_u32_le(0x101c).unwrap();
        cpu.write_mem(back.pixels, &0x07e0u16.to_le_bytes())
            .unwrap();
        call(
            &mut cpu,
            &mut kernel,
            surface_ce_flip_or_legacy_blt_fast,
            [blit_view, 0, 0, offscreen],
        );
        assert_eq!(&kernel.framebuffer.pixels[..2], &0x07e0u16.to_le_bytes());
        let table = cpu.read_u32_le(blit_view).unwrap();
        assert_eq!(
            cpu.read_u32_le(table + 7 * 4).unwrap(),
            kernel.dynamic_exports[&FAKE_MODULE_HANDLE]["surface_blt_fast"]
        );

        kernel.window_procs.insert(0xdead0001, 0x1000);
        call(
            &mut cpu,
            &mut kernel,
            ddraw_get_available_vid_mem,
            [ce, 0xdead0001, 8, 0x70001000],
        );
        call(
            &mut cpu,
            &mut kernel,
            ddraw_get_available_vid_mem,
            [ce, 0x1200, 0x1400, 0x1404],
        );
        assert_eq!(cpu.read_u32_le(0x1400).unwrap(), 320 * 240 * 2 * 4);
        assert_eq!(cpu.read_u32_le(0x1404).unwrap(), 320 * 240 * 2 * 4);
    }

    #[test]
    fn directdraw4_descriptor_and_retained_slots_match_the_older_abi() {
        use super::*;
        assert_eq!(slot(&DDRAW4_METHODS, "ddraw4_create_surface"), 6);
        assert_eq!(slot(&DDRAW4_METHODS, "ddraw_set_cooperative_level"), 20);
        assert_eq!(slot(&SURFACE4_METHODS, "surface_lock"), 25);
        assert_eq!(slot(&SURFACE4_METHODS, "surface_unlock"), 32);
        let bytes = surface_desc2_bytes(320, 240, 640, 0x78000000, false);
        let word = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        assert_eq!(word(0), 124);
        assert_eq!((word(8), word(12), word(16)), (240, 320, 640));
        assert_eq!(word(36), 0x78000000);
        assert_eq!(word(72), 32);
        assert_eq!(word(84), 16);
        assert_eq!(word(104), 0x40);
    }

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
