//! The process's one standard output, with GNU `close_stdout` semantics.
//!
//! Every byte a utility prints goes through [`stdout()`]. It writes to file
//! descriptor 1 directly (Rust's `std::io::stdout` silently discards output
//! when the descriptor is closed, so `printf foo >&-` would succeed), buffers
//! like a line-buffered stdio stream (whole lines are written as they
//! complete, so output interleaves with diagnostics in order), and remembers
//! the first write error. That error is sticky, as with stdio's `ferror`:
//! every later write fails with it at once, so a tool that loops on output
//! stops at the first failure. At exit, [`close()`] flushes what is left and
//! hands back an error the tool has not reported itself, for the caller to
//! print as GNU's `close_stdout` does (`tool: write error: ...`).
//!
//! Whether GNU's message ends in the reason (`write error: No space left on
//! device`) depends on how its stdio buffer met the error, which a tool
//! describes with [`gnu_flushes_early`] or, call by call, [`gnu_fwrite`].

use std::io::{self, Write};
use std::sync::{Mutex, MutexGuard};

/// Most bytes held back while waiting for the end of a line.
const BUF_CAP: usize = 8 * 1024;

struct State {
    /// Descriptor 1 was closed when the process started (see [`started_closed`]).
    closed: bool,
    buf: Vec<u8>,
    /// `errno` of the first failed write, if any.
    error: Option<i32>,
    /// The tool printed its own diagnostic for `error`.
    reported: bool,
    /// How GNU's stdio holds this tool's output.
    gnu: GnuStream,
    /// GNU's `close_stdout_set_ignore_EPIPE` was called.
    ignore_epipe: bool,
    /// The `st_blksize` of the input GNU would open on a closed descriptor
    /// 1 (see [`gnu_input_on_fd1`]).
    fd1_input: Option<u64>,
    /// That input stays open to the end.
    fd1_input_kept: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    closed: false,
    buf: Vec::new(),
    error: None,
    reported: false,
    gnu: GnuStream::Buffered(StdioModel::unbounded()),
    ignore_epipe: false,
    fd1_input: None,
    fd1_input_kept: false,
});

/// How GNU's standard output stream meets a write error, which decides
/// whether `close_stdout` can still say why. gnulib's `close_stream` keeps
/// `errno` only when the final `fclose` fails; when the error was met
/// earlier and nothing is left to flush at exit, it reports `errno` 0 and
/// the message is a bare `write error`.
#[derive(Clone, Copy, Debug)]
enum GnuStream {
    /// Fully buffered, the default: modelled from Dogfish's own writes as
    /// if the buffer were unbounded, so the bytes written since the last
    /// `fflush` ([`gnu_fflush`], which every diagnostic does) are left for
    /// exit, which fails with the reason.
    Buffered(StdioModel),
    /// Flushed line by line, or by a tool that stops at its first failed
    /// write: the error is met before exit, with nothing left over.
    FlushedEarly,
    /// The tool reports every stdio call GNU makes, so the buffer is modelled.
    Modelled(StdioModel),
}

/// glibc's buffer for a fully buffered standard output on which every
/// `write(2)` fails: just enough of `_IO_new_file_xsputn`,
/// `_IO_new_file_overflow` and `fflush` to tell which stdio calls write
/// (and so fail), and whether bytes are still buffered at exit.
///
/// The model assumes the output fails from its first byte (`/dev/full`, a
/// pipe with no reader, a closed descriptor), the case where GNU's own
/// behaviour is deterministic.
#[derive(Clone, Copy, Debug)]
pub struct StdioModel {
    /// The buffer size glibc picks: the descriptor's `st_blksize`, at most
    /// `BUFSIZ`.
    size: usize,
    /// Bytes in the buffer; `None` until the first output allocates it.
    used: Option<usize>,
    /// A `write(2)` was made, and failed (stdio's `ferror`).
    failed: bool,
}

impl StdioModel {
    /// The model of a fresh `stdout`, its buffer sized as glibc's
    /// `_IO_file_doallocate` sizes it.
    pub fn new() -> Self {
        state().new_model()
    }

    /// [`StdioModel::new`], told whether descriptor 1 started closed (held
    /// by /dev/null here), which makes GNU's fstat fail.
    fn for_stdout(closed: bool) -> Self {
        // SAFETY: fstat writes into the zeroed buffer it is given.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat on descriptor 1 with a properly sized buffer.
        let ok = !closed && unsafe { libc::fstat(libc::STDOUT_FILENO, &mut st) } == 0;
        Self::for_blksize(if ok {
            u64::try_from(st.st_blksize).unwrap_or(0)
        } else {
            0
        })
    }

