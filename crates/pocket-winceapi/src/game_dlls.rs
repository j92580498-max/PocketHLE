//! Stubs for the small native game libraries bundled with MetalStrike.

use pocket_cpu::Prot;
use pocket_kernel::{
    DispatchOutcome, InputEvent, KernelError, IMPORT_DATA_BASE, SYNTHETIC_FRAMEBUFFER_BASE,
};

use crate::{CallCtx, WinCeDispatcher};

pub fn register(d: &mut WinCeDispatcher) {
    register_platypus(d);
    register_game_x(d);
    register_sound_x(d);
    d.register_handler("note_prj.dll", "ord:7", find_first_flash_card);
    d.register_handler("note_prj.dll", "#7", find_first_flash_card);
    d.register_handler("note_prj.dll", "ord:8", find_next_flash_card);
    d.register_handler("note_prj.dll", "#8", find_next_flash_card);
}

const FLASH_CARD_HANDLE: u32 = 0xDEAD_F100;
const FIND_DATA_NAME_OFFSET: usize = 40;
const FIND_DATA_SIZE: usize = FIND_DATA_NAME_OFFSET + 260 * 2;

fn write_flash_card_find_data(ctx: &mut CallCtx<'_>, out: u32) -> Result<(), KernelError> {
    if out == 0 {
        return Ok(());
    }
    let mut data = vec![0u8; FIND_DATA_SIZE];
    data[0..4].copy_from_slice(&0x10u32.to_le_bytes());
    for (index, unit) in "Storage Card".encode_utf16().enumerate() {
        let offset = FIND_DATA_NAME_OFFSET + index * 2;
        data[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }
    ctx.cpu.write_mem(out, &data)?;
    Ok(())
}

fn find_first_flash_card(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(0)?;
    write_flash_card_find_data(ctx, out)?;
    Ok(DispatchOutcome::ReturnedR0(FLASH_CARD_HANDLE))
}

fn find_next_flash_card(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let _handle = ctx.arg_u32(0)?;
    let _out = ctx.arg_u32(1)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn register_platypus(d: &mut WinCeDispatcher) {
    d.register_handler("rabbitfactory.dll", "?BRinit@@YAHHH@Z", br_init);
    d.register_handler("rabbitfactory.dll", "?BRmalloc@@YAPAXJ@Z", br_malloc);
    d.register_handler("rabbitfactory.dll", "?BRfree@@YAXPAX@Z", br_free);
    d.register_handler(
        "rabbitfactory.dll",
        "?GetDeviceType@CHLWinMobile@RabbitFactory@@QAAJXZ",
        device_type,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?GetTime@RabbitFactory@@YAKXZ",
        get_time,
    );
    d.register_handler("rabbitgame.dll", "CreateGameManager", create_game_manager);
    d.register_handler(
        "rabbitgame.dll",
        "CreateHardwareLayer",
        create_hardware_layer,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "??0CGameEngine@@QAA@PAVCGameManager@RabbitFactory@@@Z",
        initialize_cpp_object,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "??0CMainGfxEngine@@QAA@PAVCGameEngine@@@Z",
        initialize_cpp_object,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?InitBitmapSession@CMainGfxEngine@@UAAXXZ",
        void_returning,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?InitializeStylus@CGameEngine@@QAAXXZ",
        void_returning,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?Initialize@CGameEngine@@QAAXXZ",
        void_returning,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?OnFocusGained@CGameEngine@@UAAXXZ",
        void_returning,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?OnFocusLost@CGameEngine@@UAAXXZ",
        void_returning,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?GetStylus@CGameEngine@@QAAPAVCStylus@RabbitFactory@@XZ",
        get_stylus,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?GetPenHandler@CGameEngine@@QAAPAVCPenHandler@@XZ",
        get_stylus,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?StylusIsDown@CStylus@RabbitFactory@@QAA_NXZ",
        stylus_false,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?StylusIsPressed@CStylus@RabbitFactory@@QAA_NXZ",
        stylus_false,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?StylusIsReleased@CStylus@RabbitFactory@@QAA_NXZ",
        stylus_false,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?GetGfxEngine@CGameEngine@@QAAPAVCMainGfxEngine@@XZ",
        get_gfx_engine,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?SetOrigin@CGfxEngine@@UAAXJJ@Z",
        set_origin,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?GetOriginX@CGfxEngine@@QAAJXZ",
        get_origin_x,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?GetOriginY@CGfxEngine@@QAAJXZ",
        get_origin_y,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?HasSurfaceOwnership@CMainGfxEngine@@UAA_NXZ",
        has_surface_ownership,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?Init@CGfxEngine@@UAAXXZ",
        void_returning,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?Destroy@CGfxEngine@@UAAXXZ",
        void_returning,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?ScreenBlitter565@CGfxEngine@@IAAXPAKJ_N@Z",
        screen_blitter_565,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?GetOrientation@CGfxEngine@@QAAJXZ",
        get_orientation,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?GetKeyHandler@CGameEngine@@QAAPAVCKeyHandler@@XZ",
        get_key_handler,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?Update@CKeyHandler@@QAA_NJ_N@Z",
        update_key_handler,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?ExternInterrupt@CGameEngine@@UAA_NXZ",
        extern_interrupt,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?SetLastTimeStamp@CTiming@RabbitFactory@@QAAXK@Z",
        void_returning,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?IsActive@CTiming@RabbitFactory@@QAA_NXZ",
        is_active,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?LoopL@CGameEngine@@QAAXJ@Z",
        void_returning,
    );
    d.register_handler(
        "rabbitfactory.dll",
        "?StringCatenate@RabbitFactory@@YAXPADPBD@Z",
        string_catenate,
    );
}

fn br_init(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn br_malloc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let size = ctx.arg_u32(0)?.max(1);
    Ok(DispatchOutcome::ReturnedR0(
        ctx.kernel.heap.alloc(size).unwrap_or(0),
    ))
}

fn br_free(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    ctx.kernel.heap.free(ptr);
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn device_type(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn get_time(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn opaque_object(ctx: &mut CallCtx<'_>, size: u32) -> Result<DispatchOutcome, KernelError> {
    let object = ctx.kernel.heap.alloc(size).unwrap_or(0);
    if object == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let vtable = ctx.kernel.heap.alloc(0x100).unwrap_or(0);
    let method = ctx
        .kernel
        .dynamic_exports
        .get(&0x1000_0008)
        .and_then(|exports| exports.get("?GetDeviceType@CHLWinMobile@RabbitFactory@@QAAJXZ"))
        .copied()
        .unwrap_or(0);
    if vtable != 0 && method != 0 {
        let mut bytes = vec![0u8; 0x100];
        for slot in bytes.chunks_exact_mut(4) {
            slot.copy_from_slice(&method.to_le_bytes());
        }
        ctx.cpu.write_mem(vtable, &bytes)?;
        ctx.cpu.write_mem(object, &vtable.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(object))
}

fn create_game_manager(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = opaque_object(ctx, 0x400)?;
    if let DispatchOutcome::ReturnedR0(object) = ptr {
        ctx.cpu
            .write_mem(IMPORT_DATA_BASE + 0x1c, &object.to_le_bytes())?;
        Ok(DispatchOutcome::ReturnedR0(object))
    } else {
        Ok(ptr)
    }
}

fn create_hardware_layer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = opaque_object(ctx, 0x400)?;
    if let DispatchOutcome::ReturnedR0(object) = ptr {
        ctx.cpu
            .write_mem(IMPORT_DATA_BASE + 0x10, &object.to_le_bytes())?;
        Ok(DispatchOutcome::ReturnedR0(object))
    } else {
        Ok(ptr)
    }
}

fn initialize_cpp_object(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let this_ptr = ctx.arg_u32(0)?;
    if this_ptr == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let vtable = ctx.kernel.heap.alloc(0x100).unwrap_or(0);
    let method = ctx
        .kernel
        .dynamic_exports
        .get(&0x1000_0008)
        .and_then(|exports| {
            exports
                .get("?GetDeviceType@CHLWinMobile@RabbitFactory@@QAAJXZ")
                .copied()
        })
        .or_else(|| {
            ctx.kernel
                .dynamic_exports
                .get(&0x1000_0000)
                .and_then(|exports| exports.get("GetLastError"))
                .copied()
        })
        .unwrap_or(0);
    if vtable != 0 && method != 0 {
        let mut bytes = vec![0u8; 0x100];
        for slot in bytes.chunks_exact_mut(4) {
            slot.copy_from_slice(&method.to_le_bytes());
        }
        ctx.cpu.write_mem(vtable, &bytes)?;
        ctx.cpu.write_mem(this_ptr, &vtable.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(this_ptr))
}

fn string_catenate(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dst = ctx.arg_u32(0)?;
    let left = ctx.arg_u32(1)?;
    let right = ctx.arg_u32(2)?;
    if dst == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let read = |cpu: &mut dyn pocket_cpu::Cpu, ptr: u32| -> Result<Vec<u8>, KernelError> {
        if ptr == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for offset in 0..4096u32 {
            let byte = cpu.read_mem(ptr + offset, 1)?[0];
            out.push(byte);
            if byte == 0 {
                break;
            }
        }
        if out.last().copied() != Some(0) {
            out.push(0);
        }
        Ok(out)
    };
    let mut out = read(ctx.cpu, left)?;
    if out.last().copied() == Some(0) {
        out.pop();
    }
    let mut tail = read(ctx.cpu, right)?;
    out.append(&mut tail);
    if out.last().copied() != Some(0) {
        out.push(0);
    }
    ctx.cpu.write_mem(dst, &out)?;
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn get_gfx_engine(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let this_ptr = ctx.arg_u32(0)?;
    if this_ptr == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    Ok(DispatchOutcome::ReturnedR0(this_ptr))
}

fn get_orientation(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn set_origin(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn get_origin_x(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn get_origin_y(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn has_surface_ownership(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn screen_blitter_565(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let src = ctx.arg_u32(1)?;
    let len = ctx
        .arg_u32(2)?
        .min(ctx.kernel.framebuffer.pixels.len() as u32);
    if src != 0 && len != 0 {
        let pixels = ctx.cpu.read_mem(src, len)?;
        ctx.kernel.framebuffer.pixels[..len as usize].copy_from_slice(&pixels);
        ctx.kernel.framebuffer.mark_dirty();
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn extern_interrupt(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn is_active(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn get_key_handler(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.kernel.heap.alloc(0x40).unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(ptr))
}

fn get_stylus(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        ctx.kernel.heap.alloc(0x40).unwrap_or(0),
    ))
}

fn stylus_false(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn update_key_handler(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let key = ctx.arg_u32(1)? as u16;
    let pressed = ctx.arg_u32(2)? != 0;
    if ctx.kernel.pending_input.len() < 256 {
        ctx.kernel.pending_input.push_back(if pressed {
            InputEvent::KeyDown { vk: key }
        } else {
            InputEvent::KeyUp { vk: key }
        });
    }
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn register_game_x(d: &mut WinCeDispatcher) {
    for dll in ["ngamex.dll", "ngamex2k3.dll"] {
        d.register_handler(dll, "?GetSurface@nGameX@@QAAQAGXZ", get_surface);
        d.register_handler(dll, "?Update@nGameX@@SAXXZ", void_returning);
        d.register_handler(dll, "??1nGameX@@QAA@XZ", destructor);
        d.register_handler(dll, "??0nGameX@@QAA@PAUHWND__@@@Z", constructor);
        d.register_handler(dll, "?m_KeyList@nGameX@@2UGXKeyList@@A", key_list);
        d.register_handler(dll, "?GetScrWidth@nGameX@@QAA?BKXZ", screen_width);
        d.register_handler(dll, "?GetScrHeight@nGameX@@QAA?BKXZ", screen_height);
        d.register_handler(dll, "?Suspend@nGameX@@SAHXZ", success);
        d.register_handler(dll, "?Resume@nGameX@@SAHXZ", success);
    }
}

fn register_sound_x(d: &mut WinCeDispatcher) {
    let dll = "nsoundx.dll";
    d.register_handler(
        dll,
        "?Ns_WaveOpen@@YA_NPAUHWND__@@W4waveSpeak@@W4waveRate@@W4waveBits@@I@Z",
        success,
    );
    d.register_handler(dll, "?_Ns_LoadMod@@YA_NPAEKK@Z", success);
    d.register_handler(dll, "?Ns_FreeMod@@YAXXZ", void_returning);
    d.register_handler(dll, "?Ns_StopMod@@YAXXZ", void_returning);
    d.register_handler(dll, "?_Ns_PlayWave@@YA_NPAEH@Z", success);
    d.register_handler(dll, "?Ns_PlayMod@@YAXXZ", void_returning);
    d.register_handler(dll, "?Ns_WaveClose@@YA_NXZ", success);
}

fn constructor(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let this_ptr = ctx.arg_u32(0)?;
    if this_ptr != 0 {
        let _ = ctx.cpu.write_mem(this_ptr, &[0u8; 0x140]);
    }
    Ok(DispatchOutcome::ReturnedR0(this_ptr))
}

fn ensure_surface(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    if ctx.kernel.fb_mapped {
        return Ok(());
    }
    let bytes = (ctx.kernel.framebuffer.byte_size() + 0xfff) & !0xfff;
    ctx.cpu
        .map_region(SYNTHETIC_FRAMEBUFFER_BASE, bytes, Prot::READ | Prot::WRITE)?;
    ctx.cpu
        .write_mem(SYNTHETIC_FRAMEBUFFER_BASE, &ctx.kernel.framebuffer.pixels)?;
    ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
    ctx.kernel.fb_mapped = true;
    Ok(())
}

fn get_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ensure_surface(ctx)?;
    Ok(DispatchOutcome::ReturnedR0(SYNTHETIC_FRAMEBUFFER_BASE))
}

fn key_list(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn screen_width(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.framebuffer.width))
}

fn screen_height(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.framebuffer.height))
}

fn destructor(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn success(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(1))
}

fn void_returning(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}
