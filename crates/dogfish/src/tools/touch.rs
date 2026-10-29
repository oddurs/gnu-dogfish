//! touch — GNU date strings (`-d`), `-t STAMP` and the obsolete `MMDDhhmm[YY]` operand

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, gnu_io_error};

use crate::common::date_format::{Stamp, local_time};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, getopt_error, print_help, print_version, usage_error,
};
use crate::common::parse_datetime::{TimeZone, parse_datetime, zero_tm};
use crate::common::path::c_path;
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

/// Which times to change (`-a`, `-m`, `--time`).
const CH_ATIME: u8 = 1;
const CH_MTIME: u8 = 2;

pub fn run_touch(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use TouchOpt::*;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let mut change_times = 0;
    let (mut no_create, mut no_deref) = (false, false);
    let (mut flex_date, mut reference): (Option<OsString>, Option<OsString>) = (None, None);
    let mut newtime = [Stamp { sec: 0, ns: 0 }; 2];
    let mut date_set = false;
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("touch", &e)),
        };
        match o {
            Help => return Ok(print_help(TOUCH_HELP)),
            Version => return Ok(print_version("touch")),
            Access => change_times |= CH_ATIME,
            Modify => change_times |= CH_MTIME,
            NoCreate => no_create = true,
            NoDeref => no_deref = true,
            Date => flex_date = v,
            Reference => reference = v,
            StampArg => {
                let t = v.unwrap_or_default();
                let bits = PDS_LEADING_YEAR | PDS_CENTURY | PDS_SECONDS;
                let Some(sec) = posixtime(t.as_bytes(), bits) else {
                    eprintln!("touch: invalid date format {}", quote_locale(&t));
                    return Ok(1);
                };
                newtime = [Stamp { sec, ns: 0 }; 2];
                date_set = true;
            }
            Time => {
                const WORDS: &[(&str, u8)] = &[
                    ("atime", CH_ATIME),
                    ("access", CH_ATIME),
                    ("use", CH_ATIME),
                    ("mtime", CH_MTIME),
                    ("modify", CH_MTIME),
                ];
                match argmatch("touch", "--time", v.unwrap_or_default(), WORDS) {
                    Ok(bit) => change_times |= bit,
                    Err(code) => return Ok(code),
                }
            }
            Ignored => {}
        }
    }
    if change_times == 0 {
        change_times = CH_ATIME | CH_MTIME;
    }
    if date_set && (reference.is_some() || flex_date.is_some()) {
        return Ok(usage_error(
            "touch",
            "cannot specify times from more than one source",
        ));
    }

    let date_relative = |d: &std::ffi::OsStr, now: Stamp| {
        let parsed = parse_datetime(d.as_bytes(), now);
        if parsed.is_none() {
            eprintln!("touch: invalid date format {}", quote_locale(d));
        }
        parsed
    };
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
        newtime = [
            Stamp {
                sec: meta.atime(),
                ns: ns(meta.atime_nsec()),
            },
            Stamp {
                sec: meta.mtime(),
                ns: ns(meta.mtime_nsec()),
            },
        ];
        date_set = true;
        // With -r, the date is relative to the reference file's times.
        if let Some(d) = &flex_date {
            for (i, bit) in [CH_ATIME, CH_MTIME].into_iter().enumerate() {
                if change_times & bit != 0 {
                    let Some(t) = date_relative(d, newtime[i]) else {
                        return Ok(1);
                    };
                    newtime[i] = t;
                }
            }
        }
    } else if let Some(d) = &flex_date {
        let now = Stamp::now();
        let Some(t) = date_relative(d, now) else {
            return Ok(1);
        };
        newtime = [t; 2];
        date_set = true;
        // Without -a or -m, treat "-d now" as no -d at all, so that touch
        // succeeds more often under restrictive permissions; but check that
        // the string is relative, not a time that happens to be now.
        if change_times == CH_ATIME | CH_MTIME && same_time(t, now) {
            let notnow = Stamp {
                sec: now.sec ^ 1,
                ns: now.ns,
            };
            if parse_datetime(d.as_bytes(), notnow).is_some_and(|t| same_time(t, notnow)) {
                date_set = false;
            }
        }
    }

    // The obsolete `touch MMDDhhmm[YY] FILE...`: valid only with two or more
    // operands and an old _POSIX2_VERSION.
    let all_operands = opts.operands();
    let mut operands = all_operands.as_slice();
    if !date_set
        && operands.len() >= 2
        && posix2_version() < 200112
        && let Some(sec) = posixtime(operands[0].as_bytes(), PDS_TRAILING_YEAR | PDS_PRE_2000)
    {
        newtime = [Stamp { sec, ns: 0 }; 2];
        date_set = true;
        if std::env::var_os("POSIXLY_CORRECT").is_none()
            && let Some(tm) = local_time(sec).map(|lt| lt.tm)
        {
            eprintln!(
                "touch: warning: 'touch {}' is obsolete; use 'touch -t {:04}{:02}{:02}{:02}{:02}.{:02}'",
                operands[0].to_string_lossy(),
                i64::from(tm.tm_year) + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec
            );
        }
        operands = &operands[1..];
    }

    // `None` (both times now) is passed as a null pointer, which works with
    // mere write access to a file we do not own.
    let mut times = if date_set {
        Some(newtime.map(|s| libc::timespec {
            tv_sec: s.sec as libc::time_t,
            tv_nsec: s.ns.into(),
        }))
    } else if change_times == CH_ATIME | CH_MTIME {
        None
    } else {
        Some(
            [libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_NOW,
            }; 2],
        )
    };
    if let Some(t) = times.as_mut() {
        if change_times == CH_MTIME {
            t[0].tv_nsec = libc::UTIME_OMIT;
        } else if change_times == CH_ATIME {
            t[1].tv_nsec = libc::UTIME_OMIT;
        }
    }

    if operands.is_empty() {
        return Ok(usage_error("touch", "missing file operand"));
    }
    let mut status = 0;
    for file in operands {
        if touch_one(file, times.as_ref(), no_create, no_deref) {
            ctx.telemetry.add_items(1);
        } else {
            status = 1;
        }
    }
    Ok(status)
}

