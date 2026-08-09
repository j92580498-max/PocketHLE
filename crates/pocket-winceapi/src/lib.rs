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
//!
//! Bundled third-party libraries we have no intention of emulating are
//! not given a submodule at all — they get one entry in [`IGNORED_DLLS`]
//! naming the start of the file name, and every call into them is
//! answered generically.

pub mod aygshell;
pub mod commctrl;
pub mod coredll;
pub mod ddraw;
pub mod dlgtemplate;
pub mod game_dlls;
pub mod gles;
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

/// A third-party library we deliberately do not emulate.
///
/// Games bundle their own middleware — audio mixers, movie players,
/// copy-protection shims — and a game that links one usually refuses to
/// start when its calls come back as unimplemented (which returns 0, and
/// 0 reads as failure to most of them). Writing a module full of
/// hand-rolled no-ops per library is a lot of dead code for something we
/// have no intention of implementing, so instead the library gets one
/// line here: match its name by prefix, answer every entry point with
/// "fine", and let the game get on with rendering.
///
/// Matching is by prefix on the lower-cased DLL name so that the version
/// suffixes these libraries ship with (`fmodce.dll`, `fmodce370.dll`, …)
/// are all covered by one entry.
struct IgnoredDll {
    /// Lower-case prefix matched against the import's DLL name.
    prefix: &'static str,
    /// What every entry point returns. `1` is the useful default: it
    /// reads as `TRUE` to a status check and as a non-NULL handle to
    /// code that stores the result and hands it back to us later.
    returns: u32,
    /// Entry points that have to answer something else. Keep this to
    /// the values a game actually gates its start-up on — if a call is
    /// worth more than a constant, the library wants real emulation
    /// rather than an entry here.
    overrides: &'static [(&'static str, u32)],
}

impl IgnoredDll {
    fn value_for(&self, name: &str) -> u32 {
        self.overrides
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(self.returns, |(_, v)| *v)
    }
}

const IGNORED_DLLS: &[IgnoredDll] = &[IgnoredDll {
    // FMOD CE, the audio middleware Xtrakt links against.
    prefix: "fmodce",
    returns: 1,
    overrides: &[
        // Xtrakt opens `Create_FmodSoundDevice` with
        //
        //     if (FSOUND_GetVersion() < 3.75f) -> "incompatible version"
        //
        // and a failure there fails the whole start-up ("Failed to
        // initialize game."). FMOD returns the version as a `float`, and
        // WinCE ARM is soft-float, so the bit pattern travels back in r0
        // and the guest feeds it straight into coredll's `__lts` against
        // the literal — hence the float encoding of 3.75 rather than the
        // integer.
        ("FSOUND_GetVersion", 0x4070_0000),
    ],
}];

fn ignored_dll(dll: &str) -> Option<&'static IgnoredDll> {
    let dll = dll.to_ascii_lowercase();
    IGNORED_DLLS.iter().find(|i| dll.starts_with(i.prefix))
}

/// Handler installed for every import from an [`IgnoredDll`]. It
/// re-derives the answer from the thunk rather than capturing it,
/// because a [`Handler`] is a plain `fn` pointer with nothing to
/// capture into; the table is three entries long, so the scan costs
/// nothing next to the dispatch itself.
fn ignored_dll_stub(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let value =
        ignored_dll(&ctx.thunk.dll).map_or(0, |i| i.value_for(import_name(ctx.thunk).as_ref()));
    Ok(DispatchOutcome::ReturnedR0(value))
}

