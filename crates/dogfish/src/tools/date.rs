//! date — GNU date strings (`-d`), strftime with GNU flags, -I/-R/--rfc-3339

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{
    DogfishContext, gnu_io_error, gnu_quote, gnu_quote_file, io::ignore_broken_pipe, stdout,
};

use crate::common::date_format::{LocalTime, MONTHS, Stamp, WEEKDAYS, format_time, local_time};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, argmatch, getopt_error, lossy, print_help, print_version,
    unsupported, usage_error,
};
use crate::common::quote::quote_locale;

unsafe extern "C" {
    /// POSIX `tzset(3)`; the `libc` crate does not bind it on every Unix.
    fn tzset();
}

fn utc_time(t: i64) -> Option<LocalTime> {
    let tt = t as libc::time_t;
    // SAFETY: `tm` is a valid out-pointer.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::gmtime_r(&tt, &mut tm).is_null() {
            return None;
        }
        Some(LocalTime {
            tm,
            zone: b"UTC".to_vec(),
        })
    }
}

/// Normalizes `tm` like `mktime` (local time) or `timegm` (`utc`) and returns
/// the time, or `None` if it is not representable.
fn make_time(tm: &mut libc::tm, utc: bool) -> Option<i64> {
    tm.tm_wday = -1;
    // SAFETY: `tm` is a valid, initialized struct tm.
    let t = unsafe {
        if utc {
            libc::timegm(tm)
        } else {
            libc::mktime(tm)
        }
    };
    // -1 is a valid time; mktime reports failure by leaving tm_wday alone.
    (tm.tm_wday >= 0).then_some(t as i64)
}

// --- date string parsing (a port of the essentials of gnulib parse-datetime) ---

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unit {
    Year,
    Month,
    /// Days, with a multiplier (week = 7, fortnight = 14).
    Day(i64),
    Hour,
    Minute,
    Second,
}

#[derive(Clone, PartialEq, Debug)]
enum DateTok {
    /// Unsigned / signed integers, with their digit counts.
    UNum(i64, usize),
    SNum(i64, usize),
    /// Decimal seconds `S.FFF` (signed when written with a sign).
    Decimal(i64, i64, bool),
    Month(i32),
    Weekday(i32),
    Meridian(bool),
    Unit(Unit),
    Ordinal(i64),
    DayShift(i64),
    Ago(i64),
    Zone(i64),
    DayZone(i64),
    Dst,
    T,
    Char(u8),
}

const ZONE_TABLE: &[(&str, i64, bool)] = &[
    ("GMT", 0, false),
    ("UT", 0, false),
    ("UTC", 0, false),
    ("WET", 0, false),
    ("WEST", 0, true),
    ("BST", 0, true),
    ("ART", -3 * 3600, false),
    ("BRT", -3 * 3600, false),
    ("BRST", -3 * 3600, true),
    ("NST", -(3 * 3600 + 1800), false),
    ("NDT", -(3 * 3600 + 1800), true),
    ("AST", -4 * 3600, false),
    ("ADT", -4 * 3600, true),
    ("CLT", -4 * 3600, false),
    ("CLST", -4 * 3600, true),
    ("EST", -5 * 3600, false),
    ("EDT", -5 * 3600, true),
    ("CST", -6 * 3600, false),
    ("CDT", -6 * 3600, true),
    ("MST", -7 * 3600, false),
    ("MDT", -7 * 3600, true),
    ("PST", -8 * 3600, false),
    ("PDT", -8 * 3600, true),
    ("AKST", -9 * 3600, false),
    ("AKDT", -9 * 3600, true),
    ("HST", -10 * 3600, false),
    ("HAST", -10 * 3600, false),
    ("HADT", -10 * 3600, true),
    ("SST", -12 * 3600, false),
    ("WAT", 3600, false),
    ("CET", 3600, false),
    ("CEST", 3600, true),
    ("MET", 3600, false),
    ("MEZ", 3600, false),
    ("MEST", 3600, true),
    ("MESZ", 3600, true),
    ("EET", 2 * 3600, false),
    ("EEST", 2 * 3600, true),
    ("CAT", 2 * 3600, false),
    ("SAST", 2 * 3600, false),
    ("EAT", 3 * 3600, false),
    ("MSK", 3 * 3600, false),
    ("MSD", 3 * 3600, true),
    ("IST", 5 * 3600 + 1800, false),
    ("SGT", 8 * 3600, false),
    ("KST", 9 * 3600, false),
    ("JST", 9 * 3600, false),
    ("GST", 10 * 3600, false),
    ("NZST", 12 * 3600, false),
    ("NZDT", 12 * 3600, true),
];

fn lookup_zone(word: &str) -> Option<DateTok> {
    ZONE_TABLE
        .iter()
        .find(|(name, _, _)| *name == word)
        .map(|&(_, off, dst)| {
            if dst {
                DateTok::DayZone(off)
            } else {
                DateTok::Zone(off)
            }
        })
}

