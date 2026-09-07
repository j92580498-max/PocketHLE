use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::xvfb;

pub struct ManagedRun {
    pub terminated_by_timeout: bool,
}

/// Compatibility assemblies shipped next to the CLI (`managed-compat/`).
///
/// .NET Compact Framework applications reference CF-only assemblies such as
/// `Microsoft.WindowsCE.Forms` that no host runtime provides. The repo ships
/// clean-room stand-ins; putting the directory on `MONO_PATH` lets images that
/// reference them resolve the types. Games that never touch these assemblies
/// are unaffected — an unused `MONO_PATH` entry is inert.
fn compat_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("managed-compat");
    dir.is_dir().then_some(dir)
}

/// The `DISPLAY` value the managed application and its helpers run against.
type DisplayEnv = Option<String>;

#[allow(clippy::too_many_arguments)]
pub fn run(
    exe: &Path,
    mount_dir: Option<&Path>,
    runtime: Option<&Path>,
    runtime_path: Option<&Path>,
    duration_seconds: u64,
    taps: &[String],
    keys: &[String],
    dump_frames_to: Option<&Path>,
    screen: Option<(u32, u32)>,
) -> Result<ManagedRun> {
    // Managed Windows Mobile games lay out from `Screen.PrimaryScreen.Bounds`,
    // so they need a display that matches the device they shipped on. Host one
    // when we can: `--screen` for the geometry, the Pocket PC portrait LCD as
    // the default when the run would otherwise have no display at all.
    let session = match xvfb::geometry(screen) {
        Some((width, height)) => {
            let session = xvfb::spawn(width, height)?;
            println!(
                "Managed virtual display: Xvfb {} at {width}x{height}",
                session.display()
            );
            Some(session)
        }
        None => None,
    };
    let display_env: DisplayEnv = session
        .as_ref()
        .map(|s| s.display().to_owned())
        .or_else(|| std::env::var("DISPLAY").ok());
    let _session = session;

    let runtime = runtime
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("POCKETHLE_MANAGED_RUNTIME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("mono"));
    let mut command = Command::new(&runtime);
    command.arg(exe).stdin(Stdio::null());
    if let Some(dir) = mount_dir {
        command.current_dir(dir);
    }
    if let Some(display) = display_env.as_deref() {
        command.env("DISPLAY", display);
    }
    if let Some(path) = runtime_path {
        let existing = std::env::var_os("MONO_PATH");
        let mut value = path.as_os_str().to_os_string();
        if let Some(existing) = existing {
            value.push(":");
            value.push(&existing);
        }
        command.env("MONO_PATH", value);
    }
    if let Some(dir) = compat_dir() {
        let existing = std::env::var_os("MONO_PATH");
        let mut value = dir.as_os_str().to_os_string();
        if let Some(existing) = existing {
            value.push(":");
            value.push(&existing);
        }
        command.env("MONO_PATH", value);
        println!("Managed compatibility assemblies: {}", dir.display());
    }
    println!(
        "Managed image: launching {} through {}",
        exe.display(),
        runtime.display()
    );
    let mut child = command
        .spawn()
        .with_context(|| format!("launching managed runtime {}", runtime.display()))?;

    let pid = child.id();
    let window = wait_for_window(pid, Duration::from_secs(3), display_env.as_deref());
    if duration_seconds == 0 && taps.is_empty() && keys.is_empty() && dump_frames_to.is_none() {
        let status = child.wait().context("waiting for managed application")?;
        if !status.success() {
            anyhow::bail!("managed application exited with {status}");
        }
        return Ok(ManagedRun {
            terminated_by_timeout: false,
        });
    }

    thread::sleep(Duration::from_millis(1200));
    for tap in taps {
        let (_, x, y) = parse_tap(tap)?;
        send_x11_click(window.as_deref(), x, y, display_env.as_deref());
        // Give menu open/select cycles and click handlers time to settle
        // before the next synthetic tap lands.
        thread::sleep(Duration::from_millis(500));
    }
    for key in keys {
        send_x11_key(window.as_deref(), key, display_env.as_deref());
    }

    let screenshot = dump_frames_to.map(|dir| {
        let path = dir.join("managed-000000.png");
        let _ = std::fs::create_dir_all(dir);
        path
    });
    if let Some(path) = screenshot.as_ref() {
        capture_x11(window.as_deref(), path, display_env.as_deref());
    }

    let timeout = if duration_seconds == 0 {
        1
    } else {
        duration_seconds
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
    loop {
        if let Some(status) = child.try_wait().context("polling managed application")? {
            if !status.success() {
                anyhow::bail!("managed application exited with {status}");
            }
            return Ok(ManagedRun {
                terminated_by_timeout: false,
            });
        }
        if std::time::Instant::now() >= deadline {
            terminate(&mut child);
            return Ok(ManagedRun {
                terminated_by_timeout: true,
            });
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn parse_tap(value: &str) -> Result<(Option<u64>, u16, u16)> {
    let (frame, coordinates) = match value.split_once(':') {
        Some((frame, coordinates)) => (
            Some(frame.parse::<u64>().context("invalid tap frame")?),
            coordinates,
        ),
        None => (None, value),
    };
    let (x, y) = coordinates
        .split_once(',')
        .with_context(|| format!("invalid managed tap {value:?}; expected [FRAME:]X,Y"))?;
    Ok((
        frame,
        x.trim().parse().context("invalid managed tap x")?,
        y.trim().parse().context("invalid managed tap y")?,
    ))
}

fn wait_for_window(pid: u32, timeout: Duration, display: Option<&str>) -> Option<String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(window) = window_id(pid, display) {
            return Some(window);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn window_id(_pid: u32, display: Option<&str>) -> Option<String> {
    let output = Command::new("xdotool")
        .args(["search", "--onlyvisible", "--name", "."])
        .env("DISPLAY", display.unwrap_or_default())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut best: Option<(u64, String)> = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let window = line.trim();
        if window.is_empty() {
            continue;
        }
        let geometry = Command::new("xdotool")
            .args(["getwindowgeometry", window])
            .env("DISPLAY", display.unwrap_or_default())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())?;
        let area = geometry
            .lines()
            .find_map(|line| line.trim().strip_prefix("Geometry: "))
            .and_then(|value| value.split_once('x'))
            .and_then(|(width, height)| {
                Some(width.parse::<u64>().ok()? * height.parse::<u64>().ok()?)
            })?;
        if best.as_ref().is_none_or(|(best_area, _)| area > *best_area) {
            best = Some((area, window.to_string()));
        }
    }
    best.map(|(_, window)| window)
}

fn send_x11_click(window: Option<&str>, x: u16, y: u16, display: Option<&str>) {
    let Some(window) = window else {
        eprintln!("warning: managed tap skipped because no application window was found");
        return;
    };
    // Mono WinForms menus open on button-down and select the highlighted
    // item on button-up, so the press and the release must be distinct
    // events with the pointer resting on the item. An instant
    // `click 1` closes the menu again without selecting anything
    // (vAlienAttack never reaches gameplay without the hold).
    let script = format!(
        "mousemove --window {window} {x} {y} mousedown 1 sleep 0.12 mouseup 1",
        window = window,
        x = x,
        y = y,
    );
    let result = Command::new("xdotool")
        .args(script.split_whitespace())
        .env("DISPLAY", display.unwrap_or_default())
        .status();
    if result.is_err() {
        eprintln!("warning: managed tap skipped because xdotool is unavailable");
    }
}

fn send_x11_key(window: Option<&str>, key: &str, display: Option<&str>) {
    let Some(window) = window else {
        eprintln!("warning: managed key skipped because no application window was found");
        return;
    };
    let result = Command::new("xdotool")
        .args(["key", "--window", window, key])
        .env("DISPLAY", display.unwrap_or_default())
        .status();
    if result.is_err() {
        eprintln!("warning: managed key skipped because xdotool is unavailable");
    }
}

fn capture_x11(window: Option<&str>, path: &Path, display: Option<&str>) {
    let Some(window) = window else {
        eprintln!("warning: managed screenshot skipped because no application window was found");
        return;
    };
    let result = Command::new("import")
        .args(["-window", window, &path.to_string_lossy()])
        .env("DISPLAY", display.unwrap_or_default())
        .status();
    if result.is_err() {
        eprintln!("warning: managed screenshot skipped because ImageMagick import is unavailable");
    } else {
        println!("Managed screenshot written to {}", path.display());
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
