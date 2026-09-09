//! OpenGL ES 1.x / EGL 1.0 dispatch for `libGLES_CM.dll` and `libGLES_CL.dll`.
//!
//! Every entry point is registered by name; the ordinal aliases are wired
//! in [`register`] by iterating the ordinal tables, mirroring what
//! [`super::WinCeDispatcher::new`] does for `coredll.dll`.

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pocket_gles::context::{Context, GuestMemory};
use pocket_gles::fixed::{word_to_f32, word_to_f32_bits};
use pocket_gles::matrix::Matrix4;
use pocket_gles::ordinals as gles_ord;
use pocket_gles::{
    EGL_ALPHA_SIZE, EGL_BAD_DISPLAY, EGL_BLUE_SIZE, EGL_BUFFER_SIZE, EGL_CONFIG_ID, EGL_DEPTH_SIZE,
    EGL_EXTENSIONS, EGL_FALSE, EGL_GREEN_SIZE, EGL_NONE, EGL_NO_SURFACE, EGL_RED_SIZE,
    EGL_RENDERABLE_TYPE, EGL_SUCCESS, EGL_SURFACE_TYPE, EGL_TRUE, EGL_VENDOR, EGL_VERSION,
    EGL_WINDOW_BIT, GL_UNSIGNED_BYTE,
};

use pocket_kernel::{DispatchOutcome, KernelError};

use crate::{CallCtx, WinCeDispatcher};

// ---- global context --------------------------------------------------------

static CTX: Lazy<Mutex<Context>> = Lazy::new(|| Mutex::new(Context::new(240, 320)));

/// Compressed formats we decode, reported through
/// `GL_COMPRESSED_TEXTURE_FORMATS` and the extension string.
const COMPRESSED_FORMATS: [u32; 15] = [
    pocket_gles::GL_ATC_RGB_AMD,
    pocket_gles::GL_ATC_RGBA_EXPLICIT_ALPHA_AMD,
    pocket_gles::GL_ATC_RGBA_INTERPOLATED_ALPHA_AMD,
    pocket_gles::GL_COMPRESSED_RGB_S3TC_DXT1_EXT,
    pocket_gles::GL_COMPRESSED_RGBA_S3TC_DXT1_EXT,
    pocket_gles::GL_PALETTE4_RGB8_OES,
    pocket_gles::GL_PALETTE4_RGBA8_OES,
    pocket_gles::GL_PALETTE4_R5_G6_B5_OES,
    pocket_gles::GL_PALETTE4_RGBA4_OES,
    pocket_gles::GL_PALETTE4_RGB5_A1_OES,
    pocket_gles::GL_PALETTE8_RGB8_OES,
    pocket_gles::GL_PALETTE8_RGBA8_OES,
    pocket_gles::GL_PALETTE8_R5_G6_B5_OES,
    pocket_gles::GL_PALETTE8_RGBA4_OES,
    pocket_gles::GL_PALETTE8_RGB5_A1_OES,
];

// ---- draw census (diagnostic) ----------------------------------------------
//
// Aggregated per-draw state, sampled at the dispatch layer. It records
// what a draw was given (bound texture, whether that name holds data,
// the texture environment, the blend state, and the range the texture
// coordinates actually span) so a frame can be read back from the log
// without a debugger.
//
// `raster::raster_clipped` keeps the interpolated vertex colour when
// the sampler returns `None`, and `apply_tex_env`'s `Add` arm
// saturates, so both "no texture data" and "additive pass over a bright
// base" end in flat bright geometry. The buckets tell them apart.
//
// Environment:
//   POCKETHLE_GLES_CENSUS=0        turn the whole thing off
//   POCKETHLE_GLES_CENSUS_SECS=N   summary period, default 1
//   POCKETHLE_GLES_CENSUS_DEEP=1   also sample texture coordinates,
//                                  vertex colours and the texture
//                                  matrix per draw. Off by default:
//                                  it reads guest memory up to 64
//                                  times and takes the Context lock
//                                  on every draw call
//   POCKETHLE_GLES_SKIP_TEX=6,325  drop draws binding these names
//   POCKETHLE_GLES_SKIP_ENV=0x104  drop draws with these tex env modes
//   POCKETHLE_GLES_ENV_OVERRIDE=0x2100
//                                  force every texture environment mode
//                                  to this value, whatever the guest asks
//                                  for -- 0x2100 is MODULATE, 0x1E01 is
//                                  REPLACE, 0x0104 is ADD
//   POCKETHLE_GLES_ENV_ALL_UNITS=1
//                                  restore the old behaviour of letting
//                                  any texture unit's glTexEnv overwrite
//                                  the one environment mode Context
//                                  holds. Off by default: see below
//
// The skip lists exist to identify geometry visually: drop a name, see
// what vanishes from the screen. They force the census on.

const CENSUS_GL_TEXTURE_2D: u32 = 0x0DE1;
const CENSUS_GL_BLEND: u32 = 0x0BE2;
const CENSUS_GL_VERTEX_ARRAY: u32 = 0x8074;
const CENSUS_GL_COLOR_ARRAY: u32 = 0x8076;
const CENSUS_GL_TEXTURE_COORD_ARRAY: u32 = 0x8078;
const CENSUS_GL_TEXTURE_ENV_MODE: u32 = 0x2200;

/// Vertices sampled per draw when measuring the texture coordinate
/// range. A range only needs a handful of points; decoding tens of
/// thousands of vertices per frame would cost more than the draw.
const CENSUS_TC_SAMPLES: u32 = 32;

/// What we know about one texture name, recorded at upload time.
#[derive(Clone, Copy, Default)]
struct TexRecord {
    levels: u32,
    width: u32,
    height: u32,
    format: u32,
    ty: u32,
    bytes: usize,
    compressed: bool,
}

/// The state a draw call is bucketed by. Two draws with the same key
/// are indistinguishable as far as this diagnostic is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct DrawKey {
    tex_enabled: bool,
    name: u32,
    has_data: bool,
    tc_enabled: bool,
    tc_type: u32,
    tc_size: u32,
    color_enabled: bool,
    col_type: u32,
    col_size: u32,
    unit: u32,
    env_mode: u32,
    blend: bool,
    blend_src: u32,
    blend_dst: u32,
}

/// Accumulated per bucket over one reporting period.
#[derive(Clone, Copy)]
struct DrawStats {
    calls: u64,
    verts: u64,
    ranged: bool,
    smin: f32,
    smax: f32,
    tmin: f32,
    tmax: f32,
    /// Whether any vertex colour was decoded for this bucket.
    colored: bool,
    /// Per-channel minimum and maximum of the vertex colour array, as
    /// the guest wrote it -- raw and undivided, so a `GL_UNSIGNED_BYTE`
    /// array reads back 0..255 rather than 0..1.
    cmin: [f32; 4],
    cmax: [f32; 4],
    /// Texture matrix scale and translation, last seen. Column-major,
    /// so these are m[0], m[5], m[12], m[13].
    mat: [f32; 4],
}

impl Default for DrawStats {
    fn default() -> Self {
        DrawStats {
            calls: 0,
            verts: 0,
            ranged: false,
            smin: f32::MAX,
            smax: f32::MIN,
            tmin: f32::MAX,
            tmax: f32::MIN,
            colored: false,
            cmin: [f32::MAX; 4],
            cmax: [f32::MIN; 4],
            mat: [1.0, 1.0, 0.0, 0.0],
        }
    }
}

/// The texture coordinate array as the guest last described it.
#[derive(Clone, Copy)]
struct TcArray {
    enabled: bool,
    size: u32,
    ty: u32,
    stride: u32,
    ptr: u32,
}

struct Census {
    on: bool,
    period: Duration,
    last: Instant,
    active_unit: u32,
    bound: [u32; 2],
    tex_enabled: bool,
    blend: bool,
    blend_src: u32,
    blend_dst: u32,
    color_array: bool,
    texcoord_array: bool,
    vertex_array: bool,
    tc_size: u32,
    tc_type: u32,
    tc_stride: u32,
    tc_ptr: u32,
    col_size: u32,
    col_type: u32,
    col_stride: u32,
    col_ptr: u32,
    env_mode: u32,
    /// Environment mode as last set for each texture unit, kept apart
    /// so a second-unit configuration is visible rather than silently
    /// folded into the one mode the rasterizer reads.
    env_per_unit: [u32; 2],
    /// glActiveTexture calls seen per unit.
    activetex: [u64; 2],
    env_override: Option<u32>,
    env_unit0_only: bool,
    current_color: [f32; 4],
    skip_names: Vec<u32>,
    skip_envs: Vec<u32>,
    skipped: u64,
    textures: HashMap<u32, TexRecord>,
    draws: HashMap<DrawKey, DrawStats>,
    dropped: HashMap<(u32, u32), u64>,
}

/// Parse a comma-separated list of decimal or `0x`-prefixed values.
fn census_parse_list(var: &str) -> Vec<u32> {
    let Ok(raw) = std::env::var(var) else {
        return Vec::new();
    };
    raw.split(',')
        .filter_map(|item| {
            let item = item.trim();
            let stripped = item.strip_prefix("0x").or_else(|| item.strip_prefix("0X"));
            match stripped {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => item.parse::<u32>().ok(),
            }
        })
        .collect()
}

