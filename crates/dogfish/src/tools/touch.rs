//! touch — `-d` understands the common GNU date formats (see `parse_datetime`)

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate, TimeZone, Timelike};
use dogfish_core::{DogfishContext, gnu_io_error};

use crate::common::file_times::{Stamp, set_file_times};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::quote::{qa, quote_locale};

const TOUCH_HELP: &str = "\
Usage: touch [OPTION]... FILE...
Update the access and modification times of each FILE to the current time.

A FILE argument that does not exist is created empty, unless -c or -h
is supplied.

  -a                     change only the access time
  -c, --no-create        do not create any files
  -d, --date=STRING      parse STRING and use it instead of current time
  -f                     (ignored)
  -h, --no-dereference   affect each symbolic link instead of any referenced
                         file
  -m                     change only the modification time
  -r, --reference=FILE   use this file's times instead of current time
  -t STAMP               use [[CC]YY]MMDDhhmm[.ss] instead of current time
      --time=WORD        change the specified time:
                           WORD is access, atime, or use: equivalent to -a
                           WORD is modify or mtime: equivalent to -m
      --help             display this help and exit
      --version          output version information and exit

STRING may be '@SECONDS', an ISO date such as '2020-01-31 13:45:00.5 +0100',
'Jan 31 2020', '1/31/2020', and relative items like 'yesterday',
'2 days ago' or '+3 hours'.
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum TouchOpt {
    Access,
    NoCreate,
    Date,
    Ignored,
    NoDeref,
    Modify,
    Reference,
    StampArg,
    Time,
    Help,
    Version,
}

/// GNU's `longopts` and `"acd:fhmr:t:"`.
const SPECS: &[OptSpec<TouchOpt>] = &[
    OptSpec::long("time", HasArg::Required, TouchOpt::Time),
    OptSpec::both(b'c', "no-create", HasArg::No, TouchOpt::NoCreate),
    OptSpec::both(b'd', "date", HasArg::Required, TouchOpt::Date),
    OptSpec::both(b'r', "reference", HasArg::Required, TouchOpt::Reference),
    OptSpec::both(b'h', "no-dereference", HasArg::No, TouchOpt::NoDeref),
    OptSpec::long("help", HasArg::No, TouchOpt::Help),
    OptSpec::long("version", HasArg::No, TouchOpt::Version),
    OptSpec::short(b'a', HasArg::No, TouchOpt::Access),
    OptSpec::short(b'f', HasArg::No, TouchOpt::Ignored),
    OptSpec::short(b'm', HasArg::No, TouchOpt::Modify),
    OptSpec::short(b't', HasArg::Required, TouchOpt::StampArg),
];

