//! Pocket PC shell extensions (`aygshell.dll`).
//!
//! Two generations of Pocket PC use *different* ordinal spaces for
//! this DLL, and neither publishes the mapping in a public SDK:
//!
//! * Pocket PC 2003 / WM5+ — `SHFullScreen` is #56, `SHCreateMenuBar`
//!   #65, `SHInitDialog` #100 (see `data/aygshell-ordinals.json`).
//! * Pocket PC 2002 — everything sits much lower. `Solitaire` for
//!   PPC2002 imports 4, 9, 22, 32, 33, 53, 54, 56 and 74, and the
//!   only one that resolves through the PPC2003 table is #56, as
//!   `SHFullScreen` — which is wrong for this generation: the call
//!   site at RVA 0x11dd4 hands it a `SHINITDLGINFO`, so on PPC2002
//!   #56 is `SHInitDialog`.
//!
//! Because the two spaces collide we can't name the PPC2002 ordinals
//! in the shared JSON table. Instead each ordinal a known PPC2002
//! title imports gets a handler here, documented with the call-site
//! evidence that pinned down its signature.

use pocket_kernel::gdi::STOCK_SYSTEM_FONT;
use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

/// Handle we hand back as the menu bar / command bar window. Games
/// stash it in a global and `SendMessageW` to it later; our window
/// handlers tolerate an HWND they've never seen.
pub const FAKE_MENUBAR_HWND: u32 = 0xDEAD_0B01;

/// `SPI_GETSIPINFO` — the `SHSipInfo` action PPC2002 Solitaire uses
/// to query the soft input panel before toggling it.
const SPI_GETSIPINFO: u32 = 0x0001_00E1;

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
        "SHSipInfo",
        "SHRecognizeGesture",
        "SHCloseApps",
        "SHDoneButton",
        "SHIdleTimerReset",
        "SHEnableSoftkey",
        "SHGetDocumentsFolder",
        "SHSetAppKeyWndAssoc",
    ] {
        let handler = match f {
            "SHCreateMenuBar" | "SHCreateMenuBarEx" => sh_create_menu_bar,
            "SHSipInfo" => sh_sip_info,
            _ => ok,
        };
        d.register_handler(dll, f, handler);
    }

    // PPcAtaxx imports `aygshell.dll` purely by ordinal. The
    // ordinals don't appear in the public WM 5/6 SDK, but ord 21
    // shows up in the leaked WM 2003 lib (`aygshell.lib`) as the
    // helper that maps to "give the menu bar adornment about an
    // edit/menu split" — i.e. a PPC-style SHFullScreen variant.
    // PocketHLE has no real shell, so just succeeding is enough to
    // get past the call site.
    // 341 / 344 are the ordinals Gameloft's SDL port (Sonic Unleashed)
    // imports for its full-screen / task-bar handling.
    //
    // 4, 9 and 74 used to be blanket-stubbed here too. They now have
    // real handlers in the PPC2002 block below, which still answers
    // TRUE for 9 — so Pocket DeathMatch keeps the return value its
    // startup path checks.
    for ord in [12u16, 13, 14, 21, 40, 49, 50, 65, 71, 72, 80, 84, 341, 344] {
        d.register_handler(dll, &format!("ord:{ord}"), ok);
    }

    // ---- Pocket PC 2002 ordinal space -------------------------------
    //
    // Signatures recovered from `Solitaire` (PPC2002); addresses are
    // RVAs in that binary.
    //
    //   #4  0x12878  SHSipInfo(dwAction, uParam, pvParam, fWinIni) —
    //                called with dwAction=0x100e1, a 48-byte SIPINFO
    //                zeroed on the stack, and cbSize pre-filled to 48.
    //   #9  0x11398  no args, returns BOOL. WinMain does
    //                `movs r3, r0; beq <teardown>` right after, so a
    //                zero return sends the app straight to its
    //                seven-`LocalFree` exit path. This is the
    //                `SHInitExtraControls`-style "wire up the shell
    //                helpers" call every PPC2002 app makes just after
    //                `ImmDisableIME`.
    //   #22 0x12780  (hwnd, ?, fShow, ?) — paired with #33 inside an
    //                activate/hibernate helper; the return is ignored.
    //   #32 0x17984  (hwnd, pszText, pszCaption, ?, uType) returning a
    //                dialog result — the caller compares against 6
    //                (IDYES). A `SHMessageBoxCheck` equivalent.
    //   #33 0x1278c  (hwnd, ?) -> BOOL. A TRUE makes the caller
    //                `PostMessageW(hwnd, WM_CLOSE, 0, 0)`, so this one
    //                MUST answer FALSE or the game closes itself.
    //   #53 0x11db4  (hwnd, ?, 1) -> HFONT, forwarded straight into
    //                `WM_SETFONT` (msg 0x30) on a child control.
    //   #54 0x119dc  (hdc, lprc, 0, n) — shell draw helper called
    //                between `GetClientRect` and `EndPaint`. Purely
    //                decorative; a no-op leaves the board intact.
    //   #56 0x11dd4  SHInitDialog(SHINITDLGINFO*) with
    //                {dwMask=1, hDlg, dwFlags=0xd} — NOT SHFullScreen.
    //   #74 0x117a8  SHCreateMenuBar(SHMENUBARINFO*): the caller reads
    //                back +0x1c and stores it as the menu bar HWND, so
    //                we have to write a handle there.
    d.register_handler(dll, "ord:4", sh_sip_info);
    d.register_handler(dll, "ord:9", ok);
    d.register_handler(dll, "ord:22", ok);
    d.register_handler(dll, "ord:32", sh_message_box_check);
    d.register_handler(dll, "ord:33", zero);
    d.register_handler(dll, "ord:53", sh_get_ui_font);
    d.register_handler(dll, "ord:54", ok);
    d.register_handler(dll, "ord:56", ok);
    d.register_handler(dll, "ord:74", sh_create_menu_bar);
}