impl Census {
    fn new() -> Self {
        let skip_names = census_parse_list("POCKETHLE_GLES_SKIP_TEX");
        let skip_envs = census_parse_list("POCKETHLE_GLES_SKIP_ENV");
        let env_override = census_parse_list("POCKETHLE_GLES_ENV_OVERRIDE")
            .first()
            .copied();
        // GL keeps texture environment state per texture unit, but
        // `Context::tex_env` holds exactly one mode. A guest that
        // configures a second unit -- NFS Undercover alternates 0 and 1
        // on every object, setting MODULATE on unit 0 and ADD on unit 1
        // for car bodies -- would otherwise hand the second unit's mode
        // to the first, and `apply_tex_env`'s Add arm saturates the
        // result to white. We advertise GL_MAX_TEXTURE_UNITS as 1, so a
        // second unit's environment cannot affect what we draw and must
        // not be forwarded.
        let env_unit0_only = std::env::var("POCKETHLE_GLES_ENV_ALL_UNITS")
            .map(|v| v == "0")
            .unwrap_or(true);
        // A skip list or an override is useless without the state
        // tracking behind it, so asking for one turns the census on.
        let forced = !skip_names.is_empty() || !skip_envs.is_empty() || env_override.is_some();
        let on = forced
            || std::env::var("POCKETHLE_GLES_CENSUS")
                .map(|v| v != "0")
                .unwrap_or(true);
        let secs = std::env::var("POCKETHLE_GLES_CENSUS_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(1);
        if !skip_names.is_empty() || !skip_envs.is_empty() {
            log::info!("GLES census skipping names {skip_names:?} env modes {skip_envs:?}");
        }
        if let Some(mode) = env_override {
            log::info!("GLES census forcing every texture env mode to 0x{mode:04x}");
        }
        log::info!(
            "GLES glTexEnv forwarding: {}",
            if env_unit0_only {
                "unit 0 only (per-unit state, GL_MAX_TEXTURE_UNITS is 1)"
            } else {
                "all units (POCKETHLE_GLES_ENV_ALL_UNITS)"
            }
        );
        Census {
            on,
            period: Duration::from_secs(secs),
            last: Instant::now(),
            active_unit: 0,
            bound: [0; 2],
            tex_enabled: false,
            blend: false,
            blend_src: 0,
            blend_dst: 0,
            color_array: false,
            texcoord_array: false,
            vertex_array: false,
            tc_size: 0,
            tc_type: 0,
            tc_stride: 0,
            tc_ptr: 0,
            col_size: 0,
            col_type: 0,
            col_stride: 0,
            col_ptr: 0,
            // GL's own default, so a guest that never calls glTexEnv
            // still reads as MODULATE here.
            env_mode: 0x2100,
            env_per_unit: [0x2100; 2],
            activetex: [0; 2],
            env_override,
            env_unit0_only,
            current_color: [1.0; 4],
            skip_names,
            skip_envs,
            skipped: 0,
            textures: HashMap::new(),
            draws: HashMap::new(),
            dropped: HashMap::new(),
        }
    }

    fn report(&mut self) {
        self.last = Instant::now();
        let mut buckets: Vec<(DrawKey, DrawStats)> = self.draws.drain().collect();
        buckets.sort_by_key(|a| std::cmp::Reverse(a.1.calls));

        let calls: u64 = buckets.iter().map(|b| b.1.calls).sum();
        let verts: u64 = buckets.iter().map(|b| b.1.verts).sum();
        let textured: u64 = buckets
            .iter()
            .filter(|b| b.0.tex_enabled && b.0.has_data)
            .map(|b| b.1.calls)
            .sum();
        let nodata: u64 = buckets
            .iter()
            .filter(|b| b.0.tex_enabled && !b.0.has_data)
            .map(|b| b.1.calls)
            .sum();
        let untextured: u64 = buckets
            .iter()
            .filter(|b| !b.0.tex_enabled)
            .map(|b| b.1.calls)
            .sum();
        let with_data = self.textures.values().filter(|r| r.bytes > 0).count();

        log::info!(
            "GLES DRAW CENSUS calls={calls} verts={verts} textured={textured} \
             texenabled_nodata={nodata} untextured={untextured} skipped={} names={} \
             names_with_data={} vertexarray={} color=[{:.2} {:.2} {:.2} {:.2}] \
             tcptr=0x{:08x} tcstride={} activetex=[{} {}] env0=0x{:04x} env1=0x{:04x}",
            self.skipped,
            self.textures.len(),
            with_data,
            self.vertex_array,
            self.current_color[0],
            self.current_color[1],
            self.current_color[2],
            self.current_color[3],
            self.tc_ptr,
            self.tc_stride,
            self.activetex[0],
            self.activetex[1],
            self.env_per_unit[0],
            self.env_per_unit[1],
        );
        self.skipped = 0;
        self.activetex = [0; 2];

        for (k, st) in buckets.iter().take(10) {
            let rec = self.textures.get(&k.name).copied().unwrap_or_default();
            let range = if st.ranged {
                format!(
                    "s=[{:.1}..{:.1}] t=[{:.1}..{:.1}]",
                    st.smin, st.smax, st.tmin, st.tmax
                )
            } else {
                "s=? t=?".to_string()
            };
            let colors = if st.colored {
                format!(
                    "col=[{:.0}..{:.0} {:.0}..{:.0} {:.0}..{:.0} {:.0}..{:.0}]",
                    st.cmin[0],
                    st.cmax[0],
                    st.cmin[1],
                    st.cmax[1],
                    st.cmin[2],
                    st.cmax[2],
                    st.cmin[3],
                    st.cmax[3],
                )
            } else {
                "col=?".to_string()
            };
            log::info!(
                "  bucket calls={} verts={} tex2d={} name={} data={} {}x{} levels={} \
                 fmt=0x{:04x} ty=0x{:04x} cmp={} tcarray={} tctype=0x{:04x} tcsize={} \
                 colorarray={} coltype=0x{:04x} colsize={} unit={} env=0x{:04x} blend={} \
                 src=0x{:04x} dst=0x{:04x} {} {} texmat=[{:.4} {:.4} {:.4} {:.4}]",
                st.calls,
                st.verts,
                k.tex_enabled,
                k.name,
                k.has_data,
                rec.width,
                rec.height,
                rec.levels,
                rec.format,
                rec.ty,
                rec.compressed,
                k.tc_enabled,
                k.tc_type,
                k.tc_size,
                k.color_enabled,
                k.col_type,
                k.col_size,
                k.unit,
                k.env_mode,
                k.blend,
                k.blend_src,
                k.blend_dst,
                range,
                colors,
                st.mat[0],
                st.mat[1],
                st.mat[2],
                st.mat[3],
            );
        }

        let mut missing: Vec<u32> = buckets
            .iter()
            .filter(|b| b.0.tex_enabled && !b.0.has_data)
            .map(|b| b.0.name)
            .collect();
        missing.sort_unstable();
        missing.dedup();
        if !missing.is_empty() {
            let shown = missing.len().min(16);
            log::info!(
                "  NO TEXTURE DATA: {} name(s) bound with texturing on, first {}: {:?}",
                missing.len(),
                shown,
                &missing[..shown],
            );
        }

        if !self.dropped.is_empty() {
            let mut d: Vec<((u32, u32), u64)> =
                self.dropped.iter().map(|(k, v)| (*k, *v)).collect();
            d.sort_by_key(|a| std::cmp::Reverse(a.1));
            for ((f, t), n) in d.iter().take(6) {
                log::info!(
                    "  DROPPED UPLOAD format=0x{f:04x} type=0x{t:04x} count={n} \
                     (texel_bytes rejected the pair; the texture kept no data)"
                );
            }
        }
    }
}

static CENSUS: Lazy<Mutex<Census>> = Lazy::new(|| Mutex::new(Census::new()));

/// Whether to sample per-draw texcoord and colour ranges. Read once,
/// so the draw path costs an atomic load rather than a mutex.
static CENSUS_DEEP: Lazy<bool> = Lazy::new(|| {
    std::env::var("POCKETHLE_GLES_CENSUS_DEEP")
        .map(|v| v != "0")
        .unwrap_or(false)
});

/// Run `f` against the census, unless it is switched off. Never called
/// from inside a [`with_ctx`] closure -- the two locks are always taken
/// one after the other, never nested.
fn with_census(f: impl FnOnce(&mut Census)) {
    let mut guard = match CENSUS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if guard.on {
        f(&mut guard);
    }
}

/// Like [`with_census`], but runs even when reporting is switched off.
/// For the handful of hooks that change what gets drawn rather than
/// only what gets logged.
fn with_census_always(f: impl FnOnce(&mut Census)) {
    let mut guard = match CENSUS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    f(&mut guard);
}

fn census_enable(cap: u32, on: bool) {
    with_census(|c| match cap {
        CENSUS_GL_TEXTURE_2D => c.tex_enabled = on,
        CENSUS_GL_BLEND => c.blend = on,
        _ => {}
    });
}

fn census_blend_func(src: u32, dst: u32) {
    with_census(|c| {
        c.blend_src = src;
        c.blend_dst = dst;
    });
}

fn census_client_state(array: u32, on: bool) {
    with_census(|c| match array {
        CENSUS_GL_COLOR_ARRAY => c.color_array = on,
        CENSUS_GL_TEXTURE_COORD_ARRAY => c.texcoord_array = on,
        CENSUS_GL_VERTEX_ARRAY => c.vertex_array = on,
        _ => {}
    });
}

fn census_active_unit(unit: u32) {
    with_census_always(|c| {
        let unit = unit.min(1);
        c.active_unit = unit;
        c.activetex[unit as usize] += 1;
    });
}

fn census_bind(target: u32, name: u32) {
    with_census(|c| {
        if target == CENSUS_GL_TEXTURE_2D {
            let unit = c.active_unit as usize;
            c.bound[unit] = name;
        }
    });
}

/// Record an upload against whatever name is bound right now. `wanted`
/// is true when the guest passed a non-null pixel pointer, so a zero
/// `bytes` with `wanted` set is a silently discarded upload.
#[allow(clippy::too_many_arguments)]
fn census_upload(
    level: i32,
    width: u32,
    height: u32,
    format: u32,
    ty: u32,
    bytes: usize,
    wanted: bool,
    compressed: bool,
) {
    with_census(|c| {
        let name = c.bound[c.active_unit as usize];
        {
            let rec = c.textures.entry(name).or_default();
            rec.levels += 1;
            if level == 0 {
                rec.width = width;
                rec.height = height;
                rec.format = format;
                rec.ty = ty;
                rec.compressed = compressed;
                rec.bytes = bytes;
            } else if bytes > 0 {
                rec.bytes += bytes;
            }
        }
        if wanted && bytes == 0 {
            *c.dropped.entry((format, ty)).or_insert(0) += 1;
        }
    });
}

fn census_sub_upload(bytes: usize) {
    with_census(|c| {
        let name = c.bound[c.active_unit as usize];
        let rec = c.textures.entry(name).or_default();
        rec.bytes += bytes;
    });
}

fn census_delete(names: &[u32]) {
    with_census(|c| {
        for n in names {
            c.textures.remove(n);
        }
    });
}

fn census_color_pointer(size: u32, ty: u32, stride: u32, ptr: u32) {
    with_census(|c| {
        c.col_size = size;
        c.col_type = ty;
        c.col_stride = stride;
        c.col_ptr = ptr;
    });
}

/// Rewrite a texture environment mode when an override is configured.
/// Returns the value the caller should actually apply, so the override
/// reaches `Context` and not just the census.
fn census_env_override(pname: u32, value: u32) -> u32 {
    if pname != CENSUS_GL_TEXTURE_ENV_MODE {
        return value;
    }
    let mut out = value;
    with_census(|c| {
        if let Some(mode) = c.env_override {
            out = mode;
        }
    });
    out
}

fn census_texcoord_pointer(size: u32, ty: u32, stride: u32, ptr: u32) {
    with_census(|c| {
        c.tc_size = size;
        c.tc_type = ty;
        c.tc_stride = stride;
        c.tc_ptr = ptr;
    });
}

fn census_tex_env(pname: u32, value: u32) {
    with_census(|c| {
        if pname == CENSUS_GL_TEXTURE_ENV_MODE {
            let unit = c.active_unit as usize;
            c.env_per_unit[unit] = value;
            // Mirror what `Context` does: one mode, last writer wins,
            // whichever unit was active. The per-unit array above is
            // the ground truth to compare it against.
            if !c.env_unit0_only || unit == 0 {
                c.env_mode = value;
            }
        }
    });
}

/// Whether a glTexEnv call should reach `Context` at all. With the
/// unit-0-only gate on, a call made while a second unit is active is
/// recorded but not applied, since the rasterizer has a single unit and
/// would otherwise take the second unit's mode as its own.
fn census_env_forward() -> bool {
    let mut forward = true;
    with_census_always(|c| {
        if c.env_unit0_only && c.active_unit != 0 {
            forward = false;
        }
    });
    forward
}

fn census_color(rgba: [f32; 4]) {
    with_census(|c| c.current_color = rgba);
}

/// True when the current bound name or texture environment is on a skip
/// list, meaning the caller should drop this draw entirely.
fn census_should_skip() -> bool {
    let mut skip = false;
    with_census(|c| {
        if c.skip_names.is_empty() && c.skip_envs.is_empty() {
            return;
        }
        let name = c.bound[c.active_unit as usize];
        if c.skip_names.contains(&name) || c.skip_envs.contains(&c.env_mode) {
            c.skipped += 1;
            skip = true;
        }
    });
    skip
}

/// Snapshot of the texture coordinate array, taken without holding the
/// census lock across guest memory reads.
fn census_tc_array() -> TcArray {
    let mut out = TcArray {
        enabled: false,
        size: 0,
        ty: 0,
        stride: 0,
        ptr: 0,
    };
    with_census(|c| {
        out = TcArray {
            enabled: c.texcoord_array,
            size: c.tc_size,
            ty: c.tc_type,
            stride: c.tc_stride,
            ptr: c.tc_ptr,
        };
    });
    out
}

/// The texture matrix scale and translation, for reading alongside the
/// texcoord range: integer texture coordinates only make sense once
/// scaled, and this says by how much.
fn census_texture_matrix() -> [f32; 4] {
    let m = with_ctx(|c| c.texture_matrix.current().to_vec());
    if m.len() < 16 {
        return [1.0, 1.0, 0.0, 0.0];
    }
    [m[0], m[5], m[12], m[13]]
}

/// Bytes in one texture coordinate component.
fn census_tc_component(ty: u32) -> Option<u32> {
    Some(match ty {
        0x1400 | 0x1401 => 1, // GL_BYTE, GL_UNSIGNED_BYTE
        0x1402 | 0x1403 => 2, // GL_SHORT, GL_UNSIGNED_SHORT
        0x1406 | 0x140C => 4, // GL_FLOAT, GL_FIXED
        _ => return None,
    })
}

/// Decode one component exactly as the guest wrote it -- no
/// normalisation, so an integer array reads back as the integers the
/// game actually supplied.
fn census_decode_tc(ty: u32, raw: &[u8]) -> f32 {
    match ty {
        0x1400 => raw[0] as i8 as f32,
        0x1401 => raw[0] as f32,
        0x1402 => i16::from_le_bytes([raw[0], raw[1]]) as f32,
        0x1403 => u16::from_le_bytes([raw[0], raw[1]]) as f32,
        0x1406 => f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
        0x140C => i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f32 / 65536.0,
        _ => 0.0,
    }
}

/// Measure the S and T range spanned by `indices` into the texture
/// coordinate array. Returns `None` when there is no client array to
/// read -- a buffer-object binding, a null pointer, or a type we do not
/// decode.
fn census_texcoord_range(
    cpu: &mut dyn pocket_cpu::Cpu,
    indices: &[u32],
) -> Option<(f32, f32, f32, f32)> {
    let a = census_tc_array();
    // A vertex-buffer binding leaves a small offset here rather than an
    // address, and there is no way to reach the buffer store from this
    // layer, so only client arrays are measured.
    if !a.enabled || a.ptr < 0x1000 || a.size < 2 {
        return None;
    }
    let comp = census_tc_component(a.ty)?;
    let stride = if a.stride == 0 {
        comp.checked_mul(a.size)?
    } else {
        a.stride
    };
    let (mut smin, mut smax) = (f32::MAX, f32::MIN);
    let (mut tmin, mut tmax) = (f32::MAX, f32::MIN);
    let mut seen = 0u32;
    for &i in indices {
        let Some(offset) = i.checked_mul(stride) else {
            continue;
        };
        let Some(addr) = a.ptr.checked_add(offset) else {
            continue;
        };
        let Ok(raw) = cpu.read_mem(addr, comp * 2) else {
            continue;
        };
        let s = census_decode_tc(a.ty, &raw[..comp as usize]);
        let t = census_decode_tc(a.ty, &raw[comp as usize..]);
        if !s.is_finite() || !t.is_finite() {
            continue;
        }
        smin = smin.min(s);
        smax = smax.max(s);
        tmin = tmin.min(t);
        tmax = tmax.max(t);
        seen += 1;
    }
    if seen == 0 {
        None
    } else {
        Some((smin, smax, tmin, tmax))
    }
}

/// Measure the range each vertex colour channel spans over `indices`.
/// Reported raw, exactly as the guest wrote it: this is here to answer
/// whether the colour array feeding `apply_tex_env` holds real shading
/// or has collapsed to white, and a normalised figure would hide the
/// difference between a decode bug and genuine white geometry.
fn census_color_range(
    cpu: &mut dyn pocket_cpu::Cpu,
    indices: &[u32],
) -> Option<([f32; 4], [f32; 4])> {
    let (enabled, size, ty, stride, ptr) = {
        let mut out = (false, 0u32, 0u32, 0u32, 0u32);
        with_census(|c| {
            out = (
                c.color_array,
                c.col_size,
                c.col_type,
                c.col_stride,
                c.col_ptr,
            );
        });
        out
    };
    if !enabled || ptr < 0x1000 || size == 0 || size > 4 {
        return None;
    }
    let comp = census_tc_component(ty)?;
    let stride = if stride == 0 {
        comp.checked_mul(size)?
    } else {
        stride
    };
    let mut cmin = [f32::MAX; 4];
    let mut cmax = [f32::MIN; 4];
    let mut seen = 0u32;
    for &i in indices {
        let Some(offset) = i.checked_mul(stride) else {
            continue;
        };
        let Some(addr) = ptr.checked_add(offset) else {
            continue;
        };
        let Ok(raw) = cpu.read_mem(addr, comp * size) else {
            continue;
        };
        for ch in 0..size as usize {
            let at = ch * comp as usize;
            let v = census_decode_tc(ty, &raw[at..]);
            if !v.is_finite() {
                continue;
            }
            cmin[ch] = cmin[ch].min(v);
            cmax[ch] = cmax[ch].max(v);
        }
        seen += 1;
    }
    if seen == 0 {
        None
    } else {
        Some((cmin, cmax))
    }
}

/// Up to [`CENSUS_TC_SAMPLES`] evenly spaced vertex indices from a
/// `glDrawArrays` range.
fn census_sample_range(first: u32, count: u32) -> Vec<u32> {
    if count == 0 {
        return Vec::new();
    }
    let step = (count / CENSUS_TC_SAMPLES).max(1);
    let mut out = Vec::new();
    let mut i = 0;
    while i < count {
        out.push(first.saturating_add(i));
        i += step;
    }
    out
}

/// Up to [`CENSUS_TC_SAMPLES`] indices read out of a `glDrawElements`
/// index array.
fn census_sample_elements(
    cpu: &mut dyn pocket_cpu::Cpu,
    count: u32,
    ty: u32,
    ptr: u32,
) -> Vec<u32> {
    if count == 0 || ptr < 0x1000 {
        return Vec::new();
    }
    let width = match ty {
        0x1401 => 1u32, // GL_UNSIGNED_BYTE
        0x1403 => 2u32, // GL_UNSIGNED_SHORT
        _ => return Vec::new(),
    };
    let step = (count / CENSUS_TC_SAMPLES).max(1);
    let mut out = Vec::new();
    let mut i = 0;
    while i < count {
        let Some(offset) = i.checked_mul(width) else {
            break;
        };
        let Some(addr) = ptr.checked_add(offset) else {
            break;
        };
        let Ok(raw) = cpu.read_mem(addr, width) else {
            break;
        };
        out.push(if width == 1 {
            raw[0] as u32
        } else {
            u16::from_le_bytes([raw[0], raw[1]]) as u32
        });
        i += step;
    }
    out
}

fn census_draw(
    verts: u32,
    range: Option<(f32, f32, f32, f32)>,
    colors: Option<([f32; 4], [f32; 4])>,
    mat: [f32; 4],
) {
    with_census(|c| {
        let name = c.bound[c.active_unit as usize];
        let has_data = c.textures.get(&name).map(|r| r.bytes > 0).unwrap_or(false);
        let key = DrawKey {
            tex_enabled: c.tex_enabled,
            name,
            has_data,
            tc_enabled: c.texcoord_array,
            tc_type: c.tc_type,
            tc_size: c.tc_size,
            color_enabled: c.color_array,
            col_type: c.col_type,
            col_size: c.col_size,
            unit: c.active_unit,
            env_mode: c.env_mode,
            blend: c.blend,
            blend_src: c.blend_src,
            blend_dst: c.blend_dst,
        };
        let entry = c.draws.entry(key).or_default();
        entry.calls += 1;
        entry.verts += verts as u64;
        entry.mat = mat;
        if let Some((smin, smax, tmin, tmax)) = range {
            entry.ranged = true;
            entry.smin = entry.smin.min(smin);
            entry.smax = entry.smax.max(smax);
            entry.tmin = entry.tmin.min(tmin);
            entry.tmax = entry.tmax.max(tmax);
        }
        if let Some((cmin, cmax)) = colors {
            entry.colored = true;
            for ch in 0..4 {
                entry.cmin[ch] = entry.cmin[ch].min(cmin[ch]);
                entry.cmax[ch] = entry.cmax[ch].max(cmax[ch]);
            }
        }
        if c.last.elapsed() >= c.period {
            c.report();
        }
    });
}

/// Thin wrapper so `pocket_cpu::Cpu` satisfies `GuestMemory`.
struct CpuMem<'a>(&'a mut dyn pocket_cpu::Cpu);