pub fn run_touch(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use TouchOpt::*;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let (mut atime, mut mtime, mut no_create, mut no_deref) = (false, false, false, false);
    let (mut date, mut reference, mut stamp) = (None, None, None);
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("touch", &e)),
        };
        match o {
            Help => return Ok(print_help(TOUCH_HELP)),
            Version => return Ok(print_version("touch")),
            Access => atime = true,
            Modify => mtime = true,
            NoCreate => no_create = true,
            NoDeref => no_deref = true,
            Date => date = v,
            Reference => reference = v,
            StampArg => stamp = v,
            Time => {
                const WORDS: &[(&str, bool)] = &[
                    ("atime", true),
                    ("access", true),
                    ("use", true),
                    ("mtime", false),
                    ("modify", false),
                ];
                match argmatch("touch", "--time", v.unwrap_or_default(), WORDS) {
                    Ok(true) => atime = true,
                    Ok(false) => mtime = true,
                    Err(code) => return Ok(code),
                }
            }
            Ignored => {}
        }
    }
    if stamp.is_some() && (date.is_some() || reference.is_some()) {
        return Ok(usage_error(
            "touch",
            "cannot specify times from more than one source",
        ));
    }
    if !atime && !mtime {
        atime = true;
        mtime = true;
    }
    let operands = opts.operands();
    if operands.is_empty() {
        return Ok(usage_error("touch", "missing file operand"));
    }

    let now = now_stamp();
    let (mut a_stamp, mut m_stamp) = (Stamp::Now, Stamp::Now);
    if let Some(r) = &reference {
        let meta = if no_deref {
            fs::symlink_metadata(r)
        } else {
            fs::metadata(r)
        };
        let meta = match meta {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "touch: failed to get attributes of {}: {}",
                    qa(r),
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
        };
        let ns = |n: i64| u32::try_from(n).unwrap_or(0);
        a_stamp = Stamp::At(meta.atime(), ns(meta.atime_nsec()));
        m_stamp = Stamp::At(meta.mtime(), ns(meta.mtime_nsec()));
    }
    if let Some(d) = &date {
        // With -r, the date is relative to the reference file's times.
        let base = |s: Stamp| match s {
            Stamp::At(sec, ns) => (sec, ns),
            _ => now,
        };
        match (
            parse_datetime(&lossy(d), base(a_stamp)),
            parse_datetime(&lossy(d), base(m_stamp)),
        ) {
            (Some((asec, ans)), Some((msec, mns))) => {
                a_stamp = Stamp::At(asec, ans);
                m_stamp = Stamp::At(msec, mns);
            }
            _ => {
                eprintln!("touch: invalid date format {}", quote_locale(d));
                return Ok(1);
            }
        }
    }
    if let Some(t) = &stamp {
        match parse_touch_stamp(&lossy(t)) {
            Some((sec, ns)) => {
                a_stamp = Stamp::At(sec, ns);
                m_stamp = Stamp::At(sec, ns);
            }
            None => {
                eprintln!("touch: invalid date format {}", quote_locale(t));
                return Ok(1);
            }
        }
    }
    if !atime {
        a_stamp = Stamp::Omit;
    }
    if !mtime {
        m_stamp = Stamp::Omit;
    }

    let mut status = 0;
    for file in &operands {
        let path = Path::new(file);
        let mut open_err = None;
        if !no_create && !no_deref {
            // O_CREAT without O_TRUNC: never truncates, even if the file appeared meanwhile.
            if let Err(e) = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
                .mode(0o666)
                .open(path)
            {
                open_err = Some(e);
            }
        }
        match set_file_times(path, a_stamp, m_stamp, !no_deref) {
            Ok(()) => ctx.telemetry.add_items(1),
            Err(e) => {
                if let Some(oe) = open_err {
                    eprintln!("touch: cannot touch {}: {}", qa(file), gnu_io_error(&oe));
                } else if no_create && e.raw_os_error() == Some(libc::ENOENT) {
                    continue;
                } else {
                    eprintln!("touch: setting times of {}: {}", qa(file), gnu_io_error(&e));
                }
                status = 1;
            }
        }
    }
    Ok(status)
}

fn now_stamp() -> (i64, u32) {
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    (
        i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        d.subsec_nanos(),
    )
}

/// Seconds since the epoch of a local wall-clock time; in a DST gap the time is
/// read with the offset in force before the gap (as `mktime` does).
fn local_to_epoch(date: NaiveDate, secs_of_day: i64) -> Option<i64> {
    let midnight = date.and_hms_opt(0, 0, 0)?;
    let naive = midnight + chrono::Duration::seconds(secs_of_day);
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(t) => Some(t.timestamp()),
        chrono::LocalResult::Ambiguous(t, _) => Some(t.timestamp()),
        chrono::LocalResult::None => {
            let before = Local
                .from_local_datetime(&(naive - chrono::Duration::hours(3)))
                .earliest()?;
            Some(naive.and_utc().timestamp() - i64::from(before.offset().local_minus_utc()))
        }
    }
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let y = i32::try_from(year).unwrap_or(1970);
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(y, month + 1, 1)
    };
    next.and_then(|n| n.pred_opt()).map_or(31, |d| d.day())
}

