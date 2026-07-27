//! JNI bridge between the PocketHLE core/library and the Android
//! frontend. The Java side talks to this crate exclusively through
//! UTF-8 JSON strings — that keeps the FFI surface small and removes
//! any need for shared Kotlin/Rust data classes.
//!
//! Exposed methods (all on `com.pockethle.app.NativeBridge`):
//!
//! * `banner()` — sanity string showing the loaded version.
//! * `listGames(libraryRoot)` — JSON array of [`pocket_library::GameEntry`].
//! * `importCab(libraryRoot, cabPath)` — JSON of the freshly-imported entry.
//! * `removeGame(libraryRoot, id)` — `"ok"` or `"err: ..."`.
//! * `readConfig(libraryRoot)` — JSON of [`pocket_library::LauncherConfig`].
//! * `writeConfig(libraryRoot, json)` — overwrite config with a JSON blob.
//! * `readGameSettings(libraryRoot, id)` — JSON of per-game settings.
//! * `writeGameSettings(libraryRoot, id, json)` — persist per-game settings.
//! * `runGame(libraryRoot, id)` — legacy single-shot emulator run that
//!   only ever returns the *final* framebuffer. Kept around for
//!   compatibility but the GUI no longer drives it because it looks
//!   like a hang to the user; see [`runner`].
//! * `nativeStartGame` / `nativePollFrame` / `nativeSendInput` /
//!   `nativeRequestStop` / `nativeFinishGame` — session-based
//!   replacement that streams framebuffers to the UI as the guest
//!   draws them, forwards touches and D-pad presses to the kernel,
//!   and lets the user quit cleanly via Back. See [`runner`] for
//!   the implementation rationale.

mod runner;

use std::path::Path;
use std::path::PathBuf;

use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jbyteArray, jint, jlong, jstring};
use jni::JNIEnv;

use pocket_core::kernel::InputEvent;
use pocket_core::Emulator;
use pocket_library::{CpuBackendPref, GameEntry, GameSettings, Library};
use serde::Serialize;

