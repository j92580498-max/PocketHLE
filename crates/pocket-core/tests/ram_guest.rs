//! Runs the shipped diagnostic as actual guest ARM, through the public APIs.
#![cfg(feature = "unicorn")]
use pocket_core::{
    kernel::{memory_division::MemoryDivision, FrameAction, InputEvent, KernelState},
    Emulator,
};
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
// These diagnostics contain real-time 5s/15s guest waits and each launches
// concurrent emulators. Running five whole suites at once on small CI runners
// can time out PROCTEST although it finishes successfully immediately after.
// Serialize suites, retaining all concurrency *within* each guest diagnostic.
static DIAGNOSTIC_SUITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn diagnostic_suite() -> std::sync::MutexGuard<'static, ()> {
    DIAGNOSTIC_SUITE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct GuestFiles(PathBuf);
impl Drop for GuestFiles {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn native_arm_guest_verifies_ram_division_stacks_and_dll_lifetimes() {
    run_guest(include_bytes!("fixtures/ramtest/GZRT999999/AUTORUN.EXE"), 0);
}
#[test]
fn explicit_exitprocess_runs_detach_before_unmapping() {
    run_guest(
        include_bytes!("fixtures/ramtest/fixtures/explicit-exit.exe"),
        77,
    );
}
#[test]
fn main_exitthread_then_last_worker_exit_keeps_callback_stacks_alive() {
    run_guest(
        include_bytes!("fixtures/ramtest/fixtures/last-worker-exit.exe"),
        33,
    );
}
fn run_guest(executable: &[u8], expected_code: u32) {
    let _suite = diagnostic_suite();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let files = GuestFiles(std::env::temp_dir().join(format!(
        "pockethle-ram-guest-{}-{nonce}",
        std::process::id()
    )));
    fs::create_dir_all(&files.0).unwrap();
    let flash = files.0.join("flash");
    fs::create_dir_all(&flash).unwrap();
    for (name, bytes) in [
        ("AUTORUN.EXE", executable),
        (
            "proctest.exe",
            include_bytes!("fixtures/ramtest/GZRT999999/proctest.exe").as_slice(),
        ),
        (
            "procworker.exe",
            include_bytes!("fixtures/ramtest/GZRT999999/procworker.exe").as_slice(),
        ),
        (
            "badproc.exe",
            include_bytes!("fixtures/ramtest/GZRT999999/badproc.exe").as_slice(),
        ),
        (
            "pageprobe.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/pageprobe.dll").as_slice(),
        ),
        (
            "ramprobe.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/ramprobe.dll").as_slice(),
        ),
        (
            "ramprobe2.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/ramprobe2.dll").as_slice(),
        ),
        (
            "ramreject.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/ramreject.dll").as_slice(),
        ),
    ] {
        fs::write(files.0.join(name), bytes).unwrap();
    }
    for (name, bytes) in [
        (
            "depleaf.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/depleaf.dll").as_slice(),
        ),
        (
            "deproot.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/deproot.dll").as_slice(),
        ),
        (
            "deppeer.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/deppeer.dll").as_slice(),
        ),
        (
            "depreject.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/depreject.dll").as_slice(),
        ),
        (
            "depmissing.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/depmissing.dll").as_slice(),
        ),
        (
            "depbadexport.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/depbadexport.dll").as_slice(),
        ),
        (
            "depcyclea.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/depcyclea.dll").as_slice(),
        ),
        (
            "depcycleb.dll",
            include_bytes!("fixtures/ramtest/GZRT999999/depcycleb.dll").as_slice(),
        ),
    ] {
        fs::write(files.0.join(name), bytes).unwrap();
    }
    let mut emu = Emulator::with_unicorn_cpu().unwrap();
    emu.max_slices = 3_000_000;
    emu.set_halt_on_unimplemented(true);
    emu.load_pe(files.0.join("AUTORUN.EXE")).unwrap();
    let ram = MemoryDivision::gizmondo_sdk_default();
    assert!(emu.set_memory_division(Some(ram.clone())));
    let initial = ram.snapshot();
    {
        let state = &mut emu.process_mut().unwrap().state;
        state
            .vfs
            .mount_read_only("\\SD Card\\GZRT999999\\", &files.0);
        state.vfs.mount_save_dir("\\Flash Disk\\", &flash);
        state.module_path = "\\SD Card\\GZRT999999\\AUTORUN.EXE".into();
        state.synthetic_message_budget = 0;
        state.process_launch_enabled = true;
    }
    let mut children = Vec::new();
    let mut acknowledge = |state: &mut KernelState| {
        launch_pending(state, &files.0, &flash, &mut children);
        if state.modal.is_some() {
            state
                .pending_input
                .push_back(InputEvent::KeyDown { vk: 0x0d });
        }
        FrameAction::Continue
    };
    emu.run_with_hook(&mut acknowledge).unwrap();
    drop(acknowledge);
    for child in children {
        child.join().unwrap();
    }
    let entries: Vec<_> = fs::read_dir(&flash)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let report_path = entries
        .iter()
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("RAMTEST.TXT")
        })
        .unwrap_or_else(|| {
            panic!(
                "no report; guest exit={:?}, directory={entries:?}",
                emu.process().unwrap().state.process_exit_code
            )
        });
    let report = fs::read_to_string(report_path).unwrap();
    println!("{report}");
    // Print child reports before asserting so CI distinguishes a parent wait
    // timeout from a failed process-contract check in the child itself.
    for entry in &entries {
        if entry
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("PROCTEST.TXT"))
        {
            println!(
                "Child process report:\n{}",
                fs::read_to_string(entry).unwrap()
            );
        }
    }
    assert!(
        report.contains("RAMTEST_RESULT PASS checks=0x00000088 failures=0x00000000"),
        "{report}"
    );
    assert!(
        !report.lines().any(|line| line.starts_with("FAIL ")),
        "{report}"
    );
    let dll_report_path = fs::read_dir(&flash)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("DLLTEST.TXT")
        })
        .unwrap();
    let dll_report = fs::read_to_string(dll_report_path).unwrap();
    println!("{dll_report}");
    assert!(dll_report.contains("DLLTEST_RESULT PASS"), "{dll_report}");
    assert!(!dll_report.contains("DLLTEST_RESULT FAIL"), "{dll_report}");
    let dep_path = fs::read_dir(&flash)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("DEPTEST.TXT")
        })
        .unwrap();
    let dep_report = fs::read_to_string(dep_path).unwrap();
    println!("{dep_report}");
    assert_eq!(
        dep_report.replace("\r", ""),
        concat!(
            "leaf attach\nroot attach\npeer attach\nroot detach\npeer detach\nleaf detach\n",
            "leaf attach\nroot attach\nroot detach\nleaf detach\n",
            "leaf attach\nreject attach\nleaf detach\n",
            "cycle-b attach\ncycle-a attach\ncycle-a detach\ncycle-b detach\nDEPTEST_RESULT PASS\n"
        )
    );
    for (name, success) in [
        ("PROCTEST.TXT", "PROCTEST_RESULT PASS"),
        ("ORPHANTEST.TXT", "ORPHANTEST_RESULT PASS"),
    ] {
        let path = entries
            .iter()
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(name)
            })
            .unwrap();
        let report = fs::read_to_string(path).unwrap();
        println!("{report}");
        assert!(
            report.contains(success) && !report.contains("FAIL"),
            "{report}"
        );
    }
    assert_eq!(
        emu.process().unwrap().state.process_exit_code,
        Some(expected_code)
    );
    assert_eq!(
        ram.snapshot().store_pages,
        initial.store_pages,
        "test must restore the RAM partition"
    );
    assert!(emu.process().unwrap().state.modules.is_empty());
    assert!(emu.process().unwrap().state.thread_tls.is_empty());
    assert_eq!(emu.process().unwrap().state.tls_slots_used, 0);
    assert!(emu
        .process()
        .unwrap()
        .state
        .threads
        .iter()
        .all(|t| t.finished && t.stack_region.is_none()));
}