fn lookup_date_word(word: &str) -> Option<DateTok> {
    match word {
        "AM" | "A.M." => return Some(DateTok::Meridian(false)),
        "PM" | "P.M." => return Some(DateTok::Meridian(true)),
        _ => {}
    }
    let abbrev = word.len() == 3 || (word.len() == 4 && word.ends_with('.'));
    let matches = |full: &str| {
        let full = full.to_ascii_uppercase();
        if abbrev {
            full.starts_with(&word[..3])
        } else {
            full == word
        }
    };
    if let Some(m) = MONTHS.iter().position(|m| matches(m)) {
        return Some(DateTok::Month(m as i32 + 1));
    }
    if word == "SEPT" {
        return Some(DateTok::Month(9));
    }
    if let Some(d) = WEEKDAYS.iter().position(|d| matches(d)) {
        return Some(DateTok::Weekday(d as i32));
    }
    match word {
        "TUES" => return Some(DateTok::Weekday(2)),
        "WEDNES" => return Some(DateTok::Weekday(3)),
        "THUR" | "THURS" => return Some(DateTok::Weekday(4)),
        _ => {}
    }
    if let Some(z) = lookup_zone(word) {
        return Some(z);
    }
    if word == "DST" {
        return Some(DateTok::Dst);
    }
    let unit = |w: &str| match w {
        "YEAR" => Some(Unit::Year),
        "MONTH" => Some(Unit::Month),
        "FORTNIGHT" => Some(Unit::Day(14)),
        "WEEK" => Some(Unit::Day(7)),
        "DAY" => Some(Unit::Day(1)),
        "HOUR" => Some(Unit::Hour),
        "MINUTE" | "MIN" => Some(Unit::Minute),
        "SECOND" | "SEC" => Some(Unit::Second),
        _ => None,
    };
    if let Some(u) = unit(word).or_else(|| unit(word.strip_suffix('S')?)) {
        return Some(DateTok::Unit(u));
    }
    let relative = match word {
        "TOMORROW" => Some(DateTok::DayShift(1)),
        "YESTERDAY" => Some(DateTok::DayShift(-1)),
        "TODAY" | "NOW" => Some(DateTok::DayShift(0)),
        "LAST" => Some(DateTok::Ordinal(-1)),
        "THIS" => Some(DateTok::Ordinal(0)),
        "NEXT" | "FIRST" => Some(DateTok::Ordinal(1)),
        "THIRD" => Some(DateTok::Ordinal(3)),
        "FOURTH" => Some(DateTok::Ordinal(4)),
        "FIFTH" => Some(DateTok::Ordinal(5)),
        "SIXTH" => Some(DateTok::Ordinal(6)),
        "SEVENTH" => Some(DateTok::Ordinal(7)),
        "EIGHTH" => Some(DateTok::Ordinal(8)),
        "NINTH" => Some(DateTok::Ordinal(9)),
        "TENTH" => Some(DateTok::Ordinal(10)),
        "ELEVENTH" => Some(DateTok::Ordinal(11)),
        "TWELFTH" => Some(DateTok::Ordinal(12)),
        "AGO" => Some(DateTok::Ago(-1)),
        "HENCE" => Some(DateTok::Ago(1)),
        _ => None,
    };
    if relative.is_some() {
        return relative;
    }
    if let [c] = word.as_bytes() {
        // Military time zones; "T" doubles as the ISO 8601 date/time separator.
        let hours = match c {
            b'A'..=b'I' => i64::from(c - b'A') + 1,
            b'K'..=b'M' => i64::from(c - b'K') + 10,
            b'N'..=b'Y' => -(i64::from(c - b'N') + 1),
            b'Z' => 0,
            _ => return None,
        };
        return Some(if *c == b'T' {
            DateTok::T
        } else {
            DateTok::Zone(hours * 3600)
        });
    }
    if word.contains('.') {
        return lookup_zone(&word.replace('.', ""));
    }
    None
}

fn lex_date(s: &str) -> Option<Vec<DateTok>> {
    let b = s.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || c == b'-' || c == b'+' {
            let mut sign = 0i64;
            if c == b'-' || c == b'+' {
                sign = if c == b'-' { -1 } else { 1 };
                i += 1;
                while b.get(i).is_some_and(u8::is_ascii_whitespace) {
                    i += 1;
                }
                if !b.get(i).is_some_and(u8::is_ascii_digit) {
                    // A sign not followed by a number is ignored.
                    continue;
                }
            }
            let start = i;
            let mut value: i64 = 0;
            while let Some(&d) = b.get(i).filter(|d| d.is_ascii_digit()) {
                value = value.checked_mul(10)?.checked_add(i64::from(d - b'0'))?;
                i += 1;
            }
            let digits = i - start;
            if matches!(b.get(i), Some(b'.' | b',')) && b.get(i + 1).is_some_and(u8::is_ascii_digit)
            {
                i += 1;
                let mut ns = 0i64;
                let mut places = 0;
                while let Some(&d) = b.get(i).filter(|d| d.is_ascii_digit()) {
                    if places < 9 {
                        ns = ns * 10 + i64::from(d - b'0');
                        places += 1;
                    }
                    i += 1;
                }
                ns *= 10i64.pow(9 - places);
                let (mut sec, mut ns) = (value, ns);
                if sign < 0 {
                    sec = -sec;
                    if ns != 0 {
                        sec -= 1;
                        ns = 1_000_000_000 - ns;
                    }
                }
                toks.push(DateTok::Decimal(sec, ns, sign != 0));
            } else if sign == 0 {
                toks.push(DateTok::UNum(value, digits));
            } else {
                toks.push(DateTok::SNum(sign * value, digits));
            }
            continue;
        }
        if c.is_ascii_alphabetic() {
            let start = i;
            while b
                .get(i)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'.')
            {
                i += 1;
            }
            let word = s[start..i].to_ascii_uppercase();
            toks.push(lookup_date_word(&word)?);
            continue;
        }
        if c == b'(' {
            let mut depth = 0;
            while i < b.len() {
                match b[i] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                i += 1;
                if depth == 0 {
                    break;
                }
            }
            continue;
        }
        if !c.is_ascii() {
            return None;
        }
        toks.push(DateTok::Char(c));
        i += 1;
    }
    Some(toks)
}

