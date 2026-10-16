//! Reproduce live waveOut mixer starvation with actual ARM CPU-only code.
#![cfg(feature = "unicorn")]

use pocket_core::{
    cpu::{regs::ArmReg, unicorn::UnicornCpu, Cpu},
    kernel::{self, Dispatcher, GuestThread, KernelError, KernelState, Process, Thunk},
    pe::{self, ImportBinding, ImportSymbol, LoadedImage, LoadedSection},
    winceapi::WinCeDispatcher,
};
use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

struct ApiOnlyDispatcher(WinCeDispatcher);
impl Dispatcher for ApiOnlyDispatcher {
    fn dispatch(
        &mut self,
        cpu: &mut dyn Cpu,
        thunk: &Thunk,
        kernel: &mut KernelState,
    ) -> Result<kernel::DispatchOutcome, KernelError> {
        self.0.dispatch(cpu, thunk, kernel)
    }
}

fn run_mixer(dispatcher: &mut dyn Dispatcher, thumb: bool) -> [i16; 4] {
    const DATA: u32 = 0x0050_0000;
    const HANDLE: u32 = 0xdead4100;
    let mut text = vec![0; 0x1000];
    // Main sets two sentinels and flags, then unloads indefinitely without APIs.
    for (index, instruction) in [0xe3a05055u32, 0xe3a06066, 0xe1550006, 0xeafffffe]
        .into_iter()
        .enumerate()
    {
        text[index * 4..index * 4 + 4].copy_from_slice(&instruction.to_le_bytes());
    }
    if thumb {
        for (index, instruction) in [0x2555u16, 0x2666, 0x42b5, 0xe7fe].into_iter().enumerate() {
            text[index * 2..index * 2 + 2].copy_from_slice(&instruction.to_le_bytes());
        }
    }
    let image = LoadedImage {
        source_path: "audio_cpu_slice_regression".into(),
        machine: if thumb {
            pe::machine::THUMB
        } else {
            pe::machine::ARM
        },
        subsystem: pe::subsystem::WINDOWS_CE_GUI,
        image_base: 0x10000,
        size_of_image: 0x2000,
        stack_reserve: 0x10000,
        entry_point: 0x1000,
        sections: vec![LoadedSection {
            name: ".text".into(),
            virtual_address: 0x1000,
            virtual_size: 0x1000,
            characteristics: 0xe0000020,
            data: text,
        }],
        imports: vec![ImportSymbol {
            dll: "coredll.dll".into(),
            binding: ImportBinding::Name("Sleep".into()),
            iat_va: 0x11800,
        }],
        exports: Default::default(),
        resources: vec![],
        managed_runtime: None,
    };
    let mut cpu = UnicornCpu::new().unwrap();
    let mut process = Process::map_into(image, &mut cpu, &|_, _| None, dispatcher).unwrap();
    let sleep = process.thunks[0].thunk_va;
    // Mixer clears the ring, publishes it through Sleep(1), then idles.
    let code = [
        0xe59f1018u32, // ldr r1, [pc, #24]
        0xe3a00000,    // mov r0, #0
        0xe5810000,    // str r0, [r1]
        0xe5810004,    // str r0, [r1, #4]
        0xe59fc00c,    // ldr ip, [pc, #12]
        0xe3a00001,    // mov r0, #1
        0xe12fff3c,    // blx ip
        0xeafffffc,    // b back to mov r0, #1 / Sleep
        DATA,
        sleep,
    ];
    let bytes: Vec<_> = code.into_iter().flat_map(u32::to_le_bytes).collect();
    cpu.write_mem(0x11100, &bytes).unwrap();
    cpu.write_mem(DATA, &[0xe8, 3, 0xe8, 3, 0xe8, 3, 0xe8, 3])
        .unwrap();
    let format = kernel::audio::GuestFormat {
        sample_rate: 48000,
        channels: 1,
        bits_per_sample: 16,
    };
    process.state.audio.open_wave_stream(HANDLE, format);
    assert!(process
        .state
        .audio
        .queue_wave_loop(HANDLE, vec![1000; 4], u32::MAX));
    process.state.wave_out.devices.insert(
        HANDLE,
        kernel::WaveOutDevice {
            format,
            callback_kind: kernel::WaveCallbackKind::None,
            callback_target: 0,
            instance: 0,
            owner_thread: 0,
            paused: false,
        },
    );
    process.state.wave_out.live_loops.insert(
        HANDLE,
        kernel::LiveWaveLoop {
            hdr: DATA + 0x100,
            data: DATA,
            bytes: 8,
        },
    );
    let mut worker = GuestThread::new(
        0x11100,
        0,
        process.stack_top - 0x1000,
        0x1000,
        0,
        0,
        0xdead7c00,
        [0; 17],
    );
    worker.started = true;
    worker.worker_saved = true;
    worker.worker_regs[13] = worker.stack_top;
    worker.worker_regs[15] = worker.entry;
    worker.worker_regs[16] = 0x10;
    process.state.threads.push(worker);
    let tap = process.state.audio.tap();
    let mut out = [0; 4];
    tap.drain_into(&mut out);
    assert_eq!(out, [1000; 4]);
    kernel::run_main_loop(&mut cpu, &mut process, dispatcher, 100, 2).unwrap();
    assert_eq!(process.state.current_thread, 0);
    assert_eq!(cpu.read_reg(ArmReg::R5).unwrap(), 0x55);
    assert_eq!(cpu.read_reg(ArmReg::R6).unwrap(), 0x66);
    assert_eq!(
        cpu.read_reg(ArmReg::Pc).unwrap(),
        if thumb { 0x11006 } else { 0x1100c }
    );
    assert_eq!(
        cpu.read_reg(ArmReg::Cpsr).unwrap() & 0x20,
        if thumb { 0x20 } else { 0 }
    );
    assert_eq!(cpu.read_reg(ArmReg::Cpsr).unwrap() & 0xf0000000, 0x80000000);
    tap.drain_into(&mut out);
    out
}