fn launch_pending(
    state: &mut KernelState,
    assets: &PathBuf,
    flash: &PathBuf,
    children: &mut Vec<std::thread::JoinHandle<()>>,
) {
    let Some(request) = state.pending_process_launch.take() else {
        return;
    };
    assert!(request.concurrent);
    let context = state.child_handle_context(request.process_handle).unwrap();
    let table = context.table.clone();
    let child_table = table.clone();
    child_table.defer_process_exit();
    let ram = state.memory_division.clone();
    let assets = assets.clone();
    let flash = flash.clone();
    let key = request.call_key;
    let handle = request.process_handle;
    let (tx, rx) = std::sync::mpsc::channel();
    children.push(std::thread::spawn(move || {
        let built = (|| -> anyhow::Result<Emulator> {
            let mut emu = Emulator::with_unicorn_cpu()?;
            emu.load_pe(&request.executable)?;
            emu.process_mut()
                .unwrap()
                .state
                .attach_handle_context(context);
            anyhow::ensure!(emu.set_memory_division(ram), "child RAM exhausted");
            emu.set_startup_command_line(&request.command_line)?;
            let state = &mut emu.process_mut().unwrap().state;
            let guest_dir = request.guest_path.rsplit_once('\\').unwrap().0;
            state
                .vfs
                .mount_read_only(&format!("{guest_dir}\\"), &assets);
            state.vfs.mount_save_dir("\\Flash Disk\\", &flash);
            state.module_path = request.guest_path;
            state.synthetic_message_budget = 0;
            state.process_launch_enabled = true;
            emu.max_slices = 3_000_000;
            emu.set_halt_on_unimplemented(true);
            Ok(emu)
        })();
        let mut emu = match built {
            Ok(emu) => {
                tx.send(Ok(())).unwrap();
                emu
            }
            Err(_error) => {
                tx.send(Err(8)).unwrap();
                child_table.complete_process_exit(0xc0000017);
                return;
            }
        };
        while !child_table.start_allowed() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut children = Vec::new();
        let mut hook = |state: &mut KernelState| {
            launch_pending(state, &assets, &flash, &mut children);
            FrameAction::Continue
        };
        emu.run_with_hook(&mut hook).unwrap();
        drop(hook);
        let code = emu
            .process()
            .unwrap()
            .state
            .process_exit_code
            .expect("child exited");
        drop(emu);
        child_table.complete_process_exit(code);
        for child in children {
            child.join().unwrap();
        }
        assert_eq!(
            child_table.exit_code(child_table.process_id(), None),
            Some(code)
        );
    }));
    state.process_launch_results.insert(
        key,
        rx.recv_timeout(std::time::Duration::from_secs(30))
            .unwrap()
            .map(|()| handle),
    );
}