/// The name an import is dispatched under: the friendly name if the
/// loader resolved one, else the imported name, else `ord:N`.
fn import_name(thunk: &Thunk) -> std::borrow::Cow<'_, str> {
    match (&thunk.binding, &thunk.friendly_name) {
        (_, Some(n)) => n.as_str().into(),
        (ImportBinding::Name(n), _) => n.as_str().into(),
        (ImportBinding::Ordinal(o), _) => format!("ord:{o}").into(),
    }
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
                // Fetch from the stack at [sp + (idx-4)*4]. Use the
                // stack-buffer `read_u32_le` helper so we don't pay
                // for a 4-byte `Vec<u8>` allocation on every stack
                // arg load — `wsprintfW` and friends pull all of
                // their varargs through this path on every call.
                let sp = self.cpu.read_reg(pocket_cpu::regs::ArmReg::Sp)?;
                let off = sp + self.cpu.stack_arg_offset() + (idx - 4) as u32 * 4;
                let value = self.cpu.read_u32_le(off)?;
                return Ok(value);
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
    /// Names that have been registered as constant-returning stubs.
    /// The kernel queries this at load time and patches the IAT with
    /// a `mov r0, #imm; bx lr` native thunk for matching imports,
    /// removing the dispatcher round-trip entirely. Populated
    /// alongside `by_name` via [`Self::register_constant`].
    by_name_constant: HashMap<(String, String), u32>,
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
            by_name_constant: HashMap::new(),
            by_thunk_va: HashMap::new(),
            halt_on_unimplemented: false,
            trace_sink: None,
        };
        coredll::register(&mut d);
        ddraw::register(&mut d);
        aygshell::register(&mut d);
        commctrl::register(&mut d);
        game_dlls::register(&mut d);
        gles::register(&mut d);
        gx::register(&mut d);
        hss::register(&mut d);
        ole32::register(&mut d);
        for ordinal in 0..=4095u16 {
            if let Some(name) = ordinals::lookup("coredll.dll", ordinal) {
                let source = ("coredll.dll".to_string(), name);
                if let Some(handler) = d.by_name.get(&source).copied() {
                    for alias in [format!("ord:{ordinal}"), format!("#{ordinal}")] {
                        let key = ("coredll.dll".to_string(), alias);
                        d.by_name.insert(key.clone(), handler);
                        if let Some(value) = d.by_name_constant.get(&source).copied() {
                            d.by_name_constant.insert(key, value);
                        }
                    }
                }
            }
        }
        d
    }

    pub fn register_handler(&mut self, dll: &str, name: &str, handler: Handler) {
        let key = (dll.to_ascii_lowercase(), name.to_string());
        self.by_name.insert(key.clone(), handler);
        // A new handler shadows any prior constant-return
        // registration for the same (dll, name).
        self.by_name_constant.remove(&key);
        // Names are registered up-front, before any thunk has fired,
        // so the per-thunk cache is always empty here. Clearing it
        // anyway keeps the invariant honest if a future caller decides
        // to register handlers post-warmup.
        self.by_thunk_va.clear();
    }

    /// Like [`Self::register_handler`], but also marks this import
    /// as a pure constant-return stub. The kernel loader will then
    /// patch the IAT with a `mov r0, #imm; bx lr` thunk that runs
    /// inside the JIT without any callback. The handler itself is
    /// still registered as a fallback in case the import is somehow
    /// reached through a path that bypassed the native thunk (e.g.
    /// runtime `GetProcAddress`).
    pub fn register_constant(&mut self, dll: &str, name: &str, value: u32, handler: Handler) {
        let key = (dll.to_ascii_lowercase(), name.to_string());
        self.by_name.insert(key.clone(), handler);
        self.by_name_constant.insert(key, value);
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
    /// `{"dll": "...", "name": "...", "args": [r0, r1, r2, r3], "ret": <u32>, "status": "ok"|"unimplemented"|"halt", "caller": <u32>}`.
    ///
    /// `caller` is the guest return address with the Thumb bit
    /// cleared, which is what makes a trace answer "which of the
    /// game's functions is this?" rather than just "what did it call?".
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
        // The HashMap key is `(String, String)`, so we still have to
        // build owned strings for the lookup itself — but we only do
        // it once per unique thunk_va, not once per call.
        let key = (dll_key, import_name(thunk).into_owned());
        let resolved = self
            .by_name
            .get(&key)
            .copied()
            // An explicit handler wins: a library can be ignored
            // wholesale and still have one or two entry points we
            // decided to emulate properly.
            .or_else(|| ignored_dll(&thunk.dll).map(|_| ignored_dll_stub as Handler));
        self.by_thunk_va.insert(thunk.thunk_va, resolved);
        resolved
    }
}

