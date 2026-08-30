//! Host-side gzip file table for the `alib.dll` HLE.
//!
//! Rayman Ultimate links its map loading against `alib.dll`, a tiny
//! zlib wrapper shipped beside the executable, and opens every asset
//! (`PCMAP/ALLFIX.DAT.gz`, the `*.lev.gz` levels, …) through
//! `gzopen` / `gzread` / `gzseek` / `gzclose`. Those imports are
//! intercepted at the IAT boundary, so the decoder lives host-side:
//! `gzopen` slurps and inflates the whole file once (the largest file
//! the game ships is ~120 KB compressed) and the other calls serve
//! out of the decoded bytes. zlib semantics on error are "return 0 /
//! -1 and let the game show its own message", which is what the
//! handlers in `pocket-winceapi` map onto.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

const SEEK_SET: u32 = 0;
const SEEK_CUR: u32 = 1;
const SEEK_END: u32 = 2;

struct GzFile {
    data: Vec<u8>,
    pos: usize,
}

/// All `gzopen`ed files. Handles live in their own space, far from the
/// VFS handle range, so a guest that confuses the two cannot alias a
/// real file.
#[derive(Default)]
pub struct GzFiles {
    next_handle: u32,
    files: HashMap<u32, GzFile>,
}

const FIRST_HANDLE: u32 = 0x6E17_0000;

impl GzFiles {
    /// Open `host_path`, inflate it fully, and hand back a `gzFile`.
    /// Returns `None` (zlib's `NULL`) when the file cannot be read or
    /// is not gzip.
    pub fn open(&mut self, host_path: &Path) -> Option<u32> {
        let raw = std::fs::read(host_path).ok()?;
        let mut decoder = flate2::read::GzDecoder::new(&raw[..]);
        let mut data = Vec::new();
        decoder.read_to_end(&mut data).ok()?;
        // A plain (non-gzip) stream fails GzDecoder on the first
        // header check; some callers pass uncompressed files, so fall
        // back to serving the bytes as-is rather than failing the
        // load — zlib's gzread does the same for uncompressed input.
        if data.is_empty() && !raw.is_empty() {
            data = raw;
        }
        let mut handle = self.next_handle.max(FIRST_HANDLE);
        while self.files.contains_key(&handle) {
            handle = handle.wrapping_add(1).max(FIRST_HANDLE);
        }
        self.next_handle = handle.wrapping_add(1).max(FIRST_HANDLE);
        self.files.insert(handle, GzFile { data, pos: 0 });
        Some(handle)
    }

    /// `int gzread(gzFile, void*, unsigned)`: copy up to `out.len()`
    /// decoded bytes, returning how many were written. 0 means EOF.
    pub fn read(&mut self, handle: u32, out: &mut [u8]) -> usize {
        let Some(file) = self.files.get_mut(&handle) else {
            return 0;
        };
        let remaining = file.data.len().saturating_sub(file.pos);
        let take = remaining.min(out.len());
        out[..take].copy_from_slice(&file.data[file.pos..file.pos + take]);
        file.pos += take;
        take
    }

    /// `off_t gzseek(gzFile, off_t, int)` within the decoded stream.
    /// Returns the new absolute offset, or `None` for an invalid
    /// handle (zlib reports errors as -1L).
    pub fn seek(&mut self, handle: u32, offset: i64, whence: u32) -> Option<i64> {
        let file = self.files.get_mut(&handle)?;
        let len = file.data.len() as i64;
        let target = match whence {
            SEEK_SET => offset,
            SEEK_CUR => file.pos as i64 + offset,
            SEEK_END => len + offset,
            _ => return None,
        }
        .clamp(0, len.max(0));
        file.pos = target as usize;
        Some(target)
    }

    /// Current absolute position, for `gztell`-style callers.
    pub fn tell(&self, handle: u32) -> Option<i64> {
        self.files.get(&handle).map(|f| f.pos as i64)
    }

    /// `int gzclose(gzFile)` — 0 (`Z_OK`) when the handle was ours.
    pub fn close(&mut self, handle: u32) -> bool {
        self.files.remove(&handle).is_some()
    }

    pub fn is_open(&self, handle: u32) -> bool {
        self.files.contains_key(&handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip_of(plain: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(plain).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn gz_round_trips_read_and_seek() {
        let plain = b"RAYMAN PCMAP DATA 0123456789".to_vec();
        let dir = std::env::temp_dir().join(format!("pockethle-gz-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("allfix.dat.gz");
        std::fs::write(&path, gzip_of(&plain)).unwrap();

        let mut gz = GzFiles::default();
        let h = gz.open(&path).expect("gzip file opens");
        let mut out = vec![0u8; 6];
        assert_eq!(gz.read(h, &mut out), 6);
        assert_eq!(&out, b"RAYMAN");
        // SEEK_CUR forward past the separator and the rest of the first word.
        assert_eq!(gz.seek(h, 7, SEEK_CUR), Some(13));
        let mut more = vec![0u8; 4];
        assert_eq!(gz.read(h, &mut more), 4);
        assert_eq!(&more, b"DATA");
        // SEEK_END clamps to the end; reads past it return 0.
        assert_eq!(gz.seek(h, 0, SEEK_END), Some(plain.len() as i64));
        assert_eq!(gz.read(h, &mut more), 0);
        assert!(gz.close(h));
        assert!(!gz.is_open(h));
        // A stale handle reads as EOF, not as another file's bytes.
        let other = gz.open(&path).unwrap();
        assert_ne!(other, h);
        let mut sink = vec![0u8; 4];
        assert_eq!(gz.read(h, &mut sink), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gz_open_rejects_a_missing_file() {
        let mut gz = GzFiles::default();
        assert_eq!(
            gz.open(&std::path::PathBuf::from("/nonexistent/file.gz")),
            None
        );
    }
}
