//! The OpenGL ES 1.1 context: state machine, vertex assembly, drawing.
//!
//! This is the layer the DLL entry points call into. It owns every piece
//! of GL state (matrix stacks, enables, texture objects, array
//! pointers), assembles primitives, and hands triangles to
//! [`crate::raster`].
//!
//! Vertex and texture data live in *guest* memory, which this crate has
//! no way to read directly. Callers therefore supply a [`GuestMemory`]
//! implementation; the emulator backs it with the CPU's address space
//! and the tests back it with a plain byte vector.

use crate::consts::*;
use crate::fixed;
use crate::matrix::{self, Matrix4, MatrixMode, MatrixStack};
use crate::raster::{
    self, BlendFactor, CompareFunc, CullMode, FogMode, FrontFace, PipelineState, RenderTarget,
    TexEnvMode, Vertex,
};
use crate::texture::{self, Texture};
use std::collections::HashMap;

/// Read access to the guest's address space.
pub trait GuestMemory {
    /// Copy `len` bytes starting at `addr` into `out`. Returns `false`
    /// if any part of the range is unmapped, in which case the caller
    /// skips the draw rather than rendering garbage.
    fn read(&mut self, addr: u32, len: usize, out: &mut Vec<u8>) -> bool;
}

/// A `gl*Pointer` client array binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArrayPointer {
    pub enabled: bool,
    /// Components per element (2, 3 or 4).
    pub size: u32,
    /// One of `GL_BYTE`, `GL_SHORT`, `GL_FIXED`, `GL_FLOAT`,
    /// `GL_UNSIGNED_BYTE`.
    pub ty: u32,
    /// Byte stride between elements. Zero means tightly packed.
    pub stride: u32,
    /// Guest virtual address of the first element — or, when `buffer`
    /// is non-zero, a byte offset into that buffer object.
    pub pointer: u32,
    /// The `GL_ARRAY_BUFFER` bound when this pointer was set. GL ES 1.1
    /// captures the binding at `gl*Pointer` time, not at draw time, so
    /// it has to be remembered per array: a game may bind a VBO, set
    /// the vertex pointer, then bind a different VBO for texcoords.
    pub buffer: u32,
}

impl Default for ArrayPointer {
    fn default() -> Self {
        Self {
            enabled: false,
            size: 4,
            ty: GL_FLOAT,
            stride: 0,
            pointer: 0,
            buffer: 0,
        }
    }
}

impl ArrayPointer {
    /// Bytes one component occupies, or `None` for an unsupported type.
    fn component_bytes(&self) -> Option<u32> {
        Some(match self.ty {
            GL_BYTE | GL_UNSIGNED_BYTE => 1,
            GL_SHORT | GL_UNSIGNED_SHORT => 2,
            GL_FLOAT | GL_FIXED => 4,
            _ => return None,
        })
    }

    /// Distance in bytes between consecutive elements.
    fn element_stride(&self) -> Option<u32> {
        let packed = self.component_bytes()?.checked_mul(self.size)?;
        Some(if self.stride == 0 {
            packed
        } else {
            self.stride
        })
    }
}

/// Decode one component out of a raw little-endian buffer.
///
/// `normalize` maps integer types into `[0, 1]`, which is what GL does
/// for colours but *not* for positions or texture coordinates.
fn decode_component(bytes: &[u8], ty: u32, normalize: bool) -> f32 {
    match ty {
        GL_FLOAT => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        GL_FIXED => fixed::to_f32(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        GL_BYTE => {
            let v = bytes[0] as i8 as f32;
            if normalize {
                (v / 127.0).clamp(-1.0, 1.0)
            } else {
                v
            }
        }
        GL_UNSIGNED_BYTE => {
            let v = bytes[0] as f32;
            if normalize {
                v / 255.0
            } else {
                v
            }
        }
        GL_SHORT => {
            let v = i16::from_le_bytes([bytes[0], bytes[1]]) as f32;
            if normalize {
                (v / 32767.0).clamp(-1.0, 1.0)
            } else {
                v
            }
        }
        GL_UNSIGNED_SHORT => {
            let v = u16::from_le_bytes([bytes[0], bytes[1]]) as f32;
            if normalize {
                v / 65535.0
            } else {
                v
            }
        }
        _ => 0.0,
    }
}

/// How many texture stages keep state, which is deliberately more than
/// the `GL_MAX_TEXTURE_UNITS` we advertise.
///
/// `glActiveTexture` past the advertised maximum is an error that leaves
/// the selected unit alone, so an engine that walks more stages than it
/// asked about aims its per-unit calls at the last stage it *did*
/// select. Sticky Balls (X-Forge) does exactly that: it sets up stage 1,
/// then either binds a second layer to stage 2 or resets stage 2 before
/// the draw. With two stages tracked, that stage-2 traffic landed on
/// stage 1 — the reset unbound the texture that had just been bound, and
/// the game's sky came out as an opaque white sheet. Tracking the stages
/// the engine actually touches, plus one spare, keeps stage 2 harmless.
const MAX_TEXTURE_UNITS: usize = raster::MAX_TEXTURE_STAGES;

/// A buffer object: the host-side copy of what `glBufferData` uploaded.
///
/// GL ES 1.1 buffer objects hold vertex attributes (`GL_ARRAY_BUFFER`)
/// or indices (`GL_ELEMENT_ARRAY_BUFFER`). The guest uploads once and
/// then draws from the buffer many times, so keeping the bytes on the
/// host side means a draw does not re-read guest memory at all.
#[derive(Debug, Default, Clone)]
pub struct Buffer {
    pub data: Vec<u8>,
    /// The `GL_STATIC_DRAW` / `GL_DYNAMIC_DRAW` hint. Recorded for
    /// debugging; a software rasterizer has no use for it.
    pub usage: u32,
}

impl Buffer {
    /// Borrow `len` bytes at `offset`, or `None` if that range runs off
    /// the end. A short read must not be padded with zeroes — that
    /// silently renders degenerate geometry instead of surfacing the
    /// out-of-range access.
    pub fn slice(&self, offset: u32, len: usize) -> Option<&[u8]> {
        let start = offset as usize;
        let end = start.checked_add(len)?;
        self.data.get(start..end)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TextureUnitState {
    pub bound_texture: u32,
    pub texture_enabled: bool,
    pub texcoord_array: ArrayPointer,
    pub current_texcoord: [f32; 2],
    /// `glTexEnv(GL_TEXTURE_ENV_MODE)` is per-unit state in GL, so a
    /// game that configures two stages leaves two different modes
    /// behind. Keeping one global would mean the stage we actually
    /// sample is combined with whichever stage the guest happened to
    /// touch last.
    pub tex_env: TexEnvMode,
    /// `GL_TEXTURE_ENV_COLOR`, per-unit for the same reason as the mode
    /// above. Only `GL_BLEND` reads it.
    pub tex_env_color: [f32; 4],
    pub lod_bias: f32,
}

impl Default for TextureUnitState {
    fn default() -> Self {
        Self {
            bound_texture: 0,
            texture_enabled: false,
            texcoord_array: ArrayPointer::default(),
            current_texcoord: [0.0, 0.0],
            tex_env: TexEnvMode::Modulate,
            tex_env_color: [0.0; 4],
            lod_bias: 0.0,
        }
    }
}

/// The full GL ES 1.1 context.
/// Colour/depth and native renderer ownership for an EGL surface. GL state
/// and guest texture objects remain in Context when switching surfaces.
pub struct SurfaceBuffers {
    pub target: RenderTarget,
    gpu: Option<crate::gpu::Renderer>,
    failed: bool,
}
impl SurfaceBuffers {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            target: RenderTarget::new(width, height),
            gpu: None,
            failed: false,
        }
    }
    pub fn release_gpu(&mut self) {
        self.gpu = None;
        self.failed = false;
    }
    pub fn delete_textures(&mut self, names: &[u32]) {
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.delete_textures(names);
        }
    }
    pub fn disable_gpu(&mut self) {
        if let Some(gpu) = self.gpu.as_mut() {
            if let Err(e) = gpu.read(&mut self.target) {
                log::warn!("GLES offscreen readback: {e}");
            }
        }
        self.release_gpu();
    }
}

pub struct Context {
    gpu: Option<crate::gpu::Renderer>,
    gpu_requested: bool,
    anisotropic_filtering: u8,
    gpu_failed: bool,
    // ---- transform state ----
    pub matrix_mode: MatrixMode,
    pub modelview: MatrixStack,
    pub projection: MatrixStack,
    /// GL ES keeps one texture matrix stack per server texture unit.
    pub texture_matrix: [MatrixStack; MAX_TEXTURE_UNITS],

    // ---- per-vertex defaults, used when the array is disabled ----
    pub current_color: [f32; 4],
    pub current_texcoord: [f32; 2],
    pub current_normal: [f32; 3],
    pub lighting: crate::lighting::LightingState,

    // ---- client arrays ----
    pub vertex_array: ArrayPointer,
    pub color_array: ArrayPointer,
    /// Texture coordinate arrays live on [`TextureUnitState`], one per
    /// unit — there is deliberately no unit-0 copy here, because a stale
    /// mirror of per-unit state is what made multitextured draws read
    /// the wrong stage.
    pub normal_array: ArrayPointer,
    /// Texture unit `glTexCoordPointer` / `glEnableClientState` affect.
    pub client_active_texture: u32,
    /// Texture unit `glTexEnv` / `glBindTexture` affect.
    pub active_texture: u32,

    // ---- textures ----
    pub textures: HashMap<u32, Texture>,
    pub texture_units: [TextureUnitState; MAX_TEXTURE_UNITS],
    pub bound_texture: u32,
    next_texture_name: u32,

    // ---- buffer objects ----
    pub buffers: HashMap<u32, Buffer>,
    /// Currently bound `GL_ARRAY_BUFFER`; captured by `gl*Pointer`.
    pub array_buffer: u32,
    /// Currently bound `GL_ELEMENT_ARRAY_BUFFER`; consulted by
    /// `glDrawElements` at draw time, unlike the array binding.
    pub element_array_buffer: u32,
    next_buffer_name: u32,

    // ---- fragment pipeline ----
    pub state: PipelineState,
    pub clear_color: [f32; 4],
    pub clear_depth: f32,
    /// `GL_CULL_FACE` enable, tracked separately from the face mode so
    /// `glCullFace` before `glEnable` is not lost.
    cull_enabled: bool,
    /// The face `glCullFace` selected; GL's initial value is `GL_BACK`.
    cull_mode: CullMode,

    // ---- framebuffer ----
    pub target: RenderTarget,

    /// Sticky error code, returned and reset by `glGetError`.
    error: u32,

    /// Scratch buffer for guest array reads, reused across draws so a
    /// 60 fps render loop does not allocate per call.
    prepared_arrays: Vec<(u32, Vec<u8>)>,
    array_read_pool: Vec<Vec<u8>>,
    vertex_stream: crate::gpu::VertexStream,
    decoded_indices: Vec<u32>,
    scratch: Vec<u8>,
    /// Scratch for index buffer reads.
    index_scratch: Vec<u8>,
}

impl Context {
    pub fn new(width: u32, height: u32) -> Self {
        let mut state = PipelineState {
            viewport: (0, 0, width as i32, height as i32),
            ..Default::default()
        };
        // GL's initial depth func is LESS, but the test is off until
        // the app enables it.
        state.depth_func = CompareFunc::Less;
        Self {
            gpu: None,
            gpu_requested: false,
            anisotropic_filtering: 0,
            gpu_failed: false,
            matrix_mode: MatrixMode::Modelview,
            modelview: MatrixStack::new(16),
            projection: MatrixStack::new(16),
            texture_matrix: std::array::from_fn(|_| MatrixStack::new(16)),
            current_color: [1.0, 1.0, 1.0, 1.0],
            current_texcoord: [0.0, 0.0],
            current_normal: [0.0, 0.0, 1.0],
            lighting: Default::default(),
            vertex_array: ArrayPointer::default(),
            color_array: ArrayPointer::default(),
            normal_array: ArrayPointer::default(),
            client_active_texture: 0,
            active_texture: 0,
            textures: HashMap::new(),
            texture_units: [TextureUnitState::default(); MAX_TEXTURE_UNITS],
            bound_texture: 0,
            next_texture_name: 1,
            buffers: HashMap::new(),
            array_buffer: 0,
            element_array_buffer: 0,
            next_buffer_name: 1,
            state,
            clear_color: [0.0, 0.0, 0.0, 1.0],
            clear_depth: 1.0,
            cull_enabled: false,
            cull_mode: CullMode::Back,
            target: RenderTarget::new(width, height),
            error: GL_NO_ERROR,
            prepared_arrays: Vec::new(),
            array_read_pool: Vec::new(),
            vertex_stream: Default::default(),
            decoded_indices: Vec::new(),
            scratch: Vec::new(),
            index_scratch: Vec::new(),
        }
    }

    pub fn park_surface(&mut self, surface: &mut SurfaceBuffers) {
        std::mem::swap(&mut self.target, &mut surface.target);
        std::mem::swap(&mut self.gpu, &mut surface.gpu);
        std::mem::swap(&mut self.gpu_failed, &mut surface.failed);
    }
    pub fn exchange_surface(&mut self, surface: &mut SurfaceBuffers) {
        self.park_surface(surface);
        if let Some(gpu) = &mut self.gpu {
            gpu.set_anisotropic_filtering(self.anisotropic_filtering);
        }
        if self.gpu_requested && self.gpu.is_none() {
            if let Err(e) = self.enable_gpu() {
                self.gpu_failure("surface", e);
            }
        }
    }

    /// Configuration alone never initializes GPU rendering.
    pub fn set_anisotropic_filtering(&mut self, level: u8) {
        self.anisotropic_filtering = match level {
            2 | 4 | 8 => level,
            _ => 0,
        };
        if let Some(gpu) = &mut self.gpu {
            gpu.set_anisotropic_filtering(self.anisotropic_filtering);
        }
    }

    /// Select before guest rendering begins. Initialization failure keeps software.
    pub fn enable_gpu(&mut self) -> Result<(), String> {
        self.gpu_requested = true;
        if self.gpu.is_none() {
            self.gpu = Some(crate::gpu::Renderer::new(
                self.target.width,
                self.target.height,
            )?);
            self.gpu
                .as_mut()
                .unwrap()
                .set_anisotropic_filtering(self.anisotropic_filtering);
        }
        Ok(())
    }
    pub fn release_gpu(&mut self) {
        self.gpu = None;
        self.gpu_failed = false;
    }
    pub fn disable_gpu(&mut self) {
        self.sync_gpu();
        for tex in self.textures.values_mut() {
            tex.ensure_rgba();
        }
        self.release_gpu();
        self.gpu_requested = false;
    }
    fn gpu_failure(&mut self, action: &str, error: String) {
        if !self.gpu_failed {
            log::error!("GLES GPU {action}: {error}; restart with software rendering if needed");
        }
        self.gpu_failed = true;
        self.set_error(GL_OUT_OF_MEMORY);
    }
    pub fn gpu_requested(&self) -> bool {
        self.gpu_requested
    }
    pub fn gpu_active(&self) -> bool {
        self.gpu.is_some()
    }
    /// Read back only for presentation or glReadPixels, never per draw.
    pub fn sync_gpu(&mut self) {
        if self.gpu_failed {
            return;
        }
        if let Some(gpu) = self.gpu.as_mut() {
            if let Err(e) = gpu.read(&mut self.target) {
                self.gpu_failure("readback", e);
            }
        }
    }

    /// Preserve the RGBA readback cache and create the display framebuffer
    /// in the same native-readback traversal. Software uses the existing converter.
    pub fn present_rgb565(&mut self, out: &mut [u8]) {
        if !self.gpu_failed {
            if let Some(gpu) = self.gpu.as_mut() {
                match gpu.read_present(&mut self.target, out) {
                    Ok(()) => return,
                    Err(e) => self.gpu_failure("readback", e),
                }
            }
        }
        self.target.to_rgb565(out);
    }

    // ---- error handling -------------------------------------------------

    /// Record an error. GL keeps the *first* error until it is read, so
    /// a later failure does not mask an earlier one.
    pub fn set_error(&mut self, code: u32) {
        if self.error == GL_NO_ERROR {
            // `glGetError` reports the *first* error since the last
            // check, so by the time a guest sees a code the offending
            // call is long gone. Log at the point of blame instead.
            log::debug!("GLES set error 0x{code:04x}");
            self.error = code;
        }
    }

