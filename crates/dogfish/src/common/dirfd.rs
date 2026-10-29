//! Tree walking relative to directory file descriptors, as GNU's `fts` does.
//!
//! The recursive tools (rm, chmod, du) act on each entry with one `*at` call
//! (`fstatat`, `unlinkat`, `fchmodat`, `openat`) on the bare name, relative
//! to its open directory, instead of resolving the full path again for every
//! entry; a directory's names are read in one pass into one buffer, with
//! `readdir`'s file type, so that an entry need not be `stat`ed just to tell
//! a directory. [`WalkPath`] keeps the printable path alongside for
//! messages. A walk keeps at most [`MAX_OPEN_DIRS`] directories open; deeper
//! down, entries are reached by full path.

use std::ffi::{CStr, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// How many directories a walk keeps open at once (one per level).
pub(crate) const MAX_OPEN_DIRS: usize = 128;

fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// A name to act on: relative to an open directory, or (with `AT_FDCWD`) a
/// path from the working directory.
#[derive(Clone, Copy)]
pub(crate) struct At<'a> {
    pub(crate) dir: RawFd,
    pub(crate) name: &'a CStr,
}

impl<'a> At<'a> {
    pub(crate) fn cwd(path: &'a CStr) -> Self {
        Self {
            dir: libc::AT_FDCWD,
            name: path,
        }
    }

    /// `fstatat`; `follow` dereferences a final symlink (`stat` vs `lstat`).
    pub(crate) fn stat(self, follow: bool) -> io::Result<libc::stat> {
        let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
        let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
        cvt(unsafe { libc::fstatat(self.dir, self.name.as_ptr(), st.as_mut_ptr(), flags) })?;
        // Safety: fstatat succeeded, so it filled the buffer.
        Ok(unsafe { st.assume_init() })
    }

