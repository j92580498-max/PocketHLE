use pocket_cpu::{regs::ArmReg, Prot};
use pocket_kernel::{DispatchOutcome, GuestCallFrame, KernelError, SYNTHETIC_FRAMEBUFFER_BASE};

use crate::{CallCtx, WinCeDispatcher};

const SDL_INIT_TIMER: u32 = 0x0000_0001;
const SDL_INIT_VIDEO: u32 = 0x0000_0020;
const SDL_SWSURFACE: u32 = 0;
const SDL_PREALLOC: u32 = 0x0100_0000;
const SDL_EVENT_KEYDOWN: u8 = 2;
const SDL_EVENT_KEYUP: u8 = 3;
const SDL_EVENT_MOUSEMOTION: u8 = 4;
const SDL_EVENT_MOUSEBUTTONDOWN: u8 = 5;
const SDL_EVENT_MOUSEBUTTONUP: u8 = 6;
const SDL_PRESSED: u8 = 1;
const SDL_BUTTON_LEFT: u8 = 1;
const FB_MAP: u32 = SYNTHETIC_FRAMEBUFFER_BASE;
const SDL_PIXEL_FORMAT_BYTES: u32 = 36;
const SDL_SURFACE_BYTES: u32 = 60;

pub fn register(d: &mut WinCeDispatcher) {
    let dll = "sdl.dll";
    for name in [
        "SDL_Init",
        "SDL_InitSubSystem",
        "SDL_WasInit",
        "SDL_Quit",
        "SDL_QuitSubSystem",
        "SDL_VideoInit",
        "SDL_VideoQuit",
        "SDL_PumpEvents",
        "SDL_SetEventFilter",
        "SDL_GetEventFilter",
        "SDL_EventState",
        "SDL_EnableUNICODE",
        "SDL_EnableKeyRepeat",
        "SDL_GetKeyRepeat",
        "SDL_SetModState",
        "SDL_GetModState",
        "SDL_NumJoysticks",
        "SDL_JoystickEventState",
        "SDL_GetMouseState",
        "SDL_GetRelativeMouseState",
        "SDL_ShowCursor",
        "SDL_WM_IconifyWindow",
        "SDL_WM_SetCaption",
        "SDL_WM_SetIcon",
        "SDL_WM_ToggleFullScreen",
        "SDL_WM_GrabInput",
        "SDL_SetGamma",
        "SDL_SetGammaRamp",
        "SDL_GetGammaRamp",
        "SDL_SetColors",
        "SDL_SetPalette",
        "SDL_SetClipRect",
        "SDL_GetClipRect",
        "SDL_SetColorKey",
        "SDL_SetAlpha",
        "SDL_DisplayFormat",
        "SDL_DisplayFormatAlpha",
        "SDL_ConvertSurface",
        "SDL_UpperBlit",
        "SDL_LowerBlit",
        "SDL_GL_SetAttribute",
        "SDL_GL_GetAttribute",
        "SDL_GL_SwapBuffers",
        "SDL_GL_UpdateRects",
        "SDL_GL_Lock",
        "SDL_GL_Unlock",
        "SDL_GetWMInfo",
        "SDL_Linked_Version",
        "SDL_VideoDriverName",
        "SDL_GetVideoInfo",
        "SDL_VideoModeOK",
        "SDL_ListModes",
        "SDL_SetError",
        "SDL_ClearError",
        "SDL_GetError",
        "SDL_ThreadID",
        "SDL_GetThreadID",
        "SDL_SetModuleHandle",
    ] {
        d.register_handler(dll, name, sdl_zero);
    }
    for name in ["SDL_Init", "SDL_InitSubSystem"] {
        d.register_handler(dll, name, sdl_init);
    }
    d.register_handler(dll, "SDL_WasInit", sdl_was_init);
    d.register_handler(dll, "SDL_GetTicks", sdl_get_ticks);
    d.register_handler(dll, "SDL_Delay", sdl_delay);
    d.register_handler(dll, "SDL_SetTimer", sdl_set_timer);
    d.register_handler(dll, "SDL_AddTimer", sdl_add_timer);
    d.register_handler(dll, "SDL_RemoveTimer", sdl_remove_timer);
    d.register_handler(dll, "SDL_WaitEvent", sdl_wait_event);
    d.register_handler(dll, "SDL_PollEvent", sdl_poll_event);
    d.register_handler(dll, "SDL_PushEvent", sdl_push_event);
    d.register_handler(dll, "SDL_SetVideoMode", sdl_set_video_mode);
    d.register_handler(dll, "SDL_GetVideoSurface", sdl_get_video_surface);
    d.register_handler(dll, "SDL_UpdateRect", sdl_present);
    d.register_handler(dll, "SDL_UpdateRects", sdl_present);
    d.register_handler(dll, "SDL_Flip", sdl_present);
    d.register_handler(dll, "SDL_LockSurface", sdl_zero);
    d.register_handler(dll, "SDL_UnlockSurface", sdl_zero);
    d.register_handler(dll, "SDL_FillRect", sdl_zero);
    d.register_handler(dll, "SDL_FreeSurface", sdl_free_surface);
    d.register_handler(dll, "SDL_CreateRGBSurface", sdl_create_rgb_surface);
    d.register_handler(dll, "SDL_CreateRGBSurfaceFrom", sdl_create_rgb_surface_from);
    d.register_handler(dll, "SDL_GetRGB", sdl_get_rgb);
    d.register_handler(dll, "SDL_MapRGB", sdl_map_rgb);
    d.register_handler(dll, "SDL_MapRGBA", sdl_map_rgb);
    d.register_handler(dll, "SDL_GetKeyState", sdl_get_key_state);
    d.register_handler(dll, "SDL_GetKeyName", sdl_get_key_name);
    for name in [
        "SDL_JoystickName",
        "SDL_JoystickOpen",
        "SDL_JoystickOpened",
        "SDL_JoystickIndex",
        "SDL_JoystickNumAxes",
        "SDL_JoystickNumBalls",
        "SDL_JoystickNumHats",
        "SDL_JoystickNumButtons",
        "SDL_JoystickUpdate",
        "SDL_JoystickGetAxis",
        "SDL_JoystickGetHat",
        "SDL_JoystickGetBall",
        "SDL_JoystickGetButton",
        "SDL_JoystickClose",
        "SDL_CreateCursor",
        "SDL_SetCursor",
        "SDL_GetCursor",
        "SDL_FreeCursor",
        "SDL_LoadBMP_RW",
        "SDL_SaveBMP_RW",
        "SDL_CreateYUVOverlay",
        "SDL_LockYUVOverlay",
        "SDL_UnlockYUVOverlay",
        "SDL_DisplayYUVOverlay",
        "SDL_FreeYUVOverlay",
        "SDL_GL_LoadLibrary",
        "SDL_GL_GetProcAddress",
        "SDL_CreateThread",
        "SDL_WaitThread",
        "SDL_KillThread",
        "SDL_LoadObject",
        "SDL_LoadFunction",
        "SDL_UnloadObject",
        "SDL_AllocRW",
        "SDL_FreeRW",
        "SDL_RWFromFile",
        "SDL_RWFromFP",
        "SDL_RWFromMem",
        "SDL_RWFromConstMem",
        "SDL_ReadLE16",
        "SDL_ReadBE16",
        "SDL_ReadLE32",
        "SDL_ReadBE32",
        "SDL_ReadLE64",
        "SDL_ReadBE64",
        "SDL_WriteLE16",
        "SDL_WriteBE16",
        "SDL_WriteLE32",
        "SDL_WriteBE32",
        "SDL_WriteLE64",
        "SDL_WriteBE64",
        "SDL_strlcpy",
        "SDL_strlcat",
        "SDL_strdup",
        "SDL_strrev",
        "SDL_strupr",
        "SDL_strlwr",
        "SDL_ltoa",
        "SDL_ultoa",
        "SDL_strcasecmp",
        "SDL_strncasecmp",
        "SDL_snprintf",
        "SDL_vsnprintf",
    ] {
        d.register_handler(dll, name, sdl_zero);
    }
}

