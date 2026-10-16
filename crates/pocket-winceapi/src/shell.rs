//! Gizmondo shell messages without inventing SIM, GSM or ActiveSync hardware.
use crate::CallCtx;
pub(crate) fn status(ctx: &CallCtx<'_>) -> u32 {
    pocket_kernel::device_status::status().shell_bits()
        | (u32::from(ctx.kernel.vfs.gps_service().is_enabled()) << 1)
        | (u32::from(ctx.kernel.vfs.bluetooth.service.is_enabled()) << 3)
        | (u32::from(
            ctx.kernel.internet.gprs_connected
                && ctx.kernel.internet.gprs_shell_network_available() == Some(true),
        ) << 4)
}
pub(crate) fn message(ctx: &mut CallCtx<'_>, hwnd: u32, message: u32, value: u32) -> bool {
    if hwnd == 0xffff && message == crate::coredll::registered_window_message_id("GPRS_CONNECT") {
        if let Some(available) = ctx.kernel.internet.gprs_shell_network_available() {
            let connected = available && value != 0;
            ctx.kernel.internet.gprs_connected = connected;
            if ctx.kernel.posted_messages.len() < 256 {
                ctx.kernel.posted_messages.push_back((
                    0xffff,
                    crate::coredll::registered_window_message_id("GPRS_STATUS"),
                    if connected { 0x1f9c } else { 0x1f9d },
                    u32::from(connected),
                ));
            }
            return true;
        }
    }
    if hwnd != 0xffff || message != crate::coredll::registered_window_message_id("GT_GETSTATUS") {
        return false;
    }
    let flags = status(ctx);
    ctx.kernel.shell_status = Some(if ctx.kernel.posted_messages.len() < 256 {
        flags
    } else {
        u32::MAX
    });
    let reply = crate::coredll::registered_window_message_id("GT_STATUS");
    if ctx.kernel.posted_messages.len() < 256 {
        ctx.kernel
            .posted_messages
            .push_back((0xffff, reply, flags, 0));
    }
    true
}

pub(crate) fn service(ctx: &mut CallCtx<'_>) {
    let Some(previous) = ctx.kernel.shell_status else {
        return;
    };
    let flags = status(ctx);
    if flags != previous && ctx.kernel.posted_messages.len() < 256 {
        ctx.kernel.posted_messages.push_back((
            0xffff,
            crate::coredll::registered_window_message_id("GT_STATUS"),
            flags,
            0,
        ));
        ctx.kernel.shell_status = Some(flags);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth::tests::{call, setup};
    use pocket_kernel::DispatchOutcome;
    #[test]
    fn shell_status_tracks_services_without_fabricating_gsm_or_sim() {
        let (mut cpu, mut k, mut d) = setup();
        k.vfs.gps_service().set_allowed(true);
        let request = crate::coredll::registered_window_message_id("GT_GETSTATUS");
        let reply = crate::coredll::registered_window_message_id("GT_STATUS");
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "SendMessageW",
                &[0xffff, request, 0, 0]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(k.posted_messages.pop_front(), Some((0xffff, reply, 10, 0)));
        k.vfs.bluetooth.service.enable(false);
        let thunk = pocket_kernel::Thunk {
            thunk_va: 0,
            iat_va: 0,
            dll: "coredll.dll".into(),
            binding: pocket_pe::ImportBinding::Name("test".into()),
            friendly_name: None,
        };
        let mut ctx = CallCtx {
            cpu: &mut cpu,
            kernel: &mut k,
            thunk: &thunk,
        };
        service(&mut ctx);
        service(&mut ctx);
        assert_eq!(
            ctx.kernel.posted_messages.pop_front(),
            Some((0xffff, reply, 2, 0))
        );
        assert!(ctx.kernel.posted_messages.is_empty());
    }
}
