//! Texture objects and pixel-format conversion.
//!
//! OpenGL ES 1.1 accepts textures in five base formats crossed with
//! three packed 16-bit types. Windows Mobile games lean on the packed
//! types heavily because a 16-bit texel halves both the file size and
//! the memory bandwidth — Call of Duty 2 uploads 85 textures, most of
//! them `GL_UNSIGNED_SHORT_5_6_5` or `GL_UNSIGNED_SHORT_4_4_4_4`.
//!
//! We decode everything to straight RGBA8888 at upload time so the
//! rasterizer has a single format to sample, and so a future host-GL
//! backend can hand the data to `glTexImage2D` unchanged.

use crate::consts::*;

/// Texture filtering mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Nearest,
    Linear,
}

/// Texture coordinate wrap mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrap {
    Repeat,
    ClampToEdge,
}

/// A single texture object, as created by `glGenTextures`.
#[derive(Debug, Clone)]
pub struct Texture {
    pub width: u32,
    pub height: u32,
    /// Decoded RGBA8888 texels, row-major, `width * height * 4` bytes.
    /// Empty until the first successful `glTexImage2D`.
    pub rgba: Vec<u8>,
    pub min_filter: Filter,
    pub mag_filter: Filter,
    pub wrap_s: Wrap,
    pub wrap_t: Wrap,
}

impl Default for Texture {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            rgba: Vec::new(),
            // GL defaults: min is mipmap-linear, mag is linear. We
            // collapse the mipmap modes to their base filter.
            min_filter: Filter::Linear,
            mag_filter: Filter::Linear,
            wrap_s: Wrap::Repeat,
            wrap_t: Wrap::Repeat,
        }
    }
}

impl Texture {
    /// Is this texture complete enough to sample?
    pub fn is_complete(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.rgba.len() >= (self.width * self.height * 4) as usize
    }