/// `touch -t [[CC]YY]MMDDhhmm[.ss]`, in local time.
fn parse_touch_stamp(s: &str) -> Option<(i64, u32)> {
    let (main, secs) = match s.split_once('.') {
        Some((m, ss)) if ss.len() == 2 && ss.bytes().all(|b| b.is_ascii_digit()) => {
            (m, ss.parse::<i64>().ok()?)
        }
        Some(_) => return None,
        None => (s, 0),
    };
    if !main.bytes().all(|b| b.is_ascii_digit()) || ![8, 10, 12].contains(&main.len()) {
        return None;
    }
    let num = |r: std::ops::Range<usize>| main[r].parse::<u32>().ok();
    let n = main.len();
    let (month, day, hour, minute) = (
        num(n - 8..n - 6)?,
        num(n - 6..n - 4)?,
        num(n - 4..n - 2)?,
        num(n - 2..n)?,
    );
    let year = match n {
        12 => i64::from(num(0..4)?),
        10 => {
            let yy = i64::from(num(0..2)?);
            if yy < 69 { 2000 + yy } else { 1900 + yy }
        }
        _ => i64::from(Local::now().year()),
    };
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || secs > 60
    {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)?;
    let epoch = local_to_epoch(date, i64::from(hour * 3600 + minute * 60))?;
    Some((epoch + secs, 0))
}

/// Time-zone abbreviations GNU `parse_datetime` knows, in seconds east of UTC.
const TZ_NAMES: &[(&str, i64)] = &[
    ("gmt", 0),
    ("ut", 0),
    ("utc", 0),
    ("z", 0),
    ("wet", 0),
    ("west", 3600),
    ("bst", 3600),
    ("art", -3 * 3600),
    ("brt", -3 * 3600),
    ("brst", -2 * 3600),
    ("nst", -(3 * 3600 + 1800)),
    ("ndt", -(2 * 3600 + 1800)),
    ("ast", -4 * 3600),
    ("adt", -3 * 3600),
    ("est", -5 * 3600),
    ("edt", -4 * 3600),
    ("cst", -6 * 3600),
    ("cdt", -5 * 3600),
    ("mst", -7 * 3600),
    ("mdt", -6 * 3600),
    ("pst", -8 * 3600),
    ("pdt", -7 * 3600),
    ("akst", -9 * 3600),
    ("akdt", -8 * 3600),
    ("hst", -10 * 3600),
    ("hast", -10 * 3600),
    ("hadt", -9 * 3600),
    ("sst", -11 * 3600),
    ("wat", 3600),
    ("cet", 3600),
    ("cest", 2 * 3600),
    ("met", 3600),
    ("mez", 3600),
    ("mest", 2 * 3600),
    ("mesz", 2 * 3600),
    ("eet", 2 * 3600),
    ("eest", 3 * 3600),
    ("cat", 2 * 3600),
    ("sast", 2 * 3600),
    ("eat", 3 * 3600),
    ("msk", 3 * 3600),
    ("msd", 4 * 3600),
    ("ist", 5 * 3600 + 1800),
    ("sgt", 8 * 3600),
    ("kst", 9 * 3600),
    ("jst", 9 * 3600),
    ("gst", 10 * 3600),
    ("nzst", 12 * 3600),
    ("nzdt", 13 * 3600),
];

const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn month_number(word: &str) -> Option<u32> {
    let word = word.strip_suffix('.').unwrap_or(word);
    if word == "sept" {
        return Some(9);
    }
    let pos = MONTH_NAMES
        .iter()
        .position(|m| word.len() >= 3 && m.starts_with(&word[..3]))?;
    let full = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ][pos];
    (word.len() == 3 || full == word).then_some(pos as u32 + 1)
}

fn day_number(word: &str) -> Option<u32> {
    let word = word.strip_suffix('.').unwrap_or(word);
    let full = [
        "sunday",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
    ];
    let pos = DAY_NAMES
        .iter()
        .position(|d| word.len() >= 3 && d.starts_with(&word[..3]))?;
    (word.len() == 3 || full[pos] == word || (word.len() == 4 && full[pos].starts_with(word)))
        .then_some(pos as u32)
}

