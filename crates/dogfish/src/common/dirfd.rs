//! Tree walking relative to directory file descriptors, as GNU's `fts` does.
//!
//! The recursive tools (rm, chmod, du) act on each entry with one `*at` call
//! (`fstatat`, `unlinkat`, `fchmodat`, `openat`) on the bare name, relative
//! to its open directory, instead of resolving the full path again for every
//! entry; a directory's names are read in one pass into one buffer, with
//! `readdir`'s file type, so that an entry need not be `stat`ed just to tell
//! a directory. [`WalkPath`] keeps the printable path alongside for
//! messages. A walk keeps at most [`MAX_OPEN_DIRS`] directories open. Deeper
//! down, it lets go of a directory while reading a subdirectory and gets it
//! back through `..` ([`Parent`], [`DirFd::open_parent`]), so no depth needs
//! a path the kernel would have to resolve in one piece (`PATH_MAX`). A walk
//! that follows every symlink cannot climb back that way, as a directory
//! reached through a symlink is not a child of its `..`: `du -L` reaches
//! entries that deep by full path ([`WalkPath::c_path`]), and `chown -RL`
//! walks by full path throughout, as GNU's `FTS_LOGICAL` does.

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
        // SAFETY: `name` is a NUL-terminated C string and `st` a writable
        // `stat`; it is read only after fstatat succeeded, which filled it.
        unsafe {
            cvt(libc::fstatat(
                self.dir,
                self.name.as_ptr(),
                st.as_mut_ptr(),
                flags,
            ))?;
            Ok(st.assume_init())
        }
    }

    /// Opens a directory to read and to act inside. With `nofollow`, a
    /// symlink is refused rather than followed.
    pub(crate) fn open_dir(self, nofollow: bool) -> io::Result<DirFd> {
        let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOCTTY;
        if nofollow {
            flags |= libc::O_NOFOLLOW;
        }
        // SAFETY: `name` is a NUL-terminated C string; on success openat
        // returns a new descriptor that nothing else owns.
        unsafe {
            let fd = cvt(libc::openat(self.dir, self.name.as_ptr(), flags))?;
            Ok(DirFd(OwnedFd::from_raw_fd(fd)))
        }
    }

    /// `unlinkat`: removes a non-directory, or with `dir` an empty directory.
    pub(crate) fn unlink(self, dir: bool) -> io::Result<()> {
        let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: `name` is a NUL-terminated C string.
        cvt(unsafe { libc::unlinkat(self.dir, self.name.as_ptr(), flags) }).map(drop)
    }

    /// `fchmodat`, following a final symlink as `chmod` does.
    pub(crate) fn chmod(self, mode: u32) -> io::Result<()> {
        // SAFETY: `name` is a NUL-terminated C string.
        cvt(unsafe { libc::fchmodat(self.dir, self.name.as_ptr(), mode as libc::mode_t, 0) })
            .map(drop)
    }

    /// `fchownat`; `follow` changes a final symlink's referent (`chown`
    /// vs `lchown`). `None` leaves that id as it is.
    pub(crate) fn chown(self, uid: Option<u32>, gid: Option<u32>, follow: bool) -> io::Result<()> {
        let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
        // `-1` (all ones) is "unchanged".
        let (uid, gid) = (uid.unwrap_or(u32::MAX), gid.unwrap_or(u32::MAX));
        // SAFETY: `name` is a NUL-terminated C string.
        cvt(unsafe { libc::fchownat(self.dir, self.name.as_ptr(), uid, gid, flags) }).map(drop)
    }

    /// `access (W_OK)` failing with `EACCES`.
    pub(crate) fn write_protected(self) -> bool {
        // SAFETY: `name` is a NUL-terminated C string.
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

    /// This directory's (device, inode), from `fstat`.
    pub(crate) fn id(&self) -> io::Result<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        // fstat through a `File` on a duplicate of the descriptor.
        let meta = std::fs::File::from(self.0.try_clone()?).metadata()?;
        Ok((meta.dev(), meta.ino()))
    }

    /// Opens this directory's parent through `..`; it must be the directory
    /// `expect` names (device, inode), or this fails with `ENOENT` as GNU's
    /// fts does when a directory was moved during the walk. Deeper than
    /// [`MAX_OPEN_DIRS`], a walk lets go of each directory while it reads a
    /// subdirectory and climbs back up this way, so it holds only a few
    /// descriptors and never needs a path longer than one name.
    pub(crate) fn open_parent(&self, expect: (u64, u64)) -> io::Result<DirFd> {
        use std::os::unix::fs::MetadataExt;
        let parent = At {
            dir: self.fd(),
            name: c"..",
        }
        .open_dir(true)?;
        // fstat through `File`, then take the descriptor back.
        let file = std::fs::File::from(parent.0);
        let meta = file.metadata()?;
        if (meta.dev(), meta.ino()) != expect {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        }
        Ok(DirFd(OwnedFd::from(file)))
    }
}

/// The directory a walk acts on entries in: the working directory (operands
/// are paths from it) or an open directory, which a deep walk lets go of
/// while it works in a subdirectory, and gets back through that
/// subdirectory's `..`.
pub(crate) enum Parent {
    Cwd,
    Dir {
        /// `None` while let go of.
        fd: Option<DirFd>,
        /// (device, inode), to check the directory got back through `..`;
        /// known when a subdirectory may let go of it.
        id: Option<(u64, u64)>,
    },
}

