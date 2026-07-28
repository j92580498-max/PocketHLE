//! Stubs for the small native game libraries bundled with MetalStrike.

use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

pub fn register(d: &mut WinCeDispatcher) {
    register_game_x(d);
    register_sound_x(d);
}

fn register_game_x(d: &mut WinCeDispatcher) {
    let dll = "ngamex.dll";
    d.register_handler(dll, "?Update@nGameX@@SAXXZ", void_returning);
    d.register_handler(dll, "??1nGameX@@QAA@XZ", destructor);
    d.register_handler(dll, "??0nGameX@@QAA@PAUHWND__@@@Z", constructor);
    d.register_handler(dll, "?Suspend@nGameX@@SAHXZ", success);
    d.register_handler(dll, "?Resume@nGameX@@SAHXZ", success);
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

fn destructor(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn success(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn void_returning(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}