impl Dispatcher for WinCeDispatcher {
    fn constant_for(&self, thunk: &Thunk) -> Option<u32> {
        // Look up the explicit constants registry populated by
        // `register_constant`. We deliberately do *not* compare
        // handler function pointers — the Rust compiler may merge
        // multiple identical-body fns into one address, so e.g.
        // `zero_returning` and `null_returning` could appear equal
        // in release builds. An explicit table sidesteps that.
        let dll_key = thunk.dll.to_ascii_lowercase();
        self.by_name_constant
            .get(&(dll_key, import_name(thunk).into_owned()))
            .copied()
        // Ignored libraries deliberately stay out of this. Answering
        // here would let the loader patch the IAT with a native
        // `mov r0, #imm; bx lr` and the calls would never reach the
        // dispatcher — but a library we have not implemented is exactly
        // the one whose calls we want to see in `--trace` when the game
        // stops at the next thing.
    }

    fn dynamic_names(&self, dll: &str) -> Vec<String> {
        let dll_key = dll.to_ascii_lowercase();
        let mut names: Vec<String> = self
            .by_name
            .keys()
            .filter(|(registered_dll, _)| registered_dll == &dll_key)
            .map(|(_, name)| name.clone())
            .collect();
        if dll_key == "coredll.dll" || pocket_gles::ordinals::is_gles_dll(&dll_key) {
            for ordinal in 0..=4095u16 {
                if ordinals::lookup(&dll_key, ordinal).is_some() {
                    names.push(format!("ord:{ordinal}"));
                    names.push(format!("#{ordinal}"));
                }
            }
        }
        // A GLES DLL exports every name in its ordinal table, whether or
        // not we implement it. `GetProcAddress` returning non-null for
        // an unimplemented entry point is what we want: the guest gets a
        // thunk, and the call is logged when it fires instead of the
        // guest silently taking a "driver too old" fallback path.
        if pocket_gles::ordinals::is_gles_dll(&dll_key) {
            names.extend(pocket_gles::ordinals::names_for(&dll_key));
        }
        names.sort();
        names.dedup();
        names
    }

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