use crate::runner::{InputCommand, Session};

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_banner<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    init_logger();
    let banner = format!("PocketHLE v{} (Android)", env!("CARGO_PKG_VERSION"));
    new_jstring(&env, banner)
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_MainActivity_banner<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    init_logger();
    new_jstring(
        &env,
        format!("PocketHLE v{} (Android)", env!("CARGO_PKG_VERSION")),
    )
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_listGames<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    library_root: JString<'local>,
) -> jstring {
    init_logger();
    let root = match jstring_to_path(&mut env, library_root) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing library root")),
    };
    let result = (|| -> anyhow::Result<String> {
        let lib = Library::open(&root)?;
        let games: Vec<&GameEntry> = lib.games().iter().collect();
        Ok(serde_json::to_string(&games)?)
    })();
    match result {
        Ok(j) => new_jstring(&env, j),
        Err(e) => new_jstring(&env, error_json(&format!("listGames: {e:#}"))),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_importCab<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    library_root: JString<'local>,
    cab_path: JString<'local>,
) -> jstring {
    init_logger();
    let root = match jstring_to_path(&mut env, library_root) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing library root")),
    };
    let cab = match jstring_to_path(&mut env, cab_path) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing cab path")),
    };
    let result = (|| -> anyhow::Result<String> {
        let mut lib = Library::open(&root)?;
        let entry = lib.import_cab(&cab)?;
        Ok(serde_json::to_string(&entry)?)
    })();
    match result {
        Ok(j) => new_jstring(&env, j),
        Err(e) => new_jstring(&env, error_json(&format!("importCab: {e:#}"))),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_importExe<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    library_root: JString<'local>,
    exe_path: JString<'local>,
) -> jstring {
    init_logger();
    let root = match jstring_to_path(&mut env, library_root) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing library root")),
    };
    let exe = match jstring_to_path(&mut env, exe_path) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing exe path")),
    };
    let result = (|| -> anyhow::Result<String> {
        let mut lib = Library::open(&root)?;
        let entry = lib.import_exe(&exe)?;
        Ok(serde_json::to_string(&entry)?)
    })();
    match result {
        Ok(j) => new_jstring(&env, j),
        Err(e) => new_jstring(&env, error_json(&format!("importExe: {e:#}"))),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_removeGame<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    library_root: JString<'local>,
    id: JString<'local>,
) -> jstring {
    init_logger();
    let root = match jstring_to_path(&mut env, library_root) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing library root")),
    };
    let id = match jstring_to_string(&mut env, id) {
        Some(s) => s,
        None => return new_jstring(&env, error_json("missing game id")),
    };
    let result = (|| -> anyhow::Result<()> {
        let mut lib = Library::open(&root)?;
        lib.remove(&id)?;
        Ok(())
    })();
    match result {
        Ok(()) => new_jstring(&env, "{\"ok\":true}"),
        Err(e) => new_jstring(&env, error_json(&format!("removeGame: {e:#}"))),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_readConfig<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    library_root: JString<'local>,
) -> jstring {
    init_logger();
    let root = match jstring_to_path(&mut env, library_root) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing library root")),
    };
    let result = (|| -> anyhow::Result<String> {
        let lib = Library::open(&root)?;
        Ok(serde_json::to_string(lib.config())?)
    })();
    match result {
        Ok(j) => new_jstring(&env, j),
        Err(e) => new_jstring(&env, error_json(&format!("readConfig: {e:#}"))),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_writeConfig<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    library_root: JString<'local>,
    config_json: JString<'local>,
) -> jstring {
    init_logger();
    let root = match jstring_to_path(&mut env, library_root) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing library root")),
    };
    let body = match jstring_to_string(&mut env, config_json) {
        Some(s) => s,
        None => return new_jstring(&env, error_json("missing config json")),
    };
    let result = (|| -> anyhow::Result<()> {
        let mut lib = Library::open(&root)?;
        let cfg = serde_json::from_str(&body)?;
        *lib.config_mut() = cfg;
        lib.save()?;
        Ok(())
    })();
    match result {
        Ok(()) => new_jstring(&env, "{\"ok\":true}"),
        Err(e) => new_jstring(&env, error_json(&format!("writeConfig: {e:#}"))),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_readGameSettings<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    library_root: JString<'local>,
    id: JString<'local>,
) -> jstring {
    init_logger();
    let root = match jstring_to_path(&mut env, library_root) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing library root")),
    };
    let id = match jstring_to_string(&mut env, id) {
        Some(s) => s,
        None => return new_jstring(&env, error_json("missing game id")),
    };
    let result = (|| -> anyhow::Result<String> {
        let lib = Library::open(&root)?;
        let entry = lib
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown game"))?;
        Ok(serde_json::to_string(&entry.settings)?)
    })();
    match result {
        Ok(j) => new_jstring(&env, j),
        Err(e) => new_jstring(&env, error_json(&format!("readGameSettings: {e:#}"))),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_writeGameSettings<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    library_root: JString<'local>,
    id: JString<'local>,
    settings_json: JString<'local>,
) -> jstring {
    init_logger();
    let root = match jstring_to_path(&mut env, library_root) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing library root")),
    };
    let id = match jstring_to_string(&mut env, id) {
        Some(s) => s,
        None => return new_jstring(&env, error_json("missing game id")),
    };
    let body = match jstring_to_string(&mut env, settings_json) {
        Some(s) => s,
        None => return new_jstring(&env, error_json("missing settings json")),
    };
    let result = (|| -> anyhow::Result<()> {
        let mut lib = Library::open(&root)?;
        let new_settings: GameSettings = serde_json::from_str(&body)?;
        lib.update_settings(&id, new_settings)?;
        Ok(())
    })();
    match result {
        Ok(()) => new_jstring(&env, "{\"ok\":true}"),
        Err(e) => new_jstring(&env, error_json(&format!("writeGameSettings: {e:#}"))),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_runGame<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    library_root: JString<'local>,
    id: JString<'local>,
) -> jstring {
    init_logger();
    let root = match jstring_to_path(&mut env, library_root) {
        Some(p) => p,
        None => return new_jstring(&env, error_json("missing library root")),
    };
    let id = match jstring_to_string(&mut env, id) {
        Some(s) => s,
        None => return new_jstring(&env, error_json("missing game id")),
    };
    let outcome = run_game(&root, &id);
    let payload = serde_json::to_string(&outcome)
        .unwrap_or_else(|e| format!("{{\"ok\":false,\"error\":\"json: {e}\"}}"));
    new_jstring(&env, payload)
}

#[derive(Serialize)]
struct RunOutcomeJson {
    ok: bool,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    frame: Option<FrameJson>,
}

#[derive(Serialize)]
struct FrameJson {
    width: u32,
    height: u32,
    /// RGBA8888 bytes encoded as base64 (standard alphabet, padded).
    rgba_b64: String,
}