/// Parser state, mirroring gnulib's `parser_control`.
#[derive(Default)]
struct DateParse {
    year: i64,
    year_digits: usize,
    month: i64,
    day: i64,
    hour: i64,
    minutes: i64,
    seconds: i64,
    ns: i64,
    /// None = 24-hour clock; Some(true) = PM.
    meridian: Option<bool>,
    day_ordinal: i64,
    day_number: i64,
    time_zone: i64,
    local_isdst: i32,
    rel_year: i64,
    rel_month: i64,
    rel_day: i64,
    rel_hour: i64,
    rel_minutes: i64,
    rel_seconds: i64,
    rel_ns: i64,
    times_seen: u32,
    dates_seen: u32,
    days_seen: u32,
    zones_seen: u32,
    dsts_seen: u32,
    rels_seen: bool,
}

impl DateParse {
    fn apply_rel(&mut self, unit: Unit, n: i64, ns: i64) -> Option<()> {
        let add = |field: &mut i64, v: i64| -> Option<()> {
            *field = field.checked_add(v)?;
            Some(())
        };
        match unit {
            Unit::Year => add(&mut self.rel_year, n)?,
            Unit::Month => add(&mut self.rel_month, n)?,
            Unit::Day(mult) => add(&mut self.rel_day, n.checked_mul(mult)?)?,
            Unit::Hour => add(&mut self.rel_hour, n)?,
            Unit::Minute => add(&mut self.rel_minutes, n)?,
            Unit::Second => {
                add(&mut self.rel_seconds, n)?;
                add(&mut self.rel_ns, ns)?;
            }
        }
        self.rels_seen = true;
        Some(())
    }

    fn set_time(&mut self, h: i64, m: i64, s: i64, ns: i64, meridian: Option<bool>) {
        self.hour = h;
        self.minutes = m;
        self.seconds = s;
        self.ns = ns;
        self.meridian = meridian;
    }

    /// A bare number: a year, a YYYYMMDD date, or an HHMM time.
    fn digits_to_date_time(&mut self, value: i64, digits: usize) {
        if self.dates_seen > 0
            && self.year_digits == 0
            && !self.rels_seen
            && (self.times_seen > 0 || digits > 2)
        {
            self.year = value;
            self.year_digits = digits;
        } else if digits > 4 {
            self.dates_seen += 1;
            self.day = value % 100;
            self.month = (value / 100) % 100;
            self.year = value / 10000;
            self.year_digits = digits - 4;
        } else {
            self.times_seen += 1;
            if digits <= 2 {
                self.set_time(value, 0, 0, 0, None);
            } else {
                self.set_time(value / 100, value % 100, 0, 0, None);
            }
        }
    }

    /// `[+-]hh[mm]` or `[+-]hh:mm` as a UTC offset in seconds.
    fn zone_hhmm(&mut self, value: i64, digits: usize, minutes: Option<i64>) -> Option<()> {
        let n_minutes = match minutes {
            None => {
                let v = if digits <= 2 { value * 100 } else { value };
                (v / 100) * 60 + v % 100
            }
            Some(mm) => value * 60 + if value < 0 { -mm } else { mm },
        };
        if n_minutes.abs() > 24 * 60 {
            return None;
        }
        self.time_zone = n_minutes * 60;
        Some(())
    }
}

/// A cursor over date tokens, with the grammar's productions as methods.
struct DateParser<'a> {
    toks: &'a [DateTok],
    pos: usize,
    pc: DateParse,
}

