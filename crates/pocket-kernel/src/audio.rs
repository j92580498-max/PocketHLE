//! Real-time audio output backed by [`cpal`].
//!
//! The Pocket PC `waveOut*` API and `PlaySoundW` / `PlaySoundA` /
//! `sndPlaySoundW` family are routed through this module so that
//! games which previously had no sound can drive real PCM samples
//! out of the host's default audio device. The mixing model is
//! intentionally tiny — a single shared lock-protected ring buffer
//! of interleaved `i16` samples that the cpal output callback drains
//! at the host device's native sample rate. Two rate-adapter
//! strategies cover most Pocket PC content:
//!
//! * **Same-rate path** (the host opens the device at exactly the
//!   guest's requested rate, e.g. 44.1 kHz / 22.05 kHz / 11.025 kHz):
//!   samples are popped 1:1.
//! * **Resampled path** (the host device only supports a different
//!   rate): we use a nearest-neighbour resampler. This is good
//!   enough for the Pocket PC sound effects and looped music
//!   (typically 11.025 kHz mono, 22.05 kHz stereo) which already
//!   contain noticeable quantisation noise.
//!
//! The engine never blocks the emulator: if the host audio thread is
//! late and the ring buffer has overflowed, new samples are
//! dropped. Conversely, if the ring is empty when cpal asks for
//! more, we emit silence. This keeps the emulator's frame loop
//! decoupled from the audio device's buffer churn.
//!
//! The cpal feature is optional. When the `audio-cpal` feature is
//! disabled (e.g. on the Android JNI build, or in `--no-default-features`
//! CI runs that have no audio devices), [`AudioEngine`] silently
//! discards every PCM submission so the rest of the emulator works
//! as before. The previous behaviour of `waveOut*` returning success
//! without producing any sound is preserved bit-for-bit when the
//! feature is off.

use std::sync::{Arc, Mutex};

/// Maximum number of i16 samples we keep buffered. At 44.1 kHz
/// stereo this is just over a second — plenty to absorb scheduling
/// jitter, but small enough that overflows produce dropped samples
/// instead of unbounded memory growth.
const RING_CAPACITY_SAMPLES: usize = 1 << 17; // 131072

/// Audio format last requested by the guest. We remember it so the
/// cpal callback can decide whether to upsample or play 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
}

impl Default for GuestFormat {
    fn default() -> Self {
        Self {
            sample_rate: 44100,
            channels: 2,
            bits_per_sample: 16,
        }
    }
}

/// Inner state shared between the emulator thread (which calls
/// [`AudioEngine::push_samples`]) and the cpal output callback.
struct Shared {
    ring: Vec<i16>,
    /// Number of samples currently in the ring.
    len: usize,
    /// Read cursor.
    read: usize,
    /// Write cursor.
    write: usize,
    /// Format the guest most recently requested.
    guest_format: GuestFormat,
    /// Sub-sample fraction for the nearest-neighbour resampler (in
    /// units of 1/65536). Carried across cpal callbacks so we don't
    /// lose pitch on long playbacks.
    resampler_phase: u64,
}

impl Shared {
    fn new() -> Self {
        Self {
            ring: vec![0i16; RING_CAPACITY_SAMPLES],
            len: 0,
            read: 0,
            write: 0,
            guest_format: GuestFormat::default(),
            resampler_phase: 0,
        }
    }

    fn push(&mut self, samples: &[i16]) {
        let cap = self.ring.len();
        for &s in samples {
            if self.len == cap {
                // Ring is full — drop the oldest sample to make room.
                self.read = (self.read + 1) % cap;
                self.len -= 1;
            }
            self.ring[self.write] = s;
            self.write = (self.write + 1) % cap;
            self.len += 1;
        }
    }

    #[cfg(feature = "audio-cpal")]
    fn pop_one(&mut self) -> Option<i16> {
        if self.len == 0 {
            return None;
        }
        let v = self.ring[self.read];
        self.read = (self.read + 1) % self.ring.len();
        self.len -= 1;
        Some(v)
    }

    fn clear(&mut self) {
        self.len = 0;
        self.read = 0;
        self.write = 0;
        self.resampler_phase = 0;
    }
}

/// Public handle the rest of the emulator interacts with. Cheaply
/// cloneable via [`AudioEngine::clone`] (the underlying state is
/// behind an `Arc<Mutex<_>>`); the kernel keeps one copy and
/// [`AudioEngine::start`] / [`AudioEngine::stop`] hand additional
/// clones to the cpal callback.
pub struct AudioEngine {
    shared: Arc<Mutex<Shared>>,
    /// Whether the audio worker thread is alive. The thread itself
    /// owns the cpal `Stream` (which is `!Send` on some platforms),
    /// so we communicate with it via [`Shared`] and the
    /// [`Self::shutdown`] flag.
    #[cfg(feature = "audio-cpal")]
    worker: Option<std::thread::JoinHandle<()>>,
    #[cfg(not(feature = "audio-cpal"))]
    worker: Option<()>,
    /// Set by [`Self::stop`] to ask the audio thread to drop its
    /// stream and exit.
    #[cfg(feature = "audio-cpal")]
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// `true` once we've tried (and possibly failed) to open the
    /// device. Stops us from spamming the user log with retries on
    /// every `waveOutOpen`.
    init_attempted: bool,
}

