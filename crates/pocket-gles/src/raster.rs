//! Software triangle rasterizer.
//!
//! This is the backend that turns transformed vertices into pixels. It
//! is deliberately self-contained: no host GPU, no guest memory, no
//! kernel types. That keeps it unit-testable — every behaviour below is
//! pinned by a test that renders into a small buffer and inspects the
//! result — and it means the same code path runs on a PC and on a
//! device where no OpenGL ES driver is available.
//!
//! Conventions, all of which follow the OpenGL ES 1.1 specification:
//!
//! * The colour target is RGBA8888, row-major, top row first. GL's
//!   window origin is bottom-left, so the viewport transform flips Y
//!   when computing the target row.
//! * Depth is stored as a `f32` in `[0, 1]` after the `glDepthRange`
//!   mapping, with smaller meaning nearer.
//! * Vertices arrive in clip space; this module does the perspective
//!   divide, the viewport transform, and near/far clipping.

/// How many texture stages a draw can cascade through.
pub const MAX_TEXTURE_STAGES: usize = 4;

/// A vertex after the modelview-projection transform, in clip space.
#[derive(Debug, Clone, Copy)]
pub struct Vertex {
    /// Clip-space position, `w` not yet divided out.
    pub pos: [f32; 4],
    /// Straight (non-premultiplied) RGBA in `[0, 1]`.
    pub color: [f32; 4],
    /// Texture coordinates, one set per texture unit.
    pub texcoord: [[f32; 2]; MAX_TEXTURE_STAGES],
    /// Eye-space Z used for fog. OpenGL defines fog distance from the
    /// eye-space depth, not clip-space or NDC Z.
    pub fog_depth: f32,
}

impl Default for Vertex {
    fn default() -> Self {
        Self {
            pos: [0.0, 0.0, 0.0, 1.0],
            color: [1.0, 1.0, 1.0, 1.0],
            texcoord: [[0.0, 0.0]; MAX_TEXTURE_STAGES],
            fog_depth: 0.0,
        }
    }
}

/// Comparison function for the depth and alpha tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareFunc {
    Never,
    Less,
    Equal,
    Lequal,
    Greater,
    Notequal,
    Gequal,
    Always,
}

impl CompareFunc {
    pub fn from_enum(value: u32) -> Option<Self> {
        use crate::consts::*;
        Some(match value {
            GL_NEVER => Self::Never,
            GL_LESS => Self::Less,
            GL_EQUAL => Self::Equal,
            GL_LEQUAL => Self::Lequal,
            GL_GREATER => Self::Greater,
            GL_NOTEQUAL => Self::Notequal,
            GL_GEQUAL => Self::Gequal,
            GL_ALWAYS => Self::Always,
            _ => return None,
        })
    }

    /// Evaluate `lhs <op> rhs`.
    #[inline]
    pub fn test(self, lhs: f32, rhs: f32) -> bool {
        match self {
            Self::Never => false,
            Self::Less => lhs < rhs,
            Self::Equal => lhs == rhs,
            Self::Lequal => lhs <= rhs,
            Self::Greater => lhs > rhs,
            Self::Notequal => lhs != rhs,
            Self::Gequal => lhs >= rhs,
            Self::Always => true,
        }
    }
}

/// Source or destination blend factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendFactor {
    Zero,
    One,
    SrcColor,
    OneMinusSrcColor,
    SrcAlpha,
    OneMinusSrcAlpha,
    DstAlpha,
    OneMinusDstAlpha,
    DstColor,
    OneMinusDstColor,
    SrcAlphaSaturate,
}

impl BlendFactor {
    pub fn from_enum(value: u32) -> Option<Self> {
        use crate::consts::*;
        Some(match value {
            GL_ZERO => Self::Zero,
            GL_ONE => Self::One,
            GL_SRC_COLOR => Self::SrcColor,
            GL_ONE_MINUS_SRC_COLOR => Self::OneMinusSrcColor,
            GL_SRC_ALPHA => Self::SrcAlpha,
            GL_ONE_MINUS_SRC_ALPHA => Self::OneMinusSrcAlpha,
            GL_DST_ALPHA => Self::DstAlpha,
            GL_ONE_MINUS_DST_ALPHA => Self::OneMinusDstAlpha,
            GL_DST_COLOR => Self::DstColor,
            GL_ONE_MINUS_DST_COLOR => Self::OneMinusDstColor,
            GL_SRC_ALPHA_SATURATE => Self::SrcAlphaSaturate,
            _ => return None,
        })
    }

    /// Resolve the factor to a per-channel multiplier.
    #[inline]
    fn resolve(self, src: [f32; 4], dst: [f32; 4]) -> [f32; 4] {
        let splat = |v: f32| [v, v, v, v];
        match self {
            Self::Zero => splat(0.0),
            Self::One => splat(1.0),
            Self::SrcColor => src,
            Self::OneMinusSrcColor => [1.0 - src[0], 1.0 - src[1], 1.0 - src[2], 1.0 - src[3]],
            Self::SrcAlpha => splat(src[3]),
            Self::OneMinusSrcAlpha => splat(1.0 - src[3]),
            Self::DstAlpha => splat(dst[3]),
            Self::OneMinusDstAlpha => splat(1.0 - dst[3]),
            Self::DstColor => dst,
            Self::OneMinusDstColor => [1.0 - dst[0], 1.0 - dst[1], 1.0 - dst[2], 1.0 - dst[3]],
            // GL defines the alpha channel of SRC_ALPHA_SATURATE as 1.
            Self::SrcAlphaSaturate => {
                let f = src[3].min(1.0 - dst[3]);
                [f, f, f, 1.0]
            }
        }
    }
}

/// Which faces to discard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CullMode {
    Front,
    Back,
    FrontAndBack,
}

/// Front-face winding direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontFace {
    Cw,
    Ccw,
}