    /// `glGetError`: return and clear the sticky error code.
    pub fn take_error(&mut self) -> u32 {
        std::mem::replace(&mut self.error, GL_NO_ERROR)
    }

    // ---- matrix stacks --------------------------------------------------

    fn stack_mut(&mut self) -> &mut MatrixStack {
        match self.matrix_mode {
            MatrixMode::Modelview => &mut self.modelview,
            MatrixMode::Projection => &mut self.projection,
            MatrixMode::Texture => &mut self.texture_matrix[self.active_texture as usize],
        }
    }

    pub fn set_matrix_mode(&mut self, mode: u32) {
        match MatrixMode::from_enum(mode) {
            Some(m) => self.matrix_mode = m,
            None => self.set_error(GL_INVALID_ENUM),
        }
    }

    pub fn load_identity(&mut self) {
        self.stack_mut().load_identity();
    }

    pub fn load_matrix(&mut self, m: Matrix4) {
        self.stack_mut().load(m);
    }

    pub fn mult_matrix(&mut self, m: Matrix4) {
        self.stack_mut().multiply_by(&m);
    }

    pub fn push_matrix(&mut self) {
        if !self.stack_mut().push() {
            self.set_error(GL_STACK_OVERFLOW);
        }
    }

    pub fn pop_matrix(&mut self) {
        if !self.stack_mut().pop() {
            self.set_error(GL_STACK_UNDERFLOW);
        }
    }

    pub fn translate(&mut self, x: f32, y: f32, z: f32) {
        let m = matrix::translate(x, y, z);
        self.stack_mut().multiply_by(&m);
    }

    pub fn scale(&mut self, x: f32, y: f32, z: f32) {
        let m = matrix::scale(x, y, z);
        self.stack_mut().multiply_by(&m);
    }

    pub fn rotate(&mut self, angle: f32, x: f32, y: f32, z: f32) {
        let m = matrix::rotate(angle, x, y, z);
        self.stack_mut().multiply_by(&m);
    }

    pub fn frustum(&mut self, l: f32, r: f32, b: f32, t: f32, n: f32, f: f32) {
        // GL rejects a degenerate frustum rather than producing NaNs.
        if n <= 0.0 || f <= 0.0 || l == r || b == t || n == f {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        let m = matrix::frustum(l, r, b, t, n, f);
        self.stack_mut().multiply_by(&m);
    }

    pub fn ortho(&mut self, l: f32, r: f32, b: f32, t: f32, n: f32, f: f32) {
        if l == r || b == t || n == f {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        let m = matrix::ortho(l, r, b, t, n, f);
        self.stack_mut().multiply_by(&m);
    }

    // ---- enables --------------------------------------------------------

    pub fn set_capability(&mut self, cap: u32, on: bool) {
        match cap {
            GL_DEPTH_TEST => self.state.depth_test = on,
            GL_ALPHA_TEST => self.state.alpha_test = on,
            GL_BLEND => self.state.blend = on,
            GL_CULL_FACE => {
                self.cull_enabled = on;
                self.sync_cull();
            }
            GL_SCISSOR_TEST => {
                if !on {
                    self.state.scissor = None;
                }
                // Enabling without a box set leaves the full-window
                // default, which matches GL's initial scissor state.
            }
            GL_TEXTURE_2D => {
                let unit = self.active_texture as usize;
                if unit >= MAX_TEXTURE_UNITS {
                    self.set_error(GL_INVALID_ENUM);
                    return;
                }
                self.texture_units[unit].texture_enabled = on;
                // No mirror into `self.state`: the rasterizer's single
                // texturing switch is derived per draw from the stage
                // `texturing_unit` picks, which is not always stage 0.
            }
            GL_FOG => self.state.fog = on,
            GL_LIGHTING => self.lighting.enabled = on,
            GL_NORMALIZE => self.lighting.normalize = on,
            GL_COLOR_MATERIAL => self.lighting.color_material = on,
            crate::lighting::RESCALE_NORMAL => self.lighting.rescale = on,
            GL_POLYGON_OFFSET_FILL => self.state.polygon_offset = on,
            GL_DITHER | GL_STENCIL_TEST => {}
            0x4000..=0x4007 => self.lighting.lights[(cap - 0x4000) as usize].enabled = on,
            _ => self.set_error(GL_INVALID_ENUM),
        }
    }

    pub fn set_client_state(&mut self, array: u32, on: bool) {
        match array {
            GL_VERTEX_ARRAY => self.vertex_array.enabled = on,
            GL_COLOR_ARRAY => self.color_array.enabled = on,
            GL_TEXTURE_COORD_ARRAY => {
                let unit = self.client_active_texture as usize;
                if unit >= MAX_TEXTURE_UNITS {
                    self.set_error(GL_INVALID_ENUM);
                    return;
                }
                self.texture_units[unit].texcoord_array.enabled = on;
            }
            GL_NORMAL_ARRAY => self.normal_array.enabled = on,
            _ => self.set_error(GL_INVALID_ENUM),
        }
    }

    // ---- textures -------------------------------------------------------

    /// `glGenTextures`: allocate `n` unused names.
    pub fn gen_textures(&mut self, n: usize) -> Vec<u32> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.next_texture_name;
            self.next_texture_name += 1;
            self.textures.insert(name, Texture::default());
            out.push(name);
        }
        out
    }

