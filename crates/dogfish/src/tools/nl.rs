//! nl — Number lines of files: GNU `nl.c` with logical pages (header,
//! body and footer sections), `-b/-h/-f` styles including `pBRE`, `-d`,
//! `-l`, `-p`, and GNU's number checks.

use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error,
    io::ignore_broken_pipe, stdout,
};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, lossy_all, print_help, print_version, try_help,
    xdectoimax,
};
use crate::common::gnu_regex::Regex;
use crate::common::input::RawStdout;
use crate::common::quote::{qf, quote_locale};

const NL_HELP: &str = "\
Usage: nl [OPTION]... [FILE]...
Write each FILE to standard output, with line numbers added.

With no FILE, or when FILE is -, read standard input.

Mandatory arguments to long options are mandatory for short options too.
  -b, --body-numbering=STYLE      use STYLE for numbering body lines
  -d, --section-delimiter=CC      use CC for logical page delimiters
  -f, --footer-numbering=STYLE    use STYLE for numbering footer lines
  -h, --header-numbering=STYLE    use STYLE for numbering header lines
  -i, --line-increment=NUMBER     line number increment at each line
  -l, --join-blank-lines=NUMBER   group of NUMBER empty lines counted as one
  -n, --number-format=FORMAT      insert line numbers according to FORMAT
  -p, --no-renumber               do not reset line numbers for each section
  -s, --number-separator=STRING   add STRING after (possible) line number
  -v, --starting-line-number=NUMBER  first line number for each section
  -w, --number-width=NUMBER       use NUMBER columns for line numbers
  -a, --all                       number all lines (as -ba; a Dogfish extension)
      --help        display this help and exit
      --version     output version information and exit

Default options are: -bt -d'\\:' -fn -hn -i1 -l1 -n'rn' -s<TAB> -v1 -w6

CC are two delimiter characters used to construct logical page delimiters;
a missing second character implies ':'.  As a GNU extension one can specify
more than two characters, and also specifying the empty string (-d '')
disables section matching.

STYLE is one of:

  a      number all lines
  t      number only nonempty lines
  n      number no lines
  pBRE   number only lines that contain a match for the basic regular
         expression, BRE

FORMAT is one of:

  ln     left justified, no leading zeros
  rn     right justified, no leading zeros
  rz     right justified, leading zeros
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum NlOpt {
    Header,
    Body,
    Footer,
    Start,
    Increment,
    NoRenumber,
    JoinBlankLines,
    Separator,
    Width,
    Format,
    SectionDelimiter,
    All,
    Help,
    Version,
}

/// GNU's `longopts` and `"h:b:f:v:i:pl:s:w:n:d:"`, plus the Dogfish `-a, --all`.
const SPECS: &[OptSpec<NlOpt>] = &[
    OptSpec::both(b'h', "header-numbering", HasArg::Required, NlOpt::Header),
    OptSpec::both(b'b', "body-numbering", HasArg::Required, NlOpt::Body),
    OptSpec::both(b'f', "footer-numbering", HasArg::Required, NlOpt::Footer),
    OptSpec::both(b'v', "starting-line-number", HasArg::Required, NlOpt::Start),
    OptSpec::both(b'i', "line-increment", HasArg::Required, NlOpt::Increment),
    OptSpec::both(b'p', "no-renumber", HasArg::No, NlOpt::NoRenumber),
    OptSpec::both(
        b'l',
        "join-blank-lines",
        HasArg::Required,
        NlOpt::JoinBlankLines,
    ),
    OptSpec::both(b's', "number-separator", HasArg::Required, NlOpt::Separator),
    OptSpec::both(b'w', "number-width", HasArg::Required, NlOpt::Width),
    OptSpec::both(b'n', "number-format", HasArg::Required, NlOpt::Format),
    OptSpec::both(
        b'd',
        "section-delimiter",
        HasArg::Required,
        NlOpt::SectionDelimiter,
    ),
    OptSpec::long("help", HasArg::No, NlOpt::Help),
    OptSpec::long("version", HasArg::No, NlOpt::Version),
    OptSpec::both(b'a', "all", HasArg::No, NlOpt::All).exact(),
];

/// How a section's lines are numbered (only the first letter of the
/// argument counts, as in GNU).
enum Style {
    All,
    NonEmpty,
    None,
    Matching(Regex),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Left,
    Right,
    RightZero,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Header,
    Body,
    Footer,
}

/// Everything nl was asked to do.
struct NlConfig {
    header: Style,
    body: Style,
    footer: Style,
    start: i64,
    increment: i64,
    blank_join: i64,
    reset_numbers: bool,
    separator: Vec<u8>,
    width: usize,
    format: Format,
    /// The two-character (or GNU-extension longer) section delimiter.
    section_del: Vec<u8>,
}