impl DateParser<'_> {
    fn peek(&self, k: usize) -> Option<&DateTok> {
        self.toks.get(self.pos + k)
    }

    fn next_if(&mut self, f: impl Fn(&DateTok) -> bool) -> Option<DateTok> {
        let t = self.peek(0).filter(|t| f(t))?.clone();
        self.pos += 1;
        Some(t)
    }

    fn is_char(t: &DateTok, c: u8) -> bool {
        *t == DateTok::Char(c)
    }

    /// `: mm` after a zone offset, if present.
    fn colon_minutes(&mut self) -> Option<Option<i64>> {
        if self.peek(0).is_some_and(|t| Self::is_char(t, b':')) {
            if let Some(DateTok::UNum(mm, _)) = self.peek(1).cloned() {
                self.pos += 2;
                return Some(Some(mm));
            }
            return None;
        }
        Some(None)
    }

    fn zone_offset(&mut self) -> Option<bool> {
        let Some(DateTok::SNum(v, digits)) = self.peek(0).cloned() else {
            return Some(false);
        };
        self.pos += 1;
        let mm = self.colon_minutes()?;
        self.pc.zones_seen += 1;
        self.pc.zone_hhmm(v, digits, mm)?;
        Some(true)
    }

    /// After `hh:mm`: optional `:ss[.fff]`, then a meridian or a zone offset.
    fn finish_time(&mut self, h: i64, m: i64, iso_only: bool) -> Option<()> {
        let (mut s, mut ns) = (0, 0);
        if self.peek(0).is_some_and(|t| Self::is_char(t, b':')) {
            match self.peek(1).cloned() {
                Some(DateTok::UNum(v, _)) => (s, ns) = (v, 0),
                Some(DateTok::Decimal(v, f, false)) => (s, ns) = (v, f),
                _ => return None,
            }
            self.pos += 2;
        }
        if !iso_only && let Some(DateTok::Meridian(pm)) = self.peek(0).cloned() {
            self.pos += 1;
            self.pc.set_time(h, m, s, ns, Some(pm));
            return Some(());
        }
        self.pc.set_time(h, m, s, ns, None);
        self.zone_offset()?;
        Some(())
    }

    fn unit_after(&self, k: usize) -> Option<Unit> {
        match self.peek(k) {
            Some(DateTok::Unit(u)) => Some(*u),
            _ => None,
        }
    }

    /// `relunit [ago]` where the multiplier (and fraction) are already known.
    fn rel(&mut self, unit: Unit, n: i64, ns: i64) -> Option<()> {
        let sign = match self.peek(0) {
            Some(DateTok::Ago(s)) => {
                let s = *s;
                self.pos += 1;
                s
            }
            _ => 1,
        };
        self.pc.apply_rel(unit, n.checked_mul(sign)?, ns * sign)
    }

    fn item(&mut self) -> Option<()> {
        let tok = self.peek(0)?.clone();
        self.pos += 1;
        match tok {
            DateTok::UNum(n, digits) => self.number_item(n, digits),
            DateTok::SNum(n, _) => {
                let unit = self.unit_after(0)?;
                self.pos += 1;
                self.rel(unit, n, 0)
            }
            DateTok::Decimal(s, ns, _) => {
                if self.unit_after(0) != Some(Unit::Second) {
                    return None;
                }
                self.pos += 1;
                self.rel(Unit::Second, s, ns)
            }
            DateTok::Month(m) => {
                self.pc.dates_seen += 1;
                self.pc.month = m.into();
                match (self.peek(0).cloned(), self.peek(1).cloned()) {
                    (Some(DateTok::SNum(d, _)), Some(DateTok::SNum(y, yd))) => {
                        self.pos += 2;
                        self.pc.day = -d;
                        self.pc.year = -y;
                        self.pc.year_digits = yd;
                    }
                    (Some(DateTok::UNum(d, _)), next) => {
                        self.pos += 1;
                        self.pc.day = d;
                        if next.is_some_and(|t| Self::is_char(&t, b','))
                            && let Some(DateTok::UNum(y, yd)) = self.peek(1).cloned()
                        {
                            self.pos += 2;
                            self.pc.year = y;
                            self.pc.year_digits = yd;
                        }
                    }
                    _ => return None,
                }
                Some(())
            }
            DateTok::Weekday(d) => {
                self.pc.days_seen += 1;
                self.pc.day_ordinal = 0;
                self.pc.day_number = d.into();
                self.next_if(|t| Self::is_char(t, b','));
                Some(())
            }
            DateTok::Ordinal(n) => match self.peek(0).cloned()? {
                DateTok::Weekday(d) => {
                    self.pos += 1;
                    self.pc.days_seen += 1;
                    self.pc.day_ordinal = n;
                    self.pc.day_number = d.into();
                    Some(())
                }
                DateTok::Unit(u) => {
                    self.pos += 1;
                    self.rel(u, n, 0)
                }
                _ => None,
            },
            DateTok::Unit(u) => self.rel(u, 1, 0),
            DateTok::DayShift(n) => self.pc.apply_rel(Unit::Day(1), n, 0),
            DateTok::Zone(off) => {
                self.pc.zones_seen += 1;
                self.pc.time_zone = off;
                self.zone_suffix(off)
            }
            DateTok::DayZone(off) => {
                self.pc.zones_seen += 1;
                self.pc.time_zone = off + 3600;
                Some(())
            }
            _ => None,
        }
    }

    /// What may follow a zone name: `DST`, a relative offset, or `+hh[:mm]`.
    fn zone_suffix(&mut self, off: i64) -> Option<()> {
        match self.peek(0).cloned() {
            Some(DateTok::Dst) => {
                self.pos += 1;
                self.pc.time_zone = off + 3600;
            }
            Some(DateTok::SNum(n, digits)) => {
                self.pos += 1;
                if let Some(unit) = self.unit_after(0) {
                    self.pos += 1;
                    self.pc.apply_rel(unit, n, 0)?;
                } else {
                    let mm = self.colon_minutes()?;
                    self.pc.zone_hhmm(n, digits, mm)?;
                    self.pc.time_zone = self.pc.time_zone.checked_add(off)?;
                }
            }
            _ => {}
        }
        Some(())
    }

    fn number_item(&mut self, n: i64, digits: usize) -> Option<()> {
        let next = self.peek(0).cloned();
        match next {
            Some(DateTok::Char(b':')) => {
                let Some(DateTok::UNum(m, _)) = self.peek(1).cloned() else {
                    return None;
                };
                self.pos += 2;
                self.pc.times_seen += 1;
                self.finish_time(n, m, false)
            }
            Some(DateTok::Meridian(pm)) => {
                self.pos += 1;
                self.pc.times_seen += 1;
                self.pc.set_time(n, 0, 0, 0, Some(pm));
                Some(())
            }
            Some(DateTok::Char(b'/')) => {
                let Some(DateTok::UNum(second, _)) = self.peek(1).cloned() else {
                    return None;
                };
                self.pos += 2;
                self.pc.dates_seen += 1;
                if self.peek(0).is_some_and(|t| Self::is_char(t, b'/')) {
                    let Some(DateTok::UNum(third, third_digits)) = self.peek(1).cloned() else {
                        return None;
                    };
                    self.pos += 2;
                    if digits >= 4 {
                        (self.pc.year, self.pc.year_digits) = (n, digits);
                        (self.pc.month, self.pc.day) = (second, third);
                    } else {
                        (self.pc.month, self.pc.day) = (n, second);
                        (self.pc.year, self.pc.year_digits) = (third, third_digits);
                    }
                } else {
                    (self.pc.month, self.pc.day) = (n, second);
                }
                Some(())
            }
            Some(DateTok::Month(m)) => {
                self.pos += 1;
                self.pc.dates_seen += 1;
                self.pc.day = n;
                self.pc.month = m.into();
                match self.peek(0).cloned() {
                    Some(DateTok::SNum(y, yd)) => {
                        self.pos += 1;
                        (self.pc.year, self.pc.year_digits) = (-y, yd);
                    }
                    Some(DateTok::UNum(y, yd)) => {
                        self.pos += 1;
                        (self.pc.year, self.pc.year_digits) = (y, yd);
                    }
                    _ => {}
                }
                Some(())
            }
            Some(DateTok::SNum(m, _)) => {
                if let Some(unit) = self.unit_after(1) {
                    // "YYYYMMDD +N days": a number, then a relative offset.
                    self.pos += 2;
                    self.pc.digits_to_date_time(n, digits);
                    return self.pc.apply_rel(unit, m, 0);
                }
                if let Some(DateTok::SNum(d, _)) = self.peek(1).cloned() {
                    // ISO 8601 YYYY-MM-DD, optionally followed by Thh:mm[:ss].
                    self.pos += 2;
                    self.pc.dates_seen += 1;
                    (self.pc.year, self.pc.year_digits) = (n, digits);
                    (self.pc.month, self.pc.day) = (-m, -d);
                    if self.peek(0) == Some(&DateTok::T)
                        && let Some(DateTok::UNum(h, _)) = self.peek(1).cloned()
                    {
                        self.pos += 2;
                        self.pc.times_seen += 1;
                        return self.iso_time(h);
                    }
                    return Some(());
                }
                // hh followed by a zone offset.
                self.pc.times_seen += 1;
                self.pc.set_time(n, 0, 0, 0, None);
                self.zone_offset()?;
                Some(())
            }
            Some(DateTok::Weekday(d)) => {
                self.pos += 1;
                self.pc.days_seen += 1;
                self.pc.day_ordinal = n;
                self.pc.day_number = d.into();
                Some(())
            }
            Some(DateTok::Unit(u)) => {
                self.pos += 1;
                self.rel(u, n, 0)
            }
            _ => {
                self.pc.digits_to_date_time(n, digits);
                Some(())
            }
        }
    }

    /// The time after an ISO 8601 `T`: `hh:mm[:ss]` or `hh` plus a zone offset.
    fn iso_time(&mut self, h: i64) -> Option<()> {
        if self.peek(0).is_some_and(|t| Self::is_char(t, b':')) {
            let Some(DateTok::UNum(m, _)) = self.peek(1).cloned() else {
                return None;
            };
            self.pos += 2;
            return self.finish_time(h, m, true);
        }
        self.pc.set_time(h, 0, 0, 0, None);
        self.zone_offset()?.then_some(())
    }
}

