//! Tiny stub for `ole32.dll`.
//!
//! Pocket PC games rarely use COM proper, but a number of them
//! (Zuma, several PopCap titles) link against `ole32.dll` for
//! `CoTaskMemAlloc` / `CoTaskMemFree` — typically as the allocator
//! behind `BSTR`-shaped strings or DirectShow-style buffers.
//! Returning `0` from `CoTaskMemAlloc` causes the game to dereference
//! a NULL pointer immediately, so we route the call through the
//! kernel's heap.
//!
//! `CoInitialize{Ex}` / `CoUninitialize` / `OleInitialize` /
//! `OleUninitialize` always return `S_OK` — we don't model the
//! apartment-threading model.

use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

pub fn register(d: &mut WinCeDispatcher) {
    let dll = "ole32.dll";
    d.register_handler(dll, "CoTaskMemAlloc", co_task_mem_alloc);
    d.register_handler(dll, "CoTaskMemFree", co_task_mem_free);
    d.register_handler(dll, "CoTaskMemRealloc", co_task_mem_realloc);
    d.register_handler(dll, "CoInitialize", s_ok);
    d.register_handler(dll, "CoInitializeEx", s_ok);
    d.register_handler(dll, "CoUninitialize", void_returning);
    d.register_handler(dll, "CoCreateGuid", co_create_guid);
    d.register_handler(dll, "OleInitialize", s_ok);
    d.register_handler(dll, "OleUninitialize", void_returning);
    d.register_handler(dll, "CoCreateInstance", co_create_instance);
    d.register_handler("coredll.dll", "com_qi", com_query_interface);
    d.register_handler("coredll.dll", "com_add_ref", com_add_ref);
    d.register_handler("coredll.dll", "com_release", com_release);
    d.register_handler("coredll.dll", "com_success", com_success);
    d.register_constant(dll, "CoGetMalloc", 0, zero_returning);
}

fn s_ok(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn void_returning(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn zero_returning(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

const COM_OBJECT_VTABLE_ENTRIES: usize = 32;

fn com_success(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn com_add_ref(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn com_release(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn com_query_interface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let object = ctx.arg_u32(0)?;
    let iid = ctx.arg_u32(1)?;
    let out = ctx.arg_u32(2)?;
    if out == 0 || object == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0x8007_0057));
    }
    if iid == 0 {
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let requested = ctx.cpu.read_mem(iid, 16)?;
    if requested.len() != 16 {
        ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
        return Ok(DispatchOutcome::ReturnedR0(0x8000_4002));
    }
    ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn com_object_vtable(ctx: &mut CallCtx<'_>) -> Result<u32, KernelError> {
    let table = ctx
        .kernel
        .heap
        .alloc((COM_OBJECT_VTABLE_ENTRIES * 4) as u32)
        .unwrap_or(0);
    if table == 0 {
        return Ok(0);
    }
    let exports = ctx
        .kernel
        .dynamic_exports
        .get(&pocket_kernel::PROCESS_INSTANCE_HANDLE);
    let query = exports.and_then(|e| e.get("com_qi")).copied().unwrap_or(0);
    let add_ref = exports
        .and_then(|e| e.get("com_add_ref"))
        .copied()
        .unwrap_or(0);
    let release = exports
        .and_then(|e| e.get("com_release"))
        .copied()
        .unwrap_or(0);
    let success = exports
        .and_then(|e| e.get("com_success"))
        .copied()
        .unwrap_or(0);
    for index in 0..COM_OBJECT_VTABLE_ENTRIES {
        let address = match index {
            0 => query,
            1 => add_ref,
            2 => release,
            _ => success,
        };
        ctx.cpu
            .write_mem(table + index as u32 * 4, &address.to_le_bytes())?;
    }
    Ok(table)
}

fn co_create_instance(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(4)?;
    if out == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0x8007_0057));
    }
    let table = com_object_vtable(ctx)?;
    if table == 0 {
        ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
        return Ok(DispatchOutcome::ReturnedR0(0x8000_4005));
    }
    let object = ctx.kernel.heap.alloc(4).unwrap_or(0);
    if object == 0 {
        ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
        return Ok(DispatchOutcome::ReturnedR0(0x8000_4005));
    }
    ctx.cpu.write_mem(object, &table.to_le_bytes())?;
    ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

/// `HRESULT CoCreateGuid(GUID *pguid)` — fill 16 bytes with a
/// pseudo-random value. Most games that call this only use the
/// resulting GUID as a unique-ish key, so we just need it to be
/// non-zero and stable within a run.
fn co_create_guid(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEED: AtomicU32 = AtomicU32::new(0xFEED_BABE);
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0x8007_0057)); // E_INVALIDARG
    }
    let mut buf = [0u8; 16];
    for chunk in buf.chunks_mut(4) {
        let v = SEED.fetch_add(0x9E37_79B9, Ordering::Relaxed);
        chunk.copy_from_slice(&v.to_le_bytes());
    }
    ctx.cpu.write_mem(p, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn co_task_mem_alloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let size = ctx.arg_u32(0)?;
    let user_ptr = ctx.kernel.heap.alloc(size).unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(user_ptr))
}

fn co_task_mem_free(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    if p != 0 {
        ctx.kernel.heap.free(p);
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn co_task_mem_realloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let p = ctx.arg_u32(0)?;
    let size = ctx.arg_u32(1)?;
    if p == 0 {
        let v = ctx.kernel.heap.alloc(size).unwrap_or(0);
        return Ok(DispatchOutcome::ReturnedR0(v));
    }
    if size == 0 {
        ctx.kernel.heap.free(p);
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let old_size = ctx.kernel.heap.msize(p).unwrap_or(0);
    let new_p = match ctx.kernel.heap.alloc(size) {
        Some(np) => np,
        None => return Ok(DispatchOutcome::ReturnedR0(0)),
    };
    let to_copy = old_size.min(size);
    if to_copy > 0 {
        let bytes = ctx.cpu.read_mem(p, to_copy)?;
        ctx.cpu.write_mem(new_p, &bytes)?;
    }
    ctx.kernel.heap.free(p);
    Ok(DispatchOutcome::ReturnedR0(new_p))
}
