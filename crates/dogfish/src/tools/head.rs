//! head — GNU `head.c`: `-n -N` / `-c -N`, `-z`, the obsolete `-Nc` syntax,
//! seekable input left just past what was printed, + NDJSON auto-table
//! rendering on terminals.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufRead, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;

use anyhow::Result;
use clap::Parser;
use dogfish_core::{
    DEFAULT_BUF_SIZE, DogfishContext, InputSource, emit_objects_or_table, gnu_io_error,
    io::ignore_broken_pipe, read_ndjson_stdin,
};
use memchr::{memchr_iter, memrchr};

use crate::common::getopt::try_help;
use crate::common::head_tail::{digit_option, header_name, parse_count, to_usize};
use crate::common::input::{RawStdout, close_stdin, full_read, safe_read, stdin_file};
use crate::common::lines::{line_end, looks_like_json, lossy_lines};
use crate::common::quote::{qa, qf, quote_locale};

#[derive(Parser)]
#[command(
    name = "head",
    about = "Output the first part of files (or NDJSON object streams)"
)]
struct HeadArgs {
    /// Print the first NUM lines (default 10); with a leading '-', all but the last NUM lines
    #[arg(
        short = 'n',
        long = "lines",
        value_name = "[-]NUM",
        allow_hyphen_values = true,
        overrides_with = "bytes"
    )]
    lines: Option<String>,

    /// Print the first NUM bytes; with a leading '-', all but the last NUM bytes
    #[arg(
        short = 'c',
        long = "bytes",
        value_name = "[-]NUM",
        allow_hyphen_values = true,
        overrides_with = "lines"
    )]
    bytes: Option<String>,

    /// Never print headers giving file names
    #[arg(
        short = 'q',
        long = "quiet",
        visible_alias = "silent",
        overrides_with = "verbose"
    )]
    quiet: bool,

    /// Always print headers giving file names
    #[arg(short = 'v', long = "verbose", overrides_with = "quiet")]
    verbose: bool,

    /// Line delimiter is NUL, not newline
    #[arg(short = 'z', long = "zero-terminated")]
    zero_terminated: bool,

    /// Force JSON output when processing NDJSON
    #[arg(short = 'J', long = "json")]
    json: bool,

    files: Vec<String>,
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
fn expand_obsolete(mut args: Vec<String>) -> Result<Vec<String>, i32> {
    let Some(arg) = args.get(1) else {
        return Ok(args);
    };
    let bytes = arg.as_bytes();
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
            b'q' => extra.push("-q".to_string()),
            b'v' => extra.push("-v".to_string()),
            b'z' => extra.push("-z".to_string()),
            _ => {
                eprintln!("head: invalid trailing option -- {}", c as char);
                return Err(try_help("head", 1));
            }
        }
    }
    let mut count = arg[1..digits_end].to_string();
    count.extend(multiplier);
    let option = if count_lines { "-n" } else { "-c" };
    let mut replacement = vec![option.to_string(), count];
    replacement.extend(extra);
    args.splice(1..2, replacement);
    Ok(args)
}

pub fn run_head(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let args = match expand_obsolete(args) {
        Ok(args) => args,
        Err(status) => return Ok(status),
    };
    if let Some(c) = digit_option(&args, b"cn", &["bytes", "lines"]) {
        eprintln!("head: invalid trailing option -- {c}");
        return Ok(try_help("head", 1));
    }
    let parsed = HeadArgs::try_parse_from(args)?;
    let count_lines = parsed.bytes.is_none();
    let spec = parsed
        .bytes
        .as_deref()
        .or(parsed.lines.as_deref())
        .unwrap_or("10");
    let (elide_from_end, num_str) = match spec.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, spec),
    };
    let count = match parse_count(num_str, count_lines) {
        Ok(n) => n,
        Err(msg) => {
            eprintln!("head: {msg}");
            return Ok(1);
        }
    };
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
        && !ctx.stdin_is_tty
        && (ctx.stdout_is_tty || parsed.json)
    {
        return head_stdin_adaptive(count, parsed.json, ctx);
    }

    ctx.telemetry
        .set_strategy("GNU head: BUFSIZ reads, seek back");
    let show_headers = parsed.verbose || (!parsed.quiet && sources.len() > 1);
    let mut out = io::BufWriter::with_capacity(DEFAULT_BUF_SIZE, RawStdout);
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
            ignore_broken_pipe(writeln!(out, "{sep}==> {} <==", header_name(src)))?;
        }
        let mut head = Head {
            input,
            name: header_name(src),
            out: &mut out,
        };
        match head.run(mode) {
            Ok(file_ok) => ok &= file_ok,
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(i32::from(!ok)),
            Err(e) => return Err(e.into()),
        }
    }
    ignore_broken_pipe(out.flush())?;
    if read_stdin && let Err(e) = close_stdin() {
        eprintln!("head: -: {}", gnu_io_error(&e));
        return Ok(1);
    }
    Ok(i32::from(!ok))
}

/// One input being copied to standard output. Read problems are diagnosed
/// here (and make the methods return `Ok(false)`); `Err` is a write error.
struct Head<'a, W: Write> {
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
        if meta.is_file() {
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
        ignore_broken_pipe(copy_first_lines(&mut &all[..], count, &mut out).map(|_| ()))?;
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    // Plain text streaming fast-path: stop immediately after `count` lines!
    if count > 0 {
        ignore_broken_pipe(out.write_all(&first_line))?;
        ignore_broken_pipe(copy_first_lines(&mut reader, count - 1, &mut out).map(|_| ()))?;
    }
    ignore_broken_pipe(out.flush())?;
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

#[cfg(test)]
mod tests {
    use super::expand_obsolete;

    fn expand(args: &[&str]) -> Vec<String> {
        expand_obsolete(args.iter().map(|s| s.to_string()).collect()).unwrap()
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
