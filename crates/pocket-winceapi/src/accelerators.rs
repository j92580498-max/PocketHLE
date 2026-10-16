//! Resource-backed accelerator tables need no host allocation: the module
//! resource address is the handle and its lifetime follows the loaded image.
use crate::{CallCtx, WinCeDispatcher};
use pocket_cpu::{regs::ArmReg, Prot};
use pocket_kernel::{DispatchOutcome, GuestCallFrame, KernelError};
use pocket_pe::resources::ResourceKey;
pub(crate) fn register(d: &mut WinCeDispatcher) {
    for name in ["LoadAcceleratorsW", "LoadAcceleratorsA"] {
        d.register_handler("coredll.dll", name, load);
    }
    for name in ["TranslateAcceleratorW", "TranslateAcceleratorA"] {
        d.register_handler("coredll.dll", name, translate);
    }
}
fn failure(ctx: &mut CallCtx<'_>, error: u32) -> Result<DispatchOutcome, KernelError> {
    crate::coredll::set_thread_error(ctx, error);
    Ok(DispatchOutcome::ReturnedR0(0))
}
fn load(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let module = ctx.arg_u32(0)?;
    let raw = ctx.arg_u32(1)?;
    let key = if raw <= 0xffff {
        ResourceKey::Id(raw)
    } else if ctx
        .thunk
        .friendly_name
        .as_deref()
        .unwrap_or("")
        .ends_with('A')
    {
        let mut bytes = Vec::new();
        for i in 0..4096 {
            let b = match ctx.cpu.read_mem(raw + i, 1) {
                Ok(b) => b[0],
                Err(_) => return failure(ctx, 87),
            };
            if b == 0 {
                break;
            }
            bytes.push(b);
        }
        ResourceKey::Name(bytes.into_iter().map(char::from).collect())
    } else {
        match crate::coredll::read_wide_resource_key(ctx, raw) {
            Ok(k) => k,
            Err(_) => return failure(ctx, 87),
        }
    };
    let Some((entry, base)) =
        crate::coredll::lookup_resource(ctx.kernel, module, &ResourceKey::Id(9), &key)
    else {
        return failure(ctx, 1814);
    };
    if entry.size == 0 || entry.size % 8 != 0 || entry.size > 65536 {
        return failure(ctx, 13);
    }
    let Some(addr) = base.checked_add(entry.data_rva) else {
        return failure(ctx, 13);
    };
    if ctx
        .cpu
        .check_guest_access(addr, entry.size, Prot::READ)
        .is_err()
    {
        return failure(ctx, 13);
    }
    Ok(DispatchOutcome::ReturnedR0(addr))
}
fn matching(bytes: &[u8], message: u32, key: u32, modifiers: u16) -> Option<u16> {
    for row in bytes.chunks_exact(8) {
        let flags = u16::from_le_bytes([row[0], row[1]]);
        let k = u16::from_le_bytes([row[2], row[3]]);
        let cmd = u16::from_le_bytes([row[4], row[5]]);
        let virtual_key = flags & 1 != 0;
        let event = if virtual_key {
            message == 0x100 || message == 0x104
        } else {
            message == 0x102 || message == 0x106
        };
        if event && key == k as u32 && flags & 0x1c == modifiers {
            return Some(cmd);
        }
        if flags & 0x80 != 0 {
            break;
        }
    }
    None
}
fn translate(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let sp = ctx.cpu.read_reg(ArmReg::Sp)?;
    let thread = ctx.kernel.current_thread;
    if let Some(index) = ctx
        .kernel
        .accelerator_frames
        .iter()
        .rposition(|(t, thunk, f)| *t == thread && *thunk == ctx.thunk.thunk_va && f.sp == sp)
    {
        let (_, _, frame) = ctx.kernel.accelerator_frames.remove(index);
        for (r, v) in [
            (ArmReg::R1, frame.args[1]),
            (ArmReg::R2, frame.args[2]),
            (ArmReg::R3, frame.args[3]),
            (ArmReg::Lr, frame.lr),
        ] {
            ctx.cpu.write_reg(r, v)?;
        }
        return Ok(DispatchOutcome::ReturnedR0(1));
    }
    let hwnd = ctx.arg_u32(0)?;
    let handle = ctx.arg_u32(1)?;
    let msg = ctx.arg_u32(2)?;
    let resource = ctx
        .kernel
        .resource_scopes()
        .flat_map(|(entries, base)| entries.iter().map(move |e| (e, base)))
        .find(|(e, base)| {
            e.ty == ResourceKey::Id(9) && base.checked_add(e.data_rva) == Some(handle)
        })
        .map(|(e, _)| e.size);
    let Some(size) = resource else {
        return failure(ctx, 6);
    };
    if size > 65536 || size % 8 != 0 || ctx.cpu.check_guest_access(msg, 28, Prot::READ).is_err() {
        return failure(ctx, 87);
    }
    let data = ctx.cpu.read_mem(msg, 28)?;
    let bytes = ctx.cpu.read_mem(handle, size)?;
    let message = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let key = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let held = |v: usize| ctx.kernel.pressed_keys[v];
    let modifiers = if held(16) || held(160) || held(161) {
        4
    } else {
        0
    } | if held(17) || held(162) || held(163) {
        8
    } else {
        0
    } | if held(18) || held(164) || held(165) {
        16
    } else {
        0
    };
    let Some(cmd) = matching(&bytes, message, key, modifiers) else {
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let proc = ctx
        .kernel
        .window_procs
        .get(&hwnd)
        .copied()
        .unwrap_or(ctx.kernel.wnd_proc);
    if proc == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if ctx.kernel.accelerator_frames.len() >= 64 {
        return failure(ctx, 8);
    }
    let frame = GuestCallFrame {
        args: [hwnd, handle, msg, ctx.cpu.read_reg(ArmReg::R3)?],
        lr: ctx.cpu.read_reg(ArmReg::Lr)?,
        sp,
    };
    ctx.kernel
        .accelerator_frames
        .push((thread, ctx.thunk.thunk_va, frame));
    for (r, v) in [
        (ArmReg::R0, hwnd),
        (ArmReg::R1, 0x111),
        (ArmReg::R2, cmd as u32 | 0x10000),
        (ArmReg::R3, 0),
        (ArmReg::Lr, ctx.thunk.thunk_va),
    ] {
        ctx.cpu.write_reg(r, v)?;
    }
    Ok(DispatchOutcome::JumpTo(proc))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn virtual_keys_modifiers_and_characters_are_distinct() {
        let bytes = [0x89, 0, 0x41, 0, 42, 0, 0, 0];
        assert_eq!(matching(&bytes, 0x100, 0x41, 8), Some(42));
        assert_eq!(matching(&bytes, 0x100, 0x41, 0), None);
        assert_eq!(matching(&bytes, 0x102, 0x41, 8), None);
        assert_eq!(
            matching(&[0x80, 0, 97, 0, 7, 0, 0, 0], 0x102, 97, 0),
            Some(7)
        );
    }
}
#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::bluetooth::tests::{call, setup};
    use pocket_cpu::Cpu;
    #[test]
    fn resource_table_dispatches_synchronously_and_restores_true_return() {
        let (mut cpu, mut k, mut d) = setup();
        k.image_base = 0x1000;
        k.resources.push(pocket_pe::resources::ResourceEntry {
            ty: ResourceKey::Id(9),
            name: ResourceKey::Id(101),
            data_rva: 0x200,
            size: 8,
            codepage: 0,
        });
        cpu.write_mem(0x1200, &[0x81, 0, 0x41, 0, 42, 0, 0, 0])
            .unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "LoadAcceleratorsW",
                &[0, 101]
            ),
            DispatchOutcome::ReturnedR0(0x1200)
        );
        cpu.write_mem(
            0x1300,
            &[55u32, 0x100, 0x41, 0, 0, 0, 0]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        k.window_procs.insert(55, 0x9000);
        cpu.write_reg(ArmReg::Lr, 0x8888).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "TranslateAcceleratorW",
                &[55, 0x1200, 0x1300]
            ),
            DispatchOutcome::JumpTo(0x9000)
        );
        assert_eq!(cpu.read_reg(ArmReg::R1).unwrap(), 0x111);
        assert_eq!(cpu.read_reg(ArmReg::R2).unwrap(), 0x1002a);
        assert_eq!(k.accelerator_frames.len(), 1);
        // The window procedure's return value must not escape as the BOOL.
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "TranslateAcceleratorW",
                &[0]
            ),
            DispatchOutcome::ReturnedR0(1)
        );
        assert_eq!(cpu.read_reg(ArmReg::Lr).unwrap(), 0x8888);
        assert!(k.accelerator_frames.is_empty());
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "TranslateAcceleratorW",
                &[55, 0xdeadbeef, 0x1300]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "coredll.dll", "GetLastError", &[]),
            DispatchOutcome::ReturnedR0(6)
        );
    }
    #[test]
    fn process_exit_clears_callback_frames_and_shell_subscription() {
        let (_, mut k, _) = setup();
        k.accelerator_frames.push((
            0,
            0x70000000,
            GuestCallFrame {
                args: [0; 4],
                lr: 0,
                sp: 0x4000,
            },
        ));
        k.shell_status = Some(8);
        k.internet.gprs_connected = true;
        k.record_process_exit(0);
        assert!(k.accelerator_frames.is_empty());
        assert_eq!(k.shell_status, None);
        assert!(!k.internet.gprs_connected);
    }
}
