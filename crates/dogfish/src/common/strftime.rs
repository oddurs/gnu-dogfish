//! Time formatting for ls and stat: GNU `strftime` and local time.

use std::time::SystemTime;

/// Seconds and nanoseconds since the epoch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) struct Timespec {
    pub(crate) sec: i64,
    pub(crate) nsec: i64,
}

impl Timespec {
    pub(crate) fn now() -> Self {
        Self::from_system(SystemTime::now())
    }

    pub(crate) fn from_system(t: SystemTime) -> Self {
        match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => Self {
                sec: d.as_secs() as i64,
                nsec: i64::from(d.subsec_nanos()),
            },
            Err(e) => {
                let d = e.duration();
                let (mut sec, mut nsec) = (-(d.as_secs() as i64), -i64::from(d.subsec_nanos()));
                if nsec < 0 {
                    sec -= 1;
                    nsec += 1_000_000_000;
                }
                Self { sec, nsec }
            }
        }
    }
}

/// Broken-down local time via the C library, so `TZ` handling and zone
/// abbreviations match GNU exactly.
fn local_tm(sec: i64) -> Option<libc::tm> {
    let t = sec as libc::time_t;
    // SAFETY: `tm` is plain data; localtime_r only writes into it.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            None
        } else {
            Some(tm)
        }
    }
}

fn tm_zone_name(tm: &libc::tm) -> String {
    if tm.tm_zone.is_null() {
        return String::new();
    }
    // SAFETY: a non-null tm_zone from localtime_r points at a static C string.
    unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }
        .to_string_lossy()
        .into_owned()
}