    /// The model of a stream on a file whose `st_blksize` is `blksize` (0
    /// when unknown): glibc uses it when smaller than `BUFSIZ`.
    pub fn for_blksize(blksize: u64) -> Self {
        // glibc's BUFSIZ.
        const BUFSIZ: usize = 8192;
        let blksize = usize::try_from(blksize).unwrap_or(0);
        Self::with_size(if (1..BUFSIZ).contains(&blksize) {
            blksize
        } else {
            BUFSIZ
        })
    }

    /// The model of a buffer of `size` bytes.
    pub const fn with_size(size: usize) -> Self {
        Self {
            size: if size == 0 { 1 } else { size },
            used: None,
            failed: false,
        }
    }

    /// A buffer that never fills: only flushes write.
    const fn unbounded() -> Self {
        Self::with_size(usize::MAX)
    }

    /// One `fwrite` (or `fputs`, or one `printf` conversion) of `n` bytes;
    /// returns whether it called `write(2)`, and so failed. glibc fills the
    /// buffer, flushes it, then writes whole blocks directly; a failure
    /// drops the rest of the call's bytes.
    pub fn fwrite(&mut self, n: usize) -> bool {
        if n == 0 {
            return false;
        }
        let Some(used) = self.used else {
            // The first output allocates the buffer (writing nothing); whole
            // blocks go straight out, and the remainder is buffered.
            let direct = if self.size >= 128 {
                n - n % self.size
            } else {
                n
            };
            if direct > 0 {
                self.used = Some(0);
                self.failed = true;
                return true;
            }
            self.used = Some(n);
            return false;
        };
        if used.saturating_add(n) <= self.size {
            self.used = Some(used.saturating_add(n));
            return false;
        }
        // The buffer is filled and flushed, which fails.
        self.used = Some(0);
        self.failed = true;
        true
    }

    /// `n` calls of `putchar`: a full buffer is flushed (and fails) on the
    /// next one, whose byte is lost.
    pub fn putchars(&mut self, mut n: usize) {
        while n > 0 {
            let used = self.used.unwrap_or(0);
            if used < self.size {
                let k = (self.size - used).min(n);
                self.used = Some(used + k);
                n -= k;
            } else {
                self.used = Some(0);
                self.failed = true;
                n -= 1;
            }
        }
    }

    /// `fflush (stdout)`, which glibc's `error` also does before printing a
    /// diagnostic.
    pub fn fflush(&mut self) {
        if self.used.is_some_and(|u| u > 0) {
            self.used = Some(0);
            self.failed = true;
        }
    }

    /// Whether `close_stdout` would have lost `errno`: a write failed
    /// before exit and nothing is left for `fclose` to flush.
    pub fn errno_lost(&self) -> bool {
        self.failed && self.used.unwrap_or(0) == 0
    }
}

impl Default for StdioModel {
    fn default() -> Self {
        Self::new()
    }
}

fn state() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `write(2)` all of `data` to descriptor 1, retrying on `EINTR`.
fn write_fd(mut data: &[u8]) -> Result<(), i32> {
    while !data.is_empty() {
        // SAFETY: `data` is a valid, initialized buffer of `data.len()` bytes.
        let n = unsafe { libc::write(libc::STDOUT_FILENO, data.as_ptr().cast(), data.len()) };
        if n < 0 {
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno == libc::EINTR {
                continue;
            }
            return Err(errno);
        }
        if n == 0 {
            return Err(libc::ENOSPC);
        }
        data = &data[n as usize..];
    }
    Ok(())
}

impl State {
    /// Whether GNU would report the write error without its reason. Writes
    /// to a closed descriptor are the exception: GNU's `fclose` fails too,
    /// with `EBADF`, and says so, unless an input took the descriptor.
    fn errno_lost(&self) -> bool {
        (!self.closed || self.fd1_input_kept)
            && match self.gnu {
                GnuStream::FlushedEarly => true,
                GnuStream::Buffered(m) | GnuStream::Modelled(m) => m.errno_lost(),
            }
    }

    /// A model of GNU's buffer for standard output, allocated now.
    fn new_model(&self) -> StdioModel {
        match self.fd1_input {
            Some(blksize) if self.closed => StdioModel::for_blksize(blksize),
            _ => StdioModel::for_stdout(self.closed),
        }
    }