#[test]
fn native_arm_audio_mixer_runs_during_main_cpu_only_unload() {
    // The old API-only policy leaves the last buffer playing indefinitely.
    assert_eq!(
        run_mixer(&mut ApiOnlyDispatcher(WinCeDispatcher::new()), false),
        [1000; 4]
    );
    // Slice scheduling lets the mixer publish silence, preserving main state.
    assert_eq!(run_mixer(&mut WinCeDispatcher::new(), false), [0; 4]);
}

#[test]
fn native_thumb_audio_mixer_preserves_main_mode_and_flags() {
    assert_eq!(run_mixer(&mut WinCeDispatcher::new(), true), [0; 4]);
}

static SLOW_READS: AtomicU32 = AtomicU32::new(0);
static READS_BEFORE_MIXER: AtomicU32 = AtomicU32::new(u32::MAX);
const IO_DATA: u32 = 0x00500000;

fn slow_read(
    ctx: &mut pocket_core::winceapi::CallCtx<'_>,
) -> Result<kernel::DispatchOutcome, KernelError> {
    assert_eq!(ctx.arg_u32(0)?, 0x55); // replay must preserve the pending argument
    std::thread::sleep(Duration::from_millis(30));
    SLOW_READS.fetch_add(1, Ordering::SeqCst);
    Ok(kernel::DispatchOutcome::ReturnedR0(0))
}
fn mixer_turn(
    ctx: &mut pocket_core::winceapi::CallCtx<'_>,
) -> Result<kernel::DispatchOutcome, KernelError> {
    READS_BEFORE_MIXER.store(SLOW_READS.load(Ordering::SeqCst), Ordering::SeqCst);
    ctx.cpu.write_mem(IO_DATA, &[0; 8])?;
    Ok(kernel::DispatchOutcome::ReturnedR0(0))
}

/// Reproduce R1's main-only preemption policy without changing the handlers.
struct MainOnlyDispatcher(WinCeDispatcher);
impl Dispatcher for MainOnlyDispatcher {
    fn dispatch(
        &mut self,
        cpu: &mut dyn Cpu,
        thunk: &Thunk,
        state: &mut KernelState,
    ) -> Result<kernel::DispatchOutcome, KernelError> {
        let worker = state.current_thread != 0;
        let deadline = state.worker_preempt_after_ms;
        if worker {
            state.worker_preempt_after_ms = u64::MAX;
        }
        let result = self.0.dispatch(cpu, thunk, state);
        if worker && state.current_thread != 0 {
            state.worker_preempt_after_ms = deadline;
        }
        result
    }
    fn schedule_cpu_slice(
        &mut self,
        cpu: &mut dyn Cpu,
        state: &mut KernelState,
    ) -> Result<Option<u32>, KernelError> {
        if state.current_thread == 0 {
            self.0.schedule_cpu_slice(cpu, state)
        } else {
            Ok(None)
        }
    }
}

