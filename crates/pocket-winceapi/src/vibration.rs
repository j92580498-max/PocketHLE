//! NLED API and the VIB1 stream-driver ABI from the Gizmondo SDK.
use crate::{CallCtx, WinCeDispatcher};
use pocket_cpu::Prot;
use pocket_kernel::{
    vibration::{Service, Settings},
    DispatchOutcome, KernelError,
};
pub fn register(d: &mut WinCeDispatcher) {
    d.register_handler("coredll.dll", "NLedGetDeviceInfo", get);
    d.register_handler("coredll.dll", "NLedSetDevice", set);
}
fn finish(ctx: &mut CallCtx<'_>, result: Result<(), u32>) -> Result<DispatchOutcome, KernelError> {
    match result {
        Ok(()) => Ok(DispatchOutcome::ReturnedR0(1)),
        Err(e) => {
            crate::coredll::set_thread_error(ctx, e);
            Ok(DispatchOutcome::ReturnedR0(0))
        }
    }
}
fn probe(ctx: &mut CallCtx<'_>, p: u32, n: u32, prot: Prot) -> bool {
    p != 0 && p.checked_add(n).is_some() && ctx.cpu.check_guest_access(p, n, prot).is_ok()
}
fn get_info(
    ctx: &mut CallCtx<'_>,
    s: &Service,
    id: u32,
    p: u32,
    led: Option<u32>,
) -> Result<(), u32> {
    let size = match id {
        0 => 4,
        1 | 2 => 28,
        _ => return Err(87),
    };
    if !probe(ctx, p, size, Prot::WRITE) {
        return Err(87);
    }
    if id != 0 {
        let led = if let Some(v) = led {
            v
        } else {
            if !probe(ctx, p, 4, Prot::READ) {
                return Err(87);
            }
            ctx.cpu.read_u32_le(p).map_err(|_| 87u32)?
        };
        if led != 0 {
            return Err(87);
        }
    }
    let values = match id {
        0 => vec![1],
        1 => vec![0, 1000, 1, 1, 1, 1, 1],
        _ => {
            let v = s.settings();
            vec![
                0,
                v.mode,
                v.total as u32,
                v.on as u32,
                v.off as u32,
                v.meta_on as u32,
                v.meta_off as u32,
            ]
        }
    };
    ctx.cpu
        .write_mem(
            p,
            &values
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .map_err(|_| 87u32)
}
fn set_info(ctx: &mut CallCtx<'_>, s: &Service, id: u32, p: u32) -> Result<(), u32> {
    if id != 2 || !probe(ctx, p, 28, Prot::READ) {
        return Err(87);
    }
    let b = ctx.cpu.read_mem(p, 28).map_err(|_| 87u32)?;
    let word = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
    if word(0) != 0 {
        return Err(87);
    }
    s.set(Settings {
        mode: word(1),
        total: word(2) as i32,
        on: word(3) as i32,
        off: word(4) as i32,
        meta_on: word(5) as i32,
        meta_off: word(6) as i32,
    })
}
fn get(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let id = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    let s = ctx.kernel.vfs.vibration_service();
    let result = get_info(ctx, &s, id, p, None);
    finish(ctx, result)
}
fn set(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let id = ctx.arg_u32(0)?;
    let p = ctx.arg_u32(1)?;
    let s = ctx.kernel.vfs.vibration_service();
    let result = set_info(ctx, &s, id, p);
    finish(ctx, result)
}
pub(crate) fn control(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let h = ctx.arg_u32(0)?;
    let id = ctx.arg_u32(1)?;
    let input = ctx.arg_u32(2)?;
    let ilen = ctx.arg_u32(3)?;
    let output = ctx.arg_u32(4)?;
    let olen = ctx.arg_u32(5)?;
    let returned = ctx.arg_u32(6)?;
    let overlapped = ctx.arg_u32(7)?;
    if returned != 0 {
        if !probe(ctx, returned, 4, Prot::WRITE) {
            return finish(ctx, Err(87));
        }
        ctx.cpu.write_mem(returned, &0u32.to_le_bytes())?;
    }
    if overlapped != 0 {
        return finish(ctx, Err(50));
    }
    let Some(open) = ctx.kernel.vfs.vibration_open(h) else {
        return finish(ctx, Err(6));
    };
    if id == 2 && ![0, 4, 28].contains(&ilen) {
        return finish(ctx, Err(87));
    }
    let result = if id == 2 && ilen == 28 {
        if open.access & 2 == 0 {
            Err(5)
        } else {
            set_info(ctx, &open.service, id, input)
        }
    } else {
        let size = match id {
            0 => 4,
            1 | 2 => 28,
            _ => return finish(ctx, Err(50)),
        };
        if open.access & 1 == 0 {
            Err(5)
        } else if olen < size {
            Err(122)
        } else {
            let led = if id != 0 && ilen != 0 {
                if ilen < 4 || !probe(ctx, input, 4, Prot::READ) {
                    return finish(ctx, Err(87));
                }
                Some(ctx.cpu.read_u32_le(input)?)
            } else {
                None
            };
            let r = get_info(ctx, &open.service, id, output, led);
            if r.is_ok() && returned != 0 {
                ctx.cpu.write_mem(returned, &size.to_le_bytes())?;
            }
            r
        }
    };
    finish(ctx, result)
}
pub(crate) fn unsupported_stream(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    finish(ctx, Err(50))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth::tests::{call, setup};
    use pocket_cpu::Cpu;
    fn value(v: DispatchOutcome) -> u32 {
        let DispatchOutcome::ReturnedR0(v) = v else {
            panic!("unexpected outcome")
        };
        v
    }
    #[test]
    fn sdk_device_and_nled_share_state_close_does_not_stop_motor() {
        let (mut cpu, mut k, mut d) = setup();
        cpu.write_mem(
            0x1000,
            &"VIB1:"
                .encode_utf16()
                .chain(Some(0))
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        macro_rules! api {
            ($n:expr,$a:expr) => {
                call(&mut cpu, &mut k, &mut d, "coredll.dll", $n, $a)
            };
        }
        let h = value(api!("CreateFileW", &[0x1000, 0xc0000000, 0, 0, 3, 0, 0]));
        assert_ne!(h, u32::MAX);
        assert_eq!(
            value(api!("DeviceIoControl", &[h, 0, 0, 0, 0x1100, 4, 0x1200, 0])),
            1
        );
        assert_eq!(cpu.read_u32_le(0x1100).unwrap(), 1);
        assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 4);
        let settings = [0u32, 2, 4000000, 2000000, 2000000, 0, 0];
        cpu.write_mem(
            0x1300,
            &settings
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(
            value(api!("DeviceIoControl", &[h, 2, 0x1300, 28, 0, 0, 0, 0])),
            1
        );
        assert_eq!(value(api!("CloseHandle", &[h])), 1);
        cpu.write_mem(0x1400, &[0; 28]).unwrap();
        assert_eq!(value(api!("NLedGetDeviceInfo", &[2, 0x1400])), 1);
        assert_eq!(cpu.read_u32_le(0x1404).unwrap(), 2);
        cpu.write_mem(0x1304, &0u32.to_le_bytes()).unwrap();
        assert_eq!(value(api!("NLedSetDevice", &[2, 0x1300])), 1);
        assert_eq!(k.vfs.vibration_service().settings().mode, 0);
        cpu.write_mem(0x1300, &1u32.to_le_bytes()).unwrap();
        assert_eq!(value(api!("NLedSetDevice", &[2, 0x1300])), 0);
        assert_eq!(value(api!("GetLastError", &[])), 87);
        assert_eq!(value(api!("NLedGetDeviceInfo", &[0, 0])), 0);
        assert_eq!(value(api!("NLedSetDevice", &[1, 0x1300])), 0);
    }
    #[test]
    fn cross_process_duplication_preserves_sharing_lease_and_access() {
        let mut a = pocket_kernel::vfs::Vfs::new();
        let mut b = pocket_kernel::vfs::Vfs::new();
        b.attach_shared_context(a.shared_context());
        let h = a.open_file("VIB1:", 1, 0, 3, false, 0).unwrap().handle;
        let object = a.export_handle(h).unwrap();
        assert!(b.import_handle(99, object));
        assert!(a.close(h));
        assert_eq!(a.open_file("VIB1:", 1, 0, 3, false, 0), Err(32));
        assert_eq!(b.vibration_open(99).unwrap().access, 1);
        assert!(b.close(99));
        assert!(a.open_file("VIB1:", 1, 0, 3, false, 0).is_ok());
    }
    #[test]
    fn errors_preserve_settings_and_success_preserves_last_error() {
        let (mut cpu, mut k, mut d) = setup();
        cpu.write_mem(
            0x1000,
            &"VIB1:"
                .encode_utf16()
                .chain(Some(0))
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        macro_rules! api {
            ($n:expr,$a:expr) => {
                call(&mut cpu, &mut k, &mut d, "coredll.dll", $n, $a)
            };
        }
        let h = value(api!("CreateFileW", &[0x1000, 0x80000000, 0, 0, 3, 0, 0]));
        let settings = [0u32, 1, 0, 0, 0, 0, 0];
        cpu.write_mem(
            0x1300,
            &settings
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(
            value(api!("DeviceIoControl", &[h, 2, 0x1300, 28, 0, 0, 0, 0])),
            0
        );
        assert_eq!(value(api!("GetLastError", &[])), 5);
        assert_eq!(k.vfs.vibration_service().settings().mode, 0);
        assert_eq!(
            value(api!("DeviceIoControl", &[h, 0, 0, 0, 0x1100, 3, 0x1200, 0])),
            0
        );
        assert_eq!(value(api!("GetLastError", &[])), 122);
        assert_eq!(cpu.read_u32_le(0x1200).unwrap(), 0);
        assert_eq!(
            value(api!("DeviceIoControl", &[h, 0, 0, 0, 0x1100, 4, 0x1200, 1])),
            0
        );
        assert_eq!(value(api!("GetLastError", &[])), 50);
        api!("SetLastError", &[1234]);
        assert_eq!(value(api!("NLedGetDeviceInfo", &[0, 0x1100])), 1);
        assert_eq!(value(api!("GetLastError", &[])), 1234);
        assert_eq!(value(api!("CloseHandle", &[h])), 1);
        assert!(k.vfs.open_handles().is_empty());
    }
}
