//! head — GNU `head.c`: `-n -N` / `-c -N`, `-z`, the obsolete `-Nc` syntax,
//! seekable input left just past what was printed, + NDJSON auto-table
//! rendering on terminals.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;

use anyhow::Result;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, InputSource, emit_objects_or_table, gnu_io_error,
    read_ndjson_stdin, stdout,
};
use memchr::{memchr_iter, memrchr};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, lossy_all, print_help, print_version, try_help,
};
use crate::common::head_tail::{header_name, parse_count, to_usize};
use crate::common::input::{RawStdout, close_stdin, full_read, safe_read, stdin_file};
use crate::common::lines::{line_end, looks_like_json, lossy_lines};
use crate::common::quote::{qa, qf, quote_locale};

const HEAD_HELP: &str = "\
Output the first part of files (or NDJSON object streams)

Usage: head [OPTIONS] [FILES]...

Arguments:
  [FILES]...  

Options:
  -n, --lines <[-]NUM>  Print the first NUM lines (default 10); with a leading '-', all but the last
                        NUM lines
  -c, --bytes <[-]NUM>  Print the first NUM bytes; with a leading '-', all but the last NUM bytes
  -q, --quiet           Never print headers giving file names [alias: --silent]
  -v, --verbose         Always print headers giving file names
  -z, --zero-terminated Line delimiter is NUL, not newline
  -J, --json            Force JSON output when processing NDJSON
      --help            Print help
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum HeadOpt {
    Bytes,
    Lines,
    PresumeInputPipe,
    Quiet,
    Verbose,
    ZeroTerminated,
    Json,
    /// A digit after other options (`-c 5 -3`): GNU's "invalid trailing option".
    Digit(u8),
    Help,
    Version,
}

const fn digit(c: u8) -> OptSpec<HeadOpt> {
    OptSpec::short(c, HasArg::No, HeadOpt::Digit(c))
}

/// GNU's `long_options` and `"c:n:qvz0123456789"`, plus the Dogfish
/// `-J, --json`. `---presume-input-pipe` is GNU's hidden test option.
const SPECS: &[OptSpec<HeadOpt>] = &[
    OptSpec::both(b'c', "bytes", HasArg::Required, HeadOpt::Bytes),
    OptSpec::both(b'n', "lines", HasArg::Required, HeadOpt::Lines),
    OptSpec::long("-presume-input-pipe", HasArg::No, HeadOpt::PresumeInputPipe),
    OptSpec::both(b'q', "quiet", HasArg::No, HeadOpt::Quiet),
    OptSpec::long("silent", HasArg::No, HeadOpt::Quiet),
    OptSpec::both(b'v', "verbose", HasArg::No, HeadOpt::Verbose),
    OptSpec::both(b'z', "zero-terminated", HasArg::No, HeadOpt::ZeroTerminated),
    OptSpec::long("help", HasArg::No, HeadOpt::Help),
    OptSpec::long("version", HasArg::No, HeadOpt::Version),
    OptSpec::both(b'J', "json", HasArg::No, HeadOpt::Json).exact(),
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

/// What `head` was asked for; as in GNU, the last `-c`/`-n` wins.
struct HeadArgs {
    count: u64,
    count_lines: bool,
    elide_from_end: bool,
    presume_input_pipe: bool,
    quiet: bool,
    verbose: bool,
    zero_terminated: bool,
    json: bool,
    files: Vec<String>,
}

/// Parses the command line like GNU's `main`; `Err` is the exit status
/// after a diagnostic, `--help` or `--version`.
fn parse_head_args(args: Vec<OsString>) -> std::result::Result<HeadArgs, i32> {
    let mut a = HeadArgs {
        count: 10,
        count_lines: true,
        elide_from_end: false,
        presume_input_pipe: false,
        quiet: false,
        verbose: false,
        zero_terminated: false,
        json: false,
        files: Vec::new(),
    };
    let mut opts = Getopt::new(expand_obsolete(args)?.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("head", &e))?;
        match o {
            HeadOpt::Bytes | HeadOpt::Lines => {
                // Like GNU, each count is checked as it is met.
                a.count_lines = o == HeadOpt::Lines;
                let spec = v.as_deref().map(lossy).unwrap_or_default();
                let (elide, num) = match spec.strip_prefix('-') {
                    Some(rest) => (true, rest),
                    None => (false, spec.as_str()),
                };
                a.elide_from_end = elide;
                a.count = parse_count(num, a.count_lines).map_err(|msg| {
                    eprintln!("head: {msg}");
                    1
                })?;
            }
            HeadOpt::PresumeInputPipe => a.presume_input_pipe = true,
            HeadOpt::Quiet => (a.quiet, a.verbose) = (true, false),
            HeadOpt::Verbose => (a.quiet, a.verbose) = (false, true),
            HeadOpt::ZeroTerminated => a.zero_terminated = true,
            HeadOpt::Json => a.json = true,
            HeadOpt::Digit(c) => {
                eprintln!("head: invalid trailing option -- {}", char::from(c));
                return Err(try_help("head"));
            }
            HeadOpt::Help => return Err(print_help(HEAD_HELP)),
            HeadOpt::Version => return Err(print_version("head")),
        }
    }
    a.files = lossy_all(opts.operands());
    Ok(a)
}