    /// Sample a texel with nearest-neighbour filtering.
    ///
    /// Returns premultiplied-by-nothing straight RGBA. Out-of-range
    /// coordinates are resolved through the wrap modes.
    pub fn sample_nearest(&self, s: f32, t: f32) -> [u8; 4] {
        if !self.is_complete() {
            // An incomplete texture samples as opaque white in our
            // pipeline, which makes `GL_MODULATE` a no-op and leaves
            // the underlying vertex colour visible instead of turning
            // the surface black.
            return [255, 255, 255, 255];
        }
        let x = Self::wrap_coord(s, self.width, self.wrap_s);
        let y = Self::wrap_coord(t, self.height, self.wrap_t);
        let idx = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[idx],
            self.rgba[idx + 1],
            self.rgba[idx + 2],
            self.rgba[idx + 3],
        ]
    }

    /// Sample a texel with bilinear filtering.
    ///
    /// GL places texel centres at half-integer coordinates, so the
    /// weights come from `s * width - 0.5`. Filtering runs on straight
    /// alpha, which lets the RGB of fully transparent texels bleed into
    /// a glyph's edge — that is what real GL does too, and font atlases
    /// are authored knowing it.
    pub fn sample_linear(&self, s: f32, t: f32) -> [u8; 4] {
        if !self.is_complete() {
            return [255, 255, 255, 255];
        }
        let fx = s * self.width as f32 - 0.5;
        let fy = t * self.height as f32 - 0.5;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (wx, wy) = (fx - x0, fy - y0);
        let (x0, y0) = (x0 as i64, y0 as i64);

        let texel = |x: i64, y: i64| {
            let x = Self::wrap_texel(x, self.width, self.wrap_s);
            let y = Self::wrap_texel(y, self.height, self.wrap_t);
            let i = ((y * self.width + x) * 4) as usize;
            &self.rgba[i..i + 4]
        };
        let (c00, c10) = (texel(x0, y0), texel(x0 + 1, y0));
        let (c01, c11) = (texel(x0, y0 + 1), texel(x0 + 1, y0 + 1));

        let mut out = [0u8; 4];
        for (ch, o) in out.iter_mut().enumerate() {
            let top = c00[ch] as f32 + (c10[ch] as f32 - c00[ch] as f32) * wx;
            let bot = c01[ch] as f32 + (c11[ch] as f32 - c01[ch] as f32) * wx;
            *o = (top + (bot - top) * wy).round().clamp(0.0, 255.0) as u8;
        }
        out
    }

    /// Sample with whichever filter the guest asked for.
    ///
    /// The rasterizer has no derivatives, so it cannot tell
    /// magnification from minification; we always honour `mag_filter`.
    /// Games set both to the same value in practice, and the case that
    /// matters visually — a HUD or font quad drawn at roughly 1:1 — is
    /// magnification anyway.
    pub fn sample(&self, s: f32, t: f32) -> [u8; 4] {
        match self.mag_filter {
            Filter::Nearest => self.sample_nearest(s, t),
            Filter::Linear => self.sample_linear(s, t),
        }
    }

    /// Resolve an integer texel coordinate through a wrap mode.
    fn wrap_texel(v: i64, size: u32, wrap: Wrap) -> u32 {
        if size == 0 {
            return 0;
        }
        match wrap {
            Wrap::Repeat => v.rem_euclid(size as i64) as u32,
            Wrap::ClampToEdge => v.clamp(0, size as i64 - 1) as u32,
        }
    }

    fn wrap_coord(v: f32, size: u32, wrap: Wrap) -> u32 {
        if size == 0 {
            return 0;
        }
        let scaled = v * size as f32;
        match wrap {
            Wrap::Repeat => {
                let m = (scaled.floor() as i64).rem_euclid(size as i64);
                m as u32
            }
            Wrap::ClampToEdge => {
                let c = scaled.floor();
                if c < 0.0 {
                    0
                } else if c >= size as f32 {
                    size - 1
                } else {
                    c as u32
                }
            }
        }
    }

    /// Apply a `glTexParameter*` value. Returns `false` if the
    /// enumerant is not one we recognise, which the caller reports as
    /// `GL_INVALID_ENUM`.
    pub fn set_parameter(&mut self, pname: u32, value: u32) -> bool {
        match pname {
            GL_TEXTURE_MIN_FILTER => {
                self.min_filter = match value {
                    GL_NEAREST | GL_NEAREST_MIPMAP_NEAREST | GL_NEAREST_MIPMAP_LINEAR => {
                        Filter::Nearest
                    }
                    GL_LINEAR | GL_LINEAR_MIPMAP_NEAREST | GL_LINEAR_MIPMAP_LINEAR => {
                        Filter::Linear
                    }
                    _ => return false,
                };
                true
            }
            GL_TEXTURE_MAG_FILTER => {
                self.mag_filter = match value {
                    GL_NEAREST => Filter::Nearest,
                    GL_LINEAR => Filter::Linear,
                    _ => return false,
                };
                true
            }
            GL_TEXTURE_WRAP_S => match wrap_from_enum(value) {
                Some(w) => {
                    self.wrap_s = w;
                    true
                }
                None => false,
            },
            GL_TEXTURE_WRAP_T => match wrap_from_enum(value) {
                Some(w) => {
                    self.wrap_t = w;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }
}

fn wrap_from_enum(value: u32) -> Option<Wrap> {
    match value {
        GL_REPEAT => Some(Wrap::Repeat),
        GL_CLAMP_TO_EDGE => Some(Wrap::ClampToEdge),
        _ => None,
    }
}

/// Number of bytes one texel occupies in the guest's buffer.
pub fn bytes_per_texel(format: u32, ty: u32) -> Option<usize> {
    match ty {
        GL_UNSIGNED_SHORT_5_6_5 | GL_UNSIGNED_SHORT_4_4_4_4 | GL_UNSIGNED_SHORT_5_5_5_1 => Some(2),
        GL_UNSIGNED_BYTE => match format {
            GL_ALPHA | GL_LUMINANCE => Some(1),
            GL_LUMINANCE_ALPHA => Some(2),
            GL_RGB => Some(3),
            GL_RGBA => Some(4),
            _ => None,
        },
        _ => None,
    }
}

/// Expand a 5-bit channel to 8 bits, replicating the high bits so that
/// `0b11111` maps to `255` rather than `248`.
#[inline]
fn expand5(v: u16) -> u8 {
    let v = (v & 0x1F) as u8;
    (v << 3) | (v >> 2)
}

#[inline]
fn expand6(v: u16) -> u8 {
    let v = (v & 0x3F) as u8;
    (v << 2) | (v >> 4)
}

#[inline]
fn expand4(v: u16) -> u8 {
    let v = (v & 0x0F) as u8;
    (v << 4) | v
}

/// Decode a guest texel buffer into RGBA8888.
///
/// Returns `None` if the format/type combination is not one OpenGL ES
/// 1.1 permits, which the caller reports as `GL_INVALID_ENUM`.
pub fn decode_to_rgba(
    data: &[u8],
    width: u32,
    height: u32,
    format: u32,
    ty: u32,
) -> Option<Vec<u8>> {
    let texel_bytes = bytes_per_texel(format, ty)?;
    let count = (width as usize).checked_mul(height as usize)?;
    let needed = count.checked_mul(texel_bytes)?;
    if data.len() < needed {
        return None;
    }
    let mut out = Vec::with_capacity(count * 4);
    match ty {
        GL_UNSIGNED_SHORT_5_6_5 => {
            for i in 0..count {
                let p = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                out.extend_from_slice(&[expand5(p >> 11), expand6(p >> 5), expand5(p), 255]);
            }
        }
        GL_UNSIGNED_SHORT_4_4_4_4 => {
            for i in 0..count {
                let p = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                out.extend_from_slice(&[
                    expand4(p >> 12),
                    expand4(p >> 8),
                    expand4(p >> 4),
                    expand4(p),
                ]);
            }
        }
        GL_UNSIGNED_SHORT_5_5_5_1 => {
            for i in 0..count {
                let p = u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]);
                let a = if p & 1 != 0 { 255 } else { 0 };
                out.extend_from_slice(&[expand5(p >> 11), expand5(p >> 6), expand5(p >> 1), a]);
            }
        }
        GL_UNSIGNED_BYTE => match format {
            GL_RGBA => out.extend_from_slice(&data[..needed]),
            GL_RGB => {
                for i in 0..count {
                    let s = i * 3;
                    out.extend_from_slice(&[data[s], data[s + 1], data[s + 2], 255]);
                }
            }
            GL_LUMINANCE => {
                for &l in data.iter().take(count) {
                    out.extend_from_slice(&[l, l, l, 255]);
                }
            }
            GL_LUMINANCE_ALPHA => {
                for i in 0..count {
                    let l = data[i * 2];
                    out.extend_from_slice(&[l, l, l, data[i * 2 + 1]]);
                }
            }
            GL_ALPHA => {
                for &a in data.iter().take(count) {
                    out.extend_from_slice(&[255, 255, 255, a]);
                }
            }
            _ => return None,
        },
        _ => return None,
    }
    Some(out)
}

