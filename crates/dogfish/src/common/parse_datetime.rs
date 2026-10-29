//! GNU date strings: a port of gnulib's `parse_datetime` (lib/parse-datetime.y),
//! used by `date -d`/`-f` and `touch -d`.
//!
//! The grammar is gnulib's yacc grammar, parsed by recursive descent with one
//! token of lookahead. Bison resolves the grammar's shift/reduce conflicts by
//! shifting, so each production below continues whenever the lookahead can
//! continue it, and reads a lookahead exactly where Bison's parser would; that
//! keeps the `--debug` trace (and where "stopped at" points) identical.
//!
//! Time zones other than the current one (`TZ="..."` inside the string) are
//! handled the way gnulib's `time_rz` does it on glibc: by switching the `TZ`
//! environment variable around each `localtime`/`mktime` call. The callers
//! are single-threaded.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use crate::common::date_format::{LocalTime, Stamp, format_time, local_time};

unsafe extern "C" {
    /// POSIX `tzset(3)`; the `libc` crate does not bind it on every Unix.
    #[link_name = "tzset"]
    fn c_tzset();
}

/// Re-reads `TZ` into the C library's time-zone state.
pub(crate) fn tzset() {
    // SAFETY: tzset only re-reads TZ into libc's zone state.
    unsafe { c_tzset() }
}

/// A time zone named by a `TZ` string; `None` means `TZ` unset (the system
/// default). gnulib's `timezone_t`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TimeZone(pub(crate) Option<Vec<u8>>);

impl TimeZone {
    /// The zone `TZ` currently names.
    pub(crate) fn from_env() -> Self {
        Self(std::env::var_os("TZ").map(OsString::into_vec))
    }

    /// Runs `f` with `TZ` set to this zone, then restores it.
    fn with<R>(&self, f: impl FnOnce() -> R) -> R {
        let old = std::env::var_os("TZ");
        let want = self.0.as_deref().map(OsStr::from_bytes);
        if old.as_deref() == want {
            return f();
        }
        let set = |v: Option<&OsStr>| {
            // SAFETY: the date tools are single-threaded; nothing reads the
            // environment concurrently. A value never contains NUL: it comes
            // from a C string or from the environment.
            unsafe {
                match v {
                    Some(v) => std::env::set_var("TZ", v),
                    None => std::env::remove_var("TZ"),
                }
            }
            tzset();
        };
        set(want);
        let r = f();
        set(old.as_deref());
        r
    }

    /// `localtime_rz`: the broken-down time and zone abbreviation.
    pub(crate) fn localtime(&self, t: i64) -> Option<LocalTime> {
        // local_time copies the zone name before TZ changes again.
        self.with(|| local_time(t))
    }

    /// `mktime_z`. Failure is seen as `tm_wday` left negative, as in gnulib.
    pub(crate) fn mktime(&self, tm: &mut libc::tm) -> i64 {
        // SAFETY: `tm` is a valid, initialized struct tm.
        self.with(|| unsafe { libc::mktime(tm) } as i64)
    }
}

/// `gmtime_r`: the broken-down UTC time.
pub(crate) fn gmtime(t: i64) -> Option<libc::tm> {
    let tt = libc::time_t::try_from(t).ok()?;
    let mut tm = zero_tm();
    // SAFETY: `tt` and `tm` are valid pointers to a time_t and a struct tm.
    let ok = !unsafe { libc::gmtime_r(&tt, &mut tm) }.is_null();
    ok.then_some(tm)
}

const BILLION: i64 = 1_000_000_000;
const TM_YEAR_BASE: i32 = 1900;

const fn hour(h: i32) -> i32 {
    h * 60 * 60
}

/// An integer and the number of digits it was written with.
#[derive(Clone, Copy, Debug, Default)]
struct TextInt {
    negative: bool,
    value: i64,
    digits: usize,
}

