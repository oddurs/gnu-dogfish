//! Setting access and modification times (cp).

use std::io;
use std::path::Path;

use crate::common::path::c_path;

/// A time to set with `utimensat`: seconds and nanoseconds.
#[derive(Clone, Copy)]
pub(crate) enum Stamp {
    At(i64, u32),
}

pub(crate) fn set_file_times(
    path: &Path,
    atime: Stamp,
    mtime: Stamp,
    follow: bool,
) -> io::Result<()> {
    let spec = |s: Stamp| match s {
        Stamp::At(sec, nsec) => libc::timespec {
            tv_sec: sec,
            tv_nsec: nsec.into(),
        },
    };
    let times = [spec(atime), spec(mtime)];
    let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
    let c = c_path(path)?;
    // SAFETY: `c` is a NUL-terminated path and `times` holds two timespecs.
    if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