// ---- ATITC (AMD ATC) ------------------------------------------------------
//
// ATC stores 4x4 blocks. The RGB block is eight bytes laid out like
// DXT1 — two endpoint colours then sixteen two-bit selectors — but the
// endpoints are derived differently, and the top bit of the first
// endpoint picks between two interpolation modes rather than signalling
// punch-through alpha. `EXPLICIT_ALPHA` prefixes each block with eight
// bytes of DXT3-style 4-bit alpha; `INTERPOLATED_ALPHA` prefixes it
// with a DXT5-style alpha block.

/// Blocks needed to cover `width` x `height`, rounding up on both axes.
#[inline]
fn block_counts(width: u32, height: u32) -> (usize, usize) {
    (width.div_ceil(4) as usize, height.div_ceil(4) as usize)
}

/// Bytes `glCompressedTexImage2D` needs for an image in `format`, or
/// `None` if the format is not one we decode.
pub fn compressed_image_size(format: u32, width: u32, height: u32) -> Option<usize> {
    let per_block = match format {
        GL_ATC_RGB_AMD | GL_COMPRESSED_RGB_S3TC_DXT1_EXT | GL_COMPRESSED_RGBA_S3TC_DXT1_EXT => 8,
        GL_ATC_RGBA_EXPLICIT_ALPHA_AMD | GL_ATC_RGBA_INTERPOLATED_ALPHA_AMD => 16,
        GL_PALETTE4_RGB8_OES
        | GL_PALETTE4_RGBA8_OES
        | GL_PALETTE4_R5_G6_B5_OES
        | GL_PALETTE4_RGBA4_OES
        | GL_PALETTE4_RGB5_A1_OES => 0,
        GL_PALETTE8_RGB8_OES
        | GL_PALETTE8_RGBA8_OES
        | GL_PALETTE8_R5_G6_B5_OES
        | GL_PALETTE8_RGBA4_OES
        | GL_PALETTE8_RGB5_A1_OES => 0,
        _ => return None,
    };
    if matches!(
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
    ) {
        let entries: usize = if format >= GL_PALETTE8_RGB8_OES {
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
        let index_bytes = if format >= GL_PALETTE8_RGB8_OES {
            (width as usize).checked_mul(height as usize)?
        } else {
            (width as usize).div_ceil(2).checked_mul(height as usize)?
        };
        return entries.checked_mul(entry_bytes)?.checked_add(index_bytes);
    }
    let (bw, bh) = block_counts(width, height);
    bw.checked_mul(bh)?.checked_mul(per_block)
}

/// Decode one eight-byte DXT1 RGB block into sixteen RGB triples in raster order within the block.
fn dxt1_rgb_block(src: &[u8]) -> ([[u8; 3]; 16], [u8; 16]) {
    let c0 = u16::from_le_bytes([src[0], src[1]]);
    let c1 = u16::from_le_bytes([src[2], src[3]]);
    let bits = u32::from_le_bytes([src[4], src[5], src[6], src[7]]);
    let e0 = [expand5(c0 >> 11), expand6(c0 >> 5), expand5(c0)];
    let e1 = [expand5(c1 >> 11), expand6(c1 >> 5), expand5(c1)];
    let mut palette = [[0u8; 3]; 4];
    palette[0] = e0;
    palette[1] = e1;
    if c0 > c1 {
        let [e0, e1, p2, p3] = &mut palette;
        for ((p2, p3), (p0, p1)) in p2
            .iter_mut()
            .zip(p3.iter_mut())
            .zip(e0.iter().zip(e1.iter()))
        {
            *p2 = ((2 * u32::from(*p0) + u32::from(*p1)) / 3) as u8;
            *p3 = ((u32::from(*p0) + 2 * u32::from(*p1)) / 3) as u8;
        }
    } else {
        let [e0, e1, p2, _] = &mut palette;
        for (p2, (p0, p1)) in p2.iter_mut().zip(e0.iter().zip(e1.iter())) {
            *p2 = ((u32::from(*p0) + u32::from(*p1)) / 2) as u8;
        }
        palette[3] = [0, 0, 0];
    }
    let mut rgb = [[0u8; 3]; 16];
    let mut alpha = [255u8; 16];
    for i in 0..16 {
        let selector = ((bits >> (i * 2)) & 3) as usize;
        rgb[i] = palette[selector];
        if c0 <= c1 && selector == 3 {
            alpha[i] = 0;
        }
    }
    (rgb, alpha)
}

/// Decode one eight-byte ATC RGB block into sixteen RGB triples in
/// raster order within the block.
fn atc_rgb_block(src: &[u8]) -> [[u8; 3]; 16] {
    let c0 = u16::from_le_bytes([src[0], src[1]]);
    let c1 = u16::from_le_bytes([src[2], src[3]]);
    let bits = u32::from_le_bytes([src[4], src[5], src[6], src[7]]);

    // The first endpoint is RGB555 with the mode in bit 15; the second
    // is always RGB565.
    let e0 = [expand5(c0 >> 10), expand5(c0 >> 5), expand5(c0)];
    let e1 = [expand5(c1 >> 11), expand6(c1 >> 5), expand5(c1)];

    let mut pal = [[0u8; 3]; 4];
    if c0 & 0x8000 == 0 {
        // Mode 0: both endpoints are used, with the two interior
        // colours at 3/8 and 5/8 — the same split DXT1 uses at 1/3.
        pal[0] = e0;
        pal[3] = e1;
        for ch in 0..3 {
            let (a, b) = (e0[ch] as u32, e1[ch] as u32);
            pal[1][ch] = ((a * 5 + b * 3) / 8) as u8;
            pal[2][ch] = ((a * 3 + b * 5) / 8) as u8;
        }
    } else {
        // Mode 1: selector 0 is forced to black, which is how ATC
        // encodes blocks that need a hard dark edge without spending an
        // endpoint on it.
        pal[0] = [0, 0, 0];
        pal[2] = e0;
        pal[3] = e1;
        for ch in 0..3 {
            pal[1][ch] = (e0[ch] as u32).saturating_sub(e1[ch] as u32 / 4) as u8;
        }
    }

    let mut out = [[0u8; 3]; 16];
    for (i, texel) in out.iter_mut().enumerate() {
        *texel = pal[((bits >> (i * 2)) & 3) as usize];
    }
    out
}

/// Decode one eight-byte DXT5-style interpolated alpha block into
/// sixteen alpha values.
fn interpolated_alpha_block(src: &[u8]) -> [u8; 16] {
    let (a0, a1) = (src[0], src[1]);
    let mut pal = [0u8; 8];
    pal[0] = a0;
    pal[1] = a1;
    if a0 > a1 {
        for (i, slot) in pal.iter_mut().enumerate().take(8).skip(2) {
            let (w0, w1) = ((8 - i) as u32, (i - 1) as u32);
            *slot = ((a0 as u32 * w0 + a1 as u32 * w1) / 7) as u8;
        }
    } else {
        for (i, slot) in pal.iter_mut().enumerate().take(6).skip(2) {
            let (w0, w1) = ((6 - i) as u32, (i - 1) as u32);
            *slot = ((a0 as u32 * w0 + a1 as u32 * w1) / 5) as u8;
        }
        pal[6] = 0;
        pal[7] = 255;
    }
    // Sixteen three-bit selectors packed into the remaining six bytes.
    let bits = src[2..8]
        .iter()
        .enumerate()
        .fold(0u64, |acc, (i, &b)| acc | ((b as u64) << (i * 8)));
    let mut out = [0u8; 16];
    for (i, a) in out.iter_mut().enumerate() {
        *a = pal[((bits >> (i * 3)) & 7) as usize];
    }
    out
}

fn decode_paletted(data: &[u8], width: u32, height: u32, format: u32) -> Option<Vec<u8>> {
    let is8 = format >= GL_PALETTE8_RGB8_OES;
    let entries = if is8 { 256usize } else { 16usize };
    let rgba_palette = matches!(format, GL_PALETTE4_RGBA8_OES | GL_PALETTE8_RGBA8_OES);
    let rgb8_palette = matches!(format, GL_PALETTE4_RGB8_OES | GL_PALETTE8_RGB8_OES);
    let entry_bytes = if rgba_palette {
        4
    } else if rgb8_palette {
        3
    } else {
        2
    };
    let palette_len = entries.checked_mul(entry_bytes)?;
    let indices_len = if is8 {
        (width as usize).checked_mul(height as usize)?
    } else {
        (width as usize).div_ceil(2).checked_mul(height as usize)?
    };
    if data.len() < palette_len + indices_len {
        return None;
    }
    let mut out = vec![
        0u8;
        (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?
    ];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let idx = if is8 {
                data[palette_len + y * width as usize + x] as usize
            } else {
                let b = data[palette_len + y * (width as usize).div_ceil(2) + x / 2];
                if x & 1 == 0 {
                    (b >> 4) as usize
                } else {
                    (b & 0x0f) as usize
                }
            };
            let po = idx.checked_mul(entry_bytes)?;
            let (r, g, b, a) = if rgba_palette {
                (data[po], data[po + 1], data[po + 2], data[po + 3])
            } else if rgb8_palette {
                (data[po], data[po + 1], data[po + 2], 255)
            } else {
                let v = u16::from_le_bytes([data[po], data[po + 1]]);
                match format {
                    GL_PALETTE4_R5_G6_B5_OES | GL_PALETTE8_R5_G6_B5_OES => {
                        (expand5(v >> 11), expand6(v >> 5), expand5(v), 255)
                    }
                    GL_PALETTE4_RGBA4_OES | GL_PALETTE8_RGBA4_OES => (
                        expand4(v >> 12),
                        expand4(v >> 8),
                        expand4(v >> 4),
                        expand4(v),
                    ),
                    GL_PALETTE4_RGB5_A1_OES | GL_PALETTE8_RGB5_A1_OES => (
                        expand5(v >> 11),
                        expand5(v >> 6),
                        expand5(v >> 1),
                        if v & 1 == 0 { 0 } else { 255 },
                    ),
                    _ => return None,
                }
            };
            let o = (y * width as usize + x) * 4;
            out[o..o + 4].copy_from_slice(&[r, g, b, a]);
        }
    }
    Some(out)
}

