//! `zlib.dll` -- the `gz*` stdio-style API.
//!
//! Several Pocket PC titles ship a copy of zlib alongside the game and
//! read their data straight out of gzip files: Rayman Ultimate keeps
//! its level data in `PCMAP\ALLFIX.DAT.gz` and friends, and a failed
//! `gzopen` there surfaces as the game's own "Can not open file ...
//! (load_all_fix)" dialog.
//!
//! The implementation decompresses eagerly: `gzopen` reads the whole
//! host file through the VFS, inflates it into memory, and every later
//! call just walks that buffer. Real zlib streams incrementally, but
//! these are level/asset files of a few hundred KB at most, and doing
//! it up front keeps `gzseek` (which games use freely, including
//! seeking backwards) trivial and exact -- streaming would otherwise
//! need to re-inflate from the start on every rewind.
//!
//! Following zlib's own behaviour, a file that is *not* gzip-compressed
//! is passed through verbatim rather than treated as an error: real
//! `gzread` transparently reads uncompressed files, and titles rely on
//! that to ship the same code path for packed and loose data.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use pocket_kernel::{DispatchOutcome, KernelError};

use crate::coredll::{open_cstr_path, read_cstr_string};
use crate::{CallCtx, WinCeDispatcher};

/// Base for synthesized `gzFile` handles. Deliberately far away from
/// the VFS's own handle space so a `gzFile` accidentally passed to
/// `fread` (or vice versa) fails loudly instead of reading a
/// half-unrelated file.
const GZ_HANDLE_BASE: u32 = 0x477A_0000;

/// zlib's `Z_OK` / `Z_STREAM_ERROR`.
const Z_OK: u32 = 0;
const Z_STREAM_ERROR: u32 = (-2i32) as u32;

struct GzFile {
    /// Fully decompressed contents.
    data: Vec<u8>,
    /// Read cursor within `data`.
    pos: usize,
}

static OPEN_FILES: Lazy<Mutex<HashMap<u32, GzFile>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static NEXT_HANDLE: Mutex<u32> = Mutex::new(GZ_HANDLE_BASE);

/// DLL names the `gz*` entry points show up under.
///
/// zlib is a static library, so a title that uses it exports the
/// symbols from whatever DLL it happened to get linked into. Rayman
/// Ultimate ships it as `zlib.dll`; a different build of the same game
/// exports the identical functions from `alib.dll`. The
/// implementation does not care which, so register the same handlers
/// for each known spelling rather than duplicating them.
const ZLIB_DLL_NAMES: &[&str] = &["zlib.dll", "alib.dll", "zlibce.dll", "libz.dll"];

pub fn register(d: &mut WinCeDispatcher) {
    for &dll in ZLIB_DLL_NAMES {
        d.register_handler(dll, "gzopen", gzopen);
        d.register_handler(dll, "gzread", gzread);
        d.register_handler(dll, "gzclose", gzclose);
        d.register_handler(dll, "gzseek", gzseek);
        d.register_handler(dll, "gztell", gztell);
        d.register_handler(dll, "gzrewind", gzrewind);
        d.register_handler(dll, "gzeof", gzeof);
        d.register_handler(dll, "gzgetc", gzgetc);
        // Sizing helpers some builds use instead of seek/tell. Not
        // real zlib exports in every version, but harmless to offer.
        d.register_handler(dll, "gzlength", gzlength);
        d.register_handler(dll, "gzsize", gzlength);
    }
}

/// Decompress `raw` if it carries the gzip magic, otherwise hand it
/// back untouched (see the module docs -- this mirrors zlib's
/// transparent handling of uncompressed input).
fn inflate_if_gzip(raw: Vec<u8>, path: &str) -> Vec<u8> {
    if raw.len() < 2 || raw[0] != 0x1f || raw[1] != 0x8b {
        log::debug!("gzopen({path:?}): not gzip-compressed, passing through {} bytes", raw.len());
        return raw;
    }
    let mut out = Vec::new();
    let mut decoder = flate2::read::GzDecoder::new(&raw[..]);
    match decoder.read_to_end(&mut out) {
        Ok(_) => {
            log::debug!(
                "gzopen({path:?}): inflated {} bytes -> {} bytes",
                raw.len(),
                out.len()
            );
            out
        }
        Err(e) => {
            // A truncated or corrupt member still yields whatever
            // inflated cleanly before the error; a game reading a
            // partially-recoverable asset is better off with that than
            // with nothing.
            log::warn!("gzopen({path:?}): inflate failed after {} bytes: {e}", out.len());
            out
        }
    }
}