impl GuestMemory for CpuMem<'_> {
    fn read(&mut self, addr: u32, len: usize, out: &mut Vec<u8>) -> bool {
        match self.0.read_mem(addr, len as u32) {
            Ok(v) => {
                out.clear();
                out.extend_from_slice(&v);
                true
            }
            Err(_) => false,
        }
    }
}

// ---- helpers ---------------------------------------------------------------

/// Read 16 consecutive words at `ptr` and decode each with `decode`.
/// The layout is already OpenGL column-major, so no transpose.
fn read_matrix(cpu: &mut dyn pocket_cpu::Cpu, ptr: u32, decode: fn(u32) -> f32) -> Option<Matrix4> {
    let b = cpu.read_mem(ptr, 64).ok()?;
    let mut m = [0f32; 16];
    for (i, c) in b.chunks_exact(4).enumerate() {
        m[i] = decode(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    }
    Some(m)
}

/// A GL float argument arrives in a core register as its raw IEEE-754
/// bit pattern (the guest is soft-float ARM, so every `GLfloat` is
/// passed in an integer register).
#[inline]
fn argf(ctx: &mut CallCtx<'_>, idx: u8) -> Result<f32, KernelError> {
    Ok(word_to_f32_bits(ctx.arg_u32(idx)?))
}

/// A `GLfixed` argument: 16.16 in a core register.
#[inline]
fn argx(ctx: &mut CallCtx<'_>, idx: u8) -> Result<f32, KernelError> {
    Ok(word_to_f32(ctx.arg_u32(idx)?))
}

/// Every handler that returns nothing.
const VOID: DispatchOutcome = DispatchOutcome::ReturnedR0(0);

/// Run `f` against the global context. Poisoning cannot lose GL state
/// we care about (a panicking handler leaves the context merely
/// half-updated), so recover rather than propagate.
fn with_ctx<R>(f: impl FnOnce(&mut Context) -> R) -> R {
    let mut guard = match CTX.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    f(&mut guard)
}

// ---- transform -------------------------------------------------------------

fn gl_matrix_mode(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let mode = ctx.arg_u32(0)?;
    with_ctx(|c| c.set_matrix_mode(mode));
    Ok(VOID)
}

fn gl_load_identity(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    with_ctx(|c| c.load_identity());
    Ok(VOID)
}

fn gl_push_matrix(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    with_ctx(|c| c.push_matrix());
    Ok(VOID)
}

fn gl_pop_matrix(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    with_ctx(|c| c.pop_matrix());
    Ok(VOID)
}

fn gl_load_matrixf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    if let Some(m) = read_matrix(ctx.cpu, ptr, f32::from_bits) {
        with_ctx(|c| c.load_matrix(m));
    }
    Ok(VOID)
}

fn gl_load_matrixx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    if let Some(m) = read_matrix(ctx.cpu, ptr, word_to_f32) {
        with_ctx(|c| c.load_matrix(m));
    }
    Ok(VOID)
}

fn gl_mult_matrixf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    if let Some(m) = read_matrix(ctx.cpu, ptr, f32::from_bits) {
        with_ctx(|c| c.mult_matrix(m));
    }
    Ok(VOID)
}

fn gl_mult_matrixx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    if let Some(m) = read_matrix(ctx.cpu, ptr, word_to_f32) {
        with_ctx(|c| c.mult_matrix(m));
    }
    Ok(VOID)
}

fn gl_translatef(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (x, y, z) = (argf(ctx, 0)?, argf(ctx, 1)?, argf(ctx, 2)?);
    with_ctx(|c| c.translate(x, y, z));
    Ok(VOID)
}

fn gl_translatex(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (x, y, z) = (argx(ctx, 0)?, argx(ctx, 1)?, argx(ctx, 2)?);
    with_ctx(|c| c.translate(x, y, z));
    Ok(VOID)
}

fn gl_scalef(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (x, y, z) = (argf(ctx, 0)?, argf(ctx, 1)?, argf(ctx, 2)?);
    with_ctx(|c| c.scale(x, y, z));
    Ok(VOID)
}

fn gl_scalex(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (x, y, z) = (argx(ctx, 0)?, argx(ctx, 1)?, argx(ctx, 2)?);
    with_ctx(|c| c.scale(x, y, z));
    Ok(VOID)
}

fn gl_rotatef(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (a, x, y, z) = (argf(ctx, 0)?, argf(ctx, 1)?, argf(ctx, 2)?, argf(ctx, 3)?);
    with_ctx(|c| c.rotate(a, x, y, z));
    Ok(VOID)
}

fn gl_rotatex(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (a, x, y, z) = (argx(ctx, 0)?, argx(ctx, 1)?, argx(ctx, 2)?, argx(ctx, 3)?);
    with_ctx(|c| c.rotate(a, x, y, z));
    Ok(VOID)
}

fn gl_frustumf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let v = [
        argf(ctx, 0)?,
        argf(ctx, 1)?,
        argf(ctx, 2)?,
        argf(ctx, 3)?,
        argf(ctx, 4)?,
        argf(ctx, 5)?,
    ];
    with_ctx(|c| c.frustum(v[0], v[1], v[2], v[3], v[4], v[5]));
    Ok(VOID)
}

fn gl_frustumx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let v = [
        argx(ctx, 0)?,
        argx(ctx, 1)?,
        argx(ctx, 2)?,
        argx(ctx, 3)?,
        argx(ctx, 4)?,
        argx(ctx, 5)?,
    ];
    with_ctx(|c| c.frustum(v[0], v[1], v[2], v[3], v[4], v[5]));
    Ok(VOID)
}

fn gl_orthof(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let v = [
        argf(ctx, 0)?,
        argf(ctx, 1)?,
        argf(ctx, 2)?,
        argf(ctx, 3)?,
        argf(ctx, 4)?,
        argf(ctx, 5)?,
    ];
    with_ctx(|c| c.ortho(v[0], v[1], v[2], v[3], v[4], v[5]));
    Ok(VOID)
}

fn gl_orthox(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let v = [
        argx(ctx, 0)?,
        argx(ctx, 1)?,
        argx(ctx, 2)?,
        argx(ctx, 3)?,
        argx(ctx, 4)?,
        argx(ctx, 5)?,
    ];
    with_ctx(|c| c.ortho(v[0], v[1], v[2], v[3], v[4], v[5]));
    Ok(VOID)
}

// ---- enables and fragment state --------------------------------------------

fn gl_enable(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let cap = ctx.arg_u32(0)?;
    log::debug!("GLES glEnable(0x{cap:04x})");
    with_ctx(|c| c.set_capability(cap, true));
    census_enable(cap, true);
    Ok(VOID)
}

fn gl_disable(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let cap = ctx.arg_u32(0)?;
    log::debug!("GLES glDisable(0x{cap:04x})");
    with_ctx(|c| c.set_capability(cap, false));
    census_enable(cap, false);
    Ok(VOID)
}

fn gl_enable_client_state(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let array = ctx.arg_u32(0)?;
    with_ctx(|c| c.set_client_state(array, true));
    census_client_state(array, true);
    Ok(VOID)
}

fn gl_disable_client_state(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let array = ctx.arg_u32(0)?;
    with_ctx(|c| c.set_client_state(array, false));
    census_client_state(array, false);
    Ok(VOID)
}

fn gl_depth_func(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let f = ctx.arg_u32(0)?;
    with_ctx(|c| c.set_depth_func(f));
    Ok(VOID)
}

fn gl_depth_mask(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let on = ctx.arg_u32(0)? != 0;
    with_ctx(|c| c.state.depth_write = on);
    Ok(VOID)
}

fn gl_depth_rangef(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (n, f) = (argf(ctx, 0)?, argf(ctx, 1)?);
    with_ctx(|c| c.set_depth_range(n, f));
    Ok(VOID)
}

fn gl_depth_rangex(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (n, f) = (argx(ctx, 0)?, argx(ctx, 1)?);
    with_ctx(|c| c.set_depth_range(n, f));
    Ok(VOID)
}

fn gl_alpha_funcf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (func, r) = (ctx.arg_u32(0)?, argf(ctx, 1)?);
    with_ctx(|c| c.set_alpha_func(func, r));
    Ok(VOID)
}

fn gl_alpha_funcx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (func, r) = (ctx.arg_u32(0)?, argx(ctx, 1)?);
    with_ctx(|c| c.set_alpha_func(func, r));
    Ok(VOID)
}

fn gl_blend_func(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (s, d) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    with_ctx(|c| c.set_blend_func(s, d));
    census_blend_func(s, d);
    Ok(VOID)
}

fn gl_cull_face(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let m = ctx.arg_u32(0)?;
    with_ctx(|c| c.set_cull_face(m));
    Ok(VOID)
}

fn gl_front_face(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let m = ctx.arg_u32(0)?;
    with_ctx(|c| c.set_front_face(m));
    Ok(VOID)
}

fn gl_shade_model(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let m = ctx.arg_u32(0)?;
    with_ctx(|c| c.set_shade_model(m));
    Ok(VOID)
}

fn gl_color_mask(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let m = [
        ctx.arg_u32(0)? != 0,
        ctx.arg_u32(1)? != 0,
        ctx.arg_u32(2)? != 0,
        ctx.arg_u32(3)? != 0,
    ];
    with_ctx(|c| c.state.color_mask = m);
    Ok(VOID)
}

fn gl_viewport(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (x, y, w, h) = (
        ctx.arg_u32(0)? as i32,
        ctx.arg_u32(1)? as i32,
        ctx.arg_u32(2)? as i32,
        ctx.arg_u32(3)? as i32,
    );
    with_ctx(|c| c.set_viewport(x, y, w, h));
    Ok(VOID)
}

fn gl_scissor(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (x, y, w, h) = (
        ctx.arg_u32(0)? as i32,
        ctx.arg_u32(1)? as i32,
        ctx.arg_u32(2)? as i32,
        ctx.arg_u32(3)? as i32,
    );
    with_ctx(|c| c.set_scissor(x, y, w, h));
    Ok(VOID)
}

/// Decode one `glFog*` value word.
///
/// `GL_FOG_MODE` names an enum, and enums are never fixed-point. Running
/// `GL_LINEAR` (9729) through the 16.16 decode yields 0.148453, which
/// `set_fog` rightly rejects as an invalid enum — and because `glGetError`
/// reports the *first* error since the last check, that stray code then
/// fails whatever the guest was actually probing. The `f` entry points
/// need no exception: they already carry the enum as a float.
fn fog_value(pname: u32, word: u32, fixed: bool) -> f32 {
    match (fixed, pname) {
        (true, pocket_gles::GL_FOG_MODE) => word as f32,
        (true, _) => word_to_f32(word),
        (false, _) => word_to_f32_bits(word),
    }
}

fn gl_fogf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (p, w) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    let v = fog_value(p, w, false);
    with_ctx(|c| c.set_fog(p, v));
    Ok(VOID)
}

fn gl_fogx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (p, w) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    let v = fog_value(p, w, true);
    with_ctx(|c| c.set_fog(p, v));
    Ok(VOID)
}

/// `glFogfv` / `glFogxv`. Only `GL_FOG_COLOR` needs the vector form;
/// the scalar parameters are forwarded to `set_fog` from element 0.
fn fog_vector(ctx: &mut CallCtx<'_>, fixed: bool) -> Result<DispatchOutcome, KernelError> {
    let pname = ctx.arg_u32(0)?;
    let ptr = ctx.arg_u32(1)?;
    let count = if pname == pocket_gles::GL_FOG_COLOR {
        4
    } else {
        1
    };
    let Ok(bytes) = ctx.cpu.read_mem(ptr, count * 4) else {
        return Ok(VOID);
    };
    let mut v = [0f32; 4];
    for (i, c) in bytes.chunks_exact(4).enumerate() {
        v[i] = fog_value(pname, u32::from_le_bytes([c[0], c[1], c[2], c[3]]), fixed);
    }
    log::debug!("GLES fog vector pname=0x{pname:04x} ptr=0x{ptr:08x} values={v:?}",);
    with_ctx(|c| {
        if pname == pocket_gles::GL_FOG_COLOR {
            c.state.fog_color = v;
            log::debug!(
                "GLES fog state mode={:?} density={:.6} start={:.6} end={:.6} color={:?} enabled={}",
                c.state.fog_mode,
                c.state.fog_density,
                c.state.fog_start,
                c.state.fog_end,
                c.state.fog_color,
                c.state.fog,
            );
        } else {
            c.set_fog(pname, v[0]);
        }
    });
    Ok(VOID)
}

fn gl_fogfv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    fog_vector(ctx, false)
}

fn gl_fogxv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    fog_vector(ctx, true)
}

// ---- clear and current values ----------------------------------------------

fn gl_clear(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let mask = ctx.arg_u32(0)?;
    with_ctx(|c| c.clear(mask));
    Ok(VOID)
}

fn gl_clear_colorf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let c = [argf(ctx, 0)?, argf(ctx, 1)?, argf(ctx, 2)?, argf(ctx, 3)?];
    with_ctx(|g| g.clear_color = c);
    Ok(VOID)
}

fn gl_clear_colorx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let c = [argx(ctx, 0)?, argx(ctx, 1)?, argx(ctx, 2)?, argx(ctx, 3)?];
    with_ctx(|g| g.clear_color = c);
    Ok(VOID)
}

