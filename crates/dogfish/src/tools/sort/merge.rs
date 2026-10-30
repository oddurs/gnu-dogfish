//! sort's files: reading inputs a line at a time, temporary files (through
//! `--compress-program`), the k-way merge of sorted inputs with GNU's
//! `--batch-size` and file-descriptor limits (`merge`, `mergefps`), and
//! `-c`/`-C` (`check`).

use std::cmp::Ordering;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, BufWriter, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use dogfish_core::gnu_io_error;
use dogfish_core::stdout::StdioModel;

use super::compare::Sorter;
use super::debug::debug_line;
use crate::common::input::{RawStdout, stdin_file};
use crate::common::quote::{qa, qf};

/// A fatal error whose message is printed: the exit status.
pub(super) struct Die(pub(super) i32);

pub(super) type Res<T> = Result<T, Die>;

/// Prints `sort: MSG` and fails with sort's status, 2.
pub(super) fn die(msg: impl std::fmt::Display) -> Die {
    eprintln!("sort: {msg}");
    Die(2)
}

/// GNU `sort_die`: `sort: WHAT: NAME: error`.
pub(super) fn sort_die(what: &str, name: &OsStr, err: &io::Error) -> Die {
    die(format_args!("{what}: {}: {}", qf(name), gnu_io_error(err)))
}

/// The name GNU gives standard output in messages.
const STDOUT_NAME: &str = "standard output";

/// Bytes read at a time.
const READ_SIZE: usize = 128 * 1024;

/// One input: a file operand (`-` is standard input) or a temporary file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Source {
    File(OsString),
    Temp(usize),
}

/// A temporary file (GNU `struct tempnode`).
struct TempNode {
    path: PathBuf,
    /// Written through the compress program.
    compressed: bool,
    /// The compressor, until it has been waited for.
    compressor: Option<Child>,
}

/// Paths of the temporary files that exist, for the signal handler.
static TEMP_PATHS: Mutex<Vec<CString>> = Mutex::new(Vec::new());

/// The signals that remove the temporary files before they end the
/// process, as in GNU (unless they were ignored when sort started).
const CLEANUP_SIGNALS: [libc::c_int; 10] = [
    libc::SIGALRM,
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGPIPE,
    libc::SIGQUIT,
    libc::SIGTERM,
    libc::SIGPROF,
    libc::SIGVTALRM,
    libc::SIGXCPU,
    libc::SIGXFSZ,
];

extern "C" fn on_signal(sig: libc::c_int) {
    // try_lock, not lock: the interrupted thread may hold the list. unlink,
    // signal and raise are async-signal-safe.
    if let Ok(paths) = TEMP_PATHS.try_lock() {
        for p in paths.iter() {
            // SAFETY: p is a live NUL-terminated path.
            unsafe { libc::unlink(p.as_ptr()) };
        }
    }
    // SAFETY: restoring the default action and re-raising the signal.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn install_cleanup_handlers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for &sig in &CLEANUP_SIGNALS {
            // SAFETY: a zeroed sigaction is "no flags, empty mask"; the query
            // only reads the current action; the handler is an extern "C" fn
            // that does only async-signal-safe work.
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                libc::sigaction(sig, std::ptr::null(), &mut old);
                if old.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = on_signal as *const () as libc::sighandler_t;
                libc::sigemptyset(&mut act.sa_mask);
                libc::sigaction(sig, &act, std::ptr::null_mut());
            }
        }
    });
}

fn forget_temp_path(path: &Path) {
    if let Ok(mut paths) = TEMP_PATHS.lock() {
        paths.retain(|p| p.as_bytes() != path.as_os_str().as_bytes());
    }
}

/// Where output goes and what sort was asked to do with lines.
pub(super) struct Env<'a> {
    pub(super) sorter: &'a Sorter,
    pub(super) eol: u8,
    pub(super) unique: bool,
    /// Annotate lines written to standard output (`--debug`).
    pub(super) debug: bool,
    /// Inputs merged at once (`--batch-size`).
    pub(super) nmerge: usize,
    /// `-S` in bytes, 0 when not given.
    pub(super) sort_size: usize,
    pub(super) temp_dirs: Vec<OsString>,
    next_dir: usize,
    pub(super) compress: Option<OsString>,
    /// `-o FILE` (open on standard output), or `None` for standard output.
    pub(super) output: Option<OsString>,
    temps: Vec<Option<TempNode>>,
    pub(super) read_stdin: bool,
}