/// Seconds and nanoseconds (`0 <= ns < 10^9`).
#[derive(Clone, Copy, Debug, Default)]
struct Timespec {
    sec: i64,
    ns: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Meridian {
    Am,
    Pm,
    H24,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unit {
    Year,
    Month,
    /// Days, with the multiplier (week = 7, fortnight = 14).
    Day(i64),
    Hour,
    Minute,
    Sec,
}

/// The parser's tokens (Bison's terminals) with their semantic values.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tok {
    Ago(i64),
    Dst,
    Unit(Unit),
    DayShift(i64),
    Day(i32),
    DayZone(i32),
    LocalZone(i32),
    Meridian(Meridian),
    Month(i64),
    Ordinal(i64),
    Zone(i32),
    SNumber(TextIntTok),
    UNumber(TextIntTok),
    SDecimal(TimespecTok),
    UDecimal(TimespecTok),
    /// Military zone J (local time).
    J,
    /// Military zone T, also the ISO 8601 date/time separator.
    T,
    /// Any other character, returned as itself.
    Char(u8),
    /// The lexer's `'?'`: a number overflow or an unknown word.
    Error,
    End,
}

// `TextInt`/`Timespec` wrappers that compare by value, for `Tok: PartialEq`.
#[derive(Clone, Copy, Debug)]
struct TextIntTok(TextInt);
#[derive(Clone, Copy, Debug)]
struct TimespecTok(Timespec);

impl PartialEq for TextIntTok {
    fn eq(&self, o: &Self) -> bool {
        (self.0.negative, self.0.value, self.0.digits) == (o.0.negative, o.0.value, o.0.digits)
    }
}
impl PartialEq for TimespecTok {
    fn eq(&self, o: &Self) -> bool {
        (self.0.sec, self.0.ns) == (o.0.sec, o.0.ns)
    }
}

/// Relative times.
#[derive(Clone, Copy, Default, Debug)]
struct RelTime {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minutes: i64,
    seconds: i64,
    ns: i32,
}

const MONTH_AND_DAY_TABLE: &[(&[u8], Tok)] = &[
    (b"JANUARY", Tok::Month(1)),
    (b"FEBRUARY", Tok::Month(2)),
    (b"MARCH", Tok::Month(3)),
    (b"APRIL", Tok::Month(4)),
    (b"MAY", Tok::Month(5)),
    (b"JUNE", Tok::Month(6)),
    (b"JULY", Tok::Month(7)),
    (b"AUGUST", Tok::Month(8)),
    (b"SEPTEMBER", Tok::Month(9)),
    (b"SEPT", Tok::Month(9)),
    (b"OCTOBER", Tok::Month(10)),
    (b"NOVEMBER", Tok::Month(11)),
    (b"DECEMBER", Tok::Month(12)),
    (b"SUNDAY", Tok::Day(0)),
    (b"MONDAY", Tok::Day(1)),
    (b"TUESDAY", Tok::Day(2)),
    (b"TUES", Tok::Day(2)),
    (b"WEDNESDAY", Tok::Day(3)),
    (b"WEDNES", Tok::Day(3)),
    (b"THURSDAY", Tok::Day(4)),
    (b"THUR", Tok::Day(4)),
    (b"THURS", Tok::Day(4)),
    (b"FRIDAY", Tok::Day(5)),
    (b"SATURDAY", Tok::Day(6)),
];

const TIME_UNITS_TABLE: &[(&[u8], Unit)] = &[
    (b"YEAR", Unit::Year),
    (b"MONTH", Unit::Month),
    (b"FORTNIGHT", Unit::Day(14)),
    (b"WEEK", Unit::Day(7)),
    (b"DAY", Unit::Day(1)),
    (b"HOUR", Unit::Hour),
    (b"MINUTE", Unit::Minute),
    (b"MIN", Unit::Minute),
    (b"SECOND", Unit::Sec),
    (b"SEC", Unit::Sec),
];

const RELATIVE_TIME_TABLE: &[(&[u8], Tok)] = &[
    (b"TOMORROW", Tok::DayShift(1)),
    (b"YESTERDAY", Tok::DayShift(-1)),
    (b"TODAY", Tok::DayShift(0)),
    (b"NOW", Tok::DayShift(0)),
    (b"LAST", Tok::Ordinal(-1)),
    (b"THIS", Tok::Ordinal(0)),
    (b"NEXT", Tok::Ordinal(1)),
    (b"FIRST", Tok::Ordinal(1)),
    // "SECOND" is not an ordinal: it is the unit.
    (b"THIRD", Tok::Ordinal(3)),
    (b"FOURTH", Tok::Ordinal(4)),
    (b"FIFTH", Tok::Ordinal(5)),
    (b"SIXTH", Tok::Ordinal(6)),
    (b"SEVENTH", Tok::Ordinal(7)),
    (b"EIGHTH", Tok::Ordinal(8)),
    (b"NINTH", Tok::Ordinal(9)),
    (b"TENTH", Tok::Ordinal(10)),
    (b"ELEVENTH", Tok::Ordinal(11)),
    (b"TWELFTH", Tok::Ordinal(12)),
    (b"AGO", Tok::Ago(-1)),
    (b"HENCE", Tok::Ago(1)),
];

/// Zones valid everywhere, even for times they would not otherwise label.
const UNIVERSAL_TIME_ZONE_TABLE: &[(&[u8], Tok)] = &[
    (b"GMT", Tok::Zone(hour(0))),
    (b"UT", Tok::Zone(hour(0))),
    (b"UTC", Tok::Zone(hour(0))),
];

/// gnulib's (necessarily incomplete and ambiguous) zone abbreviations.
const TIME_ZONE_TABLE: &[(&[u8], Tok)] = &[
    (b"WET", Tok::Zone(hour(0))),
    (b"WEST", Tok::DayZone(hour(0))),
    (b"BST", Tok::DayZone(hour(0))),
    (b"ART", Tok::Zone(-hour(3))),
    (b"BRT", Tok::Zone(-hour(3))),
    (b"BRST", Tok::DayZone(-hour(3))),
    (b"NST", Tok::Zone(-(hour(3) + 30 * 60))),
    (b"NDT", Tok::DayZone(-(hour(3) + 30 * 60))),
    (b"AST", Tok::Zone(-hour(4))),
    (b"ADT", Tok::DayZone(-hour(4))),
    (b"CLT", Tok::Zone(-hour(4))),
    (b"CLST", Tok::DayZone(-hour(4))),
    (b"EST", Tok::Zone(-hour(5))),
    (b"EDT", Tok::DayZone(-hour(5))),
    (b"CST", Tok::Zone(-hour(6))),
    (b"CDT", Tok::DayZone(-hour(6))),
    (b"MST", Tok::Zone(-hour(7))),
    (b"MDT", Tok::DayZone(-hour(7))),
    (b"PST", Tok::Zone(-hour(8))),
    (b"PDT", Tok::DayZone(-hour(8))),
    (b"AKST", Tok::Zone(-hour(9))),
    (b"AKDT", Tok::DayZone(-hour(9))),
    (b"HST", Tok::Zone(-hour(10))),
    (b"HAST", Tok::Zone(-hour(10))),
    (b"HADT", Tok::DayZone(-hour(10))),
    (b"SST", Tok::Zone(-hour(12))),
    (b"WAT", Tok::Zone(hour(1))),
    (b"CET", Tok::Zone(hour(1))),
    (b"CEST", Tok::DayZone(hour(1))),
    (b"MET", Tok::Zone(hour(1))),
    (b"MEZ", Tok::Zone(hour(1))),
    (b"MEST", Tok::DayZone(hour(1))),
    (b"MESZ", Tok::DayZone(hour(1))),
    (b"EET", Tok::Zone(hour(2))),
    (b"EEST", Tok::DayZone(hour(2))),
    (b"CAT", Tok::Zone(hour(2))),
    (b"SAST", Tok::Zone(hour(2))),
    (b"EAT", Tok::Zone(hour(3))),
    (b"MSK", Tok::Zone(hour(3))),
    (b"MSD", Tok::DayZone(hour(3))),
    (b"IST", Tok::Zone(hour(5) + 30 * 60)),
    (b"SGT", Tok::Zone(hour(8))),
    (b"KST", Tok::Zone(hour(9))),
    (b"JST", Tok::Zone(hour(9))),
    (b"GST", Tok::Zone(hour(10))),
    (b"NZST", Tok::Zone(hour(12))),
    (b"NZDT", Tok::DayZone(hour(12))),
];

/// Military zones A-Z (done the right way round, as RFC 5322 allows).
fn military_zone(c: u8) -> Option<Tok> {
    Some(match c {
        b'A'..=b'I' => Tok::Zone(hour(i32::from(c - b'A') + 1)),
        b'J' => Tok::J,
        b'K'..=b'M' => Tok::Zone(hour(i32::from(c - b'K') + 10)),
        b'N'..=b'S' => Tok::Zone(-hour(i32::from(c - b'N') + 1)),
        b'T' => Tok::T,
        b'U'..=b'Y' => Tok::Zone(-hour(i32::from(c - b'U') + 8)),
        b'Z' => Tok::Zone(0),
        _ => return None,
    })
}

/// C's `isspace` in the C locale.
fn c_isspace(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// gnulib's `time_zone_str`: `+HH`, `+HH:MM` or `+HH:MM:SS`.
fn time_zone_str(tz: i64) -> String {
    let sign = if tz < 0 { '-' } else { '+' };
    let mut s = format!("{sign}{:02}", (tz / 3600).abs());
    let off = (tz % 3600).abs();
    if off != 0 {
        s.push_str(&format!(":{:02}", off / 60));
        if off % 60 != 0 {
            s.push_str(&format!(":{:02}", off % 60));
        }
    }
    s
}

/// gnulib's `tm_year_str`: a `tm_year` as a year, even past `INT_MAX`.
fn tm_year_str(tm_year: i32) -> String {
    let sign = if tm_year < -TM_YEAR_BASE { "-" } else { "" };
    format!(
        "{sign}{:02}{:02}",
        (tm_year / 100 + TM_YEAR_BASE / 100).abs(),
        (tm_year % 100).abs()
    )
}

/// An all-zero `struct tm`, to fill in.
pub(crate) fn zero_tm() -> libc::tm {
    // SAFETY: struct tm is plain data; all-zero is a valid value.
    unsafe { std::mem::zeroed() }
}

/// gnulib's `parser_control`: the parse state and its results.
struct Control {
    day_ordinal: i64,
    day_number: i32,
    local_isdst: i32,
    time_zone: i32,
    meridian: Meridian,
    year: TextInt,
    month: i64,
    day: i64,
    hour: i64,
    minutes: i64,
    seconds: Timespec,
    rel: RelTime,
    timespec_seen: bool,
    rels_seen: bool,
    dates_seen: u64,
    days_seen: u64,
    j_zones_seen: u64,
    local_zones_seen: u64,
    dsts_seen: u64,
    times_seen: u64,
    zones_seen: u64,
    year_seen: bool,
    debug: bool,
    debug_dates_seen: bool,
    debug_days_seen: bool,
    debug_local_zones_seen: bool,
    debug_times_seen: bool,
    debug_zones_seen: bool,
    debug_year_seen: bool,
    debug_ordinal_day_seen: bool,
    /// `tm_isdst` of the current time, the default for a relative time.
    now_isdst: i32,
    /// The local zone's abbreviations with their `tm_isdst`.
    local_time_zone_table: Vec<(Vec<u8>, i32)>,
}

/// Writes one `date: ` debug line (or line start) to stderr.
fn dbg(msg: impl AsRef<[u8]>) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(b"date: ");
    let _ = err.write_all(msg.as_ref());
}

impl Control {
    fn digits_to_date_time(&mut self, t: TextInt) {
        if self.dates_seen > 0
            && self.year.digits == 0
            && !self.rels_seen
            && (self.times_seen > 0 || 2 < t.digits)
        {
            self.year_seen = true;
            self.year = t;
        } else if 4 < t.digits {
            self.dates_seen += 1;
            self.day = t.value % 100;
            self.month = (t.value / 100) % 100;
            self.year.value = t.value / 10000;
            self.year.digits = t.digits - 4;
        } else {
            self.times_seen += 1;
            if t.digits <= 2 {
                self.hour = t.value;
                self.minutes = 0;
            } else {
                self.hour = t.value / 100;
                self.minutes = t.value % 100;
            }
            self.seconds = Timespec::default();
            self.meridian = Meridian::H24;
        }
    }

