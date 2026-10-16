//! OpenGL ES 1.x / EGL 1.0 dispatch for `libGLES_CM.dll` and `libGLES_CL.dll`.
//!
//! Every entry point is registered by name; the ordinal aliases are wired
//! in [`register`] by iterating the ordinal tables, mirroring what
//! [`super::WinCeDispatcher::new`] does for `coredll.dll`.

use pocket_gles::context::SurfaceBuffers;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

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

// ---- execution-thread context ----------------------------------------------
// All guest threads are scheduled by one host execution thread. Keep GL
// resources local to it so a later game cannot inherit textures, guest
// array pointers, matrices, depth buffers or pipeline state (nor another
// concurrently running session's state).
thread_local! {
    static SURFACES: RefCell<EglSurfaces> = RefCell::new(EglSurfaces::new());
    static CTX: RefCell<Context> = RefCell::new(Context::new(240, 320));
    static PACK_ALIGNMENT: Cell<usize> = const { Cell::new(4) };
    static UNPACK_ALIGNMENT: Cell<usize> = const { Cell::new(4) };
    static EGL_ERROR: Cell<u32> = const { Cell::new(EGL_SUCCESS) };
}

struct EglImage {
    width: u32,
    height: u32,
    pixmap: Option<u32>,
    deleted: bool,
    buffers: Option<SurfaceBuffers>,
}
struct EglSurfaces {
    images: HashMap<u32, EglImage>,
    active: u32,
    draw: u32,
    read: u32,
    bound: bool,
    next: u32,
}
impl EglSurfaces {
    fn new() -> Self {
        let mut images = HashMap::new();
        images.insert(
            SURFACE_HANDLE,
            EglImage {
                width: 240,
                height: 320,
                pixmap: None,
                deleted: false,
                buffers: None,
            },
        );
        Self {
            images,
            active: SURFACE_HANDLE,
            draw: SURFACE_HANDLE,
            read: SURFACE_HANDLE,
            bound: true,
            next: 0x45471000,
        }
    }
    fn select(&mut self, c: &mut Context, handle: u32) {
        if self.active == handle {
            return;
        }
        if handle == 0 {
            let mut destination = SurfaceBuffers::new(1, 1);
            c.park_surface(&mut destination);
            if let Some(old) = self.images.get_mut(&self.active) {
                old.buffers = Some(destination);
            }
            self.active = 0;
            return;
        }
        let mut destination = self
            .images
            .get_mut(&handle)
            .unwrap()
            .buffers
            .take()
            .unwrap();
        c.exchange_surface(&mut destination);
        if let Some(old) = self.images.get_mut(&self.active) {
            old.buffers = Some(destination);
        }
        self.active = handle;
    }
    fn collect(&mut self) {
        let draw = self.draw;
        let read = self.read;
        let active = self.active;
        self.images
            .retain(|h, s| !s.deleted || (*h == draw || *h == read || *h == active));
    }
}

fn with_read_ctx<R>(f: impl FnOnce(&mut Context) -> R) -> R {
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        with_ctx(|c| {
            let active = s.active;
            let read = s.read;
            if s.images.contains_key(&read) {
                s.select(c, read);
            }
            let result = f(c);
            s.select(c, active);
            result
        })
    })
}

fn flush_pixmap(ctx: &mut CallCtx<'_>, surface: u32) -> Result<(), KernelError> {
    let bitmap = SURFACES.with(|s| s.borrow().images.get(&surface).and_then(|s| s.pixmap));
    let Some(bitmap) = bitmap else {
        return Ok(());
    };
    let Some(bm) = ctx.kernel.gdi.bitmap_mut(bitmap) else {
        set_egl_error(pocket_gles::EGL_BAD_NATIVE_PIXMAP);
        return Ok(());
    };
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        with_ctx(|c| {
            let previous = s.active;
            s.select(c, surface);
            c.present_rgb565(&mut bm.pixels);
            bm.host_dirty = true;
            s.select(c, previous);
        })
    });
    crate::coredll::sync_bitmap_to_guest(ctx, bitmap)
}

/// Opt-in host GPU renderer. Must be called on the emulation thread.
pub fn set_anisotropic_filtering(level: u8) {
    with_ctx(|c| c.set_anisotropic_filtering(level));
}

pub fn set_gpu_renderer(enabled: bool) {
    with_ctx(|c| {
        if enabled {
            if let Err(e) = c.enable_gpu() {
                log::warn!("GLES GPU unavailable; using software: {e}");
            }
        } else {
            c.disable_gpu();
            SURFACES.with(|s| {
                for image in s.borrow_mut().images.values_mut() {
                    if let Some(b) = image.buffers.as_mut() {
                        b.disable_gpu();
                    }
                }
            });
        }
    });
}

pub(crate) fn release_gpu() {
    with_ctx(|c| c.release_gpu());
    SURFACES.with(|s| {
        for image in s.borrow_mut().images.values_mut() {
            if let Some(b) = image.buffers.as_mut() {
                b.release_gpu();
            }
        }
    });
}

/// Called once for a new dispatcher/session, including sequential CLI
/// runs reusing the same host thread. Dropping the old Context frees all
/// host-side GL resources; guest memory belongs to the new kernel/CPU.
pub(crate) fn reset_for_session() {
    with_ctx(|c| *c = Context::new(240, 320));
    SURFACES.with(|s| *s.borrow_mut() = EglSurfaces::new());
    EGL_ERROR.with(|error| error.set(EGL_SUCCESS));
    PACK_ALIGNMENT.with(|s| s.set(4));
    UNPACK_ALIGNMENT.with(|s| s.set(4));
}

pub(crate) struct SuspendedGles {
    context: Context,
    error: u32,
    pack: usize,
    unpack: usize,
    surfaces: EglSurfaces,
}

pub(crate) fn suspend() -> SuspendedGles {
    let context =
        CTX.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), Context::new(240, 320)));
    let error = EGL_ERROR.with(|slot| slot.replace(EGL_SUCCESS));
    let pack = PACK_ALIGNMENT.with(|s| s.replace(4));
    let unpack = UNPACK_ALIGNMENT.with(|s| s.replace(4));
    let surfaces = SURFACES.with(|s| std::mem::replace(&mut *s.borrow_mut(), EglSurfaces::new()));
    SuspendedGles {
        context,
        error,
        pack,
        unpack,
        surfaces,
    }
}

pub(crate) fn resume(saved: SuspendedGles) {
    CTX.with(|slot| *slot.borrow_mut() = saved.context);
    SURFACES.with(|slot| *slot.borrow_mut() = saved.surfaces);
    EGL_ERROR.with(|slot| slot.set(saved.error));
    PACK_ALIGNMENT.with(|s| s.set(saved.pack));
    UNPACK_ALIGNMENT.with(|s| s.set(saved.unpack));
}

/// Compressed formats we decode, reported through
/// `GL_COMPRESSED_TEXTURE_FORMATS` and the extension string.
const COMPRESSED_FORMATS: [u32; 16] = [
    pocket_gles::GL_ATC_RGB_AMD,
    pocket_gles::GL_ATC_RGBA_EXPLICIT_ALPHA_AMD,
    pocket_gles::GL_ATC_RGBA_INTERPOLATED_ALPHA_AMD,
    pocket_gles::GL_COMPRESSED_RGB_S3TC_DXT1_EXT,
    pocket_gles::GL_COMPRESSED_RGBA_S3TC_DXT1_EXT,
    pocket_gles::GL_COMPRESSED_RGBA_S3TC_DXT3_EXT,
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

/// Thin wrapper so `pocket_cpu::Cpu` satisfies `GuestMemory`.
struct CpuMem<'a>(&'a mut dyn pocket_cpu::Cpu);

impl GuestMemory for CpuMem<'_> {
    fn read(&mut self, addr: u32, len: usize, out: &mut Vec<u8>) -> bool {
        if u32::try_from(len).is_err() {
            return false;
        }
        out.resize(len, 0);
        if self.0.read_mem_into(addr, out).is_ok() {
            true
        } else {
            out.clear();
            false
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

/// Access the context owned by the current guest execution thread.
fn with_ctx<R>(f: impl FnOnce(&mut Context) -> R) -> R {
    CTX.with(|context| f(&mut context.borrow_mut()))
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
    if cap == pocket_gles::GL_FOG {
        log::debug!("GLES enable fog");
    }
    with_ctx(|c| c.set_capability(cap, true));
    Ok(VOID)
}

fn gl_disable(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let cap = ctx.arg_u32(0)?;
    if cap == pocket_gles::GL_FOG {
        log::debug!("GLES disable fog");
    }
    with_ctx(|c| c.set_capability(cap, false));
    Ok(VOID)
}

fn gl_enable_client_state(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let array = ctx.arg_u32(0)?;
    with_ctx(|c| c.set_client_state(array, true));
    Ok(VOID)
}

fn gl_disable_client_state(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let array = ctx.arg_u32(0)?;
    with_ctx(|c| c.set_client_state(array, false));
    Ok(VOID)
}

fn gl_point_size(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let value = argf(ctx, 0)?;
    with_ctx(|c| c.set_point_size(value));
    Ok(VOID)
}
fn gl_point_sizex(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let value = argx(ctx, 0)?;
    with_ctx(|c| c.set_point_size(value));
    Ok(VOID)
}
fn gl_line_width(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let value = argf(ctx, 0)?;
    with_ctx(|c| c.set_line_width(value));
    Ok(VOID)
}
fn gl_line_widthx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let value = argx(ctx, 0)?;
    with_ctx(|c| c.set_line_width(value));
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
    Ok(VOID)
}

fn gl_color4x(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let c = [argx(ctx, 0)?, argx(ctx, 1)?, argx(ctx, 2)?, argx(ctx, 3)?];
    with_ctx(|g| g.current_color = c);
    Ok(VOID)
}

fn set_lighting(
    ctx: &mut CallCtx<'_>,
    family: u8,
    fixed: bool,
    vector: bool,
) -> Result<DispatchOutcome, KernelError> {
    use pocket_gles::lighting::{self, Target};
    let target = match family {
        0 => Target::Light(ctx.arg_u32(0)?),
        1 => Target::Material(ctx.arg_u32(0)?),
        _ => Target::Model,
    };
    let pindex = if family == 2 { 0 } else { 1 };
    let pname = ctx.arg_u32(pindex)?;
    let count = match lighting::parameter_count(target, pname, !vector) {
        Ok(n) => n,
        Err(e) => {
            with_ctx(|c| c.set_error(e));
            return Ok(VOID);
        }
    };
    let words = if vector {
        let ptr = ctx.arg_u32(pindex + 1)?;
        match ctx.cpu.read_mem(ptr, (count * 4) as u32) {
            Ok(bytes) => bytes
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .collect::<Vec<_>>(),
            Err(_) => {
                with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_OPERATION));
                return Ok(VOID);
            }
        }
    } else {
        vec![ctx.arg_u32(pindex + 1)?]
    };
    let values: Vec<f32> = words
        .into_iter()
        .map(|w| {
            if fixed {
                word_to_f32(w)
            } else {
                f32::from_bits(w)
            }
        })
        .collect();
    with_ctx(|c| {
        if let Err(e) = c
            .lighting
            .set(target, pname, &values, c.modelview.current())
        {
            c.set_error(e);
        }
    });
    Ok(VOID)
}
macro_rules! lighting_setter {
    ($name:ident,$family:expr,$fixed:expr,$vector:expr) => {
        fn $name(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
            set_lighting(ctx, $family, $fixed, $vector)
        }
    };
}
lighting_setter!(gl_lightf, 0, false, false);
lighting_setter!(gl_lightfv, 0, false, true);
lighting_setter!(gl_lightx, 0, true, false);
lighting_setter!(gl_lightxv, 0, true, true);
lighting_setter!(gl_materialf, 1, false, false);
lighting_setter!(gl_materialfv, 1, false, true);
lighting_setter!(gl_materialx, 1, true, false);
lighting_setter!(gl_materialxv, 1, true, true);
lighting_setter!(gl_light_modelf, 2, false, false);
lighting_setter!(gl_light_modelfv, 2, false, true);
lighting_setter!(gl_light_modelx, 2, true, false);
lighting_setter!(gl_light_modelxv, 2, true, true);

