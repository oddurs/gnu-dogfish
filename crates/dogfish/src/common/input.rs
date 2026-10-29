//! Low-level input helpers shared by the text tools: borrowing standard input
//! as a `File` (for `fstat`, `lseek`), and asking whether a read would block.

use std::fs::File;
use std::io::{self, Read};
use std::mem::ManuallyDrop;
use std::os::fd::{FromRawFd, RawFd};

/// Standard input as a `File` that is never closed when dropped, so callers
/// can `fstat` it, read its offset and seek it as GNU does with fd 0.
pub(crate) fn stdin_file() -> ManuallyDrop<File> {
    // Safety: fd 0 stays open for the life of the process (or is closed, in
    // which case every operation on it fails with EBADF); ManuallyDrop keeps
    // the File from closing it.
    ManuallyDrop::new(unsafe { File::from_raw_fd(0) })
}

/// Bytes that can be read from `fd` without blocking (`FIONREAD`), or `None`
/// when the descriptor does not support the query.
pub(crate) fn bytes_available(fd: RawFd) -> Option<usize> {
    let mut n: libc::c_int = 0;
    // Safety: FIONREAD writes one int through the pointer.
    let rc = unsafe { libc::ioctl(fd, libc::FIONREAD as _, &mut n) };
    (rc == 0).then(|| usize::try_from(n).unwrap_or(0))
}

/// Undoes the Rust runtime's repair of a closed standard input.
///
/// Before `main`, std reopens a closed fd 0 as `/dev/null` (read-write), so
/// `tac - <&-` would read an empty input where GNU reports `Bad file
/// descriptor`. A read-write `/dev/null` on fd 0 is taken to be that repair
/// (a shell's `< /dev/null` is read-only) and fd 0 is closed again.
pub(crate) fn restore_closed_stdin() {
    // Safety: fcntl and fstat only read descriptor state.
    let flags = unsafe { libc::fcntl(0, libc::F_GETFL) };
    if flags < 0 || flags & libc::O_ACCMODE != libc::O_RDWR {
        return;
    }
    let stdin = stdin_file();
    let (Ok(ours), Ok(null)) = (stdin.metadata(), std::fs::metadata("/dev/null")) else {
        return;
    };
    use std::os::unix::fs::MetadataExt;
    if ours.dev() == null.dev() && ours.ino() == null.ino() {
        // Safety: nothing has read fd 0 yet; std's Stdin reports EBADF.
        unsafe { libc::close(0) };
    }
}

/// Closes standard input, as GNU tools do after reading it, so that a closed
/// descriptor (`tool - <&-`) is reported.
pub(crate) fn close_stdin() -> io::Result<()> {
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
