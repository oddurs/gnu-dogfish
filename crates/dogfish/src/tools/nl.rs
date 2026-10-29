//! nl — Number lines of files: GNU `nl.c` with logical pages (header,
//! body and footer sections), `-b/-h/-f` styles including `pBRE`, `-d`,
//! `-l`, `-p`, and GNU's number checks.

use std::io::{self, BufRead, Write};

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser};
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, FastReader, InputSource, gnu_io_error, io::ignore_broken_pipe,
};

use crate::common::gnu_regex::Regex;
use crate::common::quote::{qf, quote_locale};
use crate::common::xstrtol::xdectoimax;

#[derive(Parser)]
#[command(
    name = "nl",
    about = "Write each FILE to standard output, with line numbers added",
    disable_help_flag = true
)]
struct NlArgs {
    /// Print help
    #[arg(long = "help", action = clap::ArgAction::Help)]
    help: Option<bool>,

    /// Use STYLE for numbering body lines (a: all, t: nonempty, n: none, pBRE)
    #[arg(short = 'b', long = "body-numbering")]
    body: Option<String>,

    /// Use STYLE for numbering header lines
    #[arg(short = 'h', long = "header-numbering")]
    header: Option<String>,

    /// Use STYLE for numbering footer lines
    #[arg(short = 'f', long = "footer-numbering")]
    footer: Option<String>,

    /// Number all lines (including empty ones; equivalent to -ba)
    #[arg(short = 'a', long = "all")]
    all: bool,

    /// Use CC for logical page delimiters
    #[arg(short = 'd', long = "section-delimiter", value_name = "CC")]
    section_delimiter: Option<String>,

    /// Line number increment at each line
    #[arg(short = 'i', long = "line-increment", allow_hyphen_values = true)]
    increment: Option<String>,

    /// Group of NUMBER empty lines counted as one
    #[arg(short = 'l', long = "join-blank-lines", allow_hyphen_values = true)]
    join_blank_lines: Option<String>,

    /// Insert line numbers according to FORMAT (ln, rn, rz)
    #[arg(short = 'n', long = "number-format")]
    format: Option<String>,

    /// Do not reset line numbers for each section
    #[arg(short = 'p', long = "no-renumber")]
    no_renumber: bool,

    /// Add STRING after (possible) line number
    #[arg(short = 's', long = "number-separator", default_value = "\t")]
    separator: String,

    /// First line number for each section
    #[arg(short = 'v', long = "starting-line-number", allow_hyphen_values = true)]
    start: Option<String>,

    /// Use NUMBER columns for line numbers
    #[arg(short = 'w', long = "number-width", allow_hyphen_values = true)]
    width: Option<String>,

    files: Vec<String>,
}

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
fn build_style(arg: &str) -> std::result::Result<Option<Style>, String> {
    Ok(match arg.as_bytes().first() {
        Some(b'a') => Some(Style::All),
        Some(b't') => Some(Style::NonEmpty),
        Some(b'n') => Some(Style::None),
        Some(b'p') => Some(Style::Matching(
            Regex::new_basic(&arg.as_bytes()[1..]).map_err(str::to_string)?,
        )),
        _ => None,
    })
}

/// Option processing in GNU's terms; `order` is where -h, -b and -f were
/// given. `Err` is the exit status after the diagnostics.
fn parse_config(p: &NlArgs, order: [usize; 3]) -> std::result::Result<NlConfig, i32> {
    let fatal = |msg: String| {
        eprintln!("nl: {msg}");
        1
    };
    let mut ok = true;
    let mut style = |arg: &Option<String>, default: Style, what: &str| match arg {
        None => Ok(default),
        Some(a) => match build_style(a) {
            Ok(Some(s)) => Ok(s),
            Ok(None) => {
                eprintln!("nl: invalid {what} numbering style: {}", quote_locale(a));
                ok = false;
                Ok(Style::None)
            }
            Err(msg) => Err(fatal(msg)),
        },
    };
    // Diagnose the styles in command-line order, as GNU does.
    let mut styles: [Option<Style>; 3] = [None, None, None];
    let mut keys = [0, 1, 2];
    keys.sort_by_key(|&k| order[k]);
    for k in keys {
        styles[k] = Some(match k {
            0 => style(&p.header, Style::None, "header")?,
            1 if p.all => Style::All,
            1 => style(&p.body, Style::NonEmpty, "body")?,
            _ => style(&p.footer, Style::None, "footer")?,
        });
    }
    let [Some(header), Some(body), Some(footer)] = styles else {
        return Err(1);
    };
    let number = |arg: &Option<String>, default: i64, min: i64, max: i64, what: &str| match arg {
        None => Ok(default),
        Some(a) => xdectoimax(a, min, max, what).map_err(fatal),
    };
    let start = number(
        &p.start,
        1,
        i64::MIN,
        i64::MAX,
        "invalid starting line number",
    )?;
    let increment = number(
        &p.increment,
        1,
        i64::MIN,
        i64::MAX,
        "invalid line number increment",
    )?;
    let blank_join = number(
        &p.join_blank_lines,
        1,
        1,
        i64::MAX,
        "invalid line number of blank lines",
    )?;
    let width = number(
        &p.width,
        6,
        1,
        i64::from(i32::MAX),
        "invalid line number field width",
    )?;
    let format = match p.format.as_deref() {
        None | Some("rn") => Format::Right,
        Some("ln") => Format::Left,
        Some("rz") => Format::RightZero,
        Some(other) => {
            eprintln!("nl: invalid line numbering format: {}", quote_locale(other));
            ok = false;
            Format::Right
        }
    };
    if !ok {
        return Err(crate::common::getopt::try_help("nl", 1));
    }
    let section_del = match p.section_delimiter.as_deref() {
        None => b"\\:".to_vec(),
        // POSIX: one character replaces the first of the default pair.
        Some(d) if d.len() == 1 => vec![d.as_bytes()[0], b':'],
        Some(d) => d.as_bytes().to_vec(),
    };
    Ok(NlConfig {
        header,
        body,
        footer,
        start,
        increment,
        blank_join,
        reset_numbers: !p.no_renumber,
        separator: p.separator.as_bytes().to_vec(),
        width: usize::try_from(width).unwrap_or(6),
        format,
        section_del,
    })
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

pub fn run_nl(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let matches = NlArgs::command().try_get_matches_from(args)?;
    let parsed = NlArgs::from_arg_matches(&matches)?;
    let order = ["header", "body", "footer"].map(|id| matches.index_of(id).unwrap_or(usize::MAX));
    let cfg = match parse_config(&parsed, order) {
        Ok(cfg) => cfg,
        Err(status) => return Ok(status),
    };
    let sources = InputSource::from_args(&parsed.files);
    let stdout = io::stdout();
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, stdout.lock());
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
            Err(NlError::Write(e)) => return Err(e.into()),
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}