fn get_lighting(
    ctx: &mut CallCtx<'_>,
    material: bool,
    fixed: bool,
) -> Result<DispatchOutcome, KernelError> {
    use pocket_gles::lighting::Target;
    let id = ctx.arg_u32(0)?;
    let pname = ctx.arg_u32(1)?;
    let out = ctx.arg_u32(2)?;
    let target = if material {
        if id != 0x0404 && id != 0x0405 {
            with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_ENUM));
            return Ok(VOID);
        }
        Target::Material(0x0408)
    } else {
        Target::Light(id)
    };
    let values = match with_ctx(|c| {
        if material
            && c.lighting.color_material
            && matches!(
                pname,
                pocket_gles::lighting::AMBIENT | pocket_gles::lighting::DIFFUSE
            )
        {
            Ok(c.current_color.to_vec())
        } else {
            c.lighting.get(target, pname)
        }
    }) {
        Ok(v) => v,
        Err(e) => {
            with_ctx(|c| c.set_error(e));
            return Ok(VOID);
        }
    };
    let bytes: Vec<u8> = values
        .into_iter()
        .flat_map(|v| {
            if fixed {
                pocket_gles::fixed::from_f32(v).to_le_bytes()
            } else {
                v.to_bits().to_le_bytes()
            }
        })
        .collect();
    if ctx.cpu.write_mem(out, &bytes).is_err() {
        with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_OPERATION));
    }
    Ok(VOID)
}
fn gl_get_lightfv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    get_lighting(ctx, false, false)
}
fn gl_get_lightxv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    get_lighting(ctx, false, true)
}
fn gl_get_materialfv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    get_lighting(ctx, true, false)
}
fn gl_get_materialxv(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    get_lighting(ctx, true, true)
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
    // All native surface contexts share the guest texture namespace.
    SURFACES.with(|s| {
        for image in s.borrow_mut().images.values_mut() {
            if let Some(buffers) = image.buffers.as_mut() {
                buffers.delete_textures(&names);
            }
        }
    });
    Ok(VOID)
}

fn gl_bind_texture(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (target, name) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    with_ctx(|c| c.bind_texture(target, name));
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

fn gl_polygon_offset(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let factor = f32::from_bits(ctx.arg_u32(0)?);
    let units = f32::from_bits(ctx.arg_u32(1)?);
    with_ctx(|c| {
        c.state.polygon_factor = factor;
        c.state.polygon_units = units;
    });
    Ok(VOID)
}
fn gl_polygon_offsetx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let factor = ctx.arg_u32(0)? as i32 as f32 / 65536.;
    let units = ctx.arg_u32(1)? as i32 as f32 / 65536.;
    with_ctx(|c| {
        c.state.polygon_factor = factor;
        c.state.polygon_units = units;
    });
    Ok(VOID)
}
fn gl_copy_tex_image_2d(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let target = ctx.arg_u32(0)?;
    let level = ctx.arg_u32(1)? as i32;
    let format = ctx.arg_u32(2)?;
    let x = ctx.arg_u32(3)? as i32;
    let y = ctx.arg_u32(4)? as i32;
    let width = ctx.arg_u32(5)? as i32;
    let height = ctx.arg_u32(6)? as i32;
    let border = ctx.arg_u32(7)? as i32;
    with_read_ctx(|c| c.copy_tex_image_2d(target, level, format, x, y, width, height, border));
    Ok(VOID)
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
    with_ctx(|c| c.tex_image_2d(target, level, width, height, format, ty, &data));
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
    if width > 1024 || height > 1024 {
        with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_VALUE));
        return Ok(Vec::new());
    }
    let row = width as usize * bpp as usize;
    let align = UNPACK_ALIGNMENT.with(Cell::get);
    let stride = row.div_ceil(align) * align;
    let len = if height == 0 {
        0
    } else {
        stride * (height as usize - 1) + row
    };
    let Ok(bytes) = ctx.cpu.read_mem(pixels, len as u32) else {
        return Ok(Vec::new());
    };
    let mut tight = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        tight.extend_from_slice(&bytes[y * stride..y * stride + row]);
    }
    Ok(tight)
}

fn gl_tex_parameterf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (target, pname) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?);
    // The filters and wrap modes are enums even in the float entry
    // point, so the guest passes them as a float-encoded enum value.
    let value = argf(ctx, 2)?;
    with_ctx(|c| {
        if pname == pocket_gles::GL_TEXTURE_MAX_LEVEL_SGIS && (!value.is_finite() || value < 0.) {
            c.set_error(pocket_gles::GL_INVALID_VALUE);
        } else {
            c.tex_parameter(target, pname, value as u32);
        }
    });
    Ok(VOID)
}

fn gl_tex_parameterx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (target, pname, value) = (ctx.arg_u32(0)?, ctx.arg_u32(1)?, ctx.arg_u32(2)?);
    with_ctx(|c| c.tex_parameter(target, pname, value));
    Ok(VOID)
}