    /// Opens a directory to read and to act inside. With `nofollow`, a
    /// symlink is refused rather than followed.
    pub(crate) fn open_dir(self, nofollow: bool) -> io::Result<DirFd> {
        let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOCTTY;
        if nofollow {
            flags |= libc::O_NOFOLLOW;
        }
        let fd = cvt(unsafe { libc::openat(self.dir, self.name.as_ptr(), flags) })?;
        // Safety: openat returned a new descriptor that nothing else owns.
        Ok(DirFd(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// `unlinkat`: removes a non-directory, or with `dir` an empty directory.
    pub(crate) fn unlink(self, dir: bool) -> io::Result<()> {
        let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
        cvt(unsafe { libc::unlinkat(self.dir, self.name.as_ptr(), flags) }).map(drop)
    }

    /// `fchmodat`, following a final symlink as `chmod` does.
    pub(crate) fn chmod(self, mode: u32) -> io::Result<()> {
        cvt(unsafe { libc::fchmodat(self.dir, self.name.as_ptr(), mode as libc::mode_t, 0) })
            .map(drop)
    }

    /// `access (W_OK)` failing with `EACCES`.
    pub(crate) fn write_protected(self) -> bool {
        let denied = unsafe { libc::faccessat(self.dir, self.name.as_ptr(), libc::W_OK, 0) } != 0;
        denied && io::Error::last_os_error().raw_os_error() == Some(libc::EACCES)
    }
}

/// An open directory.
pub(crate) struct DirFd(OwnedFd);

impl DirFd {
    pub(crate) fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// Reads every entry but `.` and `..`, in `readdir` order, into `names`
    /// (cleared first).
    pub(crate) fn read_names(&self, names: &mut Names) -> io::Result<()> {
        names.clear();
        read_entries(self.fd(), names)
    }
}

/// `d_type` values: what readdir says an entry is, without a `stat`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryType {
    Dir,
    Symlink,
    /// Some other known type (regular file, device, fifo, socket).
    Other,
    /// The file system did not say (`DT_UNKNOWN`).
    Unknown,
}

impl EntryType {
    fn from_d_type(t: u8) -> Self {
        match t {
            libc::DT_DIR => Self::Dir,
            libc::DT_LNK => Self::Symlink,
            libc::DT_UNKNOWN => Self::Unknown,
            _ => Self::Other,
        }
    }

    /// The type of a `stat` result.
    #[allow(clippy::unnecessary_cast)] // `mode_t` is `u16` on macOS
    pub(crate) fn of(st: &libc::stat) -> Self {
        Self::from_mode(st.st_mode as u32)
    }

    /// The type a `stat` mode describes.
    #[allow(clippy::unnecessary_cast)] // `mode_t` is `u16` on macOS
    pub(crate) fn from_mode(mode: u32) -> Self {
        match mode & libc::S_IFMT as u32 {
            m if m == libc::S_IFDIR as u32 => Self::Dir,
            m if m == libc::S_IFLNK as u32 => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// The names in one directory, NUL-terminated in one buffer.
#[derive(Default)]
pub(crate) struct Names {
    buf: Vec<u8>,
    /// (start of the name in `buf`, its type, its inode number).
    entries: Vec<(usize, EntryType, u64)>,
}

impl Names {
    fn clear(&mut self) {
        self.buf.clear();
        self.entries.clear();
    }

    fn push(&mut self, name: &[u8], d_type: u8, ino: u64) {
        if name == b"." || name == b".." {
            return;
        }
        self.entries
            .push((self.buf.len(), EntryType::from_d_type(d_type), ino));
        self.buf.extend_from_slice(name);
        self.buf.push(0);
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entry `i`: its name, type and inode number.
    pub(crate) fn get(&self, i: usize) -> (&CStr, EntryType, u64) {
        let (start, kind, ino) = self.entries[i];
        let end = start + self.buf[start..].iter().position(|&b| b == 0).unwrap_or(0);
        // Safety: push wrote the name (which has no NUL) followed by a NUL.
        let name = unsafe { CStr::from_bytes_with_nul_unchecked(&self.buf[start..=end]) };
        (name, kind, ino)
    }
}

/// Linux: `getdents64` straight into a buffer, without a `DIR` stream (which
/// costs a `dup`, `fcntl` and `fstat` per directory).
#[cfg(target_os = "linux")]
fn read_entries(fd: RawFd, names: &mut Names) -> io::Result<()> {
    thread_local! {
        static BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0; 32 * 1024]);
    }
    BUF.with_borrow_mut(|buf| getdents(fd, buf, names))
}

#[cfg(target_os = "linux")]
fn getdents(fd: RawFd, buf: &mut [u8], names: &mut Names) -> io::Result<()> {
    loop {
        let n = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                fd,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Ok(());
        }
        // struct linux_dirent64: d_ino u64, d_off i64, d_reclen u16,
        // d_type u8, then the NUL-terminated name.
        let mut rec = &buf[..n as usize];
        while rec.len() >= 19 {
            let ino = u64::from_ne_bytes(rec[0..8].try_into().unwrap_or_default());
            let reclen = usize::from(u16::from_ne_bytes([rec[16], rec[17]]));
            if reclen < 19 || reclen > rec.len() {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            let raw = &rec[19..reclen];
            let len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            names.push(&raw[..len], rec[18], ino);
            rec = &rec[reclen..];
        }
    }
}

/// Elsewhere: `readdir` on a `DIR` stream over a duplicate of the descriptor.
#[cfg(not(target_os = "linux"))]
fn read_entries(fd: RawFd, names: &mut Names) -> io::Result<()> {
    let dup = cvt(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) })?;
    let dir = unsafe { libc::fdopendir(dup) };
    if dir.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(e);
    }
    let result = loop {
        unsafe { *libc::__error() = 0 };
        let ent = unsafe { libc::readdir(dir) };
        if ent.is_null() {
            let e = io::Error::last_os_error();
            break if e.raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(e)
            };
        }
        // Safety: readdir returned a valid entry, live until the next call.
        let ent = unsafe { &*ent };
        let name = unsafe { CStr::from_ptr(ent.d_name.as_ptr()) };
        names.push(name.to_bytes(), ent.d_type, ent.d_ino);
    };
    unsafe { libc::closedir(dir) };
    result
}

/// A printable path kept in step with a walk: [`WalkPath::push`] appends a
/// name (as `Path::join` would), [`WalkPath::truncate`] takes it off again.
pub(crate) struct WalkPath {
    buf: Vec<u8>,
    /// The same, NUL-terminated, for acting by full path.
    cbuf: Vec<u8>,
}

impl WalkPath {
    pub(crate) fn new(root: &[u8]) -> Self {
        Self {
            buf: root.to_vec(),
            cbuf: Vec::new(),
        }
    }

    pub(crate) fn as_path(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.buf))
    }

    /// Appends `/name` (no slash after one already there); returns the
    /// length to [`WalkPath::truncate`] back to.
    pub(crate) fn push(&mut self, name: &[u8]) -> usize {
        let len = self.buf.len();
        if !self.buf.is_empty() && self.buf.last() != Some(&b'/') {
            self.buf.push(b'/');
        }
        self.buf.extend_from_slice(name);
        len
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        self.buf.truncate(len);
    }

    /// The path as a C string, or `None` if it holds a NUL.
    pub(crate) fn c_path(&mut self) -> Option<&CStr> {
        self.cbuf.clear();
        self.cbuf.extend_from_slice(&self.buf);
        self.cbuf.push(0);
        CStr::from_bytes_with_nul(&self.cbuf).ok()
    }
}
