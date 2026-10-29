//! nl — Number lines of files

use std::io::{self, BufRead, Write};

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error,
    io::ignore_broken_pipe, stdout,
};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, lossy_all, print_help, print_version, try_help,
    unsupported, xdectoimax,
};
use crate::common::lines::chomp;
use crate::common::quote::quote_locale;

const NL_HELP: &str = "\
Write each FILE to standard output, with line numbers added

Usage: nl [OPTIONS] [FILES]...

Arguments:
  [FILES]...  

Options:
  -b, --body-numbering <BODY>         Use STYLE for numbering body lines (a: all, t: nonempty, n:
                                      none) [default: t]
  -a, --all                           Number all lines (including empty ones; equivalent to -ba)
  -i, --line-increment <INCREMENT>    Line number increment at each line [default: 1]
  -n, --number-format <FORMAT>        Insert line numbers according to FORMAT (ln, rn, rz) [default:
                                      rn]
  -s, --number-separator <SEPARATOR>  Add STRING after (possible) line number [default: \"\\t\"]
  -v, --starting-line-number <START>  First line number [default: 1]
  -w, --number-width <WIDTH>          Use NUMBER columns for line numbers [default: 6]
      --help                          Print help
";

struct NlArgs {
    body: String,
    all: bool,
    increment: i64,
    format: String,
    separator: String,
    start: i64,
    width: usize,
    files: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NlOpt {
    Body,
    Start,
    Increment,
    Separator,
    Width,
    Format,
    All,
    /// A GNU option Dogfish does not implement yet.
    Unsupported,
    Help,
    Version,
}

/// GNU's `longopts` and `"h:b:f:v:i:pl:s:w:n:d:"`, plus the Dogfish
/// `-a, --all`.
const SPECS: &[OptSpec<NlOpt>] = &[
    OptSpec::both(
        b'h',
        "header-numbering",
        HasArg::Required,
        NlOpt::Unsupported,
    ),
    OptSpec::both(b'b', "body-numbering", HasArg::Required, NlOpt::Body),
    OptSpec::both(
        b'f',
        "footer-numbering",
        HasArg::Required,
        NlOpt::Unsupported,
    ),
    OptSpec::both(b'v', "starting-line-number", HasArg::Required, NlOpt::Start),
    OptSpec::both(b'i', "line-increment", HasArg::Required, NlOpt::Increment),
    OptSpec::both(b'p', "no-renumber", HasArg::No, NlOpt::Unsupported),
    OptSpec::both(
        b'l',
        "join-blank-lines",
        HasArg::Required,
        NlOpt::Unsupported,
    ),
    OptSpec::both(b's', "number-separator", HasArg::Required, NlOpt::Separator),
    OptSpec::both(b'w', "number-width", HasArg::Required, NlOpt::Width),
    OptSpec::both(b'n', "number-format", HasArg::Required, NlOpt::Format),
    OptSpec::both(
        b'd',
        "section-delimiter",
        HasArg::Required,
        NlOpt::Unsupported,
    ),
    OptSpec::long("help", HasArg::No, NlOpt::Help),
    OptSpec::long("version", HasArg::No, NlOpt::Version),
    OptSpec::both(b'a', "all", HasArg::No, NlOpt::All).exact(),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a diagnostic. As in GNU, numbers are checked as they are
/// met, while bad styles and formats are all reported before the usage hint.
fn parse_nl_args(args: Vec<OsString>) -> std::result::Result<NlArgs, i32> {
    let mut p = NlArgs {
        body: "t".to_string(),
        all: false,
        increment: 1,
        format: "rn".to_string(),
        separator: "\t".to_string(),
        start: 1,
        width: 6,
        files: Vec::new(),
    };
    let mut ok = true;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("nl", &e))?;
        let v = v.as_deref().map(lossy).unwrap_or_default();
        match o {
            NlOpt::Body => {
                if !matches!(v.as_str(), "a" | "t" | "n") {
                    eprintln!("nl: invalid body numbering style: {}", quote_locale(&v));
                    ok = false;
                }
                p.body = v;
            }
            NlOpt::Start => {
                p.start = xdectoimax("nl", &v, i64::MIN, i64::MAX, "invalid starting line number")?
            }
            NlOpt::Increment => {
                p.increment = xdectoimax(
                    "nl",
                    &v,
                    i64::MIN,
                    i64::MAX,
                    "invalid line number increment",
                )?
            }
            NlOpt::Separator => p.separator = v,
            NlOpt::Width => {
                let w = xdectoimax(
                    "nl",
                    &v,
                    1,
                    i64::from(i32::MAX),
                    "invalid line number field width",
                )?;
                p.width = usize::try_from(w).unwrap_or(1);
            }
            NlOpt::Format => {
                if !matches!(v.as_str(), "ln" | "rn" | "rz") {
                    eprintln!("nl: invalid line numbering format: {}", quote_locale(&v));
                    ok = false;
                }
                p.format = v;
            }
            NlOpt::All => p.all = true,
            NlOpt::Unsupported => return Err(unsupported("nl", &opts.last_spelling())),
            NlOpt::Help => return Err(print_help(NL_HELP)),
            NlOpt::Version => return Err(print_version("nl")),
        }
    }
    if !ok {
        return Err(try_help("nl"));
    }
    p.files = lossy_all(opts.operands());
    Ok(p)
}

pub fn run_nl(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_nl_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let style = if parsed.all {
        "a"
    } else {
        parsed.body.as_str()
    };
    let width = parsed.width;
    let sources = InputSource::from_args(&parsed.files);
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout::stdout());
    let st = ctx.style();
    let sep = parsed.separator.as_bytes();
    let blank_pad = vec![b' '; width + sep.len()];
    let mut line_no = parsed.start;
    let mut exit_code = 0;

    for src in &sources {
        let mut reader = match FastReader::open(src) {
            Ok(r) => r.into_bufread(),
            Err(e) => {
                let _ = out.flush();
                eprintln!("nl: {}: {}", src.display_name(), gnu_io_error(&e));
                exit_code = 1;
                continue;
            }
        };
        let mut line = Vec::with_capacity(256);
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    let _ = out.flush();
                    eprintln!("nl: {}: {}", src.display_name(), gnu_io_error(&e));
                    exit_code = 1;
                    break;
                }
            }
            ctx.telemetry.add_read(line.len() as u64);
            let text = chomp(&line);
            let numbered = match style {
                "a" => true,
                "t" => !text.is_empty(),
                _ => false,
            };
            if numbered {
                let num = match parsed.format.as_str() {
                    "ln" => format!("{:<width$}", line_no),
                    "rz" if line_no < 0 => {
                        format!("-{:0>w$}", -line_no, w = width.saturating_sub(1))
                    }
                    "rz" => format!("{:0>width$}", line_no),
                    _ => format!("{:>width$}", line_no),
                };
                if st.enabled {
                    write!(out, "\x1b[2m{}\x1b[0m", num)?;
                } else {
                    out.write_all(num.as_bytes())?;
                }
                out.write_all(sep)?;
                line_no += parsed.increment;
            } else {
                out.write_all(&blank_pad)?;
            }
            out.write_all(text)?;
            ignore_broken_pipe(out.write_all(b"\n"))?;
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}
