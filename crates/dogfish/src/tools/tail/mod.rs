//! tail — a port of GNU tail: the last part of files (reading backwards from
//! the end of seekable files), `+N`, and following files as they grow
//! (`-f`/`-F`: by descriptor or by name, with inotify on Linux and polling
//! elsewhere), plus NDJSON object streams on a terminal.

mod follow;
#[cfg(target_os = "linux")]
mod inotify;

use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::ManuallyDrop;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use anyhow::Result;
use dogfish_core::{
    DogfishContext, emit_objects_or_table, gnu_io_error, io::ignore_broken_pipe, read_ndjson_stdin,
    stdout,
};
use memchr::{memchr_iter, memrchr};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, argmatch, getopt_error, lossy, print_help, print_version,
};
use crate::common::head_tail::{last_lines_start, parse_count, to_usize};
use crate::common::input::{RawStdout, close_stdin, safe_read, stdin_file, stdin_is_closed};
use crate::common::lines::{line_end, looks_like_json, lossy_lines};
use crate::common::posixver::posix2_version;
use crate::common::quote::{qa, qf, quote_locale};
use crate::common::strtod::strtod_prefix;
use crate::common::xstrtol::{StrtolError, xdectoumax, xstrtoumax};

use follow::{FileSpec, FollowMode};

const TAIL_HELP: &str = "\
Output the last part of files (or NDJSON object streams)

Usage: tail [OPTIONS] [FILES]...

Arguments:
  [FILES]...

Options:
  -n, --lines <[+]NUM>  Output the last NUM lines (default 10); or use -n +NUM to skip NUM-1 lines
                        at the start
  -c, --bytes <[+]NUM>  Output the last NUM bytes; or use -c +NUM to output starting with byte NUM
  -f, --follow[={name|descriptor}]
                        Output appended data as the file grows; an absent option argument means
                        'descriptor'
  -F                    Same as --follow=name --retry
      --max-unchanged-stats <N>
                        With --follow=name, reopen a FILE which has not changed size after N
                        (default 5) iterations to see if it has been unlinked or renamed
      --pid <PID>       With -f, terminate after process ID, PID dies
  -q, --quiet           Never output headers giving file names [alias: --silent]
      --retry           Keep trying to open a file if it is inaccessible
  -s, --sleep-interval <N>
                        With -f, sleep for approximately N seconds (default 1.0) between
                        iterations; with inotify and --pid=P, check process P at least once
                        every N seconds
  -v, --verbose         Always output headers giving file names
  -z, --zero-terminated Line delimiter is NUL, not newline
  -J, --json
      --help            Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum TailOpt {
    Bytes,
    Follow,
    FollowWhen,
    FollowName,
    Lines,
    MaxUnchangedStats,
    DisableInotify,
    Pid,
    PresumeInputPipe,
    Quiet,
    Retry,
    SleepInterval,
    Verbose,
    ZeroTerminated,
    Json,
    Digit(u8),
    Help,
    Version,
}

const fn digit(c: u8) -> OptSpec<TailOpt> {
    OptSpec::short(c, HasArg::No, TailOpt::Digit(c))
}

/// GNU's `long_options` and `"c:n:fFqs:vz0123456789"`, plus `-J, --json`.
/// `---disable-inotify` and `---presume-input-pipe` are GNU's hidden test
/// options.
const SPECS: &[OptSpec<TailOpt>] = &[
    OptSpec::both(b'c', "bytes", HasArg::Required, TailOpt::Bytes),
    OptSpec::long("follow", HasArg::Optional, TailOpt::FollowWhen),
    OptSpec::both(b'n', "lines", HasArg::Required, TailOpt::Lines),
    OptSpec::long(
        "max-unchanged-stats",
        HasArg::Required,
        TailOpt::MaxUnchangedStats,
    ),
    OptSpec::long("-disable-inotify", HasArg::No, TailOpt::DisableInotify),
    OptSpec::long("pid", HasArg::Required, TailOpt::Pid),
    OptSpec::long("-presume-input-pipe", HasArg::No, TailOpt::PresumeInputPipe),
    OptSpec::both(b'q', "quiet", HasArg::No, TailOpt::Quiet),
    OptSpec::long("retry", HasArg::No, TailOpt::Retry),
    OptSpec::long("silent", HasArg::No, TailOpt::Quiet),
    OptSpec::both(
        b's',
        "sleep-interval",
        HasArg::Required,
        TailOpt::SleepInterval,
    ),
    OptSpec::both(b'v', "verbose", HasArg::No, TailOpt::Verbose),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, TailOpt::ZeroTerminated),
    OptSpec::long("help", HasArg::No, TailOpt::Help),
    OptSpec::long("version", HasArg::No, TailOpt::Version),
    OptSpec::both(b'J', "json", HasArg::No, TailOpt::Json).exact(),
    OptSpec::short(b'f', HasArg::No, TailOpt::Follow),
    OptSpec::short(b'F', HasArg::No, TailOpt::FollowName),
    digit(b'0'),
    digit(b'1'),
    digit(b'2'),
    digit(b'3'),
    digit(b'4'),
    digit(b'5'),
    digit(b'6'),
    digit(b'7'),
    digit(b'8'),
    digit(b'9'),
];