fn gl_tex_envf(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pname = ctx.arg_u32(1)?;
    let value = argf(ctx, 2)?;
    with_ctx(|c| {
        if pname == pocket_gles::GL_TEXTURE_LOD_BIAS_EXT {
            c.set_lod_bias(value)
        } else {
            c.tex_env(pname, value as u32)
        }
    });
    Ok(VOID)
}
fn gl_tex_envx(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pname = ctx.arg_u32(1)?;
    let value = ctx.arg_u32(2)?;
    with_ctx(|c| {
        if pname == pocket_gles::GL_TEXTURE_LOD_BIAS_EXT {
            c.set_lod_bias(word_to_f32(value))
        } else {
            c.tex_env(pname, value)
        }
    });
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
        with_ctx(|c| {
            if pname == pocket_gles::GL_TEXTURE_LOD_BIAS_EXT {
                c.set_lod_bias(decode(word))
            } else {
                c.tex_env(
                    pname,
                    if std::ptr::fn_addr_eq(decode, word_to_f32 as fn(u32) -> f32) {
                        word
                    } else {
                        decode(word) as u32
                    },
                )
            }
        });
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
    let mut mem = CpuMem(ctx.cpu);
    with_ctx(|c| c.draw_arrays(&mut mem, mode, first, count));
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
    let mut mem = CpuMem(ctx.cpu);
    with_ctx(|c| c.draw_elements_from_guest(&mut mem, mode, count, ty, ptr));
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
        0x8037 => vec![with_ctx(|c| c.state.polygon_offset) as i32],
        0x8038 => vec![with_ctx(|c| c.state.polygon_factor).round() as i32],
        0x2a00 => vec![with_ctx(|c| c.state.polygon_units).round() as i32],
        0x0b11 => vec![with_ctx(|c| c.state.point_size).round() as i32],
        0x0b21 => vec![with_ctx(|c| c.state.line_width).round() as i32],
        0x846d | 0x846e => with_ctx(|c| c.primitive_size_range(pname == 0x846d))
            .map(|x| x.round() as i32)
            .to_vec(),
        pocket_gles::GL_MAX_TEXTURE_SIZE => vec![1024],
        pocket_gles::GL_MAX_LIGHTS => vec![8],
        pocket_gles::GL_LIGHTING => vec![with_ctx(|c| c.lighting.enabled) as i32],
        pocket_gles::GL_NORMALIZE => vec![with_ctx(|c| c.lighting.normalize) as i32],
        pocket_gles::GL_COLOR_MATERIAL => vec![with_ctx(|c| c.lighting.color_material) as i32],
        pocket_gles::lighting::RESCALE_NORMAL => vec![with_ctx(|c| c.lighting.rescale) as i32],
        pocket_gles::lighting::MODEL_TWO_SIDE => vec![with_ctx(|c| c.lighting.two_sided) as i32],
        0x4000..=0x4007 => {
            vec![with_ctx(|c| c.lighting.lights[(pname - 0x4000) as usize].enabled) as i32]
        }
        pocket_gles::GL_MAX_TEXTURE_UNITS => vec![pocket_gles::raster::MAX_TEXTURE_STAGES as i32],
        pocket_gles::GL_MAX_MODELVIEW_STACK_DEPTH => vec![16],
        pocket_gles::GL_MAX_PROJECTION_STACK_DEPTH => vec![16],
        pocket_gles::GL_MAX_TEXTURE_STACK_DEPTH => vec![16],
        pocket_gles::GL_DEPTH_BITS => vec![16],
        pocket_gles::GL_STENCIL_BITS => vec![0],
        pocket_gles::GL_RED_BITS => vec![5],
        pocket_gles::GL_GREEN_BITS => vec![6],
        pocket_gles::GL_BLUE_BITS => vec![5],
        pocket_gles::GL_ALPHA_BITS => vec![0],
        pocket_gles::GL_PACK_ALIGNMENT => vec![PACK_ALIGNMENT.with(Cell::get) as i32],
        pocket_gles::GL_UNPACK_ALIGNMENT => vec![UNPACK_ALIGNMENT.with(Cell::get) as i32],
        pocket_gles::GL_MAX_TEXTURE_LOD_BIAS_EXT => vec![4],
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
        0x8037 => vec![with_ctx(|c| c.state.polygon_offset) as u8 as f32],
        0x8038 => vec![with_ctx(|c| c.state.polygon_factor)],
        0x2a00 => vec![with_ctx(|c| c.state.polygon_units)],
        0x0b11 => vec![with_ctx(|c| c.state.point_size)],
        0x0b21 => vec![with_ctx(|c| c.state.line_width)],
        0x846d | 0x846e => with_ctx(|c| c.primitive_size_range(pname == 0x846d)).to_vec(),
        pocket_gles::GL_MODELVIEW_MATRIX => with_ctx(|c| c.modelview.current().to_vec()),
        pocket_gles::GL_PROJECTION_MATRIX => with_ctx(|c| c.projection.current().to_vec()),
        pocket_gles::GL_TEXTURE_MATRIX => with_ctx(|c| {
            c.texture_matrix[c.active_texture as usize]
                .current()
                .to_vec()
        }),
        pocket_gles::GL_CURRENT_COLOR => with_ctx(|c| c.current_color.to_vec()),
        pocket_gles::GL_DEPTH_RANGE => {
            let (n, f) = with_ctx(|c| c.state.depth_range);
            vec![n, f]
        }
        // Integer-valued queries are legal through glGetFloatv as well;
        // GL converts. Only the few games actually ask for are listed.
        pocket_gles::GL_MAX_TEXTURE_SIZE => vec![1024.0],
        pocket_gles::GL_MAX_LIGHTS => vec![8.0],
        pocket_gles::GL_LIGHTING => vec![with_ctx(|c| c.lighting.enabled) as u8 as f32],
        pocket_gles::lighting::MODEL_AMBIENT => with_ctx(|c| c.lighting.model_ambient.to_vec()),
        pocket_gles::lighting::MODEL_TWO_SIDE => {
            vec![with_ctx(|c| c.lighting.two_sided) as u8 as f32]
        }
        pocket_gles::GL_MAX_TEXTURE_UNITS => vec![pocket_gles::raster::MAX_TEXTURE_STAGES as f32],
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
            "GL_AMD_compressed_ATC_texture GL_ATI_texture_compression_atitc GL_EXT_texture_compression_s3tc GL_EXT_texture_compression_dxt1 GL_EXT_texture_lod_bias GL_SGIS_texture_lod GL_OES_compressed_paletted_texture GL_OES_query_matrix"
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

/// `glReadPixels(x, y, width, height, format, type, pixels)`.
///
/// The software renderer stores RGBA8888 with the top row first, while
/// OpenGL's origin is the lower-left.  Besides the ES baseline
/// RGBA/UNSIGNED_BYTE path, Gizmondo titles use the device driver's native
/// RGB/UNSIGNED_SHORT_5_6_5 readback path (notably Supernaturals when it
/// captures the framebuffer into a 256x256 texture).
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

    let bytes_per_pixel = match (format, ty) {
        (pocket_gles::GL_RGBA, GL_UNSIGNED_BYTE) => 4usize,
        (pocket_gles::GL_RGB, GL_UNSIGNED_BYTE) => 3usize,
        (pocket_gles::GL_RGB, pocket_gles::GL_UNSIGNED_SHORT_5_6_5) => 2usize,
        _ => {
            with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_ENUM));
            return Ok(VOID);
        }
    };

    if w > 4096 || h > 4096 {
        with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_VALUE));
        return Ok(VOID);
    }
    let alignment = PACK_ALIGNMENT.with(Cell::get);
    let stride = (w as usize * bytes_per_pixel).div_ceil(alignment) * alignment;
    let buf = with_read_ctx(|c| {
        c.sync_gpu();
        let mut buf = vec![0u8; stride * h as usize];
        for row in 0..h {
            // GL row 0 is the bottom of the image.
            let src_y = c.target.height as i64 - 1 - (y as i64 + row as i64);
            if src_y < 0 || src_y >= c.target.height as i64 {
                continue;
            }
            for col in 0..w {
                let src_x = x as i64 + col as i64;
                if src_x < 0 || src_x >= c.target.width as i64 {
                    continue;
                }
                let s = ((src_y as u32 * c.target.width + src_x as u32) * 4) as usize;
                match (format, ty) {
                    (pocket_gles::GL_RGBA, GL_UNSIGNED_BYTE) => {
                        let d = row as usize * stride + col as usize * 4;
                        buf[d..d + 4].copy_from_slice(&c.target.color[s..s + 4]);
                        // EGL advertises RGB565 with no alpha channel. The
                        // internal RGBA target retains alpha for rendering,
                        // but reading a missing surface component returns 1.
                        // Stuntcar Extreme captures its HUD labels this way:
                        // leaking clear-alpha zero makes its image conversion
                        // blend the blue colour key to opaque black.
                        buf[d + 3] = 255;
                    }
                    (pocket_gles::GL_RGB, GL_UNSIGNED_BYTE) => {
                        let d = row as usize * stride + col as usize * 3;
                        buf[d..d + 3].copy_from_slice(&c.target.color[s..s + 3]);
                    }
                    (pocket_gles::GL_RGB, pocket_gles::GL_UNSIGNED_SHORT_5_6_5) => {
                        let r = c.target.color[s] as u16;
                        let g = c.target.color[s + 1] as u16;
                        let b = c.target.color[s + 2] as u16;
                        let rgb565 = ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3);
                        let d = row as usize * stride + col as usize * 2;
                        buf[d..d + 2].copy_from_slice(&rgb565.to_le_bytes());
                    }
                    _ => unreachable!(),
                }
            }
        }
        buf
    });
    for row in 0..h as usize {
        let Some(address) = out.checked_add((row * stride) as u32) else {
            with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_VALUE));
            return Ok(VOID);
        };
        ctx.cpu.write_mem(
            address,
            &buf[row * stride..row * stride + w as usize * bytes_per_pixel],
        )?;
    }
    Ok(VOID)
}

// ---- accepted and ignored --------------------------------------------------

/// Entry points whose effect our rasterizer has no equivalent for.
/// Silently succeeding matches what a device without the feature does
/// and keeps the guest from bailing out on a spurious `GL_INVALID_*`.
fn gl_pixel_store(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pname = ctx.arg_u32(0)?;
    let value = ctx.arg_u32(1)?;
    if !matches!(
        pname,
        pocket_gles::GL_PACK_ALIGNMENT | pocket_gles::GL_UNPACK_ALIGNMENT
    ) {
        with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_ENUM));
    } else if !matches!(value, 1 | 2 | 4 | 8) {
        with_ctx(|c| c.set_error(pocket_gles::GL_INVALID_VALUE));
    } else if pname == pocket_gles::GL_PACK_ALIGNMENT {
        PACK_ALIGNMENT.with(|s| s.set(value as usize));
    } else {
        UNPACK_ALIGNMENT.with(|s| s.set(value as usize));
    }
    Ok(VOID)
}

fn gl_ignored(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
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
    Ok(VOID)
}

/// Compressed subimages and framebuffer-copy subimages remain unsupported.
/// Full framebuffer copies are handled by gl_copy_tex_image_2d.
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

/// Sticky EGL error, independent of the GL one and scoped to the run.
fn set_egl_error(code: u32) {
    EGL_ERROR.with(|error| {
        if error.get() == EGL_SUCCESS {
            error.set(code);
        }
    });
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
    if with_ctx(|c| c.gpu_requested()) {
        set_gpu_renderer(true);
    }
    log::info!(
        "eglInitialize() -> EGL 1.1 (PocketHLE {})",
        with_ctx(|c| if c.gpu_active() {
            "host GPU"
        } else {
            "software"
        })
    );
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

fn egl_terminate(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    release_gpu();
    let (w, h) = with_ctx(|c| {
        let w = c.target.width;
        let h = c.target.height;
        c.target = pocket_gles::raster::RenderTarget::new(w, h);
        (w, h)
    });
    SURFACES.with(|s| {
        let mut fresh = EglSurfaces::new();
        let image = fresh.images.get_mut(&SURFACE_HANDLE).unwrap();
        image.width = w;
        image.height = h;
        *s.borrow_mut() = fresh;
    });
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
        EGL_SURFACE_TYPE => {
            (EGL_WINDOW_BIT | pocket_gles::EGL_PBUFFER_BIT | pocket_gles::EGL_PIXMAP_BIT) as i32
        }
        EGL_CONFIG_ID => 1,
        EGL_RENDERABLE_TYPE => 1, // EGL_OPENGL_ES_BIT
        pocket_gles::EGL_CONFIG_CAVEAT | pocket_gles::EGL_TRANSPARENT_TYPE => EGL_NONE as i32,
        pocket_gles::EGL_NATIVE_RENDERABLE => EGL_FALSE as i32,
        pocket_gles::EGL_LEVEL => 0,
        pocket_gles::EGL_MAX_PBUFFER_WIDTH => 1024,
        pocket_gles::EGL_MAX_PBUFFER_HEIGHT => 1024,
        pocket_gles::EGL_MAX_PBUFFER_PIXELS => 1024 * 1024,
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
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        let active = s.active;
        let image = s.images.entry(SURFACE_HANDLE).or_insert(EglImage {
            width: w,
            height: h,
            pixmap: None,
            deleted: false,
            buffers: Some(SurfaceBuffers::new(w, h)),
        });
        image.width = w;
        image.height = h;
        image.deleted = false;
        if active == SURFACE_HANDLE {
            with_ctx(|c| {
                c.target.resize(w, h);
                c.set_viewport(0, 0, w as i32, h as i32);
            });
        } else if let Some(b) = image.buffers.as_mut() {
            b.target.resize(w, h);
        }
    });
    log::info!("eglCreateWindowSurface() -> {w}×{h} RGB565");
    Ok(DispatchOutcome::ReturnedR0(SURFACE_HANDLE))
}

fn egl_create_context(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(CONTEXT_HANDLE))
}

