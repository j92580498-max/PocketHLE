//! Stubs for the small native game libraries bundled with MetalStrike.

use pocket_cpu::Prot;
use pocket_kernel::{DispatchOutcome, KernelError, SYNTHETIC_FRAMEBUFFER_BASE};

use crate::{CallCtx, WinCeDispatcher};

pub fn register(d: &mut WinCeDispatcher) {
    register_gd201b(d);
    register_game_x(d);
    register_sound_x(d);
    d.register_handler("note_prj.dll", "ord:7", find_first_flash_card);
    d.register_handler("note_prj.dll", "#7", find_first_flash_card);
    d.register_handler("note_prj.dll", "ord:8", find_next_flash_card);
    d.register_handler("note_prj.dll", "#8", find_next_flash_card);
}

fn register_gd201b(d: &mut WinCeDispatcher) {
    let dll = "gd201b.dll";
    for name in [
        "??0CGapiDisplay@@QAA@XZ",
        "??0CGapiSurface@@QAA@XZ",
        "??0CGapiInput@@QAA@XZ",
        "??0CGapiTimer@@QAA@XZ",
        "??0CGapiRGBASurface@@QAA@XZ",
        "??0CGapiBitmapFont@@QAA@XZ",
    ] {
        d.register_handler(dll, name, gd_constructor);
    }
    for name in [
        "??1CGapiDisplay@@UAA@XZ",
        "??1CGapiInput@@UAA@XZ",
        "??1CGapiTimer@@UAA@XZ",
        "??1CGapiSurface@@UAA@XZ",
        "??1CGapiRGBASurface@@UAA@XZ",
        "??1CGapiBitmapFont@@UAA@XZ",
    ] {
        d.register_handler(dll, name, gd_destructor);
    }
    for name in [
        "?SetDisplayMode@CGapiDisplay@@QAAJK@Z",
        "?OpenDisplay@CGapiDisplay@@QAAJKPAUHWND__@@KKKKKK@Z",
        "?CloseDisplay@CGapiDisplay@@QAAJXZ",
        "?StartTimer@CGapiTimer@@QAAJK@Z",
        "?WaitForNextFrame@CGapiTimer@@QAAJXZ",
        "?SurfacesAreLost@CGapiDisplay@@QAAJXZ",
        "?SetColorKey@CGapiSurface@@QAAJK@Z",
    ] {
        d.register_handler(dll, name, gd_success);
    }
    for name in [
        "?GetBackBuffer@CGapiDisplay@@QAAJPAVCGapiSurface@@@Z",
        "?CreateSurface@CGapiRGBASurface@@QAAJKPAUHINSTANCE__@@KPBG@Z",
        "?CreateSurface@CGapiSurface@@QAAJKPAUHINSTANCE__@@KPBG@Z",
        "?CreateSurface@CGapiSurface@@QAAJKKK@Z",
    ] {
        d.register_handler(dll, name, gd_surface_pointer);
    }
    for name in [
        "?GetWidth@CGapiRGBASurface@@QAAKXZ",
        "?GetWidth@CGapiSurface@@QAAKXZ",
    ] {
        d.register_handler(dll, name, gd_width);
    }
    for name in [
        "?GetHeight@CGapiRGBASurface@@QAAKXZ",
        "?GetHeight@CGapiSurface@@QAAKXZ",
    ] {
        d.register_handler(dll, name, gd_height);
    }
    d.register_handler(dll, "?Flip@CGapiDisplay@@QAAJXZ", gd_flip);
    d.register_handler(
        dll,
        "?GetHWStatus@CGapiDisplay@@QAAJPAK@Z",
        gd_get_hw_status,
    );
    d.register_handler(
        dll,
        "?DrawTextW@CGapiSurface@@QAAJKKPBGKPAK@Z",
        gd_draw_text,
    );
    d.register_handler(
        dll,
        "?DrawTextW@CGapiSurface@@QAAJKKPBGPAVCGapiBitmapFont@@KKPAU_GDBLTFASTFX@@PAK@Z",
        gd_draw_text,
    );
    for name in [
        "?FillRect@CGapiSurface@@QAAJPAUtagRECT@@KKPAU_GDFILLRECTFX@@@Z",
        "?BltFast@CGapiSurface@@QAAJKKPAV1@PAUtagRECT@@KPAU_GDBLTFASTFX@@@Z",
        "?AlphaBltFast@CGapiSurface@@QAAJKKPAVCGapiRGBASurface@@PAUtagRECT@@KPAU_GDALPHABLTFASTFX@@@Z",
    ] {
        d.register_handler(dll, name, gd_blit);
    }
}

fn gd_constructor(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.arg_u32(0)?))
}