fn run_io_worker(dispatcher: &mut dyn Dispatcher) -> u32 {
    SLOW_READS.store(0, Ordering::SeqCst);
    READS_BEFORE_MIXER.store(u32::MAX, Ordering::SeqCst);
    let mut text = vec![0; 0x1000];
    for (i, word) in [0xe3a05055u32, 0xe3a06066, 0xe1550006, 0xeafffffe]
        .into_iter()
        .enumerate()
    {
        text[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    let image = LoadedImage {
        source_path: "slow_resource_worker".into(),
        machine: pe::machine::ARM,
        subsystem: pe::subsystem::WINDOWS_CE_GUI,
        image_base: 0x10000,
        size_of_image: 0x2000,
        stack_reserve: 0x10000,
        entry_point: 0x1000,
        sections: vec![LoadedSection {
            name: ".text".into(),
            virtual_address: 0x1000,
            virtual_size: 0x1000,
            characteristics: 0xe0000020,
            data: text,
        }],
        imports: ["TestSlowRead", "TestMixerTurn", "Sleep"]
            .into_iter()
            .enumerate()
            .map(|(i, name)| ImportSymbol {
                dll: "coredll.dll".into(),
                binding: ImportBinding::Name(name.into()),
                iat_va: 0x11800 + i as u32 * 4,
            })
            .collect(),
        exports: Default::default(),
        resources: vec![],
        managed_runtime: None,
    };
    let mut cpu = UnicornCpu::new().unwrap();
    let mut process = Process::map_into(image, &mut cpu, &|_, _| None, dispatcher).unwrap();
    let slow = process.thunks[0].thunk_va;
    let mix = process.thunks[1].thunk_va;
    let sleep = process.thunks[2].thunk_va;
    for (entry, words) in [
        (
            0x11100,
            vec![
                0xe3a04040u32,
                0xe3a00055,
                0xe59fc01c,
                0xe12fff3c,
                0xe2544001,
                0x1afffffa,
                0xe3e00000,
                0xe59fc00c,
                0xe12fff3c,
                0xeafffffe,
                0,
                slow,
                sleep,
            ],
        ),
        (
            0x11200,
            vec![
                0xe59fc010u32,
                0xe12fff3c,
                0xe3e00000,
                0xe59fc008,
                0xe12fff3c,
                0xeafffffe,
                mix,
                sleep,
            ],
        ),
    ] {
        cpu.write_mem(
            entry,
            &words
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let mut thread = GuestThread::new(
            entry,
            0,
            process.stack_top - 0x1000,
            0x1000,
            0,
            0,
            entry,
            [0; 17],
        );
        thread.started = true;
        thread.worker_saved = true;
        thread.worker_regs[13] = thread.stack_top;
        thread.worker_regs[15] = entry;
        // Match CreateThread: workers inherit the main processor mode.
        thread.worker_regs[16] = cpu.read_reg(ArmReg::Cpsr).unwrap();
        process.state.threads.push(thread);
    }
    cpu.write_mem(IO_DATA, &[0xe8, 3, 0xe8, 3, 0xe8, 3, 0xe8, 3])
        .unwrap();
    let format = kernel::audio::GuestFormat {
        sample_rate: 48000,
        channels: 1,
        bits_per_sample: 16,
    };
    let handle = 0xdead4100;
    process.state.audio.open_wave_stream(handle, format);
    assert!(process
        .state
        .audio
        .queue_wave_loop(handle, vec![1000; 4], u32::MAX));
    process.state.wave_out.devices.insert(
        handle,
        kernel::WaveOutDevice {
            format,
            callback_kind: kernel::WaveCallbackKind::None,
            callback_target: 0,
            instance: 0,
            owner_thread: 0,
            paused: false,
        },
    );
    process.state.wave_out.live_loops.insert(
        handle,
        kernel::LiveWaveLoop {
            hdr: IO_DATA + 0x100,
            data: IO_DATA,
            bytes: 8,
        },
    );
    let tap = process.state.audio.tap();
    tap.drain_into(&mut []);
    let mut hook = |state: &mut KernelState| {
        if state.current_thread == 0
            && state
                .threads
                .iter()
                .all(|t| t.worker_saved && t.sleep_until_ms == u64::MAX)
        {
            kernel::FrameAction::Stop
        } else {
            kernel::FrameAction::Continue
        }
    };
    let result = kernel::run_main_loop_with_hook(
        &mut cpu,
        &mut process,
        dispatcher,
        100,
        10_000,
        Some(&mut hook),
    );
    assert!(
        result.is_ok(),
        "{result:?}; reads={} mixer_after={} current={} regs={} workers={:?}",
        SLOW_READS.load(Ordering::SeqCst),
        READS_BEFORE_MIXER.load(Ordering::SeqCst),
        process.state.current_thread,
        pocket_core::cpu::dump_regs(&mut cpu),
        process
            .state
            .threads
            .iter()
            .map(|t| (
                t.saved_regs[15],
                t.worker_regs[15],
                t.worker_saved,
                t.sleep_until_ms
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(SLOW_READS.load(Ordering::SeqCst), 64); // no skipped/repeated I/O
    assert_eq!(process.state.current_thread, 0);
    assert_eq!(cpu.read_reg(ArmReg::R5).unwrap(), 0x55);
    assert_eq!(cpu.read_reg(ArmReg::R6).unwrap(), 0x66);
    assert_eq!(cpu.read_reg(ArmReg::Cpsr).unwrap() & 0xf0000000, 0x80000000);
    let mut pcm = [1000; 4];
    tap.drain_into(&mut pcm);
    assert_eq!(pcm, [0; 4]);
    READS_BEFORE_MIXER.load(Ordering::SeqCst)
}

#[test]
fn native_arm_slow_io_worker_cannot_starve_the_live_audio_mixer() {
    let make = || {
        let mut d = WinCeDispatcher::new();
        d.register_handler("coredll.dll", "TestSlowRead", slow_read);
        d.register_handler("coredll.dll", "TestMixerTurn", mixer_turn);
        d
    };
    assert_eq!(run_io_worker(&mut MainOnlyDispatcher(make())), 64);
    let reads = run_io_worker(&mut make());
    println!("Mixer turn: main-only policy after 64 slow calls; fixed policy after {reads}");
    assert!(reads <= 3, "mixer waited for {reads} slow host calls");
}