/// Relative units: (months, seconds) per unit.
fn rel_unit(word: &str) -> Option<(i64, i64)> {
    let word = word
        .strip_suffix('s')
        .filter(|w| !w.is_empty())
        .unwrap_or(word);
    Some(match word {
        "year" => (12, 0),
        "month" => (1, 0),
        "fortnight" => (0, 14 * 86400),
        "week" => (0, 7 * 86400),
        "day" => (0, 86400),
        "hour" => (0, 3600),
        "minute" | "min" => (0, 60),
        "second" | "sec" => (0, 1),
        _ => return None,
    })
}

/// The items of a date string, as GNU `parse_datetime` collects them.
#[derive(Default)]
struct DateItems {
    year: Option<(i64, usize)>,
    month_day: Option<(u32, u32)>,
    time: Option<(u32, u32, u32, u32)>,
    meridian: Option<bool>,
    zone: Option<i64>,
    /// Day of the week (0 = Sunday) and its ordinal (`next` = 1, `last` = -1).
    weekday: Option<(u32, i64)>,
    rel_months: i64,
    rel_days: i64,
    rel_secs: i64,
    rels_seen: bool,
    last_rel: Option<(i64, i64, i64)>,
}

impl DateItems {
    fn add_rel(&mut self, n: i64, (months, secs): (i64, i64)) {
        let (m, d, s) = if secs % 86400 == 0 && secs != 0 {
            (0, n * (secs / 86400), 0)
        } else {
            (n * months, 0, n * secs)
        };
        self.rel_months += m;
        self.rel_days += d;
        self.rel_secs += s;
        self.rels_seen = true;
        self.last_rel = Some((m, d, s));
    }

    fn set_date(&mut self, year: Option<(i64, usize)>, month: u32, day: u32) -> Option<()> {
        if self.month_day.is_some() {
            return None;
        }
        self.month_day = Some((month, day));
        if year.is_some() {
            self.year = year;
        }
        Some(())
    }

    fn set_time(&mut self, t: (u32, u32, u32, u32)) -> Option<()> {
        if self.time.is_some() {
            return None;
        }
        self.time = Some(t);
        Some(())
    }
}

/// A cursor over the lowercased input.
struct DateLexer<'a> {
    s: &'a [u8],
    i: usize,
}

impl DateLexer<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn digits(&mut self) -> Option<(i64, usize)> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let len = self.i - start;
        if len == 0 || len > 18 {
            return None;
        }
        Some((
            std::str::from_utf8(&self.s[start..self.i])
                .ok()?
                .parse()
                .ok()?,
            len,
        ))
    }

    fn word(&mut self) -> &str {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'.')
        {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).unwrap_or("")
    }

    fn peek_word(&self) -> &str {
        let mut j = self.i;
        while self.s.get(j).is_some_and(|c| c.is_ascii_whitespace()) {
            j += 1;
        }
        let start = j;
        while self.s.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
            j += 1;
        }
        std::str::from_utf8(&self.s[start..j]).unwrap_or("")
    }

    /// `HH:MM[:SS[.frac]]` after the hour has been read.
    fn clock(&mut self, hour: i64) -> Option<(u32, u32, u32, u32)> {
        self.i += 1; // ':'
        let (minute, mlen) = self.digits()?;
        if mlen != 2 {
            return None;
        }
        let (mut second, mut nanos) = (0, 0);
        if self.peek() == Some(b':') {
            self.i += 1;
            let (s, slen) = self.digits()?;
            if slen != 2 {
                return None;
            }
            second = s;
            if matches!(self.peek(), Some(b'.' | b','))
                && self.s.get(self.i + 1).is_some_and(u8::is_ascii_digit)
            {
                self.i += 1;
                let start = self.i;
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
                let frac = &self.s[start..self.i];
                for k in 0..9 {
                    nanos = nanos * 10 + frac.get(k).map_or(0, |d| u32::from(d - b'0'));
                }
            }
        }
        Some((
            u32::try_from(hour).ok()?,
            u32::try_from(minute).ok()?,
            u32::try_from(second).ok()?,
            nanos,
        ))
    }

    /// A numeric zone `+HH`, `+HHMM` or `+HH:MM`, if one starts here.
    fn numeric_zone(&mut self) -> Option<i64> {
        let save = self.i;
        let sign = match self.peek() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return None,
        };
        self.i += 1;
        let Some((n, len)) = self.digits() else {
            self.i = save;
            return None;
        };
        let secs = match len {
            1 | 2 if self.peek() == Some(b':') => {
                self.i += 1;
                let (m, mlen) = self.digits()?;
                if mlen != 2 {
                    return None;
                }
                n * 3600 + m * 60
            }
            1 | 2 => n * 3600,
            4 => (n / 100) * 3600 + (n % 100) * 60,
            _ => {
                self.i = save;
                return None;
            }
        };
        Some(sign * secs)
    }

    /// After a time of day: an optional am/pm and an optional zone.
    fn time_suffix(&mut self, items: &mut DateItems) -> Option<()> {
        let save = self.i;
        self.skip_space();
        let w = self.peek_word().to_string();
        let meridian = match w.as_str() {
            "am" | "a" => Some(false),
            "pm" | "p" => Some(true),
            _ => None,
        };
        if let Some(pm) = meridian {
            self.word();
            if self.peek() == Some(b'.') {
                self.i += 1;
            }
            items.meridian = Some(pm);
        } else {
            self.i = save;
        }
        if self.peek() == Some(b'z') && !self.s.get(self.i + 1).is_some_and(u8::is_ascii_alphabetic)
        {
            self.i += 1;
            items.zone = Some(0);
        } else if let Some(z) = self.numeric_zone() {
            items.zone = Some(z);
        }
        Some(())
    }
}

