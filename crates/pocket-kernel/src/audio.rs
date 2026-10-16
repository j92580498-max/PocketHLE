//! Real-time audio output backed by [`cpal`].
//!
//! The Pocket PC `waveOut*` API and `PlaySoundW` / `PlaySoundA` /
//! `sndPlaySoundW` family are routed through this module so that
//! games which previously had no sound can drive real PCM samples
//! out of the host's default audio device. Each HWAVEOUT has an independent
//! PCM queue, format, pause state and completion clock. Both cpal and the
//! Android tap mix these queues concurrently using nearest-neighbour
//! resampling. The legacy ring remains available for push_samples callers.
//! Resetting a wave stream clears only that handle; already delivered host
//! frames can only disappear after the host's short output buffer drains.
//!
//! The cpal feature is optional. Without it, host frontends can still pull
//! mixed PCM through AudioTap (Android); headless completion uses a wall
//! clock when no output device consumes samples.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Maximum number of i16 samples we keep buffered. At 44.1 kHz
/// stereo this is about twelve seconds — plenty to absorb scheduling
/// jitter, but small enough that overflows produce dropped samples
/// instead of unbounded memory growth.
const RING_CAPACITY_SAMPLES: usize = 1 << 20; // 1048576

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

#[derive(Debug)]
#[cfg(feature = "audio-cpal")]
struct AudioVoice {
    samples: Vec<i16>,
    format: GuestFormat,
    position_q16: u64,
    looped: bool,
    volume: f32,
    group: u32,
    paused: bool,
}

/// How a voice submitted through [`AudioEngine::play_voice_with`]
/// should behave.
///
/// `group` exists so a caller can silence one class of sound without
/// touching the rest: `hss.dll`'s `stopMusics` must stop the music
/// without cutting off the sound effects that are still playing.
#[derive(Debug, Clone, Copy)]
pub struct VoiceParams {
    pub looped: bool,
    /// Tag for [`AudioEngine::stop_voice_group`]. Voices submitted
    /// through [`AudioEngine::play_voice`] land in group 0.
    pub group: u32,
    /// Linear gain. 1.0 plays the samples as submitted.
    pub volume: f32,
}

impl Default for VoiceParams {
    fn default() -> Self {
        Self {
            looped: false,
            group: 0,
            volume: 1.0,
        }
    }
}

/// One HWAVEOUT owns its PCM, format, resampling phase and completion clock.
/// Keep complete submissions: Battlestations submits its entire menu music at
/// once, and a bounded global ring both loses its beginning and delays SFX.
#[derive(Clone)]
struct WaveLoop {
    samples: Vec<i16>,
    start: u64,
    index: usize,
    remaining: u32,
}

#[derive(Clone)]
struct WaveStream {
    loop_buffer: Option<WaveLoop>,
    samples: std::collections::VecDeque<i16>,
    format: GuestFormat,
    phase: u64,
    written: u64,
    consumed: u64,
    paused: bool,
    volume: u32,
    tick: Instant,
    fraction: u128,
}

impl WaveStream {
    fn new(format: GuestFormat) -> Self {
        Self {
            loop_buffer: None,
            samples: Default::default(),
            format,
            phase: 0,
            written: 0,
            consumed: 0,
            paused: false,
            volume: u32::MAX,
            tick: Instant::now(),
            fraction: 0,
        }
    }

    fn advance_virtual(&mut self, active: bool) {
        let now = Instant::now();
        if !active && !self.paused && self.consumed < self.written {
            let rate = self.format.sample_rate.max(1) as u128;
            let elapsed = now.duration_since(self.tick).as_nanos() * rate + self.fraction;
            let frames = elapsed / 1_000_000_000;
            self.fraction = elapsed % 1_000_000_000;
            self.consume(
                (frames * self.format.channels.max(1) as u128).min(usize::MAX as u128) as usize,
            );
        }
        self.tick = now;
        if self.samples.is_empty() && self.loop_buffer.is_none() {
            self.fraction = 0;
        }
    }

    fn buffered_samples(&self) -> usize {
        self.samples.len()
            + self
                .loop_buffer
                .as_ref()
                .map(|v| v.samples.len())
                .unwrap_or(0)
    }

    fn consume(&mut self, count: usize) {
        let mut count = count as u64;
        while count > 0 {
            if let Some(v) = self.loop_buffer.as_mut() {
                if self.consumed >= v.start {
                    let length = v.samples.len() as u64;
                    let available = length * v.remaining as u64 - v.index as u64;
                    let advance = count.min(available);
                    let position = v.index as u64 + advance;
                    v.remaining -= (position / length) as u32;
                    v.index = (position % length) as usize;
                    self.consumed = self.consumed.saturating_add(advance);
                    count -= advance;
                    if v.remaining == 0 {
                        self.loop_buffer = None;
                    }
                    continue;
                }
            }
            let before_loop = self
                .loop_buffer
                .as_ref()
                .map(|v| v.start.saturating_sub(self.consumed))
                .unwrap_or(u64::MAX);
            let advance = count.min(before_loop).min(self.samples.len() as u64) as usize;
            if advance == 0 {
                break;
            }
            self.samples.drain(..advance);
            self.consumed = self.consumed.saturating_add(advance as u64);
            count -= advance as u64;
        }
    }

    fn frame(&mut self, rate: u32) -> (f32, f32) {
        if self.paused {
            return (0.0, 0.0);
        }
        let channels = self.format.channels.max(1) as usize;
        let (left, right) = if let Some(v) = self
            .loop_buffer
            .as_ref()
            .filter(|v| self.consumed >= v.start)
        {
            let left = v.samples[v.index];
            let right = if channels > 1 {
                v.samples[v.index + 1]
            } else {
                left
            };
            (left, right)
        } else {
            if self.samples.len() < channels {
                return (0.0, 0.0);
            }
            let left = self.samples[0];
            let right = if channels > 1 { self.samples[1] } else { left };
            (left, right)
        };
        self.phase += ((self.format.sample_rate.max(1) as u64) << 16) / rate.max(1) as u64;
        let frames = self.phase >> 16;
        self.phase &= 0xffff;
        self.consume((frames as usize).saturating_mul(channels));
        if self.samples.is_empty() && self.loop_buffer.is_none() {
            self.phase = 0;
        }
        (
            left as f32 / 32768.0 * volume_gain(self.volume, 0),
            right as f32 / 32768.0 * volume_gain(self.volume, 1),
        )
    }
}