    /// The model of GNU's buffer, started at the first call that feeds it.
    fn model(&mut self) -> &mut StdioModel {
        if !matches!(self.gnu, GnuStream::Modelled(_)) {
            self.gnu = GnuStream::Modelled(self.new_model());
        }
        match &mut self.gnu {
            GnuStream::Modelled(m) => m,
            _ => unreachable!("the model was just set"),
        }
    }

    fn fail(&mut self, errno: i32) -> io::Error {
        self.buf.clear();
        self.error = Some(errno);
        io::Error::from_raw_os_error(errno)
    }

    fn sticky(&self) -> Option<io::Error> {
        self.error.map(io::Error::from_raw_os_error)
    }

    fn flush_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let res = write_fd(&self.buf);
        self.buf.clear();
        res.map_err(|errno| self.fail(errno))
    }

    /// Records `data` written in the coarse model of GNU's buffer.
    fn note(&mut self, data: &[u8]) {
        if let GnuStream::Buffered(m) = &mut self.gnu {
            m.fwrite(data.len());
        }
    }

    fn put(&mut self, data: &[u8]) -> io::Result<()> {
        self.note(data);
        if let Some(err) = self.sticky() {
            return Err(err);
        }
        if data.is_empty() {
            return Ok(());
        }
        if self.closed {
            return Err(self.fail(libc::EBADF));
        }
        // Everything through the last newline goes out now; the rest waits.
        let (now, later) = match memchr::memrchr(b'\n', data) {
            Some(i) => data.split_at(i + 1),
            None => (&data[..0], data),
        };
        if !now.is_empty() {
            if self.buf.len() + now.len() <= BUF_CAP {
                self.buf.extend_from_slice(now);
                self.flush_buf()?;
            } else {
                self.flush_buf()?;
                write_fd(now).map_err(|errno| self.fail(errno))?;
            }
        }
        if self.buf.len() + later.len() > BUF_CAP {
            self.flush_buf()?;
            if later.len() > BUF_CAP {
                return write_fd(later).map_err(|errno| self.fail(errno));
            }
        }
        self.buf.extend_from_slice(later);
        Ok(())
    }
}

/// A handle to standard output; cheap to create, and every handle shares one
/// buffer and one error state. Wrap it in a `BufWriter` for many small writes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stdout;

/// The standard-output handle.
pub fn stdout() -> Stdout {
    Stdout
}

impl Write for Stdout {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        state().put(data).map(|()| data.len())
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        state().put(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut st = state();
        if let Some(err) = st.sticky() {
            return Err(err);
        }
        st.flush_buf()
    }
}

/// Writes `data` out now, after anything buffered, without line buffering:
/// for tools that buffer large blocks themselves. Errors are recorded and
/// sticky as for [`stdout()`].
pub fn write_whole(data: &[u8]) -> io::Result<()> {
    let mut st = state();
    st.note(data);
    if let Some(err) = st.sticky() {
        return Err(err);
    }
    if data.is_empty() {
        return Ok(());
    }
    if st.closed {
        return Err(st.fail(libc::EBADF));
    }
    st.flush_buf()?;
    write_fd(data).map_err(|errno| st.fail(errno))
}

/// Records that descriptor 1 was closed when the process started. The
/// caller keeps the descriptor number taken (on `/dev/null`) so that no file
/// the tool opens lands on it, and every write fails with `EBADF`, as it
/// would for GNU's `printf foo >&-`.
pub fn started_closed() {
    state().closed = true;
}

/// Whether descriptor 1 was closed when the process started, for tools that
/// use the descriptor other than by writing (`touch -`).
pub fn was_closed() -> bool {
    state().closed
}

/// Writes out any buffered bytes (before running a child process that shares
/// the descriptor, or when output must reach a reader now).
pub fn flush() -> io::Result<()> {
    stdout().flush()
}

/// The first write error, if one happened.
pub fn error() -> Option<io::Error> {
    state().sticky()
}

/// Records that the tool printed its own diagnostic for the write error, so
/// [`close()`] does not report it a second time.
pub fn mark_reported() {
    state().reported = true;
}

/// Whether `err` is the write error standard output failed with.
pub fn is_write_error(err: &io::Error) -> bool {
    let st = state();
    st.error.is_some() && st.error == err.raw_os_error()
}

/// Flushes standard output at exit; returns a write error that nobody has
/// reported yet (GNU's `close_stdout`, which the caller prints, with the
/// reason unless [`errno_lost`]). Writing nothing to a closed descriptor
/// is no error, as in GNU, and neither is a broken pipe after
/// [`ignore_epipe`].
pub fn close() -> Option<io::Error> {
    let mut st = state();
    if st.error.is_none() {
        let _ = st.flush_buf();
    }
    if st.reported {
        return None;
    }
    st.reported = true;
    let err = st.sticky()?;
    if st.ignore_epipe && st.error == Some(libc::EPIPE) && !st.errno_lost() {
        return None;
    }
    Some(err)
}

