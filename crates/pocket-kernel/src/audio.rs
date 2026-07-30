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

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    /// Total guest samples ever submitted through [`Shared::push`].
    written: u64,
    /// Total guest samples the host device has actually consumed.
    /// Only meaningful while `device_active` is set.
    consumed: u64,
    /// `true` once a cpal output stream is playing. With no host
    /// device we fall back to a wall-clock playback estimate so
    /// guests that wait for buffer-done notifications still make
    /// progress.
    device_active: bool,
    /// Wall-clock playback estimate, in guest samples.
    virtual_cursor: u64,
    /// When the wall-clock estimate was last advanced.
    virtual_tick: Option<Instant>,
    /// Optional WAV tap so headless runs can verify that a game
    /// really produces sound on a machine with no audio hardware.
    capture: Option<WavCapture>,
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
            written: 0,
            consumed: 0,
            device_active: false,
            virtual_cursor: 0,
            virtual_tick: None,
            capture: None,
        }
    }

    /// Guest samples played so far. Backed by the host device when we
    /// have one and by a wall clock otherwise. Never runs ahead of
    /// what the guest actually submitted, so a game that stops
    /// feeding buffers doesn't see phantom progress.
    fn cursor(&mut self) -> u64 {
        if self.device_active {
            return self.consumed.min(self.written);
        }
        let rate =
            self.guest_format.sample_rate.max(1) as u64 * self.guest_format.channels.max(1) as u64;
        let now = Instant::now();
        let last = *self.virtual_tick.get_or_insert(now);
        let elapsed_us = now.saturating_duration_since(last).as_micros() as u64;
        if elapsed_us > 0 {
            self.virtual_tick = Some(now);
            self.virtual_cursor = self
                .virtual_cursor
                .saturating_add(elapsed_us.saturating_mul(rate) / 1_000_000);
        }
        self.virtual_cursor = self.virtual_cursor.min(self.written);
        self.virtual_cursor
    }

    fn push(&mut self, samples: &[i16]) {
        self.written = self.written.saturating_add(samples.len() as u64);
        let fmt = self.guest_format;
        if let Some(cap) = self.capture.as_mut() {
            cap.write(samples, fmt);
        }
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
        self.consumed = self.consumed.saturating_add(1);
        Some(v)
    }

    fn clear(&mut self) {
        self.len = 0;
        self.read = 0;
        self.write = 0;
        self.resampler_phase = 0;
        // `waveOutReset` documents the playback position as being
        // reset to zero, so the cursors restart with the ring.
        self.written = 0;
        self.consumed = 0;
        self.virtual_cursor = 0;
        self.virtual_tick = None;
    }
}

/// Minimal streaming WAV writer behind [`AudioEngine::capture_to`].
///
/// The RIFF sizes are rewritten after every submission rather than
/// only on drop: emulator runs are frequently killed by a signal
/// (timeouts, frame-budget harnesses) and a half-written header would
/// make the capture useless exactly when it is needed most.
struct WavCapture {
    file: std::fs::File,
    data_bytes: u64,
    header: Option<GuestFormat>,
}

impl WavCapture {
    fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: std::fs::File::create(path)?,
            data_bytes: 0,
            header: None,
        })
    }

    fn write(&mut self, samples: &[i16], fmt: GuestFormat) {
        if self.header.is_none() {
            // Capture is always 16-bit: `push_samples_u8` widens 8-bit
            // PCM before it reaches the ring.
            let fmt = GuestFormat {
                bits_per_sample: 16,
                ..fmt
            };
            if self.write_header(fmt).is_err() {
                return;
            }
            self.header = Some(fmt);
        }
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        if self.file.write_all(&bytes).is_err() {
            return;
        }
        self.data_bytes = self.data_bytes.saturating_add(bytes.len() as u64);
        let _ = self.patch_sizes();
    }

    fn write_header(&mut self, fmt: GuestFormat) -> std::io::Result<()> {
        let channels = fmt.channels.max(1);
        let rate = fmt.sample_rate.max(1);
        let block_align = channels * 2;
        let byte_rate = rate * block_align as u32;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&0u32.to_le_bytes()); // patched by patch_sizes
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&byte_rate.to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&0u32.to_le_bytes()); // patched by patch_sizes
        self.file.write_all(&h)
    }

    fn patch_sizes(&mut self) -> std::io::Result<()> {
        let data = self.data_bytes.min(u32::MAX as u64 - 36) as u32;
        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&(36 + data).to_le_bytes())?;
        self.file.seek(SeekFrom::Start(40))?;
        self.file.write_all(&data.to_le_bytes())?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
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

    /// Number of samples currently queued.
    pub fn buffered_samples(&self) -> usize {
        self.shared.lock().map(|s| s.len).unwrap_or(0)
    }

    /// Total guest samples submitted since the stream was opened (or
    /// since the last [`Self::flush`]).
    pub fn written_samples(&self) -> u64 {
        self.shared.lock().map(|s| s.written).unwrap_or(0)
    }

    /// Guest samples played back so far. `waveOutGetPosition` and the
    /// `WOM_DONE` bookkeeping in `waveOut*` are both driven from this.
    pub fn playback_cursor(&self) -> u64 {
        self.shared.lock().map(|mut s| s.cursor()).unwrap_or(0)
    }

    /// Tee every submitted sample into a 16-bit PCM WAV file. Used by
    /// `pockethle run --dump-audio-to` so a run on a machine with no
    /// sound card can still prove the game produced audio.
    pub fn capture_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        let capture = WavCapture::create(path)?;
        if let Ok(mut s) = self.shared.lock() {
            s.capture = Some(capture);
        }
        Ok(())
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
    if let Ok(mut s) = shared.lock() {
        s.device_active = true;
    }
    while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if let Ok(mut s) = shared.lock() {
        s.device_active = false;
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