const MONTHS: [&str; 12] = [
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
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// ISO 8601 week-based `(year, week)` for `%G`/`%V`.
fn iso_week(tm: &libc::tm) -> (i64, i64) {
    fn week_days(yday: i64, wday: i64) -> i64 {
        const BIG_ENOUGH_MULTIPLE_OF_7: i64 = (366 / 7 + 2) * 7;
        yday - (yday - wday + 4 + BIG_ENOUGH_MULTIPLE_OF_7) % 7 + 3
    }
    let mut year = i64::from(tm.tm_year) + 1900;
    let (yday, wday) = (i64::from(tm.tm_yday), i64::from(tm.tm_wday));
    let mut days = week_days(yday, wday);
    if days < 0 {
        year -= 1;
        days = week_days(yday + 365 + i64::from(is_leap_year(year)), wday);
    } else {
        let next = week_days(yday - (365 + i64::from(is_leap_year(year))), wday);
        if next >= 0 {
            year += 1;
            days = next;
        }
    }
    (year, days / 7 + 1)
}

/// GNU `nstrftime` for the C locale, including `%N` (nanoseconds), `%:z`, and
/// the `_ - 0 ^ #` flags with field widths. `t` is the time `tm` describes.
fn gnu_strftime(fmt: &str, tm: &libc::tm, t: Timespec) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut spec = String::from("%");
        let mut pad: Option<char> = None;
        let mut upper = false;
        let mut swap_case = false;
        while let Some(&f) = chars.peek() {
            match f {
                '_' => pad = Some(' '),
                '-' => pad = Some('\0'),
                '0' => pad = Some('0'),
                '^' => upper = true,
                '#' => swap_case = true,
                _ => break,
            }
            spec.push(f);
            chars.next();
        }
        let mut width: Option<usize> = None;
        while let Some(&d) = chars.peek().filter(|d| d.is_ascii_digit()) {
            width = Some(width.unwrap_or(0) * 10 + d.to_digit(10).unwrap_or(0) as usize);
            spec.push(d);
            chars.next();
        }
        while let Some(&m) = chars.peek().filter(|m| **m == 'E' || **m == 'O') {
            spec.push(m);
            chars.next();
        }
        let mut colons = 0;
        while chars.peek() == Some(&':') {
            colons += 1;
            spec.push(':');
            chars.next();
        }
        let Some(conv) = chars.next() else {
            out.push_str(&spec);
            break;
        };
        let num = |value: i64, digits: usize, default_pad: char| -> String {
            let pad_char = pad.unwrap_or(default_pad);
            let w = width.unwrap_or(digits);
            let body = value.unsigned_abs().to_string();
            let sign = if value < 0 { "-" } else { "" };
            if pad_char == '\0' {
                return format!("{sign}{body}");
            }
            let len = sign.len() + body.len();
            if len >= w {
                format!("{sign}{body}")
            } else if pad_char == '0' {
                format!("{sign}{}{body}", "0".repeat(w - len))
            } else {
                format!("{}{sign}{body}", " ".repeat(w - len))
            }
        };
        let text = |s: &str, is_lower_default: bool| -> String {
            let mut s = if upper || (swap_case && !is_lower_default) {
                s.to_uppercase()
            } else if swap_case {
                s.to_lowercase()
            } else {
                s.to_string()
            };
            if let Some(w) = width
                && s.len() < w
            {
                let p = if pad == Some('0') { "0" } else { " " };
                s = format!("{}{s}", p.repeat(w - s.len()));
            }
            s
        };
        let year = i64::from(tm.tm_year) + 1900;
        let hour12 = match tm.tm_hour % 12 {
            0 => 12,
            h => i64::from(h),
        };
        let month = tm.tm_mon.clamp(0, 11) as usize;
        let wday = tm.tm_wday.clamp(0, 6) as usize;
        let piece = match conv {
            'a' => text(&WEEKDAYS[wday][..3], false),
            'A' => text(WEEKDAYS[wday], false),
            'b' | 'h' => text(&MONTHS[month][..3], false),
            'B' => text(MONTHS[month], false),
            'c' => text(&gnu_strftime("%a %b %e %H:%M:%S %Y", tm, t), false),
            'C' => num(year.div_euclid(100), 2, '0'),
            'd' => num(i64::from(tm.tm_mday), 2, '0'),
            'D' | 'x' => text(&gnu_strftime("%m/%d/%y", tm, t), false),
            'e' => num(i64::from(tm.tm_mday), 2, ' '),
            'F' => text(&gnu_strftime("%Y-%m-%d", tm, t), false),
            'g' => num(iso_week(tm).0.rem_euclid(100), 2, '0'),
            'G' => num(iso_week(tm).0, 1, '0'),
            'H' => num(i64::from(tm.tm_hour), 2, '0'),
            'I' => num(hour12, 2, '0'),
            'j' => num(i64::from(tm.tm_yday) + 1, 3, '0'),
            'k' => num(i64::from(tm.tm_hour), 2, ' '),
            'l' => num(hour12, 2, ' '),
            'm' => num(i64::from(tm.tm_mon) + 1, 2, '0'),
            'M' => num(i64::from(tm.tm_min), 2, '0'),
            'n' => "\n".to_string(),
            'N' => {
                let digits = format!("{:09}", t.nsec);
                let w = width.unwrap_or(9);
                if w <= 9 {
                    digits[..w].to_string()
                } else {
                    format!("{digits}{}", "0".repeat(w - 9))
                }
            }
            'p' => text(if tm.tm_hour < 12 { "AM" } else { "PM" }, true),
            'P' => text(if tm.tm_hour < 12 { "am" } else { "pm" }, true),
            'r' => text(&gnu_strftime("%I:%M:%S %p", tm, t), false),
            'R' => text(&gnu_strftime("%H:%M", tm, t), false),
            's' => num(t.sec, 1, '0'),
            'S' => num(i64::from(tm.tm_sec), 2, '0'),
            't' => "\t".to_string(),
            'T' | 'X' => text(&gnu_strftime("%H:%M:%S", tm, t), false),
            'u' => num(((wday + 6) % 7 + 1) as i64, 1, '0'),
            'U' => num((i64::from(tm.tm_yday) + 7 - wday as i64) / 7, 2, '0'),
            'V' => num(iso_week(tm).1, 2, '0'),
            'w' => num(wday as i64, 1, '0'),
            'W' => num(
                (i64::from(tm.tm_yday) + 7 - ((wday as i64 + 6) % 7)) / 7,
                2,
                '0',
            ),
            'y' => num(year.rem_euclid(100), 2, '0'),
            'Y' => num(year, 1, '0'),
            'z' => {
                #[allow(clippy::useless_conversion)] // `c_long` is 32-bit on some targets
                let off = i64::from(tm.tm_gmtoff);
                let sign = if off < 0 { '-' } else { '+' };
                let off = off.abs();
                let (h, m, s) = (off / 3600, off / 60 % 60, off % 60);
                match colons {
                    0 => format!("{sign}{h:02}{m:02}"),
                    1 => format!("{sign}{h:02}:{m:02}"),
                    2 => format!("{sign}{h:02}:{m:02}:{s:02}"),
                    _ if s != 0 => format!("{sign}{h:02}:{m:02}:{s:02}"),
                    _ if m != 0 => format!("{sign}{h:02}:{m:02}"),
                    _ => format!("{sign}{h:02}"),
                }
            }
            'Z' => text(&tm_zone_name(tm), true),
            '%' => "%".to_string(),
            other => {
                spec.push(other);
                spec
            }
        };
        out.push_str(&piece);
    }
    out
}

/// Formats `t` in local time with a GNU `strftime` format.
pub(crate) fn format_local_time(fmt: &str, t: Timespec) -> Option<String> {
    local_tm(t.sec).map(|tm| gnu_strftime(fmt, &tm, t))
}