    /// Adds `factor` (1 or -1) times `rel`; `None` on overflow.
    fn apply_relative_time(&mut self, rel: RelTime, factor: i64) -> Option<()> {
        let r = &mut self.rel;
        if factor < 0 {
            r.ns = r.ns.checked_sub(rel.ns)?;
            r.seconds = r.seconds.checked_sub(rel.seconds)?;
            r.minutes = r.minutes.checked_sub(rel.minutes)?;
            r.hour = r.hour.checked_sub(rel.hour)?;
            r.day = r.day.checked_sub(rel.day)?;
            r.month = r.month.checked_sub(rel.month)?;
            r.year = r.year.checked_sub(rel.year)?;
        } else {
            r.ns = r.ns.checked_add(rel.ns)?;
            r.seconds = r.seconds.checked_add(rel.seconds)?;
            r.minutes = r.minutes.checked_add(rel.minutes)?;
            r.hour = r.hour.checked_add(rel.hour)?;
            r.day = r.day.checked_add(rel.day)?;
            r.month = r.month.checked_add(rel.month)?;
            r.year = r.year.checked_add(rel.year)?;
        }
        self.rels_seen = true;
        Some(())
    }

    fn set_hhmmss(&mut self, hour: i64, minutes: i64, sec: i64, ns: i64) {
        self.hour = hour;
        self.minutes = minutes;
        self.seconds = Timespec { sec, ns };
    }

    /// `[+-]hh`, `[+-]hhmm` or (with `mm >= 0`) `[+-]hh:mm` as the zone.
    fn time_zone_hhmm(&mut self, s: TextInt, mm: i64) -> Option<()> {
        let mut value = s.value;
        if s.digits <= 2 && mm < 0 {
            value *= 100;
        }
        let n_minutes = if mm < 0 {
            (value / 100) * 60 + value % 100
        } else {
            let m = value.checked_mul(60)?;
            if s.negative {
                m.checked_sub(mm)?
            } else {
                m.checked_add(mm)?
            }
        };
        if !(-24 * 60..=24 * 60).contains(&n_minutes) {
            return None;
        }
        self.time_zone = (n_minutes * 60) as i32;
        Some(())
    }

    /// gnulib's `str_days`: e.g. `last Wed`, `Thu`.
    fn str_days(&self) -> String {
        const ORDINAL_VALUES: [&str; 14] = [
            "last",
            "this",
            "next/first",
            "(SECOND)",
            "third",
            "fourth",
            "fifth",
            "sixth",
            "seventh",
            "eight",
            "ninth",
            "tenth",
            "eleventh",
            "twelfth",
        ];
        const DAYS_VALUES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let mut s = String::new();
        if self.debug_ordinal_day_seen {
            if (-1..=12).contains(&self.day_ordinal) {
                s.push_str(ORDINAL_VALUES[(self.day_ordinal + 1) as usize]);
            } else {
                s.push_str(&self.day_ordinal.to_string());
            }
        }
        if let Some(name) = usize::try_from(self.day_number)
            .ok()
            .and_then(|d| DAYS_VALUES.get(d))
        {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(name);
        }
        s
    }

    /// gnulib's `debug_print_current_time`: the newly parsed parts.
    fn debug_print_current_time(&mut self, item: &str) {
        if !self.debug {
            return;
        }
        let mut s = format!("parsed {item} part: ");
        let mut space = false;
        if self.dates_seen > 0 && !self.debug_dates_seen {
            s.push_str(&format!(
                "(Y-M-D) {:04}-{:02}-{:02}",
                self.year.value, self.month, self.day
            ));
            self.debug_dates_seen = true;
            space = true;
        }
        if self.year_seen != self.debug_year_seen {
            if space {
                s.push(' ');
            }
            s.push_str(&format!("year: {:04}", self.year.value));
            self.debug_year_seen = self.year_seen;
            space = true;
        }
        if self.times_seen > 0 && !self.debug_times_seen {
            if space {
                s.push(' ');
            }
            s.push_str(&format!(
                "{:02}:{:02}:{:02}",
                self.hour, self.minutes, self.seconds.sec
            ));
            if self.seconds.ns != 0 {
                s.push_str(&format!(".{:09}", self.seconds.ns));
            }
            if self.meridian == Meridian::Pm {
                s.push_str("pm");
            }
            self.debug_times_seen = true;
            space = true;
        }
        if self.days_seen > 0 && !self.debug_days_seen {
            if space {
                s.push(' ');
            }
            s.push_str(&format!(
                "{} (day ordinal={} number={})",
                self.str_days(),
                self.day_ordinal,
                self.day_number
            ));
            self.debug_days_seen = true;
            space = true;
        }
        if self.local_zones_seen > 0 && !self.debug_local_zones_seen {
            if space {
                s.push(' ');
            }
            s.push_str(&format!("isdst={}", self.local_isdst));
            if self.dsts_seen > 0 {
                s.push_str(" DST");
            }
            self.debug_local_zones_seen = true;
            space = true;
        }
        if self.zones_seen > 0 && !self.debug_zones_seen {
            if space {
                s.push(' ');
            }
            s.push_str(&format!("UTC{}", time_zone_str(self.time_zone.into())));
            self.debug_zones_seen = true;
            space = true;
        }
        if self.timespec_seen {
            if space {
                s.push(' ');
            }
            s.push_str(&format!("number of seconds: {}", self.seconds.sec));
        }
        s.push('\n');
        dbg(s);
    }

    /// gnulib's `debug_print_relative_time`.
    fn debug_print_relative_time(&self, item: &str) {
        if !self.debug {
            return;
        }
        let r = &self.rel;
        let mut s = format!("parsed {item} part: ");
        if r.year == 0
            && r.month == 0
            && r.day == 0
            && r.hour == 0
            && r.minutes == 0
            && r.seconds == 0
            && r.ns == 0
        {
            s.push_str("today/this/now\n");
            dbg(s);
            return;
        }
        let mut space = false;
        for (val, name) in [
            (r.year, "year(s)"),
            (r.month, "month(s)"),
            (r.day, "day(s)"),
            (r.hour, "hour(s)"),
            (r.minutes, "minutes"),
            (r.seconds, "seconds"),
            (i64::from(r.ns), "nanoseconds"),
        ] {
            if val != 0 {
                if space {
                    s.push(' ');
                }
                s.push_str(&format!("{val:+} {name}"));
                space = true;
            }
        }
        s.push('\n');
        dbg(s);
    }

    /// gnulib's `debug_strfdatetime`: `(Y-M-D) YYYY-MM-DD HH:MM:SS`, plus the
    /// parsed zone when there is one (and `with_zone`).
    fn debug_strfdatetime(&self, tm: &libc::tm, with_zone: bool) -> String {
        let lt = LocalTime {
            tm: *tm,
            zone: Vec::new(),
        };
        let mut s =
            String::from_utf8_lossy(&format_time(b"(Y-M-D) %Y-%m-%d %H:%M:%S", &lt, 0, 0, false))
                .into_owned();
        if with_zone && self.zones_seen > 0 {
            let mut tz = i64::from(self.time_zone);
            if self.local_zones_seen > 0 && self.zones_seen == 0 && 0 < self.local_isdst {
                tz += 60 * 60;
            }
            s.push_str(&format!(" TZ={}", time_zone_str(tz)));
        }
        s
    }

    /// gnulib's `lookup_zone`.
    fn lookup_zone(&self, name: &[u8]) -> Option<Tok> {
        if let Some((_, t)) = UNIVERSAL_TIME_ZONE_TABLE.iter().find(|(n, _)| *n == name) {
            return Some(*t);
        }
        // Local abbreviations first: they are more likely to be right.
        if let Some((_, isdst)) = self
            .local_time_zone_table
            .iter()
            .find(|(n, _)| n.as_slice() == name)
        {
            return Some(Tok::LocalZone(*isdst));
        }
        TIME_ZONE_TABLE
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
    }

