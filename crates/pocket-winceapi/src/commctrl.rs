//! Common Controls library (`commctrl.dll`).
//!
//! Pocket PC apps that draw their own UI still call into commctrl
//! for [`InitCommonControls`], [`InitCommonControlsEx`] and a few
//! [`ImageList_*`] helpers. The library on PPC2003 exports
//! everything by ordinal only (no name table), so we register both
//! the friendly name (looked up via `data/commctrl-ordinals.json`,
//! when present) and the bare ordinal form.

use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

pub fn register(d: &mut WinCeDispatcher) {
    let dll = "commctrl.dll";

    // Named exports — present in the WM5/WM6 SDK headers even when
    // the lib stripped names.
    for f in [
        "InitCommonControls",
        "InitCommonControlsEx",
        "ImageList_Create",
        "ImageList_Destroy",
        "ImageList_Add",
        "ImageList_AddMasked",
        "ImageList_Draw",
        "ImageList_DrawEx",
        "ImageList_GetIconSize",
        "ImageList_SetIconSize",
        "ImageList_GetImageCount",
        "ImageList_Replace",
        "ImageList_ReplaceIcon",
        "ImageList_SetBkColor",
        "_TrackMouseEvent",
    ] {
        let handler = match f {
            "ImageList_Create" => image_list_create,
            "ImageList_Add" => image_list_add,
            "ImageList_Destroy" => image_list_destroy,
            "ImageList_GetImageCount" => image_list_get_image_count,
            _ => ok,
        };
        d.register_handler(dll, f, handler);
    }

    // Stub ordinals 1..=80 by default. The most-frequent culprits
    // for "unimplemented call -> commctrl.dll!#N" on Pocket PC are:
    //
    //   #1  InitCommonControls
    //   #2  InitCommonControlsEx
    //   #5  CreateStatusWindowW (status bar at the bottom of the
    //       screen) — Enigma calls this from its splash logic
    //   #6  CreateUpDownControl
    //
    // None of those need real implementations to keep a game alive
    // — returning success leaves the menu/status bar invisible but
    // doesn't crash anything downstream.
    for ord in 1u16..=80 {
        d.register_handler(dll, &format!("ord:{ord}"), ok);
    }
}

fn ok(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

const IMAGE_LIST_BASE: u32 = 0xDEAD_2A00;

fn image_list_create(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _cx = ctx.arg_u32(0)?;
    let _cy = ctx.arg_u32(1)?;
    let _flags = ctx.arg_u32(2)?;
    let _initial = ctx.arg_u32(3)?;
    let _grow = ctx.arg_u32(4)?;
    Ok(DispatchOutcome::ReturnedR0(IMAGE_LIST_BASE))
}

fn image_list_add(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _list = ctx.arg_u32(0)?;
    let _bitmap = ctx.arg_u32(1)?;
    let _mask = ctx.arg_u32(2)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn image_list_destroy(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn image_list_get_image_count(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(8))
}