/// `gzFile gzopen(const char *path, const char *mode)`
fn gzopen(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let path_p = ctx.arg_u32(0)?;
    let mode_p = ctx.arg_u32(1)?;
    let path = read_cstr_string(ctx, path_p, 260)?;
    let mode = read_cstr_string(ctx, mode_p, 8)?;

    // Writing a gzip stream is not supported; report failure rather
    // than silently accepting data the game would never get back.
    if mode.starts_with('w') || mode.starts_with('a') {
        log::warn!("gzopen({path:?}, {mode:?}): write modes are not supported -> NULL");
        return Ok(DispatchOutcome::ReturnedR0(0));
    }

    // Reuse the CRT path resolver so all the guest-path spellings and
    // mount fallbacks that `fopen` understands work here too.
    let handle = open_cstr_path(ctx, &path, "rb");
    if handle == 0 {
        log::debug!("gzopen({path:?}, {mode:?}) -> NULL (not found)");
        return Ok(DispatchOutcome::ReturnedR0(0));
    }

    let mut raw = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match ctx.kernel.vfs.read(handle, &mut chunk) {
            Some(0) | None => break,
            Some(n) => raw.extend_from_slice(&chunk[..n]),
        }
    }
    ctx.kernel.vfs.close(handle);

    let data = inflate_if_gzip(raw, &path);

    let mut next = NEXT_HANDLE.lock().unwrap();
    let gz_handle = *next;
    *next = next.wrapping_add(1);
    drop(next);
    OPEN_FILES
        .lock()
        .unwrap()
        .insert(gz_handle, GzFile { data, pos: 0 });
    log::debug!("gzopen({path:?}, {mode:?}) -> 0x{gz_handle:08x}");
    Ok(DispatchOutcome::ReturnedR0(gz_handle))
}

/// `int gzread(gzFile file, voidp buf, unsigned len)`
fn gzread(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let buf = ctx.arg_u32(1)?;
    let len = ctx.arg_u32(2)? as usize;

    let chunk = {
        let mut files = OPEN_FILES.lock().unwrap();
        let Some(file) = files.get_mut(&handle) else {
            log::warn!("gzread(0x{handle:08x}): unknown handle -> -1");
            return Ok(DispatchOutcome::ReturnedR0(Z_STREAM_ERROR));
        };
        let end = file.data.len().min(file.pos.saturating_add(len));
        let chunk = file.data[file.pos..end].to_vec();
        file.pos = end;
        chunk
    };
    if buf != 0 && !chunk.is_empty() {
        ctx.cpu.write_mem(buf, &chunk)?;
    }
    log::trace!("gzread(0x{handle:08x}, {len}) -> {}", chunk.len());
    Ok(DispatchOutcome::ReturnedR0(chunk.len() as u32))
}

/// `int gzclose(gzFile file)`
fn gzclose(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let removed = OPEN_FILES.lock().unwrap().remove(&handle).is_some();
    log::debug!("gzclose(0x{handle:08x}) -> {}", if removed { "Z_OK" } else { "Z_STREAM_ERROR" });
    Ok(DispatchOutcome::ReturnedR0(if removed {
        Z_OK
    } else {
        Z_STREAM_ERROR
    }))
}