    /// gnulib's `lookup_word`. Like the C code, it upper-cases `word` and may
    /// drop its periods in place (which shows in the "unknown word" message).
    fn lookup_word(&self, word: &mut Vec<u8>) -> Option<Tok> {
        word.make_ascii_uppercase();
        match word.as_slice() {
            b"AM" | b"A.M." => return Some(Tok::Meridian(Meridian::Am)),
            b"PM" | b"P.M." => return Some(Tok::Meridian(Meridian::Pm)),
            _ => {}
        }
        let len = word.len();
        let abbrev = len == 3 || (len == 4 && word[3] == b'.');
        for (name, tok) in MONTH_AND_DAY_TABLE {
            let hit = if abbrev {
                name.starts_with(&word[..3])
            } else {
                name == word
            };
            if hit {
                return Some(*tok);
            }
        }
        if let Some(t) = self.lookup_zone(word) {
            return Some(t);
        }
        if word == b"DST" {
            return Some(Tok::Dst);
        }
        let unit = |w: &[u8]| {
            TIME_UNITS_TABLE
                .iter()
                .find(|(n, _)| *n == w)
                .map(|(_, u)| Tok::Unit(*u))
        };
        if let Some(t) = unit(word) {
            return Some(t);
        }
        // Strip off any plural and try the units table again.
        if word.last() == Some(&b'S')
            && let Some(t) = unit(&word[..len - 1])
        {
            return Some(t);
        }
        if let Some((_, t)) = RELATIVE_TIME_TABLE.iter().find(|(n, _)| *n == word) {
            return Some(*t);
        }
        if len == 1 {
            return military_zone(word[0]);
        }
        // Drop out any periods and try the time zone table again.
        let period_found = word.contains(&b'.');
        word.retain(|&c| c != b'.');
        if period_found {
            return self.lookup_zone(word);
        }
        None
    }
}

/// The lexer and parser over one date string.
struct Parser<'a> {
    input: &'a [u8],
    /// `pc->input`: how far the lexer has read. May pass the end by one
    /// after the terminating NUL, as in C.
    pos: usize,
    lookahead: Option<Tok>,
    pc: Control,
}