fn gl_clear_depthf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let d = argf(ctx, 0)?;
    with_ctx(|c| c.clear_depth = d.clamp(0.0, 1.0));
    Ok(VOID)
}

fn gl_clear_depthx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let d = argx(ctx, 0)?;
    with_ctx(|c| c.clear_depth = d.clamp(0.0, 1.0));
    Ok(VOID)
}

fn gl_color4f(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let c = [argf(ctx, 0)?, argf(ctx, 1)?, argf(ctx, 2)?, argf(ctx, 3)?];
    with_ctx(|g| g.current_color = c);
    census_color(c);
    Ok(VOID)
}

fn gl_color4x(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let c = [argx(ctx, 0)?, argx(ctx, 1)?, argx(ctx, 2)?, argx(ctx, 3)?];
    with_ctx(|g| g.current_color = c);
    census_color(c);
    Ok(VOID)
}

fn gl_normal3f(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = [argf(ctx, 0)?, argf(ctx, 1)?, argf(ctx, 2)?];
    with_ctx(|c| c.current_normal = n);
    Ok(VOID)
}

fn gl_normal3x(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = [argx(ctx, 0)?, argx(ctx, 1)?, argx(ctx, 2)?];
    with_ctx(|c| c.current_normal = n);
    Ok(VOID)
}

/// `glMultiTexCoord4f` / `glMultiTexCoord4x`. Only the S and T
/// components reach the rasterizer; R and Q are consumed and dropped
/// because we have no 3D or projective texturing.
fn gl_multi_tex_coord4f(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (s, t) = (argf(ctx, 1)?, argf(ctx, 2)?);
    with_ctx(|c| c.set_multi_texcoord(ctx.arg_u32(0).unwrap_or(pocket_gles::GL_TEXTURE0), s, t));
    Ok(VOID)
}

fn gl_multi_tex_coord4x(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (s, t) = (argx(ctx, 1)?, argx(ctx, 2)?);
    with_ctx(|c| c.set_multi_texcoord(ctx.arg_u32(0).unwrap_or(pocket_gles::GL_TEXTURE0), s, t));
    Ok(VOID)
}

fn gl_active_texture(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let unit = ctx.arg_u32(0)?.saturating_sub(pocket_gles::GL_TEXTURE0);
    with_ctx(|c| c.set_active_texture(unit));
    census_active_unit(unit);
    Ok(VOID)
}

fn gl_client_active_texture(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let unit = ctx.arg_u32(0)?.saturating_sub(pocket_gles::GL_TEXTURE0);
    with_ctx(|c| c.set_client_active_texture(unit));
    Ok(VOID)
}

// ---- client arrays ---------------------------------------------------------

fn gl_vertex_pointer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (size, ty, stride, ptr) = (
        ctx.arg_u32(0)?,
        ctx.arg_u32(1)?,
        ctx.arg_u32(2)?,
        ctx.arg_u32(3)?,
    );
    // GL ES 1.1 captures the ARRAY_BUFFER binding here, not at draw
    // time, so each array remembers the VBO it was set against.
    with_ctx(|c| c.set_vertex_pointer(size, ty, stride, ptr));
    Ok(VOID)
}

fn gl_color_pointer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (size, ty, stride, ptr) = (
        ctx.arg_u32(0)?,
        ctx.arg_u32(1)?,
        ctx.arg_u32(2)?,
        ctx.arg_u32(3)?,
    );
    with_ctx(|c| c.set_color_pointer(size, ty, stride, ptr));
    census_color_pointer(size, ty, stride, ptr);
    Ok(VOID)
}

fn gl_tex_coord_pointer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (size, ty, stride, ptr) = (
        ctx.arg_u32(0)?,
        ctx.arg_u32(1)?,
        ctx.arg_u32(2)?,
        ctx.arg_u32(3)?,
    );
    log::debug!(
        "GLES glTexCoordPointer size={} type=0x{ty:04x} stride={} pointer=0x{ptr:08x}",
        size,
        stride,
    );
    with_ctx(|c| c.set_texcoord_pointer(size, ty, stride, ptr));
    census_texcoord_pointer(size, ty, stride, ptr);
    Ok(VOID)
}

/// `glNormalPointer(type, stride, pointer)` — no `size`, normals are
/// always 3-component.
fn gl_normal_pointer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (ty, stride, ptr) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?, ctx.arg_u32(2)?);
    with_ctx(|c| {
        c.set_normal_pointer(ty, stride, ptr);
    });
    Ok(VOID)
}

// ---- textures --------------------------------------------------------------

fn gl_gen_textures(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = ctx.arg_u32(0)? as i32;
    let out = ctx.arg_u32(1)?;
    if n <= 0 || out == 0 {
        return Ok(VOID);
    }
    let names = with_ctx(|c| c.gen_textures(n as usize));
    let bytes: Vec<u8> = names.iter().flat_map(|n| n.to_le_bytes()).collect();
    ctx.cpu.write_mem(out, &bytes)?;
    Ok(VOID)
}

fn gl_delete_textures(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = ctx.arg_u32(0)? as i32;
    let ptr = ctx.arg_u32(1)?;
    if n <= 0 || ptr == 0 {
        return Ok(VOID);
    }
    let bytes = ctx.cpu.read_mem(ptr, n as u32 * 4)?;
    let names: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    with_ctx(|c| c.delete_textures(&names));
    census_delete(&names);
    Ok(VOID)
}

fn gl_bind_texture(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (target, name) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    log::debug!("GLES glBindTexture(target=0x{target:04x}, name={name})");
    with_ctx(|c| c.bind_texture(target, name));
    census_bind(target, name);
    Ok(VOID)
}

// ---- buffer objects --------------------------------------------------------

fn gl_gen_buffers(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = ctx.arg_u32(0)? as i32;
    let out = ctx.arg_u32(1)?;
    if n <= 0 || out == 0 {
        return Ok(VOID);
    }
    let names = with_ctx(|c| c.gen_buffers(n as usize));
    let bytes: Vec<u8> = names.iter().flat_map(|n| n.to_le_bytes()).collect();
    ctx.cpu.write_mem(out, &bytes)?;
    Ok(VOID)
}

fn gl_delete_buffers(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let n = ctx.arg_u32(0)? as i32;
    let ptr = ctx.arg_u32(1)?;
    if n <= 0 || ptr == 0 {
        return Ok(VOID);
    }
    let bytes = ctx.cpu.read_mem(ptr, n as u32 * 4)?;
    let names: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    with_ctx(|c| c.delete_buffers(&names));
    Ok(VOID)
}

fn gl_bind_buffer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (target, name) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    with_ctx(|c| c.bind_buffer(target, name));
    Ok(VOID)
}

/// `glBufferData(target, size, data, usage)`. A null `data` allocates
/// the store without initialising it, which is legal and common — the
/// guest fills it with `glBufferSubData` afterwards.
fn gl_buffer_data(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let target = ctx.arg_u32(0)?;
    let size = ctx.arg_u32(1)? as i32;
    let data = ctx.arg_u32(2)?;
    let usage = ctx.arg_u32(3)?;
    if size < 0 {
        with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_VALUE));
        return Ok(VOID);
    }
    let bytes = if data != 0 {
        Some(ctx.cpu.read_mem(data, size as u32)?)
    } else {
        None
    };
    with_ctx(|c| c.buffer_data(target, size as usize, bytes.as_deref(), usage));
    Ok(VOID)
}

fn gl_buffer_sub_data(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let target = ctx.arg_u32(0)?;
    let offset = ctx.arg_u32(1)?;
    let size = ctx.arg_u32(2)? as i32;
    let data = ctx.arg_u32(3)?;
    if size <= 0 || data == 0 {
        return Ok(VOID);
    }
    let bytes = ctx.cpu.read_mem(data, size as u32)?;
    with_ctx(|c| c.buffer_sub_data(target, offset, &bytes));
    Ok(VOID)
}

/// Bytes one texel of `(format, type)` occupies in the guest's upload.
fn texel_bytes(format: u32, ty: u32) -> Option<u32> {
    Some(match ty {
        GL_UNSIGNED_BYTE => match format {
            pocket_gles::GL_ALPHA | pocket_gles::GL_LUMINANCE => 1,
            pocket_gles::GL_LUMINANCE_ALPHA => 2,
            pocket_gles::GL_RGB => 3,
            pocket_gles::GL_RGBA => 4,
            _ => return None,
        },
        pocket_gles::GL_UNSIGNED_SHORT_5_6_5
        | pocket_gles::GL_UNSIGNED_SHORT_4_4_4_4
        | pocket_gles::GL_UNSIGNED_SHORT_5_5_5_1 => 2,
        _ => return None,
    })
}

fn gl_tex_image_2d(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // (target, level, internalformat, width, height, border, format,
    //  type, pixels) — nine arguments, five of them on the stack.
    let target = ctx.arg_u32(0)?;
    let level = ctx.arg_u32(1)? as i32;
    let width = ctx.arg_u32(3)?;
    let height = ctx.arg_u32(4)?;
    let format = ctx.arg_u32(6)?;
    let ty = ctx.arg_u32(7)?;
    let pixels = ctx.arg_u32(8)?;
    let data = load_texels(ctx, pixels, width, height, format, ty)?;
    log::debug!(
        "GLES glTexImage2D level={level} {width}x{height} format=0x{format:04x} \
         type=0x{ty:04x} pixels=0x{pixels:08x} bytes={}",
        data.len()
    );
    with_ctx(|c| c.tex_image_2d(target, level, width, height, format, ty, &data));
    census_upload(
        level,
        width,
        height,
        format,
        ty,
        data.len(),
        pixels != 0,
        false,
    );
    Ok(VOID)
}

fn gl_tex_sub_image_2d(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // (target, level, xoffset, yoffset, width, height, format, type,
    //  pixels)
    let target = ctx.arg_u32(0)?;
    let level = ctx.arg_u32(1)? as i32;
    let xoff = ctx.arg_u32(2)?;
    let yoff = ctx.arg_u32(3)?;
    let width = ctx.arg_u32(4)?;
    let height = ctx.arg_u32(5)?;
    let format = ctx.arg_u32(6)?;
    let ty = ctx.arg_u32(7)?;
    let pixels = ctx.arg_u32(8)?;
    let data = load_texels(ctx, pixels, width, height, format, ty)?;
    if data.is_empty() {
        return Ok(VOID);
    }
    with_ctx(|c| c.tex_sub_image_2d(target, level, xoff, yoff, width, height, format, ty, &data));
    census_sub_upload(data.len());
    Ok(VOID)
}

/// Copy a `width × height` image of `(format, type)` texels out of guest
/// memory. A null pointer yields an empty vector, which
/// `Context::tex_image_2d` reads as "reserve storage, no data".
fn load_texels(
    ctx: &mut CallCtx<'_>,
    pixels: u32,
    width: u32,
    height: u32,
    format: u32,
    ty: u32,
) -> Result<Vec<u8>, KernelError> {
    if pixels == 0 {
        return Ok(Vec::new());
    }
    let Some(bpp) = texel_bytes(format, ty) else {
        return Ok(Vec::new());
    };
    let Some(len) = width.checked_mul(height).and_then(|p| p.checked_mul(bpp)) else {
        return Ok(Vec::new());
    };
    // An upload the guest has not actually mapped yet is a bug in the
    // guest, not a reason to stop the emulator — treat it as no data.
    Ok(ctx.cpu.read_mem(pixels, len).unwrap_or_default())
}

fn gl_tex_parameterf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (target, pname) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    // The filters and wrap modes are enums even in the float entry
    // point, so the guest passes them as a float-encoded enum value.
    let value = argf(ctx, 2)? as u32;
    with_ctx(|c| c.tex_parameter(target, pname, value));
    Ok(VOID)
}

fn gl_tex_parameterx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (target, pname, value) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?, ctx.arg_u32(2)?);
    with_ctx(|c| c.tex_parameter(target, pname, value));
    Ok(VOID)
}

/// `glTexParameteriv` / `glTexParameterxv` -- the value is behind a
/// pointer. Every `pname` GL ES 1.1 accepts here is single-valued.
fn gl_tex_parameter_iv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (target, pname, ptr) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?, ctx.arg_u32(2)?);
    if ptr == 0 {
        return Ok(VOID);
    }
    let raw = ctx.cpu.read_mem(ptr, 4)?;
    let value = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    with_ctx(|c| c.tex_parameter(target, pname, value));
    Ok(VOID)
}

/// `glTexParameterfv`. The float is an enum in disguise for every
/// `pname` that matters here, so it is truncated rather than scaled --
/// matching what `gl_tex_parameterf` does with the scalar form.
fn gl_tex_parameter_fv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (target, pname, ptr) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?, ctx.arg_u32(2)?);
    if ptr == 0 {
        return Ok(VOID);
    }
    let raw = ctx.cpu.read_mem(ptr, 4)?;
    let value = f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as u32;
    with_ctx(|c| c.tex_parameter(target, pname, value));
    Ok(VOID)
}

fn gl_tex_envf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pname = ctx.arg_u32(1)?;
    let value = census_env_override(pname, argf(ctx, 2)? as u32);
    if census_env_forward() {
        with_ctx(|c| c.tex_env(pname, value));
    }
    census_tex_env(pname, value);
    Ok(VOID)
}

fn gl_tex_envx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pname = ctx.arg_u32(1)?;
    let value = census_env_override(pname, ctx.arg_u32(2)?);
    if census_env_forward() {
        with_ctx(|c| c.tex_env(pname, value));
    }
    census_tex_env(pname, value);
    Ok(VOID)
}

/// `glTexEnvfv` / `glTexEnvxv`. `GL_TEXTURE_ENV_COLOR` is the only
/// parameter that needs all four components.
fn tex_env_vector(
    ctx: &mut CallCtx<'_>,
    decode: fn(u32) -> f32,
) -> Result<DispatchOutcome, KernelError> {
    let pname = ctx.arg_u32(1)?;
    let ptr = ctx.arg_u32(2)?;
    if pname != pocket_gles::GL_TEXTURE_ENV_COLOR {
        let Ok(word) = ctx.cpu.read_u32_le(ptr) else {
            return Ok(VOID);
        };
        let value = census_env_override(pname, decode(word) as u32);
        if census_env_forward() {
            with_ctx(|c| c.tex_env(pname, value));
        }
        census_tex_env(pname, value);
        return Ok(VOID);
    }
    let Ok(bytes) = ctx.cpu.read_mem(ptr, 16) else {
        return Ok(VOID);
    };
    let mut v = [0f32; 4];
    for (i, c) in bytes.chunks_exact(4).enumerate() {
        v[i] = decode(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    }
    with_ctx(|c| c.set_tex_env_color(v));
    Ok(VOID)
}

fn gl_tex_envfv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    tex_env_vector(ctx, f32::from_bits)
}

fn gl_tex_envxv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    tex_env_vector(ctx, word_to_f32)
}

// ---- drawing ---------------------------------------------------------------

