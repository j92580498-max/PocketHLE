//! Stubs for the small native game libraries bundled with MetalStrike.

use pocket_cpu::Prot;
use pocket_kernel::{DispatchOutcome, KernelError, SYNTHETIC_FRAMEBUFFER_BASE};

use crate::{CallCtx, WinCeDispatcher};

pub fn register(d: &mut WinCeDispatcher) {
    register_game_x(d);
    register_sound_x(d);
    d.register_handler("note_prj.dll", "ord:7", find_first_flash_card);
    d.register_handler("note_prj.dll", "#7", find_first_flash_card);
    d.register_handler("note_prj.dll", "ord:8", find_next_flash_card);
    d.register_handler("note_prj.dll", "#8", find_next_flash_card);
    register_alib(d);
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

// ---------- alib.dll — Rayman Ultimate's zlib wrapper ----------
//
// Rayman Ultimate links its asset loader against `alib.dll`, a small
// zlib wrapper DLL shipped beside the executable, and streams every
// map asset (`PCMAP/*.gz`, `*.lev.gz`) through `gzopen` / `gzread`.
// The HLE intercepts those imports at the IAT boundary, so the decoder
// lives host-side in `pocket_kernel::gz`: `gzopen` slurps and inflates
// the whole file once (the largest file the game ships is well under
// 1 MB compressed) and `gzread` / `gzseek` serve out of the decoded
// bytes.

const ALIB_DLL: &str = "alib.dll";

fn register_alib(d: &mut WinCeDispatcher) {
    d.register_handler(ALIB_DLL, "gzopen", gz_open);
    d.register_handler(ALIB_DLL, "gzread", gz_read);
    d.register_handler(ALIB_DLL, "gzseek", gz_seek);
    d.register_handler(ALIB_DLL, "gzclose", gz_close);
    // `gzprintf` / `gzputs` are not imported by anything we have seen,
    // so they stay absent on purpose: a stub that answers success for
    // a name the game does not import is worse than no stub.
}

/// `gzFile gzopen(const char *path, const char *mode)`
///
/// `path` is a plain guest ANSI path, resolved through the same VFS
/// every other file API uses. Inflate happens eagerly: a gzip member
/// read back with `gzread` is a stream, and a stream that can only be
/// served while the guest keeps calling is exactly the shape of bug
/// that never shows up in a trace until a level fails to load.
fn gz_open(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let path_p = ctx.arg_u32(0)?;
    if path_p == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bytes = read_guest_cstr(ctx, path_p)?;
    let path = String::from_utf8_lossy(&bytes).into_owned();
    let Some(host_path) = ctx.kernel.vfs.resolve(&path) else {
        log::debug!("gzopen({path:?}) -> NULL (not mounted)");
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    match ctx.kernel.gz_files.open(&host_path) {
        Some(handle) => {
            log::debug!("gzopen({path:?}) -> 0x{handle:08x}");
            Ok(DispatchOutcome::ReturnedR0(handle))
        }
        None => {
            log::debug!("gzopen({path:?}) -> NULL (open or inflate failed)");
            Ok(DispatchOutcome::ReturnedR0(0))
        }
    }
}

/// `int gzread(gzFile file, voidp buf, unsigned len)`
///
/// Returns the number of bytes copied; 0 means end of stream, which is
/// how zlib reports EOF on a healthy file.
fn gz_read(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let buf = ctx.arg_u32(1)?;
    let len = ctx.arg_u32(2)? as usize;
    if buf == 0 || len == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut chunk = vec![0u8; len];
    ctx.cpu.read_mem_into(buf, &mut chunk)?;
    let n = ctx.kernel.gz_files.read(handle, &mut chunk);
    if n > 0 {
        ctx.cpu.write_mem(buf, &chunk)?;
    }
    Ok(DispatchOutcome::ReturnedR0(n as u32))
}

/// `off_t gzseek(gzFile file, off_t offset, int whence)`
///
/// The stream is fully resident, so any position inside the decoded
/// bytes is fine; zlib itself only guarantees forward seeks, and the
/// game only ever seeks forward to a stored chunk boundary.
fn gz_seek(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let offset = ctx.arg_u32(1)? as i64;
    let whence = ctx.arg_u32(2)?;
    match ctx.kernel.gz_files.seek(handle, offset, whence) {
        Some(pos) => Ok(DispatchOutcome::ReturnedR0(pos as u32)),
        None => Ok(DispatchOutcome::ReturnedR0(0xffff_ffff)),
    }
}

/// `int gzclose(gzFile file)` — zlib answers 0 (`Z_OK`) on success.
fn gz_close(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(
        if ctx.kernel.gz_files.close(handle) {
            0
        } else {
            0xffff_ffff
        },
    ))
}

fn read_guest_cstr(ctx: &mut CallCtx<'_>, p: u32) -> Result<Vec<u8>, KernelError> {
    let mut out = Vec::new();
    let mut addr = p;
    // Paths are short; a bounded scan keeps a bad pointer from running
    // away through the whole address space.
    for _ in 0..0x1000 {
        let byte = ctx.cpu.read_mem(addr, 1)?[0];
        if byte == 0 {
            break;
        }
        out.push(byte);
        addr = addr.wrapping_add(1);
    }
    Ok(out)
}
