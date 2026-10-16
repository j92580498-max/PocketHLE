//! WinMM PCM capture. Host callbacks produce owned PCM; the emulator thread
//! alone fills guest WAVEHDRs and delivers WIM notifications.
use crate::{CallCtx, WinCeDispatcher};
use once_cell::sync::Lazy;
use pocket_kernel::{
    audio::GuestFormat, DispatchOutcome, KernelError, WaveCallbackKind, WaveOutDevice,
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
};
const DONE: u32 = 1;
const PREPARED: u32 = 2;
const INQUEUE: u32 = 16;
struct Buffer {
    hdr: u32,
    data: u32,
    len: u32,
    written: u32,
}
struct Capture {
    meta: WaveOutDevice,
    active: Arc<AtomicBool>,
    rx: mpsc::Receiver<Vec<i16>>,
    quit: mpsc::Sender<()>,
    buffers: VecDeque<Buffer>,
    pcm: VecDeque<u8>,
    bytes: u64,
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.quit.send(());
    }
}
struct Inputs {
    next: u32,
    devices: HashMap<u32, Capture>,
    closed: HashMap<u32, WaveOutDevice>,
}
static INPUTS: Lazy<Mutex<Inputs>> = Lazy::new(|| {
    Mutex::new(Inputs {
        next: 0xdead5100,
        devices: HashMap::new(),
        closed: HashMap::new(),
    })
});
const NAMES: [&str; 14] = [
    "waveInGetNumDevs",
    "waveInGetDevCaps",
    "waveInGetErrorText",
    "waveInClose",
    "waveInPrepareHeader",
    "waveInUnprepareHeader",
    "waveInAddBuffer",
    "waveInStart",
    "waveInStop",
    "waveInReset",
    "waveInGetPosition",
    "waveInGetID",
    "waveInMessage",
    "waveInOpen",
];
pub(crate) fn register(d: &mut WinCeDispatcher) {
    for name in NAMES {
        d.register_handler("coredll.dll", name, dispatch);
    }
}
pub(crate) fn callback_device(handle: u32, message: u32) -> Option<WaveOutDevice> {
    let mut i = INPUTS.lock().unwrap();
    if let Some(c) = i.devices.get(&handle) {
        return Some(c.meta);
    }
    if message == 0x3bf {
        i.closed.remove(&handle)
    } else {
        i.closed.get(&handle).copied()
    }
}
fn notify(ctx: &mut CallCtx<'_>, h: u32, meta: WaveOutDevice, msg: u32, hdr: u32) {
    match meta.callback_kind {
        WaveCallbackKind::Function => ctx
            .kernel
            .wave_out
            .function_done
            .push_back((h, msg, hdr, 0)),
        WaveCallbackKind::Event => {
            let key = crate::coredll::event_key(ctx.kernel, meta.callback_target);
            if let Some(mut e) = ctx.kernel.events.get_mut(&key) {
                e.signalled = true;
            }
        }
        WaveCallbackKind::Window => {
            ctx.kernel
                .posted_messages
                .push_back((meta.callback_target, msg, h, hdr))
        }
        WaveCallbackKind::Thread => {
            if let Some(t) = ctx
                .kernel
                .threads
                .iter_mut()
                .find(|t| t.id == meta.callback_target && !t.finished)
            {
                t.messages.push_back((msg, h, hdr));
            }
        }
        WaveCallbackKind::None => {}
    }
}
fn complete(
    ctx: &mut CallCtx<'_>,
    h: u32,
    meta: WaveOutDevice,
    b: Buffer,
) -> Result<(), KernelError> {
    ctx.cpu.write_mem(b.hdr + 8, &b.written.to_le_bytes())?;
    let flags = ctx.cpu.read_u32_le(b.hdr + 16)?;
    ctx.cpu
        .write_mem(b.hdr + 16, &((flags & !INQUEUE) | DONE).to_le_bytes())?;
    notify(ctx, h, meta, 0x3c0, b.hdr);
    Ok(())
}
pub(crate) fn service(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    let mut inputs = INPUTS.lock().unwrap();
    for (&h, c) in inputs.devices.iter_mut() {
        if !c.active.load(Ordering::Relaxed) {
            continue;
        }
        while let Ok(samples) = c.rx.try_recv() {
            for sample in samples {
                if c.meta.format.bits_per_sample == 8 {
                    c.pcm.push_back(((sample as i32 + 32768) >> 8) as u8);
                } else {
                    c.pcm.extend(sample.to_le_bytes());
                }
            }
        }
        while let Some(b) = c.buffers.front_mut() {
            let count = (b.len - b.written).min(c.pcm.len() as u32);
            if count > 0 {
                let bytes: Vec<u8> = c.pcm.drain(..count as usize).collect();
                ctx.cpu.write_mem(b.data + b.written, &bytes)?;
                b.written += count;
                c.bytes += u64::from(count);
                ctx.cpu.write_mem(b.hdr + 8, &b.written.to_le_bytes())?;
            }
            if b.written == b.len {
                let b = c.buffers.pop_front().unwrap();
                complete(ctx, h, c.meta, b)?;
            } else {
                break;
            }
        }
        // No application buffers: discard captured data instead of unbounded growth.
        if c.buffers.is_empty() {
            c.pcm.clear();
        }
    }
    Ok(())
}
#[cfg(not(feature = "audio-cpal"))]
fn available() -> bool {
    false
}
#[cfg(feature = "audio-cpal")]
fn available() -> bool {
    use cpal::traits::HostTrait;
    cpal::default_host().default_input_device().is_some()
}
#[cfg(not(feature = "audio-cpal"))]
fn backend(
    _fmt: GuestFormat,
) -> Result<(mpsc::Receiver<Vec<i16>>, mpsc::Sender<()>, Arc<AtomicBool>), String> {
    Err("microphone backend is disabled".into())
}
#[cfg(feature = "audio-cpal")]
fn backend(
    fmt: GuestFormat,
) -> Result<(mpsc::Receiver<Vec<i16>>, mpsc::Sender<()>, Arc<AtomicBool>), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let (tx, rx) = mpsc::sync_channel(32);
    let (quit, stop) = mpsc::channel();
    let (ready, status) = mpsc::sync_channel(1);
    let active = Arc::new(AtomicBool::new(false));
    let enabled = active.clone();
    std::thread::spawn(move || {
        let run = || -> Result<cpal::Stream, String> {
            let device = cpal::default_host()
                .default_input_device()
                .ok_or("no microphone device")?;
            let supported = device.default_input_config().map_err(|e| e.to_string())?;
            let config: cpal::StreamConfig = supported.clone().into();
            let rate = config.sample_rate.0;
            let channels = config.channels as usize;
            macro_rules! build {
                ($ty:ty,$convert:expr) => {{
                    let mut phase = 0u64;
                    let tx = tx.clone();
                    let enabled = enabled.clone();
                    device
                        .build_input_stream(
                            &config,
                            move |data: &[$ty], _| {
                                if !enabled.load(Ordering::Relaxed) {
                                    return;
                                }
                                let mut out = Vec::new();
                                for frame in data.chunks_exact(channels) {
                                    phase += u64::from(fmt.sample_rate);
                                    while phase >= u64::from(rate) {
                                        phase -= u64::from(rate);
                                        let convert = $convert;
                                        if fmt.channels == 1 {
                                            let value =
                                                frame.iter().map(|&v| convert(v)).sum::<f32>()
                                                    / channels as f32;
                                            out.push((value.clamp(-1.0, 1.0) * 32767.0) as i16);
                                        } else {
                                            for channel in 0..2 {
                                                let value =
                                                    convert(frame[channel.min(channels - 1)]);
                                                out.push((value.clamp(-1.0, 1.0) * 32767.0) as i16);
                                            }
                                        }
                                    }
                                }
                                if !out.is_empty() {
                                    let _ = tx.try_send(out);
                                }
                            },
                            |e| log::error!("waveIn microphone: {e}"),
                            None,
                        )
                        .map_err(|e| e.to_string())?
                }};
            }
            let stream = match supported.sample_format() {
                cpal::SampleFormat::F32 => build!(f32, |v: f32| v),
                cpal::SampleFormat::I16 => build!(i16, |v: i16| v as f32 / 32768.0),
                cpal::SampleFormat::U16 => build!(u16, |v: u16| (v as f32 - 32768.0) / 32768.0),
                _ => return Err("unsupported microphone sample format".into()),
            };
            stream.play().map_err(|e| e.to_string())?;
            Ok(stream)
        };
        match run() {
            Ok(stream) => {
                let _ = ready.send(Ok(()));
                let _ = stop.recv();
                drop(stream);
            }
            Err(e) => {
                let _ = ready.send(Err(e));
            }
        }
    });
    status
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())??;
    Ok((rx, quit, active))
}
fn dispatch(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    service(ctx)?;
    let name = ctx.thunk.friendly_name.as_deref().unwrap_or("").to_string();
    let h = ctx.arg_u32(0)?;
    if name == "waveInGetNumDevs" {
        return Ok(DispatchOutcome::ReturnedR0(available() as u32));
    }
    if name == "waveInGetDevCaps" {
        let p = ctx.arg_u32(1)?;
        let size = ctx.arg_u32(2)?;
        let mut data = [0u8; 80];
        data[0..2].copy_from_slice(&1u16.to_le_bytes());
        data[4..8].copy_from_slice(&0x100u32.to_le_bytes());
        for (i, v) in "PocketHLE microphone\0".encode_utf16().enumerate() {
            data[8 + i * 2..10 + i * 2].copy_from_slice(&v.to_le_bytes());
        }
        data[72..76].copy_from_slice(&0xfffu32.to_le_bytes());
        data[76..78].copy_from_slice(&2u16.to_le_bytes());
        if p != 0 {
            ctx.cpu.write_mem(p, &data[..(size as usize).min(80)])?;
        }
        return Ok(DispatchOutcome::ReturnedR0(if available() { 0 } else { 2 }));
    }
    if name == "waveInGetErrorText" {
        let p = ctx.arg_u32(1)?;
        let count = ctx.arg_u32(2)? as usize;
        let mut data: Vec<u16> = "Audio capture error"
            .encode_utf16()
            .take(count.saturating_sub(1))
            .collect();
        if count > 0 {
            data.push(0);
        }
        let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_le_bytes()).collect();
        if p != 0 {
            ctx.cpu.write_mem(p, &bytes)?;
        }
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut inputs = INPUTS.lock().unwrap();
    if name == "waveInOpen" {
        let device = ctx.arg_u32(1)?;
        let p = ctx.arg_u32(2)?;
        let callback = ctx.arg_u32(3)?;
        let instance = ctx.arg_u32(4)?;
        let flags = ctx.arg_u32(5)?;
        if device != 0 && device != u32::MAX {
            return Ok(DispatchOutcome::ReturnedR0(2));
        }
        if p == 0 {
            return Ok(DispatchOutcome::ReturnedR0(11));
        }
        let data = ctx.cpu.read_mem(p, 16)?;
        let tag = u16::from_le_bytes([data[0], data[1]]);
        let channels = u16::from_le_bytes([data[2], data[3]]);
        let rate = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let align = u16::from_le_bytes([data[12], data[13]]);
        let bits = u16::from_le_bytes([data[14], data[15]]);
        if tag != 1
            || !(channels == 1 || channels == 2)
            || !(bits == 8 || bits == 16)
            || rate == 0
            || rate > 96000
            || align != channels * (bits / 8)
        {
            return Ok(DispatchOutcome::ReturnedR0(32));
        }
        if flags & 1 != 0 {
            return Ok(DispatchOutcome::ReturnedR0(if available() { 0 } else { 6 }));
        }
        if h == 0 {
            return Ok(DispatchOutcome::ReturnedR0(11));
        }
        ctx.cpu.write_mem(h, &0u32.to_le_bytes())?;
        let fmt = GuestFormat {
            sample_rate: rate,
            channels,
            bits_per_sample: bits,
        };
        let (rx, quit, active) = match backend(fmt) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("waveInOpen: {e}");
                return Ok(DispatchOutcome::ReturnedR0(6));
            }
        };
        let handle = inputs.next;
        inputs.next = handle.wrapping_add(1);
        let kind = match flags & 0x70000 {
            0x10000 => WaveCallbackKind::Window,
            0x20000 => WaveCallbackKind::Thread,
            0x30000 => WaveCallbackKind::Function,
            0x50000 => WaveCallbackKind::Event,
            _ => WaveCallbackKind::None,
        };
        let meta = WaveOutDevice {
            callback_kind: kind,
            callback_target: callback,
            instance,
            owner_thread: ctx.kernel.current_thread,
            format: fmt,
            paused: false,
        };
        inputs.devices.insert(
            handle,
            Capture {
                meta,
                active,
                rx,
                quit,
                buffers: VecDeque::new(),
                pcm: VecDeque::new(),
                bytes: 0,
            },
        );
        ctx.cpu.write_mem(h, &handle.to_le_bytes())?;
        notify(ctx, handle, meta, 0x3be, 0);
        log::info!("waveInOpen: microphone {rate} Hz/{channels} ch/{bits} bits h=0x{handle:08x}");
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let Some(c) = inputs.devices.get_mut(&h) else {
        return Ok(DispatchOutcome::ReturnedR0(5));
    };
    let mut result = 0;
    match name.as_str() {
        "waveInPrepareHeader" | "waveInUnprepareHeader" => {
            let p = ctx.arg_u32(1)?;
            if p == 0 || ctx.arg_u32(2)? < 32 {
                return Ok(DispatchOutcome::ReturnedR0(11));
            }
            let flags = ctx.cpu.read_u32_le(p + 16)?;
            if flags & INQUEUE != 0 {
                result = 33;
            } else {
                let flags = if name == "waveInPrepareHeader" {
                    (flags | PREPARED) & !DONE
                } else {
                    flags & !PREPARED
                };
                ctx.cpu.write_mem(p + 16, &flags.to_le_bytes())?;
            }
        }
        "waveInAddBuffer" => {
            let p = ctx.arg_u32(1)?;
            if p == 0 || ctx.arg_u32(2)? < 32 {
                return Ok(DispatchOutcome::ReturnedR0(11));
            }
            let flags = ctx.cpu.read_u32_le(p + 16)?;
            if flags & PREPARED == 0 {
                result = 34;
            } else if flags & INQUEUE != 0 {
                result = 33;
            } else {
                let data = ctx.cpu.read_u32_le(p)?;
                let len = ctx.cpu.read_u32_le(p + 4)?;
                let align = u32::from(c.meta.format.channels * c.meta.format.bits_per_sample / 8);
                if data == 0 || len % align != 0 {
                    result = 11;
                } else {
                    ctx.cpu.write_mem(p + 8, &0u32.to_le_bytes())?;
                    ctx.cpu
                        .write_mem(p + 16, &((flags | INQUEUE) & !DONE).to_le_bytes())?;
                    c.buffers.push_back(Buffer {
                        hdr: p,
                        data,
                        len,
                        written: 0,
                    });
                }
            }
        }
        "waveInStart" => {
            c.pcm.clear();
            while c.rx.try_recv().is_ok() {}
            c.active.store(true, Ordering::Relaxed);
        }
        "waveInStop" | "waveInReset" => {
            c.active.store(false, Ordering::Relaxed);
            c.pcm.clear();
            while c.rx.try_recv().is_ok() {}
            if name == "waveInReset" {
                while let Some(b) = c.buffers.pop_front() {
                    complete(ctx, h, c.meta, b)?;
                }
                c.bytes = 0;
            } else if c.buffers.front().map(|b| b.written > 0).unwrap_or(false) {
                let b = c.buffers.pop_front().unwrap();
                complete(ctx, h, c.meta, b)?;
            }
        }
        "waveInGetPosition" => {
            let p = ctx.arg_u32(1)?;
            if p == 0 || ctx.arg_u32(2)? < 8 {
                return Ok(DispatchOutcome::ReturnedR0(11));
            }
            let want = ctx.cpu.read_u32_le(p)?;
            let align = u64::from(c.meta.format.channels * c.meta.format.bits_per_sample / 8);
            let frames = c.bytes / align;
            let (ty, value) = match want {
                1 => (1, frames * 1000 / u64::from(c.meta.format.sample_rate)),
                2 => (2, frames),
                _ => (4, c.bytes),
            };
            ctx.cpu.write_mem(p, &(ty as u32).to_le_bytes())?;
            ctx.cpu.write_mem(p + 4, &(value as u32).to_le_bytes())?;
        }
        "waveInGetID" => {
            let p = ctx.arg_u32(1)?;
            if p != 0 {
                ctx.cpu.write_mem(p, &0u32.to_le_bytes())?;
            }
        }
        "waveInClose" => {
            if !c.buffers.is_empty() {
                result = 33;
            } else {
                let meta = c.meta;
                inputs.devices.remove(&h);
                if matches!(meta.callback_kind, WaveCallbackKind::Function) {
                    inputs.closed.insert(h, meta);
                }
                notify(ctx, h, meta, 0x3bf, 0);
            }
        }
        _ => result = 8,
    }
    Ok(DispatchOutcome::ReturnedR0(result))
}
pub(crate) struct SuspendedInputs {
    inputs: Inputs,
    active: HashMap<u32, bool>,
}
pub(crate) fn suspend() -> SuspendedInputs {
    let inputs = std::mem::replace(
        &mut *INPUTS.lock().unwrap(),
        Inputs {
            next: 0xdead5100,
            devices: HashMap::new(),
            closed: HashMap::new(),
        },
    );
    let active = inputs
        .devices
        .iter()
        .map(|(handle, capture)| {
            (
                *handle,
                capture
                    .active
                    .swap(false, std::sync::atomic::Ordering::SeqCst),
            )
        })
        .collect();
    SuspendedInputs { inputs, active }
}
pub(crate) fn resume(saved: SuspendedInputs) {
    for (handle, capture) in &saved.inputs.devices {
        capture.active.store(
            saved.active.get(handle).copied().unwrap_or(false),
            std::sync::atomic::Ordering::SeqCst,
        );
    }
    *INPUTS.lock().unwrap() = saved.inputs;
}