/// How the texture colour combines with the incoming fragment colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TexEnvMode {
    Modulate,
    Replace,
    Decal,
    Blend,
    Add,
}

/// Fog falloff curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FogMode {
    Linear,
    Exp,
    Exp2,
}

/// Everything the per-fragment stage needs. Assembled by the state
/// machine once per draw call rather than read through a lock per pixel.
#[derive(Debug, Clone)]
pub struct PipelineState {
    pub viewport: (i32, i32, i32, i32),
    pub depth_range: (f32, f32),
    pub scissor: Option<(i32, i32, i32, i32)>,
    pub depth_test: bool,
    pub depth_func: CompareFunc,
    pub depth_write: bool,
    pub alpha_test: bool,
    pub alpha_func: CompareFunc,
    pub alpha_ref: f32,
    pub blend: bool,
    pub blend_src: BlendFactor,
    pub blend_dst: BlendFactor,
    pub cull: Option<CullMode>,
    pub front_face: FrontFace,
    pub color_mask: [bool; 4],
    pub fog: bool,
    pub fog_mode: FogMode,
    pub fog_color: [f32; 4],
    pub fog_start: f32,
    pub fog_end: f32,
    pub fog_density: f32,
    /// `false` selects flat shading, which uses the last vertex's colour
    /// across the whole triangle.
    pub smooth_shading: bool,
}

impl Default for PipelineState {
    fn default() -> Self {
        Self {
            viewport: (0, 0, 0, 0),
            depth_range: (0.0, 1.0),
            scissor: None,
            depth_test: false,
            depth_func: CompareFunc::Less,
            depth_write: true,
            alpha_test: false,
            alpha_func: CompareFunc::Always,
            alpha_ref: 0.0,
            blend: false,
            blend_src: BlendFactor::One,
            blend_dst: BlendFactor::Zero,
            cull: None,
            front_face: FrontFace::Ccw,
            color_mask: [true; 4],
            fog: false,
            fog_mode: FogMode::Exp,
            fog_color: [0.0, 0.0, 0.0, 0.0],
            fog_start: 0.0,
            fog_end: 1.0,
            fog_density: 1.0,
            smooth_shading: true,
        }
    }
}

/// Colour and depth attachments.
pub struct RenderTarget {
    pub width: u32,
    pub height: u32,
    /// RGBA8888, row-major, top row first.
    pub color: Vec<u8>,
    /// Window-space depth in `[0, 1]`, smaller is nearer.
    pub depth: Vec<f32>,
}