fn volume_gain(volume: u32, channel: usize) -> f32 {
    ((volume >> (if channel == 0 { 0 } else { 16 })) & 0xffff) as f32 / 65535.0
}

/// PCM and volume shared by aliases of one MAS device, including other processes.
#[derive(Clone)]
pub struct MasPlayback(Arc<Mutex<Option<WaveStream>>>, Arc<Mutex<u32>>);
impl Default for MasPlayback {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(None)), Arc::new(Mutex::new(u32::MAX)))
    }
}
impl MasPlayback {
    fn buffered_samples(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |v| v.buffered_samples())
    }
}

/// Inner state shared between the emulator thread (which calls
/// [`AudioEngine::push_samples`]) and the cpal output callback.
struct Shared {
    device_volume: u32,
    wave_streams: std::collections::BTreeMap<u32, WaveStream>,
    // MAS1 transport is independent: waveOutReset/Close cannot purge music.
    mas_streams: std::collections::BTreeMap<u32, MasPlayback>,
    ring: Vec<i16>,
    /// Number of samples currently in the ring.
    len: usize,
    /// Read cursor.
    read: usize,
    /// Write cursor.
    write: usize,
    /// Format of the most recently submitted guest stream.
    guest_format: GuestFormat,
    /// Stable output format used by host frontends for the lifetime of
    /// the audio session. Android AudioTrack must not be reconfigured
    /// halfway through a session when another waveOut device opens.
    mix_format: GuestFormat,
    mix_format_ready: bool,
    /// Track whether the guest explicitly opened audio so Android does
    /// not build AudioTrack from the fallback format.
    guest_format_ready: bool,
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
    /// `true` between `waveOutPause` and `waveOutRestart`. The device
    /// keeps its queue but stops consuming, so the playback cursor
    /// freezes and buffer-done notifications stop until playback
    /// resumes.
    paused: bool,
    /// Independent sound-effect voices mixed over the waveOut stream.
    #[cfg(feature = "audio-cpal")]
    voices: Vec<AudioVoice>,
    /// Optional WAV tap so headless runs can verify that a game
    /// really produces sound on a machine with no audio hardware.
    capture: Option<WavCapture>,
}

impl Shared {
    fn new() -> Self {
        Self {
            device_volume: u32::MAX,
            wave_streams: Default::default(),
            mas_streams: Default::default(),
            ring: vec![0i16; RING_CAPACITY_SAMPLES],
            len: 0,
            read: 0,
            write: 0,
            guest_format: GuestFormat::default(),
            mix_format: GuestFormat::default(),
            mix_format_ready: false,
            guest_format_ready: false,
            resampler_phase: 0,
            written: 0,
            consumed: 0,
            device_active: false,
            virtual_cursor: 0,
            virtual_tick: None,
            paused: false,
            #[cfg(feature = "audio-cpal")]
            voices: Vec::new(),
            capture: None,
        }
    }

    /// Guest samples played so far. Backed by the host device when we
    /// have one and by a wall clock otherwise. Never runs ahead of
    /// what the guest actually submitted, so a game that stops
    /// feeding buffers doesn't see phantom progress.
    fn cursor(&mut self) -> u64 {
        if self.paused {
            // Reset the wall-clock reference so a long pause doesn't
            // release a burst of buffers the moment playback resumes.
            self.virtual_tick = None;
            return if self.device_active {
                self.consumed.min(self.written)
            } else {
                self.virtual_cursor
            };
        }
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
        self.wave_streams.clear();
        self.mas_streams.clear();
        self.len = 0;
        self.read = 0;
        self.write = 0;
        self.resampler_phase = 0;
        self.written = 0;
        self.consumed = 0;
        self.virtual_cursor = 0;
        self.virtual_tick = None;
        #[cfg(feature = "audio-cpal")]
        self.voices.clear();
    }

    #[cfg(feature = "audio-cpal")]
    fn stop_voices(&mut self) {
        self.voices.clear();
    }

    #[cfg(feature = "audio-cpal")]
    fn stop_voice_group(&mut self, group: u32) {
        self.voices.retain(|v| v.group != group);
    }

    #[cfg(feature = "audio-cpal")]
    fn pause_voice_group(&mut self, group: u32, paused: bool) {
        for voice in &mut self.voices {
            if voice.group == group {
                voice.paused = paused;
            }
        }
    }

    #[cfg(feature = "audio-cpal")]
    fn add_voice(&mut self, samples: Vec<i16>, format: GuestFormat, params: VoiceParams) {
        if samples.is_empty() {
            return;
        }
        if let Some(cap) = self.capture.as_mut() {
            cap.write(&samples, format);
        }
        self.voices.push(AudioVoice {
            samples,
            format,
            position_q16: 0,
            looped: params.looped,
            volume: params.volume,
            group: params.group,
            paused: false,
        });
    }