fn sdl_zero(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn sdl_init(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _flags = ctx.arg_u32(0).unwrap_or(SDL_INIT_TIMER | SDL_INIT_VIDEO);
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn sdl_was_init(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(SDL_INIT_TIMER | SDL_INIT_VIDEO))
}

#[allow(dead_code)]
fn sdl_quit(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn sdl_get_ticks(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.sdl_clock_ms as u32))
}

fn sdl_delay(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ms = ctx.arg_u32(0).unwrap_or(0).min(1000);
    ctx.kernel.sdl_clock_ms = ctx.kernel.sdl_clock_ms.saturating_add(ms as u64);
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn sdl_set_timer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let interval = ctx.arg_u32(0).unwrap_or(0);
    let callback = ctx.arg_u32(1).unwrap_or(0);
    ctx.kernel.sdl_timer_callback = callback;
    ctx.kernel.sdl_timer_interval_ms = interval;
    ctx.kernel.sdl_timer_next_ms = ctx.kernel.sdl_clock_ms.saturating_add(interval as u64);
    Ok(DispatchOutcome::ReturnedR0(u32::from(interval != 0)))
}

fn sdl_add_timer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let interval = ctx.arg_u32(0).unwrap_or(0);
    let callback = ctx.arg_u32(1).unwrap_or(0);
    ctx.kernel.sdl_timer_callback = callback;
    ctx.kernel.sdl_timer_interval_ms = interval;
    ctx.kernel.sdl_timer_next_ms = ctx.kernel.sdl_clock_ms.saturating_add(interval as u64);
    Ok(DispatchOutcome::ReturnedR0(u32::from(
        callback != 0 && interval != 0,
    )))
}

