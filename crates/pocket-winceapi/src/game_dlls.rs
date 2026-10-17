//! HLE implementations for native libraries bundled with Windows CE games.

use image::GenericImageView;
use pocket_cpu::Prot;
use pocket_kernel::{DispatchOutcome, KernelError, SYNTHETIC_FRAMEBUFFER_BASE};

use crate::{CallCtx, WinCeDispatcher};

pub fn register(d: &mut WinCeDispatcher) {
    register_game_x(d);
    register_sound_x(d);
    register_image_library(d);

    d.register_handler("note_prj.dll", "ord:7", find_first_flash_card);
    d.register_handler("note_prj.dll", "#7", find_first_flash_card);
    d.register_handler("note_prj.dll", "ord:8", find_next_flash_card);
    d.register_handler("note_prj.dll", "#8", find_next_flash_card);
    register_alib(d);
}

fn register_image_library(d: &mut WinCeDispatcher) {
    d.register_handler(
        "imagelib_ce.dll",
        "?IMG_LoadImageStream@@YAPAIPAEIPAI1PA_N@Z",
        img_load_image_stream,
    );
    d.register_handler(
        "imagelib_ce.dll",
        "?IMG_GetLastError@@YAHXZ",
        img_get_last_error,
    );
}

fn img_load_image_stream(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    const INVALID_IMAGE: u32 = 2;
    const OUT_OF_MEMORY: u32 = 1;
    const MAX_IMAGE_STREAM_BYTES: u32 = 16 * 1024 * 1024;

    ctx.kernel.image_loader_error = 0;
    let input = ctx.arg_u32(0)?;
    let input_len = ctx.arg_u32(1)?;
    let width_out = ctx.arg_u32(2)?;
    let height_out = ctx.arg_u32(3)?;
    let alpha_out = ctx.arg_u32(4)?;
    if input == 0
        || input_len == 0
        || input_len > MAX_IMAGE_STREAM_BYTES
        || width_out == 0
        || height_out == 0
    {
        ctx.kernel.image_loader_error = INVALID_IMAGE;
        return Ok(DispatchOutcome::ReturnedR0(0));
    }

    let bytes = match ctx.cpu.read_mem(input, input_len) {
        Ok(bytes) => bytes,
        Err(error) => {
            log::debug!("IMG_LoadImageStream could not read guest input: {error}");
            ctx.kernel.image_loader_error = INVALID_IMAGE;
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    let format = match image::guess_format(&bytes) {
        Ok(format @ (image::ImageFormat::Png | image::ImageFormat::Jpeg)) => format,
        _ => {
            ctx.kernel.image_loader_error = INVALID_IMAGE;
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    let png_without_iend = if format == image::ImageFormat::Png && png_stream_missing_iend(&bytes) {
        // Active Ball's packaged system.png ends after a complete IDAT payload without IEND.
        let mut complete = bytes.clone();
        complete.extend_from_slice(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82]);
        Some(complete)
    } else {
        None
    };
    if png_without_iend.is_some() {
        log::debug!("IMG_LoadImageStream accepted a PNG stream without its terminal IEND chunk");
    }
    let decode_bytes = png_without_iend.as_deref().unwrap_or(bytes.as_slice());
    let decoded = match image::load_from_memory_with_format(decode_bytes, format) {
        Ok(decoded) => decoded,
        Err(error) => {
            log::debug!("IMG_LoadImageStream decode failed: {error}");
            ctx.kernel.image_loader_error = INVALID_IMAGE;
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    let (width, height) = decoded.dimensions();
    let Some(pixel_bytes) = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|bytes| *bytes > 0 && *bytes <= ctx.kernel.heap.size())
    else {
        ctx.kernel.image_loader_error = OUT_OF_MEMORY;
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let has_alpha = decoded.color().has_alpha();
    let rgba = decoded.to_rgba8();
    let mut argb = Vec::with_capacity(pixel_bytes as usize);
    for pixel in rgba.pixels() {
        let [red, green, blue, alpha] = pixel.0;
        argb.extend_from_slice(&[blue, green, red, alpha]);
    }
    let Some(pixels_ptr) = ctx.kernel.heap.alloc(pixel_bytes) else {
        ctx.kernel.image_loader_error = OUT_OF_MEMORY;
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    if let Err(error) = ctx.cpu.write_mem(pixels_ptr, &argb) {
        ctx.kernel.heap.free(pixels_ptr);
        log::debug!("IMG_LoadImageStream could not write decoded pixels: {error}");
        ctx.kernel.image_loader_error = OUT_OF_MEMORY;
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if let Err(error) = ctx.cpu.write_mem(width_out, &width.to_le_bytes()) {
        ctx.kernel.heap.free(pixels_ptr);
        log::debug!("IMG_LoadImageStream could not write image width: {error}");
        ctx.kernel.image_loader_error = INVALID_IMAGE;
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if let Err(error) = ctx.cpu.write_mem(height_out, &height.to_le_bytes()) {
        ctx.kernel.heap.free(pixels_ptr);
        log::debug!("IMG_LoadImageStream could not write image height: {error}");
        ctx.kernel.image_loader_error = INVALID_IMAGE;
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    if alpha_out != 0 {
        if let Err(error) = ctx.cpu.write_mem(alpha_out, &[u8::from(has_alpha)]) {
            ctx.kernel.heap.free(pixels_ptr);
            log::debug!("IMG_LoadImageStream could not write alpha flag: {error}");
            ctx.kernel.image_loader_error = INVALID_IMAGE;
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    }
    log::debug!("IMG_LoadImageStream decoded {width}x{height} ({format:?})");
    Ok(DispatchOutcome::ReturnedR0(pixels_ptr))
}

fn img_get_last_error(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(ctx.kernel.image_loader_error))
}

// Only complete PNG chunk streams with image data qualify for the missing-IEND fallback.
fn png_stream_missing_iend(bytes: &[u8]) -> bool {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(SIGNATURE) {
        return false;
    }
    let mut offset = SIGNATURE.len();
    let mut saw_idat = false;
    while offset + 12 <= bytes.len() {
        let chunk_len = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let Some(end) = offset
            .checked_add(12)
            .and_then(|start| start.checked_add(chunk_len))
        else {
            return false;
        };
        if end > bytes.len() {
            return false;
        }
        let chunk_type = &bytes[offset + 4..offset + 8];
        if chunk_type == b"IEND" {
            return false;
        }
        saw_idat |= chunk_type == b"IDAT";
        offset = end;
    }
    saw_idat && offset == bytes.len()
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

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
    use pocket_kernel::Thunk;
    use pocket_pe::ImportBinding;

    fn image_thunk() -> Thunk {
        Thunk {
            thunk_va: 0x7000_0000,
            iat_va: 0x2000,
            dll: "imagelib_ce.dll".to_string(),
            binding: ImportBinding::Name("?IMG_LoadImageStream@@YAPAIPAEIPAI1PA_N@Z".to_string()),
            friendly_name: None,
        }
    }

    #[test]
    fn image_stream_decodes_png_to_argb_and_reports_errors() {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[0x11, 0x22, 0x33, 0x44], 1, 1, image::ColorType::Rgba8)
            .unwrap();
        assert!(png_stream_missing_iend(&png[..png.len() - 12]));
        assert!(!png_stream_missing_iend(&png[..png.len() - 13]));
        png.truncate(png.len() - 12);

        let mut cpu = StubCpu::new();
        let mut kernel = crate::coredll::tests::fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.map_region(
            kernel.heap.base(),
            kernel.heap.size(),
            Prot::READ | Prot::WRITE,
        )
        .unwrap();
        cpu.write_mem(0x1000, &png).unwrap();
        cpu.write_reg(ArmReg::R0, 0x1000).unwrap();
        cpu.write_reg(ArmReg::R1, png.len() as u32).unwrap();
        cpu.write_reg(ArmReg::R2, 0x1100).unwrap();
        cpu.write_reg(ArmReg::R3, 0x1104).unwrap();
        cpu.write_mem(0x1108, &[0]).unwrap();
        cpu.write_reg(ArmReg::Sp, 0x1300).unwrap();
        let alpha_arg = 0x1300 + cpu.stack_arg_offset();
        cpu.write_mem(alpha_arg, &0x1108u32.to_le_bytes()).unwrap();
        let thunk = image_thunk();
        let heap_before = kernel.heap.free_bytes();
        let pixels = {
            let mut ctx = CallCtx {
                cpu: &mut cpu,
                thunk: &thunk,
                kernel: &mut kernel,
            };
            match img_load_image_stream(&mut ctx).unwrap() {
                DispatchOutcome::ReturnedR0(pointer) => pointer,
                other => panic!("unexpected image loader result {other:?}"),
            }
        };

        assert_ne!(pixels, 0);
        assert_eq!(cpu.read_mem(pixels, 4).unwrap(), [0x33, 0x22, 0x11, 0x44]);
        assert_eq!(cpu.read_u32_le(0x1100).unwrap(), 1);
        assert_eq!(cpu.read_u32_le(0x1104).unwrap(), 1);
        assert_eq!(cpu.read_mem(0x1108, 1).unwrap(), [1]);
        assert_eq!(kernel.image_loader_error, 0);
        let last_error = {
            let mut ctx = CallCtx {
                cpu: &mut cpu,
                thunk: &thunk,
                kernel: &mut kernel,
            };
            img_get_last_error(&mut ctx).unwrap()
        };
        assert_eq!(last_error, DispatchOutcome::ReturnedR0(0));
        kernel.heap.free(pixels);
        assert_eq!(kernel.heap.free_bytes(), heap_before);

        cpu.write_mem(0x1000, b"not an image").unwrap();
        cpu.write_reg(ArmReg::R1, 12).unwrap();
        let result = {
            let mut ctx = CallCtx {
                cpu: &mut cpu,
                thunk: &thunk,
                kernel: &mut kernel,
            };
            img_load_image_stream(&mut ctx).unwrap()
        };
        assert_eq!(result, DispatchOutcome::ReturnedR0(0));
        assert_eq!(kernel.image_loader_error, 2);
        let last_error = {
            let mut ctx = CallCtx {
                cpu: &mut cpu,
                thunk: &thunk,
                kernel: &mut kernel,
            };
            img_get_last_error(&mut ctx).unwrap()
        };
        assert_eq!(last_error, DispatchOutcome::ReturnedR0(2));
    }
}