/// Parses the date formats GNU `touch -d` / `date -d` accept most often:
/// `@SECONDS[.frac]`, ISO 8601 dates and times (`2020-01-31`, `2020-01-31T13:45:00Z`,
/// `2020-01-31 13:45:00.5 +0100`), `YYYYMMDD`, `1/31/2020`, `Jan 31 2020`,
/// `31 Jan 2020`, times with `am`/`pm`, zone names (`UTC`, `EST`, `CET`, …),
/// weekdays, and relative items (`yesterday`, `2 days ago`, `+3 hours`,
/// `next week`). `now` is `(seconds, nanoseconds)`; unspecified fields default
/// like GNU: a date alone means midnight, relative items alone apply to `now`.
fn parse_datetime(input: &str, now: (i64, u32)) -> Option<(i64, u32)> {
    let lower = input.trim().to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix('@') {
        return parse_epoch(rest.trim());
    }
    let mut lx = DateLexer {
        s: lower.as_bytes(),
        i: 0,
    };
    let mut it = DateItems::default();
    loop {
        lx.skip_space();
        let Some(c) = lx.peek() else { break };
        if c.is_ascii_digit() {
            let (n, len) = lx.digits()?;
            match lx.peek() {
                Some(b'-')
                    if lx.s.get(lx.i + 1).is_some_and(u8::is_ascii_digit)
                        && it.month_day.is_none() =>
                {
                    // ISO 8601: YYYY-MM-DD[THH:MM[:SS]]
                    lx.i += 1;
                    let (month, _) = lx.digits()?;
                    if lx.peek() != Some(b'-') {
                        return None;
                    }
                    lx.i += 1;
                    let (day, _) = lx.digits()?;
                    it.set_date(
                        Some((n, len)),
                        u32::try_from(month).ok()?,
                        u32::try_from(day).ok()?,
                    )?;
                    if lx.peek() == Some(b't') && lx.s.get(lx.i + 1).is_some_and(u8::is_ascii_digit)
                    {
                        lx.i += 1;
                        let (h, _) = lx.digits()?;
                        if lx.peek() != Some(b':') {
                            return None;
                        }
                        let t = lx.clock(h)?;
                        it.set_time(t)?;
                        lx.time_suffix(&mut it)?;
                    }
                }
                Some(b':') => {
                    let t = lx.clock(n)?;
                    it.set_time(t)?;
                    lx.time_suffix(&mut it)?;
                }
                Some(b'/') => {
                    // US style: MM/DD[/YYYY]
                    lx.i += 1;
                    let (day, _) = lx.digits()?;
                    let mut year = None;
                    if lx.peek() == Some(b'/') {
                        lx.i += 1;
                        year = Some(lx.digits()?);
                    }
                    it.set_date(year, u32::try_from(n).ok()?, u32::try_from(day).ok()?)?;
                }
                _ => {
                    let next = lx.peek_word().to_string();
                    if let Some(unit) = rel_unit(&next) {
                        lx.skip_space();
                        lx.word();
                        it.add_rel(n, unit);
                    } else if let Some(month) = month_number(&next) {
                        // DD Month [YYYY]
                        lx.skip_space();
                        lx.word();
                        lx.skip_space();
                        let save = lx.i;
                        let year = match lx.digits() {
                            Some(y) if lx.peek() != Some(b':') => Some(y),
                            _ => {
                                lx.i = save;
                                None
                            }
                        };
                        it.set_date(year, month, u32::try_from(n).ok()?)?;
                    } else if matches!(next.as_str(), "am" | "pm" | "a" | "p") && len <= 2 {
                        it.set_time((u32::try_from(n).ok()?, 0, 0, 0))?;
                        lx.time_suffix(&mut it)?;
                    } else if it.month_day.is_some()
                        && it.year.is_none()
                        && !it.rels_seen
                        && (it.time.is_some() || len > 2)
                    {
                        it.year = Some((n, len));
                    } else if len > 4 {
                        it.set_date(
                            Some((n / 10000, len - 4)),
                            u32::try_from((n / 100) % 100).ok()?,
                            u32::try_from(n % 100).ok()?,
                        )?;
                    } else {
                        let (h, m) = if len <= 2 { (n, 0) } else { (n / 100, n % 100) };
                        it.set_time((u32::try_from(h).ok()?, u32::try_from(m).ok()?, 0, 0))?;
                    }
                }
            }
        } else if c == b'+' || c == b'-' {
            let save = lx.i;
            if it.time.is_some()
                && it.zone.is_none()
                && let Some(z) = lx.numeric_zone()
                && rel_unit(lx.peek_word()).is_none()
            {
                it.zone = Some(z);
                continue;
            }
            lx.i = save + 1;
            let (n, _) = lx.digits()?;
            let n = if c == b'-' { -n } else { n };
            let unit = rel_unit(lx.peek_word())?;
            lx.skip_space();
            lx.word();
            it.add_rel(n, unit);
        } else if c.is_ascii_alphabetic() {
            let word = lx.word().to_string();
            let word = word.trim_end_matches('.');
            if let Some(month) = month_number(word) {
                // Month DD [[,] YYYY]
                lx.skip_space();
                let (day, _) = lx.digits()?;
                if lx.peek() == Some(b',') {
                    lx.i += 1;
                }
                lx.skip_space();
                let save = lx.i;
                let year = match lx.digits() {
                    Some(y) if lx.peek() != Some(b':') => Some(y),
                    _ => {
                        lx.i = save;
                        None
                    }
                };
                it.set_date(year, month, u32::try_from(day).ok()?)?;
            } else if let Some(day) = day_number(word) {
                if it.weekday.replace((day, 0)).is_some() {
                    return None;
                }
            } else if let Some(&(_, off)) = TZ_NAMES.iter().find(|(name, _)| *name == word) {
                if it.zone.is_some() {
                    return None;
                }
                let correction = lx.numeric_zone().unwrap_or(0);
                it.zone = Some(off + correction);
            } else {
                match word {
                    "now" | "today" => it.rels_seen = true,
                    "yesterday" => it.add_rel(-1, (0, 86400)),
                    "tomorrow" => it.add_rel(1, (0, 86400)),
                    "ago" => {
                        let (m, d, s) = it.last_rel.take()?;
                        it.rel_months -= 2 * m;
                        it.rel_days -= 2 * d;
                        it.rel_secs -= 2 * s;
                    }
                    "next" | "last" | "this" => {
                        let n = match word {
                            "next" => 1,
                            "last" => -1,
                            _ => 0,
                        };
                        let next = lx.peek_word().to_string();
                        if let Some(day) = day_number(&next) {
                            if it.weekday.replace((day, n)).is_some() {
                                return None;
                            }
                        } else {
                            it.add_rel(n, rel_unit(&next)?);
                        }
                        lx.skip_space();
                        lx.word();
                    }
                    w => it.add_rel(1, rel_unit(w)?),
                }
            }
        } else {
            return None;
        }
    }
    resolve_date(&it, now, lower.is_empty())
}