impl<'a> Env<'a> {
    pub(super) fn new(sorter: &'a Sorter, eol: u8) -> Self {
        Env {
            sorter,
            eol,
            unique: false,
            debug: false,
            nmerge: 16,
            sort_size: 0,
            temp_dirs: Vec::new(),
            next_dir: 0,
            compress: None,
            output: None,
            temps: Vec::new(),
            read_stdin: false,
        }
    }

    pub(super) fn name(&self, src: &Source) -> OsString {
        match src {
            Source::File(name) => name.clone(),
            Source::Temp(id) => self.temps[*id]
                .as_ref()
                .map_or_else(OsString::new, |t| t.path.clone().into_os_string()),
        }
    }

    /// Removes every temporary file left (GNU `cleanup`, at exit).
    pub(super) fn cleanup(&mut self) {
        for id in 0..self.temps.len() {
            if let Some(mut t) = self.temps[id].take() {
                if let Some(mut c) = t.compressor.take() {
                    let _ = c.wait();
                }
                let _ = std::fs::remove_file(&t.path);
                forget_temp_path(&t.path);
            }
        }
    }

    /// Waits for the compressors that have finished, so that they do not
    /// pile up (GNU `reap_exited`).
    fn reap_exited(&mut self) -> Res<()> {
        for t in self.temps.iter_mut().flatten() {
            if let Some(c) = &mut t.compressor {
                match c.try_wait() {
                    Ok(Some(status)) => {
                        t.compressor = None;
                        check_status(self.compress.as_deref(), status)?;
                    }
                    Ok(None) => {}
                    Err(e) => return Err(wait_failed(self.compress.as_deref(), &e)),
                }
            }
        }
        Ok(())
    }

