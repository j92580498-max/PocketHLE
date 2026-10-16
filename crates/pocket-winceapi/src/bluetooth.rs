//! CE stream-device contract used by the Gizmondo Bluetooth SDK sample.
use crate::{CallCtx, WinCeDispatcher};
use pocket_cpu::Prot;
use pocket_kernel::{
    bluetooth::{PortParams, WOULD_BLOCK},
    DispatchOutcome, KernelError,
};

pub fn register(d: &mut WinCeDispatcher) {
    for (name, handler) in [
        ("RegisterDevice", register_device as crate::Handler),
        ("DeregisterDevice", deregister_device),
        ("SetCommMask", set_mask),
        ("GetCommMask", get_mask),
        ("WaitCommEvent", wait_event),
    ] {
        d.register_handler("coredll.dll", name, handler);
    }
}
fn failed(ctx: &mut CallCtx<'_>, error: u32) -> Result<DispatchOutcome, KernelError> {
    finish_wait(ctx)?;
    let key = (
        ctx.kernel.current_thread,
        ctx.thunk.thunk_va,
        ctx.cpu.read_reg(pocket_cpu::regs::ArmReg::Sp)?,
    );
    ctx.kernel.vfs.bluetooth.writes.remove(&key);
    crate::coredll::set_thread_error(ctx, error);
    Ok(DispatchOutcome::ReturnedR0(0))
}
pub(crate) fn finish_wait(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    let key = (
        ctx.kernel.current_thread,
        ctx.thunk.thunk_va,
        ctx.cpu.read_reg(pocket_cpu::regs::ArmReg::Sp)?,
    );
    ctx.kernel.wait_deadlines.remove(&key);
    Ok(())
}
pub(crate) fn retry(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let outcome = crate::coredll::wait_display_until(
        ctx,
        std::time::Instant::now() + std::time::Duration::from_millis(1),
    )?;
    Ok(outcome.unwrap_or(DispatchOutcome::JumpTo(ctx.thunk.thunk_va)))
}
fn output(ctx: &mut CallCtx<'_>, ptr: u32, size: u32) -> bool {
    ptr != 0 && ctx.cpu.check_guest_access(ptr, size, Prot::WRITE).is_ok()
}
fn wide(ctx: &mut CallCtx<'_>, ptr: u32, max: u32) -> Result<String, u32> {
    if ptr == 0 {
        return Err(87);
    }
    let mut text = Vec::new();
    for i in 0..max {
        let at = ptr.checked_add(i * 2).ok_or(87u32)?;
        if ctx.cpu.check_guest_access(at, 2, Prot::READ).is_err() {
            return Err(87);
        }
        let bytes = ctx.cpu.read_mem(at, 2).map_err(|_| 87u32)?;
        let c = u16::from_le_bytes(bytes.try_into().unwrap());
        if c == 0 {
            return Ok(String::from_utf16_lossy(&text));
        }
        text.push(c);
    }
    Err(87)
}
fn register_device(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let prefix = ctx.arg_u32(0)?;
    let index = ctx.arg_u32(1)?;
    let driver = ctx.arg_u32(2)?;
    let params = ctx.arg_u32(3)?;
    let prefix = match wide(ctx, prefix, 16) {
        Ok(s) => s,
        Err(e) => return failed(ctx, e),
    };
    let driver = match wide(ctx, driver, 260) {
        Ok(s) => s,
        Err(e) => return failed(ctx, e),
    };
    if !prefix.eq_ignore_ascii_case("COM") || !driver.eq_ignore_ascii_case("btd.dll") {
        return failed(ctx, 50);
    }
    if params == 0 || ctx.cpu.check_guest_access(params, 56, Prot::READ).is_err() {
        return failed(ctx, 87);
    }
    let bytes = ctx.cpu.read_mem(params, 56)?;
    let word = |i| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
    if word(4) > 1 || word(0) > 30 || word(52) & !15 != 0 {
        return failed(ctx, 87);
    }
    // Non-default MTU/quota requests need real serial buffering semantics.
    if [16, 20, 24, 28, 32].iter().any(|i| word(*i) != 0) {
        return failed(ctx, 50);
    }
    let p = PortParams {
        server: word(4) != 0,
        address: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        channel: word(0),
        uuid: bytes[36..52].try_into().unwrap(),
        flags: word(52),
    };
    if !p.server && p.address == 0 {
        return failed(ctx, 87);
    }
    match ctx.kernel.vfs.bluetooth.service.register(index, &p) {
        Ok(h) => Ok(DispatchOutcome::ReturnedR0(h)),
        Err(e) => failed(ctx, e),
    }
}
fn deregister_device(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    if ctx.kernel.vfs.bluetooth.service.deregister(h) {
        Ok(DispatchOutcome::ReturnedR0(1))
    } else {
        failed(ctx, 6)
    }
}
fn set_mask(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let mask = ctx.arg_u32(1)?;
    let Some(open) = ctx.kernel.vfs.bluetooth_open(h) else {
        return failed(ctx, 6);
    };
    if mask & !1 != 0 {
        return failed(ctx, 50);
    }
    open.port
        .mask
        .store(mask, std::sync::atomic::Ordering::Release);
    Ok(DispatchOutcome::ReturnedR0(1))
}
fn get_mask(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    let Some(open) = ctx.kernel.vfs.bluetooth_open(h) else {
        return failed(ctx, 6);
    };
    if !output(ctx, ptr, 4) {
        return failed(ctx, 87);
    }
    ctx.cpu.write_mem(
        ptr,
        &open
            .port
            .mask
            .load(std::sync::atomic::Ordering::Acquire)
            .to_le_bytes(),
    )?;
    Ok(DispatchOutcome::ReturnedR0(1))
}
fn wait_event(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    finish_wait(ctx)?;
    let h = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    let overlapped = ctx.arg_u32(2)?;
    let Some(open) = ctx.kernel.vfs.bluetooth_open(h) else {
        return failed(ctx, 6);
    };
    if !output(ctx, ptr, 4) {
        return failed(ctx, 87);
    }
    if overlapped != 0 {
        return failed(ctx, 50);
    }
    let mask = open.port.mask.load(std::sync::atomic::Ordering::Acquire);
    if mask != 0 {
        match open.port.readable() {
            Ok(true) => {}
            Ok(false) | Err(WOULD_BLOCK) => return retry(ctx),
            Err(e) => return failed(ctx, e),
        }
    }
    ctx.cpu.write_mem(ptr, &mask.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(1))
}
pub(crate) fn read_file(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    finish_wait(ctx)?;
    let h = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    let len = ctx.arg_u32(2)?;
    let done = ctx.arg_u32(3)?;
    let Some(open) = ctx.kernel.vfs.bluetooth_open(h) else {
        return failed(ctx, 6);
    };
    if ctx.arg_u32(4)? != 0 {
        return failed(ctx, 50);
    }
    if open.access & 1 == 0 {
        return failed(ctx, 5);
    }
    if (len != 0 && !output(ctx, ptr, len)) || (done != 0 && !output(ctx, done, 4)) {
        return failed(ctx, 87);
    }
    if len > 1024 * 1024 {
        return failed(ctx, 8);
    }
    let mut bytes = vec![0; len as usize];
    match open.port.read(&mut bytes) {
        Ok(n) => {
            if n != 0 {
                ctx.cpu.write_mem(ptr, &bytes[..n])?;
            }
            if done != 0 {
                ctx.cpu.write_mem(done, &(n as u32).to_le_bytes())?;
            }
            Ok(DispatchOutcome::ReturnedR0(1))
        }
        Err(WOULD_BLOCK) => retry(ctx),
        Err(e) => failed(ctx, e),
    }
}
pub(crate) fn write_file(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    finish_wait(ctx)?;
    let h = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    let len = ctx.arg_u32(2)?;
    let done = ctx.arg_u32(3)?;
    let Some(open) = ctx.kernel.vfs.bluetooth_open(h) else {
        return failed(ctx, 6);
    };
    if ctx.arg_u32(4)? != 0 {
        return failed(ctx, 50);
    }
    if open.access & 2 == 0 {
        return failed(ctx, 5);
    }
    if (len != 0 && (ptr == 0 || ctx.cpu.check_guest_access(ptr, len, Prot::READ).is_err()))
        || (done != 0 && !output(ctx, done, 4))
    {
        return failed(ctx, 87);
    }
    if len > 1024 * 1024 {
        return failed(ctx, 8);
    }
    if len == 0 {
        if done != 0 {
            ctx.cpu.write_mem(done, &0u32.to_le_bytes())?;
        }
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let key = (
        ctx.kernel.current_thread,
        ctx.thunk.thunk_va,
        ctx.cpu.read_reg(pocket_cpu::regs::ArmReg::Sp)?,
    );
    let mut pending = match ctx.kernel.vfs.bluetooth.writes.remove(&key) {
        Some(p) if p.handle == h && p.pointer == ptr && p.bytes.len() == len as usize => p,
        Some(_) => return failed(ctx, 87),
        None => pocket_kernel::bluetooth::PendingWrite {
            handle: h,
            pointer: ptr,
            bytes: ctx.cpu.read_mem(ptr, len)?,
            offset: 0,
        },
    };
    while pending.offset < pending.bytes.len() {
        match open.port.write(&pending.bytes[pending.offset..]) {
            Ok(0) => return failed(ctx, 29),
            Ok(n) => pending.offset += n,
            Err(WOULD_BLOCK) => {
                ctx.kernel.vfs.bluetooth.writes.insert(key, pending);
                return retry(ctx);
            }
            Err(e) => {
                if done != 0 {
                    ctx.cpu
                        .write_mem(done, &(pending.offset as u32).to_le_bytes())?;
                }
                return failed(ctx, e);
            }
        }
    }
    if done != 0 {
        ctx.cpu.write_mem(done, &len.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(1))
}

pub(crate) fn driver_unsupported_stream(
    ctx: &mut CallCtx<'_>,
) -> Result<DispatchOutcome, KernelError> {
    let returned = ctx.arg_u32(3)?;
    if returned != 0 {
        if !output(ctx, returned, 4) {
            return failed(ctx, 87);
        }
        ctx.cpu.write_mem(returned, &0u32.to_le_bytes())?;
    }
    failed(ctx, 50)
}

/// Chicane's actual BTH1 power controls, reconstructed from its ARM initializer.
/// These start/stop the emulated driver, never forcibly change host radio power.
pub(crate) fn driver_control(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let code = ctx.arg_u32(1)?;
    let in_len = ctx.arg_u32(3)?;
    let out_len = ctx.arg_u32(5)?;
    let returned = ctx.arg_u32(6)?;
    let driver = ctx.kernel.vfs.bluetooth_driver_open(handle).unwrap();
    // These CTL_CODE values carry FILE_ANY_ACCESS; the hardware permission
    // gate is enforced by Service::start_driver rather than file write access.
    if returned != 0 && !output(ctx, returned, 4) {
        return failed(ctx, 87);
    }
    if in_len != 0 || out_len != 0 {
        return failed(ctx, 87);
    }
    match code {
        0x0101_2120 => {
            if let Err(e) = driver.service.start_driver() {
                return failed(
                    ctx,
                    match e {
                        10091 => 21,
                        10013 => 5,
                        _ => e,
                    },
                );
            }
        }
        0x0101_2124 => driver.service.enable(false),
        _ => return failed(ctx, 50),
    }
    if returned != 0 {
        ctx.cpu.write_mem(returned, &0u32.to_le_bytes())?;
    }
    log::info!(
        "BTH1 driver {}",
        if code == 0x0101_2120 {
            "started"
        } else {
            "stopped"
        }
    );
    Ok(DispatchOutcome::ReturnedR0(1))
}

pub(crate) fn shell_message(ctx: &mut CallCtx<'_>, hwnd: u32, message: u32, value: u32) -> bool {
    let mut hash = 0x811c9dc5u32;
    for byte in b"BT_MSG" {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    if hwnd == 0xffff && message == 0xc000 + hash % 0x4000 {
        ctx.kernel.vfs.bluetooth.service.enable(value != 0);
        true
    } else {
        false
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu};
    use pocket_kernel::bluetooth::{Backend, BtResult, Device, Service, Stream};
    use pocket_kernel::{Dispatcher, KernelState, Thunk};
    use pocket_pe::ImportBinding;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct EchoStream(Arc<Mutex<VecDeque<u8>>>);
    impl Stream for EchoStream {
        fn read(&mut self, b: &mut [u8]) -> BtResult<usize> {
            let mut q = self.0.lock().unwrap();
            if q.is_empty() {
                return Err(WOULD_BLOCK);
            }
            let n = b.len().min(q.len());
            for byte in &mut b[..n] {
                *byte = q.pop_front().unwrap();
            }
            Ok(n)
        }
        fn write(&mut self, b: &[u8]) -> BtResult<usize> {
            self.0.lock().unwrap().extend(b);
            Ok(b.len())
        }
    }
    struct TestBackend;
    impl Backend for TestBackend {
        fn hostname(&self) -> BtResult<String> {
            Ok("BT test host".into())
        }
        fn scan(&self) -> BtResult<Vec<Device>> {
            Ok(vec![Device {
                address: 0x123456789abc,
                name: "Test peer".into(),
            }])
        }
        fn open(&self, _: &PortParams) -> BtResult<Box<dyn Stream>> {
            Ok(Box::new(EchoStream(Arc::new(Mutex::new(VecDeque::new())))))
        }
    }
    pub(crate) fn setup() -> (StubCpu, KernelState, WinCeDispatcher) {
        let mut cpu = StubCpu::new();
        cpu.map_region(0x1000, 0x4000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.write_reg(ArmReg::Sp, 0x4000).unwrap();
        let mut kernel = crate::gx::tests::fresh_kernel();
        let service = Service::with_backend(Arc::new(TestBackend));
        service.set_allowed(true);
        service.enable(true);
        kernel.vfs.set_bluetooth_service(service);
        (cpu, kernel, WinCeDispatcher::new())
    }
    pub(crate) fn call(
        cpu: &mut StubCpu,
        kernel: &mut KernelState,
        dispatcher: &mut WinCeDispatcher,
        dll: &str,
        name: &str,
        args: &[u32],
    ) -> DispatchOutcome {
        for (i, v) in args.iter().enumerate() {
            if i < 4 {
                cpu.write_reg([ArmReg::R0, ArmReg::R1, ArmReg::R2, ArmReg::R3][i], *v)
                    .unwrap();
            } else {
                cpu.write_mem(0x4000 + (i as u32 - 4) * 4, &v.to_le_bytes())
                    .unwrap();
            }
        }
        let thunk = Thunk {
            thunk_va: 0x70000000,
            iat_va: 0,
            dll: dll.into(),
            binding: ImportBinding::Name(name.into()),
            friendly_name: Some(name.into()),
        };
        dispatcher.dispatch(cpu, &thunk, kernel).unwrap()
    }
    fn value(outcome: DispatchOutcome) -> u32 {
        let DispatchOutcome::ReturnedR0(v) = outcome else {
            panic!("unexpected retry")
        };
        v
    }
    #[test]
    fn chicane_bth1_startup_shutdown_errors_and_shared_handle_lifetime() {
        let (mut cpu, mut k, mut d) = setup();
        let path: Vec<_> = "BTH1:"
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        cpu.write_mem(0x1000, &path).unwrap();
        let h = value(call(
            &mut cpu,
            &mut k,
            &mut d,
            "coredll.dll",
            "CreateFileW",
            &[0x1000, 0xc0000000, 0, 0, 3, 0, 0],
        ));
        assert_ne!(h, u32::MAX);
        // Exact no-input/no-output IOCTL ABI in Chicane's ARM initializer.
        k.vfs.bluetooth.service.enable(false);
        k.thread_last_errors.insert(0, 0x1234);
        assert_eq!(
            value(call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "DeviceIoControl",
                &[h, 0x01012120, 0, 0, 0, 0, 0x1200, 0]
            )),
            1
        );
        assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 0);
        assert_eq!(k.thread_last_errors[&0], 0x1234);
        assert!(k.vfs.bluetooth.service.active_backend().is_ok());
        let object = k.vfs.export_handle(h).unwrap();
        let mut child = pocket_kernel::vfs::Vfs::new();
        child.attach_shared_context(k.vfs.shared_context());
        assert!(child.import_handle(0xdead0100, object));
        assert!(k.vfs.close(h));
        // CloseHandle alone must not power off Bluetooth. The SDK stops explicitly.
        assert!(child
            .bluetooth_driver_open(0xdead0100)
            .unwrap()
            .service
            .active_backend()
            .is_ok());
        assert_eq!(k.vfs.open_file("BTH1:", 3, 0, 3, false, 0), Err(32));
        assert!(child.close(0xdead0100));
        let h = value(call(
            &mut cpu,
            &mut k,
            &mut d,
            "coredll.dll",
            "CreateFileW",
            &[0x1000, 0xc0000000, 0, 0, 3, 0, 0],
        ));
        assert_ne!(h, u32::MAX);
        assert_eq!(
            value(call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "DeviceIoControl",
                &[h, 0x01012124, 0, 0, 0, 0, 0x1200, 0]
            )),
            1
        );
        assert!(k.vfs.bluetooth.service.active_backend().is_err());
        k.vfs.bluetooth.service.set_allowed(false);
        assert_eq!(
            value(call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "DeviceIoControl",
                &[h, 0x01012120, 0, 0, 0, 0, 0, 0]
            )),
            0
        );
        assert_eq!(k.thread_last_errors[&0], 5);
        assert_eq!(
            value(call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "DeviceIoControl",
                &[h, 0xffff, 0, 0, 0, 0, 0, 0]
            )),
            0
        );
        assert_eq!(k.thread_last_errors[&0], 50);
        assert_eq!(
            value(call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "DeviceIoControl",
                &[h, 0x01012120, 0, 0, 0, 0, 0x80000000, 0]
            )),
            0
        );
        assert_eq!(k.thread_last_errors[&0], 87);
        assert_eq!(
            value(call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "ReadFile",
                &[h, 0x1300, 4, 0x1200, 0]
            )),
            0
        );
        assert_eq!(k.thread_last_errors[&0], 50);
        k.vfs.close_all();
        child.close_all();
        assert!(k.vfs.open_handles().is_empty());
    }
    #[test]
    fn bth1_start_reports_backend_failure_instead_of_fake_success() {
        let (mut cpu, mut k, mut d) = setup();
        let s = Service::offline();
        s.set_allowed(true);
        k.vfs.set_bluetooth_service(s);
        let h = k.vfs.open_file("BTH1:", 3, 0, 3, false, 0).unwrap().handle;
        assert_eq!(
            value(call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "DeviceIoControl",
                &[h, 0x01012120, 0, 0, 0, 0, 0, 0]
            )),
            0
        );
        assert_eq!(k.thread_last_errors[&0], 21);
        k.vfs.close_all();
    }
    #[test]
    fn sdk_com_sequence_preserves_pending_data_and_cancels_duplicate_handles() {
        let (mut cpu, mut k, mut d) = setup();
        for (ptr, text) in [(0x1000, "COM"), (0x1020, "btd.dll"), (0x1040, "COM4:")] {
            let bytes: Vec<_> = text
                .encode_utf16()
                .chain(Some(0))
                .flat_map(u16::to_le_bytes)
                .collect();
            cpu.write_mem(ptr, &bytes).unwrap();
        }
        let mut p = [0; 56];
        p[..4].copy_from_slice(&2u32.to_le_bytes());
        p[4..8].copy_from_slice(&1u32.to_le_bytes());
        cpu.write_mem(0x1100, &p).unwrap();
        let registered = value(call(
            &mut cpu,
            &mut k,
            &mut d,
            "coredll.dll",
            "RegisterDevice",
            &[0x1000, 4, 0x1020, 0x1100],
        ));
        assert_ne!(registered, 0);
        let h = value(call(
            &mut cpu,
            &mut k,
            &mut d,
            "coredll.dll",
            "CreateFileW",
            &[0x1040, 0xc0000000, 0, 0, 3, 0, 0],
        ));
        assert_ne!(h, u32::MAX);
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "SetCommMask",
                &[h, 1]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "WaitCommEvent",
                &[h, 0x1200, 0]
            ),
            DispatchOutcome::JumpTo(0x70000000)
        );
        cpu.write_mem(0x1300, b"packet").unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "WriteFile",
                &[h, 0x1300, 6, 0x1204, 0]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(cpu.read_u32_le(0x1204).unwrap(), 6);
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "WaitCommEvent",
                &[h, 0x1200, 0]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 1);
        // Reject a bad output before consuming the bytes buffered by WaitCommEvent.
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "ReadFile",
                &[h, 0, 6, 0x1204, 0]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "ReadFile",
                &[h, 0x1400, 6, 0x1204, 0]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(cpu.read_mem(0x1400, 6).unwrap(), b"packet");
        assert!(k.wait_deadlines.is_empty());
        let mut child = pocket_kernel::vfs::Vfs::new();
        child.attach_shared_context(k.vfs.shared_context());
        assert!(child.import_handle(99, k.vfs.export_handle(h).unwrap()));
        k.vfs.close(h);
        assert_eq!(
            child.bluetooth_open(99).unwrap().port.write(b"child"),
            Ok(5)
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "DeregisterDevice",
                &[registered]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(
            child.bluetooth_open(99).unwrap().port.read(&mut [0; 8]),
            Err(995)
        );
        // SDK DestroyClient deregisters without closing its old COM handle.
        // A later registration at COM4 must not inherit the old sharing lease.
        let params = PortParams {
            server: true,
            address: 0,
            channel: 2,
            uuid: [0; 16],
            flags: 0,
        };
        let next = k.vfs.bluetooth.service.register(4, &params).unwrap();
        let fresh = k.vfs.open_file("COM4:", 3, 0, 3, false, 0).unwrap().handle;
        assert_eq!(
            k.vfs.bluetooth_open(fresh).unwrap().port.write(b"next"),
            Ok(4)
        );
        k.vfs.close(fresh);
        k.vfs.bluetooth.service.deregister(next);
        assert!(child.close(99));
    }
    #[test]
    fn serial_write_retries_keep_the_offset_and_do_not_resend_prefix_bytes() {
        struct Partial {
            bytes: Arc<Mutex<Vec<u8>>>,
            calls: usize,
        }
        impl Stream for Partial {
            fn read(&mut self, _: &mut [u8]) -> BtResult<usize> {
                Err(WOULD_BLOCK)
            }
            fn write(&mut self, b: &[u8]) -> BtResult<usize> {
                self.calls += 1;
                if self.calls == 2 {
                    return Err(WOULD_BLOCK);
                }
                let n = if self.calls == 1 {
                    b.len().min(2)
                } else {
                    b.len()
                };
                self.bytes.lock().unwrap().extend_from_slice(&b[..n]);
                Ok(n)
            }
        }
        struct PartialBackend(Arc<Mutex<Vec<u8>>>);
        impl Backend for PartialBackend {
            fn hostname(&self) -> BtResult<String> {
                Ok("partial".into())
            }
            fn scan(&self) -> BtResult<Vec<Device>> {
                Ok(Vec::new())
            }
            fn open(&self, _: &PortParams) -> BtResult<Box<dyn Stream>> {
                Ok(Box::new(Partial {
                    bytes: self.0.clone(),
                    calls: 0,
                }))
            }
        }
        let (mut cpu, mut k, mut d) = setup();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let service = Service::with_backend(Arc::new(PartialBackend(sent.clone())));
        service.set_allowed(true);
        service.enable(true);
        service
            .register(
                4,
                &PortParams {
                    server: true,
                    address: 0,
                    channel: 2,
                    uuid: [0; 16],
                    flags: 0,
                },
            )
            .unwrap();
        k.vfs.set_bluetooth_service(service);
        let h = k.vfs.open_file("COM4:", 3, 0, 3, false, 0).unwrap().handle;
        cpu.write_mem(0x1300, b"abcdef").unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "WriteFile",
                &[h, 0x1300, 6, 0x1200, 0]
            ),
            DispatchOutcome::JumpTo(0x70000000)
        );
        assert_eq!(&*sent.lock().unwrap(), b"ab");
        cpu.write_mem(0x1300, b"XXXXXX").unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "WriteFile",
                &[h, 0x1300, 6, 0x1200, 0]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(&*sent.lock().unwrap(), b"abcdef");
        assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 6);
        assert!(k.vfs.bluetooth.writes.is_empty());
        assert!(k.wait_deadlines.is_empty());
    }

    #[test]
    fn disabled_hardware_and_mask_reset_do_not_fake_data() {
        let (mut cpu, mut k, mut d) = setup();
        let params = PortParams {
            server: true,
            address: 0,
            channel: 2,
            uuid: [0; 16],
            flags: 0,
        };
        k.vfs.bluetooth.service.set_allowed(false);
        assert_eq!(k.vfs.bluetooth.service.register(4, &params), Err(10091));
        k.vfs.bluetooth.service.set_allowed(true);
        k.vfs.bluetooth.service.enable(true);
        let registered = k.vfs.bluetooth.service.register(4, &params).unwrap();
        let h = k.vfs.open_file("COM4:", 3, 0, 3, false, 0).unwrap().handle;
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "SetCommMask",
                &[h, 0]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "WaitCommEvent",
                &[h, 0x1200, 0]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 0);
        k.vfs.bluetooth.service.deregister(registered);
    }
}