/// When to print `==> NAME <==` headers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HeaderMode {
    MultipleFiles,
    Always,
    Never,
}

/// What `tail` was asked for (GNU's globals); as in GNU, the last `-c`/`-n`
/// wins.
pub(super) struct TailArgs {
    count_lines: bool,
    from_start: bool,
    /// From the start: the number of items to skip (once adjusted in
    /// `run_tail`); otherwise the number of items to print.
    n_units: u64,
    forever: bool,
    follow_mode: FollowMode,
    /// `--retry`: keep trying to open inaccessible files.
    reopen_inaccessible_files: bool,
    max_n_unchanged_stats_between_opens: u64,
    /// `--pid`: the writer to watch; 0 for none.
    pid: libc::pid_t,
    sleep_interval: f64,
    presume_input_pipe: bool,
    disable_inotify: bool,
    header_mode: HeaderMode,
    line_end: u8,
    json: bool,
    files: Vec<OsString>,
}

/// GNU's `xstrtod` with `cl_strtod`: the whole of `s` is a floating-point
/// number that did not overflow.
fn parse_seconds(s: &str) -> Option<f64> {
    let (value, used) = strtod_prefix(s)?;
    if used != s.len() || used == 0 {
        return None;
    }
    if value.is_infinite() && !s.to_ascii_lowercase().contains("inf") {
        return None;
    }
    Some(value)
}

/// Parses the command line; `Err` is the exit status after a diagnostic.
fn parse_tail_args(args: Vec<OsString>) -> std::result::Result<TailArgs, i32> {
    let mut a = TailArgs {
        count_lines: true,
        from_start: false,
        n_units: 10,
        forever: false,
        follow_mode: FollowMode::Descriptor,
        reopen_inaccessible_files: false,
        max_n_unchanged_stats_between_opens: 5,
        pid: 0,
        sleep_interval: 1.0,
        presume_input_pipe: false,
        disable_inotify: false,
        header_mode: HeaderMode::MultipleFiles,
        line_end: line_end(false),
        json: false,
        files: Vec::new(),
    };
    let args = expand_obsolete(args, &mut a)?;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("tail", &e))?;
        let value = v.as_deref().map(lossy).unwrap_or_default();
        match o {
            TailOpt::FollowName => {
                a.forever = true;
                a.follow_mode = FollowMode::Name;
                a.reopen_inaccessible_files = true;
            }
            TailOpt::Bytes | TailOpt::Lines => {
                // Like GNU, each count is checked as it is met.
                a.count_lines = o == TailOpt::Lines;
                let num = match value.strip_prefix('+') {
                    Some(_) => {
                        a.from_start = true;
                        &value[..]
                    }
                    None => value.strip_prefix('-').unwrap_or(&value),
                };
                a.n_units = parse_count(num, a.count_lines).map_err(|msg| {
                    eprintln!("tail: {msg}");
                    1
                })?;
            }
            TailOpt::Follow => {
                a.forever = true;
                a.follow_mode = FollowMode::Descriptor;
            }
            TailOpt::FollowWhen => {
                a.forever = true;
                a.follow_mode = match v {
                    None => FollowMode::Descriptor,
                    Some(how) => argmatch(
                        "tail",
                        "--follow",
                        how,
                        &[
                            ("descriptor", FollowMode::Descriptor),
                            ("name", FollowMode::Name),
                        ],
                    )?,
                };
            }
            TailOpt::Retry => a.reopen_inaccessible_files = true,
            TailOpt::MaxUnchangedStats => {
                a.max_n_unchanged_stats_between_opens = xdectoumax(
                    &value,
                    0,
                    u64::MAX,
                    "",
                    "invalid maximum number of unchanged stats between opens",
                )
                .map_err(|msg| {
                    eprintln!("tail: {msg}");
                    1
                })?;
            }
            TailOpt::DisableInotify => a.disable_inotify = true,
            TailOpt::Pid => {
                let pid =
                    xdectoumax(&value, 0, i32::MAX as u64, "", "invalid PID").map_err(|msg| {
                        eprintln!("tail: {msg}");
                        1
                    })?;
                a.pid = libc::pid_t::try_from(pid).unwrap_or(libc::pid_t::MAX);
            }
            TailOpt::PresumeInputPipe => a.presume_input_pipe = true,
            TailOpt::Quiet => a.header_mode = HeaderMode::Never,
            TailOpt::SleepInterval => match parse_seconds(&value) {
                Some(s) if s >= 0.0 => a.sleep_interval = s,
                _ => {
                    eprintln!("tail: invalid number of seconds: {}", quote_locale(&value));
                    return Err(1);
                }
            },
            TailOpt::Verbose => a.header_mode = HeaderMode::Always,
            TailOpt::ZeroTerminated => a.line_end = line_end(true),
            TailOpt::Json => a.json = true,
            TailOpt::Digit(c) => {
                eprintln!("tail: option used in invalid context -- {}", char::from(c));
                return Err(1);
            }
            TailOpt::Help => return Err(print_help(TAIL_HELP)),
            TailOpt::Version => return Err(print_version("tail")),
        }
    }

    if a.reopen_inaccessible_files {
        if !a.forever {
            a.reopen_inaccessible_files = false;
            eprintln!("tail: warning: --retry ignored; --retry is useful only when following");
        } else if a.follow_mode == FollowMode::Descriptor {
            eprintln!("tail: warning: --retry only effective for the initial open");
        }
    }
    if a.pid != 0 && !a.forever {
        eprintln!("tail: warning: PID ignored; --pid=PID is useful only when following");
    } else if a.pid != 0
        && follow::kill0(a.pid).is_err_and(|e| e.raw_os_error() == Some(libc::ENOSYS))
    {
        eprintln!("tail: warning: --pid=PID is not supported on this system");
        a.pid = 0;
    }
    a.files = opts.operands();
    Ok(a)
}