/// GNU's `BUFSIZ`: head reads in units of this, so what it consumes from a
/// pipe matches GNU's.
const BUFSIZ: usize = 8192;

/// What to print from each input.
#[derive(Clone, Copy)]
struct HeadMode {
    count: u64,
    count_lines: bool,
    /// `-n -N` / `-c -N`: all but the last `count` units.
    elide_from_end: bool,
    eol: u8,
}

/// Rewrites GNU's obsolete first argument (`-5`, `-1c`, `-2b`, `-3lqv`,
/// `-1z`) as the equivalent options; `Err` is the exit status after GNU's
/// `invalid trailing option` diagnostic.
fn expand_obsolete(mut args: Vec<OsString>) -> Result<Vec<OsString>, i32> {
    use std::os::unix::ffi::OsStrExt;
    let Some(arg) = args.get(1) else {
        return Ok(args);
    };
    let bytes = arg.as_bytes().to_vec();
    if bytes.len() < 2 || bytes[0] != b'-' || !bytes[1].is_ascii_digit() {
        return Ok(args);
    }
    let digits_end = 1 + bytes[1..].iter().take_while(|b| b.is_ascii_digit()).count();
    let mut count_lines = true;
    let mut multiplier = None;
    let mut extra = Vec::new();
    for &c in &bytes[digits_end..] {
        match c {
            b'c' => {
                count_lines = false;
                multiplier = None;
            }
            b'b' | b'k' | b'm' => {
                count_lines = false;
                multiplier = Some(c as char);
            }
            b'l' => count_lines = true,
            b'q' => extra.push(OsString::from("-q")),
            b'v' => extra.push(OsString::from("-v")),
            b'z' => extra.push(OsString::from("-z")),
            _ => {
                eprintln!("head: invalid trailing option -- {}", c as char);
                return Err(try_help("head"));
            }
        }
    }
    let mut count = String::from_utf8_lossy(&bytes[1..digits_end]).into_owned();
    count.extend(multiplier);
    let option = if count_lines { "-n" } else { "-c" };
    let mut replacement = vec![OsString::from(option), OsString::from(count)];
    replacement.extend(extra);
    args.splice(1..2, replacement);
    Ok(args)
}

