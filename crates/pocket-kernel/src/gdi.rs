//! Stateful GDI object table.
//!
//! Win32 GDI exposes `HDC`, `HBITMAP`, `HBRUSH`, `HPEN`, `HFONT` as
//! opaque handles. PocketHLE used to return a fake non-zero handle
//! per `Create*` call and never tracked any of them; calls to the
//! actual rendering primitives were therefore no-ops. This module
//! adds a minimal but real implementation of the parts that
//! JumpyBall (and most equivalent Pocket PC games) exercise:
//!
//! * Memory device contexts that own a back-buffer bitmap.
//! * `CreateCompatibleBitmap` allocates a 16 bpp surface so that a
//!   subsequent `BitBlt` between the memory DC and the screen DC is
//!   a straight 1:1 copy.
//! * `CreateSolidBrush` / `CreatePen` colour values get tracked per
//!   handle, then per-DC when `SelectObject` ties them together.
//!
//! The rendering primitives themselves live in
//! [`pocket-winceapi`](../../pocket-winceapi); this module only
//! manages the **data**.

use std::collections::HashMap;

use crate::framebuffer::Framebuffer;

/// Minimum non-zero handle used for GDI objects. Picked to look
/// obviously synthetic in logs.
pub const GDI_HANDLE_BASE: u32 = 0xDEAD_1000;

/// Pseudo-handle returned by `GetDC(NULL)` etc. — represents the
/// hardware screen.
pub const GDI_SCREEN_DC: u32 = 0xDEAD_0DC0;
/// Stock white brush handle (matches `GetStockObject(WHITE_BRUSH)`).
pub const STOCK_WHITE_BRUSH: u32 = 0xDEAD_5701;
pub const STOCK_BLACK_BRUSH: u32 = 0xDEAD_5704;
pub const STOCK_NULL_BRUSH: u32 = 0xDEAD_5705;
pub const STOCK_BLACK_PEN: u32 = 0xDEAD_5707;
pub const STOCK_WHITE_PEN: u32 = 0xDEAD_5706;
pub const STOCK_NULL_PEN: u32 = 0xDEAD_5708;

/// Whether a DC paints into the on-screen framebuffer or into an
/// off-screen [`Bitmap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DcSurface {
    /// Paints into [`Framebuffer`].
    Screen,
    /// Paints into the bitmap with this handle, if one is selected.
    Memory,
}

#[derive(Debug, Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    /// Bits per pixel of the *original* DIB. Internally we always
    /// keep an RGB565 copy in [`Bitmap::pixels`], but DIB-backed
    /// bitmaps remember their original depth so callers like
    /// `GetObjectW` can report the correct value.
    pub bpp: u16,
    /// 16 bpp RGB565 little-endian. `width * 2` is the row stride.
    /// For DIB-backed bitmaps this is a synced copy of the guest's
    /// pixel buffer at [`Bitmap::dib_bits_va`] — `BitBlt` re-reads
    /// the guest VA on demand to stay current.
    pub pixels: Vec<u8>,
    /// If `Some`, this bitmap was created via `CreateDIBSection` and
    /// the guest can write its pixels directly into the mapped guest
    /// VA at `dib_bits_va`. Source-side `BitBlt`s and `GetObjectW`
    /// pull from this address. `dib_bpp` records the original bit
    /// depth so we can decode 8-bpp palette formats etc.
    pub dib_bits_va: Option<u32>,
    /// DIB palette table, in RGB565. Empty for non-paletted DIBs.
    pub dib_palette: Vec<u16>,
    /// Whether the original DIB is bottom-up (Windows default) or
    /// top-down. Bottom-up DIBs need to be flipped on read.
    pub dib_bottom_up: bool,
    /// Stride in bytes of one row of the **DIB** layout (already
    /// padded to a 4-byte boundary). 0 for non-DIB bitmaps.
    pub dib_row_stride: u32,
}