/// GNU's `build_type_arg`: `Ok(None)` for an unknown style letter.
fn build_style(arg: &[u8]) -> std::result::Result<Option<Style>, String> {
    Ok(match arg.first() {
        Some(b'a') => Some(Style::All),
        Some(b't') => Some(Style::NonEmpty),
        Some(b'n') => Some(Style::None),
        Some(b'p') => Some(Style::Matching(
            Regex::new_basic(&arg[1..]).map_err(str::to_string)?,
        )),
        _ => None,
    })
}

/// Parses the command line in GNU's order: a bad style or format is
/// reported and parsing goes on (then the usage hint and exit 1), a bad
/// number or regex stops at once. Returns the configuration and the files.
fn parse_nl_args(args: Vec<OsString>) -> std::result::Result<(NlConfig, Vec<String>), i32> {
    let fatal = |msg: String| {
        eprintln!("nl: {msg}");
        1
    };
    let mut cfg = NlConfig {
        header: Style::None,
        body: Style::NonEmpty,
        footer: Style::None,
        start: 1,
        increment: 1,
        blank_join: 1,
        reset_numbers: true,
        separator: b"\t".to_vec(),
        width: 6,
        format: Format::Right,
        section_del: b"\\:".to_vec(),
    };
    let mut ok = true;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("nl", &e))?;
        let raw = v.map(OsString::into_vec).unwrap_or_default();
        let text = || lossy(OsStr::from_bytes(&raw));
        let mut style = |what: &str| match build_style(&raw) {
            Ok(Some(s)) => Ok(Some(s)),
            Ok(None) => {
                eprintln!(
                    "nl: invalid {what} numbering style: {}",
                    quote_locale(OsStr::from_bytes(&raw))
                );
                ok = false;
                Ok(None)
            }
            Err(msg) => Err(fatal(msg)),
        };
        match o {
            NlOpt::Header => {
                if let Some(s) = style("header")? {
                    cfg.header = s;
                }
            }
            NlOpt::Body => {
                if let Some(s) = style("body")? {
                    cfg.body = s;
                }
            }
            NlOpt::Footer => {
                if let Some(s) = style("footer")? {
                    cfg.footer = s;
                }
            }
            NlOpt::All => cfg.body = Style::All,
            NlOpt::Start => {
                cfg.start = xdectoimax(
                    "nl",
                    &text(),
                    i64::MIN,
                    i64::MAX,
                    "invalid starting line number",
                )?;
            }
            NlOpt::Increment => {
                cfg.increment = xdectoimax(
                    "nl",
                    &text(),
                    i64::MIN,
                    i64::MAX,
                    "invalid line number increment",
                )?;
            }
            NlOpt::NoRenumber => cfg.reset_numbers = false,
            NlOpt::JoinBlankLines => {
                cfg.blank_join = xdectoimax(
                    "nl",
                    &text(),
                    1,
                    i64::MAX,
                    "invalid line number of blank lines",
                )?;
            }
            NlOpt::Separator => cfg.separator = raw.clone(),
            NlOpt::Width => {
                let w = xdectoimax(
                    "nl",
                    &text(),
                    1,
                    i64::from(i32::MAX),
                    "invalid line number field width",
                )?;
                cfg.width = usize::try_from(w).unwrap_or(6);
            }
            NlOpt::Format => match raw.as_slice() {
                b"ln" => cfg.format = Format::Left,
                b"rn" => cfg.format = Format::Right,
                b"rz" => cfg.format = Format::RightZero,
                _ => {
                    eprintln!(
                        "nl: invalid line numbering format: {}",
                        quote_locale(OsStr::from_bytes(&raw))
                    );
                    ok = false;
                }
            },
            // POSIX: one character replaces the first of the default pair.
            NlOpt::SectionDelimiter => {
                cfg.section_del = match raw.len() {
                    1 => vec![raw[0], b':'],
                    _ => raw.clone(),
                };
            }
            NlOpt::Help => return Err(print_help(NL_HELP)),
            NlOpt::Version => return Err(print_version("nl")),
        }
    }
    if !ok {
        return Err(try_help("nl"));
    }
    Ok((cfg, lossy_all(opts.operands())))
}

/// Numbering state that carries across files.
struct NlState {
    line_no: i64,
    overflow: bool,
    section: Section,
    blank_lines: i64,
}

/// Why numbering stopped: a write error, or the line number overflowed.
enum NlError {
    Write(io::Error),
    Overflow,
}

impl From<io::Error> for NlError {
    fn from(e: io::Error) -> Self {
        Self::Write(e)
    }
}

impl NlConfig {
    fn style(&self, section: Section) -> &Style {
        match section {
            Section::Header => &self.header,
            Section::Body => &self.body,
            Section::Footer => &self.footer,
        }
    }

