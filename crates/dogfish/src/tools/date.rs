//! date — GNU date strings (`-d`), strftime with GNU flags, -I/-R/--rfc-3339

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{DogfishContext, RecordWriter, gnu_io_error, gnu_quote_file, stdout};

use crate::common::date_format::{Stamp, format_time, local_time};
use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, argmatch, getopt_error, lossy, print_help, print_version,
    unsupported, usage_error,
};
use crate::common::json_opt::{check_json_version, json_long, json_short};
use crate::common::parse_datetime::{TimeZone, parse_datetime2, tzset};
use crate::common::quote::quote_locale;

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
  -J, --json[=VERSION]       print each time as an NDJSON record: time (UTC),
                               time_ns, utc_offset, zone and formatted
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
    json_short("json"),
    json_long("json"),
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
    let mut debug = false;
    let (mut discarded_date, mut discarded_set) = (false, false);
    while let Some(item) = opts.next() {
        let (key, raw) = match item {
            Ok(kv) => kv,
            Err(e) => return Ok(getopt_error("date", &e)),
        };
        let value = raw.as_deref().map(lossy);
        match key {
            "date" => {
                discarded_date |= date_raw.is_some();
                date_raw = raw;
            }
            "file" => date_file = value,
            "reference" => reference = value,
            "set" => {
                discarded_set |= set_str.is_some();
                set_str = value;
            }
            "utc" => utc = true,
            "json" => {
                if let Err(code) = check_json_version("date", raw.as_deref()) {
                    return Ok(code);
                }
                json = true;
            }
            "debug" => debug = true,
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
    if discarded_date && debug {
        eprintln!("date: only using last of multiple -d options");
    }
    if discarded_set && debug {
        eprintln!("date: only using last of multiple -s options");
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
    tzset();
    let tz = TimeZone::from_env();
    let fmt = formats
        .pop()
        .unwrap_or_else(|| b"%a %b %e %H:%M:%S %Z %Y".to_vec());

    let now = Stamp::now();
    let mut out = stdout::stdout();
    let emit = |out: &mut stdout::Stdout, stamp: Stamp| -> Result<bool> {
        if debug {
            eprintln!(
                "date: output format: {}",
                quote_locale(OsStr::from_bytes(&fmt))
            );
        }
        if json {
            // The instant in UTC, the local zone's offset and name, and the
            // time as FORMAT (or the default format) shows it.
            let local = local_time(stamp.sec);
            let mut w = RecordWriter::new(&mut *out);
            w.record(|r| {
                r.time("time", stamp.sec, stamp.ns);
                if let Some(lt) = &local {
                    r.i64("utc_offset", lt.tm.tm_gmtoff).bytes("zone", &lt.zone);
                }
                if let Some(text) = render_date(stamp, &fmt) {
                    r.bytes("formatted", text.trim_ascii_end());
                }
            })?;
            w.flush()?;
            return Ok(true);
        }
        match render_date(stamp, &fmt) {
            Some(line) => {
                // GNU's `fprintftime` and `fputc ('\n')`, unchecked: a write
                // error is reported at exit, after all the input.
                stdout::gnu_fwrite(line.len() - 1);
                stdout::gnu_putchars(1);
                let _ = out.write_all(&line);
                Ok(true)
            }
            None => {
                eprintln!(
                    "date: time {} is out of range",
                    quote_locale(stamp.sec.to_string())
                );
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
        // Each line is parsed with its newline (white space to the parser).
        for line in content.split_inclusive(|&c| c == b'\n') {
            match parse_datetime2(line, Stamp::now(), debug, &tz) {
                Some(stamp) => {
                    if !emit(&mut out, stamp)? {
                        status = 1;
                    }
                }
                None => {
                    let line = line.strip_suffix(b"\n").unwrap_or(line);
                    // The message, like the parse, stops at a NUL.
                    let line = &line[..line.iter().position(|&c| c == 0).unwrap_or(line.len())];
                    eprintln!(
                        "date: invalid date {}",
                        quote_locale(OsStr::from_bytes(line))
                    );
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
    } else if let Some(raw) = &date_raw {
        match parse_datetime2(raw.as_bytes(), now, debug, &tz) {
            Some(s) => s,
            None => {
                eprintln!("date: invalid date {}", quote_locale(raw));
                return Ok(1);
            }
        }
    } else {
        now
    };
    Ok(if emit(&mut out, stamp)? { 0 } else { 1 })
}

/// date's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("date", DATE_HELP, DATE_OPTS)
        .choices("iso-8601", ISO_8601_FORMATS.iter().map(|(w, _)| *w))
        .choices("rfc-3339", RFC_3339_FORMATS.iter().map(|(w, _)| *w))
}

/// Entry points for the fuzz targets in `fuzz/` (only with the `fuzzing` feature).
#[cfg(feature = "fuzzing")]
#[allow(dead_code)]
pub mod fuzz {
    use super::*;

    /// Formats `stamp` with `fmt` as `date +FMT` does (`TZ` as set).
    pub fn render(stamp: Stamp, fmt: &[u8]) {
        let _ = render_date(stamp, fmt);
    }
}