pub fn run_head(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_head_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let (count, count_lines, elide_from_end) =
        (parsed.count, parsed.count_lines, parsed.elide_from_end);
    if !count_lines && elide_from_end && count > i64::MAX as u64 {
        eprintln!(
            "head: invalid number of bytes: {}: Value too large for defined data type",
            quote_locale(count.to_string())
        );
        return Ok(1);
    }
    let mode = HeadMode {
        count,
        count_lines,
        elide_from_end,
        eol: line_end(parsed.zero_terminated),
    };
    let sources = InputSource::from_args(&parsed.files);

    // Rich output: NDJSON on standard input becomes a table on a terminal
    // (or JSON with -J). Piped output is GNU's, byte for byte.
    if sources.len() == 1
        && matches!(sources[0], InputSource::Stdin)
        && count_lines
        && !elide_from_end
        && !parsed.verbose
        && !parsed.zero_terminated
        && !ctx.stdin_is_tty()
        && (ctx.stdout_is_tty() || parsed.json)
    {
        return head_stdin_adaptive(count, parsed.json, ctx);
    }

    ctx.telemetry
        .set_strategy("GNU head: BUFSIZ reads, seek back");
    let show_headers = parsed.verbose || (!parsed.quiet && sources.len() > 1);
    // Each write is one of GNU's `fwrite` calls (`xwrite_stdout`): one that
    // has to write and fails is `error writing 'standard output'`, while
    // output still buffered fails at exit with `write error`.
    let mut out = stdout::GnuFwrite::new(io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, RawStdout));
    let mut first_header = true;
    let mut ok = true;
    let mut read_stdin = false;

    for src in &sources {
        let mut stdin;
        let mut file;
        let input: &mut File = match src {
            InputSource::Stdin => {
                read_stdin = true;
                stdin = stdin_file();
                &mut stdin
            }
            InputSource::File(path) => match File::open(path) {
                Ok(f) => {
                    // With standard output closed, GNU opens it on
                    // descriptor 1 (and closes it after).
                    if stdout::was_closed() {
                        let blksize = f.metadata().map_or(0, |m| m.blksize());
                        stdout::gnu_input_on_fd1(blksize, false);
                    }
                    file = f;
                    &mut file
                }
                Err(e) => {
                    let _ = out.flush();
                    eprintln!(
                        "head: cannot open {} for reading: {}",
                        qa(path),
                        gnu_io_error(&e)
                    );
                    ok = false;
                    continue;
                }
            },
        };
        if show_headers {
            let sep = if first_header { "" } else { "\n" };
            first_header = false;
            writeln!(out, "{sep}==> {} <==", header_name(src))?;
        }
        let mut head = Head {
            presume_input_pipe: parsed.presume_input_pipe,
            input,
            name: header_name(src),
            out: &mut out,
        };
        match head.run(mode) {
            Ok(file_ok) => ok &= file_ok,
            Err(e) if stdout::is_write_error(&e) => {
                stdout::mark_reported();
                eprintln!(
                    "head: error writing 'standard output': {}",
                    gnu_io_error(&e)
                );
                return Ok(1);
            }
            Err(e) => return Err(e.into()),
        }
    }
    out.flush()?;
    if read_stdin && let Err(e) = close_stdin() {
        eprintln!("head: -: {}", gnu_io_error(&e));
        return Ok(1);
    }
    Ok(i32::from(!ok))
}

/// One input being copied to standard output. Read problems are diagnosed
/// here (and make the methods return `Ok(false)`); `Err` is a write error.
struct Head<'a, W: Write> {
    /// GNU's hidden `---presume-input-pipe`: never seek.
    presume_input_pipe: bool,
    input: &'a mut File,
    /// The input's name in messages (`standard input` for `-`).
    name: &'a str,
    out: &'a mut W,
}