fn same_time(a: Stamp, b: Stamp) -> bool {
    (a.sec, a.ns) == (b.sec, b.ns)
}

/// GNU's `touch (file)`: creates FILE unless -c or -h, then sets its times;
/// `-` means standard output. Returns whether it succeeded.
fn touch_one(
    file: &OsStr,
    times: Option<&[libc::timespec; 2]>,
    no_create: bool,
    no_deref: bool,
) -> bool {
    let mut opened = None;
    let mut open_err = None;
    let fd: Option<RawFd> = if file.as_bytes() == b"-" {
        Some(libc::STDOUT_FILENO)
    } else if !(no_create || no_deref) {
        // O_CREAT without O_TRUNC: never truncates, even if the file appeared meanwhile.
        match fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .mode(0o666)
            .open(file)
        {
            Ok(f) => {
                let fd = f.as_raw_fd();
                opened = Some(f);
                Some(fd)
            }
            Err(e) => {
                open_err = Some(e);
                None
            }
        }
    } else {
        None
    };
    let ts = times.map_or(std::ptr::null(), |t| t.as_ptr());
    let result = match fd {
        // A standard output closed at startup now holds /dev/null: report
        // what GNU's futimens on the closed descriptor reports.
        Some(libc::STDOUT_FILENO) if dogfish_core::stdout::was_closed() => {
            Err(io::Error::from_raw_os_error(libc::EBADF))
        }
        Some(fd) => {
            // SAFETY: `ts` is null or points at two timespecs; `fd` is open.
            if unsafe { libc::futimens(fd, ts) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
        None => c_path(Path::new(file)).and_then(|c| {
            let flags = if no_deref {
                libc::AT_SYMLINK_NOFOLLOW
            } else {
                0
            };
            // SAFETY: `c` is a NUL-terminated path; `ts` as above.
            if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts, flags) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }),
    };
    drop(opened);
    let Err(utime_err) = result else {
        return true;
    };
    // Do not diagnose "touch -c - >&-".
    if fd == Some(libc::STDOUT_FILENO) && no_create && utime_err.raw_os_error() == Some(libc::EBADF)
    {
        return true;
    }
    // A directory cannot be opened for writing: report the times error then
    // (e.g. for 'touch /' without write access).
    let open_err = open_err.filter(|e| match e.raw_os_error() {
        Some(libc::EISDIR) => false,
        Some(libc::EINVAL) => !fs::metadata(file).is_ok_and(|m| m.is_dir()),
        _ => true,
    });
    if let Some(oe) = open_err {
        eprintln!("touch: cannot touch {}: {}", qa(file), gnu_io_error(&oe));
    } else {
        if no_create && utime_err.raw_os_error() == Some(libc::ENOENT) {
            return true;
        }
        eprintln!(
            "touch: setting times of {}: {}",
            qa(file),
            gnu_io_error(&utime_err)
        );
    }
    false
}

/// gnulib's `posix2_version`: `_POSIX2_VERSION` from the environment.
fn posix2_version() -> i64 {
    std::env::var("_POSIX2_VERSION")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(200809)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX))
}