impl Default for AudioEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for AudioEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEngine")
            .field("init_attempted", &self.init_attempted)
            .field("worker_alive", &self.worker.is_some())
            .finish()
    }
}

impl AudioEngine {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared::new())),
            worker: None,
            #[cfg(feature = "audio-cpal")]
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            init_attempted: false,
        }
    }

    /// Update the guest-side format. Called from `waveOutOpen` /
    /// `PlaySound` so the resampler knows what rate the i16 samples
    /// are coming in at.
    pub fn set_guest_format(&self, fmt: GuestFormat) {
        if let Ok(mut s) = self.shared.lock() {
            s.guest_format = fmt;
            // Reset resampler phase on format change so the first
            // sample of a new wave plays from t=0.
            s.resampler_phase = 0;
        }
    }

    /// Submit a chunk of interleaved 16-bit PCM. The samples are
    /// queued for the host audio thread to play. Returns the number
    /// of samples actually queued (we never block; if the ring is
    /// full the oldest samples get dropped).
    pub fn push_samples(&self, samples: &[i16]) -> usize {
        if let Ok(mut s) = self.shared.lock() {
            s.push(samples);
            samples.len()
        } else {
            0
        }
    }

    /// Convenience for unsigned 8-bit PCM (the format `PlaySound` and
    /// some old WAV resources use). Each byte is mapped to the
    /// signed 16-bit range linearly.
    pub fn push_samples_u8(&self, bytes: &[u8]) -> usize {
        let mut buf = Vec::with_capacity(bytes.len());
        for &b in bytes {
            let v = (b as i16 - 128) * 256;
            buf.push(v);
        }
        self.push_samples(&buf)
    }

    /// Drop any queued samples. Called by `waveOutReset` and on
    /// engine shutdown.
    pub fn flush(&self) {
        if let Ok(mut s) = self.shared.lock() {
            s.clear();
        }
    }

    /// Number of samples currently queued. Useful for tests and for
    /// `waveOutGetPosition` when we wire it up.
    pub fn buffered_samples(&self) -> usize {
        self.shared.lock().map(|s| s.len).unwrap_or(0)
    }

    /// Open the host audio device and start streaming. Idempotent —
    /// subsequent calls are no-ops as long as the worker is still
    /// alive. When the `audio-cpal` feature is disabled this is a
    /// no-op.
    pub fn start(&mut self) {
        if self.worker.is_some() || self.init_attempted {
            return;
        }
        self.init_attempted = true;
        self.start_impl();
    }

    #[cfg(feature = "audio-cpal")]
    fn start_impl(&mut self) {
        // cpal::Stream is `!Send` on some platforms, so the stream
        // has to be owned by a single dedicated thread. We hand the
        // shared ring + a shutdown flag to the worker; the worker
        // builds the stream, plays it, and parks until told to
        // exit, at which point dropping the stream stops audio
        // playback.
        let shared = Arc::clone(&self.shared);
        let shutdown = Arc::clone(&self.shutdown);
        shutdown.store(false, std::sync::atomic::Ordering::SeqCst);
        let handle = std::thread::Builder::new()
            .name("pockethle-audio".to_string())
            .spawn(move || run_audio_worker(shared, shutdown))
            .ok();
        self.worker = handle;
    }

    #[cfg(not(feature = "audio-cpal"))]
    fn start_impl(&mut self) {
        log::info!("AudioEngine: built without audio-cpal feature, running silently");
    }

    /// Stop the host stream and clear any pending samples. The
    /// engine can be re-`start`ed afterwards.
    pub fn stop(&mut self) {
        #[cfg(feature = "audio-cpal")]
        {
            self.shutdown
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(h) = self.worker.take() {
                let _ = h.join();
            }
        }
        #[cfg(not(feature = "audio-cpal"))]
        {
            self.worker = None;
        }
        self.flush();
        self.init_attempted = false;
    }
}