impl<W: Write> Head<'_, W> {
    fn run(&mut self, mode: HeadMode) -> io::Result<bool> {
        if !mode.elide_from_end {
            return if mode.count_lines {
                self.head_lines(mode.count, mode.eol)
            } else {
                self.head_bytes(mode.count)
            };
        }
        let meta = match self.input.metadata() {
            Ok(m) => m,
            Err(e) => {
                self.report(&format!("cannot fstat {}", qa(self.name)), &e)?;
                return Ok(false);
            }
        };
        // Like GNU, only regular files have a usable size and offset.
        let mut current_pos = None;
        if meta.is_file() && !self.presume_input_pipe {
            match self.input.stream_position() {
                Ok(p) => current_pos = Some(p),
                Err(e) => {
                    self.seek_failed(0, SeekFrom::Current(0), &e)?;
                    return Ok(false);
                }
            }
        }
        let seekable = current_pos.filter(|_| meta.len() > meta.blksize());
        match (seekable, mode.count_lines) {
            (Some(pos), true) => {
                Ok(meta.len() <= pos || self.elide_tail_lines_seekable(mode, pos, meta.len())?)
            }
            (Some(pos), false) => {
                let remaining = meta.len().saturating_sub(pos);
                if remaining <= mode.count {
                    return Ok(true);
                }
                self.copy_exact(remaining - mode.count)
            }
            (None, true) => self.elide_tail_lines_pipe(mode, current_pos),
            (None, false) => self.elide_tail_bytes_pipe(mode.count, current_pos),
        }
    }

    /// Prints `head: PREFIX: ERROR` after the output so far.
    fn report(&mut self, prefix: &str, e: &io::Error) -> io::Result<()> {
        self.out.flush()?;
        eprintln!("head: {prefix}: {}", gnu_io_error(e));
        Ok(())
    }

    fn read_failed(&mut self, e: &io::Error) -> io::Result<bool> {
        self.report(&format!("error reading {}", qa(self.name)), e)?;
        Ok(false)
    }

    /// GNU's `elseek` diagnostic.
    fn seek_failed(&mut self, offset: i64, whence: SeekFrom, e: &io::Error) -> io::Result<()> {
        let what = match whence {
            SeekFrom::Start(_) => "cannot seek to offset",
            _ => "cannot seek to relative offset",
        };
        self.report(&format!("{}: {what} {offset}", qf(self.name)), e)
    }

    fn seek(&mut self, to: SeekFrom) -> io::Result<bool> {
        match self.input.seek(to) {
            Ok(_) => Ok(true),
            Err(e) => {
                let offset = match to {
                    SeekFrom::Start(p) => p as i64,
                    SeekFrom::Current(d) | SeekFrom::End(d) => d,
                };
                self.seek_failed(offset, to, &e)?;
                Ok(false)
            }
        }
    }

    /// The first `n` bytes, never reading past them.
    fn head_bytes(&mut self, mut n: u64) -> io::Result<bool> {
        let mut buf = [0u8; BUFSIZ];
        while n > 0 {
            let want = to_usize(n).min(BUFSIZ);
            match safe_read(self.input, &mut buf[..want]) {
                Ok(0) => break,
                Ok(k) => {
                    self.out.write_all(&buf[..k])?;
                    n -= k as u64;
                }
                Err(e) => return self.read_failed(&e),
            }
        }
        Ok(true)
    }

    /// The first `n` lines; a seekable input is left just after them.
    fn head_lines(&mut self, mut n: u64, eol: u8) -> io::Result<bool> {
        let mut buf = [0u8; BUFSIZ];
        while n > 0 {
            let read = match safe_read(self.input, &mut buf) {
                Ok(0) => break,
                Ok(k) => k,
                Err(e) => return self.read_failed(&e),
            };
            let mut end = read;
            for pos in memchr_iter(eol, &buf[..read]) {
                n -= 1;
                if n == 0 {
                    end = pos + 1;
                    break;
                }
            }
            self.out.write_all(&buf[..end])?;
            if n == 0 && end < read {
                // Put back what was read past the last line, as if reading
                // one byte at a time; a pipe cannot, a regular file must.
                let back = -((read - end) as i64);
                if self.input.seek(SeekFrom::Current(back)).is_err() {
                    let regular = self.input.metadata().map(|m| m.is_file());
                    if regular.unwrap_or(true) {
                        self.seek(SeekFrom::Current(back))?;
                    }
                }
            }
        }
        Ok(true)
    }

    /// Copies exactly `n` bytes (a regular file whose size is known).
    fn copy_exact(&mut self, mut n: u64) -> io::Result<bool> {
        let mut buf = [0u8; BUFSIZ];
        while n > 0 {
            let want = to_usize(n).min(BUFSIZ);
            match safe_read(self.input, &mut buf[..want]) {
                Ok(0) => {
                    self.out.flush()?;
                    eprintln!("head: {}: file has shrunk too much", qf(self.name));
                    return Ok(false);
                }
                Ok(k) => {
                    self.out.write_all(&buf[..k])?;
                    n -= k as u64;
                }
                Err(e) => return self.read_failed(&e),
            }
        }
        Ok(true)
    }

    /// All but the last `n` bytes of an input of unknown size, holding back
    /// only as much as has been read (so a huge `n` on a small input is
    /// fine). A regular file is then repositioned just after the output.
    fn elide_tail_bytes_pipe(&mut self, n: u64, current_pos: Option<u64>) -> io::Result<bool> {
        if n > (usize::MAX - BUFSIZ) as u64 {
            self.out.flush()?;
            eprintln!("head: {n}: number of bytes is too large");
            std::process::exit(1);
        }
        let n = to_usize(n);
        let mut held: VecDeque<u8> = VecDeque::new();
        let mut written = 0u64;
        let mut buf = vec![0u8; BUFSIZ];
        let mut ok = true;
        loop {
            let (k, err) = full_read(self.input, &mut buf);
            held.extend(&buf[..k]);
            if held.len() > n {
                let excess = held.len() - n;
                let (a, b) = held.as_slices();
                let from_a = excess.min(a.len());
                self.out.write_all(&a[..from_a])?;
                self.out.write_all(&b[..excess - from_a])?;
                held.drain(..excess);
                written += excess as u64;
            }
            if let Some(e) = err {
                ok = self.read_failed(&e)?;
                break;
            }
            if k < buf.len() {
                break;
            }
        }
        if let Some(pos) = current_pos
            && !self.seek(SeekFrom::Start(pos + written))?
        {
            ok = false;
        }
        Ok(ok)
    }

    /// All but the last `n` lines of an input of unknown size (GNU's
    /// `elide_tail_lines_pipe`): buffers are released as soon as enough
    /// lines follow them.
    fn elide_tail_lines_pipe(
        &mut self,
        mode: HeadMode,
        current_pos: Option<u64>,
    ) -> io::Result<bool> {
        let (n, eol) = (mode.count, mode.eol);
        // (bytes, number of terminators in them)
        let mut bufs: VecDeque<(Vec<u8>, u64)> = VecDeque::new();
        let mut total_lines = 0u64;
        let mut written = 0u64;
        let mut chunk = [0u8; BUFSIZ];
        let mut ok = true;
        loop {
            let k = match safe_read(self.input, &mut chunk) {
                Ok(0) => break,
                Ok(k) => k,
                Err(e) => {
                    ok = self.read_failed(&e)?;
                    break;
                }
            };
            let data = &chunk[..k];
            if n == 0 {
                written += k as u64;
                self.out.write_all(data)?;
                continue;
            }
            let lines = memchr_iter(eol, data).count() as u64;
            total_lines += lines;
            match bufs.back_mut() {
                // Small pipe reads are appended to the last buffer.
                Some((last, last_lines)) if last.len() + k < BUFSIZ => {
                    last.extend_from_slice(data);
                    *last_lines += lines;
                }
                _ => {
                    bufs.push_back((data.to_vec(), lines));
                    if let Some(&(_, first_lines)) = bufs.front()
                        && n < total_lines - first_lines
                    {
                        let (first, first_lines) = bufs.pop_front().unwrap_or_default();
                        written += first.len() as u64;
                        self.out.write_all(&first)?;
                        total_lines -= first_lines;
                    }
                }
            }
        }
        if ok {
            // Count an unterminated last line.
            if let Some((last, last_lines)) = bufs.back_mut()
                && last.last().is_some_and(|&b| b != eol)
            {
                *last_lines += 1;
                total_lines += 1;
            }
            while let Some((buf, lines)) = bufs.pop_front() {
                if n < total_lines - lines {
                    written += buf.len() as u64;
                    self.out.write_all(&buf)?;
                    total_lines -= lines;
                    continue;
                }
                if n < total_lines {
                    let keep = total_lines - n;
                    let end = memchr_iter(eol, &buf)
                        .nth(to_usize(keep - 1))
                        .map_or(buf.len(), |p| p + 1);
                    written += end as u64;
                    self.out.write_all(&buf[..end])?;
                }
                break;
            }
        }
        if let Some(pos) = current_pos
            && !self.seek(SeekFrom::Start(pos + written))?
        {
            ok = false;
        }
        Ok(ok)
    }

    /// All but the last `n` lines of a regular file positioned at
    /// `start_pos` with `size` bytes, scanning backwards from the end (GNU's
    /// `elide_tail_lines_seekable`). The file is left just after the output.
    fn elide_tail_lines_seekable(
        &mut self,
        mode: HeadMode,
        start_pos: u64,
        size: u64,
    ) -> io::Result<bool> {
        let eol = mode.eol;
        let mut n_lines = mode.count;
        let mut buf = [0u8; BUFSIZ];
        let mut bytes_read = to_usize((size - start_pos) % BUFSIZ as u64);
        if bytes_read == 0 {
            bytes_read = BUFSIZ;
        }
        let mut pos = size - bytes_read as u64;
        if !self.seek(SeekFrom::Start(pos))? {
            return Ok(false);
        }
        bytes_read = match safe_read(self.input, &mut buf[..bytes_read]) {
            Ok(k) => k,
            Err(e) => return self.read_failed(&e),
        };
        let all_lines = n_lines == 0;
        // Count an unterminated last line.
        if n_lines > 0 && bytes_read > 0 && buf[bytes_read - 1] != eol {
            n_lines -= 1;
        }
        loop {
            let mut n = bytes_read;
            while n > 0 {
                if all_lines {
                    n -= 1;
                } else {
                    match memrchr(eol, &buf[..n]) {
                        Some(p) => n = p,
                        None => break,
                    }
                }
                if n_lines == 0 {
                    // Found it: copy from the start to here.
                    if start_pos < pos {
                        if !self.seek(SeekFrom::Start(start_pos))? {
                            return Ok(false);
                        }
                        if !self.copy_exact(pos - start_pos)? {
                            return Ok(false);
                        }
                    }
                    self.out.write_all(&buf[..n + 1])?;
                    return self.seek(SeekFrom::Start(pos + n as u64 + 1));
                }
                n_lines -= 1;
            }
            if pos == start_pos {
                // Not enough lines in the file.
                return Ok(true);
            }
            pos -= BUFSIZ as u64;
            if !self.seek(SeekFrom::Start(pos))? {
                return Ok(false);
            }
            bytes_read = match safe_read(self.input, &mut buf) {
                Ok(0) => return Ok(true),
                Ok(k) => k,
                Err(e) => return self.read_failed(&e),
            };
        }
    }
}