/// `@SECONDS[.frac]`.
fn parse_epoch(s: &str) -> Option<(i64, u32)> {
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (int, frac) = match digits.find(['.', ',']) {
        Some(i) => (&digits[..i], &digits[i + 1..]),
        None => (digits, ""),
    };
    if int.is_empty()
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let secs: i64 = int.parse().ok()?;
    let mut nanos: u32 = 0;
    for k in 0..9 {
        nanos = nanos * 10 + frac.as_bytes().get(k).map_or(0, |d| u32::from(d - b'0'));
    }
    if !neg {
        return Some((secs, nanos));
    }
    if nanos == 0 {
        Some((-secs, 0))
    } else {
        Some((-secs - 1, 1_000_000_000 - nanos))
    }
}

fn resolve_date(it: &DateItems, now: (i64, u32), empty: bool) -> Option<(i64, u32)> {
    let now_local = Local.timestamp_opt(now.0, now.1).single()?;
    let has_date = it.month_day.is_some();
    let (mut year, mut month, mut day) = (
        i64::from(now_local.year()),
        now_local.month(),
        now_local.day(),
    );
    if let Some((y, digits)) = it.year {
        year = if digits == 2 {
            if y < 69 { y + 2000 } else { y + 1900 }
        } else {
            y
        };
    }
    if let Some((m, d)) = it.month_day {
        month = m;
        day = d;
    }
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let (mut hour, minute, second, nanos) = match it.time {
        Some(t) => t,
        None if has_date || empty || it.weekday.is_some() => (0, 0, 0, 0),
        None => (
            now_local.hour(),
            now_local.minute(),
            now_local.second(),
            now.1,
        ),
    };
    if let Some(pm) = it.meridian {
        if hour == 0 || hour > 12 {
            return None;
        }
        hour = hour % 12 + if pm { 12 } else { 0 };
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    // A weekday moves to that day (unless a date was given), as GNU does:
    // `monday` is today or the coming Monday, `next monday` the one after today.
    let mut day = i64::from(day);
    if let Some((wd, ordinal)) = it.weekday.filter(|_| !has_date) {
        let today = NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, 1)?
            .checked_add_signed(chrono::Duration::days(day - 1))?
            .weekday()
            .num_days_from_sunday();
        let (wd, today) = (i64::from(wd), i64::from(today));
        day += (wd - today + 7) % 7 + 7 * (ordinal - i64::from(ordinal > 0 && today != wd));
    }
    // Then relative months (with mktime-style day overflow), then days.
    let months = year * 12 + i64::from(month) - 1 + it.rel_months;
    let first = NaiveDate::from_ymd_opt(
        i32::try_from(months.div_euclid(12)).ok()?,
        u32::try_from(months.rem_euclid(12)).ok()? + 1,
        1,
    )?;
    let date = first.checked_add_signed(chrono::Duration::days(day - 1 + it.rel_days))?;
    let secs_of_day = i64::from(hour) * 3600 + i64::from(minute) * 60;
    let midnight_utc = date.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
    let base = match it.zone {
        Some(z) => midnight_utc + secs_of_day - z,
        // Only relative items: like GNU, keep the current UTC offset, so
        // `next week` is exactly 7 * 24 hours away even across a DST change.
        None if !has_date && it.time.is_none() && it.weekday.is_none() && !empty => {
            midnight_utc + secs_of_day - i64::from(now_local.offset().local_minus_utc())
        }
        None => local_to_epoch(date, secs_of_day)?,
    };
    Some((base + i64::from(second) + it.rel_secs, nanos))
}