/// Whether GNU's `close_stdout` would print a bare `write error`, having
/// met the failure before exit with nothing left to flush (so `errno` is
/// 0), rather than `write error: REASON`.
pub fn errno_lost() -> bool {
    state().errno_lost()
}

/// Declares that GNU meets this tool's write errors before exit, with
/// nothing left buffered: its standard output is line buffered
/// (`setvbuf (stdout, NULL, _IOLBF, 0)`), flushed after every line, or
/// abandoned at the first failed write. Called where GNU starts doing so,
/// often after option parsing, so that `--help` stays fully buffered.
pub fn gnu_flushes_early() {
    state().gnu = GnuStream::FlushedEarly;
}

/// Records that GNU writes the next `n` bytes of output with one stdio call
/// (`fwrite`, `fputs`, one `printf` conversion); returns whether glibc
/// calls `write(2)` for it, which on a failing output fails the call. See
/// [`StdioModel`].
pub fn gnu_fwrite(n: usize) -> bool {
    state().model().fwrite(n)
}

/// Records that GNU writes the next `n` bytes one `putchar` at a time.
pub fn gnu_putchars(n: usize) {
    state().model().putchars(n);
}

/// GNU flushes standard output here: an `fflush`, or glibc's `error`,
/// which flushes it before every diagnostic (see [`crate::gnu_error!`]). Buffered
/// bytes are written out now too, so that they come before the diagnostic.
pub fn gnu_fflush() {
    let mut st = state();
    if st.error.is_none() {
        let _ = st.flush_buf();
    }
    if let GnuStream::Buffered(m) | GnuStream::Modelled(m) = &mut st.gnu {
        m.fflush();
    }
}

/// Records that GNU, run with descriptor 1 closed, opens the input it is
/// about to read on that descriptor, the lowest free one; `blksize` is the
/// input's `st_blksize`, by which stdio sizes the buffer it allocates while
/// the input is there. Writes still fail, with `EBADF`; when the tool keeps
/// the input open to the end (`kept_open`, as `sort FILE` does), the final
/// `fclose` closes the input instead and succeeds, so [`errno_lost`] can
/// hold. Nothing changes when descriptor 1 was open.
pub fn gnu_input_on_fd1(blksize: u64, kept_open: bool) {
    let mut st = state();
    st.fd1_input = Some(blksize);
    st.fd1_input_kept |= kept_open;
}

/// Records that a GNU stdio call failed without writing anything, which
/// sets the stream's error flag (`printf` failing with `EOVERFLOW`).
pub fn gnu_ferror() {
    state().model().failed = true;
}

/// GNU's `close_stdout_set_ignore_EPIPE`: a broken pipe found at exit is
/// not reported.
pub fn ignore_epipe() {
    state().ignore_epipe = true;
}

/// A writer that makes each `write_all` one GNU stdio call on standard
/// output ([`gnu_fwrite`]), for tools that diagnose a failed `fwrite`
/// themselves (`head`, `base64`): `inner` is flushed whenever glibc would
/// call `write(2)`, and a write error surfaces at that call, as in GNU. One
/// met earlier stays recorded (it is sticky) and is reported there or at
/// exit.
pub struct GnuFwrite<W: Write> {
    inner: W,
}

impl<W: Write> GnuFwrite<W> {
    /// Wraps `inner`, a writer to standard output.
    pub fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<W: Write> Write for GnuFwrite<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.write_all(data).map(|()| data.len())
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        let res = self.inner.write_all(data);
        if gnu_fwrite(data.len()) {
            res?;
            self.inner.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A buffered writer that, like a glibc stream and unlike
/// `std::io::BufWriter`, drops the bytes it failed to write instead of
/// trying them again at the next flush: for tools that, as GNU does, go on
/// after a write error (flushing before each diagnostic) and report it at
/// exit.
pub struct StdioBuf<W: Write> {
    inner: W,
    buf: Vec<u8>,
    cap: usize,
}

impl<W: Write> StdioBuf<W> {
    /// Buffers up to 8 KiB for `inner`.
    pub fn new(inner: W) -> Self {
        Self::with_capacity(BUF_CAP, inner)
    }

    /// Buffers up to `cap` bytes for `inner`.
    pub fn with_capacity(cap: usize, inner: W) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(cap),
            cap,
        }
    }