fn head_stdin_adaptive(count: u64, json: bool, ctx: &DogfishContext) -> Result<i32> {
    let stdin = io::stdin();
    let mut reader = io::BufReader::with_capacity(DEFAULT_BUF_SIZE, stdin.lock());
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    // Peek the first line to see if it starts with '{' or '[' before reading all of stdin!
    let mut first_line = Vec::new();
    if reader.read_until(b'\n', &mut first_line)? == 0 {
        return Ok(0);
    }
    if looks_like_json(&first_line) {
        let mut all = first_line;
        reader.read_to_end(&mut all)?;
        if let Some(objs) = read_ndjson_stdin(&lossy_lines(&all)) {
            ctx.telemetry
                .set_strategy("NDJSON object stream + auto-table");
            let slice = &objs[..objs.len().min(to_usize(count))];
            ctx.telemetry.add_items(slice.len() as u64);
            drop(out);
            emit_objects_or_table(slice, ctx, json, !json)?;
            return Ok(0);
        }
        copy_first_lines(&mut &all[..], count, &mut out).map(|_| ())?;
        out.flush()?;
        return Ok(0);
    }

    // Plain text streaming fast-path: stop immediately after `count` lines!
    if count > 0 {
        out.write_all(&first_line)?;
        copy_first_lines(&mut reader, count - 1, &mut out).map(|_| ())?;
    }
    out.flush()?;
    Ok(0)
}

