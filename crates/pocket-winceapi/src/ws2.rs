//! CE Winsock startup, Bluetooth inquiry and RFCOMM socket operations.
use crate::{CallCtx, WinCeDispatcher};
mod sockets;
use pocket_kernel::{DispatchOutcome, KernelError};

const SOCKET_ERROR: u32 = u32::MAX;
const WSASYSNOTREADY: u32 = 10091;
const WSANOTINITIALISED: u32 = 10093;

pub fn register(dispatcher: &mut WinCeDispatcher) {
    sockets::register(dispatcher);
    dispatcher.register_handler("ws2.dll", "WSAStartup", startup);
    dispatcher.register_handler("ws2.dll", "WSACleanup", cleanup);
    dispatcher.register_handler("ws2.dll", "WSAGetLastError", get_last_error);
    dispatcher.register_handler("ws2.dll", "WSASetLastError", set_last_error);
    dispatcher.register_handler("ws2.dll", "gethostname", hostname);
    for name in ["WSALookupServiceBeginW", "WSALookupServiceBegin"] {
        dispatcher.register_handler("ws2.dll", name, lookup_begin);
    }
    for name in ["WSALookupServiceNextW", "WSALookupServiceNext"] {
        dispatcher.register_handler("ws2.dll", name, lookup_next);
    }
    dispatcher.register_handler("ws2.dll", "WSALookupServiceEnd", lookup_end);
}