/// GNU's `parse_obsolete_option`: with at most one file operand, a first
/// argument such as `-5`, `+2c`, `-l`, `-3bf` is the whole option list. It
/// sets the count, units, origin and `-f`, and is dropped from the
/// arguments; `Err` is the exit status after GNU's `invalid number`
/// diagnostic.
fn expand_obsolete(mut args: Vec<OsString>, a: &mut TailArgs) -> Result<Vec<OsString>, i32> {
    let argc = args.len();
    let operand_ok = argc == 2
        || (argc == 3 && !(args[2].as_bytes().starts_with(b"-") && args[2].len() > 1))
        || ((3..=4).contains(&argc) && args[2] == "--");
    if !operand_ok {
        return Ok(args);
    }
    let posix_ver = posix2_version();
    let obsolete_usage = posix_ver < 200112;
    let traditional_usage = obsolete_usage || 200809 <= posix_ver;
    let first = args[1].clone();
    let arg = first.as_bytes();
    let from_start = match arg.first() {
        Some(b'+') if traditional_usage => true,
        // "-" alone is standard input and "-c" needs an argument, except in
        // the obsolete usage; "-5" and the like are always accepted.
        Some(b'-')
            if obsolete_usage
                || arg
                    .get(1 + usize::from(arg.get(1) == Some(&b'c')))
                    .is_some() =>
        {
            false
        }
        _ => return Ok(args),
    };
    let rest = &arg[1..];
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let mut p = digits;
    let mut default_count = 10u64;
    let mut count_lines = true;
    match rest.get(p) {
        Some(b'b') => {
            default_count *= 512;
            count_lines = false;
            p += 1;
        }
        Some(b'c') => {
            count_lines = false;
            p += 1;
        }
        Some(b'l') => p += 1,
        _ => {}
    }
    let forever = rest.get(p) == Some(&b'f');
    if forever {
        p += 1;
    }
    if p != rest.len() {
        return Ok(args);
    }
    a.n_units = if digits == 0 {
        default_count
    } else {
        let digits = std::str::from_utf8(&rest[..digits]).unwrap_or_default();
        match xstrtoumax(digits, "b") {
            (n, None | Some(StrtolError::InvalidSuffix)) => n,
            _ => {
                eprintln!(
                    "tail: invalid number: {}: Numerical result out of range",
                    quote_locale(&first)
                );
                return Err(1);
            }
        }
    };
    a.from_start = from_start;
    a.count_lines = count_lines;
    a.forever = forever;
    args.remove(1);
    Ok(args)
}

/// gnulib `error (EXIT_FAILURE, ...)`: the diagnostic has been printed and
/// tail exits with this status.
pub(super) struct Fatal(pub(super) i32);

pub(super) type R<T> = std::result::Result<T, Fatal>;

/// `COPY_TO_EOF` and `COPY_A_BUFFER`, dump_remainder's special counts.
const COPY_TO_EOF: u64 = u64::MAX;
const COPY_A_BUFFER: u64 = u64::MAX - 1;

/// The read size (GNU's `BUFSIZ`, larger here for throughput).
const BUFSIZ: usize = 64 * 1024;

/// `S_IFMT` and the file types tail cares about, as in `st_mode`.
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFIFO: u32 = 0o010000;
const S_IFSOCK: u32 = 0o140000;
const S_IFCHR: u32 = 0o020000;

pub(super) fn is_reg(mode: u32) -> bool {
    mode & S_IFMT == S_IFREG
}

pub(super) fn is_fifo(mode: u32) -> bool {
    mode & S_IFMT == S_IFIFO
}

/// GNU's `IS_TAILABLE_FILE_TYPE`.
pub(super) fn is_tailable_file_type(mode: u32) -> bool {
    matches!(mode & S_IFMT, S_IFREG | S_IFIFO | S_IFSOCK | S_IFCHR)
}

/// An open input: standard input (never closed) or a file tail opened.
pub(super) enum Desc {
    Stdin(ManuallyDrop<File>),
    Owned(File),
}

impl Desc {
    pub(super) fn file(&self) -> &File {
        match self {
            Desc::Stdin(f) => f,
            Desc::Owned(f) => f,
        }
    }

    pub(super) fn raw(&self) -> RawFd {
        self.file().as_raw_fd()
    }

    /// `open (name, O_RDONLY | (nonblocking ? O_NONBLOCK : 0))`, or standard
    /// input for `-`.
    pub(super) fn open(name: &OsStr, is_stdin: bool, nonblocking: bool) -> io::Result<Desc> {
        if is_stdin {
            return Ok(Desc::Stdin(stdin_file()));
        }
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(if nonblocking { libc::O_NONBLOCK } else { 0 })
            .open(name)
            .map(Desc::Owned)
    }