fn sdl_remove_timer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ctx.kernel.sdl_timer_callback = 0;
    ctx.kernel.sdl_timer_interval_ms = 0;
    ctx.kernel.sdl_timer_frame = None;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn sdl_wait_event(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if let Some(frame) = ctx.kernel.sdl_timer_frame.take() {
        ctx.cpu.write_reg(ArmReg::Sp, frame.sp)?;
        ctx.cpu.write_reg(ArmReg::R0, frame.args[0])?;
        ctx.cpu.write_reg(ArmReg::R1, frame.args[1])?;
        ctx.cpu.write_reg(ArmReg::R2, frame.args[2])?;
        ctx.cpu.write_reg(ArmReg::R3, frame.args[3])?;
        ctx.cpu.write_reg(ArmReg::Lr, frame.lr)?;
        if let Some(event) = ctx.kernel.sdl_pending_event.take() {
            write_sdl_event(ctx, frame.args[0], &event)?;
        }
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    if ctx.kernel.sdl_pending_event.is_none() {
        queue_host_event(ctx)?;
    }
    if ctx.kernel.sdl_pending_event.is_some() {
        let event_ptr = ctx.arg_u32(0)?;
        let event = ctx.kernel.sdl_pending_event.take().unwrap_or([0; 24]);
        write_sdl_event(ctx, event_ptr, &event)?;
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    if ctx.kernel.sdl_timer_callback != 0 && ctx.kernel.sdl_clock_ms >= ctx.kernel.sdl_timer_next_ms
    {
        let callback = ctx.kernel.sdl_timer_callback;
        let frame = GuestCallFrame {
            args: [
                ctx.cpu.read_reg(ArmReg::R0)?,
                ctx.cpu.read_reg(ArmReg::R1)?,
                ctx.cpu.read_reg(ArmReg::R2)?,
                ctx.cpu.read_reg(ArmReg::R3)?,
            ],
            lr: ctx.cpu.read_reg(ArmReg::Lr)?,
            sp: ctx.cpu.read_reg(ArmReg::Sp)?,
        };
        ctx.kernel.sdl_timer_frame = Some(frame);
        ctx.kernel.sdl_timer_next_ms = ctx
            .kernel
            .sdl_clock_ms
            .saturating_add(ctx.kernel.sdl_timer_interval_ms.max(1) as u64);
        ctx.cpu
            .write_reg(ArmReg::R0, ctx.kernel.sdl_timer_interval_ms)?;
        ctx.cpu.write_reg(ArmReg::R1, 0)?;
        ctx.cpu.write_reg(ArmReg::Lr, ctx.thunk.thunk_va)?;
        return Ok(DispatchOutcome::JumpTo(callback));
    }
    ctx.kernel.sdl_clock_ms = ctx.kernel.sdl_clock_ms.saturating_add(10);
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn sdl_poll_event(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.sdl_pending_event.is_none() {
        queue_host_event(ctx)?;
    }
    if ctx.kernel.sdl_pending_event.is_some() {
        let event_ptr = ctx.arg_u32(0)?;
        let event = ctx.kernel.sdl_pending_event.take().unwrap_or([0; 24]);
        write_sdl_event(ctx, event_ptr, &event)?;
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn sdl_push_event(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let event_ptr = ctx.arg_u32(0)?;
    let event = ctx.cpu.read_mem(event_ptr, 24)?;
    let mut queued = [0u8; 24];
    queued.copy_from_slice(&event);
    ctx.kernel.sdl_pending_event = Some(queued);
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn write_sdl_event(ctx: &mut CallCtx<'_>, ptr: u32, event: &[u8; 24]) -> Result<(), KernelError> {
    if ptr != 0 {
        ctx.cpu.write_mem(ptr, event)?;
    }
    Ok(())
}

fn queue_host_event(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    let Some(input) = ctx.kernel.pending_input.pop_front() else {
        return Ok(());
    };
    let (event, key) = sdl_event_from_input(input);
    if let Some((sym, down)) = key {
        if ctx.kernel.sdl_key_state != 0 {
            ctx.cpu.write_mem(
                ctx.kernel.sdl_key_state.wrapping_add(sym),
                &[u8::from(down)],
            )?;
        }
    }
    ctx.kernel.sdl_pending_event = Some(event);
    Ok(())
}

fn sdl_event_from_input(input: pocket_kernel::InputEvent) -> ([u8; 24], Option<(u32, bool)>) {
    let mut event = [0u8; 24];
    match input {
        pocket_kernel::InputEvent::KeyDown { vk } => {
            let sym = sdl_key_from_vk(vk);
            event[0] = SDL_EVENT_KEYDOWN;
            event[2] = SDL_PRESSED;
            event[4..8].copy_from_slice(&sym.to_le_bytes());
            if sym < 128 {
                event[10..12].copy_from_slice(&(sym as u16).to_le_bytes());
            }
            (event, Some((sym, true)))
        }
        pocket_kernel::InputEvent::KeyUp { vk } => {
            let sym = sdl_key_from_vk(vk);
            event[0] = SDL_EVENT_KEYUP;
            event[4..8].copy_from_slice(&sym.to_le_bytes());
            (event, Some((sym, false)))
        }
        pocket_kernel::InputEvent::PointerDown { x, y } => {
            event[0] = SDL_EVENT_MOUSEBUTTONDOWN;
            event[2] = SDL_BUTTON_LEFT;
            event[3] = SDL_PRESSED;
            event[4..6].copy_from_slice(&x.to_le_bytes());
            event[6..8].copy_from_slice(&y.to_le_bytes());
            (event, None)
        }
        pocket_kernel::InputEvent::PointerUp { x, y } => {
            event[0] = SDL_EVENT_MOUSEBUTTONUP;
            event[2] = SDL_BUTTON_LEFT;
            event[4..6].copy_from_slice(&x.to_le_bytes());
            event[6..8].copy_from_slice(&y.to_le_bytes());
            (event, None)
        }
        pocket_kernel::InputEvent::PointerMove { x, y } => {
            event[0] = SDL_EVENT_MOUSEMOTION;
            event[4..6].copy_from_slice(&x.to_le_bytes());
            event[6..8].copy_from_slice(&y.to_le_bytes());
            (event, None)
        }
    }
}

fn sdl_key_from_vk(vk: u16) -> u32 {
    match vk {
        0x08 => 8,
        0x0d => 13,
        0x1b => 27,
        0x20 => 32,
        0x25 => 276,
        0x26 => 273,
        0x27 => 275,
        0x28 => 274,
        _ => vk as u32,
    }
}

fn sdl_get_key_state(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let num_keys = ctx.arg_u32(0).unwrap_or(0);
    let ptr = ctx.kernel.sdl_key_state;
    if ptr == 0 {
        let ptr = ctx.kernel.heap.alloc(512).unwrap_or(0);
        if ptr == 0 {
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
        ctx.cpu.write_mem(ptr, &[0; 512])?;
        ctx.kernel.sdl_key_state = ptr;
    }
    if num_keys != 0 {
        ctx.cpu.write_mem(num_keys, &(256u32).to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.sdl_key_state))
}

fn sdl_get_key_name(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _key = ctx.arg_u32(0)?;
    let ptr = ctx.kernel.heap.alloc(8).unwrap_or(0);
    if ptr != 0 {
        ctx.cpu.write_mem(ptr, b"?\0")?;
    }
    Ok(DispatchOutcome::ReturnedR0(ptr))
}

fn sdl_set_video_mode(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let w = ctx.arg_u32(0)?.max(1);
    let h = ctx.arg_u32(1)?.max(1);
    let pixels = page_align(w.saturating_mul(h).saturating_mul(2));
    if !ctx.kernel.fb_mapped {
        ctx.cpu
            .map_region(FB_MAP, pixels, Prot::READ | Prot::WRITE)?;
        ctx.kernel.framebuffer.resize(w, h);
        ctx.cpu.write_mem(FB_MAP, &ctx.kernel.framebuffer.pixels)?;
        ctx.kernel.fb_mapped = true;
    }
    let format = ctx.kernel.heap.alloc(SDL_PIXEL_FORMAT_BYTES).unwrap_or(0);
    let surface = ctx.kernel.heap.alloc(SDL_SURFACE_BYTES).unwrap_or(0);
    if format == 0 || surface == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut pf = [0u8; SDL_PIXEL_FORMAT_BYTES as usize];
    pf[4] = 16;
    pf[5] = 2;
    pf[6] = 3;
    pf[7] = 2;
    pf[8] = 3;
    pf[10] = 11;
    pf[11] = 5;
    pf[12] = 0;
    pf[32] = 255;
    pf[16..20].copy_from_slice(&0xf800u32.to_le_bytes());
    pf[20..24].copy_from_slice(&0x07e0u32.to_le_bytes());
    pf[24..28].copy_from_slice(&0x001fu32.to_le_bytes());
    ctx.cpu.write_mem(format, &pf)?;
    let mut s = [0u8; SDL_SURFACE_BYTES as usize];
    s[0..4].copy_from_slice(&(SDL_PREALLOC | SDL_SWSURFACE).to_le_bytes());
    s[4..8].copy_from_slice(&format.to_le_bytes());
    s[8..12].copy_from_slice(&(w as i32).to_le_bytes());
    s[12..16].copy_from_slice(&(h as i32).to_le_bytes());
    s[16..18].copy_from_slice(&((w * 2) as u16).to_le_bytes());
    s[20..24].copy_from_slice(&FB_MAP.to_le_bytes());
    s[56..60].copy_from_slice(&1u32.to_le_bytes());
    ctx.cpu.write_mem(surface, &s)?;
    ctx.kernel.sdl_video_surface = surface;
    Ok(DispatchOutcome::ReturnedR0(surface))
}

fn sdl_get_video_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.sdl_video_surface))
}

fn sdl_create_rgb_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let w = ctx.arg_u32(1)?.max(1);
    let h = ctx.arg_u32(2)?.max(1);
    let pitch = w.saturating_mul(2);
    let pixels = ctx.kernel.heap.alloc(pitch.saturating_mul(h)).unwrap_or(0);
    let format = ctx.kernel.heap.alloc(SDL_PIXEL_FORMAT_BYTES).unwrap_or(0);
    let surface = ctx.kernel.heap.alloc(SDL_SURFACE_BYTES).unwrap_or(0);
    if pixels == 0 || format == 0 || surface == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    ctx.cpu.write_mem(pixels, &vec![0; (pitch * h) as usize])?;
    write_surface(ctx, surface, format, w, h, pitch, pixels)?;
    Ok(DispatchOutcome::ReturnedR0(surface))
}

fn sdl_create_rgb_surface_from(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pixels = ctx.arg_u32(0)?;
    let w = ctx.arg_u32(1)?.max(1);
    let h = ctx.arg_u32(2)?.max(1);
    let pitch = ctx.arg_u32(4)?.max(w.saturating_mul(2));
    let format = ctx.kernel.heap.alloc(SDL_PIXEL_FORMAT_BYTES).unwrap_or(0);
    let surface = ctx.kernel.heap.alloc(SDL_SURFACE_BYTES).unwrap_or(0);
    if format == 0 || surface == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    write_surface(ctx, surface, format, w, h, pitch, pixels)?;
    Ok(DispatchOutcome::ReturnedR0(surface))
}

fn write_surface(
    ctx: &mut CallCtx<'_>,
    surface: u32,
    format: u32,
    w: u32,
    h: u32,
    pitch: u32,
    pixels: u32,
) -> Result<(), KernelError> {
    let mut pf = [0u8; SDL_PIXEL_FORMAT_BYTES as usize];
    pf[4] = 16;
    pf[5] = 2;
    pf[6] = 3;
    pf[7] = 2;
    pf[8] = 3;
    pf[10] = 11;
    pf[11] = 5;
    pf[12] = 0;
    pf[32] = 255;
    pf[16..20].copy_from_slice(&0xf800u32.to_le_bytes());
    pf[20..24].copy_from_slice(&0x07e0u32.to_le_bytes());
    pf[24..28].copy_from_slice(&0x001fu32.to_le_bytes());
    ctx.cpu.write_mem(format, &pf)?;
    let mut s = [0u8; SDL_SURFACE_BYTES as usize];
    s[0..4].copy_from_slice(&SDL_SWSURFACE.to_le_bytes());
    s[4..8].copy_from_slice(&format.to_le_bytes());
    s[8..12].copy_from_slice(&(w as i32).to_le_bytes());
    s[12..16].copy_from_slice(&(h as i32).to_le_bytes());
    s[16..18].copy_from_slice(&(pitch as u16).to_le_bytes());
    s[20..24].copy_from_slice(&pixels.to_le_bytes());
    s[56..60].copy_from_slice(&1u32.to_le_bytes());
    ctx.cpu.write_mem(surface, &s)?;
    Ok(())
}

fn sdl_free_surface(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn sdl_present(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.fb_mapped {
        let len = ctx.kernel.framebuffer.pixels.len();
        if ctx.kernel.gx_readback_scratch.len() != len {
            ctx.kernel.gx_readback_scratch.resize(len, 0);
        }
        ctx.cpu
            .read_mem_into(FB_MAP, &mut ctx.kernel.gx_readback_scratch)?;
        if ctx.kernel.gx_readback_scratch != ctx.kernel.framebuffer.pixels {
            ctx.kernel
                .framebuffer
                .pixels
                .copy_from_slice(&ctx.kernel.gx_readback_scratch);
            ctx.kernel.framebuffer.mark_dirty();
            ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
        }
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn sdl_get_rgb(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pixel = ctx.arg_u32(0)? as u16;
    let r = ((pixel >> 11) & 0x1f) << 3;
    let g = ((pixel >> 5) & 0x3f) << 2;
    let b = (pixel & 0x1f) << 3;
    for (idx, value) in [(2, r as u8), (3, g as u8), (4, b as u8)] {
        let ptr = ctx.arg_u32(idx)?;
        if ptr != 0 {
            ctx.cpu.write_mem(ptr, &[value])?;
        }
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn sdl_map_rgb(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let r = ctx.arg_u32(1).unwrap_or(0).min(255);
    let g = ctx.arg_u32(2).unwrap_or(0).min(255);
    let b = ctx.arg_u32(3).unwrap_or(0).min(255);
    Ok(DispatchOutcome::ReturnedR0(
        ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3),
    ))
}

fn page_align(value: u32) -> u32 {
    value.saturating_add(0xfff) & !0xfff
}