        // Where in the guest this call came from. Two identical calls
        // are indistinguishable in a trace without it, which makes a
        // 250k-line log almost useless for the question that matters
        // most — *which* of the game's own functions is running. The
        // low bit is the Thumb flag rather than part of the address,
        // so clear it to get something that lines up with a disassembly.
        let caller = if self.trace_sink.is_some() {
            cpu.read_reg(ArmReg::Lr).unwrap_or(0) & !1
        } else {
            0
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
            let name = import_name(thunk).into_owned();
            let (ret, status) = match &outcome {
                Ok(DispatchOutcome::ReturnedR0(v)) | Ok(DispatchOutcome::ReturnedR0R1(v, _)) => (
                    *v,
                    if ignored_dll(&thunk.dll).is_some() {
                        "ignored"
                    } else {
                        "ok"
                    },
                ),
                Ok(DispatchOutcome::Halt) => (0, "halt"),
                Ok(DispatchOutcome::Unimplemented) => (0, "unimplemented"),
                Ok(DispatchOutcome::JumpTo(pc)) => (*pc, "trampoline"),
                Err(_) => (0, "error"),
            };
            let line = format!(
                "{{\"dll\":\"{dll}\",\"name\":\"{n}\",\"args\":[{a0},{a1},{a2},{a3}],\"ret\":{ret},\"status\":\"{st}\",\"caller\":{caller}}}\n",
                dll = dll_key,
                n = name,
                a0 = args[0],
                a1 = args[1],
                a2 = args[2],
                a3 = args[3],
                ret = ret,
                st = status,
                caller = caller,
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

    fn fake_thunk(dll: &str, name: &str) -> Thunk {
        Thunk {
            thunk_va: 0,
            iat_va: 0,
            dll: dll.into(),
            binding: ImportBinding::Name(name.into()),
            friendly_name: Some(name.into()),
        }
    }

    #[test]
    fn constant_for_returns_zero_for_known_zero_stub() {
        let d = WinCeDispatcher::new();
        // `GetLastError` is registered as zero_returning in coredll.
        let t = fake_thunk("coredll.dll", "GetLastError");
        assert_eq!(d.constant_for(&t), Some(0));
    }

    #[test]
    fn constant_for_returns_one_for_known_one_stub() {
        let d = WinCeDispatcher::new();
        // `FreeLibrary` is registered as one_returning in coredll.
        let t = fake_thunk("coredll.dll", "FreeLibrary");
        assert_eq!(d.constant_for(&t), Some(1));
    }

    #[test]
    fn constant_for_returns_none_for_real_handler() {
        let d = WinCeDispatcher::new();
        // `CreateFileW` does real work, not a constant return.
        let t = fake_thunk("coredll.dll", "CreateFileW");
        assert_eq!(d.constant_for(&t), None);
    }

    #[test]
    fn constant_for_returns_none_for_unknown_import() {
        let d = WinCeDispatcher::new();
        let t = fake_thunk("coredll.dll", "ThisDoesNotExist");
        assert_eq!(d.constant_for(&t), None);
    }

    /// Every dispatcher key carries the `.dll` suffix, so a CeGCC image —
    /// which writes the bare module name `COREDLL` into its import
    /// directory — would miss all of them and degrade every single import
    /// to an unimplemented stub. `pocket_pe` normalizes the name as it
    /// enters the loader; this pins the assumption that makes that
    /// necessary, so a future change to either side breaks loudly here.
    ///
    /// Each thunk needs its own `thunk_va`: `resolve_handler` memoizes on
    /// it, so reusing one address would answer the second lookup from the
    /// first one's cache entry.
    #[test]
    fn dispatcher_keys_all_carry_the_dll_suffix() {
        let mut d = WinCeDispatcher::new();
        let mut va = 0x7000_0000;
        let mut at = |dll: &str, name: &str| {
            va += 0x20;
            Thunk {
                thunk_va: va,
                ..fake_thunk(dll, name)
            }
        };
        for name in ["malloc", "MessageBoxW", "_fpreset", "_fcloseall"] {
            assert!(
                d.resolve_handler(&at("coredll.dll", name)).is_some(),
                "coredll.dll!{name} must resolve"
            );
            assert!(
                d.resolve_handler(&at("COREDLL", name)).is_none(),
                "a suffix-less \"COREDLL\" is expected to miss — pocket_pe \
                 must normalize it before it reaches the dispatcher"
            );
        }
    }

    #[test]
    fn an_ignored_library_answers_every_name_it_exports() {
        // The point of the ignore list is that we never enumerate the
        // library's exports, so a name nobody has ever heard of has to
        // resolve just like a real one.
        let mut d = WinCeDispatcher::new();
        for name in ["FSOUND_Init", "FMUSIC_LoadSongEx", "FSOUND_NeverHeardOfIt"] {
            let t = fake_thunk("fmodce.dll", name);
            assert!(
                d.resolve_handler(&t).is_some(),
                "{name} fell through to the unimplemented path"
            );
        }
    }

    #[test]
    fn the_prefix_covers_the_version_suffixed_spellings() {
        assert!(ignored_dll("FMODCE.DLL").is_some(), "match is case-blind");
        assert!(ignored_dll("fmodce370.dll").is_some());
        assert!(ignored_dll("coredll.dll").is_none());
    }

    #[test]
    fn fmod_reports_the_version_xtrakt_gates_its_start_up_on() {
        // Xtrakt does `if (FSOUND_GetVersion() < 3.75f) -> bail`, with
        // the float arriving in r0 under the soft-float ABI. Anything
        // that is not a valid encoding of >= 3.75 fails start-up.
        let fmod = ignored_dll("fmodce.dll").expect("fmodce is on the ignore list");
        let bits = fmod.value_for("FSOUND_GetVersion");
        assert!(
            f32::from_bits(bits) >= 3.75,
            "reported {} (bits {bits:#010x})",
            f32::from_bits(bits)
        );
        // Everything else just has to look like success.
        assert_eq!(fmod.value_for("FSOUND_Init"), 1);
    }

    #[test]
    fn an_explicit_handler_still_beats_the_ignore_list() {
        let mut d = WinCeDispatcher::new();
        d.register_constant("fmodce.dll", "FSOUND_Init", 7, |_| {
            Ok(DispatchOutcome::ReturnedR0(7))
        });
        let t = fake_thunk("fmodce.dll", "FSOUND_Init");
        assert_eq!(d.constant_for(&t), Some(7));
        assert!(d.resolve_handler(&t).is_some());
    }
}