    /// `fstat`; standard input that was closed at start-up fails as in GNU.
    pub(super) fn fstat(&self) -> io::Result<Metadata> {
        if matches!(self, Desc::Stdin(_)) && stdin_is_closed() {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        self.file().metadata()
    }

    pub(super) fn lseek(&self, pos: SeekFrom) -> io::Result<u64> {
        let mut f = self.file();
        f.seek(pos)
    }

    pub(super) fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut f = self.file();
        safe_read(&mut f, buf)
    }
}

/// `st_blksize` as gnulib's `ST_BLKSIZE` sanitizes it.
fn st_blksize(st: &Metadata) -> u64 {
    match st.blksize() {
        0 => 512,
        n if n > (usize::MAX / 8 + 1) as u64 => 512,
        n => n,
    }
}

/// Standard output, buffered as stdio buffers it for GNU tail: diagnostics
/// flush it first (as gnulib's `error` does), and following flushes it at
/// GNU's points.
pub(super) struct Out {
    buf: Vec<u8>,
    written: u64,
}

impl Out {
    fn new() -> Self {
        Out {
            buf: Vec::with_capacity(BUFSIZ),
            written: 0,
        }
    }

    /// `xwrite_stdout`: a failure is fatal, `error writing 'standard output'`.
    pub(super) fn write(&mut self, data: &[u8]) -> R<()> {
        self.written += data.len() as u64;
        if self.buf.len() + data.len() <= BUFSIZ {
            self.buf.extend_from_slice(data);
            return Ok(());
        }
        let res = self.drain().and_then(|()| stdout::write_whole(data));
        res.map_err(|e| {
            stdout::mark_reported();
            eprintln!(
                "tail: error writing 'standard output': {}",
                gnu_io_error(&e)
            );
            Fatal(1)
        })
    }

    fn drain(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let res = stdout::write_whole(&self.buf);
        self.buf.clear();
        res
    }

    /// `fflush (stdout)`, ignoring a failure (it is reported at exit).
    pub(super) fn flush_quietly(&mut self) {
        let _ = self.drain();
    }

    /// `fflush (stdout) != 0 && write_error ()`.
    pub(super) fn flush(&mut self) -> R<()> {
        self.drain().map_err(|e| {
            stdout::mark_reported();
            eprintln!("tail: write error: {}", gnu_io_error(&e));
            Fatal(1)
        })
    }
}

/// tail's state: its options, standard output and GNU's static flags.
pub(super) struct Tail {
    pub(super) a: TailArgs,
    pub(super) out: Out,
    pub(super) print_headers: bool,
    first_file: bool,
    pub(super) monitor_output: bool,
    have_read_stdin: bool,
    /// dump_remainder's buffer.
    buf: Vec<u8>,
}

impl Tail {
    /// gnulib `error (0, ...)`: flushes standard output, then prints
    /// `tail: MSG`.
    pub(super) fn diag(&mut self, msg: impl std::fmt::Display) {
        self.out.flush_quietly();
        eprintln!("tail: {msg}");
    }