fn run_game(root: &Path, id: &str) -> RunOutcomeJson {
    let result = (|| -> anyhow::Result<RunOutcomeJson> {
        let lib = Library::open(root)?;
        let entry = lib.get(id).ok_or_else(|| anyhow::anyhow!("unknown game"))?;
        let mut summary_lines = vec![
            format!("Game: {}", entry.display_name),
            format!("Backend: {}", entry.settings.cpu_backend.label()),
        ];
        let exe = entry.executable_path(root);
        summary_lines.push(format!("Executable: {}", exe.display()));
        let mut emu = match entry.settings.cpu_backend {
            CpuBackendPref::Stub => Emulator::with_stub_cpu(),
            CpuBackendPref::Unicorn => match build_unicorn() {
                Ok(emu) => emu,
                Err(e) => {
                    summary_lines.push(format!("Unicorn unavailable, falling back to stub: {e}"));
                    Emulator::with_stub_cpu()
                }
            },
        };
        emu.set_halt_on_unimplemented(entry.settings.halt_on_unimplemented);
        emu.max_slices = entry.settings.max_slices;
        emu.instruction_budget_per_slice = entry.settings.instructions_per_slice;
        emu.load_pe(&exe)?;
        emu.mount_dir("\\Application\\", entry.extracted_dir(root));
        let run_status = match emu.run() {
            Ok(()) => "Emulator exited cleanly.".to_string(),
            Err(e) => format!("Emulator stopped: {e:#}"),
        };
        summary_lines.push(run_status);
        let frame = emu.process().map(|p| {
            let fb = &p.state.framebuffer;
            FrameJson {
                width: fb.width,
                height: fb.height,
                rgba_b64: base64_encode(&fb.snapshot_rgba8888()),
            }
        });
        Ok(RunOutcomeJson {
            ok: true,
            summary: summary_lines.join("\n"),
            error: None,
            frame,
        })
    })();
    match result {
        Ok(j) => j,
        Err(e) => RunOutcomeJson {
            ok: false,
            summary: "Run failed before emulator started".to_string(),
            error: Some(format!("{e:#}")),
            frame: None,
        },
    }
}

#[cfg(feature = "unicorn")]
fn build_unicorn() -> anyhow::Result<Emulator> {
    Emulator::with_unicorn_cpu()
}

#[cfg(not(feature = "unicorn"))]
fn build_unicorn() -> anyhow::Result<Emulator> {
    Err(anyhow::anyhow!(
        "binary was not compiled with the `unicorn` feature"
    ))
}

fn jstring_to_string<'a>(env: &mut JNIEnv<'a>, value: JString<'a>) -> Option<String> {
    if value.is_null() {
        return None;
    }
    match env.get_string(&value) {
        Ok(s) => Some(s.into()),
        Err(e) => {
            log::error!("could not read jstring: {e}");
            None
        }
    }
}

fn jstring_to_path<'a>(env: &mut JNIEnv<'a>, value: JString<'a>) -> Option<PathBuf> {
    jstring_to_string(env, value).map(PathBuf::from)
}

fn new_jstring(env: &JNIEnv<'_>, body: impl AsRef<str>) -> jstring {
    match env.new_string(body.as_ref()) {
        Ok(s) => s.into_raw(),
        Err(e) => {
            log::error!("could not allocate JString: {e}");
            std::ptr::null_mut()
        }
    }
}

fn error_json(msg: &str) -> String {
    let escaped = msg
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("{{\"ok\":false,\"error\":\"{escaped}\"}}")
}