// `posixtime` syntax bits (gnulib posixtm.h).
const PDS_LEADING_YEAR: u32 = 1;
const PDS_TRAILING_YEAR: u32 = 2;
const PDS_CENTURY: u32 = 4;
const PDS_SECONDS: u32 = 8;
const PDS_PRE_2000: u32 = 16;

/// gnulib's `year`: sets `tm_year` from 0, 1 or 2 digit pairs.
fn posix_year(tm: &mut libc::tm, pairs: &[i32], bits: u32) -> Option<()> {
    match *pairs {
        [yy] => {
            // POSIX: 00-68 is 2000-2068, 69-99 is 1969-1999.
            tm.tm_year = yy;
            if yy <= 68 {
                if bits & PDS_PRE_2000 != 0 {
                    return None;
                }
                tm.tm_year += 100;
            }
        }
        [cc, yy] => {
            if bits & PDS_CENTURY == 0 {
                return None;
            }
            tm.tm_year = cc * 100 + yy - 1900;
        }
        _ => {
            // The current year.
            tm.tm_year = local_time(Stamp::now().sec)?.tm.tm_year;
        }
    }
    Some(())
}

/// gnulib's `posixtime`: a local `[[CC]YY]MMDDhhmm[.ss]` time (with
/// `PDS_TRAILING_YEAR`, `MMDDhhmm[[CC]YY]`), as seconds since the epoch.
fn posixtime(s: &[u8], bits: u32) -> Option<i64> {
    let mut len = s.len();
    let mut dot = None;
    if bits & PDS_SECONDS != 0
        && let Some(d) = s.iter().position(|&c| c == b'.')
    {
        if s.len() - d != 3 {
            return None;
        }
        len = d;
        dot = Some(d);
    }
    if !(8..=12).contains(&len)
        || !len.is_multiple_of(2)
        || !s[..len].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let pairs: Vec<i32> = s[..len]
        .chunks(2)
        .map(|p| i32::from(p[0] - b'0') * 10 + i32::from(p[1] - b'0'))
        .collect();
    let mut tm0 = zero_tm();
    let mut p = pairs.as_slice();
    if bits & PDS_TRAILING_YEAR == 0 {
        posix_year(&mut tm0, &p[..p.len() - 4], bits)?;
        p = &p[p.len() - 4..];
    }
    tm0.tm_mon = p[0] - 1;
    tm0.tm_mday = p[1];
    tm0.tm_hour = p[2];
    tm0.tm_min = p[3];
    if bits & PDS_TRAILING_YEAR != 0 {
        posix_year(&mut tm0, &p[4..], bits)?;
    }
    tm0.tm_sec = match dot {
        None => 0,
        Some(d) if s[d + 1].is_ascii_digit() && s[d + 2].is_ascii_digit() => {
            i32::from(s[d + 1] - b'0') * 10 + i32::from(s[d + 2] - b'0')
        }
        Some(_) => return None,
    };
    let local_zone = TimeZone::from_env();
    let mut leapsec = false;
    loop {
        let mut tm1 = tm0;
        tm1.tm_wday = -1;
        tm1.tm_isdst = -1;
        let t = local_zone.mktime(&mut tm1);
        if tm1.tm_wday < 0 {
            return None;
        }
        // Reject dates like "September 31" and times like "25:61".
        let fields = |tm: &libc::tm| {
            (
                tm.tm_year, tm.tm_mon, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec,
            )
        };
        if fields(&tm0) == fields(&tm1) {
            return t.checked_add(i64::from(leapsec));
        }
        // But allow a seconds count of 60, as the following second.
        if tm0.tm_sec != 60 {
            return None;
        }
        tm0.tm_sec = 59;
        leapsec = true;
    }
}