    /// `error (0, errnum, ...)`: `tail: MSG: STRERROR`, or `tail: MSG` when
    /// `errnum` is 0.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) fn diag_errno(&mut self, errnum: i32, msg: impl std::fmt::Display) {
        if errnum > 0 {
            let e = io::Error::from_raw_os_error(errnum);
            self.diag(format_args!("{msg}: {}", gnu_io_error(&e)));
        } else {
            self.diag(msg);
        }
    }

    pub(super) fn diag_io(&mut self, e: &io::Error, msg: impl std::fmt::Display) {
        self.diag(format_args!("{msg}: {}", gnu_io_error(e)));
    }

    /// `error (EXIT_FAILURE, ...)`.
    pub(super) fn fatal_io(&mut self, e: &io::Error, msg: impl std::fmt::Display) -> Fatal {
        self.diag_io(e, msg);
        Fatal(1)
    }

    pub(super) fn write_header(&mut self, pretty_name: &OsStr) -> R<()> {
        let mut h = Vec::with_capacity(pretty_name.len() + 10);
        if !self.first_file {
            h.push(b'\n');
        }
        h.extend_from_slice(b"==> ");
        h.extend_from_slice(pretty_name.as_bytes());
        h.extend_from_slice(b" <==\n");
        self.first_file = false;
        self.out.write(&h)
    }

    /// GNU's `xlseek`: a failure is fatal.
    pub(super) fn xlseek(&mut self, d: &Desc, pos: SeekFrom, name: &OsStr) -> R<u64> {
        match d.lseek(pos) {
            Ok(p) => Ok(p),
            Err(e) => {
                let msg = match pos {
                    SeekFrom::Start(o) => format!("{}: cannot seek to offset {o}", qf(name)),
                    SeekFrom::Current(o) => {
                        format!("{}: cannot seek to relative offset {o}", qf(name))
                    }
                    SeekFrom::End(o) => {
                        format!("{}: cannot seek to end-relative offset {o}", qf(name))
                    }
                };
                Err(self.fatal_io(&e, msg))
            }
        }
    }

    /// Reads and writes `n_bytes` (or [`COPY_TO_EOF`], or at most one buffer
    /// for [`COPY_A_BUFFER`]) from the current position; returns the bytes
    /// read. A read error other than `EAGAIN` is fatal.
    pub(super) fn dump_remainder(
        &mut self,
        mut want_header: bool,
        pretty_name: &OsStr,
        d: &Desc,
        n_bytes: u64,
    ) -> R<u64> {
        let mut n_written = 0u64;
        let mut n_remaining = n_bytes;
        loop {
            let n = to_usize(n_remaining).min(BUFSIZ);
            let bytes_read = match d.read(&mut self.buf[..n]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    return Err(
                        self.fatal_io(&e, format_args!("error reading {}", qa(pretty_name)))
                    );
                }
            };
            if want_header {
                self.write_header(pretty_name)?;
                want_header = false;
            }
            self.out.write(&self.buf[..bytes_read])?;
            n_written += bytes_read as u64;
            if n_bytes != COPY_TO_EOF {
                n_remaining -= bytes_read as u64;
                if n_remaining == 0 || n_bytes == COPY_A_BUFFER {
                    break;
                }
            }
        }
        Ok(n_written)
    }

    /// Prints the last `n_lines` lines of the seekable file between
    /// `start_pos` and `end_pos`, reading backwards from the end.
    fn file_lines(
        &mut self,
        name: &OsStr,
        d: &Desc,
        mut n_lines: u64,
        start_pos: u64,
        end_pos: u64,
        read_pos: &mut u64,
    ) -> R<bool> {
        if n_lines == 0 {
            return Ok(true);
        }
        let eol = self.a.line_end;
        let block = BUFSIZ as u64;
        let mut buf = vec![0u8; BUFSIZ];
        let mut pos = end_pos;
        let mut bytes_read = match (pos - start_pos) % block {
            0 => BUFSIZ,
            n => n as usize,
        };
        pos -= bytes_read as u64;
        self.xlseek(d, SeekFrom::Start(pos), name)?;
        bytes_read = match d.read(&mut buf[..bytes_read]) {
            Ok(n) => n,
            Err(e) => {
                self.diag_io(&e, format_args!("error reading {}", qa(name)));
                return Ok(false);
            }
        };
        *read_pos = pos + bytes_read as u64;
        // Count the incomplete line on files that don't end with a newline.
        if bytes_read > 0 && buf[bytes_read - 1] != eol {
            n_lines -= 1;
        }
        loop {
            let mut n = bytes_read;
            while n > 0 {
                let Some(nl) = memrchr(eol, &buf[..n]) else {
                    break;
                };
                n = nl;
                if n_lines == 0 {
                    self.out.write(&buf[nl + 1..bytes_read])?;
                    let rest = end_pos - (pos + bytes_read as u64);
                    *read_pos += self.dump_remainder(false, name, d, rest)?;
                    return Ok(true);
                }
                n_lines -= 1;
            }
            if pos == start_pos {
                // Not enough lines: print everything from start_pos.
                self.xlseek(d, SeekFrom::Start(start_pos), name)?;
                *read_pos = start_pos + self.dump_remainder(false, name, d, end_pos)?;
                return Ok(true);
            }
            pos -= block;
            self.xlseek(d, SeekFrom::Start(pos), name)?;
            bytes_read = match d.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    self.diag_io(&e, format_args!("error reading {}", qa(name)));
                    return Ok(false);
                }
            };
            *read_pos = pos + bytes_read as u64;
            if bytes_read == 0 {
                return Ok(true);
            }
        }
    }

    /// Prints the last `n_lines` lines of a stream, keeping only the
    /// buffers that can hold them.
    fn pipe_lines(&mut self, name: &OsStr, d: &Desc, n_lines: u64, read_pos: &mut u64) -> R<bool> {
        let eol = self.a.line_end;
        // (bytes, newlines in them)
        let mut bufs: std::collections::VecDeque<(Vec<u8>, u64)> = Default::default();
        bufs.push_back((Vec::with_capacity(BUFSIZ), 0));
        let mut total_lines = 0u64;
        let mut tmp = vec![0u8; BUFSIZ];
        let err = loop {
            let n_read = match d.read(&mut tmp) {
                Ok(0) => break None,
                Ok(n) => n,
                Err(e) => break Some(e),
            };
            *read_pos += n_read as u64;
            let nlines = memchr_iter(eol, &tmp[..n_read]).count() as u64;
            total_lines += nlines;
            let last = bufs.back_mut().expect("never empty");
            if n_read + last.0.len() < BUFSIZ {
                last.0.extend_from_slice(&tmp[..n_read]);
                last.1 += nlines;
            } else {
                bufs.push_back((tmp[..n_read].to_vec(), nlines));
                let first_lines = bufs.front().map_or(0, |b| b.1);
                if total_lines - first_lines > n_lines {
                    total_lines -= first_lines;
                    bufs.pop_front();
                }
            }
        };
        if let Some(e) = err {
            self.diag_io(&e, format_args!("error reading {}", qa(name)));
            return Ok(false);
        }
        let last = bufs.back_mut().expect("never empty");
        if last.0.is_empty() || n_lines == 0 {
            return Ok(true);
        }
        // Count the incomplete line on files that don't end with a newline.
        if last.0.last() != Some(&eol) {
            last.1 += 1;
            total_lines += 1;
        }
        let mut idx = 0;
        while total_lines - bufs[idx].1 > n_lines {
            total_lines -= bufs[idx].1;
            idx += 1;
        }
        let first = &bufs[idx].0;
        let mut beg = 0;
        if total_lines > n_lines {
            let skip = to_usize(total_lines - n_lines);
            if let Some(p) = memchr_iter(eol, first).nth(skip - 1) {
                beg = p + 1;
            }
        }
        let bufs = Vec::from(bufs);
        self.out.write(&bufs[idx].0[beg..])?;
        for (b, _) in &bufs[idx + 1..] {
            self.out.write(b)?;
        }
        Ok(true)
    }

    /// Prints the last `n_bytes` bytes of a stream.
    fn pipe_bytes(&mut self, name: &OsStr, d: &Desc, n_bytes: u64, read_pos: &mut u64) -> R<bool> {
        let mut bufs: std::collections::VecDeque<Vec<u8>> = Default::default();
        bufs.push_back(Vec::with_capacity(BUFSIZ));
        let mut total_bytes = 0u64;
        let mut tmp = vec![0u8; BUFSIZ];
        let err = loop {
            let n_read = match d.read(&mut tmp) {
                Ok(0) => break None,
                Ok(n) => n,
                Err(e) => break Some(e),
            };
            *read_pos += n_read as u64;
            total_bytes += n_read as u64;
            let last = bufs.back_mut().expect("never empty");
            if n_read + last.len() < BUFSIZ {
                last.extend_from_slice(&tmp[..n_read]);
            } else {
                bufs.push_back(tmp[..n_read].to_vec());
                let first_len = bufs.front().map_or(0, |b| b.len() as u64);
                if total_bytes - first_len > n_bytes {
                    total_bytes -= first_len;
                    bufs.pop_front();
                }
            }
        };
        if let Some(e) = err {
            self.diag_io(&e, format_args!("error reading {}", qa(name)));
            return Ok(false);
        }
        let mut idx = 0;
        while total_bytes - bufs[idx].len() as u64 > n_bytes {
            total_bytes -= bufs[idx].len() as u64;
            idx += 1;
        }
        let i = to_usize(total_bytes.saturating_sub(n_bytes));
        let bufs = Vec::from(bufs);
        self.out.write(&bufs[idx][i..])?;
        for b in &bufs[idx + 1..] {
            self.out.write(b)?;
        }
        Ok(true)
    }

    /// Skips `n_bytes` bytes of a stream and prints what was read past
    /// them: 1 on error, 0 if ok, -1 at end of file.
    fn start_bytes(
        &mut self,
        name: &OsStr,
        d: &Desc,
        mut n_bytes: u64,
        read_pos: &mut u64,
    ) -> R<i32> {
        let mut buf = vec![0u8; BUFSIZ];
        while n_bytes > 0 {
            let bytes_read = match d.read(&mut buf) {
                Ok(0) => return Ok(-1),
                Ok(n) => n,
                Err(e) => {
                    self.diag_io(&e, format_args!("error reading {}", qa(name)));
                    return Ok(1);
                }
            };
            *read_pos += bytes_read as u64;
            if bytes_read as u64 <= n_bytes {
                n_bytes -= bytes_read as u64;
            } else {
                self.out.write(&buf[to_usize(n_bytes)..bytes_read])?;
                break;
            }
        }
        Ok(0)
    }

    /// Skips `n_lines` lines of a file or stream and prints what was read
    /// past them: 1 on error, 0 if ok, -1 at end of file.
    fn start_lines(
        &mut self,
        name: &OsStr,
        d: &Desc,
        mut n_lines: u64,
        read_pos: &mut u64,
    ) -> R<i32> {
        if n_lines == 0 {
            return Ok(0);
        }
        let eol = self.a.line_end;
        let mut buf = vec![0u8; BUFSIZ];
        loop {
            let bytes_read = match d.read(&mut buf) {
                Ok(0) => return Ok(-1),
                Ok(n) => n,
                Err(e) => {
                    self.diag_io(&e, format_args!("error reading {}", qa(name)));
                    return Ok(1);
                }
            };
            *read_pos += bytes_read as u64;
            for p in memchr_iter(eol, &buf[..bytes_read]) {
                n_lines -= 1;
                if n_lines == 0 {
                    if p + 1 < bytes_read {
                        self.out.write(&buf[p + 1..bytes_read])?;
                    }
                    return Ok(0);
                }
            }
        }
    }

    fn tail_bytes(
        &mut self,
        name: &OsStr,
        d: &Desc,
        mut n_bytes: u64,
        read_pos: &mut u64,
    ) -> R<bool> {
        let st = match d.fstat() {
            Ok(st) => st,
            Err(e) => {
                self.diag_io(&e, format_args!("cannot fstat {}", qa(name)));
                return Ok(false);
            }
        };
        let off_t_max = i64::MAX as u64;
        if self.a.from_start {
            let skipped = !self.a.presume_input_pipe
                && n_bytes <= off_t_max
                && if st.file_type().is_file() {
                    self.xlseek(d, SeekFrom::Current(n_bytes as i64), name)?;
                    true
                } else {
                    d.lseek(SeekFrom::Current(n_bytes as i64)).is_ok()
                };
            if skipped {
                *read_pos += n_bytes;
            } else {
                let t = self.start_bytes(name, d, n_bytes, read_pos)?;
                if t != 0 {
                    return Ok(t < 0);
                }
            }
            n_bytes = COPY_TO_EOF;
        } else {
            let mut end_pos: i64 = -1;
            let mut current_pos: i64 = -1;
            if !self.a.presume_input_pipe && n_bytes <= off_t_max {
                if st.file_type().is_file() {
                    end_pos = st.size() as i64;
                } else if let Ok(p) = d.lseek(SeekFrom::End(-(n_bytes as i64))) {
                    current_pos = p as i64;
                    end_pos = current_pos.saturating_add(n_bytes as i64);
                }
            }
            if end_pos <= st_blksize(&st) as i64 {
                return self.pipe_bytes(name, d, n_bytes, read_pos);
            }
            if current_pos == -1 {
                current_pos = self.xlseek(d, SeekFrom::Current(0), name)? as i64;
            }
            if current_pos < end_pos {
                let bytes_remaining = (end_pos - current_pos) as u64;
                if n_bytes < bytes_remaining {
                    current_pos = end_pos - n_bytes as i64;
                    self.xlseek(d, SeekFrom::Start(current_pos as u64), name)?;
                }
            }
            *read_pos = current_pos as u64;
        }
        *read_pos += self.dump_remainder(false, name, d, n_bytes)?;
        Ok(true)
    }

    fn tail_lines(&mut self, name: &OsStr, d: &Desc, n_lines: u64, read_pos: &mut u64) -> R<bool> {
        let st = match d.fstat() {
            Ok(st) => st,
            Err(e) => {
                self.diag_io(&e, format_args!("cannot fstat {}", qa(name)));
                return Ok(false);
            }
        };
        if self.a.from_start {
            let t = self.start_lines(name, d, n_lines, read_pos)?;
            if t != 0 {
                return Ok(t < 0);
            }
            *read_pos += self.dump_remainder(false, name, d, COPY_TO_EOF)?;
            return Ok(true);
        }
        // Use file_lines only for a regular file where lseek works.
        let mut start_pos = None;
        if !self.a.presume_input_pipe && st.file_type().is_file() {
            start_pos = d.lseek(SeekFrom::Current(0)).ok();
            if let Some(start) = start_pos
                && let Ok(end) = d.lseek(SeekFrom::End(0))
                && start < end
            {
                *read_pos = end;
                if !self.file_lines(name, d, n_lines, start, end, read_pos)? {
                    return Ok(false);
                }
                return Ok(true);
            }
        }
        if let Some(start) = start_pos {
            self.xlseek(d, SeekFrom::Start(start), name)?;
        }
        self.pipe_lines(name, d, n_lines, read_pos)
    }

    fn tail(&mut self, name: &OsStr, d: &Desc, n_units: u64, read_pos: &mut u64) -> R<bool> {
        *read_pos = 0;
        if self.a.count_lines {
            self.tail_lines(name, d, n_units, read_pos)
        } else {
            self.tail_bytes(name, d, n_units, read_pos)
        }
    }

    /// Displays the last `n_units` units of `f`; with `-f`, records what
    /// following it needs.
    fn tail_file(&mut self, f: &mut FileSpec, n_units: u64) -> R<bool> {
        let is_stdin = f.is_stdin();
        if is_stdin {
            self.have_read_stdin = true;
        }
        let opened = Desc::open(&f.name, is_stdin, false);
        f.tailable = !(self.a.reopen_inaccessible_files && opened.is_err());
        let d = match opened {
            Ok(d) => d,
            Err(e) => {
                if self.a.forever {
                    f.fd = None;
                    f.errnum = e.raw_os_error().unwrap_or(libc::EIO);
                    f.ignore = !self.a.reopen_inaccessible_files;
                    f.ino = 0;
                    f.dev = 0;
                }
                let pretty = f.pretty_name();
                self.diag_io(&e, format_args!("cannot open {} for reading", qa(&pretty)));
                return Ok(false);
            }
        };
        let pretty = f.pretty_name();
        if self.print_headers {
            self.write_header(&pretty)?;
        }
        let mut read_pos = 0;
        let mut ok = self.tail(&pretty, &d, n_units, &mut read_pos)?;
        if !self.a.forever {
            return Ok(ok);
        }
        f.errnum = i32::from(ok) - 1;
        let mut stats = None;
        match d.fstat() {
            Err(e) => {
                ok = false;
                f.errnum = e.raw_os_error().unwrap_or(libc::EIO);
                self.diag_io(&e, format_args!("error reading {}", qa(&pretty)));
            }
            Ok(st) if !is_tailable_file_type(st.mode()) => {
                ok = false;
                f.errnum = -1;
                f.tailable = false;
                f.ignore = !self.a.reopen_inaccessible_files;
                let giving_up = if f.ignore {
                    "; giving up on this name"
                } else {
                    ""
                };
                self.diag(format_args!(
                    "{}: cannot follow end of this type of file{giving_up}",
                    qf(&pretty)
                ));
            }
            Ok(st) => stats = Some(st),
        }
        match stats {
            Some(st) if ok => {
                // read_pos, not st_size, avoids a race with a writer.
                f.record_open_fd(d, read_pos, &st, if is_stdin { -1 } else { 1 });
                f.remote = follow::fremote(self, f.raw_fd(), &pretty);
            }
            _ => {
                f.ignore = !self.a.reopen_inaccessible_files;
                f.fd = None;
            }
        }
        Ok(ok)
    }
}

