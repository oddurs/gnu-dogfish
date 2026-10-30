//! Local time and GNU `nstrftime`: the one formatter behind `date`, `watch`,
//! `date --debug` and (through [`crate::common::strftime`]) `ls --time-style`,
//! `du --time` and `stat`.

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
/// widths and the `E`/`O` modifiers. `lt` is in the current `TZ`, which `%s`
/// converts it back from.
pub(crate) fn format_time(fmt: &[u8], lt: &LocalTime, ns: u32, upcase: bool) -> Vec<u8> {
    format_time_within(fmt, lt, ns, upcase, i64::from(i32::MAX))
}

/// [`format_time`] with field widths cut to `max_width`, for a caller that
/// rejects any text longer than `max_width - 1` bytes (GNU's fixed-size
/// `nstrftime` buffers): a wider field could only be longer, so the result
/// is the same whenever it is short enough, without padding a huge width.
pub(crate) fn format_time_within(
    fmt: &[u8],
    lt: &LocalTime,
    ns: u32,
    upcase: bool,
    max_width: i64,
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
                width = (width * 10 + i64::from(d - b'0')).min(max_width);
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
        // nstrftime's bad_format: what GNU does not know is copied through
        // literally, from the '%' to `end`, padded and case-converted like
        // any other text.
        let bad = |out: &mut Vec<u8>, end: usize| {
            let spec = &fmt[start..end.min(fmt.len())];
            push_padded(out, spec, width, pad, to_upper, false);
        };
        if colons > 0 && conv != Some(b'z') {
            // Colons are valid only before 'z': the spec ends at the first
            // one, and the rest of the format goes on after it.
            bad(&mut out, i + 1);
            i += 1;
            continue;
        }
        i += colons;
        let Some(conv) = conv else {
            // A trailing '%' spec (GNU extension) is copied.
            bad(&mut out, fmt.len());
            break;
        };
        let conv_at = i;
        i += 1;
        let e_bad = b"aAbhBdDeFgGHIjklmMNSUVwW";
        let o_bad = b"aAcDFxXY";
        if (modifier == b'E' && e_bad.contains(&conv))
            || (modifier == b'O' && o_bad.contains(&conv))
        {
            // `#` upcases %b before nstrftime looks at the modifier.
            let upper = to_upper || (change_case && matches!(conv, b'b' | b'h'));
            push_padded(&mut out, &fmt[start..i], width, pad, upper, false);
            continue;
        }
        let hour12 = if tm.tm_hour % 12 == 0 {
            12
        } else {
            tm.tm_hour % 12
        };
        // What gnulib leaves to the C library's strftime: `%O` with a number
        // that is not negative, and `%EC`, `%Ey`, `%EY`.
        let underlying = |out: &mut Vec<u8>| {
            push_underlying(out, modifier, conv, tm, width, pad, to_upper);
        };
        let num = |out: &mut Vec<u8>, digits: i64, value: i64, space_pad: bool| {
            if modifier == b'O' && value >= 0 {
                return underlying(out);
            }
            let pad = if space_pad && pad == 0 { b'_' } else { pad };
            push_number(out, digits, value, pad, width, false, 0);
        };
        // nstrftime's DO_YEARISH: a magnitude and a sign, so that the
        // century of years -99..-1 can be "-0".
        let year_signed = |out: &mut Vec<u8>, digits: i64, magnitude: u64, negative: bool| {
            if modifier == b'O' && !negative {
                return underlying(out);
            }
            let limit = if digits == 2 { 99 } else { 9999 };
            let sign = pad == b'+' && (magnitude > limit || digits < width);
            push_signed(out, digits, magnitude, negative, pad, width, sign, 0);
        };
        let year_sign = |out: &mut Vec<u8>, digits: i64, value: i64| {
            year_signed(out, digits, value.unsigned_abs(), value < 0);
        };
        let text = |out: &mut Vec<u8>, s: &[u8], upper: bool, lower: bool| {
            push_padded(out, s, width, pad, upper, lower);
        };
        let sub = |out: &mut Vec<u8>, subfmt: &str, upper: bool| {
            let rendered = format_time(subfmt.as_bytes(), lt, ns, upper);
            push_padded(out, &rendered, width, pad, false, false);
        };
        match conv {
            b'%' => {
                if conv_at - 1 != start {
                    // "%5%": the spec ends before the second '%', which
                    // starts the next one.
                    bad(&mut out, conv_at);
                    i = conv_at;
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
            // nstrftime leaves %c and %x to glibc's own strftime, whose %Y
            // is the plain int tm_year + 1900 (no padding, and wrapping past
            // INT_MAX) and whose %y is (tm_year % 100 + 100) % 100.
            b'c' => {
                let mut s = format_time(b"%a %b %e %H:%M:%S ", lt, ns, to_upper);
                s.extend_from_slice(tm.tm_year.wrapping_add(1900).to_string().as_bytes());
                push_padded(&mut out, &s, width, pad, false, false);
            }
            b'x' => {
                let mut s = format_time(b"%m/%d/", lt, ns, to_upper);
                let yy = (tm.tm_year % 100 + 100) % 100;
                s.extend_from_slice(format!("{yy:02}").as_bytes());
                push_padded(&mut out, &s, width, pad, false, false);
            }
            b'C' | b'y' | b'Y' if modifier == b'E' => underlying(&mut out),
            b'C' => {
                // nstrftime truncates toward zero, so the century of a
                // negative year has the year's sign: -382247 is -3822.
                let tm_year = i64::from(tm.tm_year);
                let negative = tm_year < -1900;
                let zero_thru_1899 = i64::from(!negative && tm_year < 0);
                let century = (tm_year - 99 * zero_thru_1899) / 100 + 19;
                year_signed(&mut out, 2, century.unsigned_abs(), negative);
            }
            b'd' => num(&mut out, 2, tm.tm_mday.into(), false),
            // nstrftime hands the flag to the subformat's years only
            // (`yr_spec`): "%_D" is "%m/%d/%_y".
            b'D' if pad != 0 => {
                let subfmt = format!("%m/%d/%{}y", pad as char);
                sub(&mut out, &subfmt, to_upper);
            }
            b'D' => sub(&mut out, "%m/%d/%y", to_upper),
            b'e' => num(&mut out, 2, tm.tm_mday.into(), true),
            b'F' => {
                // nstrftime's "%Y-%m-%d" with the year's width (which can
                // be 0, unlike any width a format can spell) and flag.
                let (year_pad, year_width) = if pad == 0 && width < 0 {
                    (b'+', 4)
                } else {
                    (pad, (width - 6).max(0))
                };
                let (magnitude, negative) = (year.unsigned_abs(), year < 0);
                let sign = year_pad == b'+' && (magnitude > 9999 || 4 < year_width);
                let mut s = Vec::new();
                push_signed(
                    &mut s, 4, magnitude, negative, year_pad, year_width, sign, 0,
                );
                s.extend_from_slice(&format_time(b"-%m-%d", lt, ns, to_upper));
                push_padded(&mut out, &s, width, pad, false, false);
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
                    b'g' => {
                        // Two digits of the magnitude for negative years.
                        let tm_year = i64::from(tm.tm_year);
                        let yy = (tm_year % 100 + adjust) % 100;
                        let yy = if yy >= 0 {
                            yy
                        } else if tm_year < -1900 - adjust {
                            -yy
                        } else {
                            yy + 100
                        };
                        year_sign(&mut out, 2, yy);
                    }
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
            b's' => {
                // nstrftime's %s is mktime of the broken-down time, not the
                // instant it came from: in a repeated hour whose two offsets
                // share tm_isdst (Moscow, 2014-10-26 01:00) it can name the
                // other one. The call also moves the C library's mktime
                // guess, which later mktime calls of `date -f` start from.
                let mut ltm = *tm;
                ltm.tm_yday = -1;
                // SAFETY: `ltm` is a valid, initialized struct tm.
                let t = unsafe { libc::mktime(&mut ltm) };
                if ltm.tm_yday < 0 {
                    // EOVERFLOW: GNU's strftime gives up here.
                    return out;
                }
                push_number(&mut out, 1, t as i64, pad, width, false, 0);
            }
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
            b'y' => {
                let tm_year = i64::from(tm.tm_year);
                let yy = match tm_year % 100 {
                    yy if yy >= 0 => yy,
                    yy if tm_year < -1900 => -yy,
                    yy => yy + 100,
                };
                year_sign(&mut out, 2, yy);
            }
            b'Y' => year_sign(&mut out, 4, year),
            // Without a known DST state there is no known offset.
            b'z' if tm.tm_isdst < 0 => {}
            b'z' if colons > 3 => bad(&mut out, i),
            b'z' => {
                // `c_long` is narrower than i64 on 32-bit targets.
                #[allow(clippy::useless_conversion)]
                let off = i64::from(tm.tm_gmtoff);
                // RFC 3339's "-00": UTC, but the local offset is unknown.
                let negative = off < 0 || (off == 0 && lt.zone.first() == Some(&b'-'));
                let a = off.unsigned_abs() as i64;
                let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
                if modifier == b'O' && !negative {
                    underlying(&mut out);
                    continue;
                }
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
                push_signed(
                    &mut out,
                    digits,
                    value as u64,
                    negative,
                    pad,
                    width,
                    true,
                    mask,
                );
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

/// nstrftime's `underlying_strftime`: `%` `modifier` `conv` through the C
/// library's own strftime, then padded to `width` like any other text.
fn push_underlying(
    out: &mut Vec<u8>,
    modifier: u8,
    conv: u8,
    tm: &libc::tm,
    width: i64,
    pad: u8,
    upper: bool,
) {
    // The space tells an empty result from a failure, as in gnulib.
    let ufmt = [b' ', b'%', modifier, conv, 0];
    let mut buf = [0u8; 1024];
    // SAFETY: `ufmt` is NUL-terminated, `buf` is writable for its whole
    // length, and `tm` is a valid struct tm (its tm_zone, if read at all,
    // comes from localtime_r).
    let len =
        unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), ufmt.as_ptr().cast(), tm) };
    if len != 0 {
        push_padded(out, &buf[1..len], width, pad, upper, false);
    }
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
    let (v, negative) = (value.unsigned_abs(), value < 0);
    push_signed(
        out,
        digits,
        v,
        negative,
        pad,
        width,
        always_sign,
        colon_mask,
    );
}

/// [`push_number`] with the sign apart, so that `-0` can be written.
#[allow(clippy::too_many_arguments)]
fn push_signed(
    out: &mut Vec<u8>,
    digits: i64,
    magnitude: u64,
    negative: bool,
    pad: u8,
    width: i64,
    always_sign: bool,
    colon_mask: u32,
) {
    let mut v = magnitude;
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