    /// GNU `maybe_create_temp`: a new temporary file, written through the
    /// compress program if there is one. `None` when out of file
    /// descriptors and `survive_fd_exhaustion`.
    pub(super) fn create_temp(
        &mut self,
        survive_fd_exhaustion: bool,
    ) -> Res<Option<(usize, LineWriter<'a>)>> {
        let dir = self
            .temp_dirs
            .get(self.next_dir)
            .cloned()
            .unwrap_or_else(|| "/tmp".into());
        self.next_dir = (self.next_dir + 1) % self.temp_dirs.len().max(1);
        install_cleanup_handlers();
        let state = RandomState::new();
        let mut attempt = 0u64;
        let (path, file) = loop {
            const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
            let mut r = state.hash_one(attempt);
            let mut name = b"sort".to_vec();
            for _ in 0..6 {
                name.push(CHARS[(r % CHARS.len() as u64) as usize]);
                r /= CHARS.len() as u64;
            }
            let mut path = PathBuf::from(&dir);
            path.push(OsStr::from_bytes(&name));
            // Registered before it exists, so that a signal cannot leave it.
            if let Ok(c) = CString::new(path.as_os_str().as_bytes())
                && let Ok(mut paths) = TEMP_PATHS.lock()
            {
                paths.push(c);
            }
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(f) => break (path, f),
                Err(e) => {
                    forget_temp_path(&path);
                    if e.kind() == io::ErrorKind::AlreadyExists && attempt < 1000 {
                        attempt += 1;
                        continue;
                    }
                    if survive_fd_exhaustion && e.raw_os_error() == Some(libc::EMFILE) {
                        return Ok(None);
                    }
                    return Err(die(format_args!(
                        "cannot create temporary file in {}: {}",
                        qa(&dir),
                        gnu_io_error(&e)
                    )));
                }
            }
        };
        let id = self.temps.len();
        let mut node = TempNode {
            path: path.clone(),
            compressed: false,
            compressor: None,
        };
        let mut sink = Sink::File(file);
        if let Some(prog) = self.compress.clone() {
            self.reap_exited()?;
            let Sink::File(file) = sink else {
                unreachable!()
            };
            match spawn_filter(&prog, false, file, &path) {
                Ok(mut child) => match child.stdin.take() {
                    Some(stdin) => {
                        node.compressed = true;
                        node.compressor = Some(child);
                        sink = Sink::Pipe(stdin);
                    }
                    None => unreachable!("stdin is piped"),
                },
                // As when GNU cannot fork: write the file uncompressed.
                Err(e) if is_fork_error(&e) => {
                    let reopened = OpenOptions::new().write(true).open(&path);
                    sink = Sink::File(reopened.map_err(|e| {
                        sort_die("couldn't create temporary file", path.as_os_str(), &e)
                    })?);
                }
                Err(e) => {
                    self.temps.push(Some(node));
                    return Err(exec_failed(&prog, "couldn't execute compress program", &e));
                }
            }
        }
        self.temps.push(Some(node));
        Ok(Some((
            id,
            LineWriter::new(sink, path.into_os_string(), false, self.sorter, self.eol),
        )))
    }

    /// GNU `zaptemp`: waits for the file's compressor and removes the file.
    fn zaptemp(&mut self, id: usize) -> Res<()> {
        if let Some(mut t) = self.temps[id].take() {
            if let Some(mut c) = t.compressor.take() {
                let status = c
                    .wait()
                    .map_err(|e| wait_failed(self.compress.as_deref(), &e))?;
                check_status(self.compress.as_deref(), status)?;
            }
            if let Err(e) = std::fs::remove_file(&t.path) {
                eprintln!(
                    "sort: warning: cannot remove: {}: {}",
                    qf(&t.path),
                    gnu_io_error(&e)
                );
            }
            forget_temp_path(&t.path);
        }
        Ok(())
    }

    /// Opens `src` for reading (GNU `stream_open` / `open_temp`). The error
    /// is the one `open` (or `fork`) failed with.
    pub(super) fn open(&mut self, src: &Source) -> Res<Result<Reader, io::Error>> {
        let input = match src {
            Source::File(name) if name == "-" => {
                self.read_stdin = true;
                Input::Stdin
            }
            Source::File(name) => match File::open(name) {
                Ok(f) => {
                    advise_sequential(&f);
                    Input::File(f)
                }
                Err(e) => return Ok(Err(e)),
            },
            Source::Temp(id) => {
                let compress = self.compress.clone();
                let Some(t) = self.temps[*id].as_mut() else {
                    return Ok(Err(io::Error::from_raw_os_error(libc::ENOENT)));
                };
                if let Some(mut c) = t.compressor.take() {
                    let status = c.wait().map_err(|e| wait_failed(compress.as_deref(), &e))?;
                    check_status(compress.as_deref(), status)?;
                }
                let file = match File::open(&t.path) {
                    Ok(f) => f,
                    Err(e) => return Ok(Err(e)),
                };
                match (&compress, t.compressed) {
                    (Some(prog), true) => match spawn_filter(prog, true, file, &t.path) {
                        Ok(mut child) => match child.stdout.take() {
                            Some(out) => Input::Pipe(out, child),
                            None => return Ok(Err(io::Error::from_raw_os_error(libc::EPIPE))),
                        },
                        Err(e) if e.raw_os_error() == Some(libc::EMFILE) => {
                            return Ok(Err(e));
                        }
                        Err(e) if is_fork_error(&e) => {
                            return Err(die(format_args!(
                                "couldn't create process for {} -d: {}",
                                qa(prog),
                                gnu_io_error(&e)
                            )));
                        }
                        Err(e) => {
                            return Err(exec_failed(
                                prog,
                                "couldn't execute compress program (with -d)",
                                &e,
                            ));
                        }
                    },
                    _ => Input::File(file),
                }
            }
        };
        Ok(Ok(Reader {
            input,
            name: self.name(src),
            buf: Vec::new(),
            pos: 0,
            end: 0,
            eof: false,
            line: (0, 0),
            key: (0, 0),
        }))
    }

    /// Opens what writes go to for the final output (GNU `stream_open` with
    /// "w"): standard output, which `-o FILE` was moved to and is truncated
    /// now.
    pub(super) fn open_output(&self) -> Res<LineWriter<'a>> {
        // SAFETY: descriptor only: fd 1 stays open for the life of the
        // process, and ManuallyDrop keeps this File from closing it.
        let fd1 = ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
        match &self.output {
            Some(name) => {
                if let Err(e) = fd1.set_len(0) {
                    let regular = fd1.metadata().map(|m| m.is_file()).unwrap_or(true);
                    if regular {
                        return Err(die(format_args!(
                            "{}: error truncating: {}",
                            qf(name),
                            gnu_io_error(&e)
                        )));
                    }
                }
                let blksize = fd1.metadata().map_or(0, |m| m.blksize());
                let mut w =
                    LineWriter::new(Sink::Fd(fd1), name.clone(), false, self.sorter, self.eol);
                w.gnu = Some(StdioModel::for_blksize(blksize));
                Ok(w)
            }
            None => {
                let mut w = LineWriter::new(
                    Sink::Stdout,
                    STDOUT_NAME.into(),
                    self.debug,
                    self.sorter,
                    self.eol,
                );
                w.gnu = Some(StdioModel::new());
                Ok(w)
            }
        }
    }

    /// Finishes a writer (GNU `xfclose`).
    pub(super) fn close_output(&self, mut w: LineWriter<'a>) -> Res<()> {
        match w.out.flush() {
            Ok(()) => Ok(()),
            Err(e) => Err(w.failed("close failed", &e)),
        }
    }
}