/// Parses a GNU date string relative to `now`; `None` means "invalid date".
fn parse_date(input: &str, now: Stamp) -> Option<Stamp> {
    let toks = lex_date(input)?;
    if let Some(DateTok::Char(b'@')) = toks.first() {
        return match toks[1..] {
            [DateTok::UNum(s, _)] | [DateTok::SNum(s, _)] => Some(Stamp { sec: s, ns: 0 }),
            [DateTok::Decimal(s, ns, _)] => Some(Stamp {
                sec: s,
                ns: ns as u32,
            }),
            _ => None,
        };
    }
    let now_tm = local_time(now.sec)?.tm;
    let mut parser = DateParser {
        toks: &toks,
        pos: 0,
        pc: DateParse {
            year: i64::from(now_tm.tm_year) + 1900,
            month: i64::from(now_tm.tm_mon) + 1,
            day: now_tm.tm_mday.into(),
            hour: now_tm.tm_hour.into(),
            minutes: now_tm.tm_min.into(),
            seconds: now_tm.tm_sec.into(),
            ns: now.ns.into(),
            local_isdst: now_tm.tm_isdst,
            ..DateParse::default()
        },
    };
    while parser.pos < toks.len() {
        parser.item()?;
    }
    let pc = parser.pc;
    if [
        pc.times_seen,
        pc.dates_seen,
        pc.days_seen,
        pc.dsts_seen,
        pc.zones_seen,
    ]
    .iter()
    .any(|&n| n > 1)
    {
        return None;
    }
    let year = if pc.year_digits == 2 && pc.year >= 0 {
        pc.year + if pc.year < 69 { 2000 } else { 1900 }
    } else {
        pc.year
    };
    let mut tm: libc::tm = now_tm;
    let narrow = |v: i64| -> Option<libc::c_int> { v.try_into().ok() };
    tm.tm_year = narrow(year.checked_sub(1900)?)?;
    tm.tm_mon = narrow(pc.month - 1)?;
    tm.tm_mday = narrow(pc.day)?;
    let mut ns = pc.ns;
    if pc.times_seen > 0 || (pc.rels_seen && pc.dates_seen == 0 && pc.days_seen == 0) {
        let hour = match pc.meridian {
            None => (0..=23).contains(&pc.hour).then_some(pc.hour),
            Some(pm) => (1..=12)
                .contains(&pc.hour)
                .then_some(pc.hour % 12 + if pm { 12 } else { 0 }),
        }?;
        tm.tm_hour = narrow(hour)?;
        tm.tm_min = narrow(pc.minutes)?;
        tm.tm_sec = narrow(pc.seconds)?;
    } else {
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        ns = 0;
    }
    tm.tm_isdst = if pc.dates_seen + pc.days_seen + pc.times_seen > 0 {
        -1
    } else {
        pc.local_isdst
    };
    // With an explicit zone, the fields are wall-clock time in that zone.
    let utc = pc.zones_seen > 0;
    let tm0 = tm;
    let mut start = make_time(&mut tm, utc)?;
    let same = |a: &libc::tm, b: &libc::tm| {
        (
            a.tm_sec, a.tm_min, a.tm_hour, a.tm_mday, a.tm_mon, a.tm_year,
        ) == (
            b.tm_sec, b.tm_min, b.tm_hour, b.tm_mday, b.tm_mon, b.tm_year,
        )
    };
    if !same(&tm0, &tm) {
        return None;
    }
    if pc.days_seen > 0 && pc.dates_seen == 0 {
        let wday = i64::from(tm.tm_wday);
        let ordinal_weeks = pc.day_ordinal - i64::from(pc.day_ordinal > 0 && wday != pc.day_number);
        let incr = ordinal_weeks * 7 + (pc.day_number - wday + 7).rem_euclid(7);
        tm.tm_mday = narrow(i64::from(tm.tm_mday) + incr)?;
        tm.tm_isdst = -1;
        start = make_time(&mut tm, utc)?;
    }
    if pc.rel_year != 0 || pc.rel_month != 0 || pc.rel_day != 0 {
        tm.tm_year = narrow(i64::from(tm.tm_year) + pc.rel_year)?;
        tm.tm_mon = narrow(i64::from(tm.tm_mon) + pc.rel_month)?;
        tm.tm_mday = narrow(i64::from(tm.tm_mday) + pc.rel_day)?;
        tm.tm_hour = tm0.tm_hour;
        tm.tm_min = tm0.tm_min;
        tm.tm_sec = tm0.tm_sec;
        tm.tm_isdst = tm0.tm_isdst;
        start = make_time(&mut tm, utc)?;
    }
    if utc {
        start = start.checked_sub(pc.time_zone)?;
    }
    let sum_ns = ns + pc.rel_ns;
    let norm_ns = sum_ns.rem_euclid(1_000_000_000);
    let carry = (sum_ns - norm_ns) / 1_000_000_000;
    let sec = start
        .checked_add(pc.rel_hour.checked_mul(3600)?)?
        .checked_add(pc.rel_minutes.checked_mul(60)?)?
        .checked_add(pc.rel_seconds)?
        .checked_add(carry)?;
    Some(Stamp {
        sec,
        ns: norm_ns as u32,
    })
}

