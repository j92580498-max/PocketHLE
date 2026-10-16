//! Single-owner wrappers for PocketHLE's opaque, statically linked decoder.
use std::{
    ffi::{c_char, c_int, c_void, CString},
    path::Path,
    ptr::NonNull,
};

extern "C" {
    fn pocket_media_open(
        path: *const c_char,
        video: c_int,
        width: c_int,
        height: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn pocket_media_close(media: *mut c_void);
    fn pocket_media_info(
        media: *mut c_void,
        duration: *mut f64,
        width: *mut c_int,
        height: *mut c_int,
        interval: *mut f64,
        bitrate: *mut i64,
    );
    fn pocket_media_seek(media: *mut c_void, seconds: f64) -> c_int;
    fn pocket_media_next(
        media: *mut c_void,
        data: *mut *const u8,
        length: *mut c_int,
        seconds: *mut f64,
    ) -> c_int;
    fn pocket_media_error(code: c_int, text: *mut c_char, capacity: c_int);
}

pub(crate) struct Decoder(NonNull<c_void>);
// FFmpeg contexts are not accessed concurrently. Ownership transfers to one
// worker, which performs all reads and drops the context on the same worker.
unsafe impl Send for Decoder {}

fn error(code: i32) -> String {
    let mut text = [0u8; 256];
    unsafe {
        pocket_media_error(code, text.as_mut_ptr().cast(), text.len() as i32);
    }
    let end = text.iter().position(|&v| v == 0).unwrap_or(text.len());
    String::from_utf8_lossy(&text[..end]).into_owned()
}

impl Decoder {
    pub(crate) fn info(&self) -> (f64, u32, u32, f64, i64) {
        let (mut d, mut w, mut h, mut i, mut b) = (0.0, 0, 0, 0.0, 0);
        unsafe {
            pocket_media_info(self.0.as_ptr(), &mut d, &mut w, &mut h, &mut i, &mut b);
        }
        (d, w as u32, h as u32, i, b)
    }
    pub(crate) fn seek(&mut self, seconds: f64) -> Result<(), String> {
        let code = unsafe { pocket_media_seek(self.0.as_ptr(), seconds) };
        if code < 0 {
            Err(error(code))
        } else {
            Ok(())
        }
    }
    pub(crate) fn open(
        path: &Path,
        video: bool,
        width: u32,
        height: u32,
    ) -> Result<Option<Self>, String> {
        let path = CString::new(path.to_str().ok_or("Media path is not valid UTF-8")?)
            .map_err(|_| "Media path contains NUL")?;
        let mut code = 0;
        let handle = unsafe {
            pocket_media_open(
                path.as_ptr(),
                i32::from(video),
                width as i32,
                height as i32,
                &mut code,
            )
        };
        if let Some(handle) = NonNull::new(handle) {
            Ok(Some(Self(handle)))
        } else if code == 1 {
            Ok(None)
        } else {
            Err(error(code))
        }
    }

    pub(crate) fn next(&mut self) -> Result<Option<(Vec<u8>, f64)>, String> {
        let mut data = std::ptr::null();
        let mut length = 0;
        let mut seconds = 0.0;
        let code =
            unsafe { pocket_media_next(self.0.as_ptr(), &mut data, &mut length, &mut seconds) };
        if code < 0 {
            return Err(error(code));
        }
        if code == 0 {
            return Ok(None);
        }
        if data.is_null() || length < 0 {
            return Err("Invalid native media buffer".into());
        }
        // Copy before calling the decoder again: the native buffer is borrowed.
        let bytes = unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec();
        Ok(Some((bytes, seconds)))
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            pocket_media_close(self.0.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn static_media_decodes_pcm_and_distinguishes_a_missing_video_stream() {
        let file =
            std::env::temp_dir().join(format!("pockethle-static-media-{}.wav", std::process::id()));
        let samples: Vec<i16> = (0..1024).map(|n| n as i16 * 13 - 6000).collect();
        let pcm: Vec<u8> = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&44100u32.to_le_bytes());
        wav.extend_from_slice(&176400u32.to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(&pcm);
        std::fs::write(&file, wav).unwrap();
        let mut decoder = Decoder::open(&file, false, 0, 0).unwrap().unwrap();
        let mut decoded = Vec::new();
        while let Some((bytes, _)) = decoder.next().unwrap() {
            decoded.extend(bytes);
        }
        assert_eq!(decoded, pcm);
        assert!(Decoder::open(&file, true, 320, 240).unwrap().is_none());
        drop(decoder);
        std::fs::remove_file(file).unwrap();
    }
}
#[cfg(test)]
mod seeking_tests {
    use super::*;
    #[test]
    fn real_wmv_metadata_and_keyframe_seek_keep_absolute_timestamps() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/seeking.wmv");
        let mut video = Decoder::open(&path, true, 0, 0).unwrap().unwrap();
        let (duration, w, h, interval, bitrate) = video.info();
        assert!((1.9..2.3).contains(&duration));
        assert_eq!((w, h), (96, 64));
        assert!((interval - 0.1).abs() < 0.001);
        assert!(bitrate > 0);
        let (first, seconds) = video.next().unwrap().unwrap();
        assert_eq!(first.len(), 96 * 64 * 2);
        assert!(seconds < 0.2);
        video.seek(0.75).unwrap();
        let mut found = false;
        for _ in 0..20 {
            if let Some((_, seconds)) = video.next().unwrap() {
                if seconds >= 0.75 {
                    assert!(seconds < 1.1);
                    found = true;
                    break;
                }
            } else {
                break;
            }
        }
        assert!(found);
        video.seek(0.0).unwrap();
        assert!(video.next().unwrap().unwrap().1 < 0.2);
    }
}