impl Parser<'_> {
    fn at(&self, i: usize) -> u8 {
        self.input.get(i).copied().unwrap_or(0)
    }

    /// gnulib's `yylex`.
    fn lex(&mut self) -> Tok {
        loop {
            while c_isspace(self.at(self.pos)) {
                self.pos += 1;
            }
            let mut c = self.at(self.pos);
            if c.is_ascii_digit() || c == b'-' || c == b'+' {
                let mut p = self.pos;
                let sign: i64 = if c == b'-' || c == b'+' {
                    let sign = if c == b'-' { -1 } else { 1 };
                    loop {
                        p += 1;
                        self.pos = p;
                        c = self.at(p);
                        if !c_isspace(c) {
                            break;
                        }
                    }
                    if !c.is_ascii_digit() {
                        // Skip the sign.
                        continue;
                    }
                    sign
                } else {
                    0
                };
                let mut value: i64 = 0;
                loop {
                    let d = i64::from(c - b'0');
                    let Some(v) = value.checked_mul(10).and_then(|v| {
                        if sign < 0 {
                            v.checked_sub(d)
                        } else {
                            v.checked_add(d)
                        }
                    }) else {
                        return Tok::Error;
                    };
                    value = v;
                    p += 1;
                    c = self.at(p);
                    if !c.is_ascii_digit() {
                        break;
                    }
                }
                if (c == b'.' || c == b',') && self.at(p + 1).is_ascii_digit() {
                    let mut s = value;
                    // Accumulate the fraction, to nanosecond precision.
                    p += 1;
                    let mut ns = i64::from(self.at(p) - b'0');
                    p += 1;
                    for _ in 2..=9 {
                        ns *= 10;
                        if self.at(p).is_ascii_digit() {
                            ns += i64::from(self.at(p) - b'0');
                            p += 1;
                        }
                    }
                    // Skip excess digits, truncating toward -Infinity.
                    if sign < 0 {
                        while self.at(p).is_ascii_digit() {
                            if self.at(p) != b'0' {
                                ns += 1;
                                break;
                            }
                            p += 1;
                        }
                    }
                    while self.at(p).is_ascii_digit() {
                        p += 1;
                    }
                    // tv_nsec is a positive offset even when tv_sec < 0.
                    if sign < 0 && ns != 0 {
                        let Some(s1) = s.checked_sub(1) else {
                            return Tok::Error;
                        };
                        s = s1;
                        ns = BILLION - ns;
                    }
                    self.pos = p;
                    let ts = TimespecTok(Timespec { sec: s, ns });
                    return if sign != 0 {
                        Tok::SDecimal(ts)
                    } else {
                        Tok::UDecimal(ts)
                    };
                }
                let t = TextIntTok(TextInt {
                    negative: sign < 0,
                    value,
                    digits: p - self.pos,
                });
                self.pos = p;
                return if sign != 0 {
                    Tok::SNumber(t)
                } else {
                    Tok::UNumber(t)
                };
            }
            if c.is_ascii_alphabetic() {
                let mut word = Vec::new();
                loop {
                    // The C buffer holds 19 characters; the rest are dropped.
                    if word.len() < 19 {
                        word.push(c);
                    }
                    self.pos += 1;
                    c = self.at(self.pos);
                    if !(c.is_ascii_alphabetic() || c == b'.') {
                        break;
                    }
                }
                return match self.pc.lookup_word(&mut word) {
                    Some(t) => t,
                    None => {
                        if self.pc.debug {
                            let mut m = b"error: unknown word '".to_vec();
                            m.extend_from_slice(&word);
                            m.extend_from_slice(b"'\n");
                            dbg(m);
                        }
                        Tok::Error
                    }
                };
            }
            if c != b'(' {
                self.pos += 1;
                return if c == 0 { Tok::End } else { Tok::Char(c) };
            }
            let mut count = 0i64;
            loop {
                let c = self.at(self.pos);
                self.pos += 1;
                if c == 0 {
                    return Tok::End;
                }
                if c == b'(' {
                    count += 1;
                } else if c == b')' {
                    count -= 1;
                }
                if count == 0 {
                    break;
                }
            }
        }
    }

    fn peek(&mut self) -> Tok {
        match self.lookahead {
            Some(t) => t,
            None => {
                let t = self.lex();
                self.lookahead = Some(t);
                t
            }
        }
    }

    fn take(&mut self) -> Tok {
        let t = self.peek();
        self.lookahead = None;
        t
    }

    /// Takes the lookahead if it is a unit.
    fn take_unit(&mut self) -> Option<Unit> {
        match self.peek() {
            Tok::Unit(u) => {
                self.take();
                Some(u)
            }
            _ => None,
        }
    }

    fn take_unumber(&mut self) -> Option<TextInt> {
        match self.take() {
            Tok::UNumber(n) => Some(n.0),
            _ => None,
        }
    }

    /// `unsigned_seconds`.
    fn take_unsigned_seconds(&mut self) -> Option<Timespec> {
        match self.take() {
            Tok::UDecimal(t) => Some(t.0),
            Tok::UNumber(n) => Some(Timespec {
                sec: n.0.value,
                ns: 0,
            }),
            _ => None,
        }
    }

    /// `relunit` with multiplier `n`.
    fn relunit(unit: Unit, n: i64) -> Option<RelTime> {
        let mut r = RelTime::default();
        match unit {
            Unit::Year => r.year = n,
            Unit::Month => r.month = n,
            Unit::Day(mult) => r.day = n.checked_mul(mult)?,
            Unit::Hour => r.hour = n,
            Unit::Minute => r.minutes = n,
            Unit::Sec => r.seconds = n,
        }
        Some(r)
    }

    /// `rel: relunit tAGO | relunit`, then the item's debug trace.
    fn rel(&mut self, r: RelTime) -> Option<()> {
        let factor = match self.peek() {
            Tok::Ago(f) => {
                self.take();
                f
            }
            _ => 1,
        };
        self.pc.apply_relative_time(r, factor)?;
        self.pc.debug_print_relative_time("relative");
        Some(())
    }

    /// `o_colon_minutes`: `: mm`, or -1.
    fn o_colon_minutes(&mut self) -> Option<i64> {
        if self.peek() == Tok::Char(b':') {
            self.take();
            return Some(self.take_unumber()?.value);
        }
        Some(-1)
    }

    /// `zone_offset: tSNUMBER o_colon_minutes`, the number already taken.
    fn zone_offset(&mut self, s: TextInt) -> Option<()> {
        let mm = self.o_colon_minutes()?;
        self.pc.zones_seen += 1;
        self.pc.time_zone_hhmm(s, mm)
    }

    /// `o_zone_offset`.
    fn o_zone_offset(&mut self) -> Option<()> {
        if let Tok::SNumber(s) = self.peek() {
            self.take();
            self.zone_offset(s.0)?;
        }
        Some(())
    }

    /// The `item` that starts with `tok`.
    fn item(&mut self, tok: Tok) -> Option<()> {
        match tok {
            Tok::UNumber(n) => self.unumber_item(n.0),
            Tok::SNumber(s) => {
                // relunit_snumber
                let unit = self.take_unit()?;
                let r = Self::relunit(unit, s.0.value)?;
                self.rel(r)
            }
            Tok::SDecimal(d) | Tok::UDecimal(d) => {
                if self.take() != Tok::Unit(Unit::Sec) {
                    return None;
                }
                let r = RelTime {
                    seconds: d.0.sec,
                    ns: d.0.ns as i32,
                    ..RelTime::default()
                };
                self.rel(r)
            }
            Tok::Month(m) => {
                match self.take() {
                    Tok::SNumber(d) => {
                        // E.g. JUN-17-1992.
                        let Tok::SNumber(y) = self.take() else {
                            return None;
                        };
                        self.pc.month = m;
                        self.pc.day = 0i64.checked_sub(d.0.value)?;
                        self.pc.year.value = 0i64.checked_sub(y.0.value)?;
                        self.pc.year.digits = y.0.digits;
                    }
                    Tok::UNumber(d) => {
                        if self.peek() == Tok::Char(b',') {
                            self.take();
                            let y = self.take_unumber()?;
                            self.pc.year = y;
                        }
                        self.pc.month = m;
                        self.pc.day = d.0.value;
                    }
                    _ => return None,
                }
                self.date_seen()
            }
            Tok::Day(d) => {
                if self.peek() == Tok::Char(b',') {
                    self.take();
                }
                self.pc.day_ordinal = 0;
                self.pc.day_number = d;
                self.day_seen()
            }
            Tok::Ordinal(o) => match self.take() {
                Tok::Day(d) => {
                    self.pc.day_ordinal = o;
                    self.pc.day_number = d;
                    self.pc.debug_ordinal_day_seen = true;
                    self.day_seen()
                }
                Tok::Unit(u) => {
                    let r = Self::relunit(u, o)?;
                    self.rel(r)
                }
                _ => None,
            },
            Tok::Unit(u) => {
                let r = Self::relunit(u, 1)?;
                self.rel(r)
            }
            Tok::DayShift(d) => {
                let r = RelTime {
                    day: d,
                    ..RelTime::default()
                };
                self.pc.apply_relative_time(r, 1)?;
                self.pc.debug_print_relative_time("relative");
                Some(())
            }
            Tok::LocalZone(isdst) => {
                if self.peek() == Tok::Dst {
                    self.take();
                    self.pc.local_isdst = 1;
                    self.pc.dsts_seen += 1;
                } else {
                    self.pc.local_isdst = isdst;
                }
                self.pc.local_zones_seen += 1;
                self.pc.debug_print_current_time("local_zone");
                Some(())
            }
            Tok::J => {
                self.pc.j_zones_seen += 1;
                self.pc.debug_print_current_time("J");
                Some(())
            }
            Tok::Zone(z) => {
                match self.peek() {
                    Tok::SNumber(s) => {
                        self.take();
                        if let Some(unit) = self.take_unit() {
                            self.pc.time_zone = z;
                            let r = Self::relunit(unit, s.0.value)?;
                            self.pc.apply_relative_time(r, 1)?;
                            self.pc.debug_print_relative_time("relative");
                        } else {
                            let mm = self.o_colon_minutes()?;
                            self.pc.time_zone_hhmm(s.0, mm)?;
                            self.pc.time_zone = self.pc.time_zone.checked_add(z)?;
                        }
                    }
                    Tok::Dst => {
                        self.take();
                        self.pc.time_zone = z + 60 * 60;
                    }
                    _ => self.pc.time_zone = z,
                }
                self.zone_seen()
            }
            Tok::T => {
                if let Tok::SNumber(s) = self.peek() {
                    self.take();
                    let unit = self.take_unit()?;
                    self.pc.time_zone = -hour(7);
                    let r = Self::relunit(unit, s.0.value)?;
                    self.pc.apply_relative_time(r, 1)?;
                    self.pc.debug_print_relative_time("relative");
                } else {
                    self.pc.time_zone = -hour(7);
                }
                self.zone_seen()
            }
            Tok::DayZone(z) => {
                self.pc.time_zone = z + 60 * 60;
                self.zone_seen()
            }
            _ => None,
        }
    }

    fn date_seen(&mut self) -> Option<()> {
        self.pc.dates_seen += 1;
        self.pc.debug_print_current_time("date");
        Some(())
    }

    fn day_seen(&mut self) -> Option<()> {
        self.pc.days_seen += 1;
        self.pc.debug_print_current_time("day");
        Some(())
    }

    fn zone_seen(&mut self) -> Option<()> {
        self.pc.zones_seen += 1;
        self.pc.debug_print_current_time("zone");
        Some(())
    }

    fn time_seen(&mut self) -> Option<()> {
        self.pc.times_seen += 1;
        self.pc.debug_print_current_time("time");
        Some(())
    }

    /// The items that start with an unsigned number.
    fn unumber_item(&mut self, n: TextInt) -> Option<()> {
        match self.peek() {
            Tok::Meridian(m) => {
                self.take();
                self.pc.set_hhmmss(n.value, 0, 0, 0);
                self.pc.meridian = m;
                self.time_seen()
            }
            Tok::Char(b':') => {
                self.take();
                let mm = self.take_unumber()?;
                let mut secs = Timespec::default();
                if self.peek() == Tok::Char(b':') {
                    self.take();
                    secs = self.take_unsigned_seconds()?;
                }
                if let Tok::Meridian(m) = self.peek() {
                    self.take();
                    self.pc.set_hhmmss(n.value, mm.value, secs.sec, secs.ns);
                    self.pc.meridian = m;
                } else {
                    self.o_zone_offset()?;
                    self.pc.set_hhmmss(n.value, mm.value, secs.sec, secs.ns);
                    self.pc.meridian = Meridian::H24;
                }
                self.time_seen()
            }
            Tok::SNumber(s) => {
                self.take();
                match self.peek() {
                    Tok::SNumber(d) => {
                        self.take();
                        self.iso_8601_date(n, s.0, d.0)?;
                        if self.peek() == Tok::T {
                            self.take();
                            self.iso_8601_time()?;
                            self.pc.times_seen += 1;
                            self.pc.dates_seen += 1;
                            self.pc.debug_print_current_time("datetime");
                            Some(())
                        } else {
                            self.date_seen()
                        }
                    }
                    Tok::Unit(u) => {
                        // "YYYYMMDD +N days": a number, then a relative offset.
                        self.take();
                        self.pc.digits_to_date_time(n);
                        let r = Self::relunit(u, s.0.value)?;
                        self.pc.apply_relative_time(r, 1)?;
                        self.pc.debug_print_relative_time("hybrid");
                        Some(())
                    }
                    _ => {
                        self.zone_offset(s.0)?;
                        self.pc.set_hhmmss(n.value, 0, 0, 0);
                        self.pc.meridian = Meridian::H24;
                        self.time_seen()
                    }
                }
            }
            Tok::Char(b'/') => {
                self.take();
                let second = self.take_unumber()?;
                if self.peek() == Tok::Char(b'/') {
                    self.take();
                    let third = self.take_unumber()?;
                    // YYYY/MM/DD only for legacy machine-generated dates.
                    if 4 <= n.digits {
                        if self.pc.debug {
                            dbg(format!(
                                "warning: value {} has {} digits. Assuming YYYY/MM/DD\n",
                                n.value, n.digits
                            ));
                        }
                        self.pc.year = n;
                        self.pc.month = second.value;
                        self.pc.day = third.value;
                    } else {
                        if self.pc.debug {
                            dbg(format!(
                                "warning: value {} has less than 4 digits. Assuming MM/DD/YY[YY]\n",
                                n.value
                            ));
                        }
                        self.pc.month = n.value;
                        self.pc.day = second.value;
                        self.pc.year = third;
                    }
                } else {
                    self.pc.month = n.value;
                    self.pc.day = second.value;
                }
                self.date_seen()
            }
            Tok::Month(m) => {
                self.take();
                match self.peek() {
                    Tok::SNumber(y) => {
                        // E.g. 17-JUN-1992.
                        self.take();
                        self.pc.year.value = 0i64.checked_sub(y.0.value)?;
                        self.pc.year.digits = y.0.digits;
                    }
                    Tok::UNumber(y) => {
                        self.take();
                        self.pc.year = y.0;
                    }
                    _ => {}
                }
                self.pc.day = n.value;
                self.pc.month = m;
                self.date_seen()
            }
            Tok::Day(d) => {
                self.take();
                self.pc.day_ordinal = n.value;
                self.pc.day_number = d;
                self.pc.debug_ordinal_day_seen = true;
                self.day_seen()
            }
            Tok::Unit(u) => {
                self.take();
                let r = Self::relunit(u, n.value)?;
                self.rel(r)
            }
            _ => {
                self.pc.digits_to_date_time(n);
                self.pc.debug_print_current_time("number");
                Some(())
            }
        }
    }

    /// `iso_8601_date: tUNUMBER tSNUMBER tSNUMBER` (YYYY-MM-DD).
    fn iso_8601_date(&mut self, y: TextInt, m: TextInt, d: TextInt) -> Option<()> {
        self.pc.year = y;
        self.pc.month = 0i64.checked_sub(m.value)?;
        self.pc.day = 0i64.checked_sub(d.value)?;
        Some(())
    }

    /// `iso_8601_time`, after the `T` of a `datetime`.
    fn iso_8601_time(&mut self) -> Option<()> {
        let h = self.take_unumber()?;
        match self.take() {
            Tok::SNumber(s) => {
                self.zone_offset(s.0)?;
                self.pc.set_hhmmss(h.value, 0, 0, 0);
            }
            Tok::Char(b':') => {
                let mm = self.take_unumber()?;
                let mut secs = Timespec::default();
                if self.peek() == Tok::Char(b':') {
                    self.take();
                    secs = self.take_unsigned_seconds()?;
                }
                self.o_zone_offset()?;
                self.pc.set_hhmmss(h.value, mm.value, secs.sec, secs.ns);
            }
            _ => return None,
        }
        self.pc.meridian = Meridian::H24;
        Some(())
    }

    /// `spec: timespec | items`.
    fn parse(&mut self) -> Option<()> {
        if self.peek() == Tok::Char(b'@') {
            self.take();
            let secs = match self.take() {
                Tok::SDecimal(t) | Tok::UDecimal(t) => t.0,
                Tok::SNumber(n) | Tok::UNumber(n) => Timespec {
                    sec: n.0.value,
                    ns: 0,
                },
                _ => return None,
            };
            self.pc.seconds = secs;
            self.pc.timespec_seen = true;
            self.pc.debug_print_current_time("number of seconds");
            return (self.take() == Tok::End).then_some(());
        }
        loop {
            match self.take() {
                Tok::End => return Some(()),
                tok => self.item(tok)?,
            }
        }
    }
}

