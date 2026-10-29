//! Low-level input helpers shared by the text tools: borrowing standard input
//! as a `File` (for `fstat`, `lseek`), and asking whether a read would block.

use std::fs::File;
use std::io::{self, Read};
use std::mem::ManuallyDrop;
use std::os::fd::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

/// Standard input as a `File` that is never closed when dropped, so callers
/// can `fstat` it, read its offset and seek it as GNU does with fd 0.
pub(crate) fn stdin_file() -> ManuallyDrop<File> {
    // Safety: fd 0 stays open for the life of the process (or is closed, in
    // which case every operation on it fails with EBADF); ManuallyDrop keeps
    // the File from closing it.
    ManuallyDrop::new(unsafe { File::from_raw_fd(0) })
}

/// Standard output for tools that do their own (large) buffering: each
/// buffer goes out whole through the process's one stdout writer
/// ([`dogfish_core::stdout::write_whole`]), which keeps GNU's write-error
/// reporting, rather than being split at its last newline.
pub(crate) struct RawStdout;

impl io::Write for RawStdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        dogfish_core::stdout::write_whole(buf).map(|()| buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        dogfish_core::stdout::write_whole(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        dogfish_core::stdout::flush()
    }
}

/// Bytes that can be read from `fd` without blocking (`FIONREAD`), or `None`
/// when the descriptor does not support the query.
pub(crate) fn bytes_available(fd: RawFd) -> Option<usize> {
    let mut n: libc::c_int = 0;
    // Safety: FIONREAD writes one int through the pointer.
    let rc = unsafe { libc::ioctl(fd, libc::FIONREAD as _, &mut n) };
    (rc == 0).then(|| usize::try_from(n).unwrap_or(0))
}

/// Standard input was closed when the process started (`tool <&-`); see
/// [`stdin_started_closed`].
static STDIN_CLOSED: AtomicBool = AtomicBool::new(false);

/// Records that fd 0 was closed at start-up. `main` then holds it with a
/// write-only `/dev/null` (so no file the tool opens becomes its standard
/// input and every read fails with EBADF), and [`close_stdin`] fails as
/// closing a closed descriptor does.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn stdin_started_closed() {
    STDIN_CLOSED.store(true, Ordering::Relaxed);
}

/// Closes standard input, as GNU tools do after reading it, so that a closed
/// descriptor (`tool - <&-`) is reported.
pub(crate) fn close_stdin() -> io::Result<()> {
    if STDIN_CLOSED.load(Ordering::Relaxed) {
        return Err(io::Error::from_raw_os_error(libc::EBADF));
    }
    // Safety: closing fd 0 has no memory-safety effect; nothing reads it after.
    if unsafe { libc::close(0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `read`, retried when interrupted by a signal (gnulib `safe_read`).
pub(crate) fn safe_read(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match r.read(buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            res => return res,
        }
    }
}

/// Reads until `buf` is full or end of file (gnulib `full_read`): returns the
/// bytes read and the error that stopped it early, if any.
pub(crate) fn full_read(r: &mut impl Read, buf: &mut [u8]) -> (usize, Option<io::Error>) {
    let mut n = 0;
    while n < buf.len() {
        match safe_read(r, &mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => return (n, Some(e)),
        }
    }
    (n, None)
}