fn gl_draw_arrays(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (mode, first, count) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?, ctx.arg_u32(2)?);
    if census_should_skip() {
        return Ok(VOID);
    }
    let (range, colors, mat) = if *CENSUS_DEEP {
        let samples = census_sample_range(first, count);
        (
            census_texcoord_range(&mut *ctx.cpu, &samples),
            census_color_range(&mut *ctx.cpu, &samples),
            census_texture_matrix(),
        )
    } else {
        (None, None, [1.0, 1.0, 0.0, 0.0])
    };
    let mut mem = CpuMem(ctx.cpu);
    with_ctx(|c| c.draw_arrays(&mut mem, mode, first, count));
    census_draw(count, range, colors, mat);
    Ok(VOID)
}

fn gl_draw_elements(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (mode, count, ty, ptr) = (
        ctx.arg_u32(0)?,
        ctx.arg_u32(1)?,
        ctx.arg_u32(2)?,
        ctx.arg_u32(3)?,
    );
    log::debug!(
        "GLES glDrawElements mode=0x{mode:04x} count={} type=0x{ty:04x} indices=0x{ptr:08x}",
        count,
    );
    if census_should_skip() {
        return Ok(VOID);
    }
    let (range, colors, mat) = if *CENSUS_DEEP {
        let samples = census_sample_elements(&mut *ctx.cpu, count, ty, ptr);
        (
            census_texcoord_range(&mut *ctx.cpu, &samples),
            census_color_range(&mut *ctx.cpu, &samples),
            census_texture_matrix(),
        )
    } else {
        (None, None, [1.0, 1.0, 0.0, 0.0])
    };
    let mut mem = CpuMem(ctx.cpu);
    with_ctx(|c| c.draw_elements_from_guest(&mut mem, mode, count, ty, ptr));
    census_draw(count, range, colors, mat);
    Ok(VOID)
}

// ---- queries ---------------------------------------------------------------

fn gl_get_error(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let e = with_ctx(|c| c.take_error());
    Ok(DispatchOutcome::ReturnedR0(e))
}

/// `glGetIntegerv`. Only the queries a renderer actually branches on
/// are answered; anything else writes zero, which is what a
/// conformant-but-minimal implementation reports for an unsupported
/// feature anyway.
fn gl_get_integerv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pname = ctx.arg_u32(0)?;
    let out = ctx.arg_u32(1)?;
    if out == 0 {
        return Ok(VOID);
    }
    let values: Vec<i32> = match pname {
        pocket_gles::GL_MAX_TEXTURE_SIZE => vec![1024],
        pocket_gles::GL_MAX_LIGHTS => vec![8],
        pocket_gles::GL_MAX_TEXTURE_UNITS => vec![1],
        pocket_gles::GL_MAX_MODELVIEW_STACK_DEPTH => vec![16],
        pocket_gles::GL_MAX_PROJECTION_STACK_DEPTH => vec![16],
        pocket_gles::GL_MAX_TEXTURE_STACK_DEPTH => vec![16],
        pocket_gles::GL_DEPTH_BITS => vec![16],
        pocket_gles::GL_STENCIL_BITS => vec![0],
        pocket_gles::GL_RED_BITS => vec![5],
        pocket_gles::GL_GREEN_BITS => vec![6],
        pocket_gles::GL_BLUE_BITS => vec![5],
        pocket_gles::GL_ALPHA_BITS => vec![0],
        pocket_gles::GL_NUM_COMPRESSED_TEXTURE_FORMATS => vec![COMPRESSED_FORMATS.len() as i32],
        pocket_gles::GL_COMPRESSED_TEXTURE_FORMATS => {
            COMPRESSED_FORMATS.iter().map(|&f| f as i32).collect()
        }
        pocket_gles::GL_VIEWPORT => {
            let vp = with_ctx(|c| c.state.viewport);
            vec![vp.0, vp.1, vp.2, vp.3]
        }
        pocket_gles::GL_ARRAY_BUFFER_BINDING => {
            vec![with_ctx(|c| c.array_buffer) as i32]
        }
        pocket_gles::GL_ELEMENT_ARRAY_BUFFER_BINDING => {
            vec![with_ctx(|c| c.element_array_buffer) as i32]
        }
        _ => vec![0],
    };
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    if !bytes.is_empty() {
        ctx.cpu.write_mem(out, &bytes)?;
    }
    Ok(VOID)
}

/// `glGetFloatv`. Games read the matrices back far more often than
/// anything else here — a renderer that keeps its own copy of the
/// modelview still asks GL for the projection to build a culling
/// frustum, and gets a degenerate one if we answer with zeroes.
fn gl_get_floatv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pname = ctx.arg_u32(0)?;
    let out = ctx.arg_u32(1)?;
    if out == 0 {
        return Ok(VOID);
    }
    let values: Vec<f32> = match pname {
        pocket_gles::GL_MODELVIEW_MATRIX => with_ctx(|c| c.modelview.current().to_vec()),
        pocket_gles::GL_PROJECTION_MATRIX => with_ctx(|c| c.projection.current().to_vec()),
        pocket_gles::GL_TEXTURE_MATRIX => with_ctx(|c| c.texture_matrix.current().to_vec()),
        pocket_gles::GL_CURRENT_COLOR => with_ctx(|c| c.current_color.to_vec()),
        pocket_gles::GL_DEPTH_RANGE => {
            let (n, f) = with_ctx(|c| c.state.depth_range);
            vec![n, f]
        }
        // Integer-valued queries are legal through glGetFloatv as well;
        // GL converts. Only the few games actually ask for are listed.
        pocket_gles::GL_MAX_TEXTURE_SIZE => vec![1024.0],
        pocket_gles::GL_MAX_LIGHTS => vec![8.0],
        pocket_gles::GL_MAX_TEXTURE_UNITS => vec![1.0],
        pocket_gles::GL_VIEWPORT => {
            let vp = with_ctx(|c| c.state.viewport);
            vec![vp.0 as f32, vp.1 as f32, vp.2 as f32, vp.3 as f32]
        }
        _ => vec![0.0],
    };
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    if !bytes.is_empty() {
        ctx.cpu.write_mem(out, &bytes)?;
    }
    Ok(VOID)
}

/// `glGetString`. The guest keeps the returned pointer, so the strings
/// live in a guest-side block allocated once and cached for the
/// process's lifetime.
fn gl_get_string(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name = ctx.arg_u32(0)?;
    let text = match name {
        pocket_gles::GL_VENDOR => "PocketHLE",
        pocket_gles::GL_RENDERER => "PocketHLE Software Rasterizer",
        pocket_gles::GL_VERSION => "OpenGL ES-CL 1.1",
        pocket_gles::GL_EXTENSIONS => {
            "GL_AMD_compressed_ATC_texture GL_ATI_texture_compression_atitc GL_EXT_texture_compression_s3tc GL_EXT_texture_compression_dxt1 GL_EXT_texture_lod_bias GL_SGIS_texture_lod GL_OES_compressed_paletted_texture"
        }
        _ => {
            with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_ENUM));
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    Ok(DispatchOutcome::ReturnedR0(intern_string(ctx, text)?))
}

/// Copy `text` into guest memory as NUL-terminated ASCII, reusing the
/// same allocation for repeated queries of the same string.
fn intern_string(ctx: &mut CallCtx<'_>, text: &str) -> Result<u32, KernelError> {
    if let Some(va) = ctx.kernel.gles_strings.get(text).copied() {
        return Ok(va);
    }
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    let Some(va) = ctx.kernel.heap.alloc(bytes.len() as u32) else {
        return Ok(0);
    };
    ctx.cpu.write_mem(va, &bytes)?;
    ctx.kernel.gles_strings.insert(text.to_string(), va);
    Ok(va)
}

/// `glReadPixels(x, y, width, height, format, type, pixels)`. The
/// renderer's colour buffer is RGBA8888 top-row-first; GL wants
/// bottom-row-first, so rows are emitted in reverse.
fn gl_read_pixels(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let x = ctx.arg_u32(0)? as i32;
    let y = ctx.arg_u32(1)? as i32;
    let w = ctx.arg_u32(2)?;
    let h = ctx.arg_u32(3)?;
    let format = ctx.arg_u32(4)?;
    let ty = ctx.arg_u32(5)?;
    let out = ctx.arg_u32(6)?;
    if out == 0 || w == 0 || h == 0 {
        return Ok(VOID);
    }
    if format != pocket_gles::GL_RGBA || ty != GL_UNSIGNED_BYTE {
        with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_ENUM));
        return Ok(VOID);
    }
    let buf = with_ctx(|c| {
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        for row in 0..h {
            // GL row 0 is the bottom of the image.
            let src_y = c.target.height as i32 - 1 - (y + row as i32);
            if src_y < 0 || src_y >= c.target.height as i32 {
                continue;
            }
            for col in 0..w {
                let src_x = x + col as i32;
                if src_x < 0 || src_x >= c.target.width as i32 {
                    continue;
                }
                let s = ((src_y as u32 * c.target.width + src_x as u32) * 4) as usize;
                let d = ((row * w + col) * 4) as usize;
                buf[d..d + 4].copy_from_slice(&c.target.color[s..s + 4]);
            }
        }
        buf
    });
    ctx.cpu.write_mem(out, &buf)?;
    Ok(VOID)
}

// ---- accepted and ignored --------------------------------------------------

/// Entry points whose effect our rasterizer has no equivalent for.
/// Silently succeeding matches what a device without the feature does
/// and keeps the guest from bailing out on a spurious `GL_INVALID_*`.
/// Zero a query's output buffer when the parameter list is
/// `(pname, params)`.
///
/// Four words covers every GL ES 1.1 query in this group -- the widest
/// is a clip-plane equation.
fn gl_get_zeroed_1(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(1)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &[0u8; 16])?;
    }
    Ok(VOID)
}

/// Zero a query's output buffer when the parameter list is
/// `(target, pname, params)`.
fn gl_get_zeroed_2(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let out = ctx.arg_u32(2)?;
    if out != 0 {
        ctx.cpu.write_mem(out, &[0u8; 16])?;
    }
    Ok(VOID)
}

/// `GLboolean` query answering `GL_FALSE`.
fn gl_false(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(0))
}

fn gl_ignored(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(VOID)
}

/// Same no-op, but records the call. Fixed-function lighting and
/// material state are not implemented; routing them here instead of
/// `gl_ignored` makes it visible whether a title actually relies on
/// them before anyone tries to implement them.
fn gl_ignored_logged(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let a0 = ctx.arg_u32(0).unwrap_or(0);
    let a1 = ctx.arg_u32(1).unwrap_or(0);
    let a2 = ctx.arg_u32(2).unwrap_or(0);
    log::debug!("GLES unimplemented light/material call (0x{a0:04x}, 0x{a1:04x}, 0x{a2:08x})");
    Ok(VOID)
}

/// `glCompressedTexImage2D(target, level, internalformat, width,
/// height, border, imageSize, data)` — eight arguments, four on the
/// stack. The software decoder accepts ATC and DXT1 formats used by
/// Gizmondo DDS assets.
fn gl_compressed_tex_image_2d(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let target = ctx.arg_u32(0)?;
    let level = ctx.arg_u32(1)? as i32;
    let format = ctx.arg_u32(2)?;
    let width = ctx.arg_u32(3)?;
    let height = ctx.arg_u32(4)?;
    let size = ctx.arg_u32(6)?;
    let data = ctx.arg_u32(7)?;
    let bytes = if data == 0 || size == 0 {
        Vec::new()
    } else {
        ctx.cpu.read_mem(data, size).unwrap_or_default()
    };
    with_ctx(|c| c.compressed_tex_image_2d(target, level, width, height, format, &bytes));
    census_upload(
        level,
        width,
        height,
        format,
        0,
        bytes.len(),
        data != 0 && size != 0,
        true,
    );
    Ok(VOID)
}

/// `glCompressedTexSubImage2D` and the two `glCopyTex*Image2D` entry
/// points. We advertise no framebuffer-to-texture path and no partial
/// compressed update; report the error GL specifies.
fn gl_unsupported_format(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_ENUM));
    Ok(VOID)
}

// ---- EGL -------------------------------------------------------------------
//
// There is exactly one display, one config, one surface and one
// context, because there is exactly one software rasterizer behind
// them. The handles are distinct non-zero constants so a guest that
// compares them against `EGL_NO_*` — or against each other — sees what
// it expects.

/// The single `EGLDisplay` we hand out.
const DISPLAY_HANDLE: u32 = 0x4547_0001;
/// The single `EGLConfig`.
const CONFIG_HANDLE: u32 = 0x4547_0002;
/// The single window `EGLSurface`.
const SURFACE_HANDLE: u32 = 0x4547_0003;
/// The single `EGLContext`.
const CONTEXT_HANDLE: u32 = 0x4547_0004;

/// Sticky EGL error, independent of the GL one.
static EGL_ERROR: Mutex<u32> = Mutex::new(EGL_SUCCESS);

fn set_egl_error(code: u32) {
    let mut guard = match EGL_ERROR.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if *guard == EGL_SUCCESS {
        *guard = code;
    }
}

fn egl_get_display(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    // `EGL_DEFAULT_DISPLAY` is 0 and a real HDC is not, but both mean
    // "the screen" here.
    let _native = ctx.arg_u32(0)?;
    Ok(DispatchOutcome::ReturnedR0(DISPLAY_HANDLE))
}