/// gnulib's `to_hour`.
fn to_hour(hours: i64, meridian: Meridian) -> i64 {
    match meridian {
        Meridian::H24 => {
            if (0..24).contains(&hours) {
                hours
            } else {
                -1
            }
        }
        Meridian::Am => {
            if 0 < hours && hours < 12 {
                hours
            } else if hours == 12 {
                0
            } else {
                -1
            }
        }
        Meridian::Pm => {
            if 0 < hours && hours < 12 {
                hours + 12
            } else if hours == 12 {
                12
            } else {
                -1
            }
        }
    }
}

/// gnulib's `to_tm_year`: a year as `tm_year`, mapping two-digit years
/// 00-68 to 2000-2068 and 69-99 to 1969-1999.
fn to_tm_year(textyear: TextInt, debug: bool) -> Option<i32> {
    let mut year = textyear.value;
    if 0 <= year && textyear.digits == 2 {
        year += if year < 69 { 2000 } else { 1900 };
        if debug {
            dbg(format!(
                "warning: adjusting year value {} to {year}\n",
                textyear.value
            ));
        }
    }
    let tm_year = if year < 0 {
        i64::from(-TM_YEAR_BASE).checked_sub(year)
    } else {
        year.checked_sub(TM_YEAR_BASE.into())
    }
    .and_then(|y| i32::try_from(y).ok());
    if tm_year.is_none() && debug {
        dbg(format!("error: out-of-range year {year}\n"));
    }
    tm_year
}

/// gnulib's `mktime_ok`: mktime succeeded and kept the main fields.
fn mktime_ok(tm0: &libc::tm, tm1: &libc::tm) -> bool {
    tm1.tm_wday >= 0
        && (
            tm0.tm_sec,
            tm0.tm_min,
            tm0.tm_hour,
            tm0.tm_mday,
            tm0.tm_mon,
            tm0.tm_year,
        ) == (
            tm1.tm_sec,
            tm1.tm_min,
            tm1.tm_hour,
            tm1.tm_mday,
            tm1.tm_mon,
            tm1.tm_year,
        )
}

/// `tm_gmtoff`, which is `c_long`.
#[allow(clippy::useless_conversion)]
fn gmtoff(tm: &libc::tm) -> i64 {
    i64::from(tm.tm_gmtoff)
}

