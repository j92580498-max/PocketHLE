//! Stubs for the small native game libraries bundled with MetalStrike.

use pocket_cpu::Prot;
use pocket_kernel::{DispatchOutcome, KernelError, SYNTHETIC_FRAMEBUFFER_BASE};

use crate::{CallCtx, WinCeDispatcher};

pub fn register(d: &mut WinCeDispatcher) {
    register_game_x(d);
    register_sound_x(d);
    d.register_handler("note_prj.dll", "ord:7", find_first_flash_card);
    d.register_handler("note_prj.dll", "#7", find_first_flash_card);
    d.register_handler("note_prj.dll", "ord:8", find_next_flash_card);
    d.register_handler("note_prj.dll", "#8", find_next_flash_card);
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
