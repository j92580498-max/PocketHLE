//! GPS1 packed stream ABI. Geofence/APM commands remain explicitly unsupported.
use crate::CallCtx;
use pocket_cpu::Prot;
use pocket_kernel::{gps, DispatchOutcome, KernelError};
fn fail(ctx: &mut CallCtx<'_>, e: u32) -> Result<DispatchOutcome, KernelError> {
    crate::coredll::set_thread_error(ctx, e);
    Ok(DispatchOutcome::ReturnedR0(0))
}
fn probe(ctx: &mut CallCtx<'_>, p: u32, n: u32, prot: Prot) -> bool {
    p != 0 && p.checked_add(n).is_some() && ctx.cpu.check_guest_access(p, n, prot).is_ok()
}
pub(crate) fn read_file(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let out = ctx.arg_u32(3)?;
    let overlapped = ctx.arg_u32(4)?;
    let Some(open) = ctx.kernel.vfs.gps_open(h) else {
        return fail(ctx, 6);
    };
    if out != 0 && !probe(ctx, out, 4, Prot::WRITE) {
        return fail(ctx, 87);
    }
    if out != 0 {
        ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
    }
    if overlapped != 0 {
        return fail(ctx, 50);
    }
    if open.access & 1 == 0 {
        return fail(ctx, 5);
    }
    if n != gps::PACKET_SIZE as u32 {
        return fail(ctx, 87);
    }
    if !probe(ctx, p, n, Prot::WRITE) {
        return fail(ctx, 87);
    }
    let result = open.device.lock().unwrap().packet();
    let bytes = match result {
        Ok(b) => b,
        Err(e) => return fail(ctx, e),
    };
    ctx.cpu.write_mem(p, &bytes)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &n.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}
pub(crate) fn write_file(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    let n = ctx.arg_u32(2)?;
    let out = ctx.arg_u32(3)?;
    if out != 0 && !probe(ctx, out, 4, Prot::WRITE) {
        return fail(ctx, 87);
    }
    if out != 0 {
        ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
    }
    let Some(open) = ctx.kernel.vfs.gps_open(h) else {
        return fail(ctx, 6);
    };
    if open.access & 2 == 0 {
        return fail(ctx, 5);
    }
    if n != 21 || !probe(ctx, p, n, Prot::READ) {
        return fail(ctx, 87);
    }
    fail(ctx, 50)
}
pub(crate) fn control(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let code = ctx.arg_u32(1)?;
    let returned = ctx.arg_u32(6)?;
    if returned != 0 && !probe(ctx, returned, 4, Prot::WRITE) {
        return fail(ctx, 87);
    }
    if returned != 0 {
        ctx.cpu.write_mem(returned, &0u32.to_le_bytes())?;
    }
    // SDK exposes version/restart but doesn't document their buffer contracts.
    // Don't report fictitious success or reset the PC's physical receiver.
    log::debug!("GPS1: unsupported IOCTL {code:#010x}");
    fail(ctx, 50)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth::tests::{call, setup};
    use pocket_cpu::Cpu;
    use pocket_kernel::gps::{Backend, Capture, Position, Service};
    use std::sync::Arc;
    struct Fake;
    struct Stream;
    impl Backend for Fake {
        fn start(&self) -> gps::Result<Box<dyn Capture>> {
            Ok(Box::new(Stream))
        }
    }
    impl Capture for Stream {
        fn latest(&mut self) -> gps::Result<Option<Position>> {
            Ok(None)
        }
    }
    #[test]
    fn arm_api_no_fix_pointer_access_size_errors_and_unsupported_commands() {
        let (mut cpu, mut k, mut d) = setup();
        let s = Service::with_backend(Arc::new(Fake));
        s.set_allowed(true);
        k.vfs.set_gps_service(s);
        let h = k.vfs.open_file("GPS1:", 1, 0, 3, false, 0).unwrap().handle;
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "ReadFile",
                &[h, 0x1400, 180, 0x1200, 0]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 180);
        assert_eq!(cpu.read_mem(0x1400 + 11, 4).unwrap(), vec![0; 4]);
        for (p, n, e) in [(0, 180, 87), (0x1400, 179, 87), (0xfffffff0, 180, 87)] {
            assert_eq!(
                call(
                    &mut cpu,
                    &mut k,
                    &mut d,
                    "coredll.dll",
                    "ReadFile",
                    &[h, p, n, 0x1200, 0]
                ),
                DispatchOutcome::ReturnedR0(0)
            );
            assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 0);
            assert_eq!(k.thread_last_errors.get(&0), Some(&e));
        }
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "WriteFile",
                &[h, 0x1400, 21, 0x1200, 0]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(k.thread_last_errors.get(&0), Some(&5));
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "DeviceIoControl",
                &[h, 0x8000200c, 0, 0, 0x1400, 180, 0x1200, 0]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(k.thread_last_errors.get(&0), Some(&50));
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "coredll.dll", "CloseHandle", &[h]),
            DispatchOutcome::ReturnedR0(1)
        );
        assert!(!k.vfs.is_handle(h));
    }
}