    /// GNU's `check_section` for `line` (without its newline).
    fn section_of(&self, line: &[u8]) -> Option<Section> {
        let del = &self.section_del;
        if line.len() < 2 || del.len() < 2 || line[..2] != del[..2] {
            return None;
        }
        let repeated =
            |n: usize| line.len() == del.len() * n && line.chunks(del.len()).all(|c| c == del);
        if repeated(3) {
            Some(Section::Header)
        } else if repeated(2) {
            Some(Section::Body)
        } else if repeated(1) {
            Some(Section::Footer)
        } else {
            None
        }
    }

    fn print_lineno<W: Write>(
        &self,
        st: &mut NlState,
        out: &mut W,
        dim: bool,
    ) -> Result<(), NlError> {
        if st.overflow {
            return Err(NlError::Overflow);
        }
        let width = self.width;
        let n = st.line_no;
        let num = match self.format {
            Format::Left => format!("{n:<width$}"),
            Format::Right => format!("{n:>width$}"),
            Format::RightZero => format!("{n:0width$}"),
        };
        if dim {
            write!(out, "\x1b[2m{num}\x1b[0m")?;
        } else {
            out.write_all(num.as_bytes())?;
        }
        out.write_all(&self.separator)?;
        match st.line_no.checked_add(self.increment) {
            Some(v) => st.line_no = v,
            None => st.overflow = true,
        }
        Ok(())
    }

    /// GNU's `process_file` over one input.
    fn process<W: Write>(
        &self,
        input: &mut dyn BufRead,
        st: &mut NlState,
        out: &mut W,
        dim: bool,
    ) -> Result<std::result::Result<(), io::Error>, NlError> {
        let mut line = Vec::with_capacity(256);
        let blank = vec![b' '; self.width + self.separator.len()];
        loop {
            line.clear();
            match input.read_until(b'\n', &mut line) {
                Ok(0) => return Ok(Ok(())),
                Ok(_) => {}
                Err(e) => return Ok(Err(e)),
            }
            let text = line.strip_suffix(b"\n").unwrap_or(&line);
            if let Some(section) = self.section_of(text) {
                st.section = section;
                if self.reset_numbers {
                    st.line_no = self.start;
                    st.overflow = false;
                }
                out.write_all(b"\n")?;
                continue;
            }
            let numbered = match self.style(st.section) {
                Style::All if self.blank_join > 1 => {
                    st.blank_lines += 1;
                    if !text.is_empty() || st.blank_lines == self.blank_join {
                        st.blank_lines = 0;
                        true
                    } else {
                        false
                    }
                }
                Style::All => true,
                Style::NonEmpty => !text.is_empty(),
                Style::None => false,
                Style::Matching(re) => re.is_match(text),
            };
            if numbered {
                self.print_lineno(st, out, dim)?;
            } else {
                out.write_all(&blank)?;
            }
            out.write_all(text)?;
            out.write_all(b"\n")?;
        }
    }
}

pub fn run_nl(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let (cfg, files) = match parse_nl_args(args) {
        Ok(parsed) => parsed,
        Err(status) => return Ok(status),
    };
    let sources = InputSource::from_args(&files);
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, RawStdout);
    let dim = ctx.style().enabled;
    let mut st = NlState {
        line_no: cfg.start,
        overflow: false,
        section: Section::Body,
        blank_lines: 0,
    };
    let mut exit_code = 0;

    for src in &sources {
        let mut reader = match FastReader::open(src) {
            Ok(r) => r.into_bufread(),
            Err(e) => {
                let _ = out.flush();
                eprintln!("nl: {}: {}", qf(src.display_name()), gnu_io_error(&e));
                exit_code = 1;
                continue;
            }
        };
        match cfg.process(&mut *reader, &mut st, &mut out, dim) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = out.flush();
                eprintln!("nl: {}: {}", qf(src.display_name()), gnu_io_error(&e));
                exit_code = 1;
            }
            Err(NlError::Overflow) => {
                let _ = out.flush();
                eprintln!("nl: line number overflow");
                return Ok(1);
            }
            Err(NlError::Write(e)) if e.kind() == io::ErrorKind::BrokenPipe => {
                return Ok(exit_code);
            }
            Err(NlError::Write(e)) if stdout::is_write_error(&e) => return Err(e.into()),
            Err(NlError::Write(e)) => return Err(e.into()),
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}

/// Entry points for the fuzz targets in `fuzz/` (only with the `fuzzing` feature).
#[cfg(feature = "fuzzing")]
#[allow(dead_code)]
pub mod fuzz {
    use super::*;

    pub fn args(args: Vec<OsString>) {
        let _ = parse_nl_args(args);
    }
}