const B64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Tiny standalone base64 encoder so we don't need to add a `base64`
/// dep just to ship the framebuffer over JNI.
fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= input.len() {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8) | input[i + 2] as u32;
        out.push(B64_TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64_TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64_TABLE[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = input.len() - i;
    if rem == 1 {
        let n = (input[i] as u32) << 16;
        out.push(B64_TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((input[i] as u32) << 16) | ((input[i + 1] as u32) << 8);
        out.push(B64_TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64_TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

fn init_logger() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("PocketHLE"),
        );
    });
}

// ---------------------------------------------------------------------------
// Session-based emulator API
//
// `runGame` above is single-shot: it blocks until the emulator has run to
// completion and only then returns the *final* framebuffer. With the trace
// stub backend that is fast enough to look reasonable, but with the real
// Unicorn backend the run lasts long enough that the user's UI sits on a
// loading spinner for tens of seconds and they cannot push any input. The
// methods below replace that with a streaming model:
//
//  * `nativeStartGame` returns an opaque `jlong` handle (a pointer to a
//    boxed [`Session`]).
//  * `nativePollFrame` returns the latest framebuffer that the worker has
//    produced since the last poll, as a flat `byte[]` with an 8-byte
//    little-endian header `(width: u32, height: u32)` followed by RGBA
//    pixels. Returning `null` means "no new frame", returning a zero-length
//    array means "session already ended".
//  * `nativeSendInput` pushes a virtual-key or stylus event into the
//    kernel's pending-input queue.
//  * `nativeRequestStop` asks the worker to stop at the next slice
//    boundary.
//  * `nativeFinishGame` joins the worker and frees the session, returning
//    a textual summary that the activity shows in its status panel.
//
// The pointer is opaque to Kotlin and is consumed exactly once by
// `nativeFinishGame`. After that call the handle is invalid and Kotlin must
// not pass it back into any of the methods above.

/// `kind` ordinals exchanged with Kotlin. See `NativeBridge.kt`.
const INPUT_KIND_KEY_DOWN: jint = 0;
const INPUT_KIND_KEY_UP: jint = 1;
const INPUT_KIND_POINTER_DOWN: jint = 2;
const INPUT_KIND_POINTER_UP: jint = 3;

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_nativeStartGame<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    library_root: JString<'local>,
    id: JString<'local>,
) -> jlong {
    init_logger();
    let root = match jstring_to_path(&mut env, library_root) {
        Some(p) => p,
        None => {
            log::error!("nativeStartGame: missing library root");
            return 0;
        }
    };
    let id = match jstring_to_string(&mut env, id) {
        Some(s) => s,
        None => {
            log::error!("nativeStartGame: missing game id");
            return 0;
        }
    };
    match runner::start(root, id) {
        Ok(session) => Box::into_raw(Box::new(session)) as jlong,
        Err(e) => {
            log::error!("nativeStartGame: {e:#}");
            0
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_nativePollFrame<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jbyteArray {
    let Some(session) = session_from_handle(handle) else {
        return std::ptr::null_mut();
    };
    let Some(frame) = session.poll_frame() else {
        // No new frame this tick — return null to let Kotlin keep
        // showing the previous content of the SurfaceView.
        return std::ptr::null_mut();
    };
    let mut bytes = Vec::with_capacity(8 + frame.rgba.len());
    bytes.extend_from_slice(&frame.width.to_le_bytes());
    bytes.extend_from_slice(&frame.height.to_le_bytes());
    bytes.extend_from_slice(&frame.rgba);
    new_jbyte_array(&env, &bytes)
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_nativeIsRunning<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jint {
    match session_from_handle(handle) {
        Some(session) if session.is_running() => 1,
        _ => 0,
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_nativeSendInput<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
    kind: jint,
    a: jint,
    b: jint,
) -> jint {
    let Some(session) = session_from_handle(handle) else {
        return 0;
    };
    let event = match kind {
        INPUT_KIND_KEY_DOWN => InputEvent::KeyDown { vk: a as u16 },
        INPUT_KIND_KEY_UP => InputEvent::KeyUp { vk: a as u16 },
        INPUT_KIND_POINTER_DOWN => InputEvent::PointerDown {
            x: clamp_u16(a),
            y: clamp_u16(b),
        },
        INPUT_KIND_POINTER_UP => InputEvent::PointerUp {
            x: clamp_u16(a),
            y: clamp_u16(b),
        },
        other => {
            log::warn!("nativeSendInput: unknown kind {other}");
            return 0;
        }
    };
    session.send_input(InputCommand::Input(event));
    1
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_nativeRequestStop<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) {
    if let Some(session) = session_from_handle(handle) {
        session.request_stop();
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pockethle_app_NativeBridge_nativeFinishGame<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    if handle == 0 {
        return new_jstring(&env, "(no session)");
    }
    // Reclaim ownership and join. The handle is now invalid for
    // any subsequent call from Kotlin.
    let session = unsafe { Box::from_raw(handle as *mut Session) };
    let summary = session.finish();
    new_jstring(&env, summary)
}

/// Borrow the session pointer without consuming it.
fn session_from_handle<'a>(handle: jlong) -> Option<&'a Session> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle was produced by `Box::into_raw` in
    // `nativeStartGame` and is freed exactly once by
    // `nativeFinishGame`. Kotlin guarantees the lifetime by treating
    // the handle as opaque and never passing a freed value back in.
    Some(unsafe { &*(handle as *const Session) })
}

fn clamp_u16(v: jint) -> u16 {
    v.clamp(0, u16::MAX as jint) as u16
}

fn new_jbyte_array(env: &JNIEnv<'_>, bytes: &[u8]) -> jbyteArray {
    match env.byte_array_from_slice(bytes) {
        Ok(arr) => arr.into_raw(),
        Err(e) => {
            log::error!("could not allocate jbyteArray: {e}");
            std::ptr::null_mut()
        }
    }
}

#[allow(dead_code)]
fn _unused_jbytearray_type_check(_a: JByteArray<'_>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_examples() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
