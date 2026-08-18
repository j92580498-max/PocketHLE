//! Run-loop profiler, off unless `POCKETHLE_PROFILE` is set.
//!
//! A software-rendered Pocket PC title spends its frame in three very
//! different places — inside the emulated CPU, inside our API
//! handlers, and inside the per-slice presentation work — and which
//! one dominates is not guessable. Zuma turned out to issue roughly
//! eight thousand API calls per rendered frame, so a cost that looks
//! negligible per call decides the frame rate. There is no `perf` on
//! every machine this gets debugged on, and a sampling profiler cannot
//! attribute time to a *guest* API anyway, so the run loop measures
//! itself.
//!
//! Enable with `POCKETHLE_PROFILE=1`; the summary goes to stderr when
//! the run ends. When the variable is absent every method below is a
//! branch on a cached `bool`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Wall-clock cost of one phase of the run loop, plus how often it ran.
#[derive(Default, Clone, Copy)]
struct Phase {
    calls: u64,
    total: Duration,
}

impl Phase {
    fn add(&mut self, dt: Duration) {
        self.calls += 1;
        self.total += dt;
    }
}

/// Per-import accounting, keyed by `thunk_va` — the identity of an
/// import (see the dispatch section of `docs/AGENTS.md`). The label is
/// only formatted once, because `Thunk::label` allocates.
struct ThunkStat {
    label: String,
    calls: u64,
    total: Duration,
}

pub struct Profiler {
    on: bool,
    start: Instant,
    cpu: Phase,
    dispatch: Phase,
    fb_sync: Phase,
    controls: Phase,
    hook: Phase,
    tick: Phase,
    slices: u64,
    /// Frame-to-frame gaps in microseconds, and the counter/instant of
    /// the last frame seen; see [`Profiler::note_present`].
    frame_gaps: Vec<u32>,
    last_present: Option<Instant>,
    last_present_counter: u64,
    /// Latest `frame_counter`, refreshed per slice so the report is
    /// correct however the run loop returns — a game can leave it from
    /// any of a dozen places, and every one of them wants the summary.
    frames: u64,
    thunks: HashMap<u32, ThunkStat>,
}

impl Profiler {
    /// A profiler that measures only when `POCKETHLE_PROFILE` is set.
    pub fn from_env() -> Self {
        let on = std::env::var_os("POCKETHLE_PROFILE").is_some_and(|v| v != "0");
        Self {
            on,
            start: Instant::now(),
            cpu: Phase::default(),
            dispatch: Phase::default(),
            fb_sync: Phase::default(),
            controls: Phase::default(),
            hook: Phase::default(),
            tick: Phase::default(),
            slices: 0,
            frame_gaps: Vec::new(),
            last_present: None,
            last_present_counter: 0,
            frames: 0,
            thunks: HashMap::new(),
        }
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        self.on
    }

    /// Start timing a phase. Returns `None` — and so costs nothing —
    /// when profiling is off.
    #[inline]
    pub fn mark(&self) -> Option<Instant> {
        if self.on {
            Some(Instant::now())
        } else {
            None
        }
    }

    #[inline]
    pub fn count_slice(&mut self) {
        self.slices += 1;
    }

    /// Record that the presented pixels have changed, so the report
    /// can describe the *shape* of the frame pacing and not just its
    /// mean.
    ///
    /// Averaging a whole run is actively misleading for a Pocket PC
    /// game: startup decodes assets, the menu paints once and idles,
    /// and a level load blocks on file I/O. Zuma's mean over a 25 s
    /// run reads 22 fps while its steady state is a firm 30 ms frame
    /// grid — the average describes a game nobody is playing. The
    /// median and the tail are what a player feels, so report those.
    #[inline]
    pub fn note_present(&mut self, counter: u64) {
        if !self.on || counter == self.last_present_counter {
            return;
        }
        self.last_present_counter = counter;
        let now = Instant::now();
        if let Some(previous) = self.last_present.replace(now) {
            // Bounded so a long-running session cannot grow this
            // without limit; 1 M gaps is over five hours at 60 fps.
            if self.frame_gaps.len() < 1_000_000 {
                let gap = now.duration_since(previous).as_micros();
                self.frame_gaps.push(gap.min(u32::MAX as u128) as u32);
            }
        }
    }

    #[inline]
    pub fn note_frames(&mut self, frames: u64) {
        self.frames = frames;
    }

    #[inline]
    pub fn add_cpu(&mut self, mark: Option<Instant>) {
        if let Some(t) = mark {
            self.cpu.add(t.elapsed());
        }
    }

    #[inline]
    pub fn add_fb_sync(&mut self, mark: Option<Instant>) {
        if let Some(t) = mark {
            self.fb_sync.add(t.elapsed());
        }
    }

    #[inline]
    pub fn add_controls(&mut self, mark: Option<Instant>) {
        if let Some(t) = mark {
            self.controls.add(t.elapsed());
        }
    }

    #[inline]
    pub fn add_hook(&mut self, mark: Option<Instant>) {
        if let Some(t) = mark {
            self.hook.add(t.elapsed());
        }
    }

    #[inline]
    pub fn add_tick(&mut self, mark: Option<Instant>) {
        if let Some(t) = mark {
            self.tick.add(t.elapsed());
        }
    }

    /// Attribute one dispatched API call. `label` is only called for a
    /// `thunk_va` we have not seen before.
    #[inline]
    pub fn add_dispatch(
        &mut self,
        mark: Option<Instant>,
        thunk_va: u32,
        label: impl FnOnce() -> String,
    ) {
        let Some(t) = mark else { return };
        let dt = t.elapsed();
        self.dispatch.add(dt);
        let entry = self.thunks.entry(thunk_va).or_insert_with(|| ThunkStat {
            label: label(),
            calls: 0,
            total: Duration::ZERO,
        });
        entry.calls += 1;
        entry.total += dt;
    }