fn egl_surface_args(ctx: &mut CallCtx<'_>) -> Result<bool, KernelError> {
    if ctx.arg_u32(0)? != DISPLAY_HANDLE {
        set_egl_error(EGL_BAD_DISPLAY);
        return Ok(false);
    }
    if ctx.arg_u32(1)? != CONFIG_HANDLE {
        set_egl_error(pocket_gles::EGL_BAD_CONFIG);
        return Ok(false);
    }
    Ok(true)
}
fn allocate_surface(width: u32, height: u32, pixmap: Option<u32>) -> u32 {
    if width == 0 || height == 0 || width > 1024 || height > 1024 {
        set_egl_error(pocket_gles::EGL_BAD_PARAMETER);
        return EGL_NO_SURFACE;
    }
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        let total: u64 = s
            .images
            .values()
            .map(|i| i.width as u64 * i.height as u64)
            .sum();
        if s.images.len() >= 32 || total + width as u64 * height as u64 > 4 * 1024 * 1024 {
            set_egl_error(pocket_gles::EGL_BAD_ALLOC);
            return EGL_NO_SURFACE;
        }
        let handle = s.next;
        s.next += 1;
        s.images.insert(
            handle,
            EglImage {
                width,
                height,
                pixmap,
                deleted: false,
                buffers: Some(SurfaceBuffers::new(width, height)),
            },
        );
        handle
    })
}
fn egl_create_pbuffer(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if !egl_surface_args(ctx)? {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut ptr = ctx.arg_u32(2)?;
    let (mut w, mut h) = (0, 0);
    let mut ended = ptr == 0;
    for _ in 0..32 {
        if ptr == 0 {
            break;
        }
        let key = ctx.cpu.read_u32_le(ptr)?;
        if key == EGL_NONE {
            ended = true;
            break;
        }
        let value = ctx.cpu.read_u32_le(ptr + 4)?;
        match key {
            pocket_gles::EGL_WIDTH => w = value,
            pocket_gles::EGL_HEIGHT => h = value,
            pocket_gles::EGL_LARGEST_PBUFFER => {}
            _ => {
                set_egl_error(pocket_gles::EGL_BAD_ATTRIBUTE);
                return Ok(DispatchOutcome::ReturnedR0(0));
            }
        }
        let Some(next) = ptr.checked_add(8) else {
            break;
        };
        ptr = next;
    }
    if !ended {
        set_egl_error(pocket_gles::EGL_BAD_ATTRIBUTE);
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    Ok(DispatchOutcome::ReturnedR0(allocate_surface(w, h, None)))
}
fn egl_create_pixmap(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if !egl_surface_args(ctx)? {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let bitmap = ctx.arg_u32(2)?;
    let Some(bm) = ctx.kernel.gdi.bitmap(bitmap) else {
        set_egl_error(pocket_gles::EGL_BAD_NATIVE_PIXMAP);
        return Ok(DispatchOutcome::ReturnedR0(0));
    };
    let (w, h) = (bm.width, bm.height);
    Ok(DispatchOutcome::ReturnedR0(allocate_surface(
        w,
        h,
        Some(bitmap),
    )))
}
fn egl_destroy_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let surface = ctx.arg_u32(1)?;
    flush_pixmap(ctx, surface)?;
    let ok = SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(i) = s.images.get_mut(&surface) {
            i.deleted = true;
            s.collect();
            true
        } else {
            false
        }
    });
    if !ok {
        set_egl_error(pocket_gles::EGL_BAD_SURFACE);
    }
    Ok(DispatchOutcome::ReturnedR0(ok as u32))
}
fn egl_destroy_context(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    release_gpu();
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}
fn egl_make_current(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let (dpy, draw, read, context) = (
        ctx.arg_u32(0)?,
        ctx.arg_u32(1)?,
        ctx.arg_u32(2)?,
        ctx.arg_u32(3)?,
    );
    if dpy != DISPLAY_HANDLE {
        set_egl_error(EGL_BAD_DISPLAY);
        return Ok(DispatchOutcome::ReturnedR0(EGL_FALSE));
    }
    let old = SURFACES.with(|s| s.borrow().draw);
    flush_pixmap(ctx, old)?;
    let ok = SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        if context == 0 && draw == 0 && read == 0 {
            let mut empty = SurfaceBuffers::new(1, 1);
            with_ctx(|c| c.park_surface(&mut empty));
            let active = s.active;
            if let Some(image) = s.images.get_mut(&active) {
                image.buffers = Some(empty);
            }
            s.active = 0;
            s.bound = false;
            s.draw = 0;
            s.read = 0;
            s.collect();
            return true;
        }
        if context != CONTEXT_HANDLE {
            set_egl_error(pocket_gles::EGL_BAD_CONTEXT);
            return false;
        }
        if [draw, read]
            .iter()
            .any(|h| s.images.get(h).is_none_or(|i| i.deleted))
        {
            set_egl_error(pocket_gles::EGL_BAD_SURFACE);
            return false;
        }
        with_ctx(|c| s.select(c, draw));
        s.draw = draw;
        s.read = read;
        s.bound = true;
        s.collect();
        true
    });
    Ok(DispatchOutcome::ReturnedR0(ok as u32))
}

fn egl_get_current_display(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(SURFACES.with(|s| {
        if s.borrow().bound {
            DISPLAY_HANDLE
        } else {
            0
        }
    })))
}

fn egl_get_current_context(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(SURFACES.with(|s| {
        if s.borrow().bound {
            CONTEXT_HANDLE
        } else {
            0
        }
    })))
}

fn egl_get_current_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let selector = ctx.arg_u32(0)?;
    let surface = SURFACES.with(|s| {
        let s = s.borrow();
        match selector {
            pocket_gles::EGL_DRAW => s.draw,
            pocket_gles::EGL_READ => s.read,
            _ => {
                set_egl_error(pocket_gles::EGL_BAD_PARAMETER);
                0
            }
        }
    });
    Ok(DispatchOutcome::ReturnedR0(surface))
}

fn egl_get_error(_ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(
        EGL_ERROR.with(|error| error.replace(EGL_SUCCESS)),
    ))
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
    let surface = ctx.arg_u32(1)?;
    let dims = SURFACES.with(|s| s.borrow().images.get(&surface).map(|i| (i.width, i.height)));
    let Some((w, h)) = dims else {
        set_egl_error(pocket_gles::EGL_BAD_SURFACE);
        return Ok(DispatchOutcome::ReturnedR0(EGL_FALSE));
    };
    let attribute = ctx.arg_u32(2)?;
    let out = ctx.arg_u32(3)?;
    let value: i32 = match attribute {
        pocket_gles::EGL_WIDTH => w as i32,
        pocket_gles::EGL_HEIGHT => h as i32,
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
pub(crate) fn egl_swap_buffers(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let draw = SURFACES.with(|s| s.borrow().draw);
    if draw != SURFACE_HANDLE {
        flush_pixmap(ctx, draw)?;
        with_ctx(|c| c.sync_gpu());
        return Ok(DispatchOutcome::ReturnedR0(EGL_TRUE));
    }
    // Frontend snapshot throttling does not pace guest execution. Battlestations
    // runs its simulation too quickly once GPU rendering removes the incidental
    // software rasterization cost. Use the same modeled 60 Hz LCD as DirectDraw,
    // including cooperative retries and one-shot credit for explicit blank waits.
    // Offscreen swaps above are synchronization, not display presentations.
    if let Some(outcome) = crate::ddraw::wait_display(ctx, None)? {
        return Ok(outcome);
    }
    let fb_len = ctx.kernel.framebuffer.pixels.len();
    let (w, h) = (ctx.kernel.framebuffer.width, ctx.kernel.framebuffer.height);
    with_ctx(|c| {
        // A guest that never called eglCreateWindowSurface — or a
        // frontend that resized the window since — leaves the two out
        // of step. Match the framebuffer, since that is what is shown.
        if c.target.width != w || c.target.height != h {
            c.target.resize(w, h);
        }
        c.present_rgb565(&mut ctx.kernel.framebuffer.pixels[..fb_len]);
    });
    ctx.kernel.framebuffer.mark_dirty();
    // The conversion above replaces the framebuffer, including any video
    // composed by the message pump. Invalidate that presentation before
    // DirectShow checks its cached frame, even if no new video frame is due.
    crate::directshow::service(ctx)?;
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
    ctx.kernel.framebuffer.directdraw.last_present = Some(std::time::Instant::now());
    log::trace!(
        "eglSwapBuffers() -> frame {}",
        ctx.kernel.framebuffer.frame_counter
    );
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

/// Synchronize rendering without presenting or advancing the display clock.
fn egl_wait(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let surface = SURFACES.with(|s| s.borrow().draw);
    flush_pixmap(ctx, surface)?;
    with_ctx(|c| c.sync_gpu());
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}
fn egl_copy_buffers(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let surface = ctx.arg_u32(1)?;
    let bitmap = ctx.arg_u32(2)?;
    let dims = SURFACES.with(|s| s.borrow().images.get(&surface).map(|i| (i.width, i.height)));
    let Some((w, h)) = dims else {
        set_egl_error(pocket_gles::EGL_BAD_SURFACE);
        return Ok(DispatchOutcome::ReturnedR0(EGL_FALSE));
    };
    let Some(bm) = ctx.kernel.gdi.bitmap_mut(bitmap) else {
        set_egl_error(pocket_gles::EGL_BAD_NATIVE_PIXMAP);
        return Ok(DispatchOutcome::ReturnedR0(EGL_FALSE));
    };
    if (bm.width, bm.height) != (w, h) {
        set_egl_error(pocket_gles::EGL_BAD_MATCH);
        return Ok(DispatchOutcome::ReturnedR0(EGL_FALSE));
    }
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        with_ctx(|c| {
            let active = s.active;
            s.select(c, surface);
            c.present_rgb565(&mut bm.pixels);
            bm.host_dirty = true;
            s.select(c, active);
        })
    });
    crate::coredll::sync_bitmap_to_guest(ctx, bitmap)?;
    Ok(DispatchOutcome::ReturnedR0(EGL_TRUE))
}

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

/// Resolve only known callable GLES thunks, without modifying ordinal tables.
fn egl_get_proc_address(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let pointer = ctx.arg_u32(0)?;
    if pointer == 0 {
        return Ok(DispatchOutcome::ReturnedR0(0));
    }
    let mut bytes = Vec::new();
    for index in 0..256u32 {
        let byte = ctx.cpu.read_u8(pointer + index)?;
        if byte == 0 {
            break;
        }
        bytes.push(byte);
    }
    let name = String::from_utf8_lossy(&bytes);
    let address = [
        pocket_kernel::GLES_CM_MODULE_HANDLE,
        pocket_kernel::GLES_CL_MODULE_HANDLE,
    ]
    .into_iter()
    .find_map(|module| {
        ctx.kernel
            .dynamic_exports
            .get(&module)
            .and_then(|exports| exports.get(name.as_ref()))
            .copied()
    })
    .unwrap_or(0);
    Ok(DispatchOutcome::ReturnedR0(address))
}

