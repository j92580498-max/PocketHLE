//! DirectShow properties share graph state; unsupported automation/time formats
//! report HRESULT errors rather than creating successful-looking stubs.
use super::*;
const POINTER: u32 = 0x80004003;
const INVALID: u32 = 0x80070057;
const UNSUPPORTED: u32 = 0x80004001;
fn output(ctx: &mut CallCtx<'_>, arg: usize, bytes: &[u8]) -> Result<u32, KernelError> {
    let p = ctx.arg_u32(arg as u8)?;
    write_output(ctx, p, bytes)
}
fn read_time(ctx: &mut CallCtx<'_>, pointer: u32) -> Result<Option<i64>, KernelError> {
    if pointer == 0
        || ctx
            .cpu
            .check_guest_access(pointer, 8, pocket_cpu::Prot::READ)
            .is_err()
    {
        return Ok(None);
    }
    Ok(Some(i64::from_le_bytes(
        ctx.cpu.read_mem(pointer, 8)?.try_into().unwrap(),
    )))
}
fn format_ok(ctx: &mut CallCtx<'_>, ptr: u32) -> bool {
    ptr == 0 || ctx.cpu.read_mem(ptr, 16).is_ok_and(|b| b == TIME_FORMAT)
}
fn rect(ctx: &mut CallCtx<'_>) -> Result<[i32; 4], KernelError> {
    Ok([
        ctx.arg_u32(1)? as i32,
        ctx.arg_u32(2)? as i32,
        ctx.arg_u32(3)? as i32,
        ctx.arg_u32(4)? as i32,
    ])
}
fn valid_rect(r: [i32; 4]) -> bool {
    r[2] > 0
        && r[3] > 0
        && r[2] <= 8192
        && r[3] <= 8192
        && r[0].checked_add(r[2]).is_some()
        && r[1].checked_add(r[3]).is_some()
}
fn native_rect(m: &Movie) -> [i32; 4] {
    [0, 0, m.info.width as i32, m.info.height as i32]
}
fn window_rect(ctx: &CallCtx<'_>, m: &Movie) -> [i32; 4] {
    if m.window[2] > 0 && m.window[3] > 0 {
        m.window
    } else {
        [
            0,
            0,
            ctx.kernel.framebuffer.width as i32,
            ctx.kernel.framebuffer.height as i32,
        ]
    }
}
fn rect_output(ctx: &mut CallCtx<'_>, r: [i32; 4]) -> Result<u32, KernelError> {
    for (i, v) in r.into_iter().enumerate() {
        let result = output(ctx, i + 1, &v.to_le_bytes())?;
        if result != 0 {
            return Ok(result);
        }
    }
    Ok(0)
}
pub(super) fn extended(
    ctx: &mut CallCtx<'_>,
    id: u32,
    m: &mut Movie,
    k: &str,
    slot: usize,
) -> Result<u32, KernelError> {
    if slot == 3 && k != "seeking" {
        return output(ctx, 1, &0u32.to_le_bytes());
    }
    if k != "seeking" && slot < 7 {
        return Ok(UNSUPPORTED);
    }
    if (k == "video" || k == "position" || k == "seeking") && m.path.is_none() {
        return Ok(0x80040209);
    }
    if k == "video" && m.info.width == 0 {
        return Ok(0x80004002);
    }
    match (k, slot) {
        ("seeking", 3) => output(ctx, 1, &63u32.to_le_bytes()),
        ("seeking", 4) => {
            let ptr = ctx.arg_u32(1)?;
            if ctx
                .cpu
                .check_guest_access(ptr, 4, pocket_cpu::Prot::READ | pocket_cpu::Prot::WRITE)
                .is_err()
            {
                return Ok(POINTER);
            }
            let requested = ctx.cpu.read_u32_le(ptr)?;
            let supported = requested & 63;
            write_output(ctx, ptr, &supported.to_le_bytes())?;
            Ok(if supported == requested {
                0
            } else if supported != 0 {
                1
            } else {
                0x80004005
            })
        }
        ("seeking", 5) | ("seeking", 8) => {
            let ptr = ctx.arg_u32(1)?;
            if ptr == 0 {
                return Ok(POINTER);
            }
            Ok(if format_ok(ctx, ptr) { 0 } else { 1 })
        }
        ("seeking", 6) | ("seeking", 7) => output(ctx, 1, &TIME_FORMAT),
        ("seeking", 9) => {
            let ptr = ctx.arg_u32(1)?;
            Ok(if ptr != 0 && format_ok(ctx, ptr) {
                0
            } else {
                INVALID
            })
        }
        ("seeking", 10) => output(ctx, 1, &media_time(m.info.duration).to_le_bytes()),
        ("seeking", 11) => output(ctx, 1, &media_time(m.stop).to_le_bytes()),
        ("seeking", 12) => output(ctx, 1, &media_time(m.position()).to_le_bytes()),
        ("seeking", 13) => {
            let target = ctx.arg_u32(1)?;
            let target_format = ctx.arg_u32(2)?;
            let source_format = ctx.arg_u32(6)?;
            if !format_ok(ctx, target_format) || !format_ok(ctx, source_format) {
                return Ok(INVALID);
            }
            let value = (ctx.arg_u32(4)? as u64) | ((ctx.arg_u32(5)? as u64) << 32);
            write_output(ctx, target, &value.to_le_bytes())
        }
        ("seeking", 14) => {
            let cur = ctx.arg_u32(1)?;
            let cf = ctx.arg_u32(2)?;
            let stop = ctx.arg_u32(3)?;
            let sf = ctx.arg_u32(4)?;
            if cf & !0x3f != 0 || sf & !0x3f != 0 || cf & 3 == 3 {
                return Ok(INVALID);
            }
            if (cf | sf) & 0x30 != 0 {
                return Ok(UNSUPPORTED);
            }
            let current = m.position();
            let mut next = current;
            let mut end = m.stop;
            if cf & 3 != 0 {
                let Some(value) = read_time(ctx, cur)? else {
                    return Ok(POINTER);
                };
                next = value as f64 / 10_000_000.0 + if cf & 3 == 2 { current } else { 0.0 };
            }
            if sf & 3 != 0 {
                let Some(value) = read_time(ctx, stop)? else {
                    return Ok(POINTER);
                };
                end = value as f64 / 10_000_000.0
                    + match sf & 3 {
                        2 => m.stop,
                        3 => next,
                        _ => 0.0,
                    };
            }
            for (p, flags) in [(cur, cf), (stop, sf)] {
                if flags & 8 != 0
                    && (p == 0
                        || ctx
                            .cpu
                            .check_guest_access(p, 8, pocket_cpu::Prot::WRITE)
                            .is_err())
                {
                    return Ok(POINTER);
                }
            }
            if cf & 3 != 0 || sf & 3 != 0 {
                let result = restart(ctx, id, m, next, end)?;
                if result != 0 {
                    return Ok(result);
                }
            }
            if cf & 8 != 0 {
                write_output(ctx, cur, &media_time(next).to_le_bytes())?;
            }
            if sf & 8 != 0 {
                write_output(ctx, stop, &media_time(end).to_le_bytes())?;
            }
            Ok(0)
        }
        ("seeking", 15) | ("seeking", 16) => {
            let values = if slot == 15 {
                [m.position(), m.stop]
            } else {
                [0.0, m.info.duration]
            };
            for (i, v) in values.into_iter().enumerate() {
                let result = output(ctx, i + 1, &media_time(v).to_le_bytes())?;
                if result != 0 {
                    return Ok(result);
                }
            }
            Ok(0)
        }
        ("seeking", 17) | ("position", 14) => {
            let rate = f64::from_bits(ctx.arg_u32(2)? as u64 | ((ctx.arg_u32(3)? as u64) << 32));
            Ok(if rate == 1.0 { 0 } else { UNSUPPORTED })
        }
        ("seeking", 18) | ("position", 15) => output(ctx, 1, &1.0f64.to_le_bytes()),
        ("seeking", 19) => output(ctx, 1, &0i64.to_le_bytes()),
        ("position", 7) | ("position", 9) | ("position", 10) | ("position", 12) => {
            let value = match slot {
                7 => m.info.duration,
                9 => m.position(),
                10 => m.stop,
                _ => 0.0,
            };
            output(ctx, 1, &value.to_le_bytes())
        }
        ("position", 8) | ("position", 11) | ("position", 13) => {
            let value = f64::from_bits(ctx.arg_u32(2)? as u64 | ((ctx.arg_u32(3)? as u64) << 32));
            if !value.is_finite() {
                return Ok(INVALID);
            }
            if slot == 13 {
                return Ok(if value == 0.0 { 0 } else { UNSUPPORTED });
            }
            restart(
                ctx,
                id,
                m,
                if slot == 8 { value } else { m.position() },
                if slot == 11 { value } else { m.stop },
            )
        }
        ("position", 16) | ("position", 17) => output(ctx, 1, &u32::MAX.to_le_bytes()),
        ("video", 7) => output(ctx, 1, &m.info.interval.to_le_bytes()),
        ("video", 8) => output(
            ctx,
            1,
            &(m.info.bitrate.clamp(0, i32::MAX as i64) as i32).to_le_bytes(),
        ),
        ("video", 9) => output(ctx, 1, &0u32.to_le_bytes()),
        ("video", 10) | ("video", 11) => output(
            ctx,
            1,
            &if slot == 10 {
                m.info.width
            } else {
                m.info.height
            }
            .to_le_bytes(),
        ),
        ("video", 12..=27) => {
            let source = slot < 20;
            let base = if source { 12 } else { 20 };
            let property = (slot - base) / 2;
            let index = [0, 2, 1, 3][property];
            let default = if source {
                native_rect(m)
            } else {
                let w = window_rect(ctx, m);
                [0, 0, w[2], w[3]]
            };
            let mut r = if source { m.source } else { m.destination }.unwrap_or(default);
            if (slot - base) % 2 == 1 {
                return output(ctx, 1, &r[index].to_le_bytes());
            }
            r[index] = ctx.arg_u32(1)? as i32;
            if !valid_rect(r)
                || (source
                    && (r[0] < 0
                        || r[1] < 0
                        || r[0] + r[2] > m.info.width as i32
                        || r[1] + r[3] > m.info.height as i32))
            {
                return Ok(INVALID);
            }
            if source {
                m.source = Some(r)
            } else {
                m.destination = Some(r)
            };
            Ok(0)
        }
        ("video", 28) | ("video", 31) => {
            let r = rect(ctx)?;
            if !valid_rect(r)
                || (slot == 28
                    && (r[0] < 0
                        || r[1] < 0
                        || r[0] + r[2] > m.info.width as i32
                        || r[1] + r[3] > m.info.height as i32))
            {
                return Ok(INVALID);
            }
            if slot == 28 {
                m.source = Some(r)
            } else {
                m.destination = Some(r)
            };
            Ok(0)
        }
        ("video", 29) => rect_output(ctx, m.source.unwrap_or(native_rect(m))),
        ("video", 32) => {
            let w = window_rect(ctx, m);
            rect_output(ctx, m.destination.unwrap_or([0, 0, w[2], w[3]]))
        }
        ("video", 30) => {
            m.source = None;
            Ok(0)
        }
        ("video", 33) => {
            m.destination = None;
            Ok(0)
        }
        ("video", 34) | ("window", 41) | ("window", 42) => {
            let result = output(ctx, 1, &m.info.width.to_le_bytes())?;
            if result != 0 {
                return Ok(result);
            }
            output(ctx, 2, &m.info.height.to_le_bytes())
        }
        ("video", 35) => {
            let result = output(ctx, 3, &0u32.to_le_bytes())?;
            if result != 0 {
                return Ok(result);
            }
            Ok(0x8004022a)
        } // true-colour has no palette
        ("video", 36) => current_image(ctx, m),
        ("video", 37) => Ok(u32::from(m.source.is_some())),
        ("video", 38) => Ok(u32::from(m.destination.is_some())),
        ("window", 19) => {
            let v = ctx.arg_u32(1)?;
            if v != 0 && v != u32::MAX {
                return Ok(INVALID);
            }
            m.visible = v != 0;
            m.presented = None;
            Ok(0)
        }
        ("window", 20) => output(
            ctx,
            1,
            &if m.visible { u32::MAX } else { 0u32 }.to_le_bytes(),
        ),
        ("window", 21..=28) => {
            let index = [0, 2, 1, 3][(slot - 21) / 2];
            let mut r = window_rect(ctx, m);
            if slot % 2 == 0 {
                return output(ctx, 1, &r[index].to_le_bytes());
            }
            r[index] = ctx.arg_u32(1)? as i32;
            if !valid_rect(r) {
                return Ok(INVALID);
            }
            m.window = r;
            Ok(0)
        }
        ("window", 39) => {
            let r = rect(ctx)?;
            if !valid_rect(r) {
                return Ok(INVALID);
            }
            m.window = r;
            Ok(0)
        }
        ("window", 40) | ("window", 43) => rect_output(ctx, window_rect(ctx, m)),
        ("window", 9)
        | ("window", 11)
        | ("window", 13)
        | ("window", 15)
        | ("window", 17)
        | ("window", 29)
        | ("window", 31)
        | ("window", 34)
        | ("window", 36)
        | ("window", 44) => {
            m.window_properties[slot] = ctx.arg_u32(1)?;
            if slot == 15 {
                m.visible = m.window_properties[slot] != 0;
            }
            if slot == 36 && m.window_properties[slot] != 0 {
                m.window = [
                    0,
                    0,
                    ctx.kernel.framebuffer.width as i32,
                    ctx.kernel.framebuffer.height as i32,
                ];
            }
            Ok(0)
        }
        ("window", 10)
        | ("window", 12)
        | ("window", 14)
        | ("window", 16)
        | ("window", 18)
        | ("window", 30)
        | ("window", 32)
        | ("window", 33)
        | ("window", 35)
        | ("window", 45) => {
            let property = match slot {
                33 => 34,
                35 => 36,
                _ => slot - 1,
            };
            output(ctx, 1, &m.window_properties[property].to_le_bytes())
        }
        ("window", 37) | ("window", 38) => Ok(0),
        _ => Ok(UNSUPPORTED),
    }
}
fn current_image(ctx: &mut CallCtx<'_>, m: &Movie) -> Result<u32, KernelError> {
    if m.playing {
        return Ok(0x80040227);
    }
    let Some(frame) = m.frame.as_ref() else {
        return Ok(0x80040209);
    };
    let size_ptr = ctx.arg_u32(1)?;
    let dst = ctx.arg_u32(2)?;
    let width = m.info.width;
    let height = m.info.height;
    let stride = (width * 3 + 3) & !3;
    let required = 40 + stride * height;
    if size_ptr == 0
        || ctx
            .cpu
            .check_guest_access(
                size_ptr,
                4,
                pocket_cpu::Prot::READ | pocket_cpu::Prot::WRITE,
            )
            .is_err()
    {
        return Ok(POINTER);
    }
    let capacity = ctx.cpu.read_u32_le(size_ptr)?;
    write_output(ctx, size_ptr, &required.to_le_bytes())?;
    if dst == 0 {
        return Ok(0);
    }
    if capacity < required {
        return Ok(0x8007000e);
    }
    if frame.len() != (width * height * 2) as usize {
        return Ok(0x80004005);
    }
    let mut dib = vec![0; required as usize];
    dib[..4].copy_from_slice(&40u32.to_le_bytes());
    dib[4..8].copy_from_slice(&width.to_le_bytes());
    dib[8..12].copy_from_slice(&height.to_le_bytes());
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&24u16.to_le_bytes());
    dib[20..24].copy_from_slice(&(stride * height).to_le_bytes());
    for y in 0..height {
        for x in 0..width {
            let at = ((y * width + x) * 2) as usize;
            let pixel = u16::from_le_bytes([frame[at], frame[at + 1]]);
            let dst = (40 + (height - 1 - y) * stride + x * 3) as usize;
            dib[dst] = ((pixel & 31) * 255 / 31) as u8;
            dib[dst + 1] = (((pixel >> 5) & 63) * 255 / 63) as u8;
            dib[dst + 2] = ((pixel >> 11) * 255 / 31) as u8;
        }
    }
    write_output(ctx, dst, &dib)
}
pub(super) fn present(ctx: &mut CallCtx<'_>, m: &Movie, frame: &[u8]) -> Result<(), KernelError> {
    let fb = &mut ctx.kernel.framebuffer;
    let (width, height) = if m.info.width > 0 && m.info.height > 0 {
        (m.info.width, m.info.height)
    } else {
        (fb.width, fb.height)
    };
    if frame.len() != (width * height * 2) as usize || fb.bpp != 16 {
        return Ok(());
    }
    let window = if m.window[2] > 0 {
        m.window
    } else {
        [0, 0, fb.width as i32, fb.height as i32]
    };
    let source = m.source.unwrap_or([0, 0, width as i32, height as i32]);
    let mut destination = m.destination.unwrap_or([0, 0, window[2], window[3]]);
    if m.destination.is_none() && m.source.is_none() {
        // retain aspect ratio
        if destination[2] as i64 * height as i64 > destination[3] as i64 * width as i64 {
            let w = (destination[3] as i64 * width as i64 / height as i64) as i32;
            destination[0] = (destination[2] - w) / 2;
            destination[2] = w;
        } else {
            let h = (destination[2] as i64 * height as i64 / width as i64) as i32;
            destination[1] = (destination[3] - h) / 2;
            destination[3] = h;
        }
    }
    if !valid_rect(source) || !valid_rect(destination) {
        return Ok(());
    }
    let mut pixels = fb.pixels.clone();
    // Only the video window is cleared; unrelated guest UI remains intact.
    for y in window[1].max(0)..(window[1] + window[3]).min(fb.height as i32) {
        for x in window[0].max(0)..(window[0] + window[2]).min(fb.width as i32) {
            let i = ((y as u32 * fb.width + x as u32) * 2) as usize;
            pixels[i..i + 2].fill(0);
        }
    }
    let left = window[0] as i64 + destination[0] as i64;
    let top = window[1] as i64 + destination[1] as i64;
    for y in top.max(window[1].max(0) as i64)
        ..(top + destination[3] as i64).min((window[1] + window[3]).min(fb.height as i32) as i64)
    {
        for x in left.max(window[0].max(0) as i64)
            ..(left + destination[2] as i64)
                .min((window[0] + window[2]).min(fb.width as i32) as i64)
        {
            // Pixel-centre bilinear sampling preserves smooth default playback.
            let fx = (source[0] as i64 * 65536
                + ((x - left) * 2 + 1) * source[2] as i64 * 32768 / destination[2] as i64
                - 32768)
                .clamp(
                    source[0] as i64 * 65536,
                    (source[0] + source[2] - 1) as i64 * 65536,
                );
            let fy = (source[1] as i64 * 65536
                + ((y - top) * 2 + 1) * source[3] as i64 * 32768 / destination[3] as i64
                - 32768)
                .clamp(
                    source[1] as i64 * 65536,
                    (source[1] + source[3] - 1) as i64 * 65536,
                );
            let sx = (fx >> 16) as u32;
            let sy = (fy >> 16) as u32;
            if sx >= width || sy >= height {
                continue;
            }
            let x1 = (sx + 1)
                .min((source[0] + source[2] - 1) as u32)
                .min(width - 1);
            let y1 = (sy + 1)
                .min((source[1] + source[3] - 1) as u32)
                .min(height - 1);
            let read = |x: u32, y: u32| {
                let i = ((y * width + x) * 2) as usize;
                u16::from_le_bytes([frame[i], frame[i + 1]])
            };
            let samples = [read(sx, sy), read(x1, sy), read(sx, y1), read(x1, y1)];
            let wx = (fx & 65535) as u64;
            let wy = (fy & 65535) as u64;
            let weights = [
                (65536 - wx) * (65536 - wy),
                wx * (65536 - wy),
                (65536 - wx) * wy,
                wx * wy,
            ];
            let mut pixel = 0u16;
            for (shift, mask) in [(0, 31), (5, 63), (11, 31)] {
                let sum: u64 = samples
                    .iter()
                    .zip(weights)
                    .map(|(v, w)| (((v >> shift) & mask) as u64) * w)
                    .sum();
                pixel |= (((sum + (1 << 31)) >> 32) as u16) << shift;
            }
            let to = ((y as u32 * fb.width + x as u32) * 2) as usize;
            pixels[to..to + 2].copy_from_slice(&pixel.to_le_bytes());
        }
    }
    if pixels != fb.pixels {
        fb.pixels = pixels;
        fb.mark_dirty();
        if ctx.kernel.fb_mapped {
            ctx.cpu
                .write_mem(pocket_kernel::SYNTHETIC_FRAMEBUFFER_BASE, &fb.pixels)?;
            ctx.kernel.gx_last_pushed_counter = fb.frame_counter;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use pocket_cpu::{regs::ArmReg, stub::StubCpu, Cpu, Prot};
    fn context() -> (StubCpu, pocket_kernel::KernelState, pocket_kernel::Thunk) {
        let (cpu, k, _) = crate::bluetooth::tests::setup();
        (
            cpu,
            k,
            pocket_kernel::Thunk {
                thunk_va: 0,
                iat_va: 0,
                dll: "ole32.dll".into(),
                binding: pocket_pe::ImportBinding::Name("test".into()),
                friendly_name: None,
            },
        )
    }
    #[test]
    fn media_interfaces_report_real_metadata_positions_and_formats() {
        let (mut cpu, mut k, t) = context();
        let mut m = Movie::new();
        m.path = Some("fixture.wmv".into());
        m.info = Info {
            duration: 12.5,
            width: 320,
            height: 240,
            interval: 0.04,
            bitrate: 64000,
        };
        m.stop = 12.5;
        m.offset = 3.25;
        cpu.write_reg(ArmReg::R1, 0x1100).unwrap();
        let mut ctx = CallCtx {
            cpu: &mut cpu,
            kernel: &mut k,
            thunk: &t,
        };
        assert_eq!(extended(&mut ctx, 1, &mut m, "seeking", 10).unwrap(), 0);
        assert_eq!(
            ctx.cpu.read_mem(0x1100, 8).unwrap(),
            125_000_000i64.to_le_bytes()
        );
        assert_eq!(extended(&mut ctx, 1, &mut m, "position", 9).unwrap(), 0);
        assert_eq!(ctx.cpu.read_mem(0x1100, 8).unwrap(), 3.25f64.to_le_bytes());
        assert_eq!(extended(&mut ctx, 1, &mut m, "seeking", 7).unwrap(), 0);
        assert_eq!(ctx.cpu.read_mem(0x1100, 16).unwrap(), TIME_FORMAT);
        ctx.cpu.write_reg(ArmReg::R1, 0).unwrap();
        assert_eq!(
            extended(&mut ctx, 1, &mut m, "seeking", 10).unwrap(),
            POINTER
        );
        ctx.cpu.write_reg(ArmReg::R2, 0).unwrap();
        ctx.cpu
            .write_reg(ArmReg::R3, 2f64.to_bits().wrapping_shr(32) as u32)
            .unwrap();
        assert_eq!(
            extended(&mut ctx, 1, &mut m, "seeking", 17).unwrap(),
            UNSUPPORTED
        );
        assert_eq!(m.offset, 3.25);
    }
    #[test]
    fn video_window_composites_only_its_rectangle_and_visibility_is_respected() {
        let (mut cpu, mut k, t) = context();
        k.framebuffer.resize(4, 4);
        k.framebuffer.pixels.fill(0x55);
        let mut m = Movie::new();
        m.info = Info {
            width: 2,
            height: 2,
            ..Info::default()
        };
        m.window = [1, 1, 2, 2];
        let frame = vec![0xff, 0xff, 0, 0xf8, 0xe0, 7, 0x1f, 0];
        let mut ctx = CallCtx {
            cpu: &mut cpu,
            kernel: &mut k,
            thunk: &t,
        };
        present(&mut ctx, &m, &frame).unwrap();
        assert_eq!(&ctx.kernel.framebuffer.pixels[10..14], &frame[..4]);
        assert_eq!(&ctx.kernel.framebuffer.pixels[..8], &[0x55; 8]);
        let counter = ctx.kernel.framebuffer.frame_counter;
        present(&mut ctx, &m, &frame).unwrap();
        assert_eq!(ctx.kernel.framebuffer.frame_counter, counter);
        ctx.cpu.write_reg(ArmReg::R1, 0).unwrap();
        assert_eq!(extended(&mut ctx, 1, &mut m, "window", 19).unwrap(), 0);
        assert!(!m.visible);
        ctx.cpu.write_reg(ArmReg::R1, 0x1100).unwrap();
        assert_eq!(extended(&mut ctx, 1, &mut m, "window", 20).unwrap(), 0);
        assert_eq!(ctx.cpu.read_u32_le(0x1100).unwrap(), 0);
    }
    #[test]
    fn basic_video_dib_size_query_insufficient_buffer_and_real_pixels() {
        let (mut cpu, mut k, t) = context();
        let mut m = Movie::new();
        m.info = Info {
            width: 2,
            height: 1,
            ..Info::default()
        };
        m.path = Some("frame".into());
        m.frame = Some(vec![0, 0xf8, 0xe0, 7]);
        cpu.write_reg(ArmReg::R1, 0x1100).unwrap();
        cpu.write_reg(ArmReg::R2, 0).unwrap();
        let mut ctx = CallCtx {
            cpu: &mut cpu,
            kernel: &mut k,
            thunk: &t,
        };
        assert_eq!(extended(&mut ctx, 1, &mut m, "video", 36).unwrap(), 0);
        assert_eq!(ctx.cpu.read_u32_le(0x1100).unwrap(), 48);
        ctx.cpu.write_reg(ArmReg::R2, 0x1200).unwrap();
        ctx.cpu.write_mem(0x1100, &47u32.to_le_bytes()).unwrap();
        assert_eq!(
            extended(&mut ctx, 1, &mut m, "video", 36).unwrap(),
            0x8007000e
        );
        ctx.cpu.write_mem(0x1100, &48u32.to_le_bytes()).unwrap();
        assert_eq!(extended(&mut ctx, 1, &mut m, "video", 36).unwrap(), 0);
        assert_eq!(ctx.cpu.read_mem(0x1228, 6).unwrap(), [0, 0, 255, 0, 255, 0]);
        ctx.cpu.map_region(0x9000, 4096, Prot::READ).unwrap();
        ctx.cpu.write_reg(ArmReg::R1, 0x9000).unwrap();
        assert_eq!(extended(&mut ctx, 1, &mut m, "video", 36).unwrap(), POINTER);
    }
    #[test]
    fn dropping_graph_cancels_pending_decoder_and_releases_its_receiver() {
        let mut m = Movie::new();
        let c = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        m.cancel = Some(c.clone());
        m.receiver = Some(rx);
        drop(m);
        assert!(c.load(Ordering::Acquire));
        assert!(tx.send(Packet::End).is_err());
    }
}
