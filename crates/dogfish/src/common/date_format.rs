//! Local time and GNU `strftime`-style formatting shared by date and watch.

use std::ffi::CStr;
use std::time::{SystemTime, UNIX_EPOCH};

/// A point in time: seconds since the epoch plus nanoseconds (always >= 0).
#[derive(Clone, Copy)]
pub(crate) struct Stamp {
    pub(crate) sec: i64,
    pub(crate) ns: u32,
}

impl Stamp {
    pub(crate) fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    pub(crate) fn from_system_time(t: SystemTime) -> Self {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Self {
                sec: d.as_secs() as i64,
                ns: d.subsec_nanos(),
            },
            Err(e) => {
                let d = e.duration();
                let (sec, ns) = (-(d.as_secs() as i64), d.subsec_nanos());
                if ns == 0 {
                    Self { sec, ns: 0 }
                } else {
                    Self {
                        sec: sec - 1,
                        ns: 1_000_000_000 - ns,
                    }
                }
            }
        }
    }
}

/// Broken-down local time with the zone details strftime needs.
pub(crate) struct LocalTime {
    pub(crate) tm: libc::tm,
    pub(crate) zone: Vec<u8>,
}

pub(crate) fn local_time(t: i64) -> Option<LocalTime> {
    let tt = t as libc::time_t;
    // SAFETY: `tm` is a valid out-pointer; tm_zone, when set, points at static
    // zone-name storage that is copied before any other time call.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&tt, &mut tm).is_null() {
            return None;
        }
        let zone = if tm.tm_zone.is_null() {
            Vec::new()
        } else {
            CStr::from_ptr(tm.tm_zone).to_bytes().to_vec()
        };
        Some(LocalTime { tm, zone })
    }
}