impl Bitmap {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
            bpp: 16,
            pixels: vec![0u8; (width.max(1) * height.max(1) * 2) as usize],
            dib_bits_va: None,
            dib_palette: Vec::new(),
            dib_bottom_up: false,
            dib_row_stride: 0,
        }
    }

    /// Construct a DIB-backed bitmap. The host-side [`Bitmap::pixels`]
    /// buffer is allocated empty; callers (i.e. `BitBlt` source path)
    /// are expected to refresh it from the guest pixel store via
    /// [`Bitmap::sync_from_dib`] before reading.
    pub fn new_dib(
        width: u32,
        height: u32,
        bpp: u16,
        bits_va: u32,
        row_stride: u32,
        bottom_up: bool,
        palette: Vec<u16>,
    ) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
            bpp,
            pixels: vec![0u8; (width.max(1) * height.max(1) * 2) as usize],
            dib_bits_va: Some(bits_va),
            dib_palette: palette,
            dib_bottom_up: bottom_up,
            dib_row_stride: row_stride,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dc {
    pub surface: DcSurface,
    pub selected_bitmap: Option<u32>,
    pub brush_color: u32,
    pub pen_color: u32,
    pub text_color: u32,
    pub bk_color: u32,
    pub bk_transparent: bool,
}

impl Default for Dc {
    fn default() -> Self {
        Self {
            surface: DcSurface::Memory,
            selected_bitmap: None,
            brush_color: 0x00ff_ffff,
            pen_color: 0,
            text_color: 0,
            bk_color: 0x00ff_ffff,
            bk_transparent: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Brush {
    pub color: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Pen {
    pub color: u32,
    pub width: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Font {
    pub height: i32,
}

#[derive(Debug, Clone)]
pub enum GdiObject {
    Dc(Dc),
    Bitmap(Bitmap),
    Brush(Brush),
    Pen(Pen),
    Font(Font),
}

#[derive(Debug, Default)]
pub struct GdiState {
    objects: HashMap<u32, GdiObject>,
    next_handle: u32,
}

impl GdiState {
    pub fn new() -> Self {
        let mut s = Self {
            objects: HashMap::new(),
            next_handle: GDI_HANDLE_BASE,
        };
        // Pre-register stock objects so SelectObject(GetStockObject(...))
        // resolves through the same code path as user-created handles.
        s.objects.insert(
            STOCK_WHITE_BRUSH,
            GdiObject::Brush(Brush { color: 0x00ff_ffff }),
        );
        s.objects
            .insert(STOCK_BLACK_BRUSH, GdiObject::Brush(Brush { color: 0 }));
        s.objects.insert(
            STOCK_NULL_BRUSH,
            GdiObject::Brush(Brush { color: 0xff00_0000 }),
        );
        s.objects
            .insert(STOCK_BLACK_PEN, GdiObject::Pen(Pen { color: 0, width: 1 }));
        s.objects.insert(
            STOCK_WHITE_PEN,
            GdiObject::Pen(Pen {
                color: 0x00ff_ffff,
                width: 1,
            }),
        );
        s.objects.insert(
            STOCK_NULL_PEN,
            GdiObject::Pen(Pen {
                color: 0xff00_0000,
                width: 0,
            }),
        );
        // Screen DC is also pre-registered so that get_screen_dc()
        // returns a stable handle whose surface is `Screen`.
        s.objects.insert(
            GDI_SCREEN_DC,
            GdiObject::Dc(Dc {
                surface: DcSurface::Screen,
                ..Default::default()
            }),
        );
        s
    }

    fn alloc_handle(&mut self) -> u32 {
        let h = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        // Avoid stomping on the stock handles by stepping over them.
        debug_assert!(
            !matches!(
                h,
                GDI_SCREEN_DC
                    | STOCK_WHITE_BRUSH
                    | STOCK_BLACK_BRUSH
                    | STOCK_NULL_BRUSH
                    | STOCK_BLACK_PEN
                    | STOCK_WHITE_PEN
                    | STOCK_NULL_PEN
            ),
            "alloc_handle collided with a stock handle"
        );
        h
    }

    pub fn create_memory_dc(&mut self) -> u32 {
        let h = self.alloc_handle();
        self.objects.insert(h, GdiObject::Dc(Dc::default()));
        h
    }

    pub fn create_compatible_bitmap(&mut self, width: u32, height: u32) -> u32 {
        let h = self.alloc_handle();
        self.objects
            .insert(h, GdiObject::Bitmap(Bitmap::new(width, height)));
        h
    }

    /// Register a DIB-backed bitmap. The pixel storage lives in guest
    /// memory at `bits_va`; we keep [`Bitmap`] metadata (palette,
    /// width, etc.) host-side so `BitBlt` can render through it.
    pub fn register_dib(&mut self, bitmap: Bitmap) -> u32 {
        let h = self.alloc_handle();
        self.objects.insert(h, GdiObject::Bitmap(bitmap));
        h
    }

    pub fn create_solid_brush(&mut self, color: u32) -> u32 {
        let h = self.alloc_handle();
        self.objects.insert(h, GdiObject::Brush(Brush { color }));
        h
    }

    pub fn create_pen(&mut self, color: u32, width: u32) -> u32 {
        let h = self.alloc_handle();
        self.objects.insert(h, GdiObject::Pen(Pen { color, width }));
        h
    }

    pub fn create_font(&mut self, height: i32) -> u32 {
        let h = self.alloc_handle();
        self.objects.insert(h, GdiObject::Font(Font { height }));
        h
    }

    pub fn delete(&mut self, handle: u32) -> bool {
        // Stock objects are immortal.
        if matches!(
            handle,
            GDI_SCREEN_DC
                | STOCK_WHITE_BRUSH
                | STOCK_BLACK_BRUSH
                | STOCK_NULL_BRUSH
                | STOCK_BLACK_PEN
                | STOCK_WHITE_PEN
                | STOCK_NULL_PEN
        ) {
            return true;
        }
        self.objects.remove(&handle).is_some()
    }

    pub fn get(&self, handle: u32) -> Option<&GdiObject> {
        self.objects.get(&handle)
    }

    pub fn get_mut(&mut self, handle: u32) -> Option<&mut GdiObject> {
        self.objects.get_mut(&handle)
    }

    pub fn dc(&self, handle: u32) -> Option<&Dc> {
        match self.objects.get(&handle)? {
            GdiObject::Dc(d) => Some(d),
            _ => None,
        }
    }

    pub fn dc_mut(&mut self, handle: u32) -> Option<&mut Dc> {
        match self.objects.get_mut(&handle)? {
            GdiObject::Dc(d) => Some(d),
            _ => None,
        }
    }

    pub fn bitmap(&self, handle: u32) -> Option<&Bitmap> {
        match self.objects.get(&handle)? {
            GdiObject::Bitmap(b) => Some(b),
            _ => None,
        }
    }

    pub fn bitmap_mut(&mut self, handle: u32) -> Option<&mut Bitmap> {
        match self.objects.get_mut(&handle)? {
            GdiObject::Bitmap(b) => Some(b),
            _ => None,
        }
    }

    pub fn brush(&self, handle: u32) -> Option<&Brush> {
        match self.objects.get(&handle)? {
            GdiObject::Brush(b) => Some(b),
            _ => None,
        }
    }

    pub fn pen(&self, handle: u32) -> Option<&Pen> {
        match self.objects.get(&handle)? {
            GdiObject::Pen(p) => Some(p),
            _ => None,
        }
    }

    /// SelectObject semantics: tie `obj_handle` into `dc_handle`.
    /// Returns the previous handle of the same kind (for callers that
    /// want to restore it later), or 0 if no such object existed.
    pub fn select_into(&mut self, dc_handle: u32, obj_handle: u32) -> u32 {
        // Read the object's kind without holding a borrow on self.
        let kind = match self.objects.get(&obj_handle) {
            Some(GdiObject::Bitmap(_)) => "bitmap",
            Some(GdiObject::Brush(_)) => "brush",
            Some(GdiObject::Pen(_)) => "pen",
            Some(GdiObject::Font(_)) => "font",
            _ => return 0,
        };
        // For brushes and pens, store the colour straight onto the DC
        // so primitive draws don't need a second lookup.
        let color = match self.objects.get(&obj_handle) {
            Some(GdiObject::Brush(b)) => Some(b.color),
            Some(GdiObject::Pen(p)) => Some(p.color),
            _ => None,
        };
        let dc = match self.dc_mut(dc_handle) {
            Some(d) => d,
            None => return 0,
        };
        match kind {
            "bitmap" => {
                let prev = dc.selected_bitmap.unwrap_or(0);
                dc.selected_bitmap = Some(obj_handle);
                prev
            }
            "brush" => {
                let prev_color = dc.brush_color;
                if let Some(c) = color {
                    dc.brush_color = c;
                }
                prev_color
            }
            "pen" => {
                let prev_color = dc.pen_color;
                if let Some(c) = color {
                    dc.pen_color = c;
                }
                prev_color
            }
            _ => 0,
        }
    }
}

/// Borrow either the framebuffer or a memory bitmap as a writable
/// surface.
pub enum Surface<'a> {
    Screen(&'a mut Framebuffer),
    Bitmap(&'a mut Bitmap),
}

impl<'a> Surface<'a> {
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            Surface::Screen(fb) => (fb.width, fb.height),
            Surface::Bitmap(bm) => (bm.width, bm.height),
        }
    }

    pub fn pixels_mut(&mut self) -> &mut [u8] {
        match self {
            Surface::Screen(fb) => &mut fb.pixels,
            Surface::Bitmap(bm) => &mut bm.pixels,
        }
    }

    pub fn pixels(&self) -> &[u8] {
        match self {
            Surface::Screen(fb) => &fb.pixels,
            Surface::Bitmap(bm) => &bm.pixels,
        }
    }

    pub fn mark_dirty(&mut self) {
        if let Surface::Screen(fb) = self {
            fb.mark_dirty();
        }
    }

    pub fn put_pixel(&mut self, x: i32, y: i32, color: u16) {
        let (sw, sh) = self.dimensions();
        if x < 0 || y < 0 || (x as u32) >= sw || (y as u32) >= sh {
            return;
        }
        let off = (y as u32 * sw + x as u32) as usize * 2;
        let bytes = color.to_le_bytes();
        let pix = self.pixels_mut();
        pix[off] = bytes[0];
        pix[off + 1] = bytes[1];
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u16) {
        let (sw, sh) = self.dimensions();
        if w <= 0 || h <= 0 {
            return;
        }
        let x0 = x.max(0).min(sw as i32) as u32;
        let y0 = y.max(0).min(sh as i32) as u32;
        let x1 = (x + w).max(0).min(sw as i32) as u32;
        let y1 = (y + h).max(0).min(sh as i32) as u32;
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        // Reinterpret the surface as `[u16]` so each row collapses
        // into one `slice::fill`. The naive per-byte loop became a
        // hot spot in profiles of games that issue many `FillRect`
        // calls per frame to clear the background.
        let pix = self.pixels_mut();
        let words: &mut [u16] = bytemuck::cast_slice_mut(pix);
        let stride = sw as usize;
        let row_len = (x1 - x0) as usize;
        let color_le = color.to_le();
        for row in y0..y1 {
            let off = row as usize * stride + x0 as usize;
            words[off..off + row_len].fill(color_le);
        }
        self.mark_dirty();
    }

    pub fn stroke_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u16) {
        if w <= 0 || h <= 0 {
            return;
        }
        let bytes = color.to_le_bytes();
        let put = |this: &mut Self, px: i32, py: i32| {
            let (sw, sh) = this.dimensions();
            if px < 0 || py < 0 || (px as u32) >= sw || (py as u32) >= sh {
                return;
            }
            let off = (py as u32 * sw + px as u32) as usize * 2;
            let pix = this.pixels_mut();
            pix[off] = bytes[0];
            pix[off + 1] = bytes[1];
        };
        for i in 0..w {
            put(self, x + i, y);
            put(self, x + i, y + h - 1);
        }
        for i in 0..h {
            put(self, x, y + i);
            put(self, x + w - 1, y + i);
        }
        self.mark_dirty();
    }

    /// Copy a rectangle from `src` into `(dx, dy)` of this surface.
    #[allow(clippy::too_many_arguments)]
    pub fn blit_from_bytes(
        &mut self,
        dx: i32,
        dy: i32,
        sx: i32,
        sy: i32,
        w: i32,
        h: i32,
        src: &[u8],
        src_w: u32,
        src_h: u32,
    ) {
        if w <= 0 || h <= 0 || src_w == 0 || src_h == 0 {
            return;
        }
        // Clip source to its own bounds first.
        let sx0 = sx.max(0).min(src_w as i32) as u32;
        let sy0 = sy.max(0).min(src_h as i32) as u32;
        let sx1 = (sx + w).max(0).min(src_w as i32) as u32;
        let sy1 = (sy + h).max(0).min(src_h as i32) as u32;
        if sx0 >= sx1 || sy0 >= sy1 {
            return;
        }
        let dest_x0 = dx + (sx0 as i32 - sx);
        let dest_y0 = dy + (sy0 as i32 - sy);
        let copy_w = (sx1 - sx0) as i32;
        let copy_h = (sy1 - sy0) as i32;

        let (dw, dh) = self.dimensions();
        let dx0 = dest_x0.max(0).min(dw as i32) as u32;
        let dy0 = dest_y0.max(0).min(dh as i32) as u32;
        let dx1 = (dest_x0 + copy_w).max(0).min(dw as i32) as u32;
        let dy1 = (dest_y0 + copy_h).max(0).min(dh as i32) as u32;
        if dx0 >= dx1 || dy0 >= dy1 {
            return;
        }
        let dst_stride = dw as usize * 2;
        let src_stride = src_w as usize * 2;
        let row_bytes = (dx1 - dx0) as usize * 2;
        let src_skip_x = dx0 as i32 - dest_x0;
        let src_skip_y = dy0 as i32 - dest_y0;
        let src_x0 = sx0 as i32 + src_skip_x;
        let src_y0 = sy0 as i32 + src_skip_y;
        let pix = self.pixels_mut();
        for row in 0..(dy1 - dy0) as i32 {
            let dst_off = (dy0 as i32 + row) as usize * dst_stride + dx0 as usize * 2;
            let src_off = (src_y0 + row) as usize * src_stride + src_x0 as usize * 2;
            if src_off + row_bytes > src.len() || dst_off + row_bytes > pix.len() {
                continue;
            }
            pix[dst_off..dst_off + row_bytes].copy_from_slice(&src[src_off..src_off + row_bytes]);
        }
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_select_brush() {
        let mut g = GdiState::new();
        let dc = g.create_memory_dc();
        let brush = g.create_solid_brush(0x00_22_44_66);
        let prev = g.select_into(dc, brush);
        assert_ne!(prev, 0);
        assert_eq!(g.dc(dc).unwrap().brush_color, 0x00_22_44_66);
    }

    #[test]
    fn delete_removes_object() {
        let mut g = GdiState::new();
        let p = g.create_pen(0x00_ff_ff_ff, 1);
        assert!(g.delete(p));
        assert!(g.get(p).is_none());
    }

    #[test]
    fn stock_objects_are_immortal() {
        let mut g = GdiState::new();
        assert!(g.delete(STOCK_WHITE_BRUSH));
        assert!(g.brush(STOCK_WHITE_BRUSH).is_some());
    }

    #[test]
    fn fill_rect_on_bitmap() {
        let mut g = GdiState::new();
        let bm_h = g.create_compatible_bitmap(4, 4);
        {
            let bm = g.bitmap_mut(bm_h).unwrap();
            let mut surf = Surface::Bitmap(bm);
            surf.fill_rect(0, 0, 4, 4, 0xf800); // pure red in RGB565
        }
        let bm = g.bitmap(bm_h).unwrap();
        assert_eq!(bm.pixels[0], 0x00);
        assert_eq!(bm.pixels[1], 0xf8);
    }
}