/// `z_off_t gzseek(gzFile file, z_off_t offset, int whence)`
fn gzseek(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let offset = ctx.arg_u32(1)? as i32;
    let whence = ctx.arg_u32(2)?;
    let mut files = OPEN_FILES.lock().unwrap();
    let Some(file) = files.get_mut(&handle) else {
        log::debug!("gzseek(0x{handle:08x}, {offset}, {whence}): unknown handle -> -1");
        return Ok(DispatchOutcome::ReturnedR0((-1i32) as u32));
    };
    // SEEK_SET = 0, SEEK_CUR = 1, SEEK_END = 2.
    //
    // Real zlib refuses SEEK_END on a read stream, because it is
    // decompressing incrementally and does not know where the end is
    // without inflating everything first. We already have the whole
    // file in memory, so we can answer exactly -- and it matters:
    // seek-to-end followed by gztell is the standard way to size a
    // file, and returning -1 here left Rayman computing a length of
    // zero and then subtracting a 128-byte header, which is where the
    // 0xFFFFFF80 allocation came from.
    let base = match whence {
        0 => 0i64,
        1 => file.pos as i64,
        2 => file.data.len() as i64,
        _ => {
            log::debug!("gzseek(0x{handle:08x}): bad whence {whence} -> -1");
            return Ok(DispatchOutcome::ReturnedR0((-1i32) as u32));
        }
    };
    let target = (base + offset as i64).clamp(0, file.data.len() as i64);
    file.pos = target as usize;
    log::trace!(
        "gzseek(0x{handle:08x}, {offset}, whence={whence}) -> {} (of {})",
        file.pos,
        file.data.len()
    );
    Ok(DispatchOutcome::ReturnedR0(file.pos as u32))
}

/// `z_off_t gztell(gzFile file)`
fn gztell(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let files = OPEN_FILES.lock().unwrap();
    let pos = files
        .get(&handle)
        .map(|f| f.pos as u32)
        .unwrap_or((-1i32) as u32);
    log::trace!("gztell(0x{handle:08x}) -> {pos}");
    Ok(DispatchOutcome::ReturnedR0(pos))
}

/// `int gzrewind(gzFile file)`
fn gzrewind(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let mut files = OPEN_FILES.lock().unwrap();
    match files.get_mut(&handle) {
        Some(file) => {
            file.pos = 0;
            Ok(DispatchOutcome::ReturnedR0(Z_OK))
        }
        None => Ok(DispatchOutcome::ReturnedR0(Z_STREAM_ERROR)),
    }
}

/// Uncompressed length of an open file. Not a standard zlib export,
/// but some ports add one; costs nothing to answer since we already
/// hold the inflated bytes.
fn gzlength(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let files = OPEN_FILES.lock().unwrap();
    let len = files
        .get(&handle)
        .map(|f| f.data.len() as u32)
        .unwrap_or((-1i32) as u32);
    log::debug!("gzlength(0x{handle:08x}) -> {len}");
    Ok(DispatchOutcome::ReturnedR0(len))
}

/// `int gzeof(gzFile file)`
fn gzeof(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let files = OPEN_FILES.lock().unwrap();
    let eof = files
        .get(&handle)
        .map(|f| u32::from(f.pos >= f.data.len()))
        .unwrap_or(1);
    Ok(DispatchOutcome::ReturnedR0(eof))
}

/// `int gzgetc(gzFile file)` -- returns the byte, or -1 at EOF.
fn gzgetc(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let handle = ctx.arg_u32(0)?;
    let mut files = OPEN_FILES.lock().unwrap();
    let Some(file) = files.get_mut(&handle) else {
        return Ok(DispatchOutcome::ReturnedR0((-1i32) as u32));
    };
    match file.data.get(file.pos).copied() {
        Some(byte) => {
            file.pos += 1;
            Ok(DispatchOutcome::ReturnedR0(u32::from(byte)))
        }
        None => Ok(DispatchOutcome::ReturnedR0((-1i32) as u32)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_through_uncompressed_input() {
        let raw = b"plain bytes".to_vec();
        assert_eq!(inflate_if_gzip(raw.clone(), "x"), raw);
    }

    #[test]
    fn inflates_gzip_input() {
        use std::io::Write;
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"hello rayman").unwrap();
        let packed = encoder.finish().unwrap();
        assert_eq!(inflate_if_gzip(packed, "x"), b"hello rayman");
    }
}
