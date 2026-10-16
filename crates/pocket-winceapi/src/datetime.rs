//! Deterministic WinCE formatting; the virtual default locale is en-US and
//! the virtual local clock is UTC, as for GetLocalTime elsewhere in HLE.
use crate::{CallCtx, WinCeDispatcher};
use pocket_cpu::Prot;
use pocket_kernel::{DispatchOutcome, KernelError};

pub(crate) fn now() -> [u16; 8] {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let days = (ms / 86_400_000) as i64;
    let (y, m, d) = crate::coredll::civil_from_days(days);
    [
        y as u16,
        m as u16,
        ((days + 4) % 7) as u16,
        d as u16,
        ((ms / 3_600_000) % 24) as u16,
        ((ms / 60_000) % 60) as u16,
        ((ms / 1000) % 60) as u16,
        (ms % 1000) as u16,
    ]
}
pub(crate) fn register(d: &mut WinCeDispatcher) {
    for name in [
        "GetDateFormatW",
        "GetDateFormatA",
        "GetTimeFormatW",
        "GetTimeFormatA",
    ] {
        d.register_handler("coredll.dll", name, format_api);
    }
}
fn fail(ctx: &mut CallCtx<'_>, error: u32) -> Result<DispatchOutcome, KernelError> {
    crate::coredll::set_thread_error(ctx, error);
    Ok(DispatchOutcome::ReturnedR0(0))
}
fn text(ctx: &mut CallCtx<'_>, ptr: u32, wide: bool) -> Result<String, KernelError> {
    let mut units = Vec::new();
    for i in 0..4096u32 {
        let b = ctx
            .cpu
            .read_mem(ptr + i * if wide { 2 } else { 1 }, if wide { 2 } else { 1 })?;
        let v = if wide {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            b[0] as u16
        };
        if v == 0 {
            return Ok(String::from_utf16_lossy(&units));
        }
        units.push(v);
    }
    Ok(String::from_utf16_lossy(&units))
}
const FR_MONTH_ABBR: [&str; 12] = [
    "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.",
    "déc.",
];
const FR_DAY_ABBR: [&str; 7] = ["dim.", "lun.", "mar.", "mer.", "jeu.", "ven.", "sam."];
const EN_MONTH: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const FR_MONTH: [&str; 12] = [
    "janvier",
    "février",
    "mars",
    "avril",
    "mai",
    "juin",
    "juillet",
    "août",
    "septembre",
    "octobre",
    "novembre",
    "décembre",
];
const EN_DAY: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const FR_DAY: [&str; 7] = [
    "dimanche", "lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi",
];
fn render(st: [u16; 8], pattern: &str, date: bool, french: bool, flags: u32) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut quoted = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            if i + 1 < chars.len() && chars[i + 1] == '\'' {
                out.push('\'');
                i += 2;
                continue;
            }
            quoted = !quoted;
            i += 1;
            continue;
        }
        if quoted {
            out.push(c);
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < chars.len() && chars[end] == c {
            end += 1;
        }
        let n = end - i;
        let number = |v: u16| {
            if n == 1 {
                v.to_string()
            } else {
                format!("{v:02}")
            }
        };
        let token = match c {
            'y' if date => Some(if n < 3 {
                number(st[0] % 100)
            } else {
                format!("{:04}", st[0])
            }),
            'M' if date => Some(if n < 3 {
                number(st[1])
            } else {
                let name = if french { FR_MONTH } else { EN_MONTH }[(st[1] - 1) as usize];
                if n == 3 {
                    if french {
                        FR_MONTH_ABBR[(st[1] - 1) as usize].to_owned()
                    } else {
                        name.chars().take(3).collect()
                    }
                } else {
                    name.to_owned()
                }
            }),
            'd' if date => Some(if n < 3 {
                number(st[3])
            } else {
                let day = crate::coredll::days_from_civil(st[0] as i32, st[1] as i32, st[3] as i32);
                let name = if french { FR_DAY } else { EN_DAY }[(day + 4).rem_euclid(7) as usize];
                if n == 3 {
                    if french {
                        FR_DAY_ABBR[(day + 4).rem_euclid(7) as usize].to_owned()
                    } else {
                        name.chars().take(3).collect()
                    }
                } else {
                    name.to_owned()
                }
            }),
            'H' if !date => Some(number(st[4])),
            'h' if !date => Some(number(if flags & 8 != 0 {
                st[4]
            } else {
                let h = st[4] % 12;
                if h == 0 {
                    12
                } else {
                    h
                }
            })),
            'm' if !date => Some(if flags & 1 == 0 {
                number(st[5])
            } else {
                String::new()
            }),
            's' if !date => Some(if flags & 3 == 0 {
                number(st[6])
            } else {
                String::new()
            }),
            't' if !date => Some(if flags & 4 == 0 {
                let t = if st[4] < 12 { "AM" } else { "PM" };
                if n == 1 {
                    t[..1].to_owned()
                } else {
                    t.to_owned()
                }
            } else {
                String::new()
            }),
            _ => None,
        };
        if let Some(token) = token {
            out.push_str(&token);
        } else {
            for _ in i..end {
                out.push(c);
            }
        }
        i = end;
    }
    // Suppressed time fields also suppress their adjoining separators.
    if !date && flags & 3 != 0 {
        while out.ends_with(':') || out.ends_with(' ') {
            out.pop();
        }
    }
    out.trim_end().to_owned()
}
fn format_api(ctx: &mut CallCtx<'_>) -> Result<DispatchOutcome, KernelError> {
    let name = ctx.thunk.friendly_name.as_deref().unwrap_or("");
    let wide = name.ends_with('W');
    let date = name.starts_with("GetDate");
    let locale = ctx.arg_u32(0)?;
    let flags = ctx.arg_u32(1)?;
    let ptr = ctx.arg_u32(2)?;
    let fmt = ctx.arg_u32(3)?;
    let dst = ctx.arg_u32(4)?;
    let cap = ctx.arg_u32(5)?;
    let french = match locale {
        0 | 0x400 | 0x800 | 0x409 => false,
        0x40c => true,
        _ => return fail(ctx, 87),
    };
    let allowed = if date { 0x8000_000b } else { 0x8000_000f };
    if flags & !allowed != 0
        || (date && (flags & 0xb).count_ones() > 1)
        || (fmt != 0 && flags & if date { 0xf } else { 0x8000_0000 } != 0)
        || (!date && fmt != 0 && flags & 3 != 0)
    {
        return fail(ctx, 1004);
    }
    if cap > i32::MAX as u32 || (cap > 0 && dst == 0) {
        return fail(ctx, 87);
    }
    let st = if ptr == 0 {
        now()
    } else {
        if ctx.cpu.check_guest_access(ptr, 16, Prot::READ).is_err() {
            return fail(ctx, 87);
        }
        let bytes = ctx.cpu.read_mem(ptr, 16)?;
        std::array::from_fn(|i| u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]))
    };
    if date {
        if st[0] < 1601 || st[0] > 9999 || !(1..=12).contains(&st[1]) || st[3] == 0 {
            return fail(ctx, 87);
        }
        let leap = st[0] % 4 == 0 && (st[0] % 100 != 0 || st[0] % 400 == 0);
        let days = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        if st[3] > days[(st[1] - 1) as usize] {
            return fail(ctx, 87);
        }
    } else if st[4] > 23 || st[5] > 59 || st[6] > 59 || st[7] > 999 {
        return fail(ctx, 87);
    }
    let pattern = if fmt != 0 {
        match text(ctx, fmt, wide) {
            Ok(v) => v,
            Err(_) => return fail(ctx, 87),
        }
    } else {
        if date {
            if flags & 2 != 0 {
                if french {
                    "dddd d MMMM yyyy"
                } else {
                    "dddd, MMMM d, yyyy"
                }
            } else if flags & 8 != 0 {
                "MMMM yyyy"
            } else if french {
                "dd/MM/yyyy"
            } else {
                "M/d/yyyy"
            }
        } else if flags & 1 != 0 {
            if french || flags & 8 != 0 {
                "HH"
            } else {
                "h tt"
            }
        } else if flags & 2 != 0 {
            if french || flags & 8 != 0 {
                "HH:mm"
            } else {
                "h:mm tt"
            }
        } else if french || flags & 8 != 0 {
            "HH:mm:ss"
        } else {
            "h:mm:ss tt"
        }
        .to_owned()
    };
    let output = render(st, &pattern, date, french, flags);
    let bytes: Vec<u8> = if wide {
        output
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    } else {
        output
            .chars()
            .map(|c| if c as u32 <= 255 { c as u8 } else { b'?' })
            .chain(std::iter::once(0))
            .collect()
    };
    let required = bytes.len() as u32 / if wide { 2 } else { 1 };
    if cap == 0 {
        return Ok(DispatchOutcome::ReturnedR0(required));
    }
    if cap < required {
        return fail(ctx, 122);
    }
    if ctx
        .cpu
        .check_guest_access(dst, bytes.len() as u32, Prot::WRITE)
        .is_err()
    {
        return fail(ctx, 87);
    }
    ctx.cpu.write_mem(dst, &bytes)?;
    Ok(DispatchOutcome::ReturnedR0(required))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats_civil_dates_and_literals() {
        let st = [2024, 2, 0, 29, 13, 7, 9, 0];
        assert_eq!(
            render(st, "dddd, MMMM d, yyyy", true, false, 0),
            "Thursday, February 29, 2024"
        );
        assert_eq!(render(st, "d MMMM yyyy", true, true, 0), "29 février 2024");
        assert_eq!(render(st, "hh:mm:ss tt", false, false, 0), "01:07:09 PM");
        assert_eq!(render(st, "HH 'hours' mm", false, false, 0), "13 hours 07");
    }
}
#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::bluetooth::tests::{call, setup};
    use pocket_cpu::Cpu;
    #[test]
    fn date_size_buffer_errors_leap_validation_and_ansi() {
        let (mut cpu, mut k, mut d) = setup();
        cpu.write_mem(
            0x1100,
            &[2024u16, 2, 0, 29, 13, 7, 9, 0]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "GetDateFormatW",
                &[0x409, 2, 0x1100, 0, 0, 0]
            ),
            DispatchOutcome::ReturnedR0(28)
        );
        call(
            &mut cpu,
            &mut k,
            &mut d,
            "coredll.dll",
            "SetLastError",
            &[99],
        );
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "GetDateFormatW",
                &[0x409, 2, 0x1100, 0, 0x1200, 64]
            ),
            DispatchOutcome::ReturnedR0(28)
        );
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "coredll.dll", "GetLastError", &[]),
            DispatchOutcome::ReturnedR0(99)
        );
        let bytes = cpu.read_mem(0x1200, 54).unwrap();
        let text = String::from_utf16(
            &bytes
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(text, "Thursday, February 29, 2024");
        cpu.write_mem(0x1200, &[0x55; 8]).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "GetDateFormatW",
                &[0x409, 2, 0x1100, 0, 0x1200, 4]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(cpu.read_mem(0x1200, 8).unwrap(), [0x55; 8]);
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "coredll.dll", "GetLastError", &[]),
            DispatchOutcome::ReturnedR0(122)
        );
        cpu.write_mem(0x1106, &30u16.to_le_bytes()).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "GetDateFormatW",
                &[0x409, 0, 0x1100, 0, 0, 0]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "coredll.dll", "GetLastError", &[]),
            DispatchOutcome::ReturnedR0(87)
        );
        cpu.write_mem(0x1106, &29u16.to_le_bytes()).unwrap();
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "GetDateFormatA",
                &[0x40c, 2, 0x1100, 0, 0x1200, 64]
            ),
            DispatchOutcome::ReturnedR0(22)
        );
        assert_eq!(
            cpu.read_mem(0x1200, 22).unwrap(),
            b"jeudi 29 f\xe9vrier 2024\0"
        );
    }
    #[test]
    fn time_flags_now_and_pointer_contracts() {
        let (mut cpu, mut k, mut d) = setup();
        cpu.write_mem(
            0x1100,
            &[0u16, 0, 0, 0, 13, 7, 9, 0]
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        for (flags, expected) in [
            (0, "1:07:09 PM"),
            (1, "1 PM"),
            (2, "1:07 PM"),
            (4, "1:07:09"),
            (8, "13:07:09"),
        ] {
            assert_eq!(
                call(
                    &mut cpu,
                    &mut k,
                    &mut d,
                    "coredll.dll",
                    "GetTimeFormatW",
                    &[0x409, flags, 0x1100, 0, 0x1200, 64]
                ),
                DispatchOutcome::ReturnedR0(expected.len() as u32 + 1)
            );
            let actual = cpu.read_mem(0x1200, (expected.len() * 2) as u32).unwrap();
            assert_eq!(
                actual,
                expected
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
            );
        }
        assert!(now()[0] >= 2026);
        assert_eq!(
            call(
                &mut cpu,
                &mut k,
                &mut d,
                "coredll.dll",
                "GetTimeFormatW",
                &[0x409, 0, 0xdeadbeef, 0, 0, 0]
            ),
            DispatchOutcome::ReturnedR0(0)
        );
        assert_eq!(
            call(&mut cpu, &mut k, &mut d, "coredll.dll", "GetLastError", &[]),
            DispatchOutcome::ReturnedR0(87)
        );
    }
}
