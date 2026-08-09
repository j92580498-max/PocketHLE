//! Hekkus Sound System (`hss.dll`).
//!
//! HSS is a freeware C++ audio mixer commonly bundled with Pocket PC
//! games. The JumpyBall test ROM uses the C++ classes
//! `hssSpeaker`, `hssSound`, `hssMusic` directly. HSS object methods are
//! retained as compatibility handlers; waveOut remains the authoritative
//! PCM transport used by the shipped Asphalt 2 titles.

use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

pub fn register(d: &mut WinCeDispatcher) {
    let dll = "hss.dll";
    let identity_stubs = [
        "??0hssSound@@QAA@XZ",
        "??0hssMusic@@QAA@XZ",
        "??0hssSpeaker@@QAA@XZ",
        "??1hssSound@@UAA@XZ",
        "??1hssMusic@@UAA@XZ",
        "??1hssSpeaker@@UAA@XZ",
    ];
    for f in identity_stubs {
        d.register_handler(dll, f, this_returning);
    }
    let success_stubs = [
        "?bufferLength@hssSpeaker@@QAAHH@Z",
        "?channel@hssSpeaker@@QAAPAVhssChannel@@H@Z",
        "?frequency@hssChannel@@QAAXI@Z",
        "?load@hssMusic@@QAAHPAX_N@Z",
        "?load@hssSound@@QAAHPAX_N@Z",
        "?loop@hssMusic@@QAAX_N@Z",
        "?loop@hssSound@@QAAX_N@Z",
        "?open@hssSpeaker@@QAAHII_NII@Z",
        "?pauseMusics@hssSpeaker@@QAAXXZ",
        "?pauseSounds@hssSpeaker@@QAAXXZ",
        "?playMusic@hssSpeaker@@QAAHPAVhssMusic@@I@Z",
        "?playSound@hssSpeaker@@QAAHPAVhssSound@@I@Z",
        "?playing@hssChannel@@QAA_NXZ",
        "?stop@hssChannel@@QAAXXZ",
        "?stopMusics@hssSpeaker@@QAAXXZ",
        "?stopSounds@hssSpeaker@@QAAXXZ",
        "?unpauseMusics@hssSpeaker@@QAAXXZ",
        "?unpauseSounds@hssSpeaker@@QAAXXZ",
        "?volume@hssMusic@@QAAXI@Z",
        "?volume@hssSound@@QAAIXZ",
        "?volume@hssSound@@QAAXI@Z",
        "?volumeMusics@hssSpeaker@@QAAXI@Z",
        "?volumeSounds@hssSpeaker@@QAAXI@Z",
    ];
    for f in success_stubs {
        d.register_handler(dll, f, ok);
    }
}

fn ok(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _ = ctx.arg_u32(0)?;
    let name = ctx
        .thunk
        .friendly_name
        .as_deref()
        .or(match &ctx.thunk.binding {
            pocket_pe::ImportBinding::Name(name) => Some(name.as_str()),
            pocket_pe::ImportBinding::Ordinal(_) => None,
        })
        .unwrap_or_default();
    let result = if name.contains("channel@hssSpeaker") {
        let channel = ctx.kernel.heap.alloc(0x100).unwrap_or(0);
        if channel != 0 {
            let _ = ctx.cpu.write_mem(channel, &[0; 0x100]);
        }
        channel
    } else if name.contains("playing@hssChannel")
        || name.contains("frequency@hssChannel")
        || name.contains("volume@hssSound@@QAAIXZ")
    {
        0
    } else if name.contains("bufferLength@hssSpeaker") {
        4096
    } else {
        1
    };
    Ok(DispatchOutcome::ReturnedR0(result))
}

/// Constructor stub: zeroes out a small block at `this` (so the
/// caller's object isn't full of stack garbage) and returns `this`.
fn this_returning(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.arg_u32(0)?))
}
