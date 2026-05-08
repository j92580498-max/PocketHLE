//! Pocket PC shell extensions (`aygshell.dll`).

use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

pub fn register(d: &mut WinCeDispatcher) {
    let dll = "aygshell.dll";
    for f in [
        "SHFullScreen",
        "SHCreateMenuBar",
        "SHCreateMenuBarEx",
        "SHHandleWMActivate",
        "SHHandleWMSettingChange",
        "SHInitDialog",
        "SHSipPreference",
        "SHRecognizeGesture",
        "SHCloseApps",
        "SHDoneButton",
        "SHIdleTimerReset",
        "SHEnableSoftkey",
        "SHGetDocumentsFolder",
        "SHSetAppKeyWndAssoc",
    ] {
        d.register_handler(dll, f, ok);
    }
    // PPcAtaxx imports `aygshell.dll` purely by ordinal. The
    // ordinals don't appear in the public WM 5/6 SDK, but ord 21
    // shows up in the leaked WM 2003 lib (`aygshell.lib`) as the
    // helper that maps to "give the menu bar adornment about an
    // edit/menu split" — i.e. a PPC-style SHFullScreen variant.
    // PocketHLE has no real shell, so just succeeding is enough to
    // get past the call site.
    for ord in [12u16, 13, 14, 21, 49, 50, 65, 71, 72, 74, 80, 84] {
        d.register_handler(dll, &format!("ord:{ord}"), ok);
    }
}

fn ok(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}
