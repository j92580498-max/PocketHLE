//! SDL 1.2 WinCE named-import HLE: software surfaces, input and timing.
//! This is an initial subset, not SDL2 or full SDL 1.2 (audio/GL/RWops absent).
use crate::{CallCtx, Handler, WinCeDispatcher};
use pocket_kernel::{sdl::Surface, DispatchOutcome, InputEvent, KernelError};
const VIDEO: u32 = 0x20;
const TIMER: u32 = 1;
const COLORKEY: u32 = 0x1000;
const SRCALPHA: u32 = 0x10000;
fn ret(value: u32) -> Result<DispatchOutcome, KernelError> {
    Ok(DispatchOutcome::ReturnedR0(value))
}
fn word(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}
fn put(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn fail(ctx: &mut CallCtx<'_>, message: &str, value: u32) -> Result<DispatchOutcome, KernelError> {
    ctx.kernel.sdl.error = message.into();
    ret(value)
}
fn alloc(ctx: &mut CallCtx<'_>, bytes: u32) -> Result<u32, KernelError> {
    let Some(ptr) = ctx.kernel.heap.alloc(bytes) else {
        return Ok(0);
    };
    if let Err(error) = ctx.cpu.write_mem(ptr, &vec![0; bytes as usize]) {
        ctx.kernel.heap.free(ptr);
        return Err(error.into());
    }
    Ok(ptr)
}
pub fn register(d: &mut WinCeDispatcher) {
    let handlers: &[(&str, Handler)] = &[
        ("SDL_Init", init),
        ("SDL_InitSubSystem", init),
        ("SDL_WasInit", was_init),
        ("SDL_Quit", quit),
        ("SDL_QuitSubSystem", quit_subsystem),
        ("SDL_GetTicks", crate::coredll::get_tick_count),
        ("SDL_Delay", crate::coredll::sleep),
        ("SDL_GetError", get_error),
        ("SDL_ClearError", clear_error),
        ("SDL_Linked_Version", version),
        ("SDL_SetVideoMode", set_video),
        ("SDL_VideoModeOK", mode_ok),
        ("SDL_GetVideoSurface", get_screen),
        ("SDL_CreateRGBSurface", create_surface),
        ("SDL_FreeSurface", free_surface),
        ("SDL_LockSurface", lock),
        ("SDL_UnlockSurface", unlock),
        ("SDL_Flip", flip),
        ("SDL_UpdateRect", flip),
        ("SDL_UpdateRects", flip),
        ("SDL_FillRect", fill),
        ("SDL_UpperBlit", blit),
        ("SDL_MapRGB", map_rgb),
        ("SDL_MapRGBA", map_rgba),
        ("SDL_SetColorKey", color_key),
        ("SDL_SetAlpha", alpha),
        ("SDL_SetClipRect", set_clip),
        ("SDL_GetClipRect", get_clip),
        ("SDL_PumpEvents", pump),
        ("SDL_PollEvent", poll),
        ("SDL_PushEvent", push),
        ("SDL_GetKeyState", keys),
    ];
    for &(name, handler) in handlers {
        d.register_handler("sdl.dll", name, handler);
    }
}
fn init(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let flags = ctx.arg_u32(0)? & !0x00100000;
    if flags & !(VIDEO | TIMER) != 0 {
        return fail(
            ctx,
            "SDL subsystem not supported (video/timer only)",
            u32::MAX,
        );
    }
    ctx.kernel.sdl.initialized |= flags;
    ret(0)
}
fn was_init(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let flags = ctx.arg_u32(0)?;
    ret(ctx.kernel.sdl.initialized & if flags == 0 { u32::MAX } else { flags })
}
fn release(ctx: &mut CallCtx<'_>, ptr: u32) {
    if let Some(surface) = ctx.kernel.sdl.surfaces.remove(&ptr) {
        for p in [surface.pixels, surface.format, ptr] {
            ctx.kernel.heap.free(p)
        }
    }
    if ctx.kernel.sdl.screen == ptr {
        ctx.kernel.sdl.screen = 0
    }
}
fn quit_subsystem(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let flags = ctx.arg_u32(0)?;
    ctx.kernel.sdl.initialized &= !flags;
    if flags & VIDEO != 0 {
        release(ctx, ctx.kernel.sdl.screen);
    }
    ret(0)
}
fn quit(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    release(ctx, ctx.kernel.sdl.screen);
    ctx.kernel.sdl.initialized = 0;
    ctx.kernel.sdl.events.clear();
    ctx.kernel.sdl.keys.fill(0);
    // User-owned offscreen surfaces and static query pointers survive SDL_Quit.
    if ctx.kernel.sdl.key_ptr != 0 {
        ctx.cpu
            .write_mem(ctx.kernel.sdl.key_ptr, &ctx.kernel.sdl.keys)?;
    }
    ret(0)
}
fn get_error(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.sdl.error_ptr == 0 {
        ctx.kernel.sdl.error_ptr = alloc(ctx, 256)?;
    }
    let ptr = ctx.kernel.sdl.error_ptr;
    if ptr != 0 {
        let text = ctx.kernel.sdl.error.as_bytes();
        let mut data = text[..text.len().min(255)].to_vec();
        data.push(0);
        ctx.cpu.write_mem(ptr, &data)?;
    }
    ret(ptr)
}
fn clear_error(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ctx.kernel.sdl.error.clear();
    ret(0)
}
fn version(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    if ctx.kernel.sdl.version_ptr == 0 {
        let ptr = alloc(ctx, 4)?;
        if ptr != 0 {
            ctx.cpu.write_mem(ptr, &[1, 2, 15, 0])?;
        }
        ctx.kernel.sdl.version_ptr = ptr;
    }
    ret(ctx.kernel.sdl.version_ptr)
}
fn mode(width: u32, height: u32, depth: u32, flags: u32) -> Option<u32> {
    if width == 0 || height == 0 || width > 1024 || height > 1024 || flags & 2 != 0 {
        return None;
    }
    let depth = if depth == 0 { 16 } else { depth };
    matches!(depth, 16 | 24 | 32).then_some(depth)
}
fn mode_ok(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ret(mode(
        ctx.arg_u32(0)?,
        ctx.arg_u32(1)?,
        ctx.arg_u32(2)?,
        ctx.arg_u32(3)?,
    )
    .unwrap_or(0))
}
fn get_screen(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    ret(ctx.kernel.sdl.screen)
}
fn defaults(depth: u32) -> [u32; 4] {
    if depth == 16 {
        [0xf800, 0x7e0, 0x1f, 0]
    } else {
        [0xff0000, 0xff00, 0xff, 0]
    }
}
fn new_surface(
    ctx: &mut CallCtx<'_>,
    w: u32,
    h: u32,
    depth: u32,
    masks: [u32; 4],
) -> Result<u32, KernelError> {
    if ctx.kernel.sdl.surfaces.len() >= 128 {
        return Ok(0);
    }
    let bytes = depth / 8;
    let pitch = (w * bytes + 3) & !3;
    let mut seen = 0;
    for mask in masks {
        if mask != 0 {
            let bits = mask >> mask.trailing_zeros();
            if bits & bits.wrapping_add(1) != 0
                || mask & seen != 0
                || (depth < 32 && mask >> depth != 0)
            {
                return Ok(0);
            }
            seen |= mask;
        }
    }
    let header = alloc(ctx, 60)?;
    if header == 0 {
        return Ok(0);
    }
    let format = alloc(ctx, 40)?;
    if format == 0 {
        ctx.kernel.heap.free(header);
        return Ok(0);
    }
    let pixels = alloc(ctx, pitch * h)?;
    if pixels == 0 {
        ctx.kernel.heap.free(header);
        ctx.kernel.heap.free(format);
        return Ok(0);
    }
    let mut f = [0u8; 40];
    f[4] = depth as u8;
    f[5] = bytes as u8;
    f[36] = 255;
    for (i, mask) in masks.into_iter().enumerate() {
        let count = mask.count_ones();
        if count > 8 {
            for p in [header, format, pixels] {
                ctx.kernel.heap.free(p)
            }
            return Ok(0);
        }
        f[6 + i] = (8 - count) as u8;
        f[10 + i] = if mask == 0 {
            0
        } else {
            mask.trailing_zeros() as u8
        };
        put(&mut f, 16 + i * 4, mask);
    }
    ctx.cpu.write_mem(format, &f)?;
    let mut s = [0u8; 60];
    if masks[3] != 0 {
        put(&mut s, 0, SRCALPHA);
    }
    put(&mut s, 4, format);
    put(&mut s, 8, w);
    put(&mut s, 12, h);
    s[16..18].copy_from_slice(&(pitch as u16).to_le_bytes());
    put(&mut s, 20, pixels);
    s[36..38].copy_from_slice(&(w as u16).to_le_bytes());
    s[38..40].copy_from_slice(&(h as u16).to_le_bytes());
    put(&mut s, 56, 1);
    ctx.cpu.write_mem(header, &s)?;
    ctx.kernel.sdl.surfaces.insert(
        header,
        Surface {
            width: w,
            height: h,
            pitch,
            bytes,
            pixels,
            format,
            masks,
        },
    );
    Ok(header)
}
fn set_video(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let w = ctx.arg_u32(0)?;
    let h = ctx.arg_u32(1)?;
    let Some(depth) = mode(w, h, ctx.arg_u32(2)?, ctx.arg_u32(3)?) else {
        return fail(ctx, "Unsupported SDL video mode", 0);
    };
    if ctx.kernel.sdl.initialized & VIDEO == 0 {
        return fail(ctx, "SDL video is not initialized", 0);
    }
    let ptr = new_surface(ctx, w, h, depth, defaults(depth))?;
    if ptr == 0 {
        return fail(ctx, "Could not allocate SDL screen", 0);
    }
    let old = ctx.kernel.sdl.screen;
    ctx.kernel.sdl.screen = ptr;
    release(ctx, old);
    ctx.kernel.framebuffer.resize(w, h);
    ret(ptr)
}
fn create_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let flags = ctx.arg_u32(0)?;
    let w = ctx.arg_u32(1)?;
    let h = ctx.arg_u32(2)?;
    let depth = ctx.arg_u32(3)?;
    if mode(w, h, depth, flags).is_none() || depth == 0 {
        return fail(ctx, "Unsupported SDL surface format", 0);
    }
    let mut masks = [
        ctx.arg_u32(4)?,
        ctx.arg_u32(5)?,
        ctx.arg_u32(6)?,
        ctx.arg_u32(7)?,
    ];
    if masks == [0; 4] {
        masks = defaults(depth)
    }
    let ptr = new_surface(ctx, w, h, depth, masks)?;
    if ptr == 0 {
        return fail(ctx, "Invalid format or SDL surface allocation failed", 0);
    }
    ret(ptr)
}
fn free_surface(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    if ptr != ctx.kernel.sdl.screen && ctx.kernel.sdl.surfaces.contains_key(&ptr) {
        let refs = word(&ctx.cpu.read_mem(ptr, 60)?, 56);
        if refs > 1 {
            ctx.cpu.write_mem(ptr + 56, &(refs - 1).to_le_bytes())?;
        } else {
            release(ctx, ptr);
        }
    }
    ret(0)
}
fn lock(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    if !ctx.kernel.sdl.surfaces.contains_key(&ptr) {
        return fail(ctx, "Invalid SDL surface", u32::MAX);
    }
    let mut data = ctx.cpu.read_mem(ptr, 60)?;
    let n = word(&data, 44).saturating_add(1);
    put(&mut data, 44, n);
    ctx.cpu.write_mem(ptr, &data)?;
    ret(0)
}
fn unlock(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    if ctx.kernel.sdl.surfaces.contains_key(&ptr) {
        let mut data = ctx.cpu.read_mem(ptr, 60)?;
        let n = word(&data, 44).saturating_sub(1);
        put(&mut data, 44, n);
        ctx.cpu.write_mem(ptr, &data)?;
    }
    ret(0)
}
fn component(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let value = (pixel & mask) >> mask.trailing_zeros();
    let max = mask >> mask.trailing_zeros();
    (value * 255 / max) as u8
}
fn pixel(bytes: &[u8]) -> u32 {
    let mut v = [0; 4];
    v[..bytes.len()].copy_from_slice(bytes);
    u32::from_le_bytes(v)
}
fn packed(color: [u8; 4], masks: [u32; 4]) -> u32 {
    color.into_iter().zip(masks).fold(0, |p, (c, m)| {
        if m == 0 {
            p
        } else {
            p | (((c as u32) >> (8 - m.count_ones())) << m.trailing_zeros())
        }
    })
}
fn flip(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    if ptr != ctx.kernel.sdl.screen || ptr == 0 {
        return fail(ctx, "Invalid SDL display surface", u32::MAX);
    }
    let s = ctx.kernel.sdl.surfaces[&ptr].clone();
    let data = ctx.cpu.read_mem(s.pixels, s.pitch * s.height)?;
    if ctx.kernel.framebuffer.width != s.width || ctx.kernel.framebuffer.height != s.height {
        ctx.kernel.framebuffer.resize(s.width, s.height);
    }
    for y in 0..s.height {
        for x in 0..s.width {
            let offset = (y * s.pitch + x * s.bytes) as usize;
            let p = pixel(&data[offset..offset + s.bytes as usize]);
            let r = component(p, s.masks[0]);
            let g = component(p, s.masks[1]);
            let b = component(p, s.masks[2]);
            let out = ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3);
            let i = ((y * s.width + x) * 2) as usize;
            ctx.kernel.framebuffer.pixels[i..i + 2].copy_from_slice(&out.to_le_bytes());
        }
    }
    ctx.kernel.framebuffer.mark_dirty();
    ret(0)
}
fn rectangle(
    ctx: &mut CallCtx<'_>,
    ptr: u32,
    w: u32,
    h: u32,
) -> Result<(i32, i32, i32, i32), KernelError> {
    if ptr == 0 {
        return Ok((0, 0, w as i32, h as i32));
    }
    let b = ctx.cpu.read_mem(ptr, 8)?;
    Ok((
        i16::from_le_bytes(b[0..2].try_into().unwrap()) as i32,
        i16::from_le_bytes(b[2..4].try_into().unwrap()) as i32,
        u16::from_le_bytes(b[4..6].try_into().unwrap()) as i32,
        u16::from_le_bytes(b[6..8].try_into().unwrap()) as i32,
    ))
}
fn clip(ctx: &mut CallCtx<'_>, ptr: u32, s: &Surface) -> Result<(i32, i32, i32, i32), KernelError> {
    rectangle(ctx, ptr + 32, s.width, s.height)
}
fn fill(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let rect = ctx.arg_u32(1)?;
    let color = ctx.arg_u32(2)?;
    let Some(s) = ctx.kernel.sdl.surfaces.get(&ptr).cloned() else {
        return fail(ctx, "Invalid SDL surface", u32::MAX);
    };
    let (x, y, w, h) = rectangle(ctx, rect, s.width, s.height)?;
    let (c, d, cw, ch) = clip(ctx, ptr, &s)?;
    let left = x.max(c).max(0);
    let top = y.max(d).max(0);
    let right = (x + w).min(c + cw).min(s.width as i32);
    let bottom = (y + h).min(d + ch).min(s.height as i32);
    if right > left {
        let p = color.to_le_bytes();
        let row: Vec<_> = (left..right)
            .flat_map(|_| p[..s.bytes as usize].iter().copied())
            .collect();
        for y in top..bottom {
            ctx.cpu
                .write_mem(s.pixels + y as u32 * s.pitch + left as u32 * s.bytes, &row)?;
        }
    }
    ret(0)
}
fn blit(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let src = ctx.arg_u32(0)?;
    let sr = ctx.arg_u32(1)?;
    let dst = ctx.arg_u32(2)?;
    let dr = ctx.arg_u32(3)?;
    let (Some(s), Some(d)) = (
        ctx.kernel.sdl.surfaces.get(&src).cloned(),
        ctx.kernel.sdl.surfaces.get(&dst).cloned(),
    ) else {
        return fail(ctx, "Invalid SDL blit surface", u32::MAX);
    };
    let (sx, sy, w, h) = rectangle(ctx, sr, s.width, s.height)?;
    let (dx, dy, _, _) = rectangle(ctx, dr, d.width, d.height)?;
    let (cx, cy, cw, ch) = clip(ctx, dst, &d)?;
    let first_x = 0.max(-sx).max(cx - dx).max(-dx);
    let first_y = 0.max(-sy).max(cy - dy).max(-dy);
    let end_x = w
        .min(s.width as i32 - sx)
        .min(d.width as i32 - dx)
        .min(cx + cw - dx);
    let end_y = h
        .min(s.height as i32 - sy)
        .min(d.height as i32 - dy)
        .min(cy + ch - dy);
    let source = ctx.cpu.read_mem(s.pixels, s.pitch * s.height)?;
    let mut dest = ctx.cpu.read_mem(d.pixels, d.pitch * d.height)?;
    let header = ctx.cpu.read_mem(src, 60)?;
    let format = ctx.cpu.read_mem(s.format, 40)?;
    let flags = word(&header, 0);
    let key = word(&format, 32);
    for y in first_y..end_y {
        for x in first_x..end_x {
            let si = ((sy + y) as u32 * s.pitch + (sx + x) as u32 * s.bytes) as usize;
            let di = ((dy + y) as u32 * d.pitch + (dx + x) as u32 * d.bytes) as usize;
            let p = pixel(&source[si..si + s.bytes as usize]);
            if flags & COLORKEY != 0 && (flags & SRCALPHA == 0 || s.masks[3] == 0) && p == key {
                continue;
            }
            let mut color = [
                component(p, s.masks[0]),
                component(p, s.masks[1]),
                component(p, s.masks[2]),
                if s.masks[3] == 0 {
                    255
                } else {
                    component(p, s.masks[3])
                },
            ];
            if flags & SRCALPHA != 0 {
                let a = if s.masks[3] == 0 {
                    format[36]
                } else {
                    color[3]
                } as u32;
                let old = pixel(&dest[di..di + d.bytes as usize]);
                for c in 0..3 {
                    color[c] = ((color[c] as u32 * a
                        + component(old, d.masks[c]) as u32 * (255 - a)
                        + 127)
                        / 255) as u8;
                }
                color[3] = if d.masks[3] == 0 {
                    255
                } else {
                    component(old, d.masks[3])
                };
            }
            let out = packed(color, d.masks).to_le_bytes();
            dest[di..di + d.bytes as usize].copy_from_slice(&out[..d.bytes as usize]);
        }
    }
    ctx.cpu.write_mem(d.pixels, &dest)?;
    if dr != 0 {
        let mut rect = [0; 8];
        rect[0..2].copy_from_slice(&((dx + first_x) as i16).to_le_bytes());
        rect[2..4].copy_from_slice(&((dy + first_y) as i16).to_le_bytes());
        rect[4..6].copy_from_slice(&((end_x - first_x).max(0) as u16).to_le_bytes());
        rect[6..8].copy_from_slice(&((end_y - first_y).max(0) as u16).to_le_bytes());
        ctx.cpu.write_mem(dr, &rect)?;
    }
    ret(0)
}
fn map(ctx: &mut CallCtx<'_>, alpha: u8) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let Some(masks) = ctx
        .kernel
        .sdl
        .surfaces
        .values()
        .find(|surface| surface.format == ptr)
        .map(|surface| surface.masks)
    else {
        return fail(ctx, "Invalid SDL pixel format", 0);
    };
    ret(packed(
        [
            ctx.arg_u32(1)? as u8,
            ctx.arg_u32(2)? as u8,
            ctx.arg_u32(3)? as u8,
            alpha,
        ],
        masks,
    ))
}
fn map_rgb(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    map(ctx, 255)
}
fn map_rgba(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let alpha = ctx.arg_u32(4)? as u8;
    map(ctx, alpha)
}
fn color_key(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let flags = ctx.arg_u32(1)?;
    let value = ctx.arg_u32(2)?;
    let Some(s) = ctx.kernel.sdl.surfaces.get(&ptr).cloned() else {
        return fail(ctx, "Invalid SDL surface", u32::MAX);
    };
    let mut h = ctx.cpu.read_mem(ptr, 60)?;
    let f = word(&h, 0) & !COLORKEY | flags & COLORKEY;
    put(&mut h, 0, f);
    ctx.cpu.write_mem(ptr, &h)?;
    ctx.cpu.write_mem(s.format + 32, &value.to_le_bytes())?;
    ret(0)
}
fn alpha(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let flags = ctx.arg_u32(1)?;
    let value = ctx.arg_u32(2)? as u8;
    let Some(s) = ctx.kernel.sdl.surfaces.get(&ptr).cloned() else {
        return fail(ctx, "Invalid SDL surface", u32::MAX);
    };
    let mut h = ctx.cpu.read_mem(ptr, 60)?;
    let f = word(&h, 0) & !SRCALPHA | flags & SRCALPHA;
    put(&mut h, 0, f);
    ctx.cpu.write_mem(ptr, &h)?;
    ctx.cpu.write_mem(s.format + 36, &[value])?;
    ret(0)
}
fn set_clip(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let rect = ctx.arg_u32(1)?;
    let Some(s) = ctx.kernel.sdl.surfaces.get(&ptr).cloned() else {
        return fail(ctx, "Invalid SDL surface", 0);
    };
    let (x, y, w, h) = rectangle(ctx, rect, s.width, s.height)?;
    let left = x.max(0).min(s.width as i32);
    let top = y.max(0).min(s.height as i32);
    let width = (x + w).min(s.width as i32).saturating_sub(left).max(0);
    let height = (y + h).min(s.height as i32).saturating_sub(top).max(0);
    let mut data = [0; 8];
    data[..2].copy_from_slice(&(left as i16).to_le_bytes());
    data[2..4].copy_from_slice(&(top as i16).to_le_bytes());
    data[4..6].copy_from_slice(&(width as u16).to_le_bytes());
    data[6..8].copy_from_slice(&(height as u16).to_le_bytes());
    ctx.cpu.write_mem(ptr + 32, &data)?;
    ret(u32::from(width > 0 && height > 0))
}
fn get_clip(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    let out = ctx.arg_u32(1)?;
    if ctx.kernel.sdl.surfaces.contains_key(&ptr) && out != 0 {
        let rect = ctx.cpu.read_mem(ptr + 32, 8)?;
        ctx.cpu.write_mem(out, &rect)?;
    }
    ret(0)
}
fn symbol(vk: u16) -> u32 {
    match vk {
        0x25 => 276,
        0x26 => 273,
        0x27 => 275,
        0x28 => 274,
        0x41..=0x5a => (vk + 32) as u32,
        0x70..=0x7e => (vk - 0x70 + 282) as u32,
        0x10 => 304,
        0x11 => 306,
        0x12 => 308,
        _ if vk < 128 => vk as u32,
        _ => 0,
    }
}
fn pump_input(ctx: &mut CallCtx<'_>) -> Result<(), KernelError> {
    while ctx.kernel.sdl.events.len() < 1024 {
        let Some(input) = ctx.kernel.pending_input.pop_front() else {
            break;
        };
        let mut event = [0; 20];
        match input {
            InputEvent::KeyDown { vk } | InputEvent::KeyUp { vk } => {
                let down = matches!(input, InputEvent::KeyDown { .. });
                let sym = symbol(vk);
                event[0] = if down { 2 } else { 3 };
                event[2] = u8::from(down);
                event[4] = vk as u8;
                put(&mut event, 8, sym);
                if sym != 0 {
                    ctx.kernel.sdl.keys[sym as usize] = u8::from(down);
                }
                if vk < 256 {
                    ctx.kernel.pressed_keys[vk as usize] = down;
                }
                let mut modifiers = 0;
                if ctx.kernel.pressed_keys[16] {
                    modifiers |= 1
                }
                if ctx.kernel.pressed_keys[17] {
                    modifiers |= 0x40
                }
                if ctx.kernel.pressed_keys[18] {
                    modifiers |= 0x100
                }
                put(&mut event, 12, modifiers);
            }
            InputEvent::PointerDown { x, y } | InputEvent::PointerUp { x, y } => {
                let down = matches!(input, InputEvent::PointerDown { .. });
                event[0] = if down { 5 } else { 6 };
                event[2] = 1;
                event[3] = u8::from(down);
                event[4..6].copy_from_slice(&x.to_le_bytes());
                event[6..8].copy_from_slice(&y.to_le_bytes());
                ctx.kernel.sdl.mouse = (x, y, down);
            }
            InputEvent::PointerMove { x, y } => {
                event[0] = 4;
                event[2] = u8::from(ctx.kernel.sdl.mouse.2);
                event[4..6].copy_from_slice(&x.to_le_bytes());
                event[6..8].copy_from_slice(&y.to_le_bytes());
                event[8..10].copy_from_slice(
                    &(x as i32 - ctx.kernel.sdl.mouse.0 as i32)
                        .clamp(i16::MIN as i32, i16::MAX as i32)
                        .to_le_bytes()[..2],
                );
                event[10..12].copy_from_slice(
                    &(y as i32 - ctx.kernel.sdl.mouse.1 as i32)
                        .clamp(i16::MIN as i32, i16::MAX as i32)
                        .to_le_bytes()[..2],
                );
                ctx.kernel.sdl.mouse.0 = x;
                ctx.kernel.sdl.mouse.1 = y;
            }
        }
        ctx.kernel.sdl.events.push_back(event);
    }
    if ctx.kernel.sdl.key_ptr != 0 {
        ctx.cpu
            .write_mem(ctx.kernel.sdl.key_ptr, &ctx.kernel.sdl.keys)?;
    }
    Ok(())
}
fn pump(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    pump_input(ctx)?;
    ret(0)
}
fn poll(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    pump_input(ctx)?;
    if ptr == 0 {
        return ret(u32::from(!ctx.kernel.sdl.events.is_empty()));
    }
    if let Some(event) = ctx.kernel.sdl.events.front() {
        ctx.cpu.write_mem(ptr, event)?;
        ctx.kernel.sdl.events.pop_front();
        ret(1)
    } else {
        ret(0)
    }
}
fn push(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let ptr = ctx.arg_u32(0)?;
    if ptr == 0 || ctx.kernel.sdl.events.len() >= 1024 {
        return fail(ctx, "SDL event queue full or invalid event", u32::MAX);
    }
    let event = ctx.cpu.read_mem(ptr, 20)?;
    if event[0] >= 32 {
        return fail(ctx, "Invalid SDL event type", u32::MAX);
    }
    ctx.kernel.sdl.events.push_back(event.try_into().unwrap());
    ret(0)
}
fn keys(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let count = ctx.arg_u32(0)?;
    if count != 0 {
        ctx.cpu.write_mem(count, &323u32.to_le_bytes())?;
    }
    if ctx.kernel.sdl.key_ptr == 0 {
        ctx.kernel.sdl.key_ptr = alloc(ctx, 323)?;
    }
    let ptr = ctx.kernel.sdl.key_ptr;
    if ptr != 0 {
        ctx.cpu.write_mem(ptr, &ctx.kernel.sdl.keys)?;
    }
    ret(ptr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
    const DATA: u32 = 0x20000000;
    fn setup() -> (StubCpu, pocket_kernel::KernelState) {
        let mut cpu = StubCpu::new();
        for (base, len) in [(DATA, 0x4000), (0x10000000, 0x4000), (0x50000000, 0x10000)] {
            cpu.map_region(base, len, Prot::READ | Prot::WRITE).unwrap();
        }
        cpu.write_reg(ArmReg::Sp, 0x10002000).unwrap();
        (cpu, crate::gx::tests::fresh_kernel())
    }
    fn call(
        cpu: &mut StubCpu,
        kernel: &mut pocket_kernel::KernelState,
        handler: Handler,
        args: &[u32],
    ) -> u32 {
        for (i, &value) in args.iter().take(4).enumerate() {
            cpu.write_reg([ArmReg::R0, ArmReg::R1, ArmReg::R2, ArmReg::R3][i], value)
                .unwrap();
        }
        for (i, &value) in args.iter().skip(4).enumerate() {
            cpu.write_mem(
                0x10002000 + cpu.stack_arg_offset() + i as u32 * 4,
                &value.to_le_bytes(),
            )
            .unwrap();
        }
        let thunk = pocket_kernel::Thunk {
            thunk_va: 0x70000000,
            iat_va: 0,
            dll: "sdl.dll".into(),
            binding: pocket_pe::ImportBinding::Name("test".into()),
            friendly_name: None,
        };
        match handler(&mut CallCtx {
            cpu,
            thunk: &thunk,
            kernel,
        })
        .unwrap()
        {
            DispatchOutcome::ReturnedR0(value) => value,
            _ => panic!("unexpected outcome"),
        }
    }
    #[test]
    fn video_surface_abi_clipped_fill_and_real_framebuffer_presentation() {
        let (mut cpu, mut kernel) = setup();
        assert_eq!(call(&mut cpu, &mut kernel, init, &[VIDEO | TIMER]), 0);
        let screen = call(&mut cpu, &mut kernel, set_video, &[4, 3, 16, 0]);
        assert_ne!(screen, 0);
        let header = cpu.read_mem(screen, 60).unwrap();
        assert_eq!((word(&header, 8), word(&header, 12)), (4, 3));
        assert_eq!(word(&header, 56), 1);
        let format = word(&header, 4);
        assert_eq!(cpu.read_mem(format + 4, 2).unwrap(), [16, 2]);
        let color = call(&mut cpu, &mut kernel, map_rgb, &[format, 255, 0, 0]);
        assert_eq!(color, 0xf800);
        cpu.write_mem(DATA, &[1, 0, 1, 0, 2, 0, 1, 0]).unwrap();
        assert_eq!(call(&mut cpu, &mut kernel, set_clip, &[screen, DATA]), 1);
        assert_eq!(call(&mut cpu, &mut kernel, fill, &[screen, 0, color]), 0);
        let before = kernel.framebuffer.frame_counter;
        assert_eq!(call(&mut cpu, &mut kernel, flip, &[screen]), 0);
        let pixels: Vec<_> = kernel
            .framebuffer
            .pixels
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(pixels, [0, 0, 0, 0, 0, 0xf800, 0xf800, 0, 0, 0, 0, 0]);
        assert_eq!(kernel.framebuffer.frame_counter, before + 1);
        assert_eq!(call(&mut cpu, &mut kernel, set_video, &[4, 3, 16, 2]), 0);
        assert_eq!(kernel.sdl.screen, screen);
        call(&mut cpu, &mut kernel, quit, &[]);
        assert!(kernel.sdl.surfaces.is_empty());
        assert_eq!(kernel.sdl.screen, 0);
    }
    #[test]
    fn queued_input_peek_order_and_live_key_state() {
        let (mut cpu, mut kernel) = setup();
        let key_ptr = call(&mut cpu, &mut kernel, keys, &[DATA]);
        assert_eq!(word(&cpu.read_mem(DATA, 4).unwrap(), 0), 323);
        kernel
            .pending_input
            .push_back(InputEvent::KeyDown { vk: 0x26 });
        kernel
            .pending_input
            .push_back(InputEvent::KeyUp { vk: 0x26 });
        assert_eq!(call(&mut cpu, &mut kernel, poll, &[0]), 1);
        assert_eq!(kernel.sdl.events.len(), 2);
        cpu.write_mem(DATA + 20, &[0x5a; 4]).unwrap();
        assert_eq!(call(&mut cpu, &mut kernel, poll, &[DATA]), 1);
        assert_eq!(cpu.read_mem(DATA + 20, 4).unwrap(), [0x5a; 4]);
        let ev = cpu.read_mem(DATA, 20).unwrap();
        assert_eq!(ev[0], 2);
        assert_eq!(word(&ev, 8), 273);
        assert_eq!(call(&mut cpu, &mut kernel, poll, &[DATA]), 1);
        assert_eq!(cpu.read_mem(DATA, 1).unwrap(), [3]);
        kernel
            .pending_input
            .push_back(InputEvent::KeyDown { vk: 0x41 });
        call(&mut cpu, &mut kernel, pump, &[]);
        assert_eq!(cpu.read_mem(key_ptr + 97, 1).unwrap(), [1]);
        kernel
            .pending_input
            .push_back(InputEvent::KeyUp { vk: 0x41 });
        call(&mut cpu, &mut kernel, pump, &[]);
        assert_eq!(cpu.read_mem(key_ptr + 97, 1).unwrap(), [0]);
    }
    #[test]
    fn blit_color_key_conversion_and_invalid_masks() {
        let (mut cpu, mut kernel) = setup();
        let src = call(
            &mut cpu,
            &mut kernel,
            create_surface,
            &[0, 2, 1, 32, 0xff0000, 0xff00, 0xff, 0],
        );
        let dst = call(
            &mut cpu,
            &mut kernel,
            create_surface,
            &[0, 2, 1, 16, 0xf800, 0x7e0, 0x1f, 0],
        );
        assert_ne!(src, 0);
        assert_ne!(dst, 0);
        let sp = kernel.sdl.surfaces[&src].pixels;
        let dp = kernel.sdl.surfaces[&dst].pixels;
        cpu.write_mem(sp, &[0, 0, 255, 0, 0, 255, 0, 0]).unwrap();
        call(&mut cpu, &mut kernel, color_key, &[src, COLORKEY, 0xff0000]);
        call(&mut cpu, &mut kernel, blit, &[src, 0, dst, 0]);
        assert_eq!(cpu.read_mem(dp, 4).unwrap(), [0, 0, 0xe0, 7]);
        assert_eq!(
            call(
                &mut cpu,
                &mut kernel,
                create_surface,
                &[0, 2, 1, 32, u32::MAX, 0, 0, 0]
            ),
            0
        );
        assert_eq!(call(&mut cpu, &mut kernel, init, &[0x10]), u32::MAX);
    }
}
