//! Disposable Xvfb virtual displays for managed runs.
//!
//! Windows Mobile titles lay their UI out from `Screen.PrimaryScreen.Bounds`,
//! so the host display geometry is part of the emulated device. Managed
//! applications run on the host (see [`crate::managed`]), which means the
//! guest's "screen" is whatever X display the game lands on. Spawning a
//! private Xvfb at the requested geometry gives the game the Pocket PC LCD it
//! was designed for instead of whatever the workstation happens to have.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// A running Xvfb server that is killed when the value is dropped.
pub struct XvfbSession {
    display: String,
    child: Option<Child>,
}

impl XvfbSession {
    pub fn display(&self) -> &str {
        &self.display
    }
}

impl Drop for XvfbSession {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Decide the virtual display geometry for a managed run.
///
/// An explicit `--screen WIDTHxHEIGHT` wins. Without one, a managed game
/// inherits the host display if there is one; when the run would otherwise
/// have no display at all, fall back to the Pocket PC portrait LCD.
pub fn geometry(screen: Option<(u32, u32)>) -> Option<(u32, u32)> {
    if let Some((w, h)) = screen {
        return Some((w, h));
    }
    if std::env::var_os("DISPLAY").is_none() {
        return Some((240, 320));
    }
    None
}

/// Spawn an Xvfb server of the given geometry on a free display number.
pub fn spawn(width: u32, height: u32) -> Result<XvfbSession> {
    let sockets = Path::new("/tmp/.X11-unix");
    for number in 20..100u32 {
        let socket = sockets.join(format!("X{number}"));
        if socket.exists() {
            continue;
        }
        let display = format!(":{number}");
        let mut child = Command::new("Xvfb")
            .arg(&display)
            .args(["-screen", "0", &format!("{width}x{height}x24")])
            .args(["-nolisten", "tcp"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| {
                format!("spawning Xvfb {display} ({width}x{height}) for the managed run")
            })?;
        let started = wait_for_socket(&mut child, &socket);
        match started {
            true => {
                return Ok(XvfbSession {
                    display,
                    child: Some(child),
                });
            }
            false => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    anyhow::bail!("no free display number for Xvfb under /tmp/.X11-unix");
}

fn wait_for_socket(child: &mut Child, socket: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if socket.exists() {
            return true;
        }
        if let Some(status) = child
            .try_wait()
            .context("waiting for Xvfb to start")
            .ok()
            .flatten()
        {
            eprintln!("warning: Xvfb exited early with {status}");
            return false;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