/// The NDJSON extension: on a terminal, the last objects of an NDJSON
/// stream on standard input are shown as a table (or `--json`).
fn tail_ndjson(a: &TailArgs, ctx: &DogfishContext) -> Result<i32> {
    let mut all = Vec::new();
    io::stdin().lock().read_to_end(&mut all)?;
    let count = a.n_units;
    if looks_like_json(&all)
        && let Some(objs) = read_ndjson_stdin(&lossy_lines(&all))
    {
        ctx.telemetry
            .set_strategy("NDJSON object stream + auto-table");
        let start = objs.len().saturating_sub(to_usize(count));
        let slice = &objs[start..];
        ctx.telemetry.add_items(slice.len() as u64);
        emit_objects_or_table(slice, ctx, a.json, !a.json)?;
        return Ok(0);
    }
    let start = last_lines_start(&all, count, a.line_end);
    let mut out = RawStdout;
    ignore_broken_pipe(io::Write::write_all(&mut out, &all[start..]))?;
    Ok(0)
}

pub fn run_tail(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let mut a = match parse_tail_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    // To start printing with item N_UNITS from the start, skip N_UNITS - 1
    // items; `-n +0` is treated as `-n +1`.
    if a.from_start && a.n_units > 0 {
        a.n_units -= 1;
    }
    if a.files.is_empty() {
        a.files.push(OsString::from("-"));
    }
    let found_hyphen = a.files.iter().any(|f| f == "-");
    // When following by name, there must be a name.
    if found_hyphen && a.follow_mode == FollowMode::Name {
        eprintln!("tail: cannot follow {} by name", qa("-"));
        return Ok(1);
    }
    // Warn when following a terminal on standard input, as the checks for
    // new input cannot work there.
    if a.forever && found_hyphen {
        let stdin = stdin_file();
        let blocking_stdin = a.pid == 0
            && a.follow_mode == FollowMode::Descriptor
            && a.files.len() == 1
            && !stdin_is_closed()
            && stdin.metadata().is_ok_and(|m| !m.file_type().is_file());
        if !blocking_stdin && std::io::IsTerminal::is_terminal(&*stdin) {
            eprintln!("tail: warning: following standard input indefinitely is ineffective");
        }
    }
    // Don't read anything if nothing will be output.
    if a.n_units == 0 && !a.forever && !a.from_start {
        return Ok(0);
    }

    if a.files.len() == 1
        && a.files[0] == "-"
        && a.count_lines
        && !a.from_start
        && !a.forever
        && a.header_mode != HeaderMode::Always
        && a.line_end == b'\n'
        && !ctx.stdin_is_tty()
        && !stdin_is_closed()
        && (ctx.stdout_is_tty() || a.json)
    {
        return tail_ndjson(&a, ctx);
    }

    ctx.telemetry.set_strategy("reverse block scan + follow");
    let n_files = a.files.len();
    let print_headers = a.header_mode == HeaderMode::Always
        || (a.header_mode == HeaderMode::MultipleFiles && n_files > 1);
    let mut files: Vec<FileSpec> = a.files.iter().cloned().map(FileSpec::new).collect();
    let n_units = a.n_units;
    let mut t = Tail {
        a,
        out: Out::new(),
        print_headers,
        first_file: true,
        monitor_output: false,
        have_read_stdin: false,
        buf: vec![0; BUFSIZ],
    };
    let code = match run(&mut t, &mut files, n_units) {
        Ok(ok) => i32::from(!ok),
        Err(Fatal(code)) => code,
    };
    t.out.flush_quietly();
    ctx.telemetry.add_written(t.out.written);
    Ok(code)
}