pub(crate) const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
pub(crate) const MONTHS: [&str; 12] = [
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

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// Days from the start of `yday`'s ISO week-numbering year (gnulib's
/// `iso_week_days`); negative when the day belongs to the previous year.
fn iso_week_days(yday: i64, wday: i64) -> i64 {
    const BIG_MULTIPLE_OF_7: i64 = (366 / 7 + 2) * 7;
    yday - (yday - wday + 4 + BIG_MULTIPLE_OF_7) % 7 + 4 - 1
}

/// Renders `fmt` like GNU `date`'s strftime: the C-locale conversions plus
/// GNU's `%N`, `%q`, `%P`, `%s`, `%:z`.., and the `_ - 0 + ^ #` flags, field
/// widths and the `E`/`O` modifiers.
pub(crate) fn format_time(
    fmt: &[u8],
    lt: &LocalTime,
    ns: u32,
    epoch: i64,
    upcase: bool,
) -> Vec<u8> {
    let tm = &lt.tm;
    let year = i64::from(tm.tm_year) + 1900;
    let mut out = Vec::new();
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        let mut pad: u8 = 0;
        let mut to_upper = upcase;
        let mut change_case = false;
        while let Some(&f) = fmt.get(i) {
            match f {
                b'_' | b'-' | b'+' | b'0' => pad = f,
                b'^' => to_upper = true,
                b'#' => change_case = true,
                _ => break,
            }
            i += 1;
        }
        let mut width: i64 = -1;
        if fmt.get(i).is_some_and(u8::is_ascii_digit) {
            width = 0;
            while let Some(d @ b'0'..=b'9') = fmt.get(i).copied() {
                width = (width * 10 + i64::from(d - b'0')).min(i32::MAX as i64);
                i += 1;
            }
        }
        let modifier = match fmt.get(i) {
            Some(&m @ (b'E' | b'O')) => {
                i += 1;
                m
            }
            _ => 0,
        };
        let mut colons = 0;
        while fmt.get(i + colons) == Some(&b':') {
            colons += 1;
        }
        let conv = fmt.get(i + colons).copied();
        // A conversion GNU does not know is copied through literally.
        let bad = |out: &mut Vec<u8>, end: usize| {
            // GNU pads the copied spec as if its leading '%' took no room.
            let spec = &fmt[start..end.min(fmt.len())];
            let width = if width < 0 { width } else { width + 1 };
            push_padded(out, spec, width, pad, false, false);
        };
        if colons > 0 && (conv != Some(b'z') || colons > 3) {
            i += colons;
            bad(&mut out, i + 1);
            i += 1;
            continue;
        }
        i += colons;
        let Some(conv) = conv else {
            // A trailing '%' spec (GNU extension) is copied, padded normally.
            push_padded(&mut out, &fmt[start..], width, pad, false, false);
            break;
        };
        i += 1;
        let e_bad = b"aAbhBdDeFgGHIjklmMNSUVwW";
        let o_bad = b"aAcDFqxXY";
        if (modifier == b'E' && e_bad.contains(&conv))
            || (modifier == b'O' && o_bad.contains(&conv))
        {
            bad(&mut out, i);
            continue;
        }
        let hour12 = if tm.tm_hour % 12 == 0 {
            12
        } else {
            tm.tm_hour % 12
        };
        let num = |out: &mut Vec<u8>, digits: i64, value: i64, space_pad: bool| {
            let pad = if space_pad && pad == 0 { b'_' } else { pad };
            push_number(out, digits, value, pad, width, false, 0);
        };
        let year_sign = |out: &mut Vec<u8>, digits: i64, value: i64| {
            let limit = if digits == 2 { 99 } else { 9999 };
            let sign = pad == b'+' && (value > limit || digits < width);
            push_number(out, digits, value, pad, width, sign, 0);
        };
        let text = |out: &mut Vec<u8>, s: &[u8], upper: bool, lower: bool| {
            push_padded(out, s, width, pad, upper, lower);
        };
        let sub = |out: &mut Vec<u8>, subfmt: &str, upper: bool| {
            let rendered = format_time(subfmt.as_bytes(), lt, ns, epoch, upper);
            push_padded(out, &rendered, width, pad, false, false);
        };
        match conv {
            b'%' => {
                if pad != 0 || width >= 0 || modifier != 0 || to_upper || change_case {
                    bad(&mut out, i);
                } else {
                    out.push(b'%');
                }
            }
            b'a' | b'A' => {
                let name = WEEKDAYS[tm.tm_wday.rem_euclid(7) as usize];
                let name = if conv == b'a' { &name[..3] } else { name };
                text(&mut out, name.as_bytes(), to_upper || change_case, false);
            }
            b'b' | b'h' | b'B' => {
                let name = MONTHS[tm.tm_mon.rem_euclid(12) as usize];
                let name = if conv == b'B' { name } else { &name[..3] };
                text(&mut out, name.as_bytes(), to_upper || change_case, false);
            }
            b'c' => sub(&mut out, "%a %b %e %H:%M:%S %Y", to_upper),
            b'C' => {
                let century = year.div_euclid(100);
                year_sign(&mut out, 2, century);
            }
            b'd' => num(&mut out, 2, tm.tm_mday.into(), false),
            b'D' | b'x' => sub(&mut out, "%m/%d/%y", to_upper),
            b'e' => num(&mut out, 2, tm.tm_mday.into(), true),
            b'F' => {
                let (year_pad, year_width) = if pad == 0 && width < 0 {
                    ('+', 4)
                } else {
                    (if pad == 0 { '0' } else { pad as char }, (width - 6).max(0))
                };
                let subfmt = format!("%{year_pad}{year_width}Y-%m-%d");
                let rendered = format_time(subfmt.as_bytes(), lt, ns, epoch, to_upper);
                out.extend_from_slice(&rendered);
            }
            b'g' | b'G' | b'V' => {
                let yday = i64::from(tm.tm_yday);
                let wday = i64::from(tm.tm_wday);
                let mut adjust = 0;
                let mut days = iso_week_days(yday, wday);
                if days < 0 {
                    adjust = -1;
                    days = iso_week_days(yday + 365 + i64::from(is_leap(year - 1)), wday);
                } else {
                    let d = iso_week_days(yday - (365 + i64::from(is_leap(year))), wday);
                    if d >= 0 {
                        adjust = 1;
                        days = d;
                    }
                }
                match conv {
                    b'g' => year_sign(&mut out, 2, (year + adjust).rem_euclid(100)),
                    b'G' => year_sign(&mut out, 4, year + adjust),
                    _ => num(&mut out, 2, days / 7 + 1, false),
                }
            }
            b'H' => num(&mut out, 2, tm.tm_hour.into(), false),
            b'I' => num(&mut out, 2, hour12.into(), false),
            b'j' => num(&mut out, 3, i64::from(tm.tm_yday) + 1, false),
            b'k' => num(&mut out, 2, tm.tm_hour.into(), true),
            b'l' => num(&mut out, 2, hour12.into(), true),
            b'm' => num(&mut out, 2, i64::from(tm.tm_mon) + 1, false),
            b'M' => num(&mut out, 2, tm.tm_min.into(), false),
            b'n' => text(&mut out, b"\n", false, false),
            b't' => text(&mut out, b"\t", false, false),
            b'N' => {
                let width = if width <= 0 { 9 } else { width };
                let mut n = ns;
                let mut ndigs = 9;
                while width < ndigs || (ndigs > 1 && n.is_multiple_of(10)) {
                    ndigs -= 1;
                    n /= 10;
                }
                let digits = format!("{:0w$}", n, w = ndigs as usize);
                out.extend_from_slice(digits.as_bytes());
                if pad != b'-' {
                    let fill = if pad == b'_' { b' ' } else { b'0' };
                    out.extend(std::iter::repeat_n(fill, (width - ndigs) as usize));
                }
            }
            b'p' | b'P' => {
                let ampm: &[u8] = if tm.tm_hour >= 12 { b"PM" } else { b"AM" };
                let lower = conv == b'P' || change_case;
                text(&mut out, ampm, to_upper && !lower, lower);
            }
            b'q' => num(&mut out, 1, i64::from(tm.tm_mon) / 3 + 1, false),
            b'r' => sub(&mut out, "%I:%M:%S %p", to_upper),
            b'R' => sub(&mut out, "%H:%M", to_upper),
            b's' => push_number(&mut out, 1, epoch, pad, width, false, 0),
            b'S' => num(&mut out, 2, tm.tm_sec.into(), false),
            b'T' | b'X' => sub(&mut out, "%H:%M:%S", to_upper),
            b'u' => num(&mut out, 1, i64::from((tm.tm_wday + 6) % 7 + 1), false),
            b'U' => num(
                &mut out,
                2,
                i64::from((tm.tm_yday - tm.tm_wday + 7) / 7),
                false,
            ),
            b'w' => num(&mut out, 1, tm.tm_wday.into(), false),
            b'W' => num(
                &mut out,
                2,
                i64::from((tm.tm_yday - (tm.tm_wday + 6) % 7 + 7) / 7),
                false,
            ),
            b'y' => num(&mut out, 2, year.rem_euclid(100), false),
            b'Y' => year_sign(&mut out, 4, year),
            b'z' => {
                // `c_long` is narrower than i64 on 32-bit targets.
                #[allow(clippy::useless_conversion)]
                let off = i64::from(tm.tm_gmtoff);
                let negative = off < 0;
                let a = off.unsigned_abs() as i64;
                let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
                let colons = match colons {
                    3 if s != 0 => 2,
                    3 if m != 0 => 1,
                    c => c,
                };
                let (digits, value, mask) = match colons {
                    0 => (5, h * 100 + m, 0),
                    1 => (6, h * 100 + m, 0o4),
                    2 => (9, h * 10000 + m * 100 + s, 0o24),
                    _ => (3, h, 0),
                };
                let value = if negative { -value } else { value };
                push_number(&mut out, digits, value, pad, width, true, mask);
            }
            b'Z' => {
                let lower = change_case;
                text(&mut out, &lt.zone, to_upper && !lower, lower);
            }
            _ => bad(&mut out, i),
        }
    }
    out
}

