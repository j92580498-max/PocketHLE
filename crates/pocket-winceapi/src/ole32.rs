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
    d.register_handler("coredll.dll", "com_query_interface", com_query_interface);
    d.register_handler("coredll.dll", "com_add_ref", com_add_ref);
    d.register_handler("coredll.dll", "com_release", com_release);
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

/// `HRESULT CoCreateInstance` returns a small in-process COM object for
/// the media interfaces used by Pocket PC games. The object only needs
/// IUnknown's first three methods for the startup probes we support.
fn co_create_instance(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ppv = ctx.arg_u32(4)?;
    if ppv == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0x8000_0057));
    }
    let Some(vtable) = ctx.kernel.heap.alloc(12) else {
        ctx.cpu.write_mem(ppv, &0u32.to_le_bytes())?;
        return Ok(DispatchOutcome::ReturnedR0(0x8000_4005));
    };
    let Some(object) = ctx.kernel.heap.alloc(4) else {
        ctx.cpu.write_mem(ppv, &0u32.to_le_bytes())?;
        return Ok(DispatchOutcome::ReturnedR0(0x8000_4005));
    };
    let exports = ctx.kernel.dynamic_exports.get(&0x1000_0000);
    let address = |name: &str| {
        exports
            .and_then(|table| table.get(name).copied())
            .unwrap_or(0)
    };
    for (index, name) in ["com_query_interface", "com_add_ref", "com_release"]
        .iter()
        .enumerate()
    {
        ctx.cpu
            .write_mem(vtable + index as u32 * 4, &address(name).to_le_bytes())?;
    }
    ctx.cpu.write_mem(object, &vtable.to_le_bytes())?;
    ctx.cpu.write_mem(ppv, &object.to_le_bytes())?;
    log::debug!("CoCreateInstance() -> COM object 0x{object:08x}");
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn com_query_interface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let object = ctx.arg_u32(0)?;
    let out = ctx.arg_u32(2)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(if object != 0 {
        0
    } else {
        0x8000_4002
    }))
}

fn com_add_ref(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn com_release(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
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