/// GNU tail's `main` after option parsing; returns whether all went well.
fn run(t: &mut Tail, files: &mut [FileSpec], n_units: u64) -> R<bool> {
    let mut ok = true;
    for f in files.iter_mut() {
        ok &= t.tail_file(f, n_units)?;
    }
    if t.a.forever && follow::ignore_fifo_and_pipe(files) > 0 {
        // If standard output is a pipe, exit when its reader goes away.
        if stdout::was_closed() {
            let e = io::Error::from_raw_os_error(libc::EBADF);
            return Err(t.fatal_io(&e, "standard output"));
        }
        let out_st = match io::stdout()
            .as_fd()
            .try_clone_to_owned()
            .map(File::from)
            .and_then(|f| f.metadata())
        {
            Ok(st) => st,
            Err(e) => return Err(t.fatal_io(&e, "standard output")),
        };
        t.monitor_output = is_fifo(out_st.mode());

        #[cfg(target_os = "linux")]
        {
            if !t.a.disable_inotify && !inotify::usable(t, files, ok) {
                t.a.disable_inotify = true;
            }
            if !t.a.disable_inotify {
                let errnum = inotify::follow(t, files)?;
                t.diag_errno(errnum, "inotify cannot be used, reverting to polling");
            }
        }
        t.a.disable_inotify = true;
        follow::tail_forever(t, files)?;
    }
    if t.have_read_stdin
        && let Err(e) = close_stdin()
    {
        return Err(t.fatal_io(&e, "-"));
    }
    Ok(ok)
}