    /// Describe the frame pacing: how long a frame actually takes,
    /// how consistent that is, and how many frames missed badly.
    ///
    /// The warm-up frames are dropped rather than averaged in. What we
    /// want to know is whether the emulator holds a cadence once the
    /// game is running, and the first second of any Pocket PC title is
    /// asset decoding that will never happen again.
    fn report_pacing(&self, secs: f64) {
        /// Frames skipped before the pacing statistics start. Zuma
        /// spends its opening frames decoding assets and painting a
        /// menu that idles; including them says nothing about how the
        /// game plays.
        const WARMUP_FRAMES: usize = 60;
        if self.frame_gaps.len() <= WARMUP_FRAMES + 8 {
            eprintln!(
                "frame pacing: {} frames measured, too few to characterise",
                self.frame_gaps.len()
            );
            return;
        }
        let mut gaps: Vec<u32> = self.frame_gaps[WARMUP_FRAMES..].to_vec();
        let counted = gaps.len();
        let sum: u64 = gaps.iter().map(|g| u64::from(*g)).sum();
        let mean_us = sum as f64 / counted as f64;
        gaps.sort_unstable();
        let at = |q: f64| {
            let idx = ((counted - 1) as f64 * q).round() as usize;
            f64::from(gaps[idx]) / 1000.0
        };
        // A frame that takes longer than this is a visible hitch
        // rather than a slow frame: two missed refreshes on a 60 Hz
        // panel, which is where a stutter stops being deniable.
        const HITCH_MS: f64 = 40.0;
        let hitches = gaps
            .iter()
            .filter(|g| f64::from(**g) / 1000.0 > HITCH_MS)
            .count();
        eprintln!(
            "frame pacing (after {WARMUP_FRAMES} warm-up frames, {counted} frames of {secs:.1}s):"
        );
        eprintln!(
            "  mean {:.1} ms ({:.1} fps)  median {:.1} ms  p90 {:.1} ms  p99 {:.1} ms  max {:.1} ms",
            mean_us / 1000.0,
            1_000_000.0 / mean_us,
            at(0.50),
            at(0.90),
            at(0.99),
            f64::from(gaps[counted - 1]) / 1000.0,
        );
        eprintln!(
            "  {hitches} frames over {HITCH_MS:.0} ms ({:.1}%)",
            hitches as f64 / counted as f64 * 100.0,
        );
    }

    /// Write the summary to stderr. `frames` is the framebuffer's
    /// `frame_counter`, i.e. how many times the presented pixels
    /// actually changed.
    fn report(&self, frames: u64) {
        if !self.on {
            return;
        }
        let wall = self.start.elapsed();
        let secs = wall.as_secs_f64().max(1e-9);
        let pct = |d: Duration| d.as_secs_f64() / secs * 100.0;
        let accounted = self.cpu.total
            + self.dispatch.total
            + self.fb_sync.total
            + self.controls.total
            + self.hook.total
            + self.tick.total;
        eprintln!("\n=== PocketHLE profile ===");
        eprintln!(
            "wall {:.3}s  slices {}  frames {}  {:.1} fps  {:.0} slices/frame",
            secs,
            self.slices,
            frames,
            frames as f64 / secs,
            self.slices as f64 / (frames.max(1) as f64),
        );
        self.report_pacing(secs);
        let row = |name: &str, p: &Phase| {
            eprintln!(
                "  {name:<14} {:>8.3}s {:>5.1}%  calls {:>10}  {:>8.0} ns/call",
                p.total.as_secs_f64(),
                pct(p.total),
                p.calls,
                if p.calls == 0 {
                    0.0
                } else {
                    p.total.as_nanos() as f64 / p.calls as f64
                },
            );
        };
        row("guest cpu", &self.cpu);
        row("api dispatch", &self.dispatch);
        row("fb sync", &self.fb_sync);
        row("controls", &self.controls);
        row("frame hook", &self.hook);
        row("tick page", &self.tick);
        eprintln!(
            "  {:<14} {:>8.3}s {:>5.1}%  (loader, logging, run-loop bookkeeping)",
            "unaccounted",
            (wall.saturating_sub(accounted)).as_secs_f64(),
            pct(wall.saturating_sub(accounted)),
        );
        let mut top: Vec<&ThunkStat> = self.thunks.values().collect();
        top.sort_by_key(|s| std::cmp::Reverse(s.total));
        eprintln!("top imports by host time:");
        for s in top.iter().take(20) {
            eprintln!(
                "  {:>8.3}s {:>5.1}%  {:>9} calls  {:>8.0} ns  {}",
                s.total.as_secs_f64(),
                pct(s.total),
                s.calls,
                s.total.as_nanos() as f64 / s.calls.max(1) as f64,
                s.label,
            );
        }
        let mut chatty: Vec<&ThunkStat> = self.thunks.values().collect();
        chatty.sort_by_key(|s| std::cmp::Reverse(s.calls));
        eprintln!("top imports by call count:");
        for s in chatty.iter().take(20) {
            eprintln!(
                "  {:>9} calls  {:>7.0} per frame  {:>8.0} ns  {}",
                s.calls,
                s.calls as f64 / frames.max(1) as f64,
                s.total.as_nanos() as f64 / s.calls.max(1) as f64,
                s.label,
            );
        }
    }
}

impl Drop for Profiler {
    fn drop(&mut self) {
        self.report(self.frames);
    }
}