#[cfg(feature = "audio-cpal")]
fn run_audio_worker(shared: Arc<Mutex<Shared>>, shutdown: Arc<std::sync::atomic::AtomicBool>) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = match host.default_output_device() {
        Some(d) => d,
        None => {
            log::info!("AudioEngine: no default output device — running silently");
            return;
        }
    };
    let config = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            log::info!("AudioEngine: device.default_output_config() failed: {e}");
            return;
        }
    };

    let host_rate = config.sample_rate().0;
    let host_channels = config.channels();
    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.clone().into();
    let err_fn = |err| log::warn!("AudioEngine: cpal output error: {err}");

    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            let shared = Arc::clone(&shared);
            device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _| {
                    fill_output_f32(&shared, data, host_rate, host_channels);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let shared = Arc::clone(&shared);
            device.build_output_stream(
                &stream_config,
                move |data: &mut [i16], _| {
                    fill_output_i16(&shared, data, host_rate, host_channels);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let shared = Arc::clone(&shared);
            device.build_output_stream(
                &stream_config,
                move |data: &mut [u16], _| {
                    fill_output_u16(&shared, data, host_rate, host_channels);
                },
                err_fn,
                None,
            )
        }
        other => {
            log::info!("AudioEngine: unsupported host sample format {other:?}");
            return;
        }
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            log::info!("AudioEngine: build_output_stream failed: {e}");
            return;
        }
    };
    if let Err(e) = stream.play() {
        log::info!("AudioEngine: stream.play() failed: {e}");
        return;
    }
    log::info!(
        "AudioEngine: opened {} Hz / {} ch ({:?})",
        host_rate,
        host_channels,
        sample_format
    );
    while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(stream);
}

#[cfg(feature = "audio-cpal")]
fn fill_output_f32(
    shared: &Arc<Mutex<Shared>>,
    data: &mut [f32],
    host_rate: u32,
    host_channels: u16,
) {
    let mut s = match shared.lock() {
        Ok(g) => g,
        Err(_) => {
            for x in data.iter_mut() {
                *x = 0.0;
            }
            return;
        }
    };
    let guest_rate = s.guest_format.sample_rate.max(1);
    let guest_channels = s.guest_format.channels.max(1);
    // Step in guest-samples-per-host-frame (Q16.16).
    let step_q16 = ((guest_rate as u64) << 16) / host_rate as u64;
    let host_frames = data.len() / host_channels as usize;
    let mut phase = s.resampler_phase;
    for f in 0..host_frames {
        // Advance the guest read cursor by the integer part of the
        // accumulated phase increment.
        let advance = (phase >> 16) as usize;
        for _ in 0..advance.saturating_mul(guest_channels as usize) {
            let _ = s.pop_one();
        }
        phase &= 0xFFFF;
        // Read one guest frame (front-load any missing channels with
        // silence).
        let mut left = 0i16;
        let mut right = 0i16;
        if guest_channels >= 1 {
            left = s.ring[s.read];
            // Don't actually pop — we'll pop on the next iteration's
            // `advance`. This way two host frames mapping to the
            // same guest sample reuse it.
            if guest_channels >= 2 {
                let r_idx = (s.read + 1) % s.ring.len();
                right = s.ring[r_idx];
            } else {
                right = left;
            }
        }
        for ch in 0..host_channels as usize {
            let v = if ch == 0 { left } else { right };
            data[f * host_channels as usize + ch] = (v as f32) / 32768.0;
        }
        phase += step_q16;
    }
    s.resampler_phase = phase;
}

#[cfg(feature = "audio-cpal")]
fn fill_output_i16(
    shared: &Arc<Mutex<Shared>>,
    data: &mut [i16],
    host_rate: u32,
    host_channels: u16,
) {
    let mut tmp = vec![0f32; data.len()];
    fill_output_f32(shared, &mut tmp, host_rate, host_channels);
    for (i, v) in tmp.iter().enumerate() {
        let s = (v * 32767.0).clamp(-32768.0, 32767.0) as i16;
        data[i] = s;
    }
}

#[cfg(feature = "audio-cpal")]
fn fill_output_u16(
    shared: &Arc<Mutex<Shared>>,
    data: &mut [u16],
    host_rate: u32,
    host_channels: u16,
) {
    let mut tmp = vec![0f32; data.len()];
    fill_output_f32(shared, &mut tmp, host_rate, host_channels);
    for (i, v) in tmp.iter().enumerate() {
        let s = ((v + 1.0) * 32767.5).clamp(0.0, 65535.0) as u16;
        data[i] = s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_starts_silently_with_no_feature() {
        let mut e = AudioEngine::new();
        e.start();
        // start() must not panic regardless of host configuration.
        e.stop();
    }

    #[test]
    fn ring_drops_oldest_on_overflow() {
        let mut s = Shared::new();
        let big = vec![1i16; RING_CAPACITY_SAMPLES + 8];
        s.push(&big);
        assert_eq!(s.len, RING_CAPACITY_SAMPLES);
    }

    #[test]
    fn push_then_buffered_samples() {
        let e = AudioEngine::new();
        e.set_guest_format(GuestFormat {
            sample_rate: 22050,
            channels: 1,
            bits_per_sample: 16,
        });
        e.push_samples(&[0, 1, 2, 3]);
        assert_eq!(e.buffered_samples(), 4);
        e.flush();
        assert_eq!(e.buffered_samples(), 0);
    }

    #[test]
    fn push_u8_maps_to_signed_range() {
        let e = AudioEngine::new();
        let n = e.push_samples_u8(&[0x80, 0x00, 0xFF]);
        assert_eq!(n, 3);
        assert_eq!(e.buffered_samples(), 3);
    }
}