/// Copies the first `n` lines of `r` to `out`.
fn copy_first_lines<R: BufRead + ?Sized, W: Write>(
    r: &mut R,
    mut n: u64,
    out: &mut W,
) -> io::Result<u64> {
    let mut written = 0u64;
    while n > 0 {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let mut end = buf.len();
        for pos in memchr_iter(b'\n', buf) {
            n -= 1;
            if n == 0 {
                end = pos + 1;
                break;
            }
        }
        out.write_all(&buf[..end])?;
        written += end as u64;
        r.consume(end);
    }
    Ok(written)
}

/// head's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("head", HEAD_HELP, SPECS)
}

#[cfg(test)]
mod tests {
    use super::expand_obsolete;

    fn expand(args: &[&str]) -> Vec<String> {
        expand_obsolete(args.iter().map(std::ffi::OsString::from).collect())
            .unwrap()
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect()
    }

    #[test]
    fn obsolete_forms() {
        assert_eq!(expand(&["head", "-5"]), ["head", "-n", "5"]);
        assert_eq!(expand(&["head", "-1c", "f"]), ["head", "-c", "1", "f"]);
        assert_eq!(expand(&["head", "-2b"]), ["head", "-c", "2b"]);
        assert_eq!(expand(&["head", "-3lvz"]), ["head", "-n", "3", "-v", "-z"]);
        assert_eq!(expand(&["head", "-n", "3"]), ["head", "-n", "3"]);
        assert!(expand_obsolete(vec!["head".into(), "-1x".into()]).is_err());
    }
}

/// Entry points for the fuzz targets in `fuzz/` (only with the `fuzzing` feature).
#[cfg(feature = "fuzzing")]
#[allow(dead_code)]
pub mod fuzz {
    use super::*;

    pub fn args(args: Vec<OsString>) {
        if let Ok(args) = expand_obsolete(args) {
            let _ = parse_head_args(args);
        }
    }
}