impl Parent {
    /// The open directory `dir`, `depth` levels below an operand, as the
    /// directory of its entries. Its identity is taken when subdirectories one
    /// level further down will let go of it ([`Parent::release_deep`]).
    pub(crate) fn dir(dir: DirFd, depth: usize) -> io::Result<Self> {
        let id = if depth + 1 >= MAX_OPEN_DIRS {
            Some(dir.id()?)
        } else {
            None
        };
        Ok(Parent::Dir { fd: Some(dir), id })
    }

    /// The open directory, if it is one and is open.
    pub(crate) fn open_dir(&self) -> Option<&DirFd> {
        match self {
            Parent::Dir { fd, .. } => fd.as_ref(),
            Parent::Cwd => None,
        }
    }

    /// Lets go of this directory while its subdirectory `depth` levels below
    /// an operand is walked, if that is deep enough; true if it did, when the
    /// directory must be got back with [`Parent::reacquire`].
    pub(crate) fn release_deep(&mut self, depth: usize) -> bool {
        depth >= MAX_OPEN_DIRS && self.release()
    }

    /// How to act on `name` in this directory.
    pub(crate) fn at<'a>(&self, name: &'a CStr) -> At<'a> {
        match self {
            Parent::Cwd => At::cwd(name),
            Parent::Dir { fd: Some(fd), .. } => At { dir: fd.fd(), name },
            // Not reached: a directory is got back before it is used again.
            // EBADF says so if it were.
            Parent::Dir { fd: None, .. } => At { dir: -1, name },
        }
    }

    /// Closes the directory for now; true if it was open.
    pub(crate) fn release(&mut self) -> bool {
        match self {
            Parent::Dir { fd, id: Some(_) } => fd.take().is_some(),
            _ => false,
        }
    }

    /// Opens the directory again, as the `..` of `child`.
    pub(crate) fn reacquire(&mut self, child: &DirFd) -> io::Result<()> {
        if let Parent::Dir { fd, id: Some(id) } = self {
            *fd = Some(child.open_parent(*id)?);
        }
        Ok(())
    }
}

/// A walk that had to give up altogether: a directory it let go of could not
/// be got back (it was moved meanwhile). GNU's `fts_read` fails then, and the
/// tool stops, leaving any further operands alone.
pub(crate) struct Lost;

/// How GNU's `fts_open` names a command-line operand: two or more trailing
/// slashes become one (`a///` is `a/`, `///` is `/`).
pub(crate) fn fts_root_name(arg: &[u8]) -> &[u8] {
    let mut len = arg.len();
    if len > 2 && arg[len - 1] == b'/' {
        while len > 1 && arg[len - 2] == b'/' {
            len -= 1;
        }
    }
    &arg[..len]
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
        // `push` wrote the name followed by a NUL.
        let name = CStr::from_bytes_until_nul(&self.buf[start..]).unwrap_or_default();
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
        // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
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
    // SAFETY: descriptor only: the duplicate is handed to fdopendir, which
    // owns it from then on, or closed here once if fdopendir fails.
    let dir = unsafe {
        let dup = cvt(libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0))?;
        let dir = libc::fdopendir(dup);
        if dir.is_null() {
            let e = io::Error::last_os_error();
            libc::close(dup);
            return Err(e);
        }
        dir
    };
    let result = loop {
        // SAFETY: `dir` is an open stream; a non-null entry stays valid until
        // the next readdir, and its NUL-terminated name is copied before that.
        let done = unsafe {
            *libc::__error() = 0;
            let ent = libc::readdir(dir);
            if ent.is_null() {
                true
            } else {
                let name = CStr::from_ptr((*ent).d_name.as_ptr());
                names.push(name.to_bytes(), (*ent).d_type, (*ent).d_ino);
                false
            }
        };
        if done {
            let e = io::Error::last_os_error();
            break if e.raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(e)
            };
        }
    };
    // SAFETY: `dir` is open and is closed exactly once.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_keeps_one_trailing_slash() {
        for (arg, name) in [
            (&b"a///"[..], &b"a/"[..]),
            (b"a/", b"a/"),
            (b"a", b"a"),
            (b"//", b"//"),
            (b"///", b"/"),
            (b"a//b//", b"a//b/"),
            (b"", b""),
        ] {
            assert_eq!(fts_root_name(arg), name, "{arg:?}");
        }
    }

    #[test]
    fn climbs_back_to_the_parent() {
        let dir = std::env::temp_dir().join(format!("dogfish-dirfd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).unwrap();
        let top = At::cwd(&c).open_dir(false).unwrap();
        let sub = At {
            dir: top.fd(),
            name: c"sub",
        }
        .open_dir(true)
        .unwrap();
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&dir).unwrap();
        assert!(sub.open_parent((meta.dev(), meta.ino())).is_ok());
        let wrong = sub.open_parent((meta.dev(), meta.ino() + 1));
        assert_eq!(
            wrong.err().and_then(|e| e.raw_os_error()),
            Some(libc::ENOENT)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