    fn flush_buf(&mut self) -> io::Result<()> {
        let res = self.inner.write_all(&self.buf);
        self.buf.clear();
        res
    }
}

impl<W: Write> Write for StdioBuf<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.write_all(data).map(|()| data.len())
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if self.buf.len() + data.len() > self.cap {
            self.flush_buf()?;
        }
        if data.len() >= self.cap {
            return self.inner.write_all(data);
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buf()?;
        self.inner.flush()
    }
}

impl<W: Write> Drop for StdioBuf<W> {
    fn drop(&mut self) {
        let _ = self.flush_buf();
    }
}

/// Writes `--help`-style text, ignoring the result: a failure is reported
/// once, at exit, by [`close()`].
pub fn print_str(text: &str) {
    let _ = stdout().write_all(text.as_bytes());
}

/// Like `print!`, through [`stdout()`]; a write error is reported at exit.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::write!($crate::stdout::stdout(), $($arg)*);
    }};
}

/// Like `eprintln!`, the way glibc's `error` prints a diagnostic: GNU's
/// standard output is flushed first ([`gnu_fflush`]), for the tools whose
/// stdio calls are modelled. A failure to write the diagnostic is ignored,
/// as in GNU (`eprintln!` would panic).
#[macro_export]
macro_rules! gnu_error {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        $crate::stdout::gnu_fflush();
        let _ = ::std::writeln!(::std::io::stderr(), $($arg)*);
    }};
}

/// Like `println!`, through [`stdout()`]; a write error is reported at exit.
#[macro_export]
macro_rules! outln {
    () => {{
        use ::std::io::Write as _;
        let _ = $crate::stdout::stdout().write_all(b"\n");
    }};
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let _ = ::std::writeln!($crate::stdout::stdout(), $($arg)*);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sticky_error_state() {
        let mut st = State {
            closed: false,
            buf: Vec::new(),
            error: None,
            reported: false,
            gnu: GnuStream::Buffered(StdioModel::unbounded()),
            ignore_epipe: false,
            fd1_input: None,
            fd1_input_kept: false,
        };
        st.buf.extend_from_slice(b"partial");
        let err = st.fail(libc::ENOSPC);
        assert_eq!(err.raw_os_error(), Some(libc::ENOSPC));
        assert!(st.buf.is_empty());
        let again = st.put(b"more\n").unwrap_err();
        assert_eq!(again.raw_os_error(), Some(libc::ENOSPC));
        assert!(st.buf.is_empty());
    }

    #[test]
    fn closed_descriptor_fails_only_when_written() {
        let mut st = State {
            closed: true,
            buf: Vec::new(),
            error: None,
            reported: false,
            gnu: GnuStream::Buffered(StdioModel::unbounded()),
            ignore_epipe: false,
            fd1_input: None,
            fd1_input_kept: false,
        };
        assert!(st.put(b"").is_ok());
        assert_eq!(st.put(b"x").unwrap_err().raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    fn stdio_model_follows_glibc() {
        // printf %04095d: buffered, so fclose fails and says why.
        let mut m = StdioModel::with_size(4096);
        assert!(!m.fwrite(4095));
        assert!(!m.errno_lost());
        // printf %04096d: a whole block goes straight out and fails.
        let mut m = StdioModel::with_size(4096);
        assert!(m.fwrite(4096));
        assert!(m.errno_lost());
        // printf %02000d%02096d fits; one more byte overflows, and the
        // rest of that call is dropped.
        let mut m = StdioModel::with_size(4096);
        assert!(!m.fwrite(2000));
        assert!(!m.fwrite(2096));
        assert!(!m.errno_lost());
        assert!(m.fwrite(1));
        assert!(m.errno_lost());
        // A later byte is buffered again, and fails at exit.
        m.putchars(1);
        assert!(!m.errno_lost());
        // 4097 putchar calls: the last one flushes, fails and is lost.
        let mut m = StdioModel::with_size(4096);
        m.putchars(4096);
        assert!(!m.errno_lost());
        m.putchars(1);
        assert!(m.errno_lost());
        // error() flushes standard output before its message.
        let mut m = StdioModel::with_size(4096);
        m.putchars(10);
        m.fflush();
        assert!(m.errno_lost());
        // Nothing written: no error at all.
        let mut m = StdioModel::with_size(4096);
        m.fflush();
        assert!(!m.fwrite(0));
        assert!(!m.errno_lost());
    }
}