    pub fn bind_texture(&mut self, target: u32, name: u32) {
        if target != GL_TEXTURE_2D {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        let unit = self.active_texture as usize;
        if unit >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        // Binding an unseen name creates the object, as GL requires.
        if name != 0 {
            self.textures.entry(name).or_default();
            self.next_texture_name = self.next_texture_name.max(name + 1);
        }
        self.texture_units[unit].bound_texture = name;
        if unit == 0 {
            self.bound_texture = name;
        }
        log::debug!(
            "GLES bind texture target=0x{target:04x} name={name} active_unit={} objects={}",
            self.active_texture,
            self.textures.len()
        );
    }

    /// `glGenBuffers`. Names are handed out densely from 1; GL only
    /// requires that they be unused, not that they be contiguous.
    pub fn gen_buffers(&mut self, n: usize) -> Vec<u32> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.next_buffer_name;
            self.next_buffer_name += 1;
            self.buffers.insert(name, Buffer::default());
            out.push(name);
        }
        out
    }

    /// `glBindBuffer`. Binding an unseen name creates the object, and
    /// binding 0 means "no buffer" — client arrays go back to reading
    /// guest memory directly.
    pub fn bind_buffer(&mut self, target: u32, name: u32) {
        if name != 0 {
            self.buffers.entry(name).or_default();
            self.next_buffer_name = self.next_buffer_name.max(name + 1);
        }
        match target {
            GL_ARRAY_BUFFER => self.array_buffer = name,
            GL_ELEMENT_ARRAY_BUFFER => self.element_array_buffer = name,
            _ => {
                self.set_error(GL_INVALID_ENUM);
                return;
            }
        }
        log::debug!(
            "GLES bind buffer target=0x{target:04x} name={name} objects={}",
            self.buffers.len()
        );
    }

    /// `glDeleteBuffers`. Deleting the bound buffer reverts the binding
    /// to 0, as GL requires.
    pub fn delete_buffers(&mut self, names: &[u32]) {
        for &name in names {
            if name == 0 {
                continue;
            }
            self.buffers.remove(&name);
            if self.array_buffer == name {
                self.array_buffer = 0;
            }
            if self.element_array_buffer == name {
                self.element_array_buffer = 0;
            }
        }
    }

    /// The buffer currently bound to `target`, or `None` for 0.
    fn bound_buffer_name(&self, target: u32) -> Option<u32> {
        let name = match target {
            GL_ARRAY_BUFFER => self.array_buffer,
            GL_ELEMENT_ARRAY_BUFFER => self.element_array_buffer,
            _ => return None,
        };
        (name != 0).then_some(name)
    }

    /// `glBufferData`: (re)allocate the store and fill it with `data`.
    /// A null `data` pointer allocates without initialising, which is
    /// legal and common — the guest follows up with `glBufferSubData`.
    pub fn buffer_data(&mut self, target: u32, size: usize, data: Option<&[u8]>, usage: u32) {
        let Some(name) = self.bound_buffer_name(target) else {
            // Uploading with no buffer bound is an error, not a no-op:
            // GL has nowhere to put the data.
            self.set_error(
                if target == GL_ARRAY_BUFFER || target == GL_ELEMENT_ARRAY_BUFFER {
                    GL_INVALID_OPERATION
                } else {
                    GL_INVALID_ENUM
                },
            );
            return;
        };
        let Some(buf) = self.buffers.get_mut(&name) else {
            return;
        };
        buf.usage = usage;
        buf.data.clear();
        buf.data.resize(size, 0);
        if let Some(src) = data {
            let n = src.len().min(size);
            buf.data[..n].copy_from_slice(&src[..n]);
        }
        log::debug!(
            "GLES buffer data target=0x{target:04x} name={name} size={size} usage=0x{usage:04x} initialised={}",
            data.is_some()
        );
    }

    /// `glBufferSubData`: overwrite part of an existing store. A range
    /// that runs past the end is an error and writes nothing.
    pub fn buffer_sub_data(&mut self, target: u32, offset: u32, data: &[u8]) {
        let Some(name) = self.bound_buffer_name(target) else {
            self.set_error(GL_INVALID_OPERATION);
            return;
        };
        let Some(buf) = self.buffers.get_mut(&name) else {
            return;
        };
        let start = offset as usize;
        let Some(end) = start.checked_add(data.len()) else {
            self.set_error(GL_INVALID_VALUE);
            return;
        };
        if end > buf.data.len() {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        buf.data[start..end].copy_from_slice(data);
    }

    pub fn set_active_texture(&mut self, unit: u32) {
        if unit as usize >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        self.active_texture = unit;
    }

    pub fn set_client_active_texture(&mut self, unit: u32) {
        if unit as usize >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        self.client_active_texture = unit;
    }

    /// `glVertexPointer`. The enable flag is owned by
    /// `glEnableClientState` and must survive a pointer change.
    pub fn set_vertex_pointer(&mut self, size: u32, ty: u32, stride: u32, pointer: u32) {
        self.vertex_array = ArrayPointer {
            enabled: self.vertex_array.enabled,
            size,
            ty,
            stride,
            pointer,
            buffer: self.array_buffer,
        };
    }

    /// `glColorPointer`.
    pub fn set_color_pointer(&mut self, size: u32, ty: u32, stride: u32, pointer: u32) {
        self.color_array = ArrayPointer {
            enabled: self.color_array.enabled,
            size,
            ty,
            stride,
            pointer,
            buffer: self.array_buffer,
        };
    }

    /// `glNormalPointer`. Normals are always three components.
    pub fn set_normal_pointer(&mut self, ty: u32, stride: u32, pointer: u32) {
        self.normal_array = ArrayPointer {
            enabled: self.normal_array.enabled,
            size: 3,
            ty,
            stride,
            pointer,
            buffer: self.array_buffer,
        };
    }

    pub fn set_texcoord_pointer(&mut self, size: u32, ty: u32, stride: u32, pointer: u32) {
        let unit = self.client_active_texture as usize;
        if unit >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        let enabled = self.texture_units[unit].texcoord_array.enabled;
        self.texture_units[unit].texcoord_array = ArrayPointer {
            enabled,
            size,
            ty,
            stride,
            pointer,
            buffer: self.array_buffer,
        };
    }

    pub fn set_multi_texcoord(&mut self, unit: u32, s: f32, t: f32) {
        let unit = unit.saturating_sub(GL_TEXTURE0) as usize;
        if unit >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        self.texture_units[unit].current_texcoord = [s, t];
        if unit == 0 {
            self.current_texcoord = [s, t];
        }
    }

    pub fn delete_textures(&mut self, names: &[u32]) {
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.delete_textures(names);
        }
        for &name in names {
            if name == 0 {
                continue;
            }
            self.textures.remove(&name);
            for (unit, state) in self.texture_units.iter_mut().enumerate() {
                if state.bound_texture == name {
                    state.bound_texture = 0;
                    if unit == 0 {
                        self.bound_texture = 0;
                    }
                }
            }
        }
    }

    /// `glTexImage2D`. `data` is the already-copied guest buffer; an
    /// empty slice means a null pointer, which allocates storage
    /// without initialising it.
    // The parameter list mirrors the GL entry point one-for-one.
    // Bundling them into a struct would only move the unpacking to
    // every call site in the dispatch layer.
    #[allow(clippy::too_many_arguments)]
    /// Copy framebuffer pixels into the canonical texture image. GL's source
    /// origin is bottom-left, while the framebuffer cache is stored top-down.
    pub fn copy_tex_image_2d(
        &mut self,
        target: u32,
        level: i32,
        format: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        border: i32,
    ) {
        if target != GL_TEXTURE_2D
            || !matches!(
                format,
                GL_ALPHA | GL_RGB | GL_RGBA | GL_LUMINANCE | GL_LUMINANCE_ALPHA
            )
        {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        if level < 0 || width < 0 || height < 0 || width > 1024 || height > 1024 || border != 0 {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        if level > 10 {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        self.sync_gpu();
        if self.gpu_failed {
            return;
        }
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for row in 0..height {
            for col in 0..width {
                let sx = i64::from(x) + i64::from(col);
                let sy = i64::from(y) + i64::from(row);
                let mut pixel = [0; 4];
                if sx >= 0
                    && sy >= 0
                    && sx < i64::from(self.target.width)
                    && sy < i64::from(self.target.height)
                {
                    let offset = ((i64::from(self.target.height) - 1 - sy)
                        * i64::from(self.target.width)
                        + sx) as usize
                        * 4;
                    pixel.copy_from_slice(&self.target.color[offset..offset + 4]);
                }
                pixel = match format {
                    GL_ALPHA => [255, 255, 255, pixel[3]],
                    GL_RGB => [pixel[0], pixel[1], pixel[2], 255],
                    GL_LUMINANCE => [pixel[0], pixel[0], pixel[0], 255],
                    GL_LUMINANCE_ALPHA => [pixel[0], pixel[0], pixel[0], pixel[3]],
                    _ => pixel,
                };
                rgba.extend(pixel);
            }
        }
        self.tex_image_2d(
            target,
            level,
            width as u32,
            height as u32,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &rgba,
        );
    }

    pub fn tex_image_2d(
        &mut self,
        target: u32,
        level: i32,
        width: u32,
        height: u32,
        format: u32,
        ty: u32,
        data: &[u8],
    ) {
        if target != GL_TEXTURE_2D {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        if !(0..=10).contains(&level) || width > (1024u32 >> level) || height > (1024u32 >> level) {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        let unit = self.active_texture as usize;
        if unit >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_OPERATION);
            return;
        }
        let rgba = if data.is_empty() {
            vec![0u8; (width as usize) * (height as usize) * 4]
        } else {
            match texture::decode_to_rgba(data, width, height, format, ty) {
                Some(v) => v,
                None => {
                    self.set_error(GL_INVALID_ENUM);
                    return;
                }
            }
        };
        let name = self.texture_units[self.active_texture as usize].bound_texture;
        let tex = self.textures.entry(name).or_default();
        tex.set_level(level as u32, width, height, rgba);
        log::debug!(
            "GLES tex image texture={} level={} {}x{} format=0x{format:04x} type=0x{ty:04x} bytes={} complete={} first={:02x?}",
            name,
            level,
            width,
            height,
            data.len(),
            tex.is_complete(),
            &tex.rgba[..tex.rgba.len().min(4)],
        );
    }

    /// `glCompressedTexImage2D` for the ATC formats.
    ///
    /// Decoded to RGBA at upload time like every other format, so the
    /// rasterizer never learns that compression exists.
    pub fn compressed_tex_image_2d(
        &mut self,
        target: u32,
        level: i32,
        width: u32,
        height: u32,
        format: u32,
        data: &[u8],
    ) {
        if target != GL_TEXTURE_2D {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        if texture::compressed_image_size(format, width, height).is_none() {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        // Chicane packs a shared palette followed by base/mip index rows.
        // Ordinary DDS levels (including RBR DXT3) are uploaded separately.
        let paletted = matches!(
            format,
            GL_PALETTE4_RGB8_OES
                | GL_PALETTE4_RGBA8_OES
                | GL_PALETTE4_R5_G6_B5_OES
                | GL_PALETTE4_RGBA4_OES
                | GL_PALETTE4_RGB5_A1_OES
                | GL_PALETTE8_RGB8_OES
                | GL_PALETTE8_RGBA8_OES
                | GL_PALETTE8_R5_G6_B5_OES
                | GL_PALETTE8_RGBA4_OES
                | GL_PALETTE8_RGB5_A1_OES
        );
        if !(-10..=10).contains(&level)
            || (level < 0 && !paletted)
            || width > (1024u32 >> level.max(0))
            || height > (1024u32 >> level.max(0))
        {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        if level < 0 {
            if (-level) as u32 > width.max(height).max(1).ilog2() {
                self.set_error(GL_INVALID_VALUE);
                return;
            }
            if !data.is_empty() {
                let entries = if format >= GL_PALETTE8_RGB8_OES {
                    256
                } else {
                    16
                };
                let entry_bytes = if matches!(format, GL_PALETTE4_RGB8_OES | GL_PALETTE8_RGB8_OES) {
                    3
                } else if matches!(format, GL_PALETTE4_RGBA8_OES | GL_PALETTE8_RGBA8_OES) {
                    4
                } else {
                    2
                };
                let palette_len = entries * entry_bytes;
                let mut needed = palette_len;
                for l in 0..=(-level) as u32 {
                    needed += texture::compressed_image_size(
                        format,
                        (width >> l).max(1),
                        (height >> l).max(1),
                    )
                    .unwrap()
                        - palette_len;
                }
                if data.len() < needed {
                    self.set_error(GL_INVALID_VALUE);
                    return;
                }
            }
        }
        let unit = self.active_texture as usize;
        if unit >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_OPERATION);
            return;
        }
        let defer = !paletted
            && !data.is_empty()
            && self
                .gpu
                .as_ref()
                .is_some_and(|g| g.supports_compressed(format));
        if !data.is_empty()
            && data.len() < texture::compressed_image_size(format, width, height).unwrap()
        {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        let rgba = if defer {
            Vec::new()
        } else if data.is_empty() {
            vec![0u8; (width as usize) * (height as usize) * 4]
        } else {
            match texture::decode_compressed_to_rgba(data, width, height, format) {
                Some(v) => v,
                None => {
                    // Right size for the format but the guest gave us
                    // short data — `GL_INVALID_VALUE` is what GL
                    // specifies for a mismatched `imageSize`.
                    self.set_error(GL_INVALID_VALUE);
                    return;
                }
            }
        };
        let name = self.texture_units[self.active_texture as usize].bound_texture;
        let tex = self.textures.entry(name).or_default();
        if defer {
            let needed = texture::compressed_image_size(format, width, height).unwrap();
            tex.set_compressed(
                level.max(0) as u32,
                width,
                height,
                format,
                &data[..needed],
                rgba,
            );
        } else {
            tex.set_level(level.max(0) as u32, width, height, rgba);
        }
        if level < 0 && !data.is_empty() {
            let entries = if format >= GL_PALETTE8_RGB8_OES {
                256
            } else {
                16
            };
            let entry_bytes = if matches!(format, GL_PALETTE4_RGB8_OES | GL_PALETTE8_RGB8_OES) {
                3
            } else if matches!(format, GL_PALETTE4_RGBA8_OES | GL_PALETTE8_RGBA8_OES) {
                4
            } else {
                2
            };
            let palette_len = entries * entry_bytes;
            let mut offset = texture::compressed_image_size(format, width, height).unwrap();
            for l in 1..=(-level) as u32 {
                let (w, h) = ((width >> l).max(1), (height >> l).max(1));
                let size = texture::compressed_image_size(format, w, h).unwrap() - palette_len;
                if offset + size > data.len() {
                    self.set_error(GL_INVALID_VALUE);
                    return;
                }
                let mut packed = data[..palette_len].to_vec();
                packed.extend_from_slice(&data[offset..offset + size]);
                if let Some(rgba) = texture::decode_compressed_to_rgba(&packed, w, h, format) {
                    tex.set_level(l, w, h, rgba);
                }
                offset += size;
            }
        }

        log::debug!(
            "GLES compressed tex image texture={name} {width}x{height} \
             format=0x{format:04x} bytes={} complete={}",
            data.len(),
            tex.is_complete(),
        );
    }

    /// `glTexSubImage2D`: patch a rectangle of the bound texture.
    // The parameter list mirrors the GL entry point one-for-one.
    #[allow(clippy::too_many_arguments)]
    pub fn tex_sub_image_2d(
        &mut self,
        target: u32,
        level: i32,
        xoffset: u32,
        yoffset: u32,
        width: u32,
        height: u32,
        format: u32,
        ty: u32,
        data: &[u8],
    ) {
        if target != GL_TEXTURE_2D {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        if !(0..=10).contains(&level) || width > 1024 || height > 1024 {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        let Some(patch) = texture::decode_to_rgba(data, width, height, format, ty) else {
            self.set_error(GL_INVALID_ENUM);
            return;
        };
        let unit = self.active_texture as usize;
        if unit >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        let name = self.texture_units[unit].bound_texture;
        let Some(tex) = self.textures.get_mut(&name) else {
            self.set_error(GL_INVALID_OPERATION);
            return;
        };
        tex.ensure_rgba();
        let (tw, th, pixels) = if level == 0 {
            (tex.width, tex.height, &mut tex.rgba)
        } else if let Some((w, h, p)) = tex.mipmaps.get_mut(&(level as u32)) {
            (*w, *h, p)
        } else {
            self.set_error(GL_INVALID_OPERATION);
            return;
        };
        if xoffset.checked_add(width).is_none_or(|v| v > tw)
            || yoffset.checked_add(height).is_none_or(|v| v > th)
        {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        tex.compressed.remove(&(level as u32));
        tex.revision = tex.revision.wrapping_add(1);
        for row in 0..height {
            let src = (row * width * 4) as usize;
            let dst = (((yoffset + row) * tw + xoffset) * 4) as usize;
            let len = (width * 4) as usize;
            pixels[dst..dst + len].copy_from_slice(&patch[src..src + len]);
        }
    }

    pub fn tex_parameter(&mut self, target: u32, pname: u32, value: u32) {
        if target != GL_TEXTURE_2D {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        if pname == GL_TEXTURE_MAX_LEVEL_SGIS && (value as i32) < 0 {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        let unit = self.active_texture as usize;
        if unit >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        let name = self.texture_units[unit].bound_texture;
        let tex = self.textures.entry(name).or_default();
        if !tex.set_parameter(pname, value) {
            self.set_error(GL_INVALID_ENUM);
        }
    }

    pub fn set_lod_bias(&mut self, value: f32) {
        if !value.is_finite() {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        self.texture_units[self.active_texture as usize].lod_bias = value.clamp(-4., 4.);
    }

    pub fn tex_env(&mut self, pname: u32, value: u32) {
        if pname != GL_TEXTURE_ENV_MODE {
            // TEXTURE_ENV_COLOR and the COMBINE parameters are accepted
            // and ignored rather than erroring.
            return;
        }
        let mode = match value {
            GL_MODULATE => TexEnvMode::Modulate,
            GL_REPLACE => TexEnvMode::Replace,
            GL_DECAL => TexEnvMode::Decal,
            GL_ADD => TexEnvMode::Add,
            GL_COMBINE => TexEnvMode::Modulate,
            // GL_BLEND shares its value with the blend enable enum.
            0x0BE2 => TexEnvMode::Blend,
            _ => {
                self.set_error(GL_INVALID_ENUM);
                return;
            }
        };
        let unit = self.active_texture as usize;
        if unit >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        self.texture_units[unit].tex_env = mode;
    }

    /// `glTexEnv(GL_TEXTURE_ENV_COLOR)`, which like the mode belongs to
    /// the active unit.
    pub fn set_tex_env_color(&mut self, color: [f32; 4]) {
        let unit = self.active_texture as usize;
        if unit >= MAX_TEXTURE_UNITS {
            self.set_error(GL_INVALID_ENUM);
            return;
        }
        self.texture_units[unit].tex_env_color = color;
    }

    // ---- fragment state -------------------------------------------------

    pub fn set_point_size(&mut self, size: f32) {
        if !size.is_finite() || size <= 0. {
            self.set_error(GL_INVALID_VALUE);
        } else {
            self.state.point_size = size;
        }
    }
    pub fn set_line_width(&mut self, width: f32) {
        if !width.is_finite() || width <= 0. {
            self.set_error(GL_INVALID_VALUE);
        } else {
            self.state.line_width = width;
        }
    }
    pub fn primitive_size_range(&self, point: bool) -> [f32; 2] {
        self.gpu.as_ref().map_or(
            [1., 64.],
            |g| if point { g.point_range } else { g.line_range },
        )
    }

    pub fn set_depth_func(&mut self, func: u32) {
        match CompareFunc::from_enum(func) {
            Some(f) => self.state.depth_func = f,
            None => self.set_error(GL_INVALID_ENUM),
        }
    }

    pub fn set_alpha_func(&mut self, func: u32, reference: f32) {
        match CompareFunc::from_enum(func) {
            Some(f) => {
                self.state.alpha_func = f;
                self.state.alpha_ref = reference.clamp(0.0, 1.0);
            }
            None => self.set_error(GL_INVALID_ENUM),
        }
    }

    pub fn set_blend_func(&mut self, src: u32, dst: u32) {
        match (BlendFactor::from_enum(src), BlendFactor::from_enum(dst)) {
            (Some(s), Some(d)) => {
                self.state.blend_src = s;
                self.state.blend_dst = d;
            }
            _ => self.set_error(GL_INVALID_ENUM),
        }
    }

    pub fn set_cull_face(&mut self, mode: u32) {
        let cull = match mode {
            GL_FRONT => CullMode::Front,
            GL_BACK => CullMode::Back,
            GL_FRONT_AND_BACK => CullMode::FrontAndBack,
            _ => {
                self.set_error(GL_INVALID_ENUM);
                return;
            }
        };
        self.cull_mode = cull;
        self.sync_cull();
    }

    /// `GL_CULL_FACE` and `glCullFace` are independent pieces of state;
    /// the rasterizer only sees the combination, so recompute it
    /// whenever either changes.
    fn sync_cull(&mut self) {
        self.state.cull = if self.cull_enabled {
            Some(self.cull_mode)
        } else {
            None
        };
    }

    pub fn set_front_face(&mut self, mode: u32) {
        match mode {
            GL_CW => self.state.front_face = FrontFace::Cw,
            GL_CCW => self.state.front_face = FrontFace::Ccw,
            _ => self.set_error(GL_INVALID_ENUM),
        }
    }

    pub fn set_shade_model(&mut self, mode: u32) {
        match mode {
            GL_FLAT => self.state.smooth_shading = false,
            GL_SMOOTH => self.state.smooth_shading = true,
            _ => self.set_error(GL_INVALID_ENUM),
        }
    }

    pub fn set_fog(&mut self, pname: u32, value: f32) {
        log::debug!("GLES fog scalar pname=0x{pname:04x} value={value:.6}",);
        match pname {
            GL_FOG_MODE => {
                // GL_LINEAR and GL_EXP share numeric space with other
                // enums, so compare against the fog values directly.
                self.state.fog_mode = match value as u32 {
                    GL_EXP => FogMode::Exp,
                    GL_EXP2 => FogMode::Exp2,
                    GL_LINEAR => FogMode::Linear,
                    _ => {
                        self.set_error(GL_INVALID_ENUM);
                        return;
                    }
                };
            }
            GL_FOG_DENSITY => self.state.fog_density = value,
            GL_FOG_START => self.state.fog_start = value,
            GL_FOG_END => self.state.fog_end = value,
            _ => self.set_error(GL_INVALID_ENUM),
        }
        log::debug!(
            "GLES fog state mode={:?} density={:.6} start={:.6} end={:.6} color={:?} enabled={}",
            self.state.fog_mode,
            self.state.fog_density,
            self.state.fog_start,
            self.state.fog_end,
            self.state.fog_color,
            self.state.fog,
        );
    }

    pub fn set_viewport(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if w < 0 || h < 0 {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        self.state.viewport = (x, y, w, h);
    }

    pub fn set_scissor(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if w < 0 || h < 0 {
            self.set_error(GL_INVALID_VALUE);
            return;
        }
        self.state.scissor = Some((x, y, w, h));
    }

    pub fn set_depth_range(&mut self, near: f32, far: f32) {
        self.state.depth_range = (near.clamp(0.0, 1.0), far.clamp(0.0, 1.0));
    }

    // ---- clearing -------------------------------------------------------

    pub fn clear(&mut self, mask: u32) {
        if self.gpu_failed {
            return;
        }
        if let Some(gpu) = self.gpu.as_mut() {
            if let Err(e) = gpu.clear(
                self.target.width,
                self.target.height,
                &self.state,
                self.clear_color,
                self.clear_depth,
                mask,
            ) {
                self.gpu_failure("clear", e);
            }
            return;
        }
        // Ordering against the draws is the whole point: a frame that
        // clears *after* drawing looks exactly like a frame that never
        // drew, and only an interleaved log tells the two apart.
        log::trace!(
            "GLES clear mask=0x{mask:04x} color={:?} depth={}",
            self.clear_color,
            self.clear_depth
        );
        if mask & GL_COLOR_BUFFER_BIT != 0 {
            let c = [
                (self.clear_color[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                (self.clear_color[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                (self.clear_color[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                (self.clear_color[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            ];
            self.target.clear_color(c, self.state.scissor);
        }
        if mask & GL_DEPTH_BUFFER_BIT != 0 {
            let d = self.clear_depth;
            self.target.clear_depth(d, self.state.scissor);
        }
    }

    // ---- drawing --------------------------------------------------------

    /// The combined projection × modelview transform applied to every
    /// incoming position.
    fn mvp(&self) -> Matrix4 {
        matrix::multiply(self.projection.current(), self.modelview.current())
    }

    /// Fetch element `index` of `array`, writing up to 4 components.
    /// Missing components take their GL defaults: `(0, 0, 0, 1)`.
    fn fetch(
        &mut self,
        mem: &mut dyn GuestMemory,
        array: ArrayPointer,
        index: u32,
        normalize: bool,
    ) -> Option<[f32; 4]> {
        let comp = array.component_bytes()?;
        let stride = array.element_stride()?;
        let addr = array.pointer.checked_add(stride.checked_mul(index)?)?;
        let len = (comp * array.size) as usize;

        if let Some((base, bytes)) = self.prepared_arrays.iter().find(|(base, bytes)| {
            array.buffer == 0
                && addr.checked_sub(*base).is_some_and(|off| {
                    (off as usize)
                        .checked_add(len)
                        .is_some_and(|end| end <= bytes.len())
                })
        }) {
            let offset = addr.checked_sub(*base)? as usize;
            let bytes = bytes.get(offset..offset.checked_add(len)?)?;
            let mut out = [0., 0., 0., 1.];
            for (c, slot) in out.iter_mut().enumerate().take(array.size as usize) {
                *slot = decode_component(&bytes[c * comp as usize..], array.ty, normalize);
            }
            return Some(out);
        }

        // A pointer captured while a VBO was bound is an offset into
        // that buffer's host-side copy, not a guest address. Reading it
        // as a guest address would dereference a small integer and, on
        // an unlucky mapping, silently render garbage.
        if array.buffer != 0 {
            let buf = self.buffers.get(&array.buffer)?;
            let bytes = buf.slice(addr, len)?;
            let mut out = [0.0, 0.0, 0.0, 1.0];
            for (c, slot) in out.iter_mut().enumerate().take(array.size as usize) {
                let off = c * comp as usize;
                *slot = decode_component(&bytes[off..], array.ty, normalize);
            }
            return Some(out);
        }

        let mut buf = std::mem::take(&mut self.scratch);
        let ok = mem.read(addr, len, &mut buf) && buf.len() >= len;
        let mut out = [0.0, 0.0, 0.0, 1.0];
        if ok {
            for (c, slot) in out.iter_mut().enumerate().take(array.size as usize) {
                let off = c * comp as usize;
                *slot = decode_component(&buf[off..], array.ty, normalize);
            }
        }
        self.scratch = buf;
        if ok {
            Some(out)
        } else {
            None
        }
    }

    /// The texture stages this draw cascades through, in unit order.
    ///
    /// Unit 0 is *not* a safe assumption, and neither is a single stage:
    /// nothing in GL says a textured draw has to use the first unit, and
    /// engines built for multitexturing hardware routinely leave stage 0
    /// disabled and drive the diffuse map through a later stage.
    /// X-Forge does exactly that — Sticky Balls sets up every draw as
    ///
    /// ```text
    /// glActiveTexture(GL_TEXTURE0); glDisable(GL_TEXTURE_2D); glBindTexture(2D, 0);
    /// glActiveTexture(GL_TEXTURE1); glEnable(GL_TEXTURE_2D);  glBindTexture(2D, colour);
    /// glActiveTexture(GL_TEXTURE2); glEnable(GL_TEXTURE_2D);  glBindTexture(2D, alpha);
    /// ```
    ///
    /// and it needs *both* of those stages: the colour map is DXT1, a
    /// format with no alpha channel, so the shape's cut-out lives in a
    /// separate white-RGB mask on the stage above. Sampling either one
    /// alone is visibly wrong — the mask alone paints white over the
    /// scene, the colour map alone leaves every ball and cloud as an
    /// opaque square.
    ///
    /// A unit whose texture has never been given pixels is skipped
    /// rather than sampled: GL ES treats an incomplete texture as if
    /// texturing were off for that unit, and a stage bound to a
    /// just-generated name must not shadow a later stage that has real
    /// pixels.
    fn texture_stages(&self) -> ([raster::TextureStage; MAX_TEXTURE_UNITS], usize) {
        let mut stages = [raster::TextureStage {
            unit: 0,
            env: TexEnvMode::Modulate,
            env_color: [0.; 4],
            lod_bias: 0.,
        }; MAX_TEXTURE_UNITS];
        let mut count = 0;
        for u in 0..MAX_TEXTURE_UNITS {
            let unit = &self.texture_units[u];
            if unit.texture_enabled
                && self
                    .textures
                    .get(&unit.bound_texture)
                    .is_some_and(Texture::is_complete)
            {
                stages[count] = raster::TextureStage {
                    unit: u,
                    env: unit.tex_env,
                    env_color: unit.tex_env_color,
                    lod_bias: unit.lod_bias,
                };
                count += 1;
            }
        }
        (stages, count)
    }

    /// Assemble one vertex from the enabled client arrays. `stages` are
    /// the units this draw samples, and so the only units whose
    /// coordinate arrays are worth reading; see
    /// [`Context::texture_stages`].
    fn assemble(
        &mut self,
        mem: &mut dyn GuestMemory,
        index: u32,
        mvp: &Matrix4,
        stages: &[raster::TextureStage],
        normal_matrix: &Matrix4,
    ) -> Option<Vertex> {
        let obj = self.fetch(mem, self.vertex_array, index, false)?;
        let clip = if self.gpu.is_some() {
            obj
        } else {
            matrix::transform(mvp, obj)
        };

        let color = if self.color_array.enabled {
            // Colours from integer arrays are normalized to [0, 1];
            // this is the one place GL treats a UNSIGNED_BYTE array as
            // a fraction rather than a raw value.
            self.fetch(mem, self.color_array, index, true)
                .unwrap_or(self.current_color)
        } else {
            self.current_color
        };

        let mut texcoord = [[0.0f32; 2]; raster::MAX_TEXTURE_STAGES];
        for stage in stages {
            let unit = self.texture_units[stage.unit];
            texcoord[stage.unit] = if unit.texcoord_array.enabled {
                let t = self
                    .fetch(mem, unit.texcoord_array, index, false)
                    .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                // The texture matrix applies to incoming coordinates.
                let m = if self.gpu.is_some() {
                    t
                } else {
                    matrix::transform(
                        self.texture_matrix[stage.unit].current(),
                        [t[0], t[1], 0.0, 1.0],
                    )
                };
                [m[0], m[1]]
            } else {
                unit.current_texcoord
            };
        }

        // Eye-space Z drives fog. The modelview alone takes us to eye
        // space; GL fogs on |z_eye|.
        let eye = if self.gpu.is_some() {
            [0.0; 4]
        } else {
            matrix::transform(self.modelview.current(), obj)
        };
        let normal = if self.lighting.enabled && self.normal_array.enabled {
            let n = self.fetch(mem, self.normal_array, index, true)?;
            [n[0], n[1], n[2]]
        } else {
            self.current_normal
        };
        let (color, back_color) = if self.gpu.is_none() && self.lighting.enabled {
            let n = self.lighting.transform_normal(normal_matrix, normal);
            let point = std::array::from_fn(|i| {
                if eye[3] == 0. {
                    eye[i]
                } else {
                    eye[i] / eye[3]
                }
            });
            let front = self.lighting.shade(n, point, color);
            let back = if self.lighting.two_sided {
                self.lighting.shade(n.map(|x| -x), point, color)
            } else {
                front
            };
            (front, back)
        } else {
            (color, color)
        };
        Some(Vertex {
            normal,
            back_color,
            pos: clip,
            color,
            texcoord,
            fog_depth: eye[2].abs(),
        })
    }

    /// Is there vertex data to draw from? A pointer of 0 is legitimate
    /// when it is an offset into a bound VBO, so the null check only
    /// applies to client-side arrays.
    fn vertex_source_ready(&self) -> bool {
        self.vertex_array.enabled
            && (self.vertex_array.buffer != 0 || self.vertex_array.pointer != 0)
    }

    /// `glDrawArrays`.
    pub fn draw_arrays(&mut self, mem: &mut dyn GuestMemory, mode: u32, first: u32, count: u32) {
        if !self.vertex_source_ready() {
            return;
        }
        let end = first.saturating_add(count);
        self.draw_indexed(mem, mode, first..end, (end - first) as usize, true);
    }

    /// `glDrawElements`. `indices` has already been read out of guest
    /// memory and widened to `u32`.
    pub fn draw_elements(&mut self, mem: &mut dyn GuestMemory, mode: u32, indices: &[u32]) {
        if !self.vertex_source_ready() {
            return;
        }
        self.draw_indexed(mem, mode, indices.iter().copied(), indices.len(), false);
    }

    /// Read an index buffer out of guest memory and draw with it.
    pub fn draw_elements_from_guest(
        &mut self,
        mem: &mut dyn GuestMemory,
        mode: u32,
        count: u32,
        ty: u32,
        pointer: u32,
    ) {
        let width = match ty {
            GL_UNSIGNED_BYTE => 1usize,
            GL_UNSIGNED_SHORT => 2usize,
            _ => {
                self.set_error(GL_INVALID_ENUM);
                return;
            }
        };
        let Some(len) = (count as usize).checked_mul(width) else {
            self.set_error(GL_INVALID_VALUE);
            return;
        };

        let mut indices = std::mem::take(&mut self.decoded_indices);
        indices.clear();
        if self.element_array_buffer != 0 {
            if let Some(bytes) = self
                .buffers
                .get(&self.element_array_buffer)
                .and_then(|b| b.slice(pointer, len))
            {
                if width == 1 {
                    indices.extend(bytes.iter().map(|b| *b as u32));
                } else {
                    indices.extend(
                        bytes
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]]) as u32),
                    );
                }
            } else {
                self.set_error(GL_INVALID_VALUE);
            }
        } else {
            let mut bytes = std::mem::take(&mut self.index_scratch);
            if mem.read(pointer, len, &mut bytes) && bytes.len() >= len {
                if width == 1 {
                    indices.extend(bytes[..len].iter().map(|b| *b as u32));
                } else {
                    indices.extend(
                        bytes[..len]
                            .chunks_exact(2)
                            .map(|b| u16::from_le_bytes([b[0], b[1]]) as u32),
                    );
                }
            }
            self.index_scratch = bytes;
        }
        if !indices.is_empty() {
            self.draw_elements(mem, mode, &indices);
        }
        self.decoded_indices = indices;
    }

    fn recycle_prepared_arrays(&mut self) {
        for (_, mut bytes) in self.prepared_arrays.drain(..) {
            bytes.clear();
            self.array_read_pool.push(bytes);
        }
    }

    fn stream_array(
        &self,
        stream: &mut crate::gpu::VertexStream,
        slot: usize,
        array: ArrayPointer,
        lo: u32,
        count: usize,
        normalize: bool,
        components: u32,
        native_fixed: bool,
    ) -> Option<()> {
        let comp = array.component_bytes()?;
        let stride = array.element_stride()?;
        let start = array.pointer.checked_add(lo.checked_mul(stride)?)?;
        let len = (count.checked_sub(1)? as u32)
            .checked_mul(stride)?
            .checked_add(array.size.checked_mul(comp)?)? as usize;
        start.checked_add(len as u32)?;
        if len > 1024 * 1024 {
            return None;
        }
        let source = if array.buffer != 0 {
            self.buffers.get(&array.buffer)?.slice(start, len)?
        } else {
            let (base, bytes) = self.prepared_arrays.iter().find(|(base, bytes)| {
                start.checked_sub(*base).is_some_and(|off| {
                    (off as usize)
                        .checked_add(len)
                        .is_some_and(|end| end <= bytes.len())
                })
            })?;
            &bytes[(start - *base) as usize..(start - *base) as usize + len]
        };
        let size = array.size.min(components);
        if size == 0 || size > 4 || stride > i32::MAX as u32 {
            return None;
        }
        let offset = (stream.bytes.len() + 3) & !3;
        stream.bytes.resize(offset, 0);
        let convert = (array.ty == GL_FIXED && !native_fixed) || stride % comp != 0;
        let (ty, output_stride, normalized) = if convert {
            for i in 0..count {
                for c in 0..size as usize {
                    let at = i * stride as usize + c * comp as usize;
                    stream.bytes.extend_from_slice(
                        &decode_component(&source[at..], array.ty, normalize).to_le_bytes(),
                    );
                }
            }
            (GL_FLOAT, (size * 4) as i32, 0)
        } else {
            let packed = (size * comp) as usize;
            if stride as usize == packed {
                stream.bytes.extend_from_slice(&source[..count * packed]);
            } else {
                // Keep native formats, but omit interleaved padding/other attributes.
                for i in 0..count {
                    let start = i * stride as usize;
                    stream
                        .bytes
                        .extend_from_slice(&source[start..start + packed]);
                }
            }
            (
                array.ty,
                packed as i32,
                (normalize && !matches!(array.ty, GL_FLOAT | GL_FIXED)) as u8,
            )
        };
        stream.attributes[slot] = Some(crate::gpu::StreamAttribute {
            size: size as i32,
            ty,
            normalized,
            stride: output_stride,
            offset,
        });
        Some(())
    }

    fn prepare_vertex_stream(
        &self,
        stream: &mut crate::gpu::VertexStream,
        indices: impl Iterator<Item = u32> + Clone,
        count: usize,
        sequential: bool,
        stages: &[raster::TextureStage],
    ) -> bool {
        stream.bytes.clear();
        stream.indices.clear();
        stream.attributes.fill(None);
        stream.constants = [[0., 0., 0., 1.]; 8];
        stream.constants[1] = self.current_color;
        stream.constants[6] = [
            self.current_normal[0],
            self.current_normal[1],
            self.current_normal[2],
            0.,
        ];
        let Some(lo) = indices.clone().min() else {
            return false;
        };
        let Some(hi) = indices.clone().max() else {
            return false;
        };
        let span = hi - lo;
        if span >= 65536 || span > (count as u32).saturating_mul(4) {
            return false;
        }
        stream.count = span as usize + 1;
        if !sequential {
            stream.coverage.resize(stream.count, 0);
            stream.coverage.fill(0);
            for i in indices {
                let relative = (i - lo) as u16;
                stream.indices.push(relative);
                stream.coverage[relative as usize] = 1;
            }
            // Hole-heavy draws keep the existing unique-vertex path rather than
            // uploading unused vertices and inflating native transfer size.
            if stream.coverage.contains(&0) {
                stream.indices.clear();
                return false;
            }
        }
        let native_fixed = self
            .gpu
            .as_ref()
            .is_some_and(|g| g.supports_fixed_vertices());
        let ready = (|| {
            self.stream_array(
                stream,
                0,
                self.vertex_array,
                lo,
                stream.count,
                false,
                4,
                native_fixed,
            )?;
            if self.color_array.enabled {
                self.stream_array(
                    stream,
                    1,
                    self.color_array,
                    lo,
                    stream.count,
                    true,
                    4,
                    native_fixed,
                )?;
            }
            for stage in stages {
                let unit = self.texture_units[stage.unit];
                if unit.texcoord_array.enabled {
                    self.stream_array(
                        stream,
                        stage.unit + 2,
                        unit.texcoord_array,
                        lo,
                        stream.count,
                        false,
                        2,
                        native_fixed,
                    )?;
                } else {
                    stream.constants[stage.unit + 2] =
                        [unit.current_texcoord[0], unit.current_texcoord[1], 0., 1.];
                }
            }
            if self.lighting.enabled {
                if self.normal_array.enabled {
                    self.stream_array(
                        stream,
                        6,
                        self.normal_array,
                        lo,
                        stream.count,
                        true,
                        3,
                        native_fixed,
                    )?;
                }
                stream.attributes[7] = stream.attributes[0];
            }
            Some(())
        })()
        .is_some();
        if !ready {
            stream.bytes.clear();
            stream.indices.clear();
        }
        ready
    }

    fn draw_indexed(
        &mut self,
        mem: &mut dyn GuestMemory,
        mode: u32,
        indices: impl Iterator<Item = u32> + Clone,
        count: usize,
        sequential: bool,
    ) {
        if self.gpu_failed {
            return;
        }
        if self.gpu.is_none() {
            for tex in self.textures.values_mut() {
                tex.ensure_rgba();
            }
        }
        let mvp = if self.gpu.is_some() {
            matrix::IDENTITY
        } else {
            self.mvp()
        };
        // The stages own the coordinate arrays to read, so they have to
        // be resolved before assembling vertices.
        let normal_matrix = if self.lighting.enabled {
            crate::lighting::normal_matrix(self.modelview.current())
        } else {
            matrix::IDENTITY
        };
        self.state.two_sided_lighting = self.lighting.enabled && self.lighting.two_sided;
        let (stage_storage, stage_count) = self.texture_stages();
        let stages = &stage_storage[..stage_count];
        self.recycle_prepared_arrays();
        if let (Some(lo), Some(hi)) = (indices.clone().min(), indices.clone().max()) {
            let mut arrays = [ArrayPointer::default(); MAX_TEXTURE_UNITS + 3];
            arrays[0] = self.vertex_array;
            let mut array_count = 1;
            if self.color_array.enabled {
                arrays[array_count] = self.color_array;
                array_count += 1;
            }
            if self.lighting.enabled && self.normal_array.enabled {
                arrays[array_count] = self.normal_array;
                array_count += 1;
            }
            for stage in stages {
                let array = self.texture_units[stage.unit].texcoord_array;
                if array.enabled {
                    arrays[array_count] = array;
                    array_count += 1;
                }
            }
            let mut spans = [(0u32, 0u32); MAX_TEXTURE_UNITS + 3];
            let mut span_count = 0;
            for &array in &arrays[..array_count] {
                if array.buffer != 0 {
                    continue;
                }
                let span = (|| {
                    let stride = array.element_stride()?;
                    let start = array.pointer.checked_add(lo.checked_mul(stride)?)?;
                    let len = hi
                        .checked_sub(lo)?
                        .checked_mul(stride)?
                        .checked_add(array.component_bytes()?.checked_mul(array.size)?)?;
                    start.checked_add(len)?;
                    // Sparse indices and enormous strides keep the element-by-element path.
                    if len > 1024 * 1024 || hi - lo > (count as u32).saturating_mul(4) {
                        return None;
                    }
                    Some((start, len as usize))
                })();
                if let Some((start, len)) = span {
                    spans[span_count] = (start, start + len as u32);
                    span_count += 1;
                }
            }
            spans[..span_count].sort_unstable_by_key(|s| s.0);
            let mut merged = 0;
            for i in 0..span_count {
                let (start, end) = spans[i];
                if merged > 0
                    && start <= spans[merged - 1].1
                    && end.max(spans[merged - 1].1) - spans[merged - 1].0 <= 1024 * 1024
                {
                    spans[merged - 1].1 = end.max(spans[merged - 1].1);
                } else {
                    spans[merged] = (start, end);
                    merged += 1;
                }
            }
            for &(start, end) in &spans[..merged] {
                let len = (end - start) as usize;
                let mut bytes = self.array_read_pool.pop().unwrap_or_default();
                bytes.clear();
                if mem.read(start, len, &mut bytes) && bytes.len() >= len {
                    bytes.truncate(len);
                    self.prepared_arrays.push((start, bytes));
                } else {
                    bytes.clear();
                    self.array_read_pool.push(bytes);
                }
            }
        }

        let transforms = crate::gpu::VertexTransforms {
            lighting: self.lighting,
            normal_matrix,
            projection: *self.projection.current(),
            modelview: *self.modelview.current(),
            textures: std::array::from_fn(|i| {
                if self.texture_units[i].texcoord_array.enabled {
                    *self.texture_matrix[i].current()
                } else {
                    matrix::IDENTITY
                }
            }),
        };
        if self.gpu.is_some()
            && self.state.smooth_shading
            && matches!(
                mode,
                GL_TRIANGLES | GL_TRIANGLE_STRIP | GL_TRIANGLE_FAN | GL_POINTS | GL_LINES
            )
        {
            let mut stream = std::mem::take(&mut self.vertex_stream);
            let ready =
                self.prepare_vertex_stream(&mut stream, indices.clone(), count, sequential, stages);
            if ready {
                let names = std::array::from_fn(|i| self.texture_units[i].bound_texture);
                let result = self.gpu.as_mut().unwrap().draw_stream_object_space(
                    &self.target,
                    &self.state,
                    mode,
                    &stream,
                    stages,
                    &self.textures,
                    names,
                    &transforms,
                );
                self.vertex_stream = stream;
                self.recycle_prepared_arrays();
                if let Err(e) = result {
                    self.gpu_failure("vertex stream", e);
                }
                return;
            }
            self.vertex_stream = stream;
        }
        // Native smooth draws retain the topology and prepare each unique
        // guest vertex directly, instead of expanding then hashing float records.
        let native_indices = !sequential
            && self.gpu.is_some()
            && self.state.smooth_shading
            && matches!(
                mode,
                GL_TRIANGLES | GL_TRIANGLE_STRIP | GL_TRIANGLE_FAN | GL_POINTS | GL_LINES
            );
        let mut remap: HashMap<u32, usize> = HashMap::new();
        let mut verts: Vec<Vertex> = Vec::with_capacity(count.min(65536));
        let mut dense = Vec::<u16>::new();
        let mut expanded = Vec::<usize>::new();
        for i in indices {
            let index = if !sequential {
                remap.get(&i).copied()
            } else {
                None
            };
            let index = if let Some(index) = index {
                index
            } else {
                let Some(v) = self.assemble(mem, i, &mvp, &stages, &normal_matrix) else {
                    self.recycle_prepared_arrays();
                    return;
                };
                let index = verts.len();
                verts.push(v);
                if !sequential {
                    remap.insert(i, index);
                }
                index
            };
            if !sequential {
                expanded.push(index);
            }
        }
        let direct_indexed = native_indices && verts.len() <= 65536;
        if direct_indexed {
            dense.extend(expanded.iter().map(|i| *i as u16));
        } else if !sequential {
            verts = expanded.iter().map(|i| verts[*i]).collect();
        }
        self.recycle_prepared_arrays();

        match mode {
            GL_TRIANGLES | GL_TRIANGLE_STRIP | GL_TRIANGLE_FAN | GL_POINTS | GL_LINES
            | GL_LINE_STRIP | GL_LINE_LOOP => {}
            _ => {
                self.set_error(GL_INVALID_ENUM);
                return;
            }
        }

        if let Some(gpu) = self.gpu.as_mut() {
            let names = std::array::from_fn(|i| self.texture_units[i].bound_texture);
            let result = if direct_indexed {
                gpu.draw_indexed_object_space(
                    &self.target,
                    &self.state,
                    mode,
                    &verts,
                    &dense,
                    &stages,
                    &self.textures,
                    names,
                    &transforms,
                )
            } else {
                let draw = if sequential {
                    crate::gpu::Renderer::draw_arrays_object_space
                } else {
                    crate::gpu::Renderer::draw_object_space
                };
                draw(
                    gpu,
                    &self.target,
                    &self.state,
                    mode,
                    &verts,
                    &stages,
                    &self.textures,
                    names,
                    &transforms,
                )
            };
            if let Err(e) = result {
                self.gpu_failure("draw", e);
            }
            return;
        }
        // Resolve the bound textures once per draw, not per fragment.
        let mut texs: [Option<&Texture>; raster::MAX_TEXTURE_STAGES] = [None; _];
        for stage in stages {
            texs[stage.unit] = self
                .textures
                .get(&self.texture_units[stage.unit].bound_texture);
        }
        log::debug!(
            "GLES draw mode=0x{mode:04x} indices={} stages={:?} active_unit={} vertex_array={} blend={} src={:?} dst={:?} alpha_test={} func={:?} ref={}",
            count,
            stages
                .iter()
                .map(|s| (s.unit, self.texture_units[s.unit].bound_texture, s.env))
                .collect::<Vec<_>>(),
            self.active_texture,
            self.vertex_array.enabled,
            self.state.blend,
            self.state.blend_src,
            self.state.blend_dst,
            self.state.alpha_test,
            self.state.alpha_func,
            self.state.alpha_ref,
        );
        let sample = |unit: usize, s: f32, t: f32| texs[unit].map(|tx| tx.sample(s, t));

        match mode {
            GL_POINTS => {
                for v in &verts {
                    raster::draw_point(&mut self.target, &self.state, &sample, &stages, *v);
                }
                return;
            }
            GL_LINES | GL_LINE_STRIP | GL_LINE_LOOP => {
                if mode == GL_LINES {
                    for v in verts.chunks_exact(2) {
                        raster::draw_line(
                            &mut self.target,
                            &self.state,
                            &sample,
                            &stages,
                            [v[0], v[1]],
                        );
                    }
                } else {
                    for v in verts.windows(2) {
                        raster::draw_line(
                            &mut self.target,
                            &self.state,
                            &sample,
                            &stages,
                            [v[0], v[1]],
                        );
                    }
                    if mode == GL_LINE_LOOP && verts.len() > 1 {
                        raster::draw_line(
                            &mut self.target,
                            &self.state,
                            &sample,
                            &stages,
                            [*verts.last().unwrap(), verts[0]],
                        );
                    }
                }
                return;
            }
            _ => {}
        }

        // Stream primitives in their original order without allocating a triangle list.
        let mut draw = |tri| {
            raster::draw_triangle(&mut self.target, &self.state, &sample, &stages, tri);
        };
        match mode {
            GL_TRIANGLES => {
                for c in verts.chunks_exact(3) {
                    draw([c[0], c[1], c[2]]);
                }
            }
            GL_TRIANGLE_STRIP => {
                for i in 2..verts.len() {
                    // Preserve alternating winding and the provoking vertex.
                    if i % 2 == 0 {
                        draw([verts[i - 2], verts[i - 1], verts[i]]);
                    } else {
                        draw([verts[i - 1], verts[i - 2], verts[i]]);
                    }
                }
            }
            GL_TRIANGLE_FAN => {
                for i in 2..verts.len() {
                    draw([verts[0], verts[i - 1], verts[i]]);
                }
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn anisotropy_setting_does_not_enable_software_context_gpu() {
        let mut c = Context::new(4, 4);
        c.set_anisotropic_filtering(8);
        assert_eq!(c.anisotropic_filtering, 8);
        assert!(!c.gpu_requested());
        assert!(!c.gpu_active());
        c.set_anisotropic_filtering(3);
        assert_eq!(c.anisotropic_filtering, 0);
        assert_eq!(c.take_error(), GL_NO_ERROR);
    }
    use super::*;

    #[test]
    fn explicit_mip_upload_subimage_copy_and_bounds() {
        let mut c = Context::new(4, 4);
        c.bind_texture(GL_TEXTURE_2D, 1);
        c.tex_image_2d(
            GL_TEXTURE_2D,
            0,
            4,
            4,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[255, 0, 0, 255].repeat(16),
        );
        c.tex_image_2d(
            GL_TEXTURE_2D,
            1,
            2,
            2,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[0, 255, 0, 255].repeat(4),
        );
        c.tex_sub_image_2d(
            GL_TEXTURE_2D,
            1,
            1,
            1,
            1,
            1,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[0, 0, 255, 255],
        );
        assert_eq!(&c.textures[&1].mipmaps[&1].2[12..], &[0, 0, 255, 255]);
        c.target.color.fill(255);
        c.copy_tex_image_2d(GL_TEXTURE_2D, 2, GL_RGBA, 0, 0, 1, 1, 0);
        assert_eq!(c.textures[&1].mipmaps[&2].2, [255; 4]);
        assert_eq!(c.textures[&1].width, 4);
        c.tex_image_2d(GL_TEXTURE_2D, 11, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, &[0; 4]);
        assert_eq!(c.take_error(), GL_INVALID_VALUE);
        assert_eq!(c.textures[&1].max_level, 1000);
        c.tex_parameter(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL_SGIS, 0);
        assert_eq!(c.textures[&1].max_level, 0);
        let revision = c.textures[&1].revision;
        c.tex_parameter(GL_TEXTURE_2D, GL_TEXTURE_MAX_LEVEL_SGIS, u32::MAX);
        assert_eq!(c.take_error(), GL_INVALID_VALUE);
        assert_eq!(c.textures[&1].revision, revision);
        assert_eq!(c.textures[&1].max_level, 0);
        c.set_lod_bias(-0.5);
        assert_eq!(c.texture_units[0].lod_bias, -0.5);
        c.set_lod_bias(f32::NAN);
        assert_eq!(c.take_error(), GL_INVALID_VALUE);
        assert_eq!(c.texture_units[0].lod_bias, -0.5);
    }

    #[test]
    #[ignore = "requires host OpenGL; framebuffer copy synchronizes native pixels"]
    fn gpu_framebuffer_copy_reads_native_color() {
        let mut c = Context::new(4, 4);
        c.enable_gpu().unwrap();
        c.clear_color = [1., 0.5, 0., 1.];
        c.clear(GL_COLOR_BUFFER_BIT);
        c.bind_texture(GL_TEXTURE_2D, 3);
        c.copy_tex_image_2d(GL_TEXTURE_2D, 0, GL_RGBA, 1, 1, 2, 2, 0);
        assert_eq!(c.take_error(), GL_NO_ERROR);
        for p in c.textures[&3].rgba.chunks_exact(4) {
            assert_eq!(p[0], 255);
            assert!((127..=128).contains(&p[1]));
            assert_eq!(&p[2..], &[0, 255]);
        }
    }

    #[cfg(unix)]
    fn native_ctx(es: bool) -> Context {
        let mut c = ctx();
        c.gpu = Some(if es {
            crate::gpu::Renderer::new_es_for_test(64, 64).unwrap()
        } else {
            crate::gpu::Renderer::new(64, 64).unwrap()
        });
        c.gpu_requested = true;
        c
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires native GL/GLES2; Colors snapshots, indices and stream/fallback switching"]
    fn gpu_colors_stream_refreshes_mutable_guest_data_and_reuses_storage() {
        let indices = [
            0, 2, 1, 2, 0, 3, 1, 5, 4, 5, 1, 2, 5, 3, 6, 3, 5, 2, 3, 7, 6, 7, 3, 0, 6, 4, 5, 4, 6,
            7, 1, 7, 0, 7, 1, 4,
        ];
        for es in [false, true] {
            let mut c = native_ctx(es);
            let mut bytes = Vec::new();
            for i in 0..8 {
                bytes.extend(fixeds(&[
                    if i & 1 == 0 { -0.5 } else { 0.5 },
                    if i & 2 == 0 { -0.5 } else { 0.5 },
                    0.,
                ]));
            }
            for _ in 0..8 {
                bytes.extend(fixeds(&[1., 0., 0.]));
            }
            let mut mem = CountMem {
                inner: FakeMem { base: BASE, bytes },
                reads: 0,
                reject_bulk: false,
            };
            c.set_vertex_pointer(3, GL_FIXED, 0, BASE);
            c.set_color_pointer(3, GL_FIXED, 0, BASE + 96);
            c.set_client_state(GL_VERTEX_ARRAY, true);
            c.set_client_state(GL_COLOR_ARRAY, true);
            c.draw_elements(&mut mem, GL_TRIANGLES, &indices);
            c.sync_gpu();
            assert_eq!(mem.reads, 1);
            assert_eq!(c.vertex_stream.bytes.len(), 192);
            assert_eq!(c.vertex_stream.indices.len() * 2, 72);
            assert_eq!(pixel(&c, 32, 32), [255, 0, 0, 255]);
            let ptr = c.vertex_stream.bytes.as_ptr();
            let pool_ptr = c.array_read_pool[0].as_ptr();
            for i in 0..8 {
                mem.inner.bytes[96 + i * 12..108 + i * 12].copy_from_slice(&fixeds(&[0., 1., 0.]));
            }
            c.draw_elements(&mut mem, GL_TRIANGLES, &indices);
            c.sync_gpu();
            assert_eq!(mem.reads, 2);
            assert_eq!(pixel(&c, 32, 32), [0, 255, 0, 255]);
            assert_eq!(c.vertex_stream.bytes.as_ptr(), ptr);
            assert_eq!(c.array_read_pool[0].as_ptr(), pool_ptr);
            // Failed coalesced reads must retain the elementwise fallback.
            mem.reject_bulk = true;
            c.draw_elements(&mut mem, GL_TRIANGLES, &indices);
            c.sync_gpu();
            assert_eq!(mem.reads, 19);
            assert_eq!(pixel(&c, 32, 32), [0, 255, 0, 255]);
            mem.reject_bulk = false;
            // Alternate packed/flat and typed/smooth attribute layouts.
            c.set_shade_model(GL_FLAT);
            c.draw_elements(&mut mem, GL_TRIANGLES, &indices);
            c.sync_gpu();
            assert_eq!(pixel(&c, 32, 32), [0, 255, 0, 255]);
            c.set_shade_model(GL_SMOOTH);
            c.set_client_state(GL_COLOR_ARRAY, false);
            c.current_color = [0., 0., 1., 1.];
            c.draw_elements(&mut mem, GL_TRIANGLES, &indices);
            c.sync_gpu();
            assert_eq!(pixel(&c, 32, 32), [0, 0, 255, 255]);
            assert_eq!(c.take_error(), GL_NO_ERROR);
            c.disable_gpu();
            c.current_color = [1., 1., 0., 1.];
            c.draw_elements(&mut mem, GL_TRIANGLES, &indices);
            assert_eq!(pixel(&c, 32, 32), [255, 255, 0, 255]);
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires native GL/GLES2; captured VBOs, strides, indices and update semantics"]
    fn gpu_stream_preserves_vbo_capture_and_first_with_normalized_colours() {
        for es in [false, true] {
            let mut c = native_ctx(es);
            let mut positions = Vec::new();
            for pos in [[0i16, 0, 0], [-1, -1, 0], [1, -1, 0], [0, 1, 0]] {
                for n in pos {
                    positions.extend(n.to_le_bytes());
                }
                positions.extend([0x5a, 0x5a]);
            }
            let names = c.gen_buffers(3);
            c.bind_buffer(GL_ARRAY_BUFFER, names[0]);
            c.buffer_data(
                GL_ARRAY_BUFFER,
                positions.len(),
                Some(&positions),
                GL_DYNAMIC_DRAW,
            );
            c.set_vertex_pointer(3, GL_SHORT, 8, 0);
            c.set_client_state(GL_VERTEX_ARRAY, true);
            c.bind_buffer(GL_ARRAY_BUFFER, names[1]);
            c.buffer_data(
                GL_ARRAY_BUFFER,
                16,
                Some(&[255, 0, 0, 255].repeat(4)),
                GL_DYNAMIC_DRAW,
            );
            c.set_color_pointer(4, GL_UNSIGNED_BYTE, 4, 0);
            c.set_client_state(GL_COLOR_ARRAY, true);
            c.bind_buffer(GL_ELEMENT_ARRAY_BUFFER, names[2]);
            c.buffer_data(
                GL_ELEMENT_ARRAY_BUFFER,
                3,
                Some(&[1, 2, 3]),
                GL_DYNAMIC_DRAW,
            );
            let mut mem = NoMem;
            c.draw_elements_from_guest(&mut mem, GL_TRIANGLES, 3, GL_UNSIGNED_BYTE, 0);
            c.sync_gpu();
            assert_eq!(c.vertex_stream.count, 3);
            assert_eq!(c.vertex_stream.indices, [0, 1, 2]);
            assert_eq!(c.vertex_stream.attributes[1].unwrap().normalized, 1);
            assert_eq!(pixel(&c, 32, 32), [255, 0, 0, 255]);
            let index_ptr = c.decoded_indices.as_ptr();
            c.bind_buffer(GL_ARRAY_BUFFER, names[1]);
            c.buffer_sub_data(GL_ARRAY_BUFFER, 0, &[0, 255, 0, 255].repeat(4));
            // ARRAY_BUFFER at draw time must not replace the captured position binding.
            c.draw_elements_from_guest(&mut mem, GL_TRIANGLES, 3, GL_UNSIGNED_BYTE, 0);
            c.sync_gpu();
            assert_eq!(c.decoded_indices.as_ptr(), index_ptr);
            assert_eq!(pixel(&c, 32, 32), [0, 255, 0, 255]);
            c.draw_arrays(&mut mem, GL_TRIANGLES, 1, 3);
            c.sync_gpu();
            assert_eq!(pixel(&c, 32, 32), [0, 255, 0, 255]);
            c.bind_buffer(GL_ELEMENT_ARRAY_BUFFER, names[2]);
            c.buffer_data(
                GL_ELEMENT_ARRAY_BUFFER,
                6,
                Some(&[1, 0, 2, 0, 3, 0]),
                GL_DYNAMIC_DRAW,
            );
            c.draw_elements_from_guest(&mut mem, GL_TRIANGLES, 3, GL_UNSIGNED_SHORT, 0);
            c.sync_gpu();
            assert_eq!(pixel(&c, 32, 32), [0, 255, 0, 255]);
            assert_eq!(c.take_error(), GL_NO_ERROR);
        }
    }

    struct CountMem {
        inner: FakeMem,
        reads: usize,
        reject_bulk: bool,
    }
    impl GuestMemory for CountMem {
        fn read(&mut self, addr: u32, len: usize, out: &mut Vec<u8>) -> bool {
            self.reads += 1;
            if self.reject_bulk && len > 16 {
                return false;
            }
            self.inner.read(addr, len, out)
        }
    }
    #[test]
    fn colors_shared_vertices_share_a_bulk_read_and_refresh_each_draw() {
        let mut c = ctx();
        let mut bytes = Vec::new();
        for i in 0..8 {
            bytes.extend(floats(&[
                if i & 1 == 0 { -0.5 } else { 0.5 },
                if i & 2 == 0 { -0.5 } else { 0.5 },
                0.,
            ]));
        }
        for _ in 0..8 {
            bytes.extend(floats(&[1., 0., 0., 1.]));
        }
        let mut mem = CountMem {
            inner: FakeMem { base: BASE, bytes },
            reads: 0,
            reject_bulk: false,
        };
        c.set_vertex_pointer(3, GL_FLOAT, 0, BASE);
        c.set_color_pointer(4, GL_FLOAT, 0, BASE + 96);
        c.set_client_state(GL_VERTEX_ARRAY, true);
        c.set_client_state(GL_COLOR_ARRAY, true);
        let indices = [
            0, 2, 1, 2, 0, 3, 1, 5, 4, 5, 1, 2, 5, 3, 6, 3, 5, 2, 3, 7, 6, 7, 3, 0, 6, 4, 5, 4, 6,
            7, 1, 7, 0, 7, 1, 4,
        ];
        c.draw_elements(&mut mem, GL_TRIANGLES, &indices);
        assert_eq!(mem.reads, 1); // Adjacent positions/colours share one snapshot.
        for i in 0..8 {
            mem.inner.bytes[96 + i * 16..112 + i * 16].copy_from_slice(&floats(&[0., 1., 0., 1.]));
        }
        c.draw_elements(&mut mem, GL_TRIANGLES, &indices);
        assert_eq!(mem.reads, 2);
        assert_eq!(pixel(&c, 32, 32), [0, 255, 0, 255]);
        mem.reject_bulk = true;
        c.draw_elements(&mut mem, GL_TRIANGLES, &indices);
        assert_eq!(mem.reads, 19); // One failed merged span, sixteen valid elements.
        assert_eq!(c.take_error(), GL_NO_ERROR);
    }
    #[test]
    fn sparse_and_overflowing_arrays_do_not_prefetch_large_spans() {
        let mut c = ctx();
        let mut mem = CountMem {
            inner: FakeMem {
                base: BASE,
                bytes: floats(&[0., 0., 0.]),
            },
            reads: 0,
            reject_bulk: true,
        };
        c.set_vertex_pointer(3, GL_FLOAT, 0, BASE);
        c.set_client_state(GL_VERTEX_ARRAY, true);
        c.draw_elements(&mut mem, GL_TRIANGLES, &[0, 65535, 0]);
        assert_eq!(mem.reads, 2); // Only requested elements, no sparse-span read.
        c.set_vertex_pointer(3, GL_FLOAT, 0, u32::MAX - 4);
        c.draw_elements(&mut mem, GL_POINTS, &[1]);
        assert_eq!(mem.reads, 2); // Checked offset overflow never reaches memory.
        assert!(c.prepared_arrays.is_empty());
    }

    #[test]
    fn framebuffer_copy_has_gl_origin_formats_and_errors() {
        let mut c = Context::new(2, 2);
        c.target.color = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        c.bind_texture(GL_TEXTURE_2D, 7);
        c.copy_tex_image_2d(GL_TEXTURE_2D, 0, GL_RGBA, 0, 0, 2, 2, 0);
        assert_eq!(
            c.textures[&7].rgba,
            [9, 10, 11, 12, 13, 14, 15, 16, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        c.copy_tex_image_2d(GL_TEXTURE_2D, 0, GL_RGB, 1, 1, 1, 1, 0);
        assert_eq!(c.textures[&7].rgba, [5, 6, 7, 255]);
        c.copy_tex_image_2d(GL_TEXTURE_2D, 0, GL_LUMINANCE_ALPHA, 0, 0, 1, 1, 0);
        assert_eq!(c.textures[&7].rgba, [9, 9, 9, 12]);
        c.copy_tex_image_2d(GL_TEXTURE_2D, 0, GL_RGBA, 0, 0, -1, 2, 0);
        assert_eq!(c.take_error(), GL_INVALID_VALUE);
        c.copy_tex_image_2d(GL_TEXTURE_2D, 0, GL_RGBA, 0, 0, 1, 2, 1);
        assert_eq!(c.take_error(), GL_INVALID_VALUE);
        c.copy_tex_image_2d(GL_TEXTURE_2D, 0, 0, 0, 0, 1, 2, 0);
        assert_eq!(c.take_error(), GL_INVALID_ENUM);
    }

    /// Guest memory backed by a flat buffer based at `base`.
    struct FakeMem {
        base: u32,
        bytes: Vec<u8>,
    }

    impl GuestMemory for FakeMem {
        fn read(&mut self, addr: u32, len: usize, out: &mut Vec<u8>) -> bool {
            let Some(off) = addr.checked_sub(self.base) else {
                return false;
            };
            let off = off as usize;
            if off + len > self.bytes.len() {
                return false;
            }
            out.clear();
            out.extend_from_slice(&self.bytes[off..off + len]);
            true
        }
    }

    const BASE: u32 = 0x1000;
    fn lighting_sample(gpu: bool, scenario: usize) -> [u8; 4] {
        let mut c = ctx();
        c.lighting.enabled = true;
        c.lighting.lights[0].enabled = true;
        c.lighting.model_ambient = [0.; 4];
        c.lighting.diffuse = [0.8, 0.4, 0.2, 0.5];
        c.lighting.normalize = true;
        let mut bytes = floats(&[-0.9, -0.9, 0., 0.9, -0.9, 0., 0., 0.9, 0.]);
        bytes.extend([0u8, 0, 127, 0, 0, 127, 0, 0, 127]);
        match scenario {
            1 => {
                c.lighting.two_sided = true;
                c.state.front_face = raster::FrontFace::Cw;
                for i in [38, 41, 44] {
                    bytes[i] = 128;
                }
            }
            2 => {
                c.state.smooth_shading = false;
                bytes[44] = 128;
            }
            3 => {
                c.modelview.load(matrix::scale(0.75, 0.5, 2.));
                c.lighting.normalize = false;
                c.lighting.rescale = true;
            }
            4 => {
                c.lighting.lights[0].position = [0., 0., 2., 1.];
                c.lighting.lights[0].attenuation = [1., 0.25, 0.1];
            }
            5 => {
                c.lighting.lights[0].position = [0., 0., 2., 1.];
                c.lighting.lights[0].spot_cutoff = 40.;
                c.lighting.lights[0].spot_exponent = 4.;
            }
            6 => {
                c.lighting.specular = [0.3, 0.2, 0.1, 1.];
                c.lighting.shininess = 16.;
            }
            7 => {
                c.lighting.color_material = true;
                c.current_color = [0.2, 0.4, 0.8, 0.25];
            }
            _ => {}
        }
        let mut mem = FakeMem { base: BASE, bytes };
        c.set_vertex_pointer(3, GL_FLOAT, 0, BASE);
        c.set_client_state(GL_VERTEX_ARRAY, true);
        c.set_normal_pointer(GL_BYTE, 0, BASE + 36);
        c.set_client_state(GL_NORMAL_ARRAY, true);
        if gpu {
            c.enable_gpu().unwrap();
        }
        c.draw_arrays(&mut mem, GL_TRIANGLES, 0, 3);
        c.sync_gpu();
        assert_eq!(c.take_error(), GL_NO_ERROR);
        pixel(&c, 32, 32)
    }

    #[test]
    fn software_lighting_reads_normals_two_sides_flat_and_rescale() {
        for scenario in [0, 1, 3] {
            let p = lighting_sample(false, scenario);
            for (a, b) in p.into_iter().zip([204u8, 102, 51, 128]) {
                assert!(a.abs_diff(b) <= 1, "scenario {scenario}: {p:?}");
            }
        }
        assert_eq!(&lighting_sample(false, 2)[..3], &[0, 0, 0]);
        assert!(lighting_sample(false, 4)[0] < 204);
        assert!(lighting_sample(false, 5)[0] > 0);
        assert!(lighting_sample(false, 6)[0] > 204);
        assert_eq!(lighting_sample(false, 7), [51, 102, 204, 64]);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires host OpenGL; compares software and native fixed lighting"]
    fn software_and_gpu_fixed_lighting_match() {
        for scenario in 0..8 {
            let cpu = lighting_sample(false, scenario);
            let gpu = lighting_sample(true, scenario);
            for (a, b) in cpu.into_iter().zip(gpu) {
                assert!(
                    a.abs_diff(b) <= 2,
                    "scenario {scenario}: CPU {cpu:?}, GPU {gpu:?}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires host OpenGL; signed guest normal arrays drive lighting"]
    fn gpu_signed_normal_arrays_change_fixed_lighting() {
        let mut c = ctx();
        c.lighting.enabled = true;
        c.lighting.lights[0].enabled = true;
        c.lighting.model_ambient = [0.; 4];
        c.lighting.diffuse = [0.8, 0.4, 0.2, 0.5];
        let mut bytes = floats(&[-0.9, -0.9, 0., 0.9, -0.9, 0., 0., 0.9, 0.]);
        bytes.extend([0u8, 0, 127, 0, 0, 127, 0, 0, 127]);
        let mut mem = FakeMem { base: BASE, bytes };
        c.set_vertex_pointer(3, GL_FLOAT, 0, BASE);
        c.set_client_state(GL_VERTEX_ARRAY, true);
        c.set_normal_pointer(GL_BYTE, 0, BASE + 36);
        c.set_client_state(GL_NORMAL_ARRAY, true);
        c.enable_gpu().unwrap();
        c.clear(GL_COLOR_BUFFER_BIT);
        c.draw_arrays(&mut mem, GL_TRIANGLES, 0, 3);
        c.sync_gpu();
        let shown = pixel(&c, 32, 32);
        for (actual, expected) in shown.into_iter().zip([204u8, 102, 51, 128]) {
            assert!(actual.abs_diff(expected) <= 1);
        }
        for offset in [38, 41, 44] {
            mem.bytes[offset] = 128;
        }
        c.clear(GL_COLOR_BUFFER_BIT);
        c.draw_arrays(&mut mem, GL_TRIANGLES, 0, 3);
        c.sync_gpu();
        assert_eq!(&pixel(&c, 32, 32)[..3], &[0, 0, 0]);
        assert_eq!(c.take_error(), GL_NO_ERROR);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires host OpenGL; validates guest arrays through the vertex shader"]
    fn gpu_guest_arrays_keep_object_coordinates_and_match_cpu_reference() {
        let mut c = ctx();
        c.modelview.load(matrix::multiply(
            &matrix::translate(0.125, 0., -0.25),
            &matrix::rotate(17., 0., 0., 1.),
        ));
        c.projection.load(matrix::scale(0.75, 0.625, 1.));
        c.state.fog = true;
        c.state.fog_start = 0.;
        c.state.fog_end = 1.;
        c.state.fog_color = [0.1, 0.2, 0.3, 1.];
        c.current_color = [1., 0.5, 0.25, 1.];
        let coordinates = [-0.75, -0.75, 0., 0.75, -0.75, 0., 0., 0.75, 0.];
        let mut mem = FakeMem {
            base: BASE,
            bytes: floats(&coordinates),
        };
        c.set_vertex_pointer(3, GL_FLOAT, 0, BASE);
        c.set_client_state(GL_VERTEX_ARRAY, true);
        let mvp = c.mvp();
        let reference: Vec<_> = (0..3)
            .map(|i| {
                c.assemble(&mut mem, i, &mvp, &[], &matrix::IDENTITY)
                    .unwrap()
            })
            .collect();
        let mut renderer = crate::gpu::Renderer::new(c.target.width, c.target.height).unwrap();
        renderer
            .clear(
                c.target.width,
                c.target.height,
                &c.state,
                [0., 0., 0., 1.],
                1.,
                GL_COLOR_BUFFER_BIT,
            )
            .unwrap();
        renderer
            .draw(
                &c.target,
                &c.state,
                GL_TRIANGLES,
                &reference,
                &[],
                &c.textures,
                [0; 4],
            )
            .unwrap();
        let mut expected = raster::RenderTarget::new(c.target.width, c.target.height);
        renderer.read(&mut expected).unwrap();
        c.enable_gpu().unwrap();
        let raw = c
            .assemble(&mut mem, 0, &mvp, &[], &matrix::IDENTITY)
            .unwrap();
        assert_eq!(raw.pos, [-0.75, -0.75, 0., 1.]);
        assert_ne!(raw.pos, reference[0].pos);
        c.clear(GL_COLOR_BUFFER_BIT);
        c.draw_arrays(&mut mem, GL_TRIANGLES, 0, 3);
        c.sync_gpu();
        assert_eq!(c.take_error(), GL_NO_ERROR);
        let differences = expected
            .color
            .iter()
            .zip(&c.target.color)
            .filter(|(a, b)| a.abs_diff(**b) > 2)
            .count();
        assert!(
            differences <= 12,
            "guest draw differs in {differences} channels"
        );
        c.disable_gpu();
        let restored = c
            .assemble(&mut mem, 0, &mvp, &[], &matrix::IDENTITY)
            .unwrap();
        assert_eq!(restored.pos, reference[0].pos);
    }

    fn floats(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    fn fixeds(v: &[f32]) -> Vec<u8> {
        v.iter()
            .flat_map(|f| fixed::from_f32(*f).to_le_bytes())
            .collect()
    }

    fn ctx() -> Context {
        let mut c = Context::new(64, 64);
        c.state.viewport = (0, 0, 64, 64);
        c
    }

    fn pixel(c: &Context, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * c.target.width + x) * 4) as usize;
        [
            c.target.color[i],
            c.target.color[i + 1],
            c.target.color[i + 2],
            c.target.color[i + 3],
        ]
    }

    fn covered(c: &Context) -> usize {
        c.target
            .color
            .chunks_exact(4)
            .filter(|p| p[..3] != [0, 0, 0])
            .count()
    }

    /// A full-viewport quad in NDC, as two triangles worth of vertices.
    fn quad_floats() -> Vec<u8> {
        floats(&[
            -1.0, -1.0, 0.0, //
            1.0, -1.0, 0.0, //
            1.0, 1.0, 0.0, //
            -1.0, 1.0, 0.0,
        ])
    }

    #[test]
    fn draw_arrays_renders_float_positions() {
        let mut c = ctx();
        let mut m = FakeMem {
            base: BASE,
            bytes: quad_floats(),
        };
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        c.current_color = [1.0, 0.0, 0.0, 1.0];
        c.draw_arrays(&mut m, GL_TRIANGLE_FAN, 0, 4);
        assert!(covered(&c) > 3000, "covered only {}", covered(&c));
        assert_eq!(pixel(&c, 32, 32), [255, 0, 0, 255]);
    }

    #[test]
    fn fixed_point_positions_render_identically_to_float() {
        // This is the whole reason the crate carries a fixed-point
        // module: Call of Duty 2 feeds GL_FIXED vertex arrays, which
        // desktop GL 2.1 cannot consume at all.
        let mut a = ctx();
        let mut ma = FakeMem {
            base: BASE,
            bytes: quad_floats(),
        };
        a.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        a.draw_arrays(&mut ma, GL_TRIANGLE_FAN, 0, 4);

        let mut b = ctx();
        let mut mb = FakeMem {
            base: BASE,
            bytes: fixeds(&[
                -1.0, -1.0, 0.0, //
                1.0, -1.0, 0.0, //
                1.0, 1.0, 0.0, //
                -1.0, 1.0, 0.0,
            ]),
        };
        b.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FIXED,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        b.draw_arrays(&mut mb, GL_TRIANGLE_FAN, 0, 4);

        assert_eq!(a.target.color, b.target.color);
        assert!(covered(&a) > 3000);
    }

    #[test]
    fn interleaved_stride_skips_padding() {
        // Position at offset 0 of a 20-byte struct, the classic
        // interleaved layout. A stride bug renders scrambled geometry.
        let mut bytes = Vec::new();
        for pos in [
            [-1.0f32, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ] {
            bytes.extend(floats(&pos));
            bytes.extend([0u8; 8]); // padding: uv, colour, whatever
        }
        let mut c = ctx();
        let mut m = FakeMem { base: BASE, bytes };
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 20,
            pointer: BASE,
            buffer: 0,
        };
        c.draw_arrays(&mut m, GL_TRIANGLE_FAN, 0, 4);
        assert!(covered(&c) > 3000, "stride handling dropped geometry");
    }

    #[test]
    fn unmapped_array_pointer_aborts_the_draw() {
        let mut c = ctx();
        let mut m = FakeMem {
            base: BASE,
            bytes: vec![0u8; 8],
        };
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            // Far outside the fake mapping.
            pointer: 0xDEAD_0000,
            buffer: 0,
        };
        c.draw_arrays(&mut m, GL_TRIANGLE_FAN, 0, 4);
        assert_eq!(covered(&c), 0);
    }

    #[test]
    fn disabled_vertex_array_draws_nothing() {
        let mut c = ctx();
        let mut m = FakeMem {
            base: BASE,
            bytes: quad_floats(),
        };
        c.vertex_array = ArrayPointer {
            enabled: false,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        c.draw_arrays(&mut m, GL_TRIANGLE_FAN, 0, 4);
        assert_eq!(covered(&c), 0);
    }

    #[test]
    fn draw_elements_reads_ushort_indices_from_guest_memory() {
        let mut bytes = quad_floats();
        let index_off = bytes.len() as u32;
        for i in [0u16, 1, 2, 0, 2, 3] {
            bytes.extend(i.to_le_bytes());
        }
        let mut c = ctx();
        let mut m = FakeMem { base: BASE, bytes };
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        c.draw_elements_from_guest(&mut m, GL_TRIANGLES, 6, GL_UNSIGNED_SHORT, BASE + index_off);
        assert!(covered(&c) > 3000, "indexed quad covered {}", covered(&c));
    }

    #[test]
    fn draw_elements_reads_ubyte_indices() {
        let mut bytes = quad_floats();
        let index_off = bytes.len() as u32;
        bytes.extend([0u8, 1, 2, 0, 2, 3]);
        let mut c = ctx();
        let mut m = FakeMem { base: BASE, bytes };
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        c.draw_elements_from_guest(&mut m, GL_TRIANGLES, 6, GL_UNSIGNED_BYTE, BASE + index_off);
        assert!(covered(&c) > 3000);
    }

    #[test]
    fn color_array_bytes_are_normalized() {
        // A UNSIGNED_BYTE colour array holds 0..255 meaning 0.0..1.0.
        // Treating it as a raw value would saturate every vertex white.
        let mut bytes = quad_floats();
        let color_off = bytes.len() as u32;
        for _ in 0..4 {
            bytes.extend([0u8, 0, 255, 255]); // pure blue
        }
        let mut c = ctx();
        let mut m = FakeMem { base: BASE, bytes };
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        c.color_array = ArrayPointer {
            enabled: true,
            size: 4,
            ty: GL_UNSIGNED_BYTE,
            stride: 0,
            pointer: BASE + color_off,
            buffer: 0,
        };
        c.draw_arrays(&mut m, GL_TRIANGLE_FAN, 0, 4);
        assert_eq!(pixel(&c, 32, 32), [0, 0, 255, 255]);
    }

    fn six_texture_cycle(c: &mut Context) {
        let mut bytes = quad_floats();
        let uv_off = bytes.len() as u32;
        bytes.extend(floats(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]));

        let mut m = FakeMem { base: BASE, bytes };
        for name in 0..6 {
            c.bind_texture(GL_TEXTURE_2D, name);
            c.tex_parameter(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            c.compressed_tex_image_2d(
                GL_TEXTURE_2D,
                0,
                4,
                4,
                GL_COMPRESSED_RGB_S3TC_DXT1_EXT,
                &[0xe0, 0x07, 0, 0, 0, 0, 0, 0],
            );
            assert_eq!(c.take_error(), GL_NO_ERROR);
        }
        c.set_capability(GL_TEXTURE_2D, true);
        c.texture_units[0].tex_env = TexEnvMode::Replace;
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        c.texture_units[0].texcoord_array = ArrayPointer {
            enabled: true,
            size: 2,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE + uv_off,
            buffer: 0,
        };
        for name in (0..6).cycle().take(18) {
            c.bind_texture(GL_TEXTURE_2D, name);
            c.delete_textures(&[0]);
            c.draw_arrays(&mut m, GL_TRIANGLE_FAN, 0, 4);
            c.sync_gpu();
            assert_eq!(pixel(&c, 32, 32), [0, 255, 0, 255], "name={name}");
            assert_eq!(c.take_error(), GL_NO_ERROR);
        }
    }

    #[test]
    fn default_texture_compressed_cycle_software() {
        six_texture_cycle(&mut ctx());
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires a native GL driver"]
    fn default_texture_compressed_cycle_native() {
        for es in [false, true] {
            six_texture_cycle(&mut native_ctx(es));
        }
    }

    #[test]
    fn texture_coords_sample_the_bound_texture() {
        let mut bytes = quad_floats();
        let uv_off = bytes.len() as u32;
        bytes.extend(floats(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]));

        let mut c = ctx();
        let mut m = FakeMem { base: BASE, bytes };
        let names = c.gen_textures(1);
        c.bind_texture(GL_TEXTURE_2D, names[0]);
        // 1x1 green texture.
        c.tex_image_2d(
            GL_TEXTURE_2D,
            0,
            1,
            1,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[0, 255, 0, 255],
        );
        c.set_capability(GL_TEXTURE_2D, true);
        c.texture_units[0].tex_env = TexEnvMode::Replace;
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        c.texture_units[0].texcoord_array = ArrayPointer {
            enabled: true,
            size: 2,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE + uv_off,
            buffer: 0,
        };
        c.draw_arrays(&mut m, GL_TRIANGLE_FAN, 0, 4);
        assert_eq!(pixel(&c, 32, 32), [0, 255, 0, 255]);
    }

    #[test]
    fn texture_bound_to_unit_one_is_sampled_when_unit_zero_is_disabled() {
        // X-Forge (Sticky Balls, Ball Busters) sets every textured draw
        // up as: activeTexture(0) / disable(TEXTURE_2D) / bindTexture(0),
        // then activeTexture(1) / enable(TEXTURE_2D) / bindTexture(tex).
        // Sampling stage 0 — or gating texturing on stage 0's enable —
        // leaves each fragment with its raw vertex colour, which is how
        // the game's white-on-alpha UI atlases came out as solid white
        // rectangles.
        let mut bytes = quad_floats();
        let uv_off = bytes.len() as u32;
        bytes.extend(floats(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]));

        let mut c = ctx();
        let mut m = FakeMem { base: BASE, bytes };
        let names = c.gen_textures(1);

        // A red vertex colour so that sampling the wrong stage, or
        // taking stage 0's combiner mode, is visible in the result:
        // stage 1 replaces, stage 0 would modulate red by blue to black.
        c.current_color = [1.0, 0.0, 0.0, 1.0];

        // Stage 0: explicitly off, with nothing bound.
        c.set_active_texture(0);
        c.set_capability(GL_TEXTURE_2D, false);
        c.bind_texture(GL_TEXTURE_2D, 0);

        // Stage 1: the only stage carrying a texture.
        c.set_active_texture(1);
        c.bind_texture(GL_TEXTURE_2D, names[0]);
        c.tex_image_2d(
            GL_TEXTURE_2D,
            0,
            1,
            1,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[0, 0, 255, 255],
        );
        c.set_capability(GL_TEXTURE_2D, true);
        c.tex_env(GL_TEXTURE_ENV_MODE, GL_REPLACE);

        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        c.set_client_active_texture(1);
        c.set_client_state(GL_TEXTURE_COORD_ARRAY, true);
        c.set_texcoord_pointer(2, GL_FLOAT, 0, BASE + uv_off);

        c.draw_arrays(&mut m, GL_TRIANGLE_FAN, 0, 4);
        assert_eq!(pixel(&c, 32, 32), [0, 0, 255, 255]);
    }

    #[test]
    fn resetting_a_stage_past_the_advertised_maximum_keeps_the_live_stage_bound() {
        // Sticky Balls walks one stage further than GL_MAX_TEXTURE_UNITS
        // says exists: it binds its texture to stage 1 and then clears
        // stage 2 before the draw. glActiveTexture past the last stage we
        // track is an error that leaves the selected unit alone, so that
        // clear used to land back on stage 1 and unbind the texture that
        // had just been bound there — which is how the game's sky came
        // out as an opaque white sheet.
        let mut c = ctx();
        let names = c.gen_textures(1);

        c.set_active_texture(1);
        c.set_capability(GL_TEXTURE_2D, true);
        c.bind_texture(GL_TEXTURE_2D, names[0]);

        c.set_active_texture(2);
        c.set_capability(GL_TEXTURE_2D, false);
        c.bind_texture(GL_TEXTURE_2D, 0);

        assert!(c.texture_units[1].texture_enabled, "stage 1 was disabled");
        assert_eq!(c.texture_units[1].bound_texture, names[0]);
    }

    #[test]
    fn a_second_stage_alpha_mask_cuts_out_a_colour_map_that_has_no_alpha() {
        // How Sticky Balls draws a ball: stage 1 carries the colour as
        // DXT1, a format with no alpha channel at all, and stage 2 a
        // white-RGB texture whose shape lives in its alpha. Modulating
        // across both cuts the ball out of its square. Sampling only one
        // stage loses either the colour or the cut-out — with the mask
        // alone the balls came out white, with the colour map alone an
        // opaque square stayed around each one.
        let mut bytes = quad_floats();
        let uv_off = bytes.len() as u32;
        bytes.extend(floats(&[0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0]));

        let mut c = ctx();
        let mut m = FakeMem { base: BASE, bytes };
        let names = c.gen_textures(2);

        c.set_capability(GL_BLEND, true);
        c.set_blend_func(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);

        // Stage 1: the colour map, opaque green all over.
        c.set_active_texture(1);
        c.bind_texture(GL_TEXTURE_2D, names[0]);
        c.tex_image_2d(
            GL_TEXTURE_2D,
            0,
            1,
            1,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[0, 255, 0, 255],
        );
        c.set_capability(GL_TEXTURE_2D, true);
        c.tex_env(GL_TEXTURE_ENV_MODE, GL_MODULATE);

        // Stage 2: the mask, white RGB throughout with the left half
        // opaque and the right half clear.
        c.set_active_texture(2);
        c.bind_texture(GL_TEXTURE_2D, names[1]);
        c.tex_image_2d(
            GL_TEXTURE_2D,
            0,
            2,
            1,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[255, 255, 255, 255, 255, 255, 255, 0],
        );
        c.tex_parameter(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        c.set_capability(GL_TEXTURE_2D, true);
        c.tex_env(GL_TEXTURE_ENV_MODE, GL_MODULATE);

        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        // Both stages read the same coordinates here; the game gives
        // each its own array, which is why they are per-unit state.
        for unit in [1, 2] {
            c.set_client_active_texture(unit);
            c.set_client_state(GL_TEXTURE_COORD_ARRAY, true);
            c.set_texcoord_pointer(2, GL_FLOAT, 0, BASE + uv_off);
        }

        c.draw_arrays(&mut m, GL_TRIANGLE_FAN, 0, 4);

        // Where the mask is opaque the colour map's green survives;
        // where it is clear the background shows through untouched.
        assert_eq!(pixel(&c, 8, 32), [0, 255, 0, 255]);
        assert_eq!(pixel(&c, 56, 32), [0, 0, 0, 0]);
    }

    #[test]
    fn projection_and_modelview_both_apply() {
        // Halving via the modelview and halving again via the
        // projection must compound: coverage should drop to ~1/16.
        let mut full = ctx();
        let mut m1 = FakeMem {
            base: BASE,
            bytes: quad_floats(),
        };
        let ptr = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        full.vertex_array = ptr;
        full.draw_arrays(&mut m1, GL_TRIANGLE_FAN, 0, 4);
        let full_area = covered(&full);

        let mut half = ctx();
        let mut m2 = FakeMem {
            base: BASE,
            bytes: quad_floats(),
        };
        half.vertex_array = ptr;
        half.set_matrix_mode(GL_MODELVIEW);
        half.scale(0.5, 0.5, 1.0);
        half.set_matrix_mode(GL_PROJECTION);
        half.scale(0.5, 0.5, 1.0);
        half.draw_arrays(&mut m2, GL_TRIANGLE_FAN, 0, 4);
        let quarter_area = covered(&half);

        let ratio = full_area as f32 / quarter_area.max(1) as f32;
        assert!(
            (12.0..20.0).contains(&ratio),
            "expected ~16x area ratio, got {ratio}"
        );
    }

    #[test]
    fn matrix_stack_push_pop_isolates_transforms() {
        let mut c = ctx();
        c.set_matrix_mode(GL_MODELVIEW);
        c.load_identity();
        c.push_matrix();
        c.scale(0.25, 0.25, 1.0);
        c.pop_matrix();
        assert_eq!(*c.modelview.current(), matrix::IDENTITY);
        assert_eq!(c.take_error(), GL_NO_ERROR);
    }

    #[test]
    fn stack_underflow_is_reported() {
        let mut c = ctx();
        c.set_matrix_mode(GL_MODELVIEW);
        c.pop_matrix();
        assert_eq!(c.take_error(), GL_STACK_UNDERFLOW);
    }

    #[test]
    fn first_error_survives_until_read() {
        let mut c = ctx();
        c.set_matrix_mode(0x9999); // GL_INVALID_ENUM
        c.set_viewport(0, 0, -1, -1); // GL_INVALID_VALUE
        assert_eq!(c.take_error(), GL_INVALID_ENUM);
        assert_eq!(c.take_error(), GL_NO_ERROR);
    }

    #[test]
    fn clear_uses_the_clear_color() {
        let mut c = ctx();
        c.clear_color = [0.0, 0.0, 1.0, 1.0];
        c.clear(GL_COLOR_BUFFER_BIT);
        assert_eq!(pixel(&c, 0, 0), [0, 0, 255, 255]);
    }

    #[test]
    fn gen_textures_returns_distinct_names() {
        let mut c = ctx();
        let a = c.gen_textures(3);
        let b = c.gen_textures(2);
        assert_eq!(a.len(), 3);
        assert!(a.iter().all(|n| *n != 0), "0 is the reserved default name");
        assert!(b.iter().all(|n| !a.contains(n)), "names were reused");
    }

    #[test]
    fn deleting_the_bound_texture_unbinds_it() {
        let mut c = ctx();
        let n = c.gen_textures(1);
        c.bind_texture(GL_TEXTURE_2D, n[0]);
        c.delete_textures(&n);
        assert_eq!(c.bound_texture, 0);
        assert!(!c.textures.contains_key(&n[0]));
    }

    #[test]
    fn default_texture_accepts_uploads_and_is_not_deleted() {
        let mut c = ctx();
        c.bind_texture(GL_TEXTURE_2D, 0);
        c.tex_image_2d(
            GL_TEXTURE_2D,
            0,
            1,
            1,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[1, 2, 3, 4],
        );
        assert_eq!(c.take_error(), GL_NO_ERROR);
        assert_eq!(c.textures[&0].rgba, [1, 2, 3, 4]);
        c.set_capability(GL_TEXTURE_2D, true);
        assert_eq!(c.texture_stages().1, 1);
        c.delete_textures(&[0]);
        assert_eq!(c.textures[&0].rgba, [1, 2, 3, 4]);
        c.bind_texture(GL_TEXTURE_2D, 1);
        c.delete_textures(&[1]);
        assert_eq!(c.bound_texture, 0);
        assert_eq!(c.texture_stages().1, 1);
    }

    #[test]
    fn tex_sub_image_patches_a_rectangle() {
        let mut c = ctx();
        let n = c.gen_textures(1);
        c.bind_texture(GL_TEXTURE_2D, n[0]);
        c.tex_image_2d(
            GL_TEXTURE_2D,
            0,
            2,
            2,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[0u8; 16],
        );
        c.tex_sub_image_2d(
            GL_TEXTURE_2D,
            0,
            1,
            1,
            1,
            1,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[9, 9, 9, 9],
        );
        let t = &c.textures[&n[0]];
        assert_eq!(&t.rgba[12..16], &[9, 9, 9, 9]);
        assert_eq!(&t.rgba[0..4], &[0, 0, 0, 0]);
        assert_eq!(c.take_error(), GL_NO_ERROR);
    }

    #[test]
    fn tex_sub_image_out_of_bounds_is_rejected() {
        let mut c = ctx();
        let n = c.gen_textures(1);
        c.bind_texture(GL_TEXTURE_2D, n[0]);
        c.tex_image_2d(
            GL_TEXTURE_2D,
            0,
            2,
            2,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[0u8; 16],
        );
        c.tex_sub_image_2d(
            GL_TEXTURE_2D,
            0,
            2,
            2,
            2,
            2,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            &[0u8; 16],
        );
        assert_eq!(c.take_error(), GL_INVALID_VALUE);
    }

    #[test]
    fn triangle_strip_winding_stays_consistent_under_culling() {
        // Every second triangle of a strip has reversed winding. If the
        // assembler doesn't compensate, back-face culling erases half
        // the mesh — the classic "striped" rendering bug.
        //
        // Vertices run top, bottom, top, bottom left-to-right, which
        // makes the first triangle counter-clockwise in GL's frame and
        // therefore front-facing.
        let strip = || {
            let mut b = Vec::new();
            for p in [
                [-1.0f32, 1.0, 0.0],
                [-1.0, -1.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, -1.0, 0.0],
                [1.0, 1.0, 0.0],
                [1.0, -1.0, 0.0],
            ] {
                b.extend(floats(&p));
            }
            b
        };
        let ptr = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };

        let mut plain = ctx();
        let mut m1 = FakeMem {
            base: BASE,
            bytes: strip(),
        };
        plain.vertex_array = ptr;
        plain.draw_arrays(&mut m1, GL_TRIANGLE_STRIP, 0, 6);
        let no_cull = covered(&plain);

        let mut culled = ctx();
        let mut m2 = FakeMem {
            base: BASE,
            bytes: strip(),
        };
        culled.vertex_array = ptr;
        culled.set_cull_face(GL_BACK);
        culled.set_capability(GL_CULL_FACE, true);
        culled.draw_arrays(&mut m2, GL_TRIANGLE_STRIP, 0, 6);
        let with_cull = covered(&culled);

        assert!(no_cull > 3000, "unculled strip covered {no_cull}");
        assert_eq!(
            with_cull, no_cull,
            "culling removed part of a consistently-wound strip"
        );

        // And the converse: flipping the front face must erase the
        // whole strip, proving the strip really was uniformly wound
        // rather than the cull test being a no-op.
        let mut flipped = ctx();
        let mut m3 = FakeMem {
            base: BASE,
            bytes: strip(),
        };
        flipped.vertex_array = ptr;
        flipped.set_front_face(GL_CW);
        flipped.set_cull_face(GL_BACK);
        flipped.set_capability(GL_CULL_FACE, true);
        flipped.draw_arrays(&mut m3, GL_TRIANGLE_STRIP, 0, 6);
        assert_eq!(covered(&flipped), 0, "strip was not uniformly wound");
    }

    #[test]
    fn streamed_strip_and_fan_match_explicit_triangles() {
        for (mode, indices) in [
            (GL_TRIANGLE_STRIP, vec![0, 1, 2, 2, 1, 3]),
            (GL_TRIANGLE_FAN, vec![0, 1, 2, 0, 2, 3]),
        ] {
            let bytes: Vec<u8> = [
                [-0.8f32, -0.8, 0.0],
                [0.8, -0.8, 0.0],
                [-0.8, 0.8, 0.0],
                [0.8, 0.8, 0.0],
            ]
            .iter()
            .flat_map(|p| floats(p))
            .collect();
            let mut optimized = ctx();
            let mut explicit = ctx();
            let pointer = ArrayPointer {
                enabled: true,
                size: 3,
                ty: GL_FLOAT,
                stride: 0,
                pointer: BASE,
                buffer: 0,
            };
            optimized.vertex_array = pointer;
            explicit.vertex_array = pointer;
            // Blending makes primitive order and overlapping coverage observable.
            optimized.set_capability(GL_BLEND, true);
            explicit.set_capability(GL_BLEND, true);
            optimized.draw_arrays(
                &mut FakeMem {
                    base: BASE,
                    bytes: bytes.clone(),
                },
                mode,
                0,
                4,
            );
            explicit.draw_elements(&mut FakeMem { base: BASE, bytes }, GL_TRIANGLES, &indices);
            assert_eq!(optimized.target.color, explicit.target.color);
            assert_eq!(optimized.target.depth, explicit.target.depth);
        }
    }

    #[test]
    fn line_strip_loop_guest_arrays_match_explicit_indexed_segments() {
        for mode in [GL_LINE_STRIP, GL_LINE_LOOP] {
            let mut strip = ctx();
            let mut explicit = ctx();
            for c in [&mut strip, &mut explicit] {
                c.set_vertex_pointer(3, GL_FLOAT, 0, BASE);
                c.set_client_state(GL_VERTEX_ARRAY, true);
                c.current_color = [0.8, 0.4, 0.2, 0.5];
                c.state.blend = true;
                c.state.blend_src = BlendFactor::SrcAlpha;
                c.state.blend_dst = BlendFactor::OneMinusSrcAlpha;
            }
            let bytes = floats(&[-0.65, 0.4, 0., 0.3, 0.4, 0., 0.3, -0.6, 0., -0.65, -0.6, 0.]);
            strip.draw_arrays(
                &mut FakeMem {
                    base: BASE,
                    bytes: bytes.clone(),
                },
                mode,
                0,
                4,
            );
            let indices = if mode == GL_LINE_LOOP {
                vec![0, 1, 1, 2, 2, 3, 3, 0]
            } else {
                vec![0, 1, 1, 2, 2, 3]
            };
            explicit.draw_elements(&mut FakeMem { base: BASE, bytes }, GL_LINES, &indices);
            assert!(covered(&strip) > 50);
            assert_eq!(strip.target.color, explicit.target.color);
            assert_eq!(strip.take_error(), GL_NO_ERROR);
        }
    }

    #[test]
    fn points_guest_arrays_render_and_size_errors_preserve_previous_values() {
        let mut c = ctx();
        c.set_vertex_pointer(3, GL_FLOAT, 0, BASE);
        c.set_client_state(GL_VERTEX_ARRAY, true);
        c.set_point_size(3.);
        c.set_line_width(2.);
        c.draw_arrays(
            &mut FakeMem {
                base: BASE,
                bytes: floats(&[-0.5, 0.25, 0., 0.25, -0.5, 0.]),
            },
            GL_POINTS,
            0,
            2,
        );
        assert_eq!(covered(&c), 18);
        c.set_point_size(0.);
        assert_eq!(c.take_error(), GL_INVALID_VALUE);
        assert_eq!(c.state.point_size, 3.);
        c.set_line_width(f32::INFINITY);
        assert_eq!(c.take_error(), GL_INVALID_VALUE);
        assert_eq!(c.state.line_width, 2.);
    }

    #[test]
    fn points_and_lines_render_without_error() {
        let mut c = ctx();
        let mut m = FakeMem {
            base: BASE,
            bytes: quad_floats(),
        };
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        c.draw_arrays(&mut m, GL_LINES, 0, 4);
        c.draw_arrays(&mut m, GL_POINTS, 0, 4);
        assert!(covered(&c) > 0);
        assert_eq!(c.take_error(), GL_NO_ERROR);
    }

    #[test]
    fn unknown_primitive_mode_is_an_error() {
        let mut c = ctx();
        let mut m = FakeMem {
            base: BASE,
            bytes: quad_floats(),
        };
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: BASE,
            buffer: 0,
        };
        c.draw_arrays(&mut m, 0x99, 0, 4);
        assert_eq!(c.take_error(), GL_INVALID_ENUM);
    }

    #[test]
    fn lighting_capabilities_are_retained_without_rejecting_existing_games() {
        // COD2 enables lighting; erroring here would make the game
        // think the driver is broken.
        let mut c = ctx();
        c.set_capability(GL_LIGHTING, true);
        c.set_capability(0x4000, true); // GL_LIGHT0
        assert!(c.lighting.enabled && c.lighting.lights[0].enabled);
        assert_eq!(c.take_error(), GL_NO_ERROR);
    }

    #[test]
    fn cull_face_before_enable_is_not_lost() {
        // Engines commonly call glCullFace once at init and toggle
        // GL_CULL_FACE per material. If the mode were only stored while
        // culling happened to be enabled, the toggle would silently
        // revert to GL_BACK.
        let mut c = ctx();
        c.set_cull_face(GL_FRONT);
        assert!(c.state.cull.is_none(), "mode set should not enable culling");
        c.set_capability(GL_CULL_FACE, true);
        assert_eq!(c.state.cull, Some(CullMode::Front));
        c.set_capability(GL_CULL_FACE, false);
        assert!(c.state.cull.is_none());
        c.set_capability(GL_CULL_FACE, true);
        assert_eq!(c.state.cull, Some(CullMode::Front), "mode was forgotten");
    }

    #[test]
    fn degenerate_frustum_is_rejected_without_nans() {
        let mut c = ctx();
        c.set_matrix_mode(GL_PROJECTION);
        c.frustum(-1.0, 1.0, -1.0, 1.0, 0.0, 100.0);
        assert_eq!(c.take_error(), GL_INVALID_VALUE);
        assert!(c.projection.current().iter().all(|v| v.is_finite()));
    }

    // ---- buffer objects -------------------------------------------------

    /// Guest memory that refuses every read.
    ///
    /// A VBO-backed draw must not touch guest memory at all, so any
    /// read is a bug the test should fail on rather than tolerate.
    struct NoMem;

    impl GuestMemory for NoMem {
        fn read(&mut self, _addr: u32, _len: usize, _out: &mut Vec<u8>) -> bool {
            panic!("VBO draw read guest memory");
        }
    }

    #[test]
    fn draw_arrays_reads_from_array_buffer() {
        let mut c = ctx();
        let names = c.gen_buffers(1);
        c.bind_buffer(GL_ARRAY_BUFFER, names[0]);
        let quad = quad_floats();
        c.buffer_data(GL_ARRAY_BUFFER, quad.len(), Some(&quad), GL_STATIC_DRAW);
        // Offset 0 into the buffer, which is also the null guest pointer:
        // the draw must still happen.
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: 0,
            buffer: names[0],
        };
        c.draw_arrays(&mut NoMem, GL_TRIANGLE_FAN, 0, 4);
        assert_eq!(c.take_error(), GL_NO_ERROR);
        assert!(covered(&c) > 3000, "VBO quad covered {}", covered(&c));
    }

    #[test]
    fn draw_elements_reads_indices_from_element_buffer() {
        let mut c = ctx();
        let names = c.gen_buffers(2);
        c.bind_buffer(GL_ARRAY_BUFFER, names[0]);
        let quad = quad_floats();
        c.buffer_data(GL_ARRAY_BUFFER, quad.len(), Some(&quad), GL_STATIC_DRAW);
        c.vertex_array = ArrayPointer {
            enabled: true,
            size: 3,
            ty: GL_FLOAT,
            stride: 0,
            pointer: 0,
            buffer: names[0],
        };
        c.bind_buffer(GL_ELEMENT_ARRAY_BUFFER, names[1]);
        let indices: Vec<u8> = [0u16, 1, 2, 0, 2, 3]
            .iter()
            .flat_map(|i| i.to_le_bytes())
            .collect();
        c.buffer_data(
            GL_ELEMENT_ARRAY_BUFFER,
            indices.len(),
            Some(&indices),
            GL_STATIC_DRAW,
        );
        c.draw_elements_from_guest(&mut NoMem, GL_TRIANGLES, 6, GL_UNSIGNED_SHORT, 0);
        assert_eq!(c.take_error(), GL_NO_ERROR);
        assert!(
            covered(&c) > 3000,
            "indexed VBO quad covered {}",
            covered(&c)
        );
    }

    #[test]
    fn pointer_captures_binding_at_call_time_not_draw_time() {
        // GL ES 1.1 latches GL_ARRAY_BUFFER when gl*Pointer runs. A game
        // that binds a VBO, sets the vertex pointer, then binds a
        // different VBO for another array must still draw from the first.
        let mut c = ctx();
        let names = c.gen_buffers(2);
        c.bind_buffer(GL_ARRAY_BUFFER, names[0]);
        let quad = quad_floats();
        c.buffer_data(GL_ARRAY_BUFFER, quad.len(), Some(&quad), GL_STATIC_DRAW);
        c.set_vertex_pointer(3, GL_FLOAT, 0, 0);
        assert_eq!(c.vertex_array.buffer, names[0]);

        c.bind_buffer(GL_ARRAY_BUFFER, names[1]);
        c.buffer_data(GL_ARRAY_BUFFER, 16, None, GL_DYNAMIC_DRAW);
        assert_eq!(
            c.vertex_array.buffer, names[0],
            "rebinding clobbered an already-set pointer"
        );

        c.vertex_array.enabled = true;
        c.draw_arrays(&mut NoMem, GL_TRIANGLE_FAN, 0, 4);
        assert!(covered(&c) > 3000, "covered {}", covered(&c));
    }

    #[test]
    fn buffer_sub_data_rejects_out_of_range_writes() {
        let mut c = ctx();
        let names = c.gen_buffers(1);
        c.bind_buffer(GL_ARRAY_BUFFER, names[0]);
        c.buffer_data(GL_ARRAY_BUFFER, 8, None, GL_STATIC_DRAW);
        c.buffer_sub_data(GL_ARRAY_BUFFER, 4, &[1, 2, 3, 4]);
        assert_eq!(c.take_error(), GL_NO_ERROR);
        assert_eq!(c.buffers[&names[0]].data, [0, 0, 0, 0, 1, 2, 3, 4]);

        // One byte past the end must write nothing at all, not clip.
        c.buffer_sub_data(GL_ARRAY_BUFFER, 5, &[9, 9, 9, 9]);
        assert_eq!(c.take_error(), GL_INVALID_VALUE);
        assert_eq!(c.buffers[&names[0]].data, [0, 0, 0, 0, 1, 2, 3, 4]);
    }

    #[test]
    fn upload_without_a_bound_buffer_is_an_error() {
        let mut c = ctx();
        c.buffer_data(GL_ARRAY_BUFFER, 16, None, GL_STATIC_DRAW);
        assert_eq!(c.take_error(), GL_INVALID_OPERATION);
        c.buffer_sub_data(GL_ELEMENT_ARRAY_BUFFER, 0, &[1, 2]);
        assert_eq!(c.take_error(), GL_INVALID_OPERATION);
    }

    #[test]
    fn deleting_a_bound_buffer_clears_the_binding() {
        let mut c = ctx();
        let names = c.gen_buffers(2);
        c.bind_buffer(GL_ARRAY_BUFFER, names[0]);
        c.bind_buffer(GL_ELEMENT_ARRAY_BUFFER, names[1]);
        c.delete_buffers(&names);
        assert_eq!(c.array_buffer, 0);
        assert_eq!(c.element_array_buffer, 0);
        assert!(c.buffers.is_empty());
    }

    #[test]
    fn draw_from_a_short_buffer_renders_nothing() {
        // An out-of-range VBO read must drop the draw. Padding it with
        // zeroes would render a degenerate triangle at the origin and
        // hide the guest's mistake.
        let mut c = ctx();
        let names = c.gen_buffers(1);
        c.bind_buffer(GL_ARRAY_BUFFER, names[0]);
        let quad = quad_floats();
        c.buffer_data(GL_ARRAY_BUFFER, quad.len() - 4, Some(&quad), GL_STATIC_DRAW);
        c.set_vertex_pointer(3, GL_FLOAT, 0, 0);
        c.vertex_array.enabled = true;
        c.draw_arrays(&mut NoMem, GL_TRIANGLE_FAN, 0, 4);
        assert_eq!(covered(&c), 0, "short buffer still rendered");
    }
}