#[test]
fn native_arm_vfs_diagnostic_runs_twice_and_restores_device_ram() {
    let _suite = diagnostic_suite();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let files = GuestFiles(std::env::temp_dir().join(format!("pockethle-vfs-{nonce}")));
    fs::create_dir_all(&files.0).unwrap();
    let flash = files.0.join("flash");
    fs::create_dir_all(&flash).unwrap();
    for (n, b) in [
        (
            "AUTORUN.EXE",
            include_bytes!("fixtures/vfstest/GZVT999998/AUTORUN.EXE").as_slice(),
        ),
        (
            "vfsworker.exe",
            include_bytes!("fixtures/vfstest/GZVT999998/vfsworker.exe").as_slice(),
        ),
        (
            "asset.bin",
            include_bytes!("fixtures/vfstest/GZVT999998/asset.bin").as_slice(),
        ),
    ] {
        fs::write(files.0.join(n), b).unwrap();
    }
    let ram = MemoryDivision::gizmondo_sdk_default();
    // The device registry persists between sessions; initialize it before measuring.
    {
        let mut init = Emulator::with_unicorn_cpu().unwrap();
        init.load_pe(files.0.join("AUTORUN.EXE")).unwrap();
        assert!(init.set_memory_division(Some(ram.clone())));
    }
    let baseline = ram.snapshot();
    for iteration in 0..2 {
        let mut emu = Emulator::with_unicorn_cpu().unwrap();
        emu.max_slices = 3_000_000;
        emu.set_halt_on_unimplemented(true);
        emu.load_pe(files.0.join("AUTORUN.EXE")).unwrap();
        assert!(emu.set_memory_division(Some(ram.clone())));
        let state = &mut emu.process_mut().unwrap().state;
        state
            .vfs
            .mount_read_only("\\SD Card\\GZVT999998\\", &files.0);
        state.vfs.mount_save_dir("\\Flash Disk\\", &flash);
        state.module_path = "\\SD Card\\GZVT999998\\AUTORUN.EXE".into();
        state.synthetic_message_budget = 0;
        state.process_launch_enabled = true;
        let mut children = Vec::new();
        let mut hook = |state: &mut KernelState| {
            launch_pending(state, &files.0, &flash, &mut children);
            if state.modal.is_some() {
                state
                    .pending_input
                    .push_back(InputEvent::KeyDown { vk: 0x0d });
            }
            FrameAction::Continue
        };
        emu.run_with_hook(&mut hook).unwrap();
        drop(hook);
        for child in children {
            child.join().unwrap();
        }
        let path = fs::read_dir(&flash)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("VFSTEST.TXT")
            })
            .unwrap();
        let report = fs::read_to_string(path).unwrap();
        println!("{report}");
        assert!(report.contains("VFSTEST_RESULT PASS"), "{report}");
        assert!(
            report.contains("PASS cycles.64_duplicate_flush_close_delete"),
            "{report}"
        );
        let marker_path = fs::read_dir(&flash)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("PocketHLE-VFSTEST-PERSIST-v3.bin")
            })
            .unwrap();
        let marker = fs::read(marker_path).unwrap();
        assert_eq!(&marker[..8], &[0x33, 0x46, 0x54, 0x56, 1, 0, 0, 0]);
        assert_eq!(
            u32::from_le_bytes(marker[8..12].try_into().unwrap()),
            iteration + 1
        );
        assert!(!report.lines().any(|l| l.starts_with("FAIL ")), "{report}");
        assert_eq!(emu.process().unwrap().state.process_exit_code, Some(0));
        drop(emu);
        assert_eq!(ram.snapshot().program_used, baseline.program_used);
        assert_eq!(ram.snapshot().store_used, baseline.store_used);
        assert_eq!(
            fs::read_dir(&flash).unwrap().count(),
            2,
            "only report and the diagnostic persistence marker remain"
        );
    }
}