    fn render_frames(&mut self, output: &mut [f32], output_rate: u32, output_channels: u16) {
        let channels = output_channels.max(1) as usize;
        let frames = output.len() / channels;
        for frame in 0..frames {
            let mut left = 0.0f32;
            let mut right = 0.0f32;
            if !self.paused {
                let guest_channels = self.guest_format.channels.max(1) as usize;
                if self.len >= guest_channels {
                    let left_sample = self.ring[self.read] as f32 / 32768.0;
                    let right_sample = if guest_channels > 1 {
                        self.ring[(self.read + 1) % self.ring.len()] as f32 / 32768.0
                    } else {
                        left_sample
                    };
                    left += left_sample;
                    right += right_sample;
                    let step_q16 = ((self.guest_format.sample_rate.max(1) as u64) << 16)
                        / output_rate.max(1) as u64;
                    self.resampler_phase = self.resampler_phase.saturating_add(step_q16);
                    let advance_frames = (self.resampler_phase >> 16) as usize;
                    self.resampler_phase &= 0xFFFF;
                    for _ in 0..advance_frames.saturating_mul(guest_channels) {
                        let _ = self.pop_one();
                    }
                } else {
                    self.resampler_phase = 0;
                }

                for transport in self.mas_streams.values() {
                    if let Some(stream) = transport.0.lock().unwrap().as_mut() {
                        let (l, r) = stream.frame(output_rate);
                        left += l;
                        right += r;
                    }
                }

                #[cfg(feature = "audio-cpal")]
                {
                    let mut index = 0;
                    while index < self.voices.len() {
                        let voice = &mut self.voices[index];
                        if voice.paused {
                            index += 1;
                            continue;
                        }
                        let voice_channels = voice.format.channels.max(1) as usize;
                        let frame_count = voice.samples.len() / voice_channels;
                        let source_frame = (voice.position_q16 >> 16) as usize;
                        if frame_count == 0 || source_frame >= frame_count {
                            if voice.looped && frame_count > 0 {
                                voice.position_q16 %= (frame_count as u64) << 16;
                            } else {
                                self.voices.swap_remove(index);
                                continue;
                            }
                        }
                        let source_frame = (voice.position_q16 >> 16) as usize;
                        let sample_index = source_frame * voice_channels;
                        let left_sample =
                            voice.samples[sample_index] as f32 / 32768.0 * voice.volume;
                        let right_sample = if voice_channels > 1 {
                            voice.samples[sample_index + 1] as f32 / 32768.0 * voice.volume
                        } else {
                            left_sample
                        };
                        left += left_sample;
                        right += right_sample;
                        voice.position_q16 = voice.position_q16.saturating_add(
                            ((voice.format.sample_rate.max(1) as u64) << 16)
                                / output_rate.max(1) as u64,
                        );
                        index += 1;
                    }
                }
                // MAS, legacy PCM and HSS share the emulated output device.
                // waveOut handles have absolute instance volumes of their own.
                left *= volume_gain(self.device_volume, 0);
                right *= volume_gain(self.device_volume, 1);
                for stream in self.wave_streams.values_mut() {
                    let (l, r) = stream.frame(output_rate);
                    left += l;
                    right += r;
                }
            }
            left = left.clamp(-1.0, 1.0);
            right = right.clamp(-1.0, 1.0);
            for channel in 0..channels {
                output[frame * channels + channel] = if channel == 0 { left } else { right };
            }
        }
        for sample in &mut output[frames * channels..] {
            *sample = 0.0;
        }
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

    /// Append decoded MAS1 buffers to one continuous per-device PCM queue.
    /// START/WRITE must never replace the previous buffer (Interstellar Flames 2).
    pub fn queue_mas_samples(&self, handle: u32, format: GuestFormat, samples: &[i16]) -> u64 {
        let Ok(mut s) = self.shared.lock() else {
            return 0;
        };
        if !s.mix_format_ready {
            s.mix_format = format;
            s.mix_format_ready = true;
        }
        s.guest_format_ready = true;
        if let Some(capture) = s.capture.as_mut() {
            capture.write(samples, format);
        }
        let active = s.device_active;
        let transport = s.mas_streams.entry(handle).or_default();
        let mut buffer = transport.0.lock().unwrap();
        let stream = buffer.get_or_insert_with(|| WaveStream::new(format));
        stream.volume = *transport.1.lock().unwrap();
        stream.advance_virtual(active);
        stream.samples.extend(samples.iter().copied());
        stream.written += samples.len() as u64;
        stream.written
    }

    pub fn mas_written_samples(&self, handle: u32) -> u64 {
        self.shared
            .lock()
            .ok()
            .and_then(|s| {
                s.mas_streams
                    .get(&handle)
                    .map(|v| v.0.lock().unwrap().as_ref().map_or(0, |v| v.written))
            })
            .unwrap_or(0)
    }

    pub fn mas_playback_cursor(&self, handle: u32) -> u64 {
        let Ok(s) = self.shared.lock() else {
            return 0;
        };
        let active = s.device_active;
        s.mas_streams
            .get(&handle)
            .map(|transport| {
                let mut buffer = transport.0.lock().unwrap();
                buffer.as_mut().map_or(0, |v| {
                    v.advance_virtual(active);
                    v.consumed
                })
            })
            .unwrap_or(0)
    }

    pub fn stop_mas_stream(&self, handle: u32) {
        if let Ok(s) = self.shared.lock() {
            if let Some(transport) = s.mas_streams.get(&handle) {
                *transport.0.lock().unwrap() = None;
            }
        }
    }

    pub fn pause_mas_stream(&self, handle: u32, paused: bool) {
        if let Ok(s) = self.shared.lock() {
            let active = s.device_active;
            if let Some(transport) = s.mas_streams.get(&handle) {
                if let Some(v) = transport.0.lock().unwrap().as_mut() {
                    v.advance_virtual(active);
                    v.paused = paused;
                }
            }
        }
    }

    pub fn export_mas_stream(&self, key: u32) -> MasPlayback {
        self.shared
            .lock()
            .unwrap()
            .mas_streams
            .entry(key)
            .or_default()
            .clone()
    }
    pub fn import_mas_stream(&self, key: u32, transport: MasPlayback) {
        let format = transport.0.lock().unwrap().as_ref().map(|v| v.format);
        let mut shared = self.shared.lock().unwrap();
        if let Some(format) = format {
            if !shared.mix_format_ready {
                shared.mix_format = format;
                shared.mix_format_ready = true;
            }
            shared.guest_format_ready = true;
        }
        shared.mas_streams.insert(key, transport);
    }
    /// Drop this session's output reference without resetting another owner's PCM.
    pub fn detach_mas_stream(&self, key: u32) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.mas_streams.remove(&key);
        }
    }

    pub fn open_wave_stream(&self, handle: u32, format: GuestFormat) {
        self.set_guest_format(format);
        if let Ok(mut s) = self.shared.lock() {
            let mut stream = WaveStream::new(format);
            stream.volume = s.device_volume;
            s.wave_streams.insert(handle, stream);
        }
    }

    /// Device ID 0 updates existing instances and the default for future opens.
    /// An open HWAVEOUT updates only that stream, including queued/live-loop PCM.
    pub fn set_wave_volume(&self, handle: u32, volume: u32) -> bool {
        let Ok(mut shared) = self.shared.lock() else {
            return false;
        };
        if handle == 0 {
            shared.device_volume = volume;
            for stream in shared.wave_streams.values_mut() {
                stream.volume = volume;
            }
            true
        } else if let Some(stream) = shared.wave_streams.get_mut(&handle) {
            stream.volume = volume;
            true
        } else {
            false
        }
    }

    pub fn wave_volume(&self, handle: u32) -> Option<u32> {
        let shared = self.shared.lock().ok()?;
        if handle == 0 {
            Some(shared.device_volume)
        } else {
            shared.wave_streams.get(&handle).map(|stream| stream.volume)
        }
    }

    /// Retain MAS volume before START and share it with imported aliases.
    pub fn set_mas_volume(&self, key: u32, volume: u32) {
        if let Ok(mut shared) = self.shared.lock() {
            let transport = shared.mas_streams.entry(key).or_default();
            *transport.1.lock().unwrap() = volume;
            if let Some(stream) = transport.0.lock().unwrap().as_mut() {
                stream.volume = volume;
            }
        }
    }

    pub fn push_wave_samples(&self, handle: u32, samples: &[i16]) {
        if let Ok(mut s) = self.shared.lock() {
            if let Some(format) = s.wave_streams.get(&handle).map(|v| v.format) {
                if let Some(capture) = s.capture.as_mut() {
                    capture.write(samples, format);
                }
            }
            let active = s.device_active;
            if let Some(stream) = s.wave_streams.get_mut(&handle) {
                stream.advance_virtual(active);
                stream.samples.extend(samples.iter().copied());
                stream.written += samples.len() as u64;
            }
        }
    }

    pub fn wave_written_samples(&self, handle: u32) -> u64 {
        self.shared
            .lock()
            .ok()
            .and_then(|s| s.wave_streams.get(&handle).map(|v| v.written))
            .unwrap_or(0)
    }

    pub fn wave_playback_cursor(&self, handle: u32) -> u64 {
        let Ok(mut s) = self.shared.lock() else {
            return 0;
        };
        let active = s.device_active;
        s.wave_streams
            .get_mut(&handle)
            .map(|v| {
                v.advance_virtual(active);
                v.consumed
            })
            .unwrap_or(0)
    }

    /// Queue one WAVEHDR with BEGINLOOP|ENDLOOP. No repeated PCM copies.
    /// Emulator-side refreshes replace its data without resetting its position.
    pub fn queue_wave_loop(&self, handle: u32, samples: Vec<i16>, repeats: u32) -> bool {
        let Ok(mut s) = self.shared.lock() else {
            return false;
        };
        let active = s.device_active;
        let Some(v) = s.wave_streams.get_mut(&handle) else {
            return false;
        };
        let channels = v.format.channels.max(1) as usize;
        if samples.is_empty() || samples.len() % channels != 0 || v.loop_buffer.is_some() {
            return false;
        }
        v.advance_virtual(active);
        let repeats = repeats.max(1);
        let start = v.written;
        v.written = v
            .written
            .saturating_add((samples.len() as u64).saturating_mul(repeats as u64));
        v.loop_buffer = Some(WaveLoop {
            samples,
            start,
            index: 0,
            remaining: repeats,
        });
        true
    }

    /// Finish the current loop iteration, preserving its playback position and
    /// the PCM queued afterwards. Returns the old and shortened loop endpoints.
    pub fn break_wave_loop(&self, handle: u32) -> Option<(u64, u64)> {
        let mut shared = self.shared.lock().ok()?;
        let active = shared.device_active;
        let stream = shared.wave_streams.get_mut(&handle)?;
        stream.advance_virtual(active);
        let wave_loop = stream.loop_buffer.as_mut()?;
        let length = wave_loop.samples.len() as u64;
        let base = stream.consumed.max(wave_loop.start);
        let old_end = base + length * wave_loop.remaining as u64 - wave_loop.index as u64;
        let new_end = base + length - wave_loop.index as u64;
        wave_loop.remaining = 1;
        stream.written -= old_end - new_end;
        Some((old_end, new_end))
    }

    pub fn update_wave_loop(&self, handle: u32, samples: Vec<i16>) {
        if let Ok(mut s) = self.shared.lock() {
            if let Some(v) = s
                .wave_streams
                .get_mut(&handle)
                .and_then(|v| v.loop_buffer.as_mut())
            {
                if samples.len() == v.samples.len() {
                    v.samples = samples;
                }
            }
        }
    }

    pub fn reset_wave_stream(&self, handle: u32) {
        if let Ok(mut s) = self.shared.lock() {
            if let Some(v) = s.wave_streams.get_mut(&handle) {
                let volume = v.volume;
                *v = WaveStream::new(v.format);
                v.volume = volume;
            }
        }
    }

    pub fn close_wave_stream(&self, handle: u32) {
        if let Ok(mut s) = self.shared.lock() {
            s.wave_streams.remove(&handle);
        }
    }

    pub fn pause_wave_stream(&self, handle: u32, paused: bool) {
        if let Ok(mut s) = self.shared.lock() {
            let active = s.device_active;
            if let Some(v) = s.wave_streams.get_mut(&handle) {
                v.advance_virtual(active);
                v.paused = paused;
            }
        }
    }

    /// Update the guest-side format. Called from `waveOutOpen` /
    /// `PlaySound` so the resampler knows what rate the i16 samples
    /// are coming in at.
    pub fn set_guest_format(&self, fmt: GuestFormat) {
        if let Ok(mut s) = self.shared.lock() {
            s.guest_format = fmt;
            if !s.mix_format_ready {
                s.mix_format = fmt;
                s.mix_format_ready = true;
            }
            s.guest_format_ready = true;
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

    pub fn play_voice(&self, samples: &[i16], format: GuestFormat, looped: bool) -> usize {
        self.play_voice_with(
            samples,
            format,
            VoiceParams {
                looped,
                ..Default::default()
            },
        )
    }

    /// Submit a voice with an explicit group and gain — see
    /// [`VoiceParams`].
    pub fn play_voice_with(
        &self,
        samples: &[i16],
        format: GuestFormat,
        params: VoiceParams,
    ) -> usize {
        #[cfg(feature = "audio-cpal")]
        {
            if let Ok(mut s) = self.shared.lock() {
                s.add_voice(samples.to_vec(), format, params);
                samples.len()
            } else {
                0
            }
        }
        #[cfg(not(feature = "audio-cpal"))]
        {
            let _ = (format, params);
            self.push_samples(samples)
        }
    }

    pub fn stop_voices(&self) {
        #[cfg(feature = "audio-cpal")]
        if let Ok(mut s) = self.shared.lock() {
            s.stop_voices();
        }
    }

    /// Stop only the voices tagged with `group`, leaving every other
    /// voice playing. Without the `audio-cpal` feature there are no
    /// voices to stop and this does nothing.
    pub fn stop_voice_group(&self, group: u32) {
        #[cfg(feature = "audio-cpal")]
        if let Ok(mut s) = self.shared.lock() {
            s.stop_voice_group(group);
        }
        #[cfg(not(feature = "audio-cpal"))]
        let _ = group;
    }

    /// Pause or resume only one mixer voice group.  This is used by devices
    /// such as Gizmondo MAS1 whose transport is independent of waveOut.
    pub fn pause_voice_group(&self, group: u32, paused: bool) {
        #[cfg(feature = "audio-cpal")]
        if let Ok(mut s) = self.shared.lock() {
            s.pause_voice_group(group, paused);
        }
        #[cfg(not(feature = "audio-cpal"))]
        let _ = (group, paused);
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
            s.mix_format_ready = false;
            s.guest_format_ready = false;
            s.resampler_phase = 0;
        }
    }

    /// Flush only the WinMM/waveOut PCM stream. Independent mixer voices
    /// (for example Gizmondo MAS1 playback) keep running.
    pub fn flush_wave_out(&self) {
        if let Ok(mut s) = self.shared.lock() {
            s.wave_streams.clear();
            s.len = 0;
            s.read = 0;
            s.write = 0;
            s.resampler_phase = 0;
            s.written = 0;
            s.consumed = 0;
            s.virtual_cursor = 0;
            s.virtual_tick = None;
            if s.mas_streams.is_empty() {
                s.mix_format_ready = false;
                s.guest_format_ready = false;
            }
        }
    }

    /// Suspend or resume playback (`waveOutPause` / `waveOutRestart`).
    /// While paused the device keeps its queue but stops consuming, so
    /// [`Self::playback_cursor`] freezes and no further buffers are
    /// reported as finished.
    pub fn set_paused(&self, paused: bool) {
        if let Ok(mut s) = self.shared.lock() {
            s.paused = paused;
        }
    }

    /// Number of samples currently queued.
    pub fn buffered_samples(&self) -> usize {
        self.shared
            .lock()
            .map(|s| {
                s.len
                    + s.wave_streams
                        .values()
                        .map(|v| v.buffered_samples())
                        .sum::<usize>()
                    + s.mas_streams
                        .values()
                        .map(|v| v.buffered_samples())
                        .sum::<usize>()
            })
            .unwrap_or(0)
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

    /// A cheap, clone-able handle that lets the *host* play the guest's
    /// PCM itself instead of leaving it to cpal.
    ///
    /// Android needs this: cpal's Android backend goes through
    /// `oboe-sys`, which needs an NDK C++ toolchain we do not ship, so
    /// the `audio-cpal` feature is off there and the ring buffer would
    /// otherwise just fill up and drop samples. The JNI layer pulls from
    /// a tap and writes into a Java `AudioTrack` in streaming mode — the
    /// same shape J2ME emulators such as J2ME Loader use for PCM output.
    ///
    /// The tap can be moved to another thread; it locks the same shared
    /// state the guest pushes into.
    pub fn tap(&self) -> AudioTap {
        AudioTap {
            shared: Arc::clone(&self.shared),
        }
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
        let handle = match std::thread::Builder::new()
            .name("pockethle-audio".to_string())
            .spawn(move || run_audio_worker(shared, shutdown))
        {
            Ok(handle) => Some(handle),
            Err(e) => {
                log::warn!("AudioEngine: could not spawn audio thread — running silently: {e}");
                None
            }
        };
        self.worker = handle;
    }

    #[cfg(not(feature = "audio-cpal"))]
    fn start_impl(&mut self) {
        // A build with the feature off can never make a sound, and the
        // guest cannot tell — so this is a warning, not a note. It is
        // the first thing to rule out when a packaged binary is silent
        // but a local one is not.
        log::warn!("AudioEngine: built without the audio-cpal feature — running silently");
    }

    /// Stop host output while preserving every queued sample and cursor.
    pub fn suspend_output(&mut self) {
        #[cfg(feature = "audio-cpal")]
        {
            self.shutdown
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
        #[cfg(not(feature = "audio-cpal"))]
        {
            self.worker = None;
        }
        self.init_attempted = false;
    }

    pub fn resume_output(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.virtual_tick = None;
            let now = Instant::now();
            for stream in shared.wave_streams.values_mut() {
                stream.tick = now;
            }
            for transport in shared.mas_streams.values() {
                if let Some(stream) = transport.0.lock().unwrap().as_mut() {
                    stream.tick = now;
                }
            }
        }
        self.start();
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

/// Host-side pull handle produced by [`AudioEngine::tap`].
///
/// Draining moves the playback cursor forward, so `waveOutGetPosition`
/// and the `WHDR_DONE` notifications stay in step with what the host has
/// actually played. While nothing drains the tap the engine keeps using
/// its wall-clock estimate, so a frontend that never calls
/// [`AudioTap::drain_into`] behaves exactly as it did before.
#[derive(Clone)]
pub struct AudioTap {
    shared: Arc<Mutex<Shared>>,
}

impl std::fmt::Debug for AudioTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioTap").finish_non_exhaustive()
    }
}

impl AudioTap {
    /// The format the guest is currently submitting samples in, so the
    /// host can configure its own output device to match.
    pub fn guest_format(&self) -> GuestFormat {
        self.shared
            .lock()
            .map(|s| {
                if s.mix_format_ready {
                    s.mix_format
                } else {
                    s.guest_format
                }
            })
            .unwrap_or_default()
    }

    pub fn format_ready(&self) -> bool {
        self.shared
            .lock()
            .map(|s| s.guest_format_ready)
            .unwrap_or(false)
    }

    /// Move up to `dst.len()` queued samples out of the ring into `dst`,
    /// returning how many were written. Never blocks: a starved host
    /// gets a short read and should pad the rest with silence.
    pub fn drain_into(&self, dst: &mut [i16]) -> usize {
        let Ok(mut s) = self.shared.lock() else {
            return 0;
        };
        s.device_active = true;
        if !s.wave_streams.is_empty() || !s.mas_streams.is_empty() {
            let format = s.mix_format;
            let mut output = vec![0.0; dst.len()];
            s.render_frames(&mut output, format.sample_rate, format.channels);
            for (sample, mixed) in dst.iter_mut().zip(output) {
                *sample = (mixed * 32768.0).clamp(-32768.0, 32767.0) as i16;
            }
            return dst.len();
        }
        let mut n = 0;
        while n < dst.len() {
            match s.pop_one() {
                Some(v) => {
                    let channel = (s.consumed.saturating_sub(1)
                        % s.guest_format.channels.max(1) as u64)
                        as usize;
                    dst[n] = (v as f32 * volume_gain(s.device_volume, channel)) as i16;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// Move up to `dst.len()` samples without taking ownership of the
    /// playback cursor. This is used by Android while it is probing the
    /// guest format; probing must not switch the engine to device-clock
    /// mode before AudioTrack has actually started.
    pub fn peek_into(&self, dst: &mut [i16]) -> usize {
        let Ok(s) = self.shared.lock() else {
            return 0;
        };
        if !s.wave_streams.is_empty() || !s.mas_streams.is_empty() {
            let channels = s.mix_format.channels.max(1) as usize;
            let mut streams = s.wave_streams.clone();
            let mut mas_streams: std::collections::BTreeMap<u32, WaveStream> = s
                .mas_streams
                .iter()
                .filter_map(|(&key, transport)| {
                    transport
                        .0
                        .lock()
                        .unwrap()
                        .clone()
                        .map(|stream| (key, stream))
                })
                .collect();
            for frame in dst.chunks_exact_mut(channels) {
                let (mut left, mut right) = (0.0f32, 0.0f32);
                for stream in streams.values_mut().chain(mas_streams.values_mut()) {
                    let (l, r) = stream.frame(s.mix_format.sample_rate);
                    left += l;
                    right += r;
                }
                for (ch, sample) in frame.iter_mut().enumerate() {
                    *sample = ((if ch == 0 { left } else { right }) * 32768.0)
                        .clamp(-32768.0, 32767.0) as i16;
                }
            }
            return dst.len() / channels * channels;
        }
        let mut n = 0;
        let mut index = s.read;
        let mut remaining = s.len;
        while n < dst.len() && remaining > 0 {
            dst[n] = s.ring[index];
            n += 1;
            remaining -= 1;
            index = (index + 1) % s.ring.len();
        }
        n
    }

    /// How many samples are queued and ready to be drained.
    pub fn buffered_samples(&self) -> usize {
        self.shared
            .lock()
            .map(|s| {
                s.len
                    + s.wave_streams
                        .values()
                        .map(|v| v.buffered_samples())
                        .sum::<usize>()
                    + s.mas_streams
                        .values()
                        .map(|v| v.buffered_samples())
                        .sum::<usize>()
            })
            .unwrap_or(0)
    }
}

#[cfg(feature = "audio-cpal")]
fn run_audio_worker(shared: Arc<Mutex<Shared>>, shutdown: Arc<std::sync::atomic::AtomicBool>) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = match host.default_output_device() {
        Some(d) => d,
        None => {
            // Every early return in this function means the run is
            // silent. That is a user-visible failure, not a detail, so
            // these log at `warn`: the desktop GUI is built with
            // `windows_subsystem = "windows"` and has no console, so an
            // `info` line reaches nobody and "no sound" arrives with no
            // explanation attached.
            log::warn!("AudioEngine: no default output device — running silently");
            return;
        }
    };
    let device_name = device.name().unwrap_or_else(|_| "<unnamed>".to_string());
    let config = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "AudioEngine: default_output_config() failed on {device_name:?} \
                 — running silently: {e}"
            );
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
            log::warn!("AudioEngine: host device {device_name:?} wants unsupported sample format {other:?} — running silently");
            return;
        }
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "AudioEngine: build_output_stream failed on {device_name:?} \
                 ({host_rate} Hz / {host_channels} ch / {sample_format:?}) \
                 — running silently: {e}"
            );
            return;
        }
    };
    if let Err(e) = stream.play() {
        log::warn!("AudioEngine: stream.play() failed on {device_name:?} — running silently: {e}");
        return;
    }
    log::info!(
        "AudioEngine: opened {:?} at {} Hz / {} ch ({:?}), buffer={:?}",
        device_name,
        host_rate,
        host_channels,
        sample_format,
        stream_config.buffer_size
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
            data.fill(0.0);
            return;
        }
    };
    s.render_frames(data, host_rate, host_channels);
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
    fn wave_volume_updates_queued_loop_and_future_streams_without_stopping_cursors() {
        let engine = AudioEngine::new();
        let tap = engine.tap();
        tap.drain_into(&mut []);
        engine.open_wave_stream(1, mono(44100));
        engine.queue_wave_loop(1, vec![12000; 4], 10);
        assert!(engine.set_wave_volume(0, 0));
        let mut out = [1; 2];
        tap.drain_into(&mut out);
        assert_eq!(out, [0, 0]);
        assert_eq!(engine.wave_playback_cursor(1), 2);
        assert!(engine.set_wave_volume(0, 0x8000_8000));
        tap.drain_into(&mut out);
        assert_eq!(out, [6000, 6000]);
        engine.open_wave_stream(2, mono(44100));
        assert_eq!(engine.wave_volume(2), Some(0x8000_8000));
        assert!(engine.set_wave_volume(1, u32::MAX));
        assert_eq!(engine.wave_volume(2), Some(0x8000_8000));
        tap.drain_into(&mut out);
        assert_eq!(out, [12000, 12000]);
        assert_eq!(engine.wave_playback_cursor(1), 6);
        engine.set_wave_volume(1, 0x8000_8000);
        engine.reset_wave_stream(1);
        assert_eq!(engine.wave_volume(1), Some(0x8000_8000));
        engine.push_wave_samples(1, &[12000; 2]);
        tap.drain_into(&mut out);
        assert_eq!(out, [6000, 6000]);
        assert!(!engine.set_wave_volume(99, 0));
        assert_eq!(engine.wave_volume(99), None);
    }

    #[test]
    fn mas_volume_is_live_retained_before_start_and_shared_by_aliases() {
        let engine = AudioEngine::new();
        let tap = engine.tap();
        tap.drain_into(&mut []);
        engine.set_mas_volume(7, 0x8000_8000);
        engine.queue_mas_samples(7, mono(44100), &[12000; 8]);
        let mut out = [0; 1];
        tap.drain_into(&mut out);
        assert_eq!(out, [6000]);
        let alias = AudioEngine::new();
        alias.import_mas_stream(7, engine.export_mas_stream(7));
        alias.set_mas_volume(7, 0);
        tap.drain_into(&mut out);
        assert_eq!(out, [0]);
        assert_eq!(engine.mas_playback_cursor(7), 2);
        alias.set_mas_volume(7, u32::MAX);
        engine.set_wave_volume(0, 0x8000_8000);
        tap.drain_into(&mut out);
        assert_eq!(out, [6000]);
        engine.stop_mas_stream(7);
        engine.queue_mas_samples(7, mono(44100), &[12000; 2]);
        engine.set_wave_volume(0, u32::MAX);
        tap.drain_into(&mut out);
        assert_eq!(out, [12000]);
    }

    #[test]
    fn legacy_volume_keeps_channel_alignment_across_short_pulls() {
        let engine = AudioEngine::new();
        engine.set_guest_format(GuestFormat::default());
        engine.push_samples(&[12000; 4]);
        engine.set_wave_volume(0, 0xffff_0000);
        let tap = engine.tap();
        let mut out = [1; 1];
        for expected in [0, 12000, 0, 12000] {
            assert_eq!(tap.drain_into(&mut out), 1);
            assert_eq!(out, [expected]);
        }
    }

    #[test]
    fn wave_volume_has_independent_left_and_right_channels() {
        let engine = AudioEngine::new();
        engine.open_wave_stream(
            1,
            GuestFormat {
                channels: 2,
                ..GuestFormat::default()
            },
        );
        engine.push_wave_samples(1, &[12000; 4]);
        engine.set_wave_volume(1, 0xffff_0000);
        let mut out = [0; 2];
        engine.tap().drain_into(&mut out);
        assert_eq!(out, [0, 12000]);
        engine.set_wave_volume(1, 0x0000_ffff);
        engine.tap().drain_into(&mut out);
        assert_eq!(out, [12000, 0]);
    }

    #[test]
    fn break_loop_finishes_current_iteration_and_preserves_suffix() {
        for prefix in [false, true] {
            let engine = AudioEngine::new();
            let tap = engine.tap();
            tap.drain_into(&mut []);
            engine.open_wave_stream(1, mono(44100));
            if prefix {
                engine.push_wave_samples(1, &[10]);
            }
            assert!(engine.queue_wave_loop(1, vec![20, 30, 40], 4));
            engine.push_wave_samples(1, &[50]);
            let old_end = if prefix { 13 } else { 12 };
            if !prefix {
                let mut consumed = [0; 4];
                tap.drain_into(&mut consumed);
                assert_eq!(consumed, [20, 30, 40, 20]);
            }
            engine.pause_wave_stream(1, true);
            let new_end = if prefix { 4 } else { 6 };
            assert_eq!(engine.break_wave_loop(1), Some((old_end, new_end)));
            assert_eq!(engine.break_wave_loop(1), Some((new_end, new_end)));
            assert_eq!(engine.wave_written_samples(1), new_end + 1);
            engine.pause_wave_stream(1, false);
            if prefix {
                let mut output = [0; 5];
                tap.drain_into(&mut output);
                assert_eq!(output, [10, 20, 30, 40, 50]);
            } else {
                let mut output = [0; 3];
                tap.drain_into(&mut output);
                assert_eq!(output, [30, 40, 50]);
            }
            assert_eq!(engine.break_wave_loop(1), None);
            assert_eq!(engine.break_wave_loop(99), None);
        }
    }

    #[test]
    fn android_short_pulls_expose_live_loop_cursor_and_updated_pcm() {
        let engine = AudioEngine::new();
        let tap = engine.tap();
        tap.drain_into(&mut []);
        engine.open_wave_stream(1, mono(22050));
        assert!(engine.queue_wave_loop(1, vec![0; 4096], u32::MAX));
        // Previous Android pull: the guest sees the same circular position.
        tap.drain_into(&mut [0; 4096]);
        assert_eq!(engine.wave_playback_cursor(1) % 4096, 0);
        let mut output = [0; 220];
        tap.drain_into(&mut output);
        assert_eq!(engine.wave_playback_cursor(1) % 4096, 220);
        // The emulator refreshes the ring after the guest fills the next region.
        let mut pcm = vec![0; 4096];
        pcm[220..440].fill(12000);
        engine.update_wave_loop(1, pcm);
        tap.drain_into(&mut output);
        assert_eq!(output, [12000; 220]);
        assert_eq!(engine.wave_playback_cursor(1) % 4096, 440);
    }

    #[test]
    fn wave_loop_preserves_queue_order_and_finishes_finite_repeats() {
        let e = AudioEngine::new();
        let tap = e.tap();
        tap.drain_into(&mut []);
        e.open_wave_stream(1, mono(44100));
        e.push_wave_samples(1, &[10]);
        assert!(e.queue_wave_loop(1, vec![20, 30], 2));
        e.push_wave_samples(1, &[40]);
        let mut out = [0; 6];
        tap.drain_into(&mut out);
        assert_eq!(out, [10, 20, 30, 20, 30, 40]);
        assert_eq!(e.wave_playback_cursor(1), 6);
        assert_eq!(e.wave_written_samples(1), 6);
        assert_eq!(e.buffered_samples(), 0);
    }

    #[test]
    fn wave_loop_refresh_preserves_position_and_pause_then_reset() {
        let e = AudioEngine::new();
        let tap = e.tap();
        tap.drain_into(&mut []);
        e.open_wave_stream(1, mono(44100));
        assert!(e.queue_wave_loop(1, vec![0; 4], u32::MAX));
        assert_eq!(e.buffered_samples(), 4); // never allocate 4 billion copies
        let mut out = [0; 2];
        tap.drain_into(&mut out);
        e.update_wave_loop(1, vec![100, 200, 300, 400]);
        tap.drain_into(&mut out);
        assert_eq!(out, [300, 400]);
        tap.drain_into(&mut out);
        assert_eq!(out, [100, 200]);
        assert_eq!(e.wave_playback_cursor(1), 6);
        e.pause_wave_stream(1, true);
        tap.drain_into(&mut out);
        assert_eq!(out, [0, 0]);
        assert_eq!(e.wave_playback_cursor(1), 6);
        e.pause_wave_stream(1, false);
        tap.drain_into(&mut out);
        assert_eq!(out, [300, 400]);
        e.reset_wave_stream(1);
        tap.drain_into(&mut out);
        assert_eq!(out, [0, 0]);
        assert_eq!(e.wave_playback_cursor(1), 0);
        assert_eq!(e.buffered_samples(), 0);
    }

    #[cfg(not(feature = "audio-cpal"))]
    #[test]
    fn suspended_output_preserves_music_sfx_samples_and_cursors() {
        let mut engine = AudioEngine::new();
        let tap = engine.tap();
        tap.drain_into(&mut []);
        let format = GuestFormat {
            sample_rate: 44100,
            channels: 1,
            bits_per_sample: 16,
        };
        engine.open_wave_stream(1, format);
        engine.push_wave_samples(1, &[100, 100]);
        engine.queue_mas_samples(2, format, &[1000, 2000]);
        let mut output = [0; 1];
        tap.drain_into(&mut output);
        assert_eq!(output, [1100]);
        engine.suspend_output();
        assert_eq!(engine.wave_written_samples(1), 2);
        assert_eq!(engine.mas_written_samples(2), 2);
        assert_eq!(engine.wave_playback_cursor(1), 1);
        assert_eq!(engine.mas_playback_cursor(2), 1);
        engine.resume_output();
        tap.drain_into(&mut output);
        assert_eq!(output, [2100]);
    }

    #[test]
    fn mas_appends_buffers_and_survives_wave_reset_close() {
        let e = AudioEngine::new();
        let tap = e.tap();
        tap.drain_into(&mut []);
        let format = GuestFormat {
            sample_rate: 44100,
            channels: 1,
            bits_per_sample: 16,
        };
        e.open_wave_stream(1, format);
        assert_eq!(e.queue_mas_samples(2, format, &[1000, 2000]), 2);
        assert_eq!(e.queue_mas_samples(2, format, &[3000, 4000]), 4);
        e.push_wave_samples(1, &[100; 4]);
        let mut out = [0; 1];
        tap.drain_into(&mut out);
        assert_eq!(out, [1100]);
        e.reset_wave_stream(1);
        e.close_wave_stream(1);
        e.flush_wave_out();
        tap.drain_into(&mut out);
        assert_eq!(out, [2000]);
        e.pause_mas_stream(2, true);
        tap.drain_into(&mut out);
        assert_eq!(out, [0]);
        assert_eq!(e.mas_playback_cursor(2), 2);
        e.pause_mas_stream(2, false);
        tap.drain_into(&mut out);
        assert_eq!(out, [3000]);
        e.stop_mas_stream(2);
        assert_eq!(e.buffered_samples(), 0);
    }

    fn mono(rate: u32) -> GuestFormat {
        GuestFormat {
            sample_rate: rate,
            channels: 1,
            bits_per_sample: 16,
        }
    }

    #[test]
    fn wave_handles_mix_and_reset_only_the_requested_stream() {
        let e = AudioEngine::new();
        let tap = e.tap();
        tap.drain_into(&mut []); // deterministic device clock, no wall time
        e.open_wave_stream(1, mono(44100));
        e.open_wave_stream(2, mono(44100));
        e.push_wave_samples(1, &[1000; 8]);
        e.push_wave_samples(2, &[2000; 8]);
        let mut out = [0; 2];
        tap.drain_into(&mut out);
        assert_eq!(out, [3000; 2]);
        assert_eq!(e.wave_playback_cursor(1), 2);
        assert_eq!(e.wave_playback_cursor(2), 2);
        e.reset_wave_stream(1);
        tap.drain_into(&mut out);
        assert_eq!(out, [2000; 2]);
        assert_eq!(e.wave_playback_cursor(1), 0);
        assert_eq!(e.wave_written_samples(1), 0);
        assert_eq!(e.wave_playback_cursor(2), 4);
        e.push_wave_samples(1, &[4000; 2]);
        tap.drain_into(&mut out);
        assert_eq!(out, [6000; 2]);
        e.close_wave_stream(1);
        tap.drain_into(&mut out);
        assert_eq!(out, [2000; 2]);
    }

    #[test]
    fn wave_pause_and_formats_are_independent_on_android_tap() {
        let e = AudioEngine::new();
        let tap = e.tap();
        tap.drain_into(&mut []);
        e.open_wave_stream(1, mono(44100));
        e.open_wave_stream(2, mono(22050));
        e.push_wave_samples(1, &[1000; 8]);
        e.push_wave_samples(2, &[2000, 3000, 4000, 5000]);
        e.pause_wave_stream(1, true);
        let mut out = [0; 4];
        tap.drain_into(&mut out);
        assert_eq!(out, [2000, 2000, 3000, 3000]);
        assert_eq!(e.wave_playback_cursor(1), 0);
        assert_eq!(e.wave_playback_cursor(2), 2);
        assert_eq!(tap.guest_format(), mono(44100));
        e.pause_wave_stream(1, false);
        tap.drain_into(&mut out);
        assert_eq!(out, [5000, 5000, 6000, 6000]);
        assert_eq!(e.wave_playback_cursor(1), 4);
    }

    #[test]
    fn wave_keeps_entire_music_and_peek_does_not_consume_it() {
        let e = AudioEngine::new();
        let tap = e.tap();
        tap.drain_into(&mut []);
        e.open_wave_stream(1, mono(44100));
        e.push_wave_samples(1, &vec![1234; RING_CAPACITY_SAMPLES + 8]);
        assert_eq!(e.buffered_samples(), RING_CAPACITY_SAMPLES + 8);
        let mut out = [0; 4];
        tap.peek_into(&mut out);
        assert_eq!(out, [1234; 4]);
        assert_eq!(e.wave_playback_cursor(1), 0);
        tap.drain_into(&mut out);
        assert_eq!(e.wave_playback_cursor(1), 4);
    }

    #[test]
    fn wave_headless_clock_respects_pause_and_clamps_to_submission() {
        let mut stream = WaveStream::new(mono(1000));
        stream.samples.extend([42; 10]);
        stream.written = 10;
        stream.tick = Instant::now() - std::time::Duration::from_secs(1);
        stream.paused = true;
        stream.advance_virtual(false);
        assert_eq!(stream.consumed, 0);
        stream.paused = false;
        stream.tick = Instant::now() - std::time::Duration::from_secs(1);
        stream.advance_virtual(false);
        assert_eq!(stream.consumed, 10);
        assert!(stream.samples.is_empty());
    }

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