fn gd_destructor(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn gd_success(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gd_get_hw_status(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let status = ctx.arg_u32(1)?;
    if status != 0 {
        ctx.cpu.write_mem(status, &0u32.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gd_get_back_buffer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_surface(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(SYNTHETIC_FRAMEBUFFER_BASE))
}

fn gd_surface_pointer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_surface(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(SYNTHETIC_FRAMEBUFFER_BASE))
}

fn gd_width(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.framebuffer.width))
}

fn gd_height(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.framebuffer.height))
}

fn gd_surface_object(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.arg_u32(0)?))
}

fn gd_draw_text(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _ = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gd_blit(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_surface(ctx)?;
    ctx.kernel.framebuffer.mark_dirty();
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn gd_flip(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_surface(ctx)?;
    ctx.kernel.framebuffer.mark_dirty();
    ctx.kernel.direct_fb_frames = ctx.kernel.direct_fb_frames.saturating_add(1);
    ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
    Ok(DispatchOutcome::ReturnedR0(1))
}

const FLASH_CARD_HANDLE: u32 = 0xDEAD_F100;
const FIND_DATA_NAME_OFFSET: usize = 40;
const FIND_DATA_SIZE: usize = FIND_DATA_NAME_OFFSET + 260 * 2;

fn write_flash_card_find_data(ctx: &mut CallCtx<'_>, out: u32) -> Result<(), KernelError> {
    if out == 0 {
        return Ok(());
    }
    let mut data = vec![0u8; FIND_DATA_SIZE];
    data[0..4].copy_from_slice(&0x10u32.to_le_bytes());
    for (index, unit) in "Storage Card".encode_utf16().enumerate() {
        let offset = FIND_DATA_NAME_OFFSET + index * 2;
        data[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }
    ctx.cpu.write_mem(out, &data)?;
    Ok(())
}

fn find_first_flash_card(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(0)?;
    write_flash_card_find_data(ctx, out)?;
    Ok(DispatchOutcome::ReturnedR0(FLASH_CARD_HANDLE))
}

fn find_next_flash_card(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _handle = ctx.arg_u32(0)?;
    let _out = ctx.arg_u32(1)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn register_game_x(d: &mut WinCeDispatcher) {
    for dll in ["ngamex.dll", "ngamex2k3.dll"] {
        d.register_handler(dll, "?GetSurface@nGameX@@QAAQAGXZ", get_surface);
        d.register_handler(dll, "?Update@nGameX@@SAXXZ", void_returning);
        d.register_handler(dll, "??1nGameX@@QAA@XZ", destructor);
        d.register_handler(dll, "??0nGameX@@QAA@PAUHWND__@@@Z", constructor);
        d.register_handler(dll, "?m_KeyList@nGameX@@2UGXKeyList@@A", key_list);
        d.register_handler(dll, "?GetScrWidth@nGameX@@QAA?BKXZ", screen_width);
        d.register_handler(dll, "?GetScrHeight@nGameX@@QAA?BKXZ", screen_height);
        d.register_handler(dll, "?Suspend@nGameX@@SAHXZ", success);
        d.register_handler(dll, "?Resume@nGameX@@SAHXZ", success);
    }
}

fn register_sound_x(d: &mut WinCeDispatcher) {
    let dll = "nsoundx.dll";
    d.register_handler(
        dll,
        "?Ns_WaveOpen@@YA_NPAUHWND__@@W4waveSpeak@@W4waveRate@@W4waveBits@@I@Z",
        success,
    );
    d.register_handler(dll, "?_Ns_LoadMod@@YA_NPAEKK@Z", success);
    d.register_handler(dll, "?Ns_FreeMod@@YAXXZ", void_returning);
    d.register_handler(dll, "?Ns_StopMod@@YAXXZ", void_returning);
    d.register_handler(dll, "?_Ns_PlayWave@@YA_NPAEH@Z", success);
    d.register_handler(dll, "?Ns_PlayMod@@YAXXZ", void_returning);
    d.register_handler(dll, "?Ns_WaveClose@@YA_NXZ", success);
}

fn constructor(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let this_ptr = ctx.arg_u32(0)?;
    if this_ptr != 0 {
        let _ = ctx.cpu.write_mem(this_ptr, &[0u8; 0x140]);
    }
    Ok(DispatchOutcome::ReturnedR0(this_ptr))
}

fn ensure_surface(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    if ctx.kernel.fb_mapped {
        return Ok(());
    }
    let bytes = (ctx.kernel.framebuffer.byte_size() + 0xfff) & !0xfff;
    ctx.cpu
        .map_region(SYNTHETIC_FRAMEBUFFER_BASE, bytes, Prot::READ | Prot::WRITE)?;
    ctx.cpu
        .write_mem(SYNTHETIC_FRAMEBUFFER_BASE, &ctx.kernel.framebuffer.pixels)?;
    ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
    ctx.kernel.fb_mapped = true;
    Ok(())
}

fn get_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_surface(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(SYNTHETIC_FRAMEBUFFER_BASE))
}

fn key_list(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn screen_width(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.framebuffer.width))
}

fn screen_height(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.framebuffer.height))
}

fn destructor(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn success(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn void_returning(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}