fn startup(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.vfs.bluetooth.service.backend().is_err() {
        return Ok(DispatchOutcome::ReturnedR0(WSASYSNOTREADY));
    }
    let version = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    if !matches!(version, 0x0101 | 0x0002 | 0x0102 | 0x0202) {
        return Ok(DispatchOutcome::ReturnedR0(10092));
    }
    if ptr == 0
        || ctx
            .cpu
            .check_guest_access(ptr, 400, pocket_cpu::Prot::WRITE)
            .is_err()
    {
        return Ok(DispatchOutcome::ReturnedR0(10014));
    }
    if ctx.kernel.vfs.bluetooth.startups == u32::MAX {
        return Ok(DispatchOutcome::ReturnedR0(10055));
    }
    let mut data = [0u8; 400];
    data[..2].copy_from_slice(&(version as u16).to_le_bytes());
    data[2..4].copy_from_slice(&0x0202u16.to_le_bytes());
    let label = b"PocketHLE Bluetooth RFCOMM";
    data[4..4 + label.len()].copy_from_slice(label);
    ctx.cpu.write_mem(ptr, &data)?;
    ctx.kernel.vfs.bluetooth.startups += 1;
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn cleanup(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.vfs.bluetooth.startups == 0 {
        return not_initialized(ctx);
    }
    ctx.kernel.vfs.bluetooth.startups -= 1;
    if ctx.kernel.vfs.bluetooth.startups == 0 {
        ctx.kernel.vfs.bluetooth.lookups.clear();
        ctx.kernel.vfs.bluetooth.sockets.clear();
        ctx.kernel.vfs.bluetooth.socket_deadlines.clear();
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}
fn fail(ctx: &mut CallCtx<'_>, code: u32) -> Result<DispatchOutcome, KernelError> {
    ctx.kernel
        .winsock_last_errors
        .insert(ctx.kernel.current_thread, code);
    Ok(DispatchOutcome::ReturnedR0(SOCKET_ERROR))
}
fn hostname(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.vfs.bluetooth.startups == 0 {
        return not_initialized(ctx);
    }
    let ptr = ctx.arg_u32(0)?;
    let len = ctx.arg_u32(1)?;
    let name = match ctx
        .kernel
        .vfs
        .bluetooth
        .service
        .backend()
        .and_then(|b| b.hostname())
    {
        Ok(n) => n,
        Err(e) => return fail(ctx, e),
    };
    let mut bytes: Vec<u8> = name
        .chars()
        .map(|c| if c.is_ascii() { c as u8 } else { b'?' })
        .collect();
    bytes.push(0);
    if ptr == 0
        || len < bytes.len() as u32
        || ctx
            .cpu
            .check_guest_access(ptr, bytes.len() as u32, pocket_cpu::Prot::WRITE)
            .is_err()
    {
        return fail(ctx, 10014);
    }
    ctx.cpu.write_mem(ptr, &bytes)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}
fn lookup_begin(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.vfs.bluetooth.startups == 0 {
        return not_initialized(ctx);
    }
    let query = ctx.arg_u32(0)?;
    let flags = ctx.arg_u32(1)?;
    let output = ctx.arg_u32(2)?;
    if query == 0
        || ctx
            .cpu
            .check_guest_access(query, 60, pocket_cpu::Prot::READ)
            .is_err()
        || output == 0
        || ctx
            .cpu
            .check_guest_access(output, 4, pocket_cpu::Prot::WRITE)
            .is_err()
    {
        return fail(ctx, 10014);
    }
    let q = ctx.cpu.read_mem(query, 60)?;
    if u32::from_le_bytes(q[..4].try_into().unwrap()) != 60
        || u32::from_le_bytes(q[20..24].try_into().unwrap()) != 16
        || flags & 2 == 0
        || flags & !(2 | 0x1000) != 0
    {
        return fail(ctx, 10022);
    }
    if [4, 8, 12, 16, 24, 28, 32, 36, 40, 44, 48, 56]
        .iter()
        .any(|i| q[*i..*i + 4] != [0; 4])
    {
        return fail(ctx, 10045); // Service/filter queries are a separate contract.
    }
    match ctx.kernel.vfs.bluetooth.begin_lookup() {
        Ok(id) => {
            ctx.cpu.write_mem(output, &id.to_le_bytes())?;
            Ok(DispatchOutcome::ReturnedR0(0))
        }
        Err(e) => fail(ctx, e),
    }
}
fn lookup_next(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    crate::bluetooth::finish_wait(ctx)?;
    if ctx.kernel.vfs.bluetooth.startups == 0 {
        return not_initialized(ctx);
    }
    let id = ctx.arg_u32(0)?;
    let flags = ctx.arg_u32(1)?;
    let length = ctx.arg_u32(2)?;
    let ptr = ctx.arg_u32(3)?;
    if flags & !(0x10 | 0x100) != 0 {
        return fail(ctx, 10022);
    }
    if length == 0
        || ctx
            .cpu
            .check_guest_access(length, 4, pocket_cpu::Prot::READ | pocket_cpu::Prot::WRITE)
            .is_err()
    {
        return fail(ctx, 10014);
    }
    let Some(lookup) = ctx.kernel.vfs.bluetooth.lookups.get(&id) else {
        return fail(ctx, 6);
    };
    let result = lookup.result.lock().unwrap().clone();
    let device = match result {
        None => return crate::bluetooth::retry(ctx),
        Some(Err(e)) => return fail(ctx, e),
        Some(Ok(devices)) => match devices.get(lookup.index).cloned() {
            Some(d) => d,
            None => return fail(ctx, 10110),
        },
    };
    let name: Vec<u8> = device
        .name
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    let Some(required) = u32::try_from(name.len())
        .ok()
        .and_then(|n| 124u32.checked_add(n))
    else {
        return fail(ctx, 10055);
    };
    let capacity = ctx.cpu.read_u32_le(length)?;
    if capacity < required || ptr == 0 {
        ctx.cpu.write_mem(length, &required.to_le_bytes())?;
        return fail(ctx, 10014);
    }
    if ptr.checked_add(required).is_none()
        || ctx
            .cpu
            .check_guest_access(ptr, required, pocket_cpu::Prot::WRITE)
            .is_err()
    {
        ctx.cpu.write_mem(length, &required.to_le_bytes())?;
        return fail(ctx, 10014);
    }
    let mut bytes = vec![0; required as usize];
    for (offset, value) in [
        (0, 60),
        (4, ptr + 124),
        (20, 16),
        (44, 1),
        (48, ptr + 60),
        (68, ptr + 84),
        (72, 40),
        (76, 1),
        (80, 3),
    ] {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    bytes[84..86].copy_from_slice(&32u16.to_le_bytes());
    bytes[92..100].copy_from_slice(&device.address.to_le_bytes());
    bytes[124..].copy_from_slice(&name);
    ctx.cpu.write_mem(ptr, &bytes)?;
    ctx.kernel.vfs.bluetooth.lookups.get_mut(&id).unwrap().index += 1;
    Ok(DispatchOutcome::ReturnedR0(0))
}
fn lookup_end(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.vfs.bluetooth.startups == 0 {
        return not_initialized(ctx);
    }
    let id = ctx.arg_u32(0)?;
    if ctx.kernel.vfs.bluetooth.lookups.remove(&id).is_none() {
        fail(ctx, 6)
    } else {
        Ok(DispatchOutcome::ReturnedR0(0))
    }
}

fn not_initialized(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ctx.kernel
        .winsock_last_errors
        .insert(ctx.kernel.current_thread, WSANOTINITIALISED);
    Ok(DispatchOutcome::ReturnedR0(SOCKET_ERROR))
}

fn get_last_error(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        *ctx.kernel
            .winsock_last_errors
            .get(&ctx.kernel.current_thread)
            .unwrap_or(&0),
    ))
}

fn set_last_error(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let error = ctx.arg_u32(0)?;
    ctx.kernel
        .winsock_last_errors
        .insert(ctx.kernel.current_thread, error);
    Ok(DispatchOutcome::ReturnedR0(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu};
    use pocket_kernel::{Dispatcher, Thunk};
    use pocket_pe::ImportBinding;

    #[test]
    fn chicane_inquiry_reuses_capacity_across_increasing_device_names() {
        use crate::bluetooth::tests::{call, setup};
        use pocket_kernel::bluetooth::{Device, Lookup};
        use std::sync::{Arc, Mutex};
        let (mut cpu, mut kernel, mut dispatcher) = setup();
        kernel.vfs.bluetooth.startups = 1;
        kernel.vfs.bluetooth.lookups.insert(
            1,
            Lookup {
                result: Arc::new(Mutex::new(Some(Ok(vec![
                    Device {
                        address: 1,
                        name: "PLT_K100".into(),
                    },
                    Device {
                        address: 2,
                        name: "BlackBerry KEY2".into(),
                    },
                    Device {
                        address: 3,
                        name: "A much longer Bluetooth device name".into(),
                    },
                ])))),
                index: 0,
            },
        );
        // The actual ARM loop initializes this DWORD once, outside the loop.
        cpu.write_mem(0x1204, &1024u32.to_le_bytes()).unwrap();
        for (index, name) in [
            "PLT_K100",
            "BlackBerry KEY2",
            "A much longer Bluetooth device name",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(
                call(
                    &mut cpu,
                    &mut kernel,
                    &mut dispatcher,
                    "ws2.dll",
                    "WSALookupServiceNextW",
                    &[1, 0x110, 0x1204, 0x2000]
                ),
                DispatchOutcome::ReturnedR0(0)
            );
            assert_eq!(cpu.read_u32_le(0x1204).unwrap(), 1024);
            assert_eq!(
                cpu.read_mem(0x205c, 8).unwrap(),
                ((index + 1) as u64).to_le_bytes()
            );
            let expected: Vec<u8> = name
                .encode_utf16()
                .chain(Some(0))
                .flat_map(u16::to_le_bytes)
                .collect();
            assert_eq!(
                cpu.read_mem(0x207c, expected.len() as u32).unwrap(),
                expected
            );
        }
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "WSALookupServiceNextW",
                &[1, 0x110, 0x1204, 0x2000]
            ),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert_eq!(kernel.winsock_last_errors[&0], 10110);
        assert_eq!(cpu.read_u32_le(0x1204).unwrap(), 1024);
    }

    #[test]
    fn bluetooth_inquiry_reports_size_without_consuming_and_writes_guest_pointers() {
        use crate::bluetooth::tests::{call, setup};
        let (mut cpu, mut kernel, mut dispatcher) = setup();
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "WSAStartup",
                &[0x0202, 0x1800]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(cpu.read_mem(0x1800, 4).unwrap(), [2, 2, 2, 2]);
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "WSAStartup",
                &[0x0202, 0]
            ),
            DispatchOutcome::ReturnedR0(10014)
        );
        assert_eq!(kernel.vfs.bluetooth.startups, 1);
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "gethostname",
                &[0x1700, 64]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(cpu.read_mem(0x1700, 13).unwrap(), b"BT test host\0");
        let mut query = [0; 60];
        query[..4].copy_from_slice(&60u32.to_le_bytes());
        query[20..24].copy_from_slice(&16u32.to_le_bytes());
        cpu.write_mem(0x1100, &query).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "WSALookupServiceBeginW",
                &[0x1100, 2, 0x1200]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        let lookup = cpu.read_u32_le(0x1200).unwrap();
        let result = kernel.vfs.bluetooth.lookups[&lookup].result.clone();
        for _ in 0..100 {
            if result.lock().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(result.lock().unwrap().is_some());
        cpu.write_mem(0x1204, &0u32.to_le_bytes()).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "WSALookupServiceNextW",
                &[lookup, 0x110, 0x1204, 0]
            ),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        let required = cpu.read_u32_le(0x1204).unwrap();
        assert!(required > 124);
        assert_eq!(kernel.vfs.bluetooth.lookups[&lookup].index, 0);
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "WSALookupServiceNextW",
                &[lookup, 0x110, 0x1204, 0x2000]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(cpu.read_u32_le(0x2004).unwrap(), 0x207c);
        assert_eq!(cpu.read_u32_le(0x2030).unwrap(), 0x203c);
        assert_eq!(
            cpu.read_mem(0x205c, 8).unwrap(),
            0x123456789abcu64.to_le_bytes()
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "WSALookupServiceNextW",
                &[lookup, 0x110, 0x1204, 0x2000]
            ),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert_eq!(kernel.winsock_last_errors[&0], 10110);
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "WSALookupServiceEnd",
                &[lookup]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "WSACleanup",
                &[]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                &mut dispatcher,
                "ws2.dll",
                "WSACleanup",
                &[]
            ),
            DispatchOutcome::ReturnedR0(u32::MAX)
        );
        assert!(kernel.thread_last_errors.is_empty());
    }

    #[test]
    fn gizmondo_rom_ordinals_resolve_the_offline_handlers() {
        assert_eq!(
            crate::ordinals::lookup("WS2.dll", 35).as_deref(),
            Some("WSAStartup")
        );
        assert_eq!(
            crate::ordinals::lookup("ws2.dll", 71).as_deref(),
            Some("recv")
        );
        assert_eq!(
            crate::ordinals::lookup("btd.dll", 31).as_deref(),
            Some("COM_Open")
        );
        let mut cpu = StubCpu::new();
        let mut kernel = crate::gx::tests::fresh_kernel();
        kernel
            .vfs
            .set_bluetooth_service(pocket_kernel::bluetooth::Service::offline());
        let mut dispatcher = WinCeDispatcher::new();
        let thunk = Thunk {
            thunk_va: 0,
            iat_va: 0,
            dll: "WS2.dll".into(),
            binding: ImportBinding::Ordinal(35),
            friendly_name: None,
        };
        assert_eq!(
            dispatcher.dispatch(&mut cpu, &thunk, &mut kernel).unwrap(),
            DispatchOutcome::ReturnedR0(WSASYSNOTREADY)
        );
    }

    #[test]
    fn offline_recv_and_thread_errors_are_dispatched_without_touching_buffers() {
        let mut cpu = StubCpu::new();
        let mut kernel = crate::gx::tests::fresh_kernel();
        kernel
            .vfs
            .set_bluetooth_service(pocket_kernel::bluetooth::Service::offline());
        let mut dispatcher = WinCeDispatcher::new();
        let call = |name: &str,
                    dispatcher: &mut WinCeDispatcher,
                    cpu: &mut StubCpu,
                    kernel: &mut pocket_kernel::KernelState| {
            let thunk = Thunk {
                thunk_va: 0,
                iat_va: 0,
                dll: "WS2.dll".into(),
                binding: ImportBinding::Name(name.into()),
                friendly_name: Some(name.into()),
            };
            dispatcher.dispatch(cpu, &thunk, kernel).unwrap()
        };
        // Reproduce the demo's NULL receive buffer: no access or fabricated EOF.
        cpu.write_reg(ArmReg::R0, 0).unwrap();
        cpu.write_reg(ArmReg::R1, 0).unwrap();
        cpu.write_reg(ArmReg::R2, 4096).unwrap();
        assert_eq!(
            call("recv", &mut dispatcher, &mut cpu, &mut kernel),
            DispatchOutcome::ReturnedR0(SOCKET_ERROR)
        );
        assert_eq!(
            call("WSAGetLastError", &mut dispatcher, &mut cpu, &mut kernel),
            DispatchOutcome::ReturnedR0(WSANOTINITIALISED)
        );
        assert_eq!(
            call("WSAGetLastError", &mut dispatcher, &mut cpu, &mut kernel),
            DispatchOutcome::ReturnedR0(WSANOTINITIALISED)
        );
        kernel.current_thread = 1;
        assert_eq!(
            call("WSAGetLastError", &mut dispatcher, &mut cpu, &mut kernel),
            DispatchOutcome::ReturnedR0(0)
        );
        cpu.write_reg(ArmReg::R0, 10038).unwrap();
        call("WSASetLastError", &mut dispatcher, &mut cpu, &mut kernel);
        assert_eq!(
            call("WSAGetLastError", &mut dispatcher, &mut cpu, &mut kernel),
            DispatchOutcome::ReturnedR0(10038)
        );
        kernel.current_thread = 0;
        assert_eq!(
            call("WSAStartup", &mut dispatcher, &mut cpu, &mut kernel),
            DispatchOutcome::ReturnedR0(WSASYSNOTREADY)
        );
        assert_eq!(
            call("WSACleanup", &mut dispatcher, &mut cpu, &mut kernel),
            DispatchOutcome::ReturnedR0(SOCKET_ERROR)
        );
        assert_eq!(
            call("WSAGetLastError", &mut dispatcher, &mut cpu, &mut kernel),
            DispatchOutcome::ReturnedR0(WSANOTINITIALISED)
        );
        assert!(kernel.thread_last_errors.is_empty());
    }
}
