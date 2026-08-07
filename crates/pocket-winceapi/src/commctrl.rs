//! Common Controls library (`commctrl.dll`).
//!
//! Pocket PC apps that draw their own UI still call into commctrl
//! for [`InitCommonControls`], [`InitCommonControlsEx`] and a few
//! [`ImageList_*`] helpers. The library on PPC2003 exports
//! everything by ordinal only (no name table), so we register both
//! the friendly name (looked up via `data/commctrl-ordinals.json`,
//! when present) and the bare ordinal form.

use pocket_kernel::{DispatchOutcome, KernelError, StatusBar};

use crate::{coredll::FAKE_STATUSBAR_HWND, CallCtx, WinCeDispatcher};

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

    // Stub ordinals 1..=80 by default, then override the ones we do
    // model. The most-frequent culprits for "unimplemented call ->
    // commctrl.dll!#N" on Pocket PC are:
    //
    //   #1  InitCommonControls
    //   #2  InitCommonControlsEx
    //   #6  CreateUpDownControl
    //
    // Returning success leaves the control invisible but doesn't crash
    // anything downstream.
    for ord in 1u16..=80 {
        d.register_handler(dll, &format!("ord:{ord}"), ok);
    }

    // #2, #4 and #5 are successful no-op common-control calls in the
    // PPC2003 ordinal space used by Cubis.
    d.register_handler(dll, "ord:2", ok);
    d.register_handler(dll, "ord:4", ok);
    d.register_handler(dll, "ord:5", ok);
    // Pocket PC 2002's command-bar helper is exported as ordinal 12
    // by commctrl.dll. Armor Game passes a 32-byte MENU_BAR_INFO
    // record and treats a false return as fatal, so it must succeed and
    // publish the synthetic command-bar handle in both legacy layouts.
    d.register_handler(dll, "ord:12", create_menu_bar);

    // #17 is `CreateStatusWindowW` in the PPC2002 ordinal space.
    // PPC2002 Solitaire calls it as
    //   (0x50000003, NULL, 0xdead0001, 45)
    // = (WS_CHILD|WS_VISIBLE|CCS_BOTTOM, no initial text, hwndParent,
    //   control id 45), then drives it with SB_SETPARTS(2) and two
    // SB_SETTEXTW calls carrying "Time: %d " and "  Score: ". The app
    // imports no text-output API at all, so unless we both create the
    // control and draw it, the status strip can never appear.
    d.register_handler(dll, "ord:17", create_status_window);
    for f in ["CreateStatusWindow", "CreateStatusWindowW"] {
        d.register_handler(dll, f, create_status_window);
    }
}

fn create_menu_bar(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let info = ctx.arg_u32(0)?;
    if info == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let cb_size = ctx.cpu.read_u32_le(info).unwrap_or(0);
    let offsets: &[u32] = match cb_size {
        32 => &[0x18, 0x1c],
        36 => &[0x1c, 0x18],
        _ => &[0x18, 0x1c],
    };
    for offset in offsets {
        ctx.cpu
            .write_mem(info + offset, &FAKE_STATUSBAR_HWND.to_le_bytes())?;
    }
    log::debug!("commctrl ordinal 12 command bar created from 0x{info:08x} (cbSize={cb_size})");
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `CreateStatusWindowW(style, lpszText, hwndParent, wID)`.
fn create_status_window(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let style = ctx.arg_u32(0)?;
    let text = ctx.arg_u32(1)?;
    let parent = ctx.arg_u32(2)?;
    let id = ctx.arg_u32(3)?;

    let mut bar = StatusBar {
        parent,
        height: StatusBar::DEFAULT_HEIGHT,
        ..Default::default()
    };
    // A non-NULL lpszText seeds part 0 — CreateStatusWindow is
    // documented as equivalent to creating the control and sending it
    // one SB_SETTEXT.
    if text != 0 {
        let s = String::from_utf16_lossy(&read_status_text(ctx, text));
        if !s.is_empty() {
            bar.set_part_text(0, s);
        }
    }
    ctx.kernel.status_bar = Some(bar);
    log::debug!(
        "CreateStatusWindowW(style=0x{style:08x}, parent=0x{parent:08x}, id={id}) \
         -> hwnd=0x{FAKE_STATUSBAR_HWND:08x}"
    );
    Ok(DispatchOutcome::ReturnedR0(FAKE_STATUSBAR_HWND))
}

/// Read a NUL-terminated UTF-16 string, bounded so a bogus pointer
/// can't spin.
fn read_status_text(ctx: &mut CallCtx<'_>, p: u32) -> Vec<u16> {
    let mut out = Vec::new();
    for i in 0..256u32 {
        match ctx.cpu.read_u32_le(p + i * 2) {
            Ok(v) => {
                let c = (v & 0xffff) as u16;
                if c == 0 {
                    break;
                }
                out.push(c);
            }
            Err(_) => break,
        }
    }
    out
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
