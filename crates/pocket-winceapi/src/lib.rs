//! High-level emulation of WinCE / Windows Mobile system DLLs.
//!
//! Each emulated DLL has its own submodule:
//!
//! * [`coredll`] — the catch-all kernel/runtime DLL. Imported almost
//!   exclusively by ordinal, so we ship a JSON ordinal map (see
//!   `data/coredll-ordinals.json`).
//! * [`aygshell`] — Pocket PC shell extensions (`SHFullScreen`,
//!   `SHCreateMenuBar`).
//! * [`gx`] — GAPI (Game API) for direct framebuffer access.
//! * [`hss`] — Hekkus Sound System (popular freeware audio engine
//!   bundled with many Pocket PC games).
//!
//! All four are dispatched through a single [`WinCeDispatcher`] that
//! implements [`pocket_kernel::Dispatcher`].

pub mod aygshell;
pub mod coredll;
pub mod gx;
pub mod hss;
pub mod ole32;
pub mod ordinals;

use std::collections::HashMap;
use std::io::Write;

use pocket_cpu::{regs::ArmReg, Cpu};
use pocket_kernel::{DispatchOutcome, Dispatcher, KernelError, KernelState, Thunk};
use pocket_pe::ImportBinding;

/// Convert an ordinal-only import to a friendly name where possible.
pub fn resolve_ordinal(dll: &str, ordinal: u16) -> Option<String> {
    ordinals::lookup(dll, ordinal)
}

/// Per-call context passed to handler functions.
pub struct CallCtx<'a> {
    pub cpu: &'a mut dyn Cpu,
    pub thunk: &'a Thunk,
    pub kernel: &'a mut KernelState,
}

impl<'a> CallCtx<'a> {
    pub fn arg_u32(&mut self, idx: u8) -> Result<u32, KernelError> {
        use pocket_cpu::regs::ArmReg::*;
        let reg = match idx {
            0 => R0,
            1 => R1,
            2 => R2,
            3 => R3,
            _ => {
                // Fetch from the stack at [sp + (idx-4)*4].
                let sp = self.cpu.read_reg(pocket_cpu::regs::ArmReg::Sp)?;
                let off = sp + (idx - 4) as u32 * 4;
                let bytes = self.cpu.read_mem(off, 4)?;
                return Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
            }
        };
        Ok(self.cpu.read_reg(reg)?)
    }
}

/// Function pointer for a host-side handler.
pub type Handler = fn(&mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError>;

/// Top-level dispatcher that owns per-DLL handler tables.
pub struct WinCeDispatcher {
    /// Key is `(dll_lowercased, friendly_name)`.
    by_name: HashMap<(String, String), Handler>,
    /// Per-thunk lookup cache populated lazily on the first dispatch
    /// for that thunk. The hot path (which can fire ~10k times a
    /// second during a JumpyBall frame) used to recompute the
    /// lowercased DLL string and a `(String, String)` key on every
    /// call; now it just hashes a `u32`.
    ///
    /// `None` means the name was looked up but no handler was
    /// registered — we cache the negative result too so we don't pay
    /// the string-allocation cost on every unimplemented call either.
    by_thunk_va: HashMap<u32, Option<Handler>>,
    /// If `true`, an unimplemented call halts the emulator instead of
    /// returning 0. Useful for the Linux CLI tracing run.
    pub halt_on_unimplemented: bool,
    /// Optional JSON-lines sink. One record per dispatched call.
    trace_sink: Option<Box<dyn Write + Send>>,
}

impl Default for WinCeDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl WinCeDispatcher {
    pub fn new() -> Self {
        let mut d = Self {
            by_name: HashMap::new(),
            by_thunk_va: HashMap::new(),
            halt_on_unimplemented: false,
            trace_sink: None,
        };
        coredll::register(&mut d);
        aygshell::register(&mut d);
        gx::register(&mut d);
        hss::register(&mut d);
        ole32::register(&mut d);
        d
    }

    pub fn register_handler(&mut self, dll: &str, name: &str, handler: Handler) {
        self.by_name
            .insert((dll.to_ascii_lowercase(), name.to_string()), handler);
        // Names are registered up-front, before any thunk has fired,
        // so the per-thunk cache is always empty here. Clearing it
        // anyway keeps the invariant honest if a future caller decides
        // to register handlers post-warmup.
        self.by_thunk_va.clear();
    }

    pub fn registered_count(&self) -> usize {
        self.by_name.len()
    }

    /// Iterate every (dll, name) pair currently registered.
    pub fn registered_iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.by_name.keys().map(|(d, n)| (d.as_str(), n.as_str()))
    }

    /// Enable JSON-lines tracing. Each dispatched call writes one
    /// record with the form
    /// `{"dll": "...", "name": "...", "args": [r0, r1, r2, r3], "ret": <u32>, "status": "ok"|"unimplemented"|"halt"}`.
    pub fn set_trace_sink(&mut self, sink: Box<dyn Write + Send>) {
        self.trace_sink = Some(sink);
    }

    /// Resolve `thunk` to a handler, populating [`Self::by_thunk_va`]
    /// on the first call. Subsequent calls for the same `thunk_va`
    /// hit the cache and pay only one `u32` hash.
    fn resolve_handler(&mut self, thunk: &Thunk) -> Option<Handler> {
        if let Some(cached) = self.by_thunk_va.get(&thunk.thunk_va) {
            return *cached;
        }
        let dll_key = thunk.dll.to_ascii_lowercase();
        let name_owned;
        let name: &str = match (&thunk.binding, &thunk.friendly_name) {
            (_, Some(n)) => n.as_str(),
            (ImportBinding::Name(n), _) => n.as_str(),
            (ImportBinding::Ordinal(o), _) => {
                name_owned = format!("ord:{o}");
                &name_owned
            }
        };
        // The HashMap key is `(String, String)`, so we still have to
        // build owned strings for the lookup itself — but we only do
        // it once per unique thunk_va, not once per call.
        let key = (dll_key, name.to_string());
        let resolved = self.by_name.get(&key).copied();
        self.by_thunk_va.insert(thunk.thunk_va, resolved);
        resolved
    }
}