fn query_matrix_parts(values: &[f32]) -> ([i32; 16], [i32; 16], u32) {
    let mut mantissa = [0; 16];
    let mut exponent = [0; 16];
    let mut status = 0;
    for (index, &value) in values.iter().take(16).enumerate() {
        if !value.is_finite() {
            status |= 1 << index;
            continue;
        }
        if value == 0.0 {
            continue;
        }
        let value = f64::from(value);
        let power = value.abs().log2().floor() as i32 + 1;
        mantissa[index] = (value * 2f64.powi(-power) * 65536.0).round() as i32;
        exponent[index] = power;
    }
    (mantissa, exponent, status)
}

fn gl_query_matrix_x_oes(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let output = ctx.arg_u32(0)?;
    let powers = ctx.arg_u32(1)?;
    let values = with_ctx(|c| match c.matrix_mode {
        pocket_gles::matrix::MatrixMode::Modelview => c.modelview.current().to_vec(),
        pocket_gles::matrix::MatrixMode::Projection => c.projection.current().to_vec(),
        pocket_gles::matrix::MatrixMode::Texture => c.texture_matrix[c.active_texture as usize]
            .current()
            .to_vec(),
    });
    let (mantissa, exponent, status) = query_matrix_parts(&values);
    for index in 0..16 {
        ctx.cpu
            .write_mem(output + index as u32 * 4, &mantissa[index].to_le_bytes())?;
        ctx.cpu
            .write_mem(powers + index as u32 * 4, &exponent[index].to_le_bytes())?;
    }
    Ok(DispatchOutcome::ReturnedR0(status))
}

#[cfg(test)]
mod query_matrix_tests {
    use super::*;
    #[test]
    fn query_matrix_reconstructs_finite_values_and_marks_invalid_components() {
        let values = [
            1.0,
            -1.0,
            0.0,
            0.125,
            123456.0,
            f32::from_bits(1),
            f32::INFINITY,
            f32::NAN,
        ];
        let (mantissa, exponent, status) = query_matrix_parts(&values);
        assert_eq!(status, (1 << 6) | (1 << 7));
        for index in 0..6 {
            let reconstructed = f64::from(mantissa[index]) / 65536.0 * 2f64.powi(exponent[index]);
            assert!(
                (reconstructed - f64::from(values[index])).abs()
                    <= f64::from(values[index]).abs() / 32768.0
            );
        }
    }
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
        "glQueryMatrixxOES" => gl_query_matrix_x_oes,
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
        "glCopyTexImage2D" => gl_copy_tex_image_2d,
        "glPolygonOffset" => gl_polygon_offset,
        "glPolygonOffsetx" => gl_polygon_offsetx,
        "glTexSubImage2D" => gl_tex_sub_image_2d,
        "glTexParameterf" => gl_tex_parameterf,
        "glTexParameterx" => gl_tex_parameterx,
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

        "glLightf" => gl_lightf,
        "glLightfv" => gl_lightfv,
        "glLightx" => gl_lightx,
        "glLightxv" => gl_lightxv,
        "glMaterialf" => gl_materialf,
        "glMaterialfv" => gl_materialfv,
        "glMaterialx" => gl_materialx,
        "glMaterialxv" => gl_materialxv,
        "glLightModelf" => gl_light_modelf,
        "glLightModelfv" => gl_light_modelfv,
        "glLightModelx" => gl_light_modelx,
        "glLightModelxv" => gl_light_modelxv,
        "glGetLightfv" => gl_get_lightfv,
        "glGetLightxv" => gl_get_lightxv,
        "glGetMaterialfv" => gl_get_materialfv,
        "glGetMaterialxv" => gl_get_materialxv,

        "glPointSize" => gl_point_size,
        "glPointSizex" => gl_point_sizex,
        "glLineWidth" => gl_line_width,
        "glLineWidthx" => gl_line_widthx,

        // No stencil buffer, no logic op, no multisample or mipmap hints.
        "glStencilFunc" | "glStencilMask" | "glStencilOp" | "glClearStencil" | "glLogicOp"
        | "glSampleCoverage" | "glSampleCoveragex"
        | "glHint"
        | "glFinish" | "glFlush" => gl_ignored,
        "glPixelStorei" => gl_pixel_store,

        // Compressed and copy-from-framebuffer texture paths.
        "glCompressedTexImage2D" => gl_compressed_tex_image_2d,
        "glCompressedTexSubImage2D" | "glCopyTexSubImage2D" => {
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
        "eglCreatePbufferSurface" => egl_create_pbuffer,
        "eglCreatePixmapSurface" => egl_create_pixmap,
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
        "eglWaitGL" | "eglWaitNative" | "eglWaitClient" => egl_wait,
        "eglCopyBuffers" => egl_copy_buffers,
        "eglSwapInterval"
        // The Gizmondo's GoForce driver exposed the swap interval under an
        // NV name, and Sticky Balls calls that one. Retain compatibility
        // acceptance; window swaps always observe the emulator's 60 Hz cap.
        | "eglSwapIntervalNV"
        | "eglSurfaceAttrib" | "eglBindAPI" => egl_true,
        "eglReleaseThread" => egl_terminate,
        "eglCreatePbufferFromClientBuffer" => egl_no_surface,
        "eglBindTexImage" | "eglReleaseTexImage" | "eglQueryContext"
        | "eglQueryAPI" => egl_unsupported,
        "eglGetProcAddress" => egl_get_proc_address,

        _ => return None,
    })
}