#[test]
fn native_arm_timing_waits_callbacks_and_egl_release_resources() {
    let _suite = diagnostic_suite();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let files = GuestFiles(std::env::temp_dir().join(format!("pockethle-timing-{nonce}")));
    fs::create_dir_all(&files.0).unwrap();
    let flash = files.0.join("flash");
    fs::create_dir_all(&flash).unwrap();
    fs::write(
        files.0.join("AUTORUN.EXE"),
        include_bytes!("fixtures/timetest/GZTT999995/AUTORUN.EXE"),
    )
    .unwrap();
    let mut emu = Emulator::with_unicorn_cpu().unwrap();
    emu.max_slices = 3_000_000;
    emu.set_halt_on_unimplemented(true);
    emu.load_pe(files.0.join("AUTORUN.EXE")).unwrap();
    emu.set_gpu_renderer(false);
    assert!(emu.set_memory_division(Some(MemoryDivision::gizmondo_sdk_default())));
    emu.set_screen_size(320, 240);
    {
        let state = &mut emu.process_mut().unwrap().state;
        state
            .vfs
            .mount_read_only("\\SD Card\\GZTT999995\\", &files.0);
        state.vfs.mount_save_dir("\\Flash Disk\\", &flash);
        state.module_path = "\\SD Card\\GZTT999995\\AUTORUN.EXE".into();
        state.synthetic_message_budget = 0;
    }
    let mut hook = |state: &mut KernelState| {
        if state.modal.is_some() {
            state
                .pending_input
                .push_back(InputEvent::KeyDown { vk: 0x0d });
        }
        FrameAction::Continue
    };
    emu.run_with_hook(&mut hook).unwrap();
    let report_path = fs::read_dir(&flash)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case("TIMETEST.TXT")
        })
        .unwrap_or_else(|| {
            panic!(
                "Missing TIMETEST report; guest exit={:?}",
                emu.process().unwrap().state.process_exit_code
            )
        });
    let report = fs::read_to_string(report_path).unwrap();
    println!("{report}");
    assert!(report.contains("TIMETEST_RESULT PASS"), "{report}");
    assert!(
        !report.lines().any(|line| line.starts_with("FAIL ")),
        "{report}"
    );
    let state = &emu.process().unwrap().state;
    assert_eq!(state.process_exit_code, Some(0));
    assert!(state.timers.is_empty());
    assert!(state.wave_out.devices.is_empty());
    assert!(state.vfs.open_handles().is_empty());
}