/// Decode an ATC image into RGBA8888.
///
/// Returns `None` if the format is not ATC or the guest handed us fewer
/// bytes than the block layout needs.
pub fn decode_compressed_to_rgba(
    data: &[u8],
    width: u32,
    height: u32,
    format: u32,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return Some(Vec::new());
    }
    let needed = compressed_image_size(format, width, height)?;
    if data.len() < needed {
        return None;
    }
    if matches!(
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
    ) {
        return decode_paletted(data, width, height, format);
    }
    let stride = match format {
        GL_ATC_RGB_AMD | GL_COMPRESSED_RGB_S3TC_DXT1_EXT | GL_COMPRESSED_RGBA_S3TC_DXT1_EXT => 8,
        _ => 16,
    };
    let (bw, bh) = block_counts(width, height);
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w.checked_mul(h)?.checked_mul(4)?];

    for by in 0..bh {
        for bx in 0..bw {
            let block = &data[(by * bw + bx) * stride..];
            let (rgb, alpha) = match format {
                GL_ATC_RGB_AMD => (atc_rgb_block(block), [255u8; 16]),
                GL_COMPRESSED_RGB_S3TC_DXT1_EXT | GL_COMPRESSED_RGBA_S3TC_DXT1_EXT => {
                    dxt1_rgb_block(block)
                }
                GL_ATC_RGBA_EXPLICIT_ALPHA_AMD => {
                    let mut a = [0u8; 16];
                    for (i, slot) in a.iter_mut().enumerate() {
                        let nibble = block[i / 2] >> (4 * (i % 2));
                        *slot = expand4(nibble as u16);
                    }
                    (atc_rgb_block(&block[8..]), a)
                }
                _ => (atc_rgb_block(&block[8..]), interpolated_alpha_block(block)),
            };
            // Blocks at the right and bottom edges hang over the image
            // when the dimensions are not multiples of four; the
            // overhanging texels are simply dropped.
            for ty in 0..4 {
                let y = by * 4 + ty;
                if y >= h {
                    break;
                }
                for tx in 0..4 {
                    let x = bx * 4 + tx;
                    if x >= w {
                        break;
                    }
                    let i = ty * 4 + tx;
                    let o = (y * w + x) * 4;
                    out[o..o + 3].copy_from_slice(&rgb[i]);
                    out[o + 3] = alpha[i];
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paletted_rgba8_4bit_decodes_palette_before_indices() {
        let mut data = vec![0u8; 16 * 4 + 1];
        data[0..4].copy_from_slice(&[255, 0, 0, 255]);
        data[4..8].copy_from_slice(&[0, 255, 0, 255]);
        data[64] = 0x10;
        let out = decode_compressed_to_rgba(&data, 2, 1, GL_PALETTE4_RGBA8_OES).unwrap();
        assert_eq!(out, vec![0, 255, 0, 255, 255, 0, 0, 255]);
    }

    #[test]
    fn paletted_rgb565_8bit_decodes_packed_palette() {
        let mut data = vec![0u8; 256 * 2 + 2];
        data[0..2].copy_from_slice(&0xF800u16.to_le_bytes());
        data[2..4].copy_from_slice(&0x001Fu16.to_le_bytes());
        data[512] = 0;
        data[513] = 1;
        let out = decode_compressed_to_rgba(&data, 2, 1, GL_PALETTE8_R5_G6_B5_OES).unwrap();
        assert_eq!(out, vec![255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn rgb565_expands_to_full_range() {
        // Pure white in 5:6:5 must decode to 255,255,255 — not
        // 248,252,248, which is what a naive `<< 3` shift produces and
        // which makes every bright surface visibly dingy.
        let white = 0xFFFFu16.to_le_bytes();
        let out = decode_to_rgba(&white, 1, 1, GL_RGB, GL_UNSIGNED_SHORT_5_6_5).unwrap();
        assert_eq!(out, vec![255, 255, 255, 255]);
    }

    #[test]
    fn rgb565_channel_order_is_r_g_b() {
        // 0xF800 = red only.
        let red = 0xF800u16.to_le_bytes();
        let out = decode_to_rgba(&red, 1, 1, GL_RGB, GL_UNSIGNED_SHORT_5_6_5).unwrap();
        assert_eq!(out, vec![255, 0, 0, 255]);
        // 0x001F = blue only.
        let blue = 0x001Fu16.to_le_bytes();
        let out = decode_to_rgba(&blue, 1, 1, GL_RGB, GL_UNSIGNED_SHORT_5_6_5).unwrap();
        assert_eq!(out, vec![0, 0, 255, 255]);
    }

    #[test]
    fn rgba4444_decodes_alpha() {
        // 0xF00F: red=F, g=0, b=0, a=F
        let px = 0xF00Fu16.to_le_bytes();
        let out = decode_to_rgba(&px, 1, 1, GL_RGBA, GL_UNSIGNED_SHORT_4_4_4_4).unwrap();
        assert_eq!(out, vec![255, 0, 0, 255]);
    }

    #[test]
    fn rgba5551_alpha_is_one_bit() {
        let opaque = 0x0001u16.to_le_bytes();
        let out = decode_to_rgba(&opaque, 1, 1, GL_RGBA, GL_UNSIGNED_SHORT_5_5_5_1).unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(out[3], 255);
        let transparent = 0x0000u16.to_le_bytes();
        let out = decode_to_rgba(&transparent, 1, 1, GL_RGBA, GL_UNSIGNED_SHORT_5_5_5_1).unwrap();
        assert_eq!(out[3], 0);
    }

    #[test]
    fn rgb_ubyte_gains_opaque_alpha() {
        let out = decode_to_rgba(&[10, 20, 30], 1, 1, GL_RGB, GL_UNSIGNED_BYTE).unwrap();
        assert_eq!(out, vec![10, 20, 30, 255]);
    }

    #[test]
    fn truncated_buffer_is_rejected() {
        // A 2×2 RGBA texture needs 16 bytes; 8 must not be read past.
        assert!(decode_to_rgba(&[0u8; 8], 2, 2, GL_RGBA, GL_UNSIGNED_BYTE).is_none());
    }

    #[test]
    fn unsupported_format_is_rejected() {
        assert!(decode_to_rgba(&[0u8; 4], 1, 1, 0x9999, GL_UNSIGNED_BYTE).is_none());
    }

    #[test]
    fn repeat_wraps_negative_coordinates() {
        let mut t = Texture {
            width: 4,
            height: 1,
            rgba: vec![0; 16],
            ..Default::default()
        };
        t.wrap_s = Wrap::Repeat;
        // -0.25 in a 4-texel row is texel 3, not a clamp to 0.
        assert_eq!(Texture::wrap_coord(-0.25, 4, Wrap::Repeat), 3);
        assert_eq!(Texture::wrap_coord(1.25, 4, Wrap::Repeat), 1);
    }

    #[test]
    fn clamp_to_edge_saturates() {
        assert_eq!(Texture::wrap_coord(-5.0, 4, Wrap::ClampToEdge), 0);
        assert_eq!(Texture::wrap_coord(99.0, 4, Wrap::ClampToEdge), 3);
    }

    #[test]
    fn incomplete_texture_samples_white() {
        let t = Texture::default();
        assert_eq!(t.sample_nearest(0.5, 0.5), [255, 255, 255, 255]);
        assert_eq!(t.sample_linear(0.5, 0.5), [255, 255, 255, 255]);
    }

    /// A 2x1 texture, black then white, clamped so the edges do not
    /// wrap around into each other.
    fn ramp_2x1() -> Texture {
        Texture {
            width: 2,
            height: 1,
            rgba: vec![0, 0, 0, 0, 255, 255, 255, 255],
            wrap_s: Wrap::ClampToEdge,
            wrap_t: Wrap::ClampToEdge,
            ..Default::default()
        }
    }

    #[test]
    fn linear_blends_halfway_between_texel_centres() {
        let t = ramp_2x1();
        // Texel centres sit at s = 0.25 and 0.75; sampling exactly
        // between them must return the midpoint of the two colours.
        assert_eq!(t.sample_linear(0.5, 0.5), [128, 128, 128, 128]);
        // And a quarter of the way across is a quarter of the blend.
        assert_eq!(t.sample_linear(0.375, 0.5), [64, 64, 64, 64]);
    }

    #[test]
    fn linear_reproduces_texel_colours_at_their_centres() {
        let t = ramp_2x1();
        assert_eq!(t.sample_linear(0.25, 0.5), [0, 0, 0, 0]);
        assert_eq!(t.sample_linear(0.75, 0.5), [255, 255, 255, 255]);
    }

    #[test]
    fn linear_clamps_rather_than_reading_out_of_bounds() {
        let t = ramp_2x1();
        // Left of the first texel centre there is nothing to blend
        // with, so ClampToEdge repeats the edge texel.
        assert_eq!(t.sample_linear(0.0, 0.5), [0, 0, 0, 0]);
        assert_eq!(t.sample_linear(1.0, 0.5), [255, 255, 255, 255]);
    }

    #[test]
    fn linear_wraps_across_the_seam_when_repeating() {
        let mut t = ramp_2x1();
        t.wrap_s = Wrap::Repeat;
        // At s = 0 the sample straddles the seam, blending texel 1
        // (white, wrapped from the right edge) with texel 0 (black).
        assert_eq!(t.sample_linear(0.0, 0.5), [128, 128, 128, 128]);
    }

    #[test]
    fn sample_dispatches_on_the_requested_filter() {
        let mut t = ramp_2x1();
        t.mag_filter = Filter::Nearest;
        assert_eq!(t.sample(0.5, 0.5), [255, 255, 255, 255]);
        t.mag_filter = Filter::Linear;
        assert_eq!(t.sample(0.5, 0.5), [128, 128, 128, 128]);
    }

    #[test]
    fn mipmap_min_filters_collapse_to_base_filter() {
        let mut t = Texture::default();
        assert!(t.set_parameter(GL_TEXTURE_MIN_FILTER, GL_LINEAR_MIPMAP_LINEAR));
        assert_eq!(t.min_filter, Filter::Linear);
        assert!(t.set_parameter(GL_TEXTURE_MIN_FILTER, GL_NEAREST_MIPMAP_NEAREST));
        assert_eq!(t.min_filter, Filter::Nearest);
    }

    #[test]
    fn unknown_parameter_is_rejected() {
        let mut t = Texture::default();
        assert!(!t.set_parameter(0x9999, GL_LINEAR));
        assert!(!t.set_parameter(GL_TEXTURE_MAG_FILTER, 0x9999));
    }

    /// Build one ATC RGB block: two endpoints and sixteen selectors.
    fn atc_block(c0: u16, c1: u16, sel: u32) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&c0.to_le_bytes());
        b[2..4].copy_from_slice(&c1.to_le_bytes());
        b[4..8].copy_from_slice(&sel.to_le_bytes());
        b
    }

    #[test]
    fn atc_image_size_follows_the_block_layout() {
        // 8 bytes per 4x4 block for RGB, 16 with an alpha block.
        assert_eq!(compressed_image_size(GL_ATC_RGB_AMD, 8, 8), Some(4 * 8));
        assert_eq!(
            compressed_image_size(GL_ATC_RGBA_EXPLICIT_ALPHA_AMD, 8, 8),
            Some(4 * 16)
        );
        // Non-multiples of four round up to whole blocks.
        assert_eq!(compressed_image_size(GL_ATC_RGB_AMD, 5, 1), Some(2 * 8));
        // Anything else is not ours to decode.
        assert_eq!(compressed_image_size(GL_RGBA, 8, 8), None);
    }

    #[test]
    fn atc_mode0_interpolates_between_both_endpoints() {
        // Mode 0 (top bit of c0 clear): white RGB555 endpoint and black
        // RGB565 endpoint, with the four selectors 0..3 in texels 0..3.
        let block = atc_block(0x7FFF, 0x0000, 0b11_10_01_00);
        let out = decode_compressed_to_rgba(&block, 4, 1, GL_ATC_RGB_AMD).unwrap();
        // Selector 0 is endpoint 0, selector 3 is endpoint 1, and the
        // interior colours sit at 5/8 and 3/8 rather than DXT1's 2/3.
        assert_eq!(&out[0..4], &[255, 255, 255, 255]);
        assert_eq!(&out[4..8], &[159, 159, 159, 255]);
        assert_eq!(&out[8..12], &[95, 95, 95, 255]);
        assert_eq!(&out[12..16], &[0, 0, 0, 255]);
    }

    #[test]
    fn atc_mode1_forces_selector_zero_to_black() {
        // Mode 1 (top bit of c0 set) spends no endpoint on black, so
        // selector 0 is black regardless of what the endpoints hold.
        let block = atc_block(0x8000 | 0x7FFF, 0xFFFF, 0b00_00_00_00);
        let out = decode_compressed_to_rgba(&block, 4, 1, GL_ATC_RGB_AMD).unwrap();
        assert_eq!(&out[0..4], &[0, 0, 0, 255]);
    }

    #[test]
    fn atc_explicit_alpha_reads_one_nibble_per_texel() {
        let mut block = [0u8; 16];
        // Texel 0 alpha = 0x0, texel 1 = 0xF, texel 2 = 0x8.
        block[0] = 0xF0;
        block[1] = 0x08;
        block[8..16].copy_from_slice(&atc_block(0x7FFF, 0x7FFF, 0));
        let out = decode_compressed_to_rgba(&block, 4, 1, GL_ATC_RGBA_EXPLICIT_ALPHA_AMD).unwrap();
        assert_eq!(out[3], 0x00);
        assert_eq!(out[7], 0xFF);
        // 0x8 replicated to eight bits is 0x88, not 0x80 — the same
        // reasoning as `expand5`.
        assert_eq!(out[11], 0x88);
    }

    #[test]
    fn atc_rejects_short_data_rather_than_panicking() {
        // A guest that passes a wrong `imageSize` must get None, not an
        // out-of-bounds slice.
        let short = [0u8; 4];
        assert!(decode_compressed_to_rgba(&short, 4, 4, GL_ATC_RGB_AMD).is_none());
    }

    #[test]
    fn atc_drops_texels_that_hang_past_the_edge() {
        // A 2x2 image still costs one whole block; the twelve
        // overhanging texels must not be written anywhere.
        let block = atc_block(0x7FFF, 0x7FFF, 0);
        let out = decode_compressed_to_rgba(&block, 2, 2, GL_ATC_RGB_AMD).unwrap();
        assert_eq!(out.len(), 2 * 2 * 4);
        assert!(out.chunks(4).all(|p| p == [255, 255, 255, 255]));
    }

    #[test]
    fn dxt1_opaque_block_decodes_rgb() {
        let mut block = [0u8; 8];
        block[0..2].copy_from_slice(&0xF800u16.to_le_bytes());
        block[2..4].copy_from_slice(&0x001Fu16.to_le_bytes());
        block[4..8].fill(0);
        let out =
            decode_compressed_to_rgba(&block, 4, 4, GL_COMPRESSED_RGBA_S3TC_DXT1_EXT).unwrap();
        assert_eq!(&out[0..4], &[255, 0, 0, 255]);
        assert_eq!(&out[4..8], &[255, 0, 0, 255]);
    }

    #[test]
    fn dxt1_transparent_selector_has_zero_alpha() {
        let mut block = [0u8; 8];
        block[0..2].copy_from_slice(&0x001Fu16.to_le_bytes());
        block[2..4].copy_from_slice(&0x001Fu16.to_le_bytes());
        block[4..8].fill(0xFF);
        let out =
            decode_compressed_to_rgba(&block, 4, 4, GL_COMPRESSED_RGBA_S3TC_DXT1_EXT).unwrap();
        assert_eq!(&out[0..4], &[0, 0, 0, 0]);
    }
}