impl Dispatcher for WinCeDispatcher {
    fn dispatch(
        &mut self,
        cpu: &mut dyn Cpu,
        thunk: &Thunk,
        kernel: &mut KernelState,
    ) -> Result<DispatchOutcome, KernelError> {
        let handler_opt = self.resolve_handler(thunk);

        // Capture args before the handler may mutate them. Skip the
        // four register reads entirely when nothing is going to log
        // them — these reads aren't free in the unicorn backend.
        let args = if self.trace_sink.is_some() {
            [
                cpu.read_reg(ArmReg::R0).unwrap_or(0),
                cpu.read_reg(ArmReg::R1).unwrap_or(0),
                cpu.read_reg(ArmReg::R2).unwrap_or(0),
                cpu.read_reg(ArmReg::R3).unwrap_or(0),
            ]
        } else {
            [0; 4]
        };

        let outcome = if let Some(handler) = handler_opt {
            if log::log_enabled!(log::Level::Trace) {
                log::trace!("call {}", thunk.label());
            }
            let mut ctx = CallCtx { cpu, thunk, kernel };
            match handler(&mut ctx) {
                Ok(o) => Ok(o),
                Err(e) => {
                    // A handler that hit bad guest memory shouldn't
                    // bring the whole emulator down — the game itself
                    // is the one that passed garbage. Log loudly and
                    // synthesise a 0 return so the trace still
                    // captures every call after this one.
                    log::warn!("handler {} failed: {}; returning 0", thunk.label(), e);
                    Ok(DispatchOutcome::ReturnedR0(0))
                }
            }
        } else {
            log::warn!("unimplemented call -> {}", thunk.label());
            if self.halt_on_unimplemented {
                Ok(DispatchOutcome::Halt)
            } else {
                Ok(DispatchOutcome::Unimplemented)
            }
        };

        if let Some(sink) = self.trace_sink.as_mut() {
            // Trace path is cold-ish (only on `--trace`), so it's
            // fine to pay the formatting cost here.
            let dll_key = thunk.dll.to_ascii_lowercase();
            let name = match (&thunk.binding, &thunk.friendly_name) {
                (_, Some(n)) => n.clone(),
                (ImportBinding::Name(n), _) => n.clone(),
                (ImportBinding::Ordinal(o), _) => format!("ord:{o}"),
            };
            let (ret, status) = match &outcome {
                Ok(DispatchOutcome::ReturnedR0(v)) => (*v, "ok"),
                Ok(DispatchOutcome::ReturnedR0R1(v, _)) => (*v, "ok"),
                Ok(DispatchOutcome::Halt) => (0, "halt"),
                Ok(DispatchOutcome::Unimplemented) => (0, "unimplemented"),
                Ok(DispatchOutcome::JumpTo(pc)) => (*pc, "trampoline"),
                Err(_) => (0, "error"),
            };
            let line = format!(
                "{{\"dll\":\"{dll}\",\"name\":\"{n}\",\"args\":[{a0},{a1},{a2},{a3}],\"ret\":{ret},\"status\":\"{st}\"}}\n",
                dll = dll_key,
                n = name,
                a0 = args[0],
                a1 = args[1],
                a2 = args[2],
                a3 = args[3],
                ret = ret,
                st = status,
            );
            let _ = sink.write_all(line.as_bytes());
        }

        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_built_in_handlers() {
        let d = WinCeDispatcher::new();
        assert!(d.registered_count() > 0);
    }
}