/// `eglInitialize(dpy, *major, *minor)` — report EGL 1.1.
fn egl_initialize(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let dpy = ctx.arg_u32(0)?;
    if dpy != DISPLAY_HANDLE {
        set_egl_error(EGL_BAD_DISPLAY);
        return Ok(DispatchOutcome::ReturnedR0(EGL_FALSE));
    }
    let major = ctx.arg_u32(1)?;
    let minor = ctx.arg_u32(2)?;
    if major != 0 {
        ctx.cpu.write_mem(major, &1i32.to_le_bytes())?;
    }
    if minor != 0 {
        ctx.cpu.write_mem(minor, &1i32.to_le_bytes())?;
    }
    log::info!("eglInitialize() -> EGL 1.1 (PocketHLE software)");
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

fn egl_terminate(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

/// `eglGetConfigs(dpy, configs, config_size, *num_config)` — we have
/// exactly one config to offer.
fn egl_get_configs(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let configs = ctx.arg_u32(1)?;
    let size = ctx.arg_u32(2)? as i32;
    let num = ctx.arg_u32(3)?;
    let written: i32 = if configs != 0 && size > 0 {
        ctx.cpu.write_mem(configs, &CONFIG_HANDLE.to_le_bytes())?;
        1
    } else {
        // A null `configs` is the "how many are there?" query.
        1
    };
    if num != 0 {
        ctx.cpu.write_mem(num, &written.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

/// `eglChooseConfig(dpy, attrib_list, configs, config_size,
/// *num_config)`. The attribute list is read for logging but not
/// honoured: there is one config, and refusing to return it would
/// leave the guest with nothing to render into.
fn egl_choose_config(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let attribs = ctx.arg_u32(1)?;
    let configs = ctx.arg_u32(2)?;
    let size = ctx.arg_u32(3)? as i32;
    let num = ctx.arg_u32(4)?;
    if attribs != 0 {
        log_attrib_list(ctx, attribs);
    }
    if configs != 0 && size > 0 {
        ctx.cpu.write_mem(configs, &CONFIG_HANDLE.to_le_bytes())?;
    }
    if num != 0 {
        let count: i32 = if configs != 0 && size <= 0 { 0 } else { 1 };
        ctx.cpu.write_mem(num, &count.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

/// Walk an `EGL_NONE`-terminated attribute list and log it. Helps when
/// a game silently refuses to start because it wanted a config we
/// don't describe accurately.
fn log_attrib_list(ctx: &mut CallCtx<'_>, mut ptr: u32) {
    if !log::log_enabled!(log::Level::Debug) {
        return;
    }
    let mut pairs = Vec::new();
    for _ in 0..32 {
        let Ok(key) = ctx.cpu.read_u32_le(ptr) else {
            break;
        };
        if key == EGL_NONE {
            break;
        }
        let Ok(value) = ctx.cpu.read_u32_le(ptr + 4) else {
            break;
        };
        pairs.push(format!("0x{key:04x}={value}"));
        ptr += 8;
    }
    log::debug!("eglChooseConfig attribs: [{}]", pairs.join(", "));
}

/// `eglGetConfigAttrib(dpy, config, attribute, *value)`. The numbers
/// describe the RGB565 window surface the rasterizer actually presents.
fn egl_get_config_attrib(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let attribute = ctx.arg_u32(2)?;
    let out = ctx.arg_u32(3)?;
    let value: i32 = match attribute {
        EGL_BUFFER_SIZE => 16,
        EGL_RED_SIZE => 5,
        EGL_GREEN_SIZE => 6,
        EGL_BLUE_SIZE => 5,
        EGL_ALPHA_SIZE => 0,
        EGL_DEPTH_SIZE => 16,
        pocket_gles::EGL_STENCIL_SIZE => 0,
        pocket_gles::EGL_SAMPLES | pocket_gles::EGL_SAMPLE_BUFFERS => 0,
        EGL_SURFACE_TYPE => EGL_WINDOW_BIT as i32,
        EGL_CONFIG_ID => 1,
        EGL_RENDERABLE_TYPE => 1, // EGL_OPENGL_ES_BIT
        pocket_gles::EGL_CONFIG_CAVEAT | pocket_gles::EGL_TRANSPARENT_TYPE => EGL_NONE as i32,
        pocket_gles::EGL_NATIVE_RENDERABLE => EGL_FALSE as i32,
        pocket_gles::EGL_LEVEL => 0,
        pocket_gles::EGL_MAX_PBUFFER_WIDTH => 0,
        pocket_gles::EGL_MAX_PBUFFER_HEIGHT => 0,
        pocket_gles::EGL_MAX_PBUFFER_PIXELS => 0,
        _ => {
            set_egl_error(pocket_gles::EGL_BAD_ATTRIBUTE);
            return Ok(DispatchOutcome::ReturnedR0(EGL_FALSE));
        }
    };
    if out != 0 {
        ctx.cpu.write_mem(out, &value.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

/// `eglCreateWindowSurface(dpy, config, window, attrib_list)`. The
/// surface is sized from the kernel framebuffer, which is what the
/// frontend actually shows.
fn egl_create_window_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (w, h) = (ctx.kernel.framebuffer.width, ctx.kernel.framebuffer.height);
    with_ctx(|c| {
        c.target.resize(w, h);
        c.set_viewport(0, 0, w as i32, h as i32);
    });
    log::info!("eglCreateWindowSurface() -> {w}×{h} RGB565");
    Ok(DispatchOutcome::ReturnedR0(SURFACE_HANDLE))
}

fn egl_create_context(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(CONTEXT_HANDLE))
}

fn egl_destroy_surface(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

fn egl_destroy_context(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

fn egl_make_current(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

fn egl_get_current_display(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(DISPLAY_HANDLE))
}

fn egl_get_current_context(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(CONTEXT_HANDLE))
}

fn egl_get_current_surface(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(SURFACE_HANDLE))
}

fn egl_get_error(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let mut guard = match EGL_ERROR.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    Ok(DispatchOutcome::ReturnedR0(std::mem::replace(
        &mut *guard,
        EGL_SUCCESS,
    )))
}

fn egl_query_string(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name = ctx.arg_u32(1)?;
    let text = match name {
        EGL_VENDOR => "PocketHLE",
        EGL_VERSION => "1.1 PocketHLE",
        EGL_EXTENSIONS => "",
        _ => {
            set_egl_error(pocket_gles::EGL_BAD_PARAMETER);
            return Ok(DispatchOutcome::ReturnedR0(0));
        }
    };
    Ok(DispatchOutcome::ReturnedR0(intern_string(ctx, text)?))
}

/// `eglQuerySurface(dpy, surface, attribute, *value)`.
fn egl_query_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let attribute = ctx.arg_u32(2)?;
    let out = ctx.arg_u32(3)?;
    let value: i32 = match attribute {
        pocket_gles::EGL_WIDTH => ctx.kernel.framebuffer.width as i32,
        pocket_gles::EGL_HEIGHT => ctx.kernel.framebuffer.height as i32,
        EGL_CONFIG_ID => 1,
        pocket_gles::EGL_LARGEST_PBUFFER => EGL_FALSE as i32,
        _ => {
            set_egl_error(pocket_gles::EGL_BAD_ATTRIBUTE);
            return Ok(DispatchOutcome::ReturnedR0(EGL_FALSE));
        }
    };
    if out != 0 {
        ctx.cpu.write_mem(out, &value.to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

/// `eglSwapBuffers` — the only place GL output becomes visible.
/// Converts the RGBA8888 render target down to the kernel
/// framebuffer's RGB565 and marks it dirty so the frontend picks it up.
fn egl_swap_buffers(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let fb_len = ctx.kernel.framebuffer.pixels.len();
    let (w, h) = (ctx.kernel.framebuffer.width, ctx.kernel.framebuffer.height);
    with_ctx(|c| {
        // A guest that never called eglCreateWindowSurface — or a
        // frontend that resized the window since — leaves the two out
        // of step. Match the framebuffer, since that is what is shown.
        if c.target.width != w || c.target.height != h {
            c.target.resize(w, h);
        }
        c.target
            .to_rgb565(&mut ctx.kernel.framebuffer.pixels[..fb_len]);
    });
    ctx.kernel.framebuffer.mark_dirty();
    // A guest can open GAPI for its input side alone: COD2 calls
    // GXOpenDisplay/GXOpenInput/GXGetDefaultKeys to pick up the device
    // key mapping and then renders exclusively through EGL. That maps
    // the synthetic framebuffer, but nothing ever draws into it, so it
    // stays zero-filled. Leaving it stale lets the end-of-slice GAPI
    // readback copy those zeros back over the frame we just presented,
    // blanking the screen. Push the presented pixels into the guest
    // mapping so both views agree and the readback sees no change.
    if ctx.kernel.fb_mapped {
        ctx.cpu.write_mem(
            pocket_kernel::SYNTHETIC_FRAMEBUFFER_BASE,
            &ctx.kernel.framebuffer.pixels,
        )?;
    }
    ctx.kernel.gx_last_pushed_counter = ctx.kernel.framebuffer.frame_counter;
    log::trace!(
        "eglSwapBuffers() -> frame {}",
        ctx.kernel.framebuffer.frame_counter
    );
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

/// Entry points that succeed without doing anything: there is no
/// asynchronous queue to wait on and no swap interval to honour.
fn egl_true(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

/// Entry points for functionality we do not provide (pbuffers, pixmaps,
/// buffer copies). Returning the null handle plus `EGL_BAD_MATCH` is
/// how a driver without the capability answers.
fn egl_unsupported(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    set_egl_error(pocket_gles::EGL_BAD_MATCH);
    Ok(DispatchOutcome::ReturnedR0(EGL_FALSE))
}

fn egl_no_surface(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    set_egl_error(pocket_gles::EGL_BAD_MATCH);
    Ok(DispatchOutcome::ReturnedR0(EGL_NO_SURFACE))
}

// ---- registration ----------------------------------------------------------

/// Both client libraries we emulate. The Common profile is a strict
/// superset of Common-Lite, so a single handler table serves both and
/// each DLL simply exposes the subset its ordinal table names.
pub const GLES_DLLS: [&str; 2] = ["libgles_cm.dll", "libgles_cl.dll"];

/// Map an entry-point name to its handler. `None` means the name is in
/// the ordinal table but has no implementation, in which case the
/// dispatcher's unimplemented path takes over and logs it — which is
/// exactly the signal we want when a new game calls something new.
fn handler_for(name: &str) -> Option<crate::Handler> {
    Some(match name {
        // transform
        "glMatrixMode" => gl_matrix_mode,
        "glLoadIdentity" => gl_load_identity,
        "glPushMatrix" => gl_push_matrix,
        "glPopMatrix" => gl_pop_matrix,
        "glLoadMatrixf" => gl_load_matrixf,
        "glLoadMatrixx" => gl_load_matrixx,
        "glMultMatrixf" => gl_mult_matrixf,
        "glMultMatrixx" => gl_mult_matrixx,
        "glTranslatef" => gl_translatef,
        "glTranslatex" => gl_translatex,
        "glScalef" => gl_scalef,
        "glScalex" => gl_scalex,
        "glRotatef" => gl_rotatef,
        "glRotatex" => gl_rotatex,
        "glFrustumf" => gl_frustumf,
        "glFrustumx" => gl_frustumx,
        "glOrthof" => gl_orthof,
        "glOrthox" => gl_orthox,

        // enables and fragment state
        "glEnable" => gl_enable,
        "glDisable" => gl_disable,
        "glEnableClientState" => gl_enable_client_state,
        "glDisableClientState" => gl_disable_client_state,
        "glDepthFunc" => gl_depth_func,
        "glDepthMask" => gl_depth_mask,
        "glDepthRangef" => gl_depth_rangef,
        "glDepthRangex" => gl_depth_rangex,
        "glAlphaFunc" => gl_alpha_funcf,
        "glAlphaFuncx" => gl_alpha_funcx,
        "glBlendFunc" => gl_blend_func,
        "glCullFace" => gl_cull_face,
        "glFrontFace" => gl_front_face,
        "glShadeModel" => gl_shade_model,
        "glColorMask" => gl_color_mask,
        "glViewport" => gl_viewport,
        "glScissor" => gl_scissor,
        "glFogf" => gl_fogf,
        "glFogx" => gl_fogx,
        "glFogfv" => gl_fogfv,
        "glFogxv" => gl_fogxv,

        // clear and current values
        "glClear" => gl_clear,
        "glClearColor" => gl_clear_colorf,
        "glClearColorx" => gl_clear_colorx,
        "glClearDepthf" => gl_clear_depthf,
        "glClearDepthx" => gl_clear_depthx,
        "glColor4f" => gl_color4f,
        "glColor4x" => gl_color4x,
        "glNormal3f" => gl_normal3f,
        "glNormal3x" => gl_normal3x,
        "glMultiTexCoord4f" => gl_multi_tex_coord4f,
        "glMultiTexCoord4x" => gl_multi_tex_coord4x,
        "glActiveTexture" => gl_active_texture,
        "glClientActiveTexture" => gl_client_active_texture,

        // client arrays
        "glVertexPointer" => gl_vertex_pointer,
        "glColorPointer" => gl_color_pointer,
        "glTexCoordPointer" => gl_tex_coord_pointer,
        "glNormalPointer" => gl_normal_pointer,

        // textures
        "glGenTextures" => gl_gen_textures,
        "glDeleteTextures" => gl_delete_textures,
        "glBindTexture" => gl_bind_texture,
        "glTexImage2D" => gl_tex_image_2d,
        "glTexSubImage2D" => gl_tex_sub_image_2d,
        "glTexParameterf" => gl_tex_parameterf,
        "glTexParameterx" => gl_tex_parameterx,
        // `glTexParameteri` takes the same `(target, pname, param)`
        // shape and, like the fixed-point form, carries an enum rather
        // than a scaled number -- `tex_parameter` wants the raw word
        // either way, so the two share a handler. This is the call
        // every GL program uses to set its min/mag filter and wrap
        // mode, so leaving it unresolved is not survivable.
        "glTexParameteri" => gl_tex_parameterx,
        // Vector forms: the value sits behind a pointer.
        "glTexParameteriv" | "glTexParameterxv" => gl_tex_parameter_iv,
        "glTexParameterfv" => gl_tex_parameter_fv,
        // Point sprite sizing and distance attenuation. The rasterizer
        // draws points at a fixed size, so these are accepted and
        // dropped rather than left to resolve as null.
        "glPointParameterf" | "glPointParameterfv" | "glPointParameterx"
        | "glPointParameterxv" => gl_ignored,
        "glTexEnvf" => gl_tex_envf,
        "glTexEnvx" => gl_tex_envx,
        "glTexEnvfv" => gl_tex_envfv,
        "glTexEnvxv" => gl_tex_envxv,

        // drawing
        "glDrawArrays" => gl_draw_arrays,
        "glDrawElements" => gl_draw_elements,

        // queries
        "glGetError" => gl_get_error,
        "glGetIntegerv" => gl_get_integerv,
        "glGetFloatv" => gl_get_floatv,
        "glGetString" => gl_get_string,
        "glReadPixels" => gl_read_pixels,

        // The rest of the GL ES 1.1 query family.
        //
        // These have to resolve to *something*. A game that fetches its
        // entry points with `GetProcAddress` stores whatever it gets and
        // calls it later without checking: NFS Undercover resolves all
        // of these at startup and then branches to a null Thumb address
        // (`pc=0x00000001`) the moment it uses one. Returning zeroed
        // output is a poor answer to the query but a survivable one,
        // where an unresolved name is fatal.
        //
        // Split by where the output pointer sits: `(pname, params)` for
        // the global getters, `(target, pname, params)` for the ones
        // scoped to a texture unit, light, material or buffer.
        "glGetBooleanv" | "glGetFixedv" | "glGetPointerv" | "glGetClipPlanef"
        | "glGetClipPlanex" => gl_get_zeroed_1,
        "glGetTexParameterfv" | "glGetTexParameteriv" | "glGetTexEnvfv" | "glGetTexEnviv"
        | "glGetTexEnvxv" | "glGetLightfv" | "glGetLightxv" | "glGetMaterialfv"
        | "glGetMaterialxv" | "glGetBufferParameteriv" => gl_get_zeroed_2,

        // `GLboolean` predicates. False is the safe answer: a game that
        // asks whether a name is a live texture or buffer and is told
        // "no" re-creates it, where "yes" would have it use a handle we
        // never made.
        "glIsEnabled" | "glIsBuffer" | "glIsTexture" => gl_false,

        // Clip planes: accepted and ignored. The rasterizer has no
        // user clip stage, so geometry that should be clipped is drawn
        // whole rather than not at all.
        "glClipPlanef" | "glClipPlanex" => gl_ignored,

        // buffer objects
        "glGenBuffers" => gl_gen_buffers,
        "glDeleteBuffers" => gl_delete_buffers,
        "glBindBuffer" => gl_bind_buffer,
        "glBufferData" => gl_buffer_data,
        "glBufferSubData" => gl_buffer_sub_data,

        // Matrix-palette skinning (OES_matrix_palette). The rasterizer
        // transforms by the modelview alone, so a skinned mesh renders
        // in its bind pose rather than not at all.
        "glMatrixIndexPointerOES"
        | "glWeightPointerOES"
        | "glCurrentPaletteMatrixOES"
        | "glLoadPaletteFromModelViewMatrixOES"
        | "glPointSizePointerOES" => gl_ignored,

        // Lighting is not evaluated: the rasterizer shades from the
        // vertex colour alone. Games that light their geometry get flat
        // colours rather than a black screen or a hard error.
        "glLightf" | "glLightfv" | "glLightx" | "glLightxv" | "glLightModelf"
        | "glLightModelfv" | "glLightModelx" | "glLightModelxv" | "glMaterialf"
        | "glMaterialfv" | "glMaterialx" | "glMaterialxv" => gl_ignored_logged,

        // No stencil buffer, no logic op, no multisample, no polygon
        // offset, no point/line rasterization, no mipmap hints.
        "glStencilFunc" | "glStencilMask" | "glStencilOp" | "glClearStencil" | "glLogicOp"
        | "glSampleCoverage" | "glSampleCoveragex" | "glPolygonOffset" | "glPolygonOffsetx"
        | "glPointSize" | "glPointSizex" | "glLineWidth" | "glLineWidthx" | "glHint"
        | "glPixelStorei" | "glFinish" | "glFlush" => gl_ignored,

        // Compressed and copy-from-framebuffer texture paths.
        "glCompressedTexImage2D" => gl_compressed_tex_image_2d,
        "glCompressedTexSubImage2D" | "glCopyTexImage2D" | "glCopyTexSubImage2D" => {
            gl_unsupported_format
        }

        // EGL
        "eglGetDisplay" => egl_get_display,
        "eglInitialize" => egl_initialize,
        "eglTerminate" => egl_terminate,
        "eglGetConfigs" => egl_get_configs,
        "eglChooseConfig" => egl_choose_config,
        "eglGetConfigAttrib" => egl_get_config_attrib,
        "eglCreateWindowSurface" => egl_create_window_surface,
        "eglCreateContext" => egl_create_context,
        "eglDestroySurface" => egl_destroy_surface,
        "eglDestroyContext" => egl_destroy_context,
        "eglMakeCurrent" => egl_make_current,
        "eglSwapBuffers" => egl_swap_buffers,
        "eglGetError" => egl_get_error,
        "eglQueryString" => egl_query_string,
        "eglQuerySurface" => egl_query_surface,
        "eglGetCurrentDisplay" => egl_get_current_display,
        "eglGetCurrentContext" => egl_get_current_context,
        "eglGetCurrentSurface" => egl_get_current_surface,
        "eglWaitGL" | "eglWaitNative" | "eglWaitClient" | "eglSwapInterval"
        // The Gizmondo's GoForce driver exposed the swap interval under an
        // NV name, and Sticky Balls calls that one. Nothing here is
        // presented on a real vblank, so accepting it is the whole job.
        | "eglSwapIntervalNV"
        | "eglReleaseThread" | "eglSurfaceAttrib" | "eglBindAPI" => egl_true,
        "eglCreatePbufferSurface"
        | "eglCreatePixmapSurface"
        | "eglCreatePbufferFromClientBuffer" => egl_no_surface,
        "eglCopyBuffers" | "eglBindTexImage" | "eglReleaseTexImage" | "eglQueryContext"
        | "eglQueryAPI" | "eglGetProcAddress" => egl_unsupported,

        _ => return None,
    })
}

/// Reset the GL context to its initial state. Called between test
/// cases, since the context is process-global.
#[cfg(test)]
fn reset_for_test(width: u32, height: u32) {
    with_ctx(|c| *c = Context::new(width, height));
    let mut guard = match EGL_ERROR.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *guard = EGL_SUCCESS;
}

/// Entry points that are not in the ordinal tables but that games
/// import *by name*.
///
/// The tables in `pocket-gles/data` are generated by alphabetising the
/// Khronos entry-point lists, so the ordinal of every function depends
/// on every other function in the list. Adding names there would shift
/// the ordinals COD2 is verified against and break it. Vendor DLLs
/// export these anyway — the GL ES 1.1 buffer-object calls are core,
/// not extensions — and a game that imports by name (Xtrakt does, with
/// 73 named symbols) resolves them through the name path regardless of
/// what the ordinal table says.
const EXTRA_NAMED_EXPORTS: [&str; 40] = [
    "glGenBuffers",
    "glDeleteBuffers",
    "glBindBuffer",
    "glBufferData",
    "glBufferSubData",
    "glGetFloatv",
    "glMatrixIndexPointerOES",
    "glWeightPointerOES",
    "glCurrentPaletteMatrixOES",
    "glLoadPaletteFromModelViewMatrixOES",
    "glPointSizePointerOES",
    // The GL ES 1.1 query family, absent from the generated tables for
    // the same reason as the buffer calls above. A real vendor DLL
    // exports all of these -- they are core, not extensions -- and a
    // game resolving them by name gets a valid pointer. NFS Undercover
    // asks for every one of them at startup and calls into whatever it
    // was handed, so an unresolved name is a null branch rather than a
    // missing feature.
    "glGetBooleanv",
    "glGetFixedv",
    "glGetPointerv",
    "glGetClipPlanef",
    "glGetClipPlanex",
    "glGetTexParameterfv",
    "glGetTexParameteriv",
    "glGetTexEnvfv",
    "glGetTexEnviv",
    "glGetTexEnvxv",
    "glGetLightfv",
    "glGetLightxv",
    "glGetMaterialfv",
    "glGetMaterialxv",
    "glGetBufferParameteriv",
    "glIsEnabled",
    "glIsBuffer",
    "glIsTexture",
    "glClipPlanef",
    "glClipPlanex",
    "glTexParameteri",
    "glTexParameteriv",
    "glTexParameterfv",
    "glTexParameterxv",
    "glPointParameterf",
    "glPointParameterfv",
    "glPointParameterx",
    "glPointParameterxv",
    // Vendor extension, so it is in no Khronos list: the Gizmondo's
    // GoForce driver named the swap-interval call this way and Sticky
    // Balls imports it.
    "eglSwapIntervalNV",
];

pub fn register(d: &mut WinCeDispatcher) {
    for dll in GLES_DLLS {
        for name in gles_ord::names_for(dll) {
            let Some(handler) = handler_for(&name) else {
                continue;
            };
            d.register_handler(dll, &name, handler);
        }
        for name in EXTRA_NAMED_EXPORTS {
            let Some(handler) = handler_for(name) else {
                continue;
            };
            d.register_handler(dll, name, handler);
        }
        // Games import these libraries purely by ordinal, so the
        // ordinal spellings the loader produces need to reach the same
        // handler. This mirrors the `coredll.dll` aliasing in
        // `WinCeDispatcher::new`.
        for ordinal in 0..=4095u16 {
            let Some(name) = gles_ord::lookup(dll, ordinal) else {
                continue;
            };
            let Some(handler) = handler_for(&name) else {
                continue;
            };
            for alias in [format!("ord:{ordinal}"), format!("#{ordinal}")] {
                d.register_handler(dll, &alias, handler);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gx::tests::fresh_kernel;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
    use pocket_kernel::{KernelState, Thunk};
    use pocket_pe::ImportBinding;

    /// The tests share one process-global GL context, so they must not
    /// run concurrently. A single mutex serializes them.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        match TEST_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn thunk() -> Thunk {
        Thunk {
            thunk_va: 0x7000_0000,
            iat_va: 0x4000_0000,
            dll: "libgles_cl.dll".to_string(),
            binding: ImportBinding::Ordinal(0),
            friendly_name: None,
        }
    }

    /// Invoke `handler` with `args` in r0..r3 (plus a stack tail) and
    /// return its r0.
    fn call(
        cpu: &mut StubCpu,
        kernel: &mut KernelState,
        handler: crate::Handler,
        args: &[u32],
    ) -> u32 {
        let t = thunk();
        for (i, v) in args.iter().enumerate().take(4) {
            let reg = [ArmReg::R0, ArmReg::R1, ArmReg::R2, ArmReg::R3][i];
            cpu.write_reg(reg, *v).unwrap();
        }
        if args.len() > 4 {
            // Stack arguments start at [sp + stack_arg_offset].
            let sp = cpu.read_reg(ArmReg::Sp).unwrap();
            let base = sp + cpu.stack_arg_offset();
            for (i, v) in args[4..].iter().enumerate() {
                cpu.write_mem(base + i as u32 * 4, &v.to_le_bytes())
                    .unwrap();
            }
        }
        let mut c = CallCtx {
            cpu,
            thunk: &t,
            kernel,
        };
        match handler(&mut c).unwrap() {
            DispatchOutcome::ReturnedR0(v) => v,
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    /// A CPU with a stack and a scratch data page mapped.
    const SCRATCH: u32 = 0x2000_0000;

    fn fresh_cpu() -> StubCpu {
        let mut cpu = StubCpu::new();
        cpu.map_region(SCRATCH, 0x4000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.map_region(0x1000_0000, 0x4000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.write_reg(ArmReg::Sp, 0x1000_2000).unwrap();
        cpu
    }

    #[test]
    fn every_ordinal_a_game_imports_reaches_a_handler() {
        let d = WinCeDispatcher::new();
        let registered: std::collections::HashSet<(String, String)> = d
            .registered_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        // Call of Duty 2 imports these by ordinal only. If the ordinal
        // table or the handler map drifts, the game silently gets an
        // unimplemented stub instead of a renderer.
        for (dll, ordinal, name) in [
            ("libgles_cl.dll", 50u16, "glDrawElements"),
            ("libgles_cl.dll", 49, "glDrawArrays"),
            ("libgles_cl.dll", 97, "glTexImage2D"),
            ("libgles_cl.dll", 22, "eglSwapBuffers"),
            ("libgles_cm.dll", 64, "glDrawElements"),
            ("libgles_cm.dll", 63, "glDrawArrays"),
            ("libgles_cm.dll", 133, "glTexImage2D"),
            ("libgles_cm.dll", 29, "eglSwapBuffers"),
        ] {
            assert_eq!(
                gles_ord::lookup(dll, ordinal).as_deref(),
                Some(name),
                "{dll} #{ordinal} should be {name}"
            );
            for spelling in [
                name.to_string(),
                format!("ord:{ordinal}"),
                format!("#{ordinal}"),
            ] {
                assert!(
                    registered.contains(&(dll.to_string(), spelling.clone())),
                    "{dll}!{spelling} is not registered"
                );
            }
        }
    }

    /// `eglSwapIntervalNV` is a vendor extension, so it appears in no
    /// Khronos ordinal list. Sticky Balls imports it by name, and an
    /// entry point that only exists inside `handler_for` is never
    /// registered — it has to be in `EXTRA_NAMED_EXPORTS` too.
    #[test]
    fn vendor_named_exports_are_registered_not_just_handled() {
        let d = WinCeDispatcher::new();
        let names: std::collections::HashSet<(String, String)> = d
            .registered_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        for name in EXTRA_NAMED_EXPORTS {
            assert!(
                handler_for(name).is_some(),
                "{name} is listed as an extra export but has no handler"
            );
            assert!(
                names.contains(&("libgles_cm.dll".to_string(), name.to_string())),
                "libgles_cm.dll!{name} is not registered"
            );
        }
    }

    #[test]
    fn the_common_lite_profile_has_no_float_entry_points_registered() {
        // `libGLES_CL.dll` genuinely does not export `glFrustumf`; a
        // guest that found one would be reading a table we invented.
        let d = WinCeDispatcher::new();
        let names: std::collections::HashSet<String> = d
            .registered_iter()
            .filter(|(dll, _)| *dll == "libgles_cl.dll")
            .map(|(_, n)| n.to_string())
            .collect();
        assert!(names.contains("glFrustumx"));
        assert!(!names.contains("glFrustumf"));
        assert!(!names.contains("glColor4f"));
        // ...but the Common profile exports both.
        let cm: std::collections::HashSet<String> = d
            .registered_iter()
            .filter(|(dll, _)| *dll == "libgles_cm.dll")
            .map(|(_, n)| n.to_string())
            .collect();
        assert!(cm.contains("glFrustumf"));
        assert!(cm.contains("glFrustumx"));
    }

    #[test]
    fn egl_initialize_reports_one_point_one() {
        let _g = guard();
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        let dpy = call(&mut cpu, &mut kernel, egl_get_display, &[0]);
        assert_ne!(dpy, 0, "eglGetDisplay must not return EGL_NO_DISPLAY");
        let ok = call(
            &mut cpu,
            &mut kernel,
            egl_initialize,
            &[dpy, SCRATCH, SCRATCH + 4],
        );
        assert_eq!(ok, EGL_TRUE);
        assert_eq!(cpu.read_u32_le(SCRATCH).unwrap(), 1);
        assert_eq!(cpu.read_u32_le(SCRATCH + 4).unwrap(), 1);
    }

    #[test]
    fn egl_initialize_rejects_a_display_it_never_handed_out() {
        let _g = guard();
        reset_for_test(64, 64);
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        let ok = call(&mut cpu, &mut kernel, egl_initialize, &[0xdead, 0, 0]);
        assert_eq!(ok, EGL_FALSE);
        let err = call(&mut cpu, &mut kernel, egl_get_error, &[]);
        assert_eq!(err, EGL_BAD_DISPLAY);
        // The error is sticky-then-cleared, GL style.
        assert_eq!(call(&mut cpu, &mut kernel, egl_get_error, &[]), EGL_SUCCESS);
    }

    #[test]
    fn choose_config_yields_one_config_and_describes_it_as_rgb565() {
        let _g = guard();
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        // An EGL_NONE-terminated empty attribute list.
        cpu.write_mem(SCRATCH, &EGL_NONE.to_le_bytes()).unwrap();
        let ok = call(
            &mut cpu,
            &mut kernel,
            egl_choose_config,
            &[DISPLAY_HANDLE, SCRATCH, SCRATCH + 0x10, 1, SCRATCH + 0x20],
        );
        assert_eq!(ok, EGL_TRUE);
        assert_eq!(cpu.read_u32_le(SCRATCH + 0x20).unwrap(), 1);
        let config = cpu.read_u32_le(SCRATCH + 0x10).unwrap();
        assert_eq!(config, CONFIG_HANDLE);

        for (attr, want) in [
            (EGL_RED_SIZE, 5),
            (EGL_GREEN_SIZE, 6),
            (EGL_BLUE_SIZE, 5),
            (EGL_DEPTH_SIZE, 16),
            (EGL_BUFFER_SIZE, 16),
        ] {
            let ok = call(
                &mut cpu,
                &mut kernel,
                egl_get_config_attrib,
                &[DISPLAY_HANDLE, config, attr, SCRATCH + 0x30],
            );
            assert_eq!(ok, EGL_TRUE, "attrib 0x{attr:04x}");
            assert_eq!(
                cpu.read_u32_le(SCRATCH + 0x30).unwrap(),
                want,
                "attrib 0x{attr:04x}"
            );
        }
    }

    #[test]
    fn an_unknown_config_attribute_is_a_bad_attribute_not_a_zero() {
        let _g = guard();
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        let ok = call(
            &mut cpu,
            &mut kernel,
            egl_get_config_attrib,
            &[DISPLAY_HANDLE, CONFIG_HANDLE, 0x9999, SCRATCH],
        );
        assert_eq!(ok, EGL_FALSE);
        assert_eq!(
            call(&mut cpu, &mut kernel, egl_get_error, &[]),
            pocket_gles::EGL_BAD_ATTRIBUTE
        );
    }

    #[test]
    fn create_window_surface_sizes_the_target_from_the_framebuffer() {
        let _g = guard();
        reset_for_test(8, 8);
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        let (w, h) = (kernel.framebuffer.width, kernel.framebuffer.height);
        let surf = call(
            &mut cpu,
            &mut kernel,
            egl_create_window_surface,
            &[DISPLAY_HANDLE, CONFIG_HANDLE, 0x1234, 0],
        );
        assert_ne!(surf, EGL_NO_SURFACE);
        with_ctx(|c| {
            assert_eq!((c.target.width, c.target.height), (w, h));
            assert_eq!(c.state.viewport, (0, 0, w as i32, h as i32));
        });
    }

    #[test]
    fn swap_buffers_presents_the_clear_colour_to_the_framebuffer() {
        let _g = guard();
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        let (w, h) = (kernel.framebuffer.width, kernel.framebuffer.height);
        reset_for_test(w, h);
        let before = kernel.framebuffer.frame_counter;

        // glClearColor(1, 0, 0, 1) in 16.16 fixed point, then clear and
        // swap — the CL profile's only clear-colour entry point.
        call(
            &mut cpu,
            &mut kernel,
            gl_clear_colorx,
            &[
                pocket_gles::fixed::ONE as u32,
                0,
                0,
                pocket_gles::fixed::ONE as u32,
            ],
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_clear,
            &[pocket_gles::GL_COLOR_BUFFER_BIT],
        );
        let ok = call(
            &mut cpu,
            &mut kernel,
            egl_swap_buffers,
            &[DISPLAY_HANDLE, SURFACE_HANDLE],
        );
        assert_eq!(ok, EGL_TRUE);
        assert!(
            kernel.framebuffer.frame_counter > before,
            "swap must mark the framebuffer dirty"
        );
        // Pure red in RGB565 little-endian is 0xF800.
        assert_eq!(
            &kernel.framebuffer.pixels[0..2],
            &[0x00, 0xF8],
            "framebuffer should hold red"
        );
    }

    #[test]
    fn a_fixed_point_triangle_lands_in_the_framebuffer() {
        let _g = guard();
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        let (w, h) = (kernel.framebuffer.width, kernel.framebuffer.height);
        reset_for_test(w, h);

        // A GL_FIXED vertex array covering most of the viewport,
        // counter-clockwise so a default front-face setting keeps it.
        let verts: [f32; 9] = [
            -0.9, -0.9, 0.0, //
            0.9, -0.9, 0.0, //
            0.0, 0.9, 0.0,
        ];
        let bytes: Vec<u8> = verts
            .iter()
            .flat_map(|v| pocket_gles::fixed::from_f32(*v).to_le_bytes())
            .collect();
        cpu.write_mem(SCRATCH, &bytes).unwrap();

        call(&mut cpu, &mut kernel, gl_clear_colorx, &[0, 0, 0, 0]);
        call(
            &mut cpu,
            &mut kernel,
            gl_clear,
            &[pocket_gles::GL_COLOR_BUFFER_BIT],
        );
        // glColor4x(0, 1, 0, 1) — green.
        call(
            &mut cpu,
            &mut kernel,
            gl_color4x,
            &[
                0,
                pocket_gles::fixed::ONE as u32,
                0,
                pocket_gles::fixed::ONE as u32,
            ],
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_enable_client_state,
            &[pocket_gles::GL_VERTEX_ARRAY],
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_vertex_pointer,
            &[3, pocket_gles::GL_FIXED, 0, SCRATCH],
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_draw_arrays,
            &[pocket_gles::GL_TRIANGLES, 0, 3],
        );
        call(
            &mut cpu,
            &mut kernel,
            egl_swap_buffers,
            &[DISPLAY_HANDLE, SURFACE_HANDLE],
        );

        // The triangle's centroid is at the middle of the screen.
        let mid = ((h / 2) * w + w / 2) as usize * 2;
        let px = u16::from_le_bytes([
            kernel.framebuffer.pixels[mid],
            kernel.framebuffer.pixels[mid + 1],
        ]);
        assert_eq!(px, 0x07E0, "centre pixel should be pure green in RGB565");
        // A corner is outside the triangle and must stay cleared.
        assert_eq!(
            u16::from_le_bytes([kernel.framebuffer.pixels[0], kernel.framebuffer.pixels[1]]),
            0,
            "corner should still be black"
        );
    }

    #[test]
    fn get_string_hands_back_a_stable_guest_pointer() {
        let _g = guard();
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        cpu.map_region(0x5000_0000, 0x10000, Prot::READ | Prot::WRITE)
            .unwrap();
        let first = call(
            &mut cpu,
            &mut kernel,
            gl_get_string,
            &[pocket_gles::GL_VERSION],
        );
        assert_ne!(first, 0);
        let again = call(
            &mut cpu,
            &mut kernel,
            gl_get_string,
            &[pocket_gles::GL_VERSION],
        );
        assert_eq!(first, again, "the pointer must not move between calls");
        let bytes = cpu.read_mem(first, 17).unwrap();
        let text = String::from_utf8_lossy(&bytes[..16]).to_string();
        assert_eq!(text, "OpenGL ES-CL 1.1");
        assert_eq!(bytes[16], 0, "must be NUL-terminated");
    }

    #[test]
    fn gen_textures_writes_distinct_non_zero_names() {
        let _g = guard();
        reset_for_test(16, 16);
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        call(&mut cpu, &mut kernel, gl_gen_textures, &[3, SCRATCH]);
        let names: Vec<u32> = (0..3)
            .map(|i| cpu.read_u32_le(SCRATCH + i * 4).unwrap())
            .collect();
        assert!(names.iter().all(|&n| n != 0), "0 is not a texture name");
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "names must be distinct"
        );
    }

    #[test]
    fn a_fixed_point_matrix_is_read_without_transposing() {
        let _g = guard();
        reset_for_test(16, 16);
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        // Column-major translate(2, 3, 4): the translation sits in the
        // last column, i.e. elements 12..15.
        let mut m = [0f32; 16];
        m[0] = 1.0;
        m[5] = 1.0;
        m[10] = 1.0;
        m[15] = 1.0;
        m[12] = 2.0;
        m[13] = 3.0;
        m[14] = 4.0;
        let bytes: Vec<u8> = m
            .iter()
            .flat_map(|v| pocket_gles::fixed::from_f32(*v).to_le_bytes())
            .collect();
        cpu.write_mem(SCRATCH, &bytes).unwrap();
        call(
            &mut cpu,
            &mut kernel,
            gl_matrix_mode,
            &[pocket_gles::GL_MODELVIEW],
        );
        call(&mut cpu, &mut kernel, gl_load_matrixx, &[SCRATCH]);
        with_ctx(|c| {
            let cur = c.modelview.current();
            assert_eq!((cur[12], cur[13], cur[14]), (2.0, 3.0, 4.0));
        });
    }

    #[test]
    fn glgetintegerv_answers_the_queries_a_renderer_branches_on() {
        let _g = guard();
        reset_for_test(64, 64);
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        for (pname, want) in [
            (pocket_gles::GL_MAX_TEXTURE_SIZE, 1024u32),
            (pocket_gles::GL_MAX_TEXTURE_UNITS, 1),
            (pocket_gles::GL_DEPTH_BITS, 16),
            (
                pocket_gles::GL_NUM_COMPRESSED_TEXTURE_FORMATS,
                COMPRESSED_FORMATS.len() as u32,
            ),
        ] {
            call(&mut cpu, &mut kernel, gl_get_integerv, &[pname, SCRATCH]);
            assert_eq!(
                cpu.read_u32_le(SCRATCH).unwrap(),
                want,
                "pname 0x{pname:04x}"
            );
        }
        // GL_VIEWPORT writes four values.
        call(&mut cpu, &mut kernel, gl_viewport, &[0, 0, 240u32, 320u32]);
        call(
            &mut cpu,
            &mut kernel,
            gl_get_integerv,
            &[pocket_gles::GL_VIEWPORT, SCRATCH],
        );
        assert_eq!(cpu.read_u32_le(SCRATCH + 8).unwrap(), 240);
        assert_eq!(cpu.read_u32_le(SCRATCH + 12).unwrap(), 320);
    }

    #[test]
    fn fogx_takes_the_mode_as_an_enum_not_as_fixed_point() {
        let _g = guard();
        reset_for_test(320, 240);
        let (mut cpu, mut kernel) = (fresh_cpu(), fresh_kernel());

        // Xtrakt calls glFogx(GL_FOG_MODE, GL_LINEAR) during start-up.
        // 16.16-decoding the enum gives 9729/65536 = 0.148453, which
        // `set_fog` rejects — and the stray GL_INVALID_ENUM then fails
        // the game's next `glGetError`, its VBO capability probe.
        call(
            &mut cpu,
            &mut kernel,
            gl_fogx,
            &[pocket_gles::GL_FOG_MODE, pocket_gles::GL_LINEAR],
        );
        with_ctx(|c| {
            assert_eq!(c.take_error(), pocket_gles::GL_NO_ERROR);
            assert_eq!(c.state.fog_mode, pocket_gles::raster::FogMode::Linear);
        });

        // The scalar parameters really are fixed-point.
        call(
            &mut cpu,
            &mut kernel,
            gl_fogx,
            &[pocket_gles::GL_FOG_END, 0x0002_0000],
        );
        with_ctx(|c| {
            assert_eq!(c.take_error(), pocket_gles::GL_NO_ERROR);
            assert_eq!(c.state.fog_end, 2.0);
        });
    }

    #[test]
    fn fogf_takes_the_mode_as_a_float_encoded_enum() {
        let _g = guard();
        reset_for_test(320, 240);
        let (mut cpu, mut kernel) = (fresh_cpu(), fresh_kernel());

        // The Common profile spells the same enum as a float, so the
        // fixed-point exception must not leak into `glFogf`.
        call(
            &mut cpu,
            &mut kernel,
            gl_fogf,
            &[
                pocket_gles::GL_FOG_MODE,
                (pocket_gles::GL_EXP2 as f32).to_bits(),
            ],
        );
        with_ctx(|c| {
            assert_eq!(c.take_error(), pocket_gles::GL_NO_ERROR);
            assert_eq!(c.state.fog_mode, pocket_gles::raster::FogMode::Exp2);
        });
    }

    #[test]
    fn fogxv_reads_the_mode_as_an_enum_and_the_colour_as_fixed_point() {
        let _g = guard();
        reset_for_test(320, 240);
        let (mut cpu, mut kernel) = (fresh_cpu(), fresh_kernel());

        cpu.write_mem(SCRATCH, &pocket_gles::GL_EXP.to_le_bytes())
            .unwrap();
        call(
            &mut cpu,
            &mut kernel,
            gl_fogxv,
            &[pocket_gles::GL_FOG_MODE, SCRATCH],
        );
        with_ctx(|c| {
            assert_eq!(c.take_error(), pocket_gles::GL_NO_ERROR);
            assert_eq!(c.state.fog_mode, pocket_gles::raster::FogMode::Exp);
        });

        for (i, v) in [0x0001_0000u32, 0, 0x0000_8000, 0x0001_0000]
            .iter()
            .enumerate()
        {
            cpu.write_mem(SCRATCH + i as u32 * 4, &v.to_le_bytes())
                .unwrap();
        }
        call(
            &mut cpu,
            &mut kernel,
            gl_fogxv,
            &[pocket_gles::GL_FOG_COLOR, SCRATCH],
        );
        with_ctx(|c| assert_eq!(c.state.fog_color, [1.0, 0.0, 0.5, 1.0]));
    }

    #[test]
    fn load_library_and_get_module_handle_agree_on_the_gles_handle() {
        // The dynamic-export table is keyed by these handles, so a
        // mismatch would make every `GetProcAddress` return null.
        assert_eq!(
            crate::coredll::gles_module_handle("\\Windows\\libGLES_CL.dll"),
            Some(pocket_kernel::GLES_CL_MODULE_HANDLE)
        );
        assert_eq!(
            crate::coredll::gles_module_handle("libgles_cm"),
            Some(pocket_kernel::GLES_CM_MODULE_HANDLE)
        );
        assert_eq!(crate::coredll::gles_module_handle("gx.dll"), None);
    }

    /// Call of Duty 2 opens GAPI for its key mapping but renders only
    /// through EGL, so the synthetic framebuffer is mapped and stays
    /// zero-filled. `eglSwapBuffers` has to push the presented pixels
    /// into that mapping, otherwise the end-of-slice GAPI readback sees
    /// a difference, copies the zeros back and blanks every frame.
    #[test]
    fn swap_buffers_pushes_pixels_into_a_mapped_gapi_framebuffer() {
        let _g = guard();
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        let (w, h) = (kernel.framebuffer.width, kernel.framebuffer.height);
        cpu.map_region(
            pocket_kernel::SYNTHETIC_FRAMEBUFFER_BASE,
            kernel.framebuffer.pixels.len() as u32,
            Prot::READ | Prot::WRITE,
        )
        .unwrap();
        kernel.fb_mapped = true;

        // Paint the render target a colour that is not the zero-fill.
        with_ctx(|c| {
            c.target.resize(w, h);
            for px in c.target.color.chunks_exact_mut(4) {
                px.copy_from_slice(&[0xff, 0x00, 0x00, 0xff]);
            }
        });
        call(&mut cpu, &mut kernel, egl_swap_buffers, &[0, 0]);

        let presented = kernel.framebuffer.pixels.clone();
        assert!(
            presented.iter().any(|&b| b != 0),
            "swap should have presented non-black pixels"
        );
        let guest = cpu
            .read_mem(
                pocket_kernel::SYNTHETIC_FRAMEBUFFER_BASE,
                presented.len() as u32,
            )
            .unwrap();
        assert_eq!(
            guest, presented,
            "the guest GAPI mapping must match what was presented"
        );
    }
}
