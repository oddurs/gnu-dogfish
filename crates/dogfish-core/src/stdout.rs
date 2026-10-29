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
}

static STATE: Mutex<State> = Mutex::new(State {
    closed: false,
    buf: Vec::new(),
    error: None,
    reported: false,
});

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

    fn put(&mut self, data: &[u8]) -> io::Result<()> {
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
/// reported yet (GNU's `close_stdout`, which the caller prints). Writing
/// nothing to a closed descriptor is no error, as in GNU.
pub fn close() -> Option<io::Error> {
    let mut st = state();
    if st.error.is_none() {
        let _ = st.flush_buf();
    }
    if st.reported {
        return None;
    }
    st.reported = true;
    st.sticky()
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
        };
        assert!(st.put(b"").is_ok());
        assert_eq!(st.put(b"x").unwrap_err().raw_os_error(), Some(libc::EBADF));
    }
}