const DATE_HELP: &str = "\
Usage: date [OPTION]... [+FORMAT]
Display date and time in the given FORMAT.

  -d, --date=STRING          display time described by STRING, not 'now'
  -f, --file=DATEFILE        like --date; once for each line of DATEFILE
  -I[FMT], --iso-8601[=FMT]  output date/time in ISO 8601 format.
                               FMT='date' for date only (the default),
                               'hours', 'minutes', 'seconds', or 'ns'
                               for date and time to the indicated precision.
  -R, --rfc-email            output date and time in RFC 5322 format.
      --rfc-3339=FMT         output date/time in RFC 3339 format.
                               FMT='date', 'seconds', or 'ns'
  -r, --reference=FILE       display the last modification time of FILE
  -u, --utc, --universal     print Coordinated Universal Time (UTC)
  -J, --json                 print the time as a JSON object
      --help        display this help and exit
      --version     output version information and exit

FORMAT controls the output, with the usual strftime conversions (%a %b %c
%d %F %H %j %m %M %N %s %S %T %u %V %Y %z %Z ...) and the flags
'-' (no padding), '_' (pad with spaces), '0' (pad with zeros),
'+' (pad with zeros, sign years), '^' (upper case) and '#' (opposite case).
";

/// GNU's `long_options` and `short_options` (`"d:f:I::r:Rs:u"`), plus the
/// Dogfish `-J, --json`.
const DATE_OPTS: &[OptSpec<&str>] = &[
    OptSpec::both(b'd', "date", HasArg::Required, "date"),
    OptSpec::long("debug", HasArg::No, "debug"),
    OptSpec::both(b'f', "file", HasArg::Required, "file"),
    OptSpec::long("iso-8601", HasArg::Optional, "iso-8601"),
    OptSpec::both(b'r', "reference", HasArg::Required, "reference"),
    OptSpec::long("resolution", HasArg::No, "unsupported"),
    OptSpec::long("rfc-email", HasArg::No, "rfc-email"),
    OptSpec::long("rfc-822", HasArg::No, "rfc-email"),
    OptSpec::long("rfc-2822", HasArg::No, "rfc-email"),
    OptSpec::long("rfc-3339", HasArg::Required, "rfc-3339"),
    OptSpec::both(b's', "set", HasArg::Required, "set"),
    OptSpec::long("uct", HasArg::No, "utc"),
    OptSpec::both(b'u', "utc", HasArg::No, "utc"),
    OptSpec::long("universal", HasArg::No, "utc"),
    OptSpec::long("help", HasArg::No, "help"),
    OptSpec::long("version", HasArg::No, "version"),
    OptSpec::both(b'J', "json", HasArg::No, "json").exact(),
    OptSpec::short(b'I', HasArg::Optional, "iso-8601"),
    OptSpec::short(b'R', HasArg::No, "rfc-email"),
];