/// Test dimensions differ from the session's initial portrait panel.
#[cfg(test)]
fn reset_for_test(width: u32, height: u32) {
    reset_for_session();
    SURFACES.with(|s| {
        let mut s = s.borrow_mut();
        let image = s.images.get_mut(&SURFACE_HANDLE).unwrap();
        image.width = width;
        image.height = height;
    });
    with_ctx(|c| *c = Context::new(width, height));
    EGL_ERROR.with(|error| error.set(EGL_SUCCESS));
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
const EXTRA_NAMED_EXPORTS: [&str; 17] = [
    "glGetLightfv",
    "glGetLightxv",
    "glGetMaterialfv",
    "glGetMaterialxv",
    "glQueryMatrixxOES",
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

    /// Retained for existing test fixtures; production GL state is local
    /// to each host execution thread, not shared through this lock.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        match TEST_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    #[test]
    fn guest_attribute_reads_reuse_buffer_and_clear_failed_reads() {
        let mut cpu = StubCpu::new();
        cpu.map_region(0x1000, 0x1000, Prot::READ | Prot::WRITE)
            .unwrap();
        cpu.write_mem(0x1000, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let mut bytes = Vec::with_capacity(64);
        let allocation = bytes.as_ptr();
        let mut mem = CpuMem(&mut cpu);
        assert!(mem.read(0x1000, 8, &mut bytes));
        assert_eq!(bytes, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(mem.read(0x1004, 4, &mut bytes));
        assert_eq!(bytes, [5, 6, 7, 8]);
        assert_eq!(bytes.as_ptr(), allocation);
        assert!(!mem.read(0x3000, 4, &mut bytes));
        assert!(bytes.is_empty());
        assert!(mem.read(0x1000, 4, &mut bytes));
        assert_eq!(bytes, [1, 2, 3, 4]);
        assert_eq!(bytes.as_ptr(), allocation);
    }

    #[test]
    fn suspended_session_restores_parent_gles_instead_of_reinitializing() {
        let _lock = guard();
        reset_for_test(320, 240);
        with_ctx(|context| {
            context.clear_color = [0.8, 0.2, 0.1, 1.0];
            context.vertex_array.pointer = 0x50ab1234;
            context.vertex_array.enabled = true;
            context.target.color.fill(0x37);
            context.target.depth.fill(0.25);
            context.set_error(pocket_gles::GL_INVALID_ENUM);
        });
        set_egl_error(EGL_BAD_DISPLAY);
        let saved = crate::suspend_session();
        reset_for_session();
        with_ctx(|context| {
            context.clear_color = [0.0, 1.0, 0.0, 1.0];
            context.vertex_array.pointer = 0x50000001;
            context.target.color.fill(0xff);
        });
        set_egl_error(EGL_SUCCESS);
        drop(saved);
        with_ctx(|context| {
            assert_eq!(context.clear_color, [0.8, 0.2, 0.1, 1.0]);
            assert_eq!(context.vertex_array.pointer, 0x50ab1234);
            assert!(context.vertex_array.enabled);
            assert!(context.target.color.iter().all(|value| *value == 0x37));
            assert!(context.target.depth.iter().all(|value| *value == 0.25));
            assert_eq!(context.take_error(), pocket_gles::GL_INVALID_ENUM);
        });
        assert_eq!(EGL_ERROR.with(|error| error.get()), EGL_BAD_DISPLAY);
    }

    #[test]
    fn render_session_new_dispatcher_discards_previous_gl_state() {
        let _lock = guard();
        reset_for_test(320, 240);
        with_ctx(|c| {
            c.gen_textures(3);
            c.gen_buffers(2);
            c.vertex_array.pointer = 0x50ab1234;
            c.vertex_array.enabled = true;
            c.clear_color = [0.8, 0.2, 0.1, 1.0];
            c.state.depth_test = true;
            c.target.color.fill(0xff);
            c.target.depth.fill(0.25);
            c.set_error(pocket_gles::GL_INVALID_ENUM);
        });
        set_egl_error(EGL_BAD_DISPLAY);
        // A fresh dispatcher is the actual production session boundary,
        // even if a caller runs the next game on the same host thread.
        let _dispatcher = WinCeDispatcher::new();
        let defaults = Context::new(240, 320);
        with_ctx(|c| {
            assert!(c.textures.is_empty());
            assert!(c.buffers.is_empty());
            assert_eq!(c.vertex_array.pointer, 0);
            assert!(!c.vertex_array.enabled);
            assert_eq!(c.clear_color, defaults.clear_color);
            assert_eq!(c.state.depth_test, defaults.state.depth_test);
            assert_eq!((c.target.width, c.target.height), (240, 320));
            assert_eq!(c.target.color, defaults.target.color);
            assert_eq!(c.target.depth, defaults.target.depth);
            assert_eq!(c.take_error(), pocket_gles::GL_NO_ERROR);
            assert_eq!(c.gen_textures(1), vec![1]);
            assert_eq!(c.gen_buffers(1), vec![1]);
        });
        assert_eq!(EGL_ERROR.with(Cell::get), EGL_SUCCESS);
    }

    #[test]
    fn render_session_execution_threads_have_independent_gl_and_egl_state() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = [13u32, 19]
            .into_iter()
            .map(|width| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    reset_for_test(width, 7);
                    with_ctx(|c| {
                        c.gen_textures(width as usize);
                        c.vertex_array.pointer = width * 0x1000;
                    });
                    let error = if width == 13 {
                        EGL_BAD_DISPLAY
                    } else {
                        EGL_SUCCESS
                    };
                    if error != EGL_SUCCESS {
                        set_egl_error(error);
                    }
                    barrier.wait();
                    with_ctx(|c| {
                        assert_eq!(c.target.width, width);
                        assert_eq!(c.textures.len(), width as usize);
                        assert_eq!(c.vertex_array.pointer, width * 0x1000);
                    });
                    assert_eq!(EGL_ERROR.with(Cell::get), error);
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
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
        let started = std::time::Instant::now();
        loop {
            match handler(&mut c).unwrap() {
                DispatchOutcome::ReturnedR0(v) => return v,
                DispatchOutcome::JumpTo(pc) => {
                    assert_eq!(pc, t.thunk_va);
                    assert!(started.elapsed() < std::time::Duration::from_secs(2));
                }
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
    }

    /// A CPU with a stack and a scratch data page mapped.
    const SCRATCH: u32 = 0x2000_0000;

    #[test]
    fn polygon_offset_fixed_queries_and_copy_texture_stack_arguments() {
        let _g = guard();
        reset_for_test(2, 2);
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        call(
            &mut cpu,
            &mut kernel,
            gl_polygon_offsetx,
            &[98304, (-131072i32) as u32],
        );
        call(&mut cpu, &mut kernel, gl_get_floatv, &[0x8038, SCRATCH]);
        assert_eq!(f32::from_bits(cpu.read_u32_le(SCRATCH).unwrap()), 1.5);
        call(&mut cpu, &mut kernel, gl_get_floatv, &[0x2a00, SCRATCH]);
        assert_eq!(f32::from_bits(cpu.read_u32_le(SCRATCH).unwrap()), -2.);
        with_ctx(|c| {
            c.bind_texture(pocket_gles::GL_TEXTURE_2D, 7);
            c.target.color.fill(127);
        });
        call(
            &mut cpu,
            &mut kernel,
            gl_copy_tex_image_2d,
            &[
                pocket_gles::GL_TEXTURE_2D,
                0,
                pocket_gles::GL_RGBA,
                0,
                0,
                2,
                2,
                0,
            ],
        );
        assert_eq!(
            call(&mut cpu, &mut kernel, gl_get_error, &[]),
            pocket_gles::GL_NO_ERROR
        );
        with_ctx(|c| assert_eq!(c.textures[&7].rgba, vec![127; 16]));
        call(
            &mut cpu,
            &mut kernel,
            gl_copy_tex_image_2d,
            &[
                pocket_gles::GL_TEXTURE_2D,
                0,
                pocket_gles::GL_RGBA,
                0,
                0,
                2,
                2,
                1,
            ],
        );
        assert_eq!(
            call(&mut cpu, &mut kernel, gl_get_error, &[]),
            pocket_gles::GL_INVALID_VALUE
        );
    }

    #[test]
    fn point_line_size_float_fixed_validation_and_queries() {
        let _lock = guard();
        reset_for_test(32, 32);
        let (mut cpu, mut kernel) = (fresh_cpu(), fresh_kernel());
        call(&mut cpu, &mut kernel, gl_point_size, &[3.5f32.to_bits()]);
        call(&mut cpu, &mut kernel, gl_line_widthx, &[2 << 16]);
        call(&mut cpu, &mut kernel, gl_get_floatv, &[0x0b11, SCRATCH]);
        assert_eq!(cpu.read_mem(SCRATCH, 4).unwrap(), 3.5f32.to_le_bytes());
        call(&mut cpu, &mut kernel, gl_get_integerv, &[0x0b21, SCRATCH]);
        assert_eq!(cpu.read_mem(SCRATCH, 4).unwrap(), 2i32.to_le_bytes());
        call(&mut cpu, &mut kernel, gl_get_floatv, &[0x846d, SCRATCH]);
        assert_eq!(
            cpu.read_mem(SCRATCH, 8).unwrap(),
            [1f32.to_le_bytes(), 64f32.to_le_bytes()].concat()
        );
        for (handler, arg) in [
            (gl_point_size as crate::Handler, 0f32.to_bits()),
            (gl_line_width, f32::NAN.to_bits()),
            (gl_point_sizex, (-1i32) as u32),
            (gl_line_widthx, 0),
        ] {
            call(&mut cpu, &mut kernel, handler, &[arg]);
            assert_eq!(
                call(&mut cpu, &mut kernel, gl_get_error, &[]),
                pocket_gles::GL_INVALID_VALUE
            );
        }
        assert_eq!(with_ctx(|c| c.state.point_size), 3.5);
        assert_eq!(with_ctx(|c| c.state.line_width), 2.);
        reset_for_test(32, 32);
        assert_eq!(with_ctx(|c| c.state.point_size), 1.);
        assert_eq!(with_ctx(|c| c.state.line_width), 1.);
    }

    #[test]
    fn lighting_guest_float_fixed_setters_getters_errors_and_session_reset() {
        use pocket_gles::lighting::*;
        let _lock = guard();
        reset_for_test(32, 32);
        let (mut cpu, mut kernel) = (fresh_cpu(), fresh_kernel());
        let input = SCRATCH;
        let output = SCRATCH + 0x100;
        let values = [0.25f32, 0.5, 0.75, 1.];
        cpu.write_mem(
            input,
            &values
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        call(&mut cpu, &mut kernel, gl_lightfv, &[0x4000, DIFFUSE, input]);
        call(
            &mut cpu,
            &mut kernel,
            gl_get_lightxv,
            &[0x4000, DIFFUSE, output],
        );
        let bytes = cpu.read_mem(output, 16).unwrap();
        let fixed: Vec<_> = bytes
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(fixed, [16384, 32768, 49152, 65536]);
        call(
            &mut cpu,
            &mut kernel,
            gl_materialx,
            &[0x0408, SHININESS, 32 * 65536],
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_get_materialfv,
            &[0x0404, SHININESS, output],
        );
        assert_eq!(cpu.read_mem(output, 4).unwrap(), 32f32.to_le_bytes());
        call(
            &mut cpu,
            &mut kernel,
            gl_lightf,
            &[0x4000, SPOT_CUTOFF, 91f32.to_bits()],
        );
        assert_eq!(
            call(&mut cpu, &mut kernel, gl_get_error, &[]),
            pocket_gles::GL_INVALID_VALUE
        );
        call(&mut cpu, &mut kernel, gl_lightfv, &[0x4008, DIFFUSE, input]);
        assert_eq!(
            call(&mut cpu, &mut kernel, gl_get_error, &[]),
            pocket_gles::GL_INVALID_ENUM
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_lightfv,
            &[0x4000, DIFFUSE, 0xffff0000],
        );
        assert_eq!(
            call(&mut cpu, &mut kernel, gl_get_error, &[]),
            pocket_gles::GL_INVALID_OPERATION
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_light_modelx,
            &[MODEL_TWO_SIDE, 65536],
        );
        assert!(with_ctx(|c| c.lighting.two_sided));
        let saved = crate::suspend_session();
        reset_for_session();
        assert_eq!(with_ctx(|c| c.lighting.lights[0].diffuse), [1.; 4]);
        assert!(!with_ctx(|c| c.lighting.two_sided));
        drop(saved);
        assert_eq!(with_ctx(|c| c.lighting.lights[0].diffuse), values);
        assert!(with_ctx(|c| c.lighting.two_sided));
    }

    fn assert_rgb_surface_rgba_readback(gpu: bool) {
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        reset_for_test(1, 1);
        if gpu {
            set_gpu_renderer(true);
            assert!(with_ctx(|c| c.gpu_active()));
        }
        for alpha in [0, 32768, 65536] {
            call(
                &mut cpu,
                &mut kernel,
                gl_clear_colorx,
                &[0, 0, 65280, alpha],
            );
            call(
                &mut cpu,
                &mut kernel,
                gl_clear,
                &[pocket_gles::GL_COLOR_BUFFER_BIT],
            );
            call(
                &mut cpu,
                &mut kernel,
                gl_read_pixels,
                &[0, 0, 1, 1, pocket_gles::GL_RGBA, GL_UNSIGNED_BYTE, SCRATCH],
            );
            assert_eq!(cpu.read_mem(SCRATCH, 4).unwrap(), [0, 0, 254, 255]);
            // Readback must not rewrite the renderer's alpha or RGB precision.
            assert_eq!(
                with_ctx(|c| c.target.color[3]),
                ((alpha * 255 + 32768) / 65536) as u8
            );
        }
    }

    #[test]
    fn rgba_readback_of_rgb_surface_does_not_leak_internal_alpha() {
        let _g = guard();
        assert_rgb_surface_rgba_readback(false);
    }

    #[test]
    #[ignore = "requires native GL/GLES2"]
    fn native_rgba_readback_of_rgb_surface_does_not_leak_internal_alpha() {
        let _g = guard();
        assert_rgb_surface_rgba_readback(true);
    }

    #[test]
    fn rgb_readback_respects_pack_alignment_and_preserves_padding() {
        let _g = guard();
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        reset_for_test(1, 2);
        with_ctx(|c| c.target.color = vec![255, 0, 0, 255, 0, 0, 255, 255]);
        cpu.write_mem(SCRATCH, &[0x5a; 16]).unwrap();
        call(
            &mut cpu,
            &mut kernel,
            gl_read_pixels,
            &[0, 0, 1, 2, pocket_gles::GL_RGB, GL_UNSIGNED_BYTE, SCRATCH],
        );
        assert_eq!(
            cpu.read_mem(SCRATCH, 8).unwrap(),
            [0, 0, 255, 0x5a, 255, 0, 0, 0x5a]
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_pixel_store,
            &[pocket_gles::GL_PACK_ALIGNMENT, 1],
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_read_pixels,
            &[0, 0, 1, 2, pocket_gles::GL_RGB, GL_UNSIGNED_BYTE, SCRATCH],
        );
        assert_eq!(cpu.read_mem(SCRATCH, 6).unwrap(), [0, 0, 255, 255, 0, 0]);
        call(
            &mut cpu,
            &mut kernel,
            gl_pixel_store,
            &[pocket_gles::GL_PACK_ALIGNMENT, 3],
        );
        assert_eq!(
            call(&mut cpu, &mut kernel, gl_get_error, &[]),
            pocket_gles::GL_INVALID_VALUE
        );
        assert_eq!(PACK_ALIGNMENT.with(Cell::get), 1);
        let parent = suspend();
        assert_eq!(PACK_ALIGNMENT.with(Cell::get), 4);
        resume(parent);
        assert_eq!(PACK_ALIGNMENT.with(Cell::get), 1);
        call(
            &mut cpu,
            &mut kernel,
            gl_tex_envx,
            &[
                0x8500,
                pocket_gles::GL_TEXTURE_LOD_BIAS_EXT,
                (-32768i32) as u32,
            ],
        );
        assert_eq!(with_ctx(|c| c.texture_units[0].lod_bias), -0.5);
        call(
            &mut cpu,
            &mut kernel,
            gl_tex_envf,
            &[0x8500, pocket_gles::GL_TEXTURE_LOD_BIAS_EXT, 1f32.to_bits()],
        );
        assert_eq!(with_ctx(|c| c.texture_units[0].lod_bias), 1.);
        call(
            &mut cpu,
            &mut kernel,
            gl_bind_texture,
            &[pocket_gles::GL_TEXTURE_2D, 1],
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_tex_parameterf,
            &[
                pocket_gles::GL_TEXTURE_2D,
                pocket_gles::GL_TEXTURE_MAX_LEVEL_SGIS,
                2f32.to_bits(),
            ],
        );
        assert_eq!(with_ctx(|c| c.textures[&1].max_level), 2);
        for invalid in [-1f32, f32::NAN, f32::INFINITY] {
            call(
                &mut cpu,
                &mut kernel,
                gl_tex_parameterf,
                &[
                    pocket_gles::GL_TEXTURE_2D,
                    pocket_gles::GL_TEXTURE_MAX_LEVEL_SGIS,
                    invalid.to_bits(),
                ],
            );
            assert_eq!(
                call(&mut cpu, &mut kernel, gl_get_error, &[]),
                pocket_gles::GL_INVALID_VALUE
            );
            assert_eq!(with_ctx(|c| c.textures[&1].max_level), 2);
        }
        call(
            &mut cpu,
            &mut kernel,
            gl_tex_parameterx,
            &[
                pocket_gles::GL_TEXTURE_2D,
                pocket_gles::GL_TEXTURE_MAX_LEVEL_SGIS,
                0,
            ],
        );
        assert_eq!(with_ctx(|c| c.textures[&1].max_level), 0);
    }

    fn offscreen_roundtrip(gpu: bool) {
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        reset_for_test(8, 8);
        if gpu {
            set_gpu_renderer(true);
            assert!(with_ctx(|c| c.gpu_active()));
        }
        with_ctx(|c| {
            c.clear_color = [1., 0., 0., 1.];
            c.clear(pocket_gles::GL_COLOR_BUFFER_BIT);
        });
        let attrs = [
            pocket_gles::EGL_WIDTH,
            2,
            pocket_gles::EGL_HEIGHT,
            2,
            EGL_NONE,
        ];
        cpu.write_mem(
            SCRATCH,
            &attrs
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let off = call(
            &mut cpu,
            &mut kernel,
            egl_create_pbuffer,
            &[DISPLAY_HANDLE, CONFIG_HANDLE, SCRATCH],
        );
        assert_ne!(off, 0);
        call(
            &mut cpu,
            &mut kernel,
            egl_make_current,
            &[DISPLAY_HANDLE, off, off, CONTEXT_HANDLE],
        );
        with_ctx(|c| {
            c.clear_color = [0., 0., 1., 1.];
            c.clear(pocket_gles::GL_COLOR_BUFFER_BIT);
        });
        let counter = kernel.framebuffer.frame_counter;
        let anchor = std::time::Instant::now() + std::time::Duration::from_secs(1);
        kernel.framebuffer.directdraw.last_present = Some(anchor);
        call(
            &mut cpu,
            &mut kernel,
            egl_swap_buffers,
            &[DISPLAY_HANDLE, off],
        );
        assert_eq!(kernel.framebuffer.frame_counter, counter);
        assert_eq!(kernel.framebuffer.directdraw.last_present, Some(anchor));
        assert!(kernel.framebuffer.directdraw.pending.is_empty());
        kernel.framebuffer.directdraw.last_present = None;
        call(
            &mut cpu,
            &mut kernel,
            egl_make_current,
            &[DISPLAY_HANDLE, SURFACE_HANDLE, off, CONTEXT_HANDLE],
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_read_pixels,
            &[
                0,
                0,
                1,
                1,
                pocket_gles::GL_RGB,
                GL_UNSIGNED_BYTE,
                SCRATCH + 128,
            ],
        );
        assert_eq!(cpu.read_mem(SCRATCH + 128, 3).unwrap(), [0, 0, 255]);
        with_ctx(|c| {
            c.sync_gpu();
            assert_eq!(&c.target.color[..4], &[255, 0, 0, 255]);
        });
        let parent = suspend();
        reset_for_test(3, 3);
        resume(parent);
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                egl_get_current_surface,
                &[pocket_gles::EGL_READ]
            ),
            off
        );
        call(
            &mut cpu,
            &mut kernel,
            egl_make_current,
            &[DISPLAY_HANDLE, off, off, CONTEXT_HANDLE],
        );
        call(
            &mut cpu,
            &mut kernel,
            egl_destroy_surface,
            &[DISPLAY_HANDLE, off],
        );
        assert!(SURFACES.with(|s| s.borrow().images.contains_key(&off)));
        call(
            &mut cpu,
            &mut kernel,
            egl_make_current,
            &[DISPLAY_HANDLE, 0, 0, 0],
        );
        assert!(!SURFACES.with(|s| s.borrow().images.contains_key(&off)));
        assert_eq!(call(&mut cpu, &mut kernel, egl_get_current_context, &[]), 0);
        call(
            &mut cpu,
            &mut kernel,
            egl_make_current,
            &[
                DISPLAY_HANDLE,
                SURFACE_HANDLE,
                SURFACE_HANDLE,
                CONTEXT_HANDLE,
            ],
        );
        for _ in 0..64 {
            let off = call(
                &mut cpu,
                &mut kernel,
                egl_create_pbuffer,
                &[DISPLAY_HANDLE, CONFIG_HANDLE, SCRATCH],
            );
            assert_ne!(off, 0);
            call(
                &mut cpu,
                &mut kernel,
                egl_destroy_surface,
                &[DISPLAY_HANDLE, off],
            );
        }
        assert_eq!(SURFACES.with(|s| s.borrow().images.len()), 1);
        // A bottom-up 24-bit DIB receives blue through the same writeback as GDI.
        let bm = kernel.gdi.register_dib(pocket_kernel::gdi::Bitmap::new_dib(
            2,
            2,
            24,
            SCRATCH + 256,
            8,
            true,
            vec![],
            false,
        ));
        let pix = call(
            &mut cpu,
            &mut kernel,
            egl_create_pixmap,
            &[DISPLAY_HANDLE, CONFIG_HANDLE, bm, 0],
        );
        assert_ne!(pix, 0);
        call(
            &mut cpu,
            &mut kernel,
            egl_make_current,
            &[DISPLAY_HANDLE, pix, pix, CONTEXT_HANDLE],
        );
        with_ctx(|c| {
            c.clear_color = [0., 0., 1., 1.];
            c.clear(pocket_gles::GL_COLOR_BUFFER_BIT);
        });
        call(&mut cpu, &mut kernel, egl_wait, &[]);
        assert_eq!(
            &cpu.read_mem(SCRATCH + 256, 16).unwrap()[..6],
            &[255, 0, 0, 255, 0, 0]
        );
        assert_eq!(
            kernel.gdi.bitmap(bm).unwrap().pixels,
            [31, 0, 31, 0, 31, 0, 31, 0]
        );
        call(
            &mut cpu,
            &mut kernel,
            egl_make_current,
            &[
                DISPLAY_HANDLE,
                SURFACE_HANDLE,
                SURFACE_HANDLE,
                CONTEXT_HANDLE,
            ],
        );
        call(
            &mut cpu,
            &mut kernel,
            egl_destroy_surface,
            &[DISPLAY_HANDLE, pix],
        );
        assert_eq!(SURFACES.with(|s| s.borrow().images.len()), 1);
        // Deleting/reusing a name while another native context is parked
        // must not revive its same-revision old texture or retain its object.
        let off = call(
            &mut cpu,
            &mut kernel,
            egl_create_pbuffer,
            &[DISPLAY_HANDLE, CONFIG_HANDLE, SCRATCH],
        );
        let positions = [-1f32, -1., 0., 1., -1., 0., 0., 1., 0.];
        cpu.write_mem(
            SCRATCH + 512,
            &positions
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        call(
            &mut cpu,
            &mut kernel,
            gl_vertex_pointer,
            &[3, pocket_gles::GL_FLOAT, 0, SCRATCH + 512],
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_enable_client_state,
            &[pocket_gles::GL_VERTEX_ARRAY],
        );
        with_ctx(|c| {
            c.set_capability(pocket_gles::GL_TEXTURE_2D, true);
            c.bind_texture(pocket_gles::GL_TEXTURE_2D, 7);
            c.tex_image_2d(
                pocket_gles::GL_TEXTURE_2D,
                0,
                1,
                1,
                pocket_gles::GL_RGBA,
                GL_UNSIGNED_BYTE,
                &[255, 0, 0, 255],
            );
        });
        for surface in [SURFACE_HANDLE, off] {
            call(
                &mut cpu,
                &mut kernel,
                egl_make_current,
                &[DISPLAY_HANDLE, surface, surface, CONTEXT_HANDLE],
            );
            with_ctx(|c| c.set_viewport(0, 0, c.target.width as i32, c.target.height as i32));
            call(
                &mut cpu,
                &mut kernel,
                gl_draw_arrays,
                &[pocket_gles::GL_TRIANGLES, 0, 3],
            );
        }
        cpu.write_mem(SCRATCH + 600, &7u32.to_le_bytes()).unwrap();
        call(
            &mut cpu,
            &mut kernel,
            gl_delete_textures,
            &[1, SCRATCH + 600],
        );
        with_ctx(|c| {
            c.bind_texture(pocket_gles::GL_TEXTURE_2D, 7);
            c.tex_image_2d(
                pocket_gles::GL_TEXTURE_2D,
                0,
                1,
                1,
                pocket_gles::GL_RGBA,
                GL_UNSIGNED_BYTE,
                &[0, 255, 0, 255],
            );
        });
        for surface in [off, SURFACE_HANDLE] {
            call(
                &mut cpu,
                &mut kernel,
                egl_make_current,
                &[DISPLAY_HANDLE, surface, surface, CONTEXT_HANDLE],
            );
            with_ctx(|c| c.set_viewport(0, 0, c.target.width as i32, c.target.height as i32));
            call(
                &mut cpu,
                &mut kernel,
                gl_draw_arrays,
                &[pocket_gles::GL_TRIANGLES, 0, 3],
            );
            with_ctx(|c| {
                c.sync_gpu();
                let index = ((c.target.height / 2) * c.target.width + c.target.width / 2) * 4;
                assert_eq!(
                    &c.target.color[index as usize..index as usize + 4],
                    &[0, 255, 0, 255]
                );
            });
        }
        call(
            &mut cpu,
            &mut kernel,
            egl_destroy_surface,
            &[DISPLAY_HANDLE, off],
        );
        release_gpu();
        reset_for_session();
    }
    #[test]
    fn egl_offscreen_images_read_draw_pixmap_teardown_and_parent_resume() {
        let _g = guard();
        offscreen_roundtrip(false);
    }
    #[test]
    #[ignore = "requires native OpenGL; offscreen images retain native colour and depth"]
    fn gpu_egl_offscreen_images_read_draw_pixmap_teardown_and_parent_resume() {
        let _g = guard();
        offscreen_roundtrip(true);
    }

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

    fn window_swap_timing(gpu: bool) {
        use pocket_kernel::framebuffer::DirectDrawTiming;
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        reset_for_test(kernel.framebuffer.width, kernel.framebuffer.height);
        if gpu {
            set_gpu_renderer(true);
            assert!(with_ctx(|c| c.gpu_active()));
        }
        // First presentation has no artificial startup delay.
        call(
            &mut cpu,
            &mut kernel,
            egl_swap_buffers,
            &[DISPLAY_HANDLE, SURFACE_HANDLE],
        );
        let previous = kernel.framebuffer.directdraw.last_present.unwrap();
        with_ctx(|c| {
            c.clear_color = [1., 0., 0., 1.];
            c.clear(pocket_gles::GL_COLOR_BUFFER_BIT);
        });
        // Force a future anchor: even a slow CI runner must exercise retry.
        let anchor = std::time::Instant::now() + std::time::Duration::from_millis(5);
        kernel.framebuffer.directdraw.last_present = Some(anchor);
        let before = kernel.framebuffer.pixels.clone();
        let counter = kernel.framebuffer.frame_counter;
        cpu.write_reg(ArmReg::R0, DISPLAY_HANDLE).unwrap();
        cpu.write_reg(ArmReg::R1, SURFACE_HANDLE).unwrap();
        let t = thunk();
        let mut ctx = CallCtx {
            cpu: &mut cpu,
            kernel: &mut kernel,
            thunk: &t,
        };
        assert_eq!(
            egl_swap_buffers(&mut ctx).unwrap(),
            DispatchOutcome::JumpTo(t.thunk_va)
        );
        assert_eq!(ctx.cpu.read_reg(ArmReg::R0).unwrap(), DISPLAY_HANDLE);
        assert_eq!(ctx.cpu.read_reg(ArmReg::R1).unwrap(), SURFACE_HANDLE);
        assert_eq!(ctx.kernel.framebuffer.pixels, before);
        assert_eq!(ctx.kernel.framebuffer.frame_counter, counter);
        let key = (0, t.thunk_va, ctx.cpu.read_reg(ArmReg::Sp).unwrap());
        let deadline = ctx.kernel.framebuffer.directdraw.pending[&key];
        assert_eq!(deadline, anchor + DirectDrawTiming::PERIOD);
        // Each retry retains the same deadline and intact arguments.
        loop {
            match egl_swap_buffers(&mut ctx).unwrap() {
                DispatchOutcome::JumpTo(pc) => {
                    assert_eq!(pc, t.thunk_va);
                    assert_eq!(ctx.kernel.framebuffer.directdraw.pending[&key], deadline);
                    assert_eq!(ctx.cpu.read_reg(ArmReg::R0).unwrap(), DISPLAY_HANDLE);
                }
                DispatchOutcome::ReturnedR0(EGL_TRUE) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(ctx.kernel.framebuffer.directdraw.pending.is_empty());
        assert!(ctx.kernel.framebuffer.directdraw.last_present.unwrap() >= deadline);
        assert!(ctx.kernel.framebuffer.directdraw.last_present.unwrap() > previous);
        assert_eq!(&ctx.kernel.framebuffer.pixels[..2], &[0, 0xf8]);
        // Identical frames are still paced; pixel changes cannot be the clock.
        let first = ctx.kernel.framebuffer.directdraw.last_present.unwrap();
        let started = std::time::Instant::now();
        loop {
            match egl_swap_buffers(&mut ctx).unwrap() {
                DispatchOutcome::ReturnedR0(EGL_TRUE) => break,
                DispatchOutcome::JumpTo(pc) => {
                    assert_eq!(pc, t.thunk_va);
                    assert!(started.elapsed() < std::time::Duration::from_secs(2));
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(
            ctx.kernel.framebuffer.directdraw.last_present.unwrap()
                >= first + DirectDrawTiming::PERIOD
        );
        // Resuming a late process does not produce a catch-up burst.
        ctx.kernel.framebuffer.directdraw.last_present =
            Some(std::time::Instant::now() - DirectDrawTiming::PERIOD * 10);
        assert_eq!(
            egl_swap_buffers(&mut ctx).unwrap(),
            DispatchOutcome::ReturnedR0(EGL_TRUE)
        );
        assert_eq!(
            egl_swap_buffers(&mut ctx).unwrap(),
            DispatchOutcome::JumpTo(t.thunk_va)
        );
        // Drain this final pending wait before ending the test.
        let started = std::time::Instant::now();
        loop {
            match egl_swap_buffers(&mut ctx).unwrap() {
                DispatchOutcome::ReturnedR0(EGL_TRUE) => break,
                DispatchOutcome::JumpTo(pc) => {
                    assert_eq!(pc, t.thunk_va);
                    assert!(started.elapsed() < std::time::Duration::from_secs(2));
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn window_swaps_pace_software_and_preserve_retry_arguments() {
        let _g = guard();
        window_swap_timing(false);
    }

    #[test]
    #[ignore = "requires host OpenGL; native swaps use the same modeled LCD clock"]
    fn window_swaps_pace_native_gpu_and_preserve_retry_arguments() {
        let _g = guard();
        window_swap_timing(true);
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
            (
                pocket_gles::GL_MAX_TEXTURE_UNITS,
                pocket_gles::raster::MAX_TEXTURE_STAGES as u32,
            ),
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
    #[test]
    #[ignore = "requires host OpenGL; exercises guest GLES and presentation"]
    fn gpu_guest_draw_readpixels_gapi_and_parent_resume() {
        let _lock = guard();
        reset_for_test(32, 32);
        let mut cpu = fresh_cpu();
        let mut kernel = fresh_kernel();
        kernel.framebuffer.resize(32, 32);
        cpu.map_region(
            pocket_kernel::SYNTHETIC_FRAMEBUFFER_BASE,
            kernel.framebuffer.pixels.len() as u32,
            Prot::READ | Prot::WRITE,
        )
        .unwrap();
        kernel.fb_mapped = true;
        set_gpu_renderer(true);
        assert!(with_ctx(|c| c.gpu_active()));
        let vertices: [f32; 9] = [-0.9, -0.9, 0., 0.9, -0.9, 0., 0., 0.9, 0.];
        let bytes: Vec<u8> = vertices.iter().flat_map(|v| v.to_le_bytes()).collect();
        cpu.write_mem(SCRATCH, &bytes).unwrap();
        call(
            &mut cpu,
            &mut kernel,
            gl_vertex_pointer,
            &[3, pocket_gles::GL_FLOAT, 0, SCRATCH],
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
            gl_clear,
            &[pocket_gles::GL_COLOR_BUFFER_BIT | pocket_gles::GL_DEPTH_BUFFER_BIT],
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
            &[DISPLAY_HANDLE, 0],
        );
        let pixel = ((16 * 32 + 16) * 2) as usize;
        assert_eq!(&kernel.framebuffer.pixels[pixel..pixel + 2], &[255, 255]);
        assert_eq!(
            cpu.read_mem(
                pocket_kernel::SYNTHETIC_FRAMEBUFFER_BASE,
                kernel.framebuffer.pixels.len() as u32
            )
            .unwrap(),
            kernel.framebuffer.pixels
        );
        call(
            &mut cpu,
            &mut kernel,
            gl_read_pixels,
            &[
                16,
                16,
                1,
                1,
                pocket_gles::GL_RGBA,
                pocket_gles::GL_UNSIGNED_BYTE,
                SCRATCH + 256,
            ],
        );
        assert_eq!(cpu.read_mem(SCRATCH + 256, 4).unwrap(), [255; 4]);
        with_ctx(|c| c.bind_texture(pocket_gles::GL_TEXTURE_2D, 3));
        call(
            &mut cpu,
            &mut kernel,
            gl_copy_tex_image_2d,
            &[
                pocket_gles::GL_TEXTURE_2D,
                0,
                pocket_gles::GL_RGBA,
                16,
                16,
                1,
                1,
                0,
            ],
        );
        with_ctx(|c| assert_eq!(c.textures[&3].rgba, [255; 4]));
        let parent = suspend();
        set_gpu_renderer(true);
        assert!(with_ctx(|c| c.gpu_active()));
        release_gpu();
        resume(parent);
        with_ctx(|c| c.current_color = [1., 0., 0., 1.]);
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
            &[DISPLAY_HANDLE, 0],
        );
        assert_eq!(&kernel.framebuffer.pixels[pixel..pixel + 2], &[0, 248]);
        call(&mut cpu, &mut kernel, egl_terminate, &[DISPLAY_HANDLE]);
        assert!(!with_ctx(|c| c.gpu_active()));
        call(
            &mut cpu,
            &mut kernel,
            egl_initialize,
            &[DISPLAY_HANDLE, 0, 0],
        );
        assert!(with_ctx(|c| c.gpu_active()));
        release_gpu();
        reset_for_session();
    }
}