pub(crate) fn reset() {
    let mut i = INPUTS.lock().unwrap();
    i.devices.clear();
    i.closed.clear();
}
#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
    use pocket_kernel::{KernelState, Thunk};
    use pocket_pe::ImportBinding;
    fn call(
        cpu: &mut StubCpu,
        kernel: &mut KernelState,
        name: &str,
        args: [u32; 3],
    ) -> DispatchOutcome {
        for (reg, value) in [ArmReg::R0, ArmReg::R1, ArmReg::R2].into_iter().zip(args) {
            cpu.write_reg(reg, value).unwrap();
        }
        let thunk = Thunk {
            thunk_va: 0x70000000,
            iat_va: 0x20000,
            dll: "coredll.dll".into(),
            binding: ImportBinding::Name(name.into()),
            friendly_name: Some(name.into()),
        };
        dispatch(&mut CallCtx {
            cpu,
            kernel,
            thunk: &thunk,
        })
        .unwrap()
    }
    #[test]
    fn wavein_pcm_buffers_stop_reset_and_close_obey_header_lifecycle() {
        let mut cpu = StubCpu::new();
        let mut kernel = crate::gx::tests::fresh_kernel();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        let h = 0xdead5ffe;
        let (tx, rx) = mpsc::channel();
        let (quit, _stop) = mpsc::channel();
        let meta = WaveOutDevice {
            callback_kind: WaveCallbackKind::Window,
            callback_target: 0xdead0001,
            instance: 0,
            owner_thread: 0,
            format: GuestFormat {
                sample_rate: 8000,
                channels: 1,
                bits_per_sample: 16,
            },
            paused: false,
        };
        INPUTS.lock().unwrap().devices.insert(
            h,
            Capture {
                meta,
                active: Arc::new(AtomicBool::new(true)),
                rx,
                quit,
                buffers: VecDeque::new(),
                pcm: VecDeque::new(),
                bytes: 0,
            },
        );
        cpu.write_mem(0x1100, &0x1200u32.to_le_bytes()).unwrap();
        cpu.write_mem(0x1104, &8u32.to_le_bytes()).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                "waveInPrepareHeader",
                [h, 0x1100, 32]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(&mut cpu, &mut kernel, "waveInAddBuffer", [h, 0x1100, 32]),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                "waveInUnprepareHeader",
                [h, 0x1100, 32]
            ),
            DispatchOutcome::ReturnedR0(33)
        );
        tx.send(vec![100i16, -200]).unwrap();
        assert_eq!(
            call(&mut cpu, &mut kernel, "waveInStop", [h, 0, 0]),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(cpu.read_u32_le(0x1108).unwrap(), 4);
        assert_eq!(cpu.read_mem(0x1200, 4).unwrap(), [100, 0, 56, 255]);
        assert_eq!(cpu.read_u32_le(0x1110).unwrap() & (DONE | INQUEUE), DONE);
        assert!(kernel
            .posted_messages
            .iter()
            .any(|m| m.1 == 0x3c0 && m.3 == 0x1100));
        assert_eq!(
            call(&mut cpu, &mut kernel, "waveInAddBuffer", [h, 0x1100, 32]),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(&mut cpu, &mut kernel, "waveInClose", [h, 0, 0]),
            DispatchOutcome::ReturnedR0(33)
        );
        assert_eq!(
            call(&mut cpu, &mut kernel, "waveInReset", [h, 0, 0]),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(cpu.read_u32_le(0x1108).unwrap(), 0);
        assert_eq!(
            call(&mut cpu, &mut kernel, "waveInClose", [h, 0, 0]),
            DispatchOutcome::ReturnedR0(0)
        );
    }
}