/// Whether a failed spawn failed to create the process (GNU's `fork`
/// failing) rather than to run the program in it.
fn is_fork_error(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::ENOMEM))
}

/// The compress program could not be run. In GNU the child says so, as an
/// error number, and dies; sort then finds that it terminated abnormally.
fn exec_failed(prog: &OsStr, what: &str, e: &io::Error) -> Die {
    eprintln!("{what}: errno {}", e.raw_os_error().unwrap_or(0));
    check_status(Some(prog), std::process::ExitStatus::from_raw(2 << 8))
        .err()
        .unwrap_or(Die(2))
}

fn wait_failed(prog: Option<&OsStr>, e: &io::Error) -> Die {
    die(format_args!(
        "waiting for {} [-d]: {}",
        qa(prog.unwrap_or_default()),
        gnu_io_error(e)
    ))
}

fn check_status(prog: Option<&OsStr>, status: std::process::ExitStatus) -> Res<()> {
    if status.success() {
        Ok(())
    } else {
        Err(die(format_args!(
            "{} [-d] terminated abnormally",
            qa(prog.unwrap_or_default())
        )))
    }
}

/// Starts the compress program writing to the temporary file `file` at
/// `path`, or (`decompress`) `PROG -d` reading it. As with `execlp`, a file
/// that is no executable format is run by `/bin/sh`.
fn spawn_filter(prog: &OsStr, decompress: bool, file: File, path: &Path) -> io::Result<Child> {
    let spawn = |cmd: &mut Command, file: File| {
        if decompress {
            cmd.arg("-d").stdin(file).stdout(Stdio::piped());
        } else {
            cmd.stdin(Stdio::piped()).stdout(file);
        }
        cmd.spawn()
    };
    match spawn(&mut Command::new(prog), file) {
        Err(e) if e.raw_os_error() == Some(libc::ENOEXEC) => {
            let Some(script) = find_program(prog) else {
                return Err(e);
            };
            let file = if decompress {
                File::open(path)?
            } else {
                OpenOptions::new().write(true).open(path)?
            };
            let mut sh = Command::new("/bin/sh");
            sh.arg0("/bin/sh").arg(script);
            spawn(&mut sh, file)
        }
        res => res,
    }
}

/// Where `execvp` finds `prog`: itself when it names a path, else the first
/// executable regular file of that name in `PATH`.
fn find_program(prog: &OsStr) -> Option<PathBuf> {
    if prog.as_bytes().contains(&b'/') {
        return Some(PathBuf::from(prog));
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into());
    path.as_bytes().split(|&c| c == b':').find_map(|dir| {
        let dir = if dir.is_empty() { b"." } else { dir };
        let candidate = Path::new(OsStr::from_bytes(dir)).join(prog);
        let meta = std::fs::metadata(&candidate).ok()?;
        (meta.is_file() && meta.mode() & 0o111 != 0).then_some(candidate)
    })
}

