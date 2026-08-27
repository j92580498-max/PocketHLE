//! Minimal OLE32 and Windows Media Player compatibility layer.

use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

const WMP_METHODS: [&str; 38] = [
    "wmp_query_interface",
    "wmp_add_ref",
    "wmp_release",
    "wmp_close",
    "wmp_get_url",
    "wmp_put_url",
    "wmp_get_open_state",
    "wmp_get_play_state",
    "wmp_get_controls",
    "wmp_get_settings",
    "wmp_get_current_media",
    "wmp_put_current_media",
    "wmp_get_media_collection",
    "wmp_get_playlist_collection",
    "wmp_get_version_info",
    "wmp_launch_url",
    "wmp_get_network",
    "wmp_get_current_playlist",
    "wmp_put_current_playlist",
    "wmp_get_cdrom_collection",
    "wmp_get_closed_caption",
    "wmp_get_is_online",
    "wmp_get_error",
    "wmp_get_status",
    "wmp_get_enabled",
    "wmp_put_enabled",
    "wmp_get_full_screen",
    "wmp_put_full_screen",
    "wmp_get_enable_context_menu",
    "wmp_put_enable_context_menu",
    "wmp_get_ui_mode",
    "wmp_put_ui_mode",
    "wmp_get_stretch_to_fit",
    "wmp_put_stretch_to_fit",
    "wmp_get_windowless_video",
    "wmp_put_windowless_video",
    "wmp_get_is_remote",
    "wmp_get_player_application",
];

const WMP_CHILD_METHOD: &str = "wmp_child_method";
const WMP_CHILD_SLOTS: usize = 64;

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
    for &name in &WMP_METHODS {
        d.register_handler(dll, name, wmp_method);
    }
    d.register_handler(dll, WMP_CHILD_METHOD, wmp_child_method);
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

fn wmp_export_address(ctx: &CallCtx<'_>, name: &str) -> u32 {
    ctx.kernel
        .dynamic_exports
        .get(&pocket_kernel::OLE32_MODULE_HANDLE)
        .and_then(|exports| exports.get(name).copied())
        .unwrap_or(0)
}

fn alloc_child(ctx: &mut CallCtx<'_>) -> Result<u32, KernelError> {
    let vtable = ctx
        .kernel
        .heap
        .alloc((WMP_CHILD_SLOTS * 4) as u32)
        .unwrap_or(0);
    let object = ctx.kernel.heap.alloc(4).unwrap_or(0);
    if vtable == 0 || object == 0 {
        return Ok(0);
    }
    let address = wmp_export_address(ctx, WMP_CHILD_METHOD);
    for index in 0..WMP_CHILD_SLOTS {
        ctx.cpu
            .write_mem(vtable + (index as u32) * 4, &address.to_le_bytes())?;
    }
    ctx.cpu.write_mem(object, &vtable.to_le_bytes())?;
    Ok(object)
}