fn ok(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn zero(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `SHCreateMenuBar(SHMENUBARINFO *pmb)`.
///
/// The PPC2002 struct is 36 bytes:
///
/// ```text
///   +0x00 cbSize        (36)
///   +0x04 hwndParent
///   +0x08 dwFlags
///   +0x0c nToolBarId
///   +0x10 hInstRes
///   +0x14 nBmpId
///   +0x18 cBmpImages
///   +0x1c hwndMB        <- out
///   +0x20 clrBk
/// ```
///
/// We have no real shell menu bar, but the caller stores `hwndMB` in a
/// global and `SendMessageW`s to it later, so leaving it NULL would
/// route every menu message to the default window proc. Hand back a
/// dedicated fake HWND instead.
fn sh_create_menu_bar(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pmb = ctx.arg_u32(0)?;
    if pmb == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if pmb >= 0xDEAD_0000 {
        log::debug!(
            "SHCreateMenuBar received HWND-like argument 0x{pmb:08x}; treating it as the legacy shell fullscreen call"
        );
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let cb_size = match ctx.cpu.read_u32_le(pmb) {
        Ok(size) => size,
        Err(_) => {
            log::debug!(
                "SHCreateMenuBar received non-pointer argument 0x{pmb:08x}; treating it as an alternate shell call"
            );
            return Ok(DispatchOutcome::ReturnedR0(1));
        }
    };
    // The 32-byte (WM5) layout puts hwndMB at +0x18, the 36-byte
    // (PPC2002) one at +0x1c. Pick by cbSize; for an unrecognised
    // size write both so the caller finds a handle either way.
    let offsets: &[u32] = match cb_size {
        32 => &[0x18],
        36 => &[0x1c],
        _ => &[0x18, 0x1c],
    };
    for off in offsets {
        ctx.cpu
            .write_mem(pmb + off, &FAKE_MENUBAR_HWND.to_le_bytes())?;
    }
    log::debug!("SHCreateMenuBar(cbSize={cb_size}) -> hwndMB=0x{FAKE_MENUBAR_HWND:08x}");
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// `SHSipInfo(dwAction, uParam, pvParam, fWinIni)`.
///
/// The SIP is never visible under PocketHLE, so report a panel that is
/// off and takes no screen space: `fdwFlags` clear, `rcVisibleDesktop`
/// the whole framebuffer, `rcSipRect` empty. Solitaire reads the flags
/// back, flips `SIPF_ON`, and calls in again with `SPI_SETSIPINFO`;
/// succeeding and ignoring the set keeps the round-trip harmless.
fn sh_sip_info(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let action = ctx.arg_u32(0)?;
    let pv = ctx.arg_u32(2)?;
    if action == SPI_GETSIPINFO && pv != 0 {
        // SIPINFO { cbSize, fdwFlags, rcVisibleDesktop[4],
        //           rcSipRect[4], dwImDataSize, pvImData } = 48 bytes.
        let cb_size = ctx.cpu.read_u32_le(pv).unwrap_or(48);
        let (w, h) = (
            ctx.kernel.framebuffer.width as i32,
            ctx.kernel.framebuffer.height as i32,
        );
        let mut buf = [0u8; 48];
        buf[0..4].copy_from_slice(&cb_size.to_le_bytes());
        buf[8..12].copy_from_slice(&0i32.to_le_bytes());
        buf[12..16].copy_from_slice(&0i32.to_le_bytes());
        buf[16..20].copy_from_slice(&w.to_le_bytes());
        buf[20..24].copy_from_slice(&h.to_le_bytes());
        // Never write past what the caller declared, and never past
        // our own buffer even if cbSize is garbage.
        let len = (cb_size as usize).clamp(24, buf.len());
        ctx.cpu.write_mem(pv, &buf[..len])?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

/// aygshell's shell-owned message box. The PPC2002 Solitaire call site
/// compares the result against `IDYES` (6).
///
/// `IDNO` is the conservative answer: it's what a user gets by
/// dismissing the prompt, and it avoids the destructive branch (the
/// one call site guards a "start over?" style question).
fn sh_message_box_check(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const IDNO: u32 = 7;
    Ok(DispatchOutcome::ReturnedR0(IDNO))
}

/// Returns the shell UI font. The caller forwards it straight into
/// `WM_SETFONT`, and our GDI layer maps an unknown font handle onto
/// the system font anyway, so the stock handle is exactly right.
fn sh_get_ui_font(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(STOCK_SYSTEM_FONT))
}