fn debug_strftime(tm: &libc::tm) -> String {
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn debug_strfdate(tm: &libc::tm) -> String {
    format!(
        "(Y-M-D) {}-{:02}-{:02}",
        tm_year_str(tm.tm_year),
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

/// gnulib's `debug_mktime_not_ok`: why mktime refused the parsed time.
fn debug_mktime_not_ok(tm0: &libc::tm, tm1: &libc::tm, pc: &Control, time_zone_seen: bool) {
    let eq_sec = tm0.tm_sec == tm1.tm_sec;
    let eq_min = tm0.tm_min == tm1.tm_min;
    let eq_hour = tm0.tm_hour == tm1.tm_hour;
    let eq_mday = tm0.tm_mday == tm1.tm_mday;
    let eq_month = tm0.tm_mon == tm1.tm_mon;
    let eq_year = tm0.tm_year == tm1.tm_year;
    let dst_shift = eq_sec && eq_min && !eq_hour && eq_mday && eq_month && eq_year;
    if !pc.debug {
        return;
    }
    dbg("error: invalid date/time value:\n");
    dbg(format!(
        "    user provided time: '{}'\n",
        pc.debug_strfdatetime(tm0, true)
    ));
    dbg(format!(
        "       normalized time: '{}'\n",
        pc.debug_strfdatetime(tm1, true)
    ));
    let mark = |eq: bool, s: &'static str| if eq { "" } else { s };
    let line = format!(
        "                                 {:>4} {:>2} {:>2} {:>2} {:>2} {:>2}",
        mark(eq_year, "----"),
        mark(eq_month, "--"),
        mark(eq_mday, "--"),
        mark(eq_hour, "--"),
        mark(eq_min, "--"),
        mark(eq_sec, "--"),
    );
    dbg(format!("{}\n", line.trim_end_matches(' ')));
    dbg("     possible reasons:\n");
    if dst_shift {
        dbg("       nonexistent due to daylight-saving time;\n");
    }
    if !eq_mday && !eq_month {
        dbg("       invalid day/month combination;\n");
    }
    dbg("       numeric values overflow;\n");
    dbg(if time_zone_seen {
        "       incorrect timezone\n"
    } else {
        "       missing timezone\n"
    });
}

/// Parses `input` like `parse_datetime` with the zone `TZ` names: relative
/// to `now`, `None` meaning "invalid date".
pub(crate) fn parse_datetime(input: &[u8], now: Stamp) -> Option<Stamp> {
    let tz = TimeZone::from_env();
    parse_datetime2(input, now, false, &tz)
}

/// gnulib's `parse_datetime2`: like [`parse_datetime`] in the default zone
/// `tz` (which `date` reports in its `--debug` trace), optionally tracing the
/// parse to stderr.
pub(crate) fn parse_datetime2(
    input: &[u8],
    now: Stamp,
    debug: bool,
    tzdefault: &TimeZone,
) -> Option<Stamp> {
    // The string ends at a NUL, as in C.
    let input = &input[..input.iter().position(|&c| c == 0).unwrap_or(input.len())];
    let mut p = 0;
    while input.get(p).copied().is_some_and(c_isspace) {
        p += 1;
    }

    let mut tz = tzdefault.clone();
    let mut tz_from_string = false;
    if input[p..].starts_with(b"TZ=\"") {
        let base = p + 4;
        let mut s = base;
        while s < input.len() {
            match input[s] {
                b'\\' => {
                    s += 1;
                    if !matches!(input.get(s), Some(b'\\' | b'"')) {
                        break;
                    }
                }
                b'"' => {
                    let mut name = Vec::new();
                    let mut i = base;
                    while input[i] != b'"' {
                        if input[i] == b'\\' {
                            i += 1;
                        }
                        name.push(input[i]);
                        i += 1;
                    }
                    tz = TimeZone(Some(name));
                    tz_from_string = true;
                    p = s + 1;
                    while input.get(p).copied().is_some_and(c_isspace) {
                        p += 1;
                    }
                    break;
                }
                _ => {}
            }
            s += 1;
        }
    }
    let tzstring = tz.0.clone();

    let tmp = tz.localtime(now.sec)?;
    // An empty string means "0", so that it is valid even in a DST change.
    let rest: &[u8] = if p >= input.len() { b"0" } else { &input[p..] };

    let year = i64::from(tmp.tm.tm_year) + i64::from(TM_YEAR_BASE);
    let mut local_time_zone_table = vec![(tmp.zone.clone(), tmp.tm.tm_isdst)];
    // Probe the next three calendar quarters for the other DST setting.
    for quarter in 1..=3 {
        let Some(probe) = now.sec.checked_add(quarter * 90 * 24 * 60 * 60) else {
            break;
        };
        if let Some(pt) = tz.localtime(probe)
            && pt.tm.tm_isdst != tmp.tm.tm_isdst
        {
            local_time_zone_table.push((pt.zone, pt.tm.tm_isdst));
            break;
        }
    }
    if tmp.zone.is_empty() {
        local_time_zone_table.clear();
    }
    if local_time_zone_table.len() == 2 && local_time_zone_table[0].0 == local_time_zone_table[1].0
    {
        // Standard and daylight time share the abbreviation: it says nothing
        // about DST.
        local_time_zone_table[0].1 = -1;
        local_time_zone_table.truncate(1);
    }

    let mut parser = Parser {
        input: rest,
        pos: 0,
        lookahead: None,
        pc: Control {
            day_ordinal: 0,
            day_number: 0,
            local_isdst: 0,
            time_zone: 0,
            meridian: Meridian::H24,
            year: TextInt {
                negative: false,
                value: year,
                digits: 0,
            },
            month: i64::from(tmp.tm.tm_mon) + 1,
            day: tmp.tm.tm_mday.into(),
            hour: tmp.tm.tm_hour.into(),
            minutes: tmp.tm.tm_min.into(),
            seconds: Timespec {
                sec: tmp.tm.tm_sec.into(),
                ns: now.ns.into(),
            },
            rel: RelTime::default(),
            timespec_seen: false,
            rels_seen: false,
            dates_seen: 0,
            days_seen: 0,
            j_zones_seen: 0,
            local_zones_seen: 0,
            dsts_seen: 0,
            times_seen: 0,
            zones_seen: 0,
            year_seen: false,
            debug,
            debug_dates_seen: false,
            debug_days_seen: false,
            debug_local_zones_seen: false,
            debug_times_seen: false,
            debug_zones_seen: false,
            debug_year_seen: false,
            debug_ordinal_day_seen: false,
            now_isdst: tmp.tm.tm_isdst,
            local_time_zone_table,
        },
    };
    if parser.parse().is_none() {
        if debug {
            if parser.pos >= parser.input.len() {
                dbg("error: parsing failed\n");
            } else {
                let mut m = b"error: parsing failed, stopped at '".to_vec();
                m.extend_from_slice(&parser.input[parser.pos..]);
                m.extend_from_slice(b"'\n");
                dbg(m);
            }
        }
        return None;
    }
    let mut pc = parser.pc;

    if debug {
        let mut m = String::from("input timezone: ");
        if pc.timespec_seen {
            m.push_str("'@timespec' - always UTC");
        } else if pc.zones_seen > 0 {
            m.push_str("parsed date/time string");
        } else if let Some(name) = &tzstring {
            let name = String::from_utf8_lossy(name);
            if tz_from_string {
                m.push_str(&format!("TZ=\"{name}\" in date string"));
            } else if name == "UTC0" {
                m.push_str("TZ=\"UTC0\" environment value or -u");
            } else {
                m.push_str(&format!("TZ=\"{name}\" environment value"));
            }
        } else {
            m.push_str("system default");
        }
        if pc.local_zones_seen > 0 && pc.zones_seen == 0 && 0 < pc.local_isdst {
            m.push_str(", dst");
        }
        if pc.zones_seen > 0 {
            m.push_str(&format!(" ({})", time_zone_str(pc.time_zone.into())));
        }
        m.push('\n');
        dbg(m);
    }

    let result = if pc.timespec_seen {
        Timespec {
            sec: pc.seconds.sec,
            ns: pc.seconds.ns,
        }
    } else {
        resolve(&mut pc, &tz)?
    };

    if debug {
        match &tzstring {
            None => dbg("timezone: system default\n"),
            Some(name) if name == b"UTC0" => dbg("timezone: Universal Time\n"),
            Some(name) => {
                let mut m = b"timezone: TZ=\"".to_vec();
                m.extend_from_slice(name);
                m.extend_from_slice(b"\" environment value\n");
                dbg(m);
            }
        }
        dbg(format!(
            "final: {}.{:09} (epoch-seconds)\n",
            result.sec, result.ns
        ));
        if let Some(gmt) = gmtime(result.sec) {
            dbg(format!(
                "final: {} (UTC)\n",
                pc.debug_strfdatetime(&gmt, false)
            ));
        }
        if let Some(lmt) = tz.localtime(result.sec) {
            dbg(format!(
                "final: {} (UTC{})\n",
                pc.debug_strfdatetime(&lmt.tm, false),
                time_zone_str(gmtoff(&lmt.tm))
            ));
        }
    }
    Some(Stamp {
        sec: result.sec,
        ns: result.ns as u32,
    })
}

/// The second half of `parse_datetime_body`: from the parsed items to a time.
fn resolve(pc: &mut Control, tz: &TimeZone) -> Option<Timespec> {
    let debug = pc.debug;
    if 1 < (pc.times_seen
        | pc.dates_seen
        | pc.days_seen
        | pc.dsts_seen
        | (pc.j_zones_seen + pc.local_zones_seen + pc.zones_seen))
    {
        if debug {
            if pc.times_seen > 1 {
                dbg("error: seen multiple time parts\n");
            }
            if pc.dates_seen > 1 {
                dbg("error: seen multiple date parts\n");
            }
            if pc.days_seen > 1 {
                dbg("error: seen multiple days parts\n");
            }
            if pc.dsts_seen > 1 {
                dbg("error: seen multiple daylight-saving parts\n");
            }
            if (pc.j_zones_seen + pc.local_zones_seen + pc.zones_seen) > 1 {
                dbg("error: seen multiple time-zone parts\n");
            }
        }
        return None;
    }

    let mut tm = zero_tm();
    let fields = to_tm_year(pc.year, debug).and_then(|y| {
        Some((
            y,
            i32::try_from(pc.month.checked_sub(1)?).ok()?,
            i32::try_from(pc.day).ok()?,
        ))
    });
    let Some((year, mon, mday)) = fields else {
        if debug {
            dbg("error: year, month, or day overflow\n");
        }
        return None;
    };
    (tm.tm_year, tm.tm_mon, tm.tm_mday) = (year, mon, mday);
    if pc.times_seen > 0 || (pc.rels_seen && pc.dates_seen == 0 && pc.days_seen == 0) {
        let h = to_hour(pc.hour, pc.meridian);
        if h < 0 {
            if debug {
                let mrd = match pc.meridian {
                    Meridian::Am => "am",
                    Meridian::Pm => "pm",
                    Meridian::H24 => "",
                };
                dbg(format!("error: invalid hour {}{mrd}\n", pc.hour));
            }
            return None;
        }
        tm.tm_hour = h as i32;
        // Narrowing like C's assignment to int.
        tm.tm_min = pc.minutes as i32;
        tm.tm_sec = pc.seconds.sec as i32;
        if debug {
            let which = if pc.times_seen > 0 {
                "specified"
            } else {
                "current"
            };
            dbg(format!(
                "using {which} time as starting value: '{}'\n",
                debug_strftime(&tm)
            ));
        }
    } else {
        tm.tm_hour = 0;
        tm.tm_min = 0;
        tm.tm_sec = 0;
        pc.seconds.ns = 0;
        if debug {
            dbg("warning: using midnight as starting time: 00:00:00\n");
        }
    }

    // Let mktime deduce tm_isdst for an absolute time, unless the input
    // explicitly says local standard or daylight time.
    tm.tm_isdst = if pc.local_zones_seen > 0 {
        pc.local_isdst
    } else if pc.dates_seen + pc.days_seen + pc.times_seen > 0 {
        -1
    } else {
        pc.now_isdst
    };

    let mut tm0 = zero_tm();
    tm0.tm_sec = tm.tm_sec;
    tm0.tm_min = tm.tm_min;
    tm0.tm_hour = tm.tm_hour;
    tm0.tm_mday = tm.tm_mday;
    tm0.tm_mon = tm.tm_mon;
    tm0.tm_year = tm.tm_year;
    tm0.tm_isdst = tm.tm_isdst;
    tm.tm_wday = -1;

    let mut start = tz.mktime(&mut tm);

    if !mktime_ok(&tm0, &tm) {
        let mut repaired = false;
        let time_zone_seen = pc.zones_seen != 0;
        if time_zone_seen {
            // Near the time_t limits mktime may fail in our zone but not in
            // the parsed one: retry in a zone at the parsed offset.
            let tz2 = TimeZone(Some(
                format!("XXX{}", time_zone_str(pc.time_zone.into())).into_bytes(),
            ));
            tm.tm_sec = tm0.tm_sec;
            tm.tm_min = tm0.tm_min;
            tm.tm_hour = tm0.tm_hour;
            tm.tm_mday = tm0.tm_mday;
            tm.tm_mon = tm0.tm_mon;
            tm.tm_year = tm0.tm_year;
            tm.tm_isdst = tm0.tm_isdst;
            tm.tm_wday = -1;
            start = tz2.mktime(&mut tm);
            repaired = mktime_ok(&tm0, &tm);
        }
        if !repaired {
            debug_mktime_not_ok(&tm0, &tm, pc, time_zone_seen);
            return None;
        }
    }

    if pc.days_seen > 0 && pc.dates_seen == 0 {
        tm.tm_yday = -1;
        let day_ordinal =
            pc.day_ordinal - i64::from(0 < pc.day_ordinal && tm.tm_wday != pc.day_number);
        let dayincr = day_ordinal
            .checked_mul(7)
            .and_then(|d| d.checked_add(i64::from((pc.day_number - tm.tm_wday + 7) % 7)));
        if let Some(dayincr) = dayincr {
            // Like C's ckd_add, store the wrapped sum even when it overflows
            // (the debug trace shows it).
            let sum = dayincr.wrapping_add(tm.tm_mday.into());
            tm.tm_mday = sum as i32;
            if i32::try_from(sum).is_ok() {
                tm.tm_isdst = -1;
                start = tz.mktime(&mut tm);
            }
        }
        if tm.tm_yday < 0 {
            if debug {
                dbg(format!(
                    "error: day '{}' (day ordinal={} number={}) resulted in an invalid date: '{}'\n",
                    pc.str_days(),
                    pc.day_ordinal,
                    pc.day_number,
                    pc.debug_strfdatetime(&tm, true)
                ));
            }
            return None;
        }
        if debug {
            dbg(format!(
                "new start date: '{}' is '{}'\n",
                pc.str_days(),
                pc.debug_strfdatetime(&tm, true)
            ));
        }
    }

    if debug {
        if pc.dates_seen == 0 && pc.days_seen == 0 {
            dbg(format!(
                "using current date as starting value: '{}'\n",
                debug_strfdate(&tm)
            ));
        }
        if pc.days_seen > 0 && pc.dates_seen > 0 {
            dbg(format!(
                "warning: day ({}) ignored when explicit dates are given\n",
                pc.str_days()
            ));
        }
        dbg(format!(
            "starting date/time: '{}'\n",
            pc.debug_strfdatetime(&tm, true)
        ));
    }

    // Add the relative date.
    if pc.rel.year != 0 || pc.rel.month != 0 || pc.rel.day != 0 {
        if debug {
            if (pc.rel.year != 0 || pc.rel.month != 0) && tm.tm_mday != 15 {
                dbg(
                    "warning: when adding relative months/years, it is recommended to specify the 15th of the months\n",
                );
            }
            if pc.rel.day != 0 && tm.tm_hour != 12 {
                dbg("warning: when adding relative days, it is recommended to specify noon\n");
            }
        }
        let add = |a: i32, b: i64| i32::try_from(i64::from(a).checked_add(b)?).ok();
        let (Some(year), Some(month), Some(day)) = (
            add(tm.tm_year, pc.rel.year),
            add(tm.tm_mon, pc.rel.month),
            add(tm.tm_mday, pc.rel.day),
        ) else {
            if debug {
                // GNU reports its __FILE__ and __LINE__ here.
                dbg("error: parse-datetime.y:2144\n");
            }
            return None;
        };
        tm.tm_year = year;
        tm.tm_mon = month;
        tm.tm_mday = day;
        tm.tm_hour = tm0.tm_hour;
        tm.tm_min = tm0.tm_min;
        tm.tm_sec = tm0.tm_sec;
        tm.tm_isdst = tm0.tm_isdst;
        tm.tm_wday = -1;
        start = tz.mktime(&mut tm);
        if tm.tm_wday < 0 {
            if debug {
                dbg(format!(
                    "error: adding relative date resulted in an invalid date: '{}'\n",
                    pc.debug_strfdatetime(&tm, true)
                ));
            }
            return None;
        }
        if debug {
            dbg(format!(
                "after date adjustment ({:+} years, {:+} months, {:+} days),\n",
                pc.rel.year, pc.rel.month, pc.rel.day
            ));
            dbg(format!(
                "    new date/time = '{}'\n",
                pc.debug_strfdatetime(&tm, true)
            ));
            if tm0.tm_isdst != -1 && tm.tm_isdst != tm0.tm_isdst {
                dbg("warning: daylight saving time changed after date adjustment\n");
            }
            if pc.rel.day == 0 && (tm.tm_mday != day || (pc.rel.month == 0 && tm.tm_mon != month)) {
                dbg("warning: month/year adjustment resulted in shifted dates:\n");
                dbg(format!(
                    "     adjusted Y M D: {} {:02} {:02}\n",
                    tm_year_str(year),
                    month + 1,
                    day
                ));
                dbg(format!(
                    "   normalized Y M D: {} {:02} {:02}\n",
                    tm_year_str(tm.tm_year),
                    tm.tm_mon + 1,
                    tm.tm_mday
                ));
            }
        }
    }

    // The parsed zone: only Start changes, so this follows everything that
    // recomputes Start.
    if pc.zones_seen > 0 {
        let shifted = i64::from(pc.time_zone)
            .checked_sub(gmtoff(&tm))
            .and_then(|delta| start.checked_sub(delta));
        let Some(t1) = shifted else {
            if debug {
                dbg(format!(
                    "error: timezone {} caused time_t overflow\n",
                    pc.time_zone
                ));
            }
            return None;
        };
        start = t1;
    }

    if debug {
        dbg(format!(
            "'{}' = {start} epoch-seconds\n",
            pc.debug_strfdatetime(&tm, true)
        ));
    }

    // Add relative hours, minutes and seconds, ignoring leap seconds.
    let orig_ns = pc.seconds.ns;
    let sum_ns = orig_ns + i64::from(pc.rel.ns);
    let normalized_ns = sum_ns.rem_euclid(BILLION);
    let d4 = (sum_ns - normalized_ns) / BILLION;
    let t4 = pc
        .rel
        .hour
        .checked_mul(60 * 60)
        .and_then(|d1| start.checked_add(d1))
        .and_then(|t1| t1.checked_add(pc.rel.minutes.checked_mul(60)?))
        .and_then(|t2| t2.checked_add(pc.rel.seconds))
        .and_then(|t3| t3.checked_add(d4))
        .filter(|&t| libc::time_t::try_from(t).is_ok());
    let Some(t4) = t4 else {
        if debug {
            dbg("error: adding relative time caused an overflow\n");
        }
        return None;
    };

    if debug && (pc.rel.hour | pc.rel.minutes | pc.rel.seconds | i64::from(pc.rel.ns)) != 0 {
        dbg(format!(
            "after time adjustment ({:+} hours, {:+} minutes, {:+} seconds, {:+} ns),\n",
            pc.rel.hour, pc.rel.minutes, pc.rel.seconds, pc.rel.ns
        ));
        dbg(format!("    new time = {t4} epoch-seconds\n"));
        if tm.tm_isdst != -1
            && let Some(lmt) = tz.localtime(t4)
            && tm.tm_isdst != lmt.tm.tm_isdst
        {
            dbg("warning: daylight saving time changed after time adjustment\n");
        }
    }
    Some(Timespec {
        sec: t4,
        ns: normalized_ns,
    })
}