/// Appends `s` padded on the left to `width` (zeros for the `0`/`+` flags,
/// spaces otherwise; no padding with `-`), optionally case-converted.
fn push_padded(out: &mut Vec<u8>, s: &[u8], width: i64, pad: u8, upper: bool, lower: bool) {
    if pad != b'-' && width > s.len() as i64 {
        let fill = if pad == b'0' || pad == b'+' {
            b'0'
        } else {
            b' '
        };
        out.extend(std::iter::repeat_n(fill, width as usize - s.len()));
    }
    out.extend(s.iter().map(|c| {
        if lower {
            c.to_ascii_lowercase()
        } else if upper {
            c.to_ascii_uppercase()
        } else {
            *c
        }
    }));
}

/// gnulib nstrftime's `do_number`: renders `value` with `digits` default
/// width, the pad flag, an optional forced sign, and colons inserted where
/// `colon_mask` bits are set (for `%:z`).
fn push_number(
    out: &mut Vec<u8>,
    digits: i64,
    value: i64,
    pad: u8,
    width: i64,
    always_sign: bool,
    colon_mask: u32,
) {
    let negative = value < 0;
    let mut v = value.unsigned_abs();
    let mut mask = colon_mask;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if mask & 1 != 0 {
            buf.push(b':');
        }
        mask >>= 1;
        buf.push(b'0' + (v % 10) as u8);
        v /= 10;
        if v == 0 && mask == 0 {
            break;
        }
    }
    buf.reverse();
    let pad = if pad == 0 { b'0' } else { pad };
    let mut width = if width < 0 { digits } else { width };
    let sign = if negative {
        Some(b'-')
    } else if always_sign {
        Some(b'+')
    } else {
        None
    };
    if let Some(sign) = sign {
        let shortage = width - 1 - buf.len() as i64;
        if pad == b'_' && shortage > 0 {
            out.extend(std::iter::repeat_n(b' ', shortage as usize));
            width -= shortage;
        }
        out.push(sign);
        width -= 1;
    }
    push_padded(out, &buf, width, pad, false, false);
}