const ISO_8601_FORMATS: [(&str, &str); 5] = [
    ("hours", "%Y-%m-%dT%H%:z"),
    ("minutes", "%Y-%m-%dT%H:%M%:z"),
    ("date", "%Y-%m-%d"),
    ("seconds", "%Y-%m-%dT%H:%M:%S%:z"),
    ("ns", "%Y-%m-%dT%H:%M:%S,%N%:z"),
];

const RFC_3339_FORMATS: [(&str, &str); 3] = [
    ("date", "%Y-%m-%d"),
    ("seconds", "%Y-%m-%d %H:%M:%S%:z"),
    ("ns", "%Y-%m-%d %H:%M:%S.%N%:z"),
];

/// Formats one time with `fmt`, or reports it as out of range.
fn render_date(stamp: Stamp, fmt: &[u8]) -> Option<Vec<u8>> {
    let lt = local_time(stamp.sec)?;
    let mut line = format_time(fmt, &lt, stamp.ns, stamp.sec, false);
    line.push(b'\n');
    Some(line)
}

pub fn run_date(args: Vec<OsString>, _ctx: &DogfishContext) -> Result<i32> {
    let usage = |msg: String| Ok(usage_error("date", msg));
    let mut opts = Getopt::with_order(
        args.into_iter().skip(1).collect(),
        DATE_OPTS,
        Order::Permute,
    );
    // `-d` as given, for diagnostics, which echo its bytes.
    let mut date_raw: Option<OsString> = None;
    let mut date_file: Option<String> = None;
    let mut reference: Option<String> = None;
    let mut set_str: Option<String> = None;
    // Formats are bytes: `date +$'\xb0'` prints that byte.
    let mut formats: Vec<Vec<u8>> = Vec::new();
    let mut utc = false;
    let mut json = false;
    while let Some(item) = opts.next() {
        let (key, raw) = match item {
            Ok(kv) => kv,
            Err(e) => return Ok(getopt_error("date", &e)),
        };
        let value = raw.as_deref().map(lossy);
        match key {
            "date" => date_raw = raw,
            "file" => date_file = value,
            "reference" => reference = value,
            "set" => set_str = value,
            "utc" => utc = true,
            "json" => json = true,
            "debug" => {}
            "rfc-email" => formats.push(b"%a, %d %b %Y %H:%M:%S %z".to_vec()),
            "iso-8601" => {
                let spec = value.as_deref().unwrap_or("date");
                match argmatch("date", "--iso-8601", spec, &ISO_8601_FORMATS) {
                    Ok(f) => formats.push(f.as_bytes().to_vec()),
                    Err(status) => return Ok(status),
                }
            }
            "rfc-3339" => {
                let spec = value.as_deref().unwrap_or_default();
                match argmatch("date", "--rfc-3339", spec, &RFC_3339_FORMATS) {
                    Ok(f) => formats.push(f.as_bytes().to_vec()),
                    Err(status) => return Ok(status),
                }
            }
            "unsupported" => return Ok(unsupported("date", &opts.last_spelling())),
            "help" => return Ok(print_help(DATE_HELP)),
            _ => return Ok(print_version("date")),
        }
    }
    let operands = opts.operands();
    let date_str = date_raw.as_deref().map(lossy);
    let sources = [&date_str, &date_file, &reference]
        .iter()
        .filter(|s| s.is_some())
        .count();
    if sources > 1 {
        return usage("the options to specify dates for printing are mutually exclusive".into());
    }
    if set_str.is_some() && sources > 0 {
        return usage("the options to print and set the time may not be used together".into());
    }
    if operands.len() > 1 {
        return usage(format!("extra operand {}", quote_locale(&operands[1])));
    }
    if let Some(op) = operands.first() {
        match op.as_bytes().strip_prefix(b"+") {
            Some(f) => formats.push(f.to_vec()),
            None if sources > 0 || set_str.is_some() => {
                return usage(format!(
                    "the argument {} lacks a leading '+';\n\
                     when using an option to specify date(s), any non-option\n\
                     argument must be a format string beginning with '+'",
                    quote_locale(op)
                ));
            }
            None => set_str = Some(lossy(op)),
        }
    }
    if formats.len() > 1 {
        eprintln!("date: multiple output formats specified");
        return Ok(1);
    }
    if set_str.is_some() {
        eprintln!("date: setting the system clock is not supported");
        return Ok(1);
    }
    if utc {
        // SAFETY: date is single-threaded; GNU likewise switches TZ to UTC0
        // before parsing so that -d strings are read as UTC too.
        unsafe {
            std::env::set_var("TZ", "UTC0");
        }
    }
    // SAFETY: tzset only re-reads TZ into libc's zone state.
    unsafe { tzset() };
    let fmt = formats
        .pop()
        .unwrap_or_else(|| b"%a %b %e %H:%M:%S %Z %Y".to_vec());

    let now = Stamp::now();
    let mut out = stdout::stdout();
    let emit = |out: &mut stdout::Stdout, stamp: Stamp| -> Result<bool> {
        if json {
            let iso = |s: Stamp| {
                render_date(s, b"%Y-%m-%dT%H:%M:%S%:z")
                    .map(|v| String::from_utf8_lossy(v.trim_ascii_end()).into_owned())
            };
            let obj = serde_json::json!({
                "unix": stamp.sec,
                "unix_ms": stamp.sec.saturating_mul(1000) + i64::from(stamp.ns / 1_000_000),
                "utc": utc_time(stamp.sec).map(|lt| {
                    let s = format_time(b"%Y-%m-%dT%H:%M:%S%:z", &lt, stamp.ns, stamp.sec, false);
                    String::from_utf8_lossy(&s).into_owned()
                }),
                "local": iso(stamp),
                "format": render_date(stamp, &fmt)
                    .map(|v| String::from_utf8_lossy(v.trim_ascii_end()).into_owned()),
            });
            writeln!(out, "{}", serde_json::to_string_pretty(&obj)?)?;
            return Ok(true);
        }
        match render_date(stamp, &fmt) {
            Some(line) => {
                ignore_broken_pipe(out.write_all(&line))?;
                Ok(true)
            }
            None => {
                eprintln!("date: time {} is out of range", stamp.sec);
                Ok(false)
            }
        }
    };

    if let Some(path) = &date_file {
        let content = if path == "-" {
            let mut buf = Vec::new();
            io::stdin().read_to_end(&mut buf).map(|_| buf)
        } else {
            std::fs::read(path)
        };
        let content = match content {
            Ok(c) => c,
            Err(e) => {
                eprintln!("date: {}: {}", gnu_quote_file(path), gnu_io_error(&e));
                return Ok(1);
            }
        };
        let mut status = 0;
        let text = String::from_utf8_lossy(&content);
        let body = text.strip_suffix('\n').unwrap_or(&text);
        for line in body.split('\n').filter(|_| !content.is_empty()) {
            match parse_date(line, now) {
                Some(stamp) => {
                    if !emit(&mut out, stamp)? {
                        status = 1;
                    }
                }
                None => {
                    eprintln!("date: invalid date {}", gnu_quote(line));
                    status = 1;
                }
            }
        }
        return Ok(status);
    }

    let stamp = if let Some(path) = &reference {
        match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(t) => Stamp::from_system_time(t),
            Err(e) => {
                eprintln!("date: {}: {}", gnu_quote_file(path), gnu_io_error(&e));
                return Ok(1);
            }
        }
    } else if let Some(d) = &date_str {
        match parse_date(d, now) {
            Some(s) => s,
            None => {
                let shown = date_raw.as_deref().unwrap_or(OsStr::new(d));
                eprintln!("date: invalid date {}", quote_locale(shown));
                return Ok(1);
            }
        }
    } else {
        now
    };
    Ok(if emit(&mut out, stamp)? { 0 } else { 1 })
}