impl RenderTarget {
    pub fn new(width: u32, height: u32) -> Self {
        let px = (width as usize) * (height as usize);
        Self {
            width,
            height,
            color: vec![0u8; px * 4],
            depth: vec![1.0f32; px],
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height {
            return;
        }
        *self = Self::new(width, height);
    }

    /// `glClear(GL_COLOR_BUFFER_BIT)` honouring the scissor box.
    pub fn clear_color(&mut self, rgba: [u8; 4], scissor: Option<(i32, i32, i32, i32)>) {
        match self.scissor_bounds(scissor) {
            None => {}
            Some((x0, y0, x1, y1)) if self.covers_all(x0, y0, x1, y1) => {
                for px in self.color.chunks_exact_mut(4) {
                    px.copy_from_slice(&rgba);
                }
            }
            Some((x0, y0, x1, y1)) => {
                for y in y0..y1 {
                    let row = (y as usize) * (self.width as usize);
                    for x in x0..x1 {
                        let i = (row + x as usize) * 4;
                        self.color[i..i + 4].copy_from_slice(&rgba);
                    }
                }
            }
        }
    }

    /// `glClear(GL_DEPTH_BUFFER_BIT)` honouring the scissor box.
    pub fn clear_depth(&mut self, value: f32, scissor: Option<(i32, i32, i32, i32)>) {
        match self.scissor_bounds(scissor) {
            None => {}
            Some((x0, y0, x1, y1)) if self.covers_all(x0, y0, x1, y1) => {
                self.depth.fill(value);
            }
            Some((x0, y0, x1, y1)) => {
                for y in y0..y1 {
                    let row = (y as usize) * (self.width as usize);
                    for x in x0..x1 {
                        self.depth[row + x as usize] = value;
                    }
                }
            }
        }
    }

    fn covers_all(&self, x0: i32, y0: i32, x1: i32, y1: i32) -> bool {
        x0 == 0 && y0 == 0 && x1 == self.width as i32 && y1 == self.height as i32
    }

    /// Intersect a GL scissor box (bottom-left origin) with the target,
    /// returning top-left-origin half-open bounds. `None` means the
    /// intersection is empty and the caller should do nothing.
    fn scissor_bounds(
        &self,
        scissor: Option<(i32, i32, i32, i32)>,
    ) -> Option<(i32, i32, i32, i32)> {
        let (w, h) = (self.width as i32, self.height as i32);
        let Some((sx, sy, sw, sh)) = scissor else {
            return Some((0, 0, w, h));
        };
        if sw <= 0 || sh <= 0 {
            return None;
        }
        let x0 = sx.max(0);
        let x1 = (sx + sw).min(w);
        // GL's scissor Y is measured from the bottom; flip to rows.
        let top = h - (sy + sh);
        let y0 = top.max(0);
        let y1 = (h - sy).min(h);
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        Some((x0, y0, x1, y1))
    }

    /// Convert the colour buffer to RGB565 little-endian, the format the
    /// PocketHLE framebuffer holds. Alpha is dropped: the window surface
    /// is opaque.
    pub fn to_rgb565(&self, out: &mut [u8]) {
        for (i, px) in self.color.chunks_exact(4).enumerate() {
            let o = i * 2;
            if o + 1 >= out.len() {
                break;
            }
            let v = (((px[0] as u16) >> 3) << 11)
                | (((px[1] as u16) >> 2) << 5)
                | ((px[2] as u16) >> 3);
            out[o] = (v & 0xFF) as u8;
            out[o + 1] = (v >> 8) as u8;
        }
    }
}

/// Texture sampling callback, called with the texture unit a stage
/// draws from. Returning `None` means that unit has no usable texture,
/// in which case the stage leaves the fragment colour alone.
pub type SampleFn<'a> = &'a dyn Fn(usize, f32, f32) -> Option<[u8; 4]>;

/// One enabled texture unit, as the rasterizer sees it.
///
/// GL ES 1.1 multitexturing is a cascade: the stages are applied in
/// ascending unit order and each one combines its texel with the colour
/// the stage before it produced, starting from the interpolated vertex
/// colour. Games lean on that. Sticky Balls stores a ball as a DXT1
/// colour map — a format with no alpha channel at all — on one stage
/// and a white-RGB circular alpha mask on the next, so only running
/// both, in order, cuts the ball out of its square.
#[derive(Debug, Clone, Copy)]
pub struct TextureStage {
    /// Which texture unit this is. Selects both the texture
    /// [`SampleFn`] reads and the vertex coordinate set it reads with.
    pub unit: usize,
    /// This unit's `glTexEnv(GL_TEXTURE_ENV_MODE)`.
    pub env: TexEnvMode,
    /// This unit's `GL_TEXTURE_ENV_COLOR`; only `GL_BLEND` reads it.
    pub env_color: [f32; 4],
}

#[inline]
fn to_unit(v: u8) -> f32 {
    v as f32 / 255.0
}

#[inline]
fn to_byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// Window-space vertex after the perspective divide.
#[derive(Debug, Clone, Copy)]
struct ScreenVertex {
    x: f32,
    y: f32,
    z: f32,
    /// Reciprocal of clip `w`, for perspective-correct interpolation.
    inv_w: f32,
    color: [f32; 4],
    texcoord: [[f32; 2]; MAX_TEXTURE_STAGES],
    fog_depth: f32,
}

/// Rasterize one triangle. Returns the number of fragments written,
/// which the tests use to distinguish "culled" from "drawn but
/// invisible".
pub fn draw_triangle(
    target: &mut RenderTarget,
    state: &PipelineState,
    sample: SampleFn<'_>,
    stages: &[TextureStage],
    tri: [Vertex; 3],
) -> usize {
    // Near/far clipping. A full Sutherland-Hodgman clip against all six
    // planes would be more correct, but the near plane is the only one
    // that can produce a divide-by-zero or a sign flip in the
    // perspective divide, and X/Y are handled by the bounding-box
    // intersection below. Clipping the near plane can turn a triangle
    // into a quad, hence the fan.
    let mut polygon: Vec<Vertex> = Vec::with_capacity(4);
    clip_near(&tri, &mut polygon);
    if polygon.len() < 3 {
        return 0;
    }
    let mut written = 0;
    for i in 1..polygon.len() - 1 {
        written += raster_clipped(
            target,
            state,
            sample,
            stages,
            [polygon[0], polygon[i], polygon[i + 1]],
        );
    }
    written
}

/// Clip a triangle against `z > -w`, the near plane in GL clip space.
fn clip_near(tri: &[Vertex; 3], out: &mut Vec<Vertex>) {
    const EPS: f32 = 1e-6;
    let dist = |v: &Vertex| v.pos[2] + v.pos[3];
    for i in 0..3 {
        let a = &tri[i];
        let b = &tri[(i + 1) % 3];
        let da = dist(a);
        let db = dist(b);
        if da >= EPS {
            out.push(*a);
        }
        // Sign change means the edge crosses the plane; emit the
        // intersection point.
        if (da >= EPS) != (db >= EPS) {
            let t = da / (da - db);
            out.push(lerp_vertex(a, b, t));
        }
    }
}

fn lerp_vertex(a: &Vertex, b: &Vertex, t: f32) -> Vertex {
    let mix = |x: f32, y: f32| x + (y - x) * t;
    Vertex {
        pos: [
            mix(a.pos[0], b.pos[0]),
            mix(a.pos[1], b.pos[1]),
            mix(a.pos[2], b.pos[2]),
            mix(a.pos[3], b.pos[3]),
        ],
        color: [
            mix(a.color[0], b.color[0]),
            mix(a.color[1], b.color[1]),
            mix(a.color[2], b.color[2]),
            mix(a.color[3], b.color[3]),
        ],
        texcoord: std::array::from_fn(|u| {
            [
                mix(a.texcoord[u][0], b.texcoord[u][0]),
                mix(a.texcoord[u][1], b.texcoord[u][1]),
            ]
        }),
        fog_depth: mix(a.fog_depth, b.fog_depth),
    }
}

fn raster_clipped(
    target: &mut RenderTarget,
    state: &PipelineState,
    sample: SampleFn<'_>,
    stages: &[TextureStage],
    tri: [Vertex; 3],
) -> usize {
    let (vx, vy, vw, vh) = state.viewport;
    if vw <= 0 || vh <= 0 {
        return 0;
    }
    let (dn, df) = state.depth_range;

    // Perspective divide and viewport transform. GL's viewport has a
    // bottom-left origin; our colour buffer's first row is the top, so
    // Y is negated here rather than at every pixel.
    let mut sv = [ScreenVertex {
        x: 0.0,
        y: 0.0,
        z: 0.0,
        inv_w: 1.0,
        color: [0.0; 4],
        texcoord: [[0.0; 2]; MAX_TEXTURE_STAGES],
        fog_depth: 0.0,
    }; 3];
    for (i, v) in tri.iter().enumerate() {
        let w = v.pos[3];
        if w.abs() < 1e-9 || !w.is_finite() {
            return 0;
        }
        let inv_w = 1.0 / w;
        let ndc = [v.pos[0] * inv_w, v.pos[1] * inv_w, v.pos[2] * inv_w];
        if !ndc[0].is_finite() || !ndc[1].is_finite() || !ndc[2].is_finite() {
            return 0;
        }
        sv[i] = ScreenVertex {
            x: vx as f32 + (ndc[0] * 0.5 + 0.5) * vw as f32,
            y: target.height as f32 - (vy as f32 + (ndc[1] * 0.5 + 0.5) * vh as f32),
            z: (dn + (df - dn) * (ndc[2] * 0.5 + 0.5)).clamp(0.0, 1.0),
            inv_w,
            color: v.color,
            texcoord: v.texcoord,
            fog_depth: v.fog_depth,
        };
    }

    // Signed area in window space. Y was flipped above, so a
    // counter-clockwise triangle in GL's frame has a negative area here.
    let area =
        (sv[1].x - sv[0].x) * (sv[2].y - sv[0].y) - (sv[2].x - sv[0].x) * (sv[1].y - sv[0].y);
    if area == 0.0 || !area.is_finite() {
        return 0;
    }
    let is_front = match state.front_face {
        FrontFace::Ccw => area < 0.0,
        FrontFace::Cw => area > 0.0,
    };
    if let Some(cull) = state.cull {
        let discard = match cull {
            CullMode::FrontAndBack => true,
            CullMode::Front => is_front,
            CullMode::Back => !is_front,
        };
        if discard {
            return 0;
        }
    }

    // Bounding box, clipped to the target and the scissor box.
    let Some((cx0, cy0, cx1, cy1)) = target.scissor_bounds(state.scissor) else {
        return 0;
    };
    let min_x = sv.iter().fold(f32::INFINITY, |m, v| m.min(v.x)).floor() as i32;
    let max_x = sv.iter().fold(f32::NEG_INFINITY, |m, v| m.max(v.x)).ceil() as i32;
    let min_y = sv.iter().fold(f32::INFINITY, |m, v| m.min(v.y)).floor() as i32;
    let max_y = sv.iter().fold(f32::NEG_INFINITY, |m, v| m.max(v.y)).ceil() as i32;
    let x0 = min_x.max(cx0);
    let x1 = (max_x + 1).min(cx1);
    let y0 = min_y.max(cy0);
    let y1 = (max_y + 1).min(cy1);
    if x0 >= x1 || y0 >= y1 {
        return 0;
    }

    let inv_area = 1.0 / area;
    // Flat shading takes the provoking vertex, which for a triangle is
    // the last one.
    let flat_color = sv[2].color;
    let mut written = 0usize;
    let stride = target.width as usize;

    for py in y0..y1 {
        for px in x0..x1 {
            // Sample at the pixel centre.
            let sx = px as f32 + 0.5;
            let sy = py as f32 + 0.5;

            // Barycentric coordinates via edge functions.
            let w0 = ((sv[2].x - sv[1].x) * (sy - sv[1].y) - (sv[2].y - sv[1].y) * (sx - sv[1].x))
                * inv_area;
            let w1 = ((sv[0].x - sv[2].x) * (sy - sv[2].y) - (sv[0].y - sv[2].y) * (sx - sv[2].x))
                * inv_area;
            let w2 = 1.0 - w0 - w1;
            // Negative weights are outside the triangle. The tiny
            // epsilon keeps shared edges from dropping a pixel column
            // between two triangles of a strip.
            const EDGE_EPS: f32 = -1e-6;
            if w0 < EDGE_EPS || w1 < EDGE_EPS || w2 < EDGE_EPS {
                continue;
            }

            let idx = py as usize * stride + px as usize;

            // Depth interpolates linearly in window space.
            let z = w0 * sv[0].z + w1 * sv[1].z + w2 * sv[2].z;
            if state.depth_test && !state.depth_func.test(z, target.depth[idx]) {
                continue;
            }

            // Perspective-correct attribute interpolation: weight each
            // attribute by 1/w, interpolate, then divide by the
            // interpolated 1/w.
            let iw = w0 * sv[0].inv_w + w1 * sv[1].inv_w + w2 * sv[2].inv_w;
            if iw <= 0.0 || !iw.is_finite() {
                continue;
            }
            let rec = 1.0 / iw;
            let p0 = w0 * sv[0].inv_w * rec;
            let p1 = w1 * sv[1].inv_w * rec;
            let p2 = w2 * sv[2].inv_w * rec;

            let mut frag = if state.smooth_shading {
                [
                    p0 * sv[0].color[0] + p1 * sv[1].color[0] + p2 * sv[2].color[0],
                    p0 * sv[0].color[1] + p1 * sv[1].color[1] + p2 * sv[2].color[1],
                    p0 * sv[0].color[2] + p1 * sv[1].color[2] + p2 * sv[2].color[2],
                    p0 * sv[0].color[3] + p1 * sv[1].color[3] + p2 * sv[2].color[3],
                ]
            } else {
                flat_color
            };

            // The multitexture cascade: each stage's combiner takes the
            // colour the previous stage produced, so the loop order is
            // the stage order. Coordinates are interpolated per stage
            // rather than up front, because an unused unit's coordinate
            // set must not cost anything per fragment.
            for stage in stages {
                let uv = |c: usize| {
                    p0 * sv[0].texcoord[stage.unit][c]
                        + p1 * sv[1].texcoord[stage.unit][c]
                        + p2 * sv[2].texcoord[stage.unit][c]
                };
                if let Some(texel) = sample(stage.unit, uv(0), uv(1)) {
                    let tc = [
                        to_unit(texel[0]),
                        to_unit(texel[1]),
                        to_unit(texel[2]),
                        to_unit(texel[3]),
                    ];
                    frag = apply_tex_env(stage, frag, tc);
                }
            }

            if state.alpha_test && !state.alpha_func.test(frag[3], state.alpha_ref) {
                continue;
            }

            if state.fog {
                let depth = p0 * sv[0].fog_depth + p1 * sv[1].fog_depth + p2 * sv[2].fog_depth;
                let f = fog_factor(state, depth);
                for (c, fc) in frag.iter_mut().zip(state.fog_color).take(3) {
                    *c = f * *c + (1.0 - f) * fc;
                }
            }

            let ci = idx * 4;
            let dst = [
                to_unit(target.color[ci]),
                to_unit(target.color[ci + 1]),
                to_unit(target.color[ci + 2]),
                to_unit(target.color[ci + 3]),
            ];
            let out = if state.blend {
                let sf = state.blend_src.resolve(frag, dst);
                let df = state.blend_dst.resolve(frag, dst);
                [
                    (frag[0] * sf[0] + dst[0] * df[0]).clamp(0.0, 1.0),
                    (frag[1] * sf[1] + dst[1] * df[1]).clamp(0.0, 1.0),
                    (frag[2] * sf[2] + dst[2] * df[2]).clamp(0.0, 1.0),
                    (frag[3] * sf[3] + dst[3] * df[3]).clamp(0.0, 1.0),
                ]
            } else {
                frag
            };

            for (c, (&value, &masked)) in out.iter().zip(&state.color_mask).enumerate() {
                if masked {
                    target.color[ci + c] = to_byte(value);
                }
            }
            if state.depth_write {
                target.depth[idx] = z;
            }
            written += 1;
        }
    }
    written
}

/// The `glTexEnv` colour combiners GL ES 1.1 defines for a single unit.
/// `frag` is the incoming colour: the interpolated vertex colour for the
/// first stage, and the previous stage's result for every stage after.
fn apply_tex_env(stage: &TextureStage, frag: [f32; 4], tex: [f32; 4]) -> [f32; 4] {
    match stage.env {
        TexEnvMode::Replace => tex,
        TexEnvMode::Modulate => [
            frag[0] * tex[0],
            frag[1] * tex[1],
            frag[2] * tex[2],
            frag[3] * tex[3],
        ],
        // DECAL blends RGB by the texture alpha and keeps fragment alpha.
        TexEnvMode::Decal => [
            frag[0] * (1.0 - tex[3]) + tex[0] * tex[3],
            frag[1] * (1.0 - tex[3]) + tex[1] * tex[3],
            frag[2] * (1.0 - tex[3]) + tex[2] * tex[3],
            frag[3],
        ],
        // BLEND interpolates towards the constant env colour.
        TexEnvMode::Blend => [
            frag[0] * (1.0 - tex[0]) + stage.env_color[0] * tex[0],
            frag[1] * (1.0 - tex[1]) + stage.env_color[1] * tex[1],
            frag[2] * (1.0 - tex[2]) + stage.env_color[2] * tex[2],
            frag[3] * tex[3],
        ],
        TexEnvMode::Add => [
            (frag[0] + tex[0]).min(1.0),
            (frag[1] + tex[1]).min(1.0),
            (frag[2] + tex[2]).min(1.0),
            frag[3] * tex[3],
        ],
    }
}

/// Fog blend factor: 1.0 means fully unfogged.
fn fog_factor(state: &PipelineState, depth: f32) -> f32 {
    let depth = depth.max(0.0);
    let density = state.fog_density.max(0.0);
    let f = match state.fog_mode {
        FogMode::Linear => {
            let span = state.fog_end - state.fog_start;
            if span.abs() < 1e-9 {
                1.0
            } else {
                (state.fog_end - depth) / span
            }
        }
        FogMode::Exp => (-density * depth).exp(),
        FogMode::Exp2 => {
            let d = density * depth;
            (-(d * d)).exp()
        }
    };
    f.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_240x320() -> RenderTarget {
        RenderTarget::new(240, 320)
    }

    fn full_viewport(t: &RenderTarget) -> PipelineState {
        PipelineState {
            viewport: (0, 0, t.width as i32, t.height as i32),
            ..Default::default()
        }
    }

    fn no_texture(_unit: usize, _s: f32, _t: f32) -> Option<[u8; 4]> {
        None
    }

    /// A single stage on unit 0 with the given combiner.
    fn stage(env: TexEnvMode) -> [TextureStage; 1] {
        [TextureStage {
            unit: 0,
            env,
            env_color: [0.0; 4],
        }]
    }

    /// A triangle covering the whole lower-left half of clip space.
    fn big_tri(color: [f32; 4]) -> [Vertex; 3] {
        [
            Vertex {
                pos: [-1.0, -1.0, 0.0, 1.0],
                color,
                ..Default::default()
            },
            Vertex {
                pos: [1.0, -1.0, 0.0, 1.0],
                color,
                ..Default::default()
            },
            Vertex {
                pos: [-1.0, 1.0, 0.0, 1.0],
                color,
                ..Default::default()
            },
        ]
    }

    fn pixel(t: &RenderTarget, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * t.width + x) * 4) as usize;
        [t.color[i], t.color[i + 1], t.color[i + 2], t.color[i + 3]]
    }

    #[test]
    fn triangle_covers_expected_pixels() {
        let mut t = target_240x320();
        let s = full_viewport(&t);
        let n = draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0, 0.0, 0.0, 1.0]));
        assert!(n > 0, "triangle produced no fragments");
        // Bottom-left of the GL viewport is the bottom-left row of our
        // buffer: inside the triangle.
        assert_eq!(pixel(&t, 2, 317), [255, 0, 0, 255]);
        // Top-right corner is outside the lower-left half.
        assert_eq!(pixel(&t, 237, 2), [0, 0, 0, 0]);
    }

    #[test]
    fn y_axis_is_flipped_for_the_color_buffer() {
        // A triangle in the *upper* half of GL clip space must land in
        // the *top* rows of the buffer. Getting this backwards renders
        // every game upside down.
        let mut t = RenderTarget::new(8, 8);
        let s = full_viewport(&t);
        let tri = [
            Vertex {
                pos: [-1.0, 0.1, 0.0, 1.0],
                color: [0.0, 1.0, 0.0, 1.0],
                ..Default::default()
            },
            Vertex {
                pos: [1.0, 0.1, 0.0, 1.0],
                color: [0.0, 1.0, 0.0, 1.0],
                ..Default::default()
            },
            Vertex {
                pos: [0.0, 1.0, 0.0, 1.0],
                color: [0.0, 1.0, 0.0, 1.0],
                ..Default::default()
            },
        ];
        draw_triangle(&mut t, &s, &no_texture, &[], tri);
        assert_eq!(pixel(&t, 4, 0)[1], 255, "top row should be covered");
        assert_eq!(pixel(&t, 4, 7)[1], 0, "bottom row should be empty");
    }

    #[test]
    fn back_face_culling_discards_clockwise_triangles() {
        let mut t = target_240x320();
        let mut s = full_viewport(&t);
        s.cull = Some(CullMode::Back);
        s.front_face = FrontFace::Ccw;
        // big_tri is counter-clockwise in GL's frame, so it survives.
        assert!(draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0; 4])) > 0);
        // Reversing the winding must make it vanish.
        let mut rev = big_tri([1.0; 4]);
        rev.swap(1, 2);
        let mut t2 = target_240x320();
        assert_eq!(draw_triangle(&mut t2, &s, &no_texture, &[], rev), 0);
    }

    #[test]
    fn front_face_setting_inverts_which_side_is_culled() {
        let mut t = target_240x320();
        let mut s = full_viewport(&t);
        s.cull = Some(CullMode::Back);
        s.front_face = FrontFace::Cw;
        // With CW declared front, the CCW triangle is now the back face.
        assert_eq!(
            draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0; 4])),
            0
        );
    }

    #[test]
    fn depth_test_rejects_farther_fragments() {
        let mut t = target_240x320();
        let mut s = full_viewport(&t);
        s.depth_test = true;
        s.depth_func = CompareFunc::Less;

        let mut near = big_tri([1.0, 0.0, 0.0, 1.0]);
        for v in near.iter_mut() {
            v.pos[2] = -0.5;
        }
        let mut far = big_tri([0.0, 1.0, 0.0, 1.0]);
        for v in far.iter_mut() {
            v.pos[2] = 0.5;
        }

        draw_triangle(&mut t, &s, &no_texture, &[], near);
        let after_near = pixel(&t, 2, 317);
        assert_eq!(after_near, [255, 0, 0, 255]);
        // The farther triangle must not overwrite it.
        assert_eq!(draw_triangle(&mut t, &s, &no_texture, &[], far), 0);
        assert_eq!(pixel(&t, 2, 317), [255, 0, 0, 255]);
    }

    #[test]
    fn depth_write_disabled_leaves_buffer_untouched() {
        let mut t = target_240x320();
        let mut s = full_viewport(&t);
        s.depth_test = true;
        s.depth_write = false;
        let before = t.depth[317 * 240 + 2];
        draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0; 4]));
        assert_eq!(t.depth[317 * 240 + 2], before);
    }

    #[test]
    fn alpha_test_discards_below_reference() {
        let mut t = target_240x320();
        let mut s = full_viewport(&t);
        s.alpha_test = true;
        s.alpha_func = CompareFunc::Greater;
        s.alpha_ref = 0.5;
        // Alpha 0.25 fails a `> 0.5` test everywhere.
        assert_eq!(
            draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0, 1.0, 1.0, 0.25])),
            0
        );
        assert!(draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0, 1.0, 1.0, 0.75])) > 0);
    }

    #[test]
    fn src_alpha_blending_halves_a_white_triangle_over_black() {
        let mut t = target_240x320();
        let mut s = full_viewport(&t);
        s.blend = true;
        s.blend_src = BlendFactor::SrcAlpha;
        s.blend_dst = BlendFactor::OneMinusSrcAlpha;
        draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0, 1.0, 1.0, 0.5]));
        let px = pixel(&t, 2, 317);
        // 0.5 * 255 rounds to 128.
        assert!(
            (px[0] as i32 - 128).abs() <= 1,
            "expected ~128, got {}",
            px[0]
        );
    }

    #[test]
    fn modulate_multiplies_texture_by_vertex_color() {
        let mut t = target_240x320();
        let s = full_viewport(&t);
        let half_grey = |_u: usize, _s: f32, _t: f32| Some([128u8, 128, 128, 255]);
        let stages = stage(TexEnvMode::Modulate);
        draw_triangle(
            &mut t,
            &s,
            &half_grey,
            &stages,
            big_tri([1.0, 0.0, 0.0, 1.0]),
        );
        let px = pixel(&t, 2, 317);
        assert!((px[0] as i32 - 128).abs() <= 1);
        assert_eq!(px[1], 0);
    }

    #[test]
    fn replace_ignores_vertex_color() {
        let mut t = target_240x320();
        let s = full_viewport(&t);
        let blue = |_u: usize, _s: f32, _t: f32| Some([0u8, 0, 255, 255]);
        let stages = stage(TexEnvMode::Replace);
        draw_triangle(&mut t, &s, &blue, &stages, big_tri([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(pixel(&t, 2, 317), [0, 0, 255, 255]);
    }

    #[test]
    fn a_later_stage_combines_with_the_colour_the_earlier_one_produced() {
        // Sticky Balls draws a ball as a DXT1 colour map — a format with
        // no alpha channel at all — on one stage and a white-RGB texture
        // whose shape lives in its alpha on the next. Only cascading the
        // stages in order both colours the ball and cuts it out of its
        // square; either stage on its own loses one of the two.
        let mut t = target_240x320();
        let s = full_viewport(&t);
        let layers = |unit: usize, _s: f32, _t: f32| match unit {
            0 => Some([0u8, 0, 255, 255]),
            _ => Some([255u8, 255, 255, 64]),
        };
        let stages = [
            TextureStage {
                unit: 0,
                env: TexEnvMode::Replace,
                env_color: [0.0; 4],
            },
            TextureStage {
                unit: 1,
                env: TexEnvMode::Modulate,
                env_color: [0.0; 4],
            },
        ];
        draw_triangle(&mut t, &s, &layers, &stages, big_tri([1.0, 0.0, 0.0, 1.0]));
        // Stage 0 replaces the red vertex colour with the map's blue;
        // stage 1's white RGB keeps that blue and its alpha carries
        // through as the fragment's.
        let px = pixel(&t, 2, 317);
        assert_eq!(px[..3], [0, 0, 255]);
        assert!((px[3] as i32 - 64).abs() <= 1, "mask alpha lost: {px:?}");
    }

    #[test]
    fn color_mask_protects_channels() {
        let mut t = target_240x320();
        let mut s = full_viewport(&t);
        s.color_mask = [true, false, false, true];
        draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0, 1.0, 1.0, 1.0]));
        let px = pixel(&t, 2, 317);
        assert_eq!(px[0], 255);
        assert_eq!(px[1], 0, "green was masked off but got written");
        assert_eq!(px[2], 0);
    }

    #[test]
    fn scissor_box_confines_drawing_and_uses_gl_origin() {
        let mut t = RenderTarget::new(8, 8);
        let mut s = full_viewport(&t);
        // Bottom-left 4x4 quadrant in GL coordinates.
        s.scissor = Some((0, 0, 4, 4));
        draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0; 4]));
        // Bottom-left rows (high row indices) are inside.
        assert_eq!(pixel(&t, 1, 7)[0], 255);
        // Row 3 is in the top half, outside the scissor.
        assert_eq!(pixel(&t, 1, 3)[0], 0);
    }

    #[test]
    fn empty_scissor_box_draws_nothing() {
        let mut t = RenderTarget::new(8, 8);
        let mut s = full_viewport(&t);
        s.scissor = Some((0, 0, 0, 0));
        assert_eq!(
            draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0; 4])),
            0
        );
    }

    #[test]
    fn near_plane_clipping_keeps_the_visible_part() {
        let mut t = target_240x320();
        let s = full_viewport(&t);
        // One vertex behind the eye (z < -w) must be clipped, not
        // produce a garbage projection or a divide-by-zero.
        let tri = [
            Vertex {
                pos: [-1.0, -1.0, -2.0, 1.0],
                color: [1.0, 1.0, 1.0, 1.0],
                ..Default::default()
            },
            Vertex {
                pos: [1.0, -1.0, 0.0, 1.0],
                color: [1.0, 1.0, 1.0, 1.0],
                ..Default::default()
            },
            Vertex {
                pos: [-1.0, 1.0, 0.0, 1.0],
                color: [1.0, 1.0, 1.0, 1.0],
                ..Default::default()
            },
        ];
        let n = draw_triangle(&mut t, &s, &no_texture, &[], tri);
        assert!(n > 0, "fully clipped a partially visible triangle");
    }

    #[test]
    fn fully_behind_the_eye_is_rejected() {
        let mut t = target_240x320();
        let s = full_viewport(&t);
        let mut tri = big_tri([1.0; 4]);
        for v in tri.iter_mut() {
            v.pos[2] = -5.0;
        }
        assert_eq!(draw_triangle(&mut t, &s, &no_texture, &[], tri), 0);
    }

    #[test]
    fn degenerate_triangle_is_rejected() {
        let mut t = target_240x320();
        let s = full_viewport(&t);
        let v = Vertex {
            pos: [0.0, 0.0, 0.0, 1.0],
            ..Default::default()
        };
        assert_eq!(draw_triangle(&mut t, &s, &no_texture, &[], [v, v, v]), 0);
    }

    #[test]
    fn flat_shading_uses_the_last_vertex_color() {
        let mut t = target_240x320();
        let mut s = full_viewport(&t);
        s.smooth_shading = false;
        let mut tri = big_tri([1.0; 4]);
        tri[0].color = [1.0, 0.0, 0.0, 1.0];
        tri[1].color = [0.0, 1.0, 0.0, 1.0];
        tri[2].color = [0.0, 0.0, 1.0, 1.0];
        draw_triangle(&mut t, &s, &no_texture, &[], tri);
        // Every covered pixel must be the provoking vertex's blue.
        assert_eq!(pixel(&t, 2, 317), [0, 0, 255, 255]);
    }

    #[test]
    fn smooth_shading_interpolates_between_vertices() {
        let mut t = RenderTarget::new(64, 64);
        let mut s = full_viewport(&t);
        s.smooth_shading = true;
        let tri = [
            Vertex {
                pos: [-1.0, -1.0, 0.0, 1.0],
                color: [1.0, 0.0, 0.0, 1.0],
                ..Default::default()
            },
            Vertex {
                pos: [1.0, -1.0, 0.0, 1.0],
                color: [0.0, 0.0, 0.0, 1.0],
                ..Default::default()
            },
            Vertex {
                pos: [-1.0, 1.0, 0.0, 1.0],
                color: [0.0, 0.0, 0.0, 1.0],
                ..Default::default()
            },
        ];
        draw_triangle(&mut t, &s, &no_texture, &[], tri);
        // Near the red vertex (bottom-left) red is strong; far from it
        // (towards the black vertices) it decays.
        let near_red = pixel(&t, 1, 62)[0];
        let far_red = pixel(&t, 30, 33)[0];
        assert!(near_red > far_red, "no gradient: {near_red} vs {far_red}");
    }

    #[test]
    fn linear_fog_fades_to_the_fog_color() {
        let mut t = target_240x320();
        let mut s = full_viewport(&t);
        s.fog = true;
        s.fog_mode = FogMode::Linear;
        s.fog_start = 0.0;
        s.fog_end = 10.0;
        s.fog_color = [0.0, 0.0, 1.0, 1.0];
        let mut tri = big_tri([1.0, 0.0, 0.0, 1.0]);
        // Fully fogged: at fog_end the factor is 0, so the fragment
        // becomes pure fog colour.
        for v in tri.iter_mut() {
            v.fog_depth = 10.0;
        }
        draw_triangle(&mut t, &s, &no_texture, &[], tri);
        assert_eq!(pixel(&t, 2, 317), [0, 0, 255, 255]);
    }

    #[test]
    fn exponential_fog_is_unfogged_at_zero_depth() {
        let s = PipelineState {
            fog_mode: FogMode::Exp,
            fog_density: 0.25,
            ..PipelineState::default()
        };
        assert_eq!(fog_factor(&s, 0.0), 1.0);
        assert_eq!(fog_factor(&s, -10.0), 1.0);
        assert!((fog_factor(&s, 4.0) - (-1.0f32).exp()).abs() < 1e-6);
    }

    #[test]
    fn exp2_fog_uses_density_squared_in_the_exponent() {
        let s = PipelineState {
            fog_mode: FogMode::Exp2,
            fog_density: 0.5,
            ..PipelineState::default()
        };
        assert!((fog_factor(&s, 2.0) - (-1.0f32).exp()).abs() < 1e-6);
    }

    #[test]
    fn unfogged_geometry_keeps_its_color() {
        let mut t = target_240x320();
        let mut s = full_viewport(&t);
        s.fog = true;
        s.fog_mode = FogMode::Linear;
        s.fog_start = 5.0;
        s.fog_end = 10.0;
        s.fog_color = [0.0, 0.0, 1.0, 1.0];
        let mut tri = big_tri([1.0, 0.0, 0.0, 1.0]);
        for v in tri.iter_mut() {
            v.fog_depth = 0.0;
        }
        draw_triangle(&mut t, &s, &no_texture, &[], tri);
        assert_eq!(pixel(&t, 2, 317), [255, 0, 0, 255]);
    }

    #[test]
    fn clear_color_fills_the_whole_target() {
        let mut t = RenderTarget::new(4, 4);
        t.clear_color([10, 20, 30, 40], None);
        assert_eq!(pixel(&t, 0, 0), [10, 20, 30, 40]);
        assert_eq!(pixel(&t, 3, 3), [10, 20, 30, 40]);
    }

    #[test]
    fn clear_respects_the_scissor_box() {
        let mut t = RenderTarget::new(4, 4);
        t.clear_color([255, 255, 255, 255], Some((0, 0, 2, 2)));
        // Bottom-left 2x2 in GL space = rows 2..4, cols 0..2.
        assert_eq!(pixel(&t, 0, 3), [255, 255, 255, 255]);
        assert_eq!(pixel(&t, 3, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn clear_depth_resets_to_the_far_value() {
        let mut t = RenderTarget::new(2, 2);
        t.depth.fill(0.0);
        t.clear_depth(1.0, None);
        assert!(t.depth.iter().all(|&d| d == 1.0));
    }

    #[test]
    fn rgb565_conversion_preserves_pure_channels() {
        let mut t = RenderTarget::new(2, 1);
        t.color = vec![255, 0, 0, 255, 0, 0, 255, 255];
        let mut out = vec![0u8; 4];
        t.to_rgb565(&mut out);
        assert_eq!(u16::from_le_bytes([out[0], out[1]]), 0xF800);
        assert_eq!(u16::from_le_bytes([out[2], out[3]]), 0x001F);
    }

    #[test]
    fn perspective_divide_shrinks_distant_geometry() {
        // Scaling position and w together leaves NDC unchanged, so
        // coverage must be identical — that is what proves the divide
        // happens at all. Scaling position alone must shrink coverage.
        let s = PipelineState {
            viewport: (0, 0, 64, 64),
            ..Default::default()
        };
        let mut near = RenderTarget::new(64, 64);
        let mut far = RenderTarget::new(64, 64);
        let make = |w: f32| {
            [
                Vertex {
                    pos: [-w, -w, 0.0, w],
                    ..Default::default()
                },
                Vertex {
                    pos: [w, -w, 0.0, w],
                    ..Default::default()
                },
                Vertex {
                    pos: [-w, w, 0.0, w],
                    ..Default::default()
                },
            ]
        };
        // Same NDC coordinates, so identical coverage — this confirms
        // the divide happens at all.
        let a = draw_triangle(&mut near, &s, &no_texture, &[], make(1.0));
        let b = draw_triangle(&mut far, &s, &no_texture, &[], make(4.0));
        assert_eq!(a, b);
        // Now scale only the position, leaving w at 1: coverage must
        // shrink.
        let mut small = RenderTarget::new(64, 64);
        let shrunk = [
            Vertex {
                pos: [-0.25, -0.25, 0.0, 1.0],
                ..Default::default()
            },
            Vertex {
                pos: [0.25, -0.25, 0.0, 1.0],
                ..Default::default()
            },
            Vertex {
                pos: [-0.25, 0.25, 0.0, 1.0],
                ..Default::default()
            },
        ];
        let c = draw_triangle(&mut small, &s, &no_texture, &[], shrunk);
        assert!(c < a, "shrunk triangle covered {c}, full covered {a}");
    }

    #[test]
    fn zero_sized_viewport_draws_nothing() {
        let mut t = target_240x320();
        let s = PipelineState {
            viewport: (0, 0, 0, 0),
            ..Default::default()
        };
        assert_eq!(
            draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0; 4])),
            0
        );
    }

    #[test]
    fn viewport_offsets_the_rendered_area() {
        let mut t = RenderTarget::new(16, 16);
        let s = PipelineState {
            // Right half only.
            viewport: (8, 0, 8, 16),
            ..Default::default()
        };
        draw_triangle(&mut t, &s, &no_texture, &[], big_tri([1.0; 4]));
        // Left half must be untouched.
        for y in 0..16 {
            assert_eq!(pixel(&t, 0, y)[0], 0, "row {y} leaked into the left half");
        }
        assert_eq!(pixel(&t, 9, 15)[0], 255);
    }
}
