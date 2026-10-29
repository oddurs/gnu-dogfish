//! date — GNU date strings (`-d`), strftime with GNU flags, -I/-R/--rfc-3339

use std::io::{self, Read, Write};

use anyhow::Result;
use dogfish_core::{
    DogfishContext, gnu_io_error, gnu_quote, gnu_quote_file, io::ignore_broken_pipe,
};

use crate::common::date_format::{LocalTime, Stamp, format_time, local_time};
use crate::common::getopt::{GetOpt, HasArg, OptSpec, argmatch, opt, print_help, usage_error};
use crate::common::parse_datetime::{TimeZone, parse_datetime2, tzset};

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

static DATE_OPTS: [OptSpec; 15] = [
    opt(Some('d'), Some("date"), HasArg::Required, "date"),
    opt(Some('f'), Some("file"), HasArg::Required, "file"),
    opt(Some('I'), Some("iso-8601"), HasArg::Optional, "iso-8601"),
    opt(Some('r'), Some("reference"), HasArg::Required, "reference"),
    opt(Some('R'), Some("rfc-email"), HasArg::No, "rfc-email"),
    opt(None, Some("rfc-822"), HasArg::No, "rfc-email"),
    opt(None, Some("rfc-2822"), HasArg::No, "rfc-email"),
    opt(None, Some("rfc-3339"), HasArg::Required, "rfc-3339"),
    opt(Some('s'), Some("set"), HasArg::Required, "set"),
    opt(Some('u'), Some("utc"), HasArg::No, "utc"),
    opt(None, Some("universal"), HasArg::No, "utc"),
    opt(None, Some("debug"), HasArg::No, "debug"),
    opt(Some('J'), Some("json"), HasArg::No, "json"),
    opt(None, Some("help"), HasArg::No, "help"),
    opt(None, Some("version"), HasArg::No, "version"),
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
fn render_date(stamp: Stamp, fmt: &str) -> Option<Vec<u8>> {
    let lt = local_time(stamp.sec)?;
    let mut line = format_time(fmt.as_bytes(), &lt, stamp.ns, stamp.sec, false);
    line.push(b'\n');
    Some(line)
}

pub fn run_date(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let usage = |msg: String| Ok(usage_error("date", msg, 1));
    let mut opts = GetOpt::new(args.into_iter().skip(1).collect(), &DATE_OPTS, true);
    let mut date_str: Option<String> = None;
    let mut date_file: Option<String> = None;
    let mut reference: Option<String> = None;
    let mut set_str: Option<String> = None;
    let mut formats: Vec<String> = Vec::new();
    let mut utc = false;
    let mut json = false;
    let mut debug = false;
    let (mut discarded_date, mut discarded_set) = (false, false);
    while let Some(item) = opts.next_opt() {
        let (key, value) = match item {
            Ok(kv) => kv,
            Err(msg) => return usage(msg),
        };
        match key {
            "date" => {
                discarded_date |= date_str.is_some();
                date_str = value;
            }
            "file" => date_file = value,
            "reference" => reference = value,
            "set" => {
                discarded_set |= set_str.is_some();
                set_str = value;
            }
            "utc" => utc = true,
            "json" => json = true,
            "debug" => debug = true,
            "rfc-email" => formats.push("%a, %d %b %Y %H:%M:%S %z".to_string()),
            "iso-8601" => {
                let names = ISO_8601_FORMATS.map(|(n, _)| n);
                let spec = value.as_deref().unwrap_or("date");
                match argmatch("date", "--iso-8601", spec, &names, 1) {
                    Ok(i) => formats.push(ISO_8601_FORMATS[i].1.to_string()),
                    Err(status) => return Ok(status),
                }
            }
            "rfc-3339" => {
                let names = RFC_3339_FORMATS.map(|(n, _)| n);
                let spec = value.as_deref().unwrap_or_default();
                match argmatch("date", "--rfc-3339", spec, &names, 1) {
                    Ok(i) => formats.push(RFC_3339_FORMATS[i].1.to_string()),
                    Err(status) => return Ok(status),
                }
            }
            "help" => return print_help(DATE_HELP),
            _ => {
                println!("date (GNU/Dogfish) {}", env!("CARGO_PKG_VERSION"));
                return Ok(0);
            }
        }
    }
    let operands = opts.into_operands();
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
        return usage(format!("extra operand {}", gnu_quote(&operands[1])));
    }
    if let Some(op) = operands.first() {
        match op.strip_prefix('+') {
            Some(f) => formats.push(f.to_string()),
            None if sources > 0 || set_str.is_some() => {
                return usage(format!(
                    "the argument {} lacks a leading '+';\n\
                     when using an option to specify date(s), any non-option\n\
                     argument must be a format string beginning with '+'",
                    gnu_quote(op)
                ));
            }
            None => set_str = Some(op.clone()),
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
        .unwrap_or_else(|| "%a %b %e %H:%M:%S %Z %Y".to_string());

    let now = Stamp::now();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let emit = |out: &mut io::StdoutLock<'_>, stamp: Stamp| -> Result<bool> {
        if debug {
            eprintln!("date: output format: {}", gnu_quote(&fmt));
        }
        if json {
            let iso = |s: Stamp| {
                render_date(s, "%Y-%m-%dT%H:%M:%S%:z")
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
                eprintln!(
                    "date: time {} is out of range",
                    gnu_quote(&stamp.sec.to_string())
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
                        gnu_quote(&String::from_utf8_lossy(line))
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
    } else if let Some(d) = &date_str {
        match parse_datetime2(d.as_bytes(), now, debug, &tz) {
            Some(s) => s,
            None => {
                eprintln!("date: invalid date {}", gnu_quote(d));
                return Ok(1);
            }
        }
    } else {
        now
    };
    Ok(if emit(&mut out, stamp)? { 0 } else { 1 })
}