fn co_create_instance(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const CLSID_WMP: [u8; 16] = [
        0x52, 0x2a, 0xf5, 0x6b, 0x4a, 0x39, 0xd3, 0x11, 0xb1, 0x53, 0x00, 0xc0, 0x4f, 0x79, 0xfa,
        0xa6,
    ];
    const IID_WMP_PLAYER: [u8; 16] = [
        0x4f, 0x2a, 0xf5, 0x6b, 0x4a, 0x39, 0xd3, 0x11, 0xb1, 0x53, 0x00, 0xc0, 0x4f, 0x79, 0xfa,
        0xa6,
    ];
    let clsid = ctx.arg_u32(0)?;
    let outer = ctx.arg_u32(1)?;
    let iid = ctx.arg_u32(3)?;
    let out = ctx.arg_u32(4)?;
    if outer != 0 || clsid == 0 || iid == 0 || out == 0 {
        if out != 0 {
            ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
        }
        return Ok(DispatchOutcome::ReturnedR0(0x8000_4002));
    }
    let clsid_bytes = ctx.cpu.read_mem(clsid, 16)?;
    let iid_bytes = ctx.cpu.read_mem(iid, 16)?;
    if clsid_bytes != CLSID_WMP || iid_bytes != IID_WMP_PLAYER {
        ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
        return Ok(DispatchOutcome::ReturnedR0(0x8000_4002));
    }
    let vtable_slots = WMP_CHILD_SLOTS.max(WMP_METHODS.len());
    let vtable = ctx
        .kernel
        .heap
        .alloc((vtable_slots * 4) as u32)
        .unwrap_or(0);
    let object = ctx.kernel.heap.alloc(4).unwrap_or(0);
    let child = alloc_child(ctx)?;
    if vtable == 0 || object == 0 || child == 0 {
        ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
        return Ok(DispatchOutcome::ReturnedR0(0x8000_4005));
    }
    for index in 0..vtable_slots {
        let name = WMP_METHODS.get(index).copied().unwrap_or(WMP_CHILD_METHOD);
        let address = wmp_export_address(ctx, name);
        ctx.cpu
            .write_mem(vtable + (index as u32) * 4, &address.to_le_bytes())?;
    }
    ctx.cpu.write_mem(object, &vtable.to_le_bytes())?;
    ctx.cpu.write_mem(out, &object.to_le_bytes())?;
    log::info!(
        "CoCreateInstance: created Windows Media Player compatibility object 0x{object:08x}"
    );
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn wmp_method(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let method = ctx.thunk.friendly_name.as_deref().unwrap_or_default();
    match method {
        "wmp_query_interface" => {
            let out = ctx.arg_u32(2)?;
            if out != 0 {
                let this = ctx.arg_u32(0)?;
                ctx.cpu.write_mem(out, &this.to_le_bytes())?;
            }
            Ok(DispatchOutcome::ReturnedR0(0))
        }
        "wmp_add_ref" | "wmp_release" => Ok(DispatchOutcome::ReturnedR0(1)),
        "wmp_get_controls"
        | "wmp_get_settings"
        | "wmp_get_current_media"
        | "wmp_get_media_collection"
        | "wmp_get_playlist_collection"
        | "wmp_get_network"
        | "wmp_get_current_playlist"
        | "wmp_get_cdrom_collection"
        | "wmp_get_closed_caption"
        | "wmp_get_error"
        | "wmp_get_player_application" => {
            let out = ctx.arg_u32(1)?;
            if out != 0 {
                let child = alloc_child(ctx)?;
                ctx.cpu.write_mem(out, &child.to_le_bytes())?;
            }
            Ok(DispatchOutcome::ReturnedR0(0))
        }
        "wmp_get_open_state"
        | "wmp_get_play_state"
        | "wmp_get_is_online"
        | "wmp_get_enabled"
        | "wmp_get_full_screen"
        | "wmp_get_enable_context_menu"
        | "wmp_get_stretch_to_fit"
        | "wmp_get_windowless_video"
        | "wmp_get_is_remote" => {
            let out = ctx.arg_u32(1)?;
            if out != 0 {
                ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
            }
            Ok(DispatchOutcome::ReturnedR0(0))
        }
        "wmp_get_url" | "wmp_get_version_info" | "wmp_get_status" | "wmp_get_ui_mode" => {
            let out = ctx.arg_u32(1)?;
            if out != 0 {
                ctx.cpu.write_mem(out, &0u32.to_le_bytes())?;
            }
            Ok(DispatchOutcome::ReturnedR0(0))
        }
        _ => Ok(DispatchOutcome::ReturnedR0(0)),
    }
}

fn wmp_child_method(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
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

fn co_create_guid(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEED: AtomicU32 = AtomicU32::new(0xFEED_BABE);
    let p = ctx.arg_u32(0)?;
    if p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0x8007_0057));
    }
    let mut buf = [0u8; 16];
    for chunk in buf.chunks_mut(4) {
        let v = SEED.fetch_add(0x9E37_79B9, Ordering::Relaxed);
        chunk.copy_from_slice(&v.to_le_bytes());
    }
    ctx.cpu.write_mem(p, &buf)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}
