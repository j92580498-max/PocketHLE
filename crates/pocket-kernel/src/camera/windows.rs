//! Media Foundation acquisition lives on its own MTA thread. Only the latest
//! RGB frame crosses into the emulator; ReadSample never blocks a guest thread.
use super::{Backend, Capture, Frame, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use windows::{
    core::{AgileReference, Interface},
    Win32::{Media::MediaFoundation::*, System::Com::*},
};

pub struct WindowsBackend;
#[derive(Default)]
struct Mailbox {
    frame: Option<Arc<Frame>>,
    error: Option<u32>,
}
// Marshal only the shutdown interface; reader and sample buffers stay on MTA.
struct State {
    alive: AtomicBool,
    mail: Mutex<Mailbox>,
    source: Mutex<Option<AgileReference<IMFMediaSource>>>,
}
struct WindowsCapture(Arc<State>);
impl Backend for WindowsBackend {
    fn start(&self) -> Result<Box<dyn Capture>> {
        let state = Arc::new(State {
            alive: AtomicBool::new(true),
            mail: Mutex::new(Mailbox::default()),
            source: Mutex::new(None),
        });
        let worker = state.clone();
        std::thread::Builder::new()
            .name("pockethle-camera".into())
            .spawn(move || {
                let result = unsafe { acquire(&worker) };
                if let Err(error) = result {
                    worker.mail.lock().unwrap().error = Some(error);
                }
            })
            .map_err(|_| 8u32)?;
        Ok(Box::new(WindowsCapture(state)))
    }
}
impl Capture for WindowsCapture {
    fn latest(&mut self) -> Result<Option<Arc<Frame>>> {
        let mail = self.0.mail.lock().unwrap();
        if let Some(error) = mail.error {
            Err(error)
        } else {
            Ok(mail.frame.clone())
        }
    }
}
impl Drop for WindowsCapture {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::Release);
        let source = self.0.source.lock().unwrap().take();
        if let Some(source) = source {
            shutdown(source);
        }
    }
}
fn error(e: windows::core::Error) -> u32 {
    let hr = e.code().0 as u32;
    if hr & 0xffff0000 == 0x80070000 {
        hr & 0xffff
    } else {
        31
    }
}
struct Com;
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
struct Foundation;
impl Drop for Foundation {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}
struct SourceGuard<'a>(&'a State);
impl Drop for SourceGuard<'_> {
    fn drop(&mut self) {
        let source = self.0.source.lock().unwrap().take();
        if let Some(source) = source {
            shutdown(source);
        }
    }
}
fn shutdown(reference: AgileReference<IMFMediaSource>) {
    unsafe {
        // S_FALSE also increments COM's thread refcount. An existing STA can
        // resolve the agile reference in its own apartment without changing it.
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_ok() || hr.0 as u32 == 0x80010106 {
            if let Ok(source) = reference.resolve() {
                let _ = source.Shutdown();
            }
        }
        if hr.is_ok() {
            CoUninitialize();
        }
    }
}
unsafe fn attrs() -> Result<IMFAttributes> {
    let mut a = None;
    MFCreateAttributes(&mut a, 4).map_err(error)?;
    a.ok_or(8)
}
unsafe fn first_source() -> Result<IMFMediaSource> {
    let a = attrs()?;
    a.SetGUID(
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    )
    .map_err(error)?;
    let mut devices = std::ptr::null_mut();
    let mut count = 0;
    MFEnumDeviceSources(&a, &mut devices, &mut count).map_err(error)?;
    if devices.is_null() {
        return Err(2);
    }
    let result = if count == 0 {
        Err(2)
    } else {
        (*devices)
            .as_ref()
            .ok_or(2u32)
            .and_then(|a| a.ActivateObject::<IMFMediaSource>().map_err(error))
    };
    for i in 0..count as usize {
        std::ptr::drop_in_place(devices.add(i));
    }
    CoTaskMemFree(Some(devices.cast()));
    result
}
unsafe fn acquire(state: &State) -> Result<()> {
    CoInitializeEx(None, COINIT_MULTITHREADED)
        .ok()
        .map_err(error)?;
    let _com = Com;
    MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(error)?;
    let _mf = Foundation;
    let source = first_source()?;
    let reference = match AgileReference::new(&source) {
        Ok(reference) => reference,
        Err(e) => {
            let _ = source.Shutdown();
            return Err(error(e));
        }
    };
    {
        let mut slot = state.source.lock().unwrap();
        if !state.alive.load(Ordering::Acquire) {
            let _ = source.Shutdown();
            return Ok(());
        }
        *slot = Some(reference);
    }
    let _source_guard = SourceGuard(state);
    let a = attrs()?;
    a.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
        .map_err(error)?;
    let reader = MFCreateSourceReaderFromMediaSource(&source, &a).map_err(error)?;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    reader
        .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
        .map_err(error)?;
    reader.SetStreamSelection(stream, true).map_err(error)?;
    let media = MFCreateMediaType().map_err(error)?;
    media
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(error)?;
    media
        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
        .map_err(error)?;
    // Prefer the console's native size, avoiding expensive HD frames. Drivers
    // that cannot negotiate 640x480 fall back to a supported native geometry.
    media
        .SetUINT64(&MF_MT_FRAME_SIZE, (640u64 << 32) | 480)
        .map_err(error)?;
    if reader.SetCurrentMediaType(stream, None, &media).is_err() {
        media.DeleteItem(&MF_MT_FRAME_SIZE).map_err(error)?;
        reader
            .SetCurrentMediaType(stream, None, &media)
            .map_err(error)?;
    }
    let mut serial = 0;
    while state.alive.load(Ordering::Acquire) {
        let mut sample = None;
        let mut flags = 0;
        reader
            .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
            .map_err(error)?;
        if !state.alive.load(Ordering::Acquire) {
            break;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Err(1167);
        }
        let Some(sample) = sample else {
            continue;
        };
        let media = reader.GetCurrentMediaType(stream).map_err(error)?;
        let size = media.GetUINT64(&MF_MT_FRAME_SIZE).map_err(error)?;
        let (width, height) = ((size >> 32) as u32, size as u32);
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err(13);
        }
        let stride = media
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|v| v as i32)
            .or_else(|_| MFGetStrideForBitmapInfoHeader(MFVideoFormat_RGB32.data1, width))
            .map_err(error)?;
        let buffer = sample.ConvertToContiguousBuffer().map_err(error)?;
        let rgb = rgb(&buffer, width, height, stride)?;
        serial += 1;
        state.mail.lock().unwrap().frame = Some(Arc::new(Frame {
            width,
            height,
            serial,
            rgb,
        }));
    }
    Ok(())
}
unsafe fn rgb(
    buffer: &IMFMediaBuffer,
    width: u32,
    height: u32,
    default_stride: i32,
) -> Result<Vec<u8>> {
    let two = buffer.cast::<IMF2DBuffer>().ok();
    let mut row0 = std::ptr::null_mut();
    let mut stride = default_stride;
    if let Some(two) = &two {
        two.Lock2D(&mut row0, &mut stride).map_err(error)?;
    } else {
        let mut length = 0;
        buffer
            .Lock(&mut row0, None, Some(&mut length))
            .map_err(error)?;
        let needed = (height as u64 - 1) * stride.unsigned_abs() as u64 + width as u64 * 4;
        if needed > length as u64 {
            let _ = buffer.Unlock();
            return Err(13);
        }
        if stride < 0 {
            row0 = row0.add((height as usize - 1) * stride.unsigned_abs() as usize);
        }
    }
    let result = if row0.is_null() || stride.unsigned_abs() < width * 4 {
        Err(13)
    } else {
        let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
        for y in 0..height as isize {
            let row =
                std::slice::from_raw_parts(row0.offset(y * stride as isize), width as usize * 4);
            for p in row.chunks_exact(4) {
                rgb.extend_from_slice(&[p[2], p[1], p[0]]);
            }
        }
        Ok(rgb)
    };
    if let Some(two) = two {
        let _ = two.Unlock2D();
    } else {
        let _ = buffer.Unlock();
    }
    result
}