/// GNU's `fadvise (fp, FADVISE_SEQUENTIAL)` on an input.
fn advise_sequential(f: &File) {
    #[cfg(target_os = "linux")]
    // SAFETY: descriptor only: a hint about an open file; failure is ignored.
    unsafe {
        libc::posix_fadvise(
            std::os::fd::AsRawFd::as_raw_fd(f),
            0,
            0,
            libc::POSIX_FADV_SEQUENTIAL,
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = f;
}

enum Input {
    Stdin,
    File(File),
    Pipe(ChildStdout, Child),
}

/// An input read a line at a time; the current line stays valid until the
/// next [`Reader::advance`].
pub(super) struct Reader {
    input: Input,
    name: OsString,
    buf: Vec<u8>,
    pos: usize,
    end: usize,
    eof: bool,
    /// The current line, without its terminator.
    line: (usize, usize),
    /// The current line's first key (GNU `keybeg`/`keylim`), within it.
    key: (usize, usize),
}

impl Reader {
    pub(super) fn line(&self) -> &[u8] {
        &self.buf[self.line.0..self.line.1]
    }

    fn key(&self) -> &[u8] {
        &self.buf[self.line.0 + self.key.0..self.line.0 + self.key.1]
    }

    fn fill(&mut self) -> io::Result<usize> {
        if self.buf.len() - self.end < READ_SIZE / 2 {
            self.buf.resize((self.buf.len() * 2).max(READ_SIZE), 0);
        }
        let room = &mut self.buf[self.end..];
        loop {
            let res = match &mut self.input {
                Input::Stdin => (&*stdin_file()).read(room),
                Input::File(f) => f.read(room),
                Input::Pipe(p, _) => p.read(room),
            };
            match res {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                res => return res,
            }
        }
    }

    /// Moves to the next line; false at the end of the input (GNU supplies
    /// a missing final newline).
    pub(super) fn advance(&mut self, sorter: &Sorter, eol: u8) -> Res<bool> {
        loop {
            if let Some(i) = memchr::memchr(eol, &self.buf[self.pos..self.end]) {
                self.line = (self.pos, self.pos + i);
                self.pos += i + 1;
                break;
            }
            if self.eof {
                if self.pos == self.end {
                    return Ok(false);
                }
                self.line = (self.pos, self.end);
                self.pos = self.end;
                break;
            }
            if self.pos > 0 {
                self.buf.copy_within(self.pos..self.end, 0);
                self.end -= self.pos;
                self.pos = 0;
            }
            match self.fill() {
                Ok(0) => self.eof = true,
                Ok(n) => self.end += n,
                Err(e) => return Err(sort_die("read failed", &self.name, &e)),
            }
        }
        if let Some(first) = sorter.keys.first() {
            self.key = sorter.key_bounds(self.line(), first);
        }
        Ok(true)
    }

    /// Closes the input (GNU `xfclose`): waits for a decompressor that ran to
    /// the end, and stops one that did not.
    fn close(self, prog: Option<&OsStr>, finished: bool) -> Res<()> {
        if let Input::Pipe(out, mut child) = self.input {
            drop(out);
            if !finished {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
            let status = child.wait().map_err(|e| wait_failed(prog, &e))?;
            check_status(prog, status)?;
        }
        Ok(())
    }
}

/// Where a [`LineWriter`] writes.
pub(super) enum Sink {
    /// Standard output through Dogfish's writer.
    Stdout,
    /// `-o FILE`, moved onto descriptor 1.
    Fd(ManuallyDrop<File>),
    File(File),
    Pipe(ChildStdin),
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::Stdout => RawStdout.write(buf),
            Sink::Fd(f) => (&**f).write(buf),
            Sink::File(f) => f.write(buf),
            Sink::Pipe(p) => p.write(buf),
        }
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        match self {
            Sink::Stdout => RawStdout.write_all(buf),
            Sink::Fd(f) => (&**f).write_all(buf),
            Sink::File(f) => f.write_all(buf),
            Sink::Pipe(p) => p.write_all(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::Stdout => RawStdout.flush(),
            _ => Ok(()),
        }
    }
}

/// GNU `write_line` over a buffered output.
pub(super) struct LineWriter<'a> {
    out: BufWriter<Sink>,
    name: OsString,
    debug: bool,
    sorter: &'a Sorter,
    eol: u8,
    scratch: Vec<u8>,
    pub(super) lines: u64,
    pub(super) bytes: u64,
    /// For the final output, glibc's buffer as GNU's `stdout` would fill it,
    /// to word a write error as GNU does.
    gnu: Option<StdioModel>,
    /// GNU's `fwrite` would have called write(2) by now.
    gnu_wrote: bool,
}

impl<'a> LineWriter<'a> {
    fn new(sink: Sink, name: OsString, debug: bool, sorter: &'a Sorter, eol: u8) -> Self {
        LineWriter {
            out: BufWriter::with_capacity(256 * 1024, sink),
            name,
            debug,
            sorter,
            eol,
            scratch: Vec::new(),
            lines: 0,
            bytes: 0,
            gnu: None,
            gnu_wrote: false,
        }
    }

    /// Records GNU's `fwrite` of `n` bytes of output.
    #[inline]
    fn gnu_fwrite(&mut self, n: usize) {
        if !self.gnu_wrote
            && let Some(m) = &mut self.gnu
        {
            self.gnu_wrote = m.fwrite(n);
        }
    }

    /// The diagnostic for a failed write. On the final output GNU dies in
    /// the `fwrite` that had to write (`write failed`) or, with the output
    /// still buffered, in the final `fflush` (`fflush failed`); its
    /// `close_stdout` then adds `sort: write error` (Dogfish's exit does
    /// that for standard output). On a temporary file, `what` happened.
    fn failed(&self, what: &str, e: &io::Error) -> Die {
        if self.gnu.is_none() {
            return sort_die(what, &self.name, e);
        }
        let what = if self.gnu_wrote {
            "write failed"
        } else {
            "fflush failed"
        };
        let die = sort_die(what, &self.name, e);
        if matches!(self.out.get_ref(), Sink::Fd(_)) {
            eprintln!("sort: write error");
        }
        die
    }

    pub(super) fn write_line(&mut self, line: &[u8]) -> Res<()> {
        self.lines += 1;
        self.bytes += line.len() as u64 + 1;
        let res = if self.debug {
            self.scratch.clear();
            debug_line(self.sorter, &mut self.scratch, line);
            self.gnu_fwrite(self.scratch.len());
            self.out.write_all(&self.scratch)
        } else {
            self.gnu_fwrite(line.len() + 1);
            self.out
                .write_all(line)
                .and_then(|()| self.out.write_all(&[self.eol]))
        };
        res.map_err(|e| self.failed("write failed", &e))
    }

    /// [`LineWriter::write_line`] for each of `lines`.
    pub(super) fn write_lines(&mut self, lines: &[&[u8]]) -> Res<()> {
        if self.debug {
            return lines.iter().try_for_each(|line| self.write_line(line));
        }
        let eol = [self.eol];
        for line in lines {
            self.gnu_fwrite(line.len() + 1);
            if let Err(e) = self
                .out
                .write_all(line)
                .and_then(|()| self.out.write_all(&eol))
            {
                return Err(self.failed("write failed", &e));
            }
            self.bytes += line.len() as u64 + 1;
        }
        self.lines += lines.len() as u64;
        Ok(())
    }
}

/// Opens as many of `files` as possible, in order (GNU `open_input_files`);
/// the error is why the next one did not open.
fn open_input_files(env: &mut Env, files: &[Source]) -> Res<(Vec<Reader>, Option<io::Error>)> {
    let mut readers = Vec::with_capacity(files.len());
    for f in files {
        match env.open(f)? {
            Ok(r) => readers.push(r),
            Err(e) => return Ok((readers, Some(e))),
        }
    }
    Ok((readers, None))
}

/// GNU `mergefps`: merges `readers` (the first `ntemps` are temporary files
/// removed once read) onto `out`, then closes everything.
fn mergefps(
    env: &mut Env,
    files: &[Source],
    ntemps: usize,
    readers: Vec<Reader>,
    mut out: LineWriter,
) -> Res<()> {
    let sorter = env.sorter;
    let eol = env.eol;
    let prog = env.compress.clone();
    let mut readers: Vec<Option<Reader>> = readers.into_iter().map(Some).collect();
    // Each input's first line; drop the empty ones.
    let mut live: Vec<usize> = Vec::with_capacity(readers.len());
    for i in 0..readers.len() {
        let has_line = match readers[i].as_mut() {
            Some(r) => r.advance(sorter, eol)?,
            None => false,
        };
        if has_line {
            live.push(i);
        } else {
            finish_input(env, &mut readers, files, i, ntemps, prog.as_deref())?;
        }
    }
    let cmp = |readers: &[Option<Reader>], i: usize, j: usize| -> Ordering {
        match (&readers[i], &readers[j]) {
            (Some(a), Some(b)) => sorter.compare_pre(a.line(), a.key(), b.line(), b.key()),
            _ => Ordering::Equal,
        }
    };
    // `ord`: the inputs in the order of their current lines; this only
    // swaps strictly greater lines, so it is stable.
    let mut ord = live;
    let mut i = 1;
    while i < ord.len() {
        if cmp(&readers, ord[i - 1], ord[i]) == Ordering::Greater {
            ord.swap(i - 1, i);
            i = 1;
        } else {
            i += 1;
        }
    }
    let mut saved: Option<(Vec<u8>, (usize, usize))> = None;
    while let Some(&smallest) = ord.first() {
        {
            let Some(r) = readers[smallest].as_ref() else {
                break;
            };
            if env.unique {
                let differs = saved.as_ref().is_some_and(|(line, key)| {
                    sorter.compare_pre(line, &line[key.0..key.1], r.line(), r.key())
                        != Ordering::Equal
                });
                if differs && let Some((line, _)) = saved.take() {
                    out.write_line(&line)?;
                }
                if saved.is_none() {
                    saved = Some((r.line().to_vec(), r.key));
                }
            } else {
                out.write_line(r.line())?;
            }
        }
        let more = match readers[smallest].as_mut() {
            Some(r) => r.advance(sorter, eol)?,
            None => false,
        };
        if !more {
            ord.remove(0);
            finish_input(env, &mut readers, files, smallest, ntemps, prog.as_deref())?;
            continue;
        }
        // The new line may be larger than others: move it back to its
        // place, after equal lines of inputs that come before it.
        let (mut lo, mut hi) = (1, ord.len());
        while lo < hi {
            let probe = (lo + hi) / 2;
            let c = cmp(&readers, smallest, ord[probe]);
            if c == Ordering::Less || (c == Ordering::Equal && smallest < ord[probe]) {
                hi = probe;
            } else {
                lo = probe + 1;
            }
        }
        ord.copy_within(1..lo, 0);
        ord[lo - 1] = smallest;
    }
    if let Some((line, _)) = saved {
        out.write_line(&line)?;
    }
    env.close_output(out)
}

/// An input of `mergefps` reached its end: close it, and remove it if it is
/// one of the temporary files counted in `ntemps`.
fn finish_input(
    env: &mut Env,
    readers: &mut [Option<Reader>],
    files: &[Source],
    i: usize,
    ntemps: usize,
    prog: Option<&OsStr>,
) -> Res<()> {
    if let Some(r) = readers[i].take() {
        r.close(prog, true)?;
    }
    if i < ntemps
        && let Source::Temp(id) = files[i]
    {
        env.zaptemp(id)?;
    }
    Ok(())
}

/// GNU `mergefiles`: merges as many of `files` as can be opened (at least
/// two) onto `out`; returns how many.
fn mergefiles(env: &mut Env, files: &[Source], ntemps: usize, out: LineWriter) -> Res<usize> {
    let (readers, err) = open_input_files(env, files)?;
    let n = readers.len();
    if n < files.len() && n < 2 {
        let e = err.unwrap_or_else(|| io::Error::from_raw_os_error(libc::EMFILE));
        return Err(sort_die("open failed", &env.name(&files[n]), &e));
    }
    mergefps(env, &files[..n], ntemps, readers, out)?;
    Ok(n)
}

/// The identity of the final output (GNU `get_outstatus`).
fn output_identity() -> Option<(u64, u64)> {
    // SAFETY: descriptor only: fd 1 stays open; ManuallyDrop keeps it so.
    let fd1 = ManuallyDrop::new(unsafe { File::from_raw_fd(1) });
    fd1.metadata().ok().map(|m| (m.dev(), m.ino()))
}

/// GNU `avoid_trashing_input`: an input that is also the output is first
/// copied to a temporary file, which is read in its place.
fn avoid_trashing_input(env: &mut Env, files: &mut [Source], ntemps: usize) -> Res<()> {
    let mut copy: Option<usize> = None;
    let out_id = output_identity();
    for i in ntemps..files.len() {
        let Source::File(name) = &files[i] else {
            continue;
        };
        let is_stdin = name == "-";
        let same = if env.output.as_deref() == Some(name.as_os_str()) && !is_stdin {
            true
        } else {
            let Some(out_id) = out_id else {
                break;
            };
            let meta = if is_stdin {
                stdin_file().metadata()
            } else {
                std::fs::metadata(name)
            };
            meta.is_ok_and(|m| (m.dev(), m.ino()) == out_id)
        };
        if same {
            let id = match copy {
                Some(id) => id,
                None => {
                    let Some((id, w)) = env.create_temp(false)? else {
                        return Err(die("cannot create temporary file"));
                    };
                    mergefiles(env, &files[i..=i], 0, w)?;
                    copy = Some(id);
                    id
                }
            };
            files[i] = Source::Temp(id);
        }
    }
    Ok(())
}

/// GNU `merge`: merges `files` (the first `ntemps` temporary) onto the
/// output, through temporary files when there are more than `--batch-size`
/// of them or too few file descriptors.
pub(super) fn merge(env: &mut Env, mut files: Vec<Source>, mut ntemps: usize) -> Res<()> {
    let nmerge = env.nmerge;
    while nmerge < files.len() {
        let nfiles = files.len();
        let (mut i, mut out) = (0, 0);
        // As many full-size merges as possible.
        while nmerge <= nfiles - i {
            let Some((id, w)) = env.create_temp(false)? else {
                return Err(die("cannot create temporary file"));
            };
            let n = mergefiles(env, &files[i..i + nmerge], ntemps.min(nmerge), w)?;
            ntemps -= ntemps.min(n);
            files[out] = Source::Temp(id);
            out += 1;
            i += n;
        }
        let remainder = nfiles - i;
        let cheap_slots = nmerge - out % nmerge;
        if cheap_slots < remainder {
            // Too many left for the last window: one more, short merge.
            let nshort = remainder - cheap_slots + 1;
            let Some((id, w)) = env.create_temp(false)? else {
                return Err(die("cannot create temporary file"));
            };
            let n = mergefiles(env, &files[i..i + nshort], ntemps.min(nshort), w)?;
            ntemps -= ntemps.min(n);
            files[out] = Source::Temp(id);
            out += 1;
            i += n;
        }
        files.drain(out..i);
        ntemps += out;
    }

    avoid_trashing_input(env, &mut files, ntemps)?;

    // The final merge may lack file descriptors: then merge what could be
    // opened into a temporary file, and try again.
    loop {
        let (mut readers, err) = open_input_files(env, &files)?;
        let mut n = readers.len();
        if n == files.len() {
            let out = env.open_output()?;
            return mergefps(env, &files, ntemps, readers, out);
        }
        if n <= 2 {
            let e = err.unwrap_or_else(|| io::Error::from_raw_os_error(libc::EMFILE));
            return Err(sort_die("open failed", &env.name(&files[n]), &e));
        }
        // Close an input to free a descriptor for the temporary file.
        let prog = env.compress.clone();
        let (id, w) = loop {
            n -= 1;
            if let Some(r) = readers.pop() {
                r.close(prog.as_deref(), false)?;
            }
            if let Some(t) = env.create_temp(n > 2)? {
                break t;
            }
        };
        mergefps(env, &files[..n], ntemps.min(n), readers, w)?;
        ntemps -= ntemps.min(n);
        files[0] = Source::Temp(id);
        files.drain(1..n);
        ntemps += 1;
    }
}

/// GNU `check`: whether `file` is sorted; with `-c`, the first disorder is
/// reported.
pub(super) fn check(env: &mut Env, file: &OsStr, quiet: bool) -> Res<bool> {
    let src = Source::File(file.to_os_string());
    let mut r = match env.open(&src)? {
        Ok(r) => r,
        Err(e) => return Err(sort_die("open failed", file, &e)),
    };
    let sorter = env.sorter;
    let eol = env.eol;
    let mut prev: Vec<u8> = Vec::new();
    let mut prev_key = (0, 0);
    let mut have_prev = false;
    let mut number: u64 = 0;
    let disorder_at = if env.unique {
        Ordering::Equal
    } else {
        Ordering::Greater
    };
    while r.advance(sorter, eol)? {
        number += 1;
        if have_prev {
            let ord = sorter.compare_pre(&prev, &prev[prev_key.0..prev_key.1], r.line(), r.key());
            if ord >= disorder_at {
                if !quiet {
                    let mut msg = b"sort: ".to_vec();
                    msg.extend_from_slice(file.as_bytes());
                    msg.extend_from_slice(format!(":{number}: disorder: ").as_bytes());
                    msg.extend_from_slice(r.line());
                    msg.push(eol);
                    let _ = io::stderr().write_all(&msg);
                }
                r.close(None, true)?;
                return Ok(false);
            }
        }
        prev.clear();
        prev.extend_from_slice(r.line());
        prev_key = r.key;
        have_prev = true;
    }
    r.close(None, true)?;
    Ok(true)
}
