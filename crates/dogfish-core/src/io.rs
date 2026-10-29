use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

use memmap2::Mmap;

/// 64 KiB aligned buffer size for high-throughput streaming.
pub const DEFAULT_BUF_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMethod {
    ClonefileCoW,
    FicloneCoW,
    StandardCopy,
}

impl CopyMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClonefileCoW => "zero-copy CoW (macOS clonefile)",
            Self::FicloneCoW => "zero-copy CoW (Linux FICLONE)",
            Self::StandardCopy => "buffered kernel copy",
        }
    }
}

#[derive(Debug, Clone)]
pub enum InputSource {
    Stdin,
    File(String),
}

impl InputSource {
    pub fn from_args(files: &[String]) -> Vec<Self> {
        if files.is_empty() {
            vec![Self::Stdin]
        } else {
            files
                .iter()
                .map(|f| {
                    if f == "-" {
                        Self::Stdin
                    } else {
                        Self::File(f.clone())
                    }
                })
                .collect()
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            Self::Stdin => "-",
            Self::File(p) => p.as_str(),
        }
    }
}

/// High-speed reader that automatically uses `mmap` on regular files when beneficial,
/// falling back to a 64 KiB `BufReader`.
pub enum FastReader {
    Mapped(Mmap),
    Buffered(BufReader<Box<dyn Read>>),
}

impl FastReader {
    pub fn open(source: &InputSource) -> io::Result<Self> {
        match source {
            InputSource::Stdin => Ok(Self::Buffered(BufReader::with_capacity(
                DEFAULT_BUF_SIZE,
                Box::new(io::stdin()),
            ))),
            InputSource::File(path) => {
                let file = File::open(path)?;
                let meta = file.metadata()?;
                if meta.is_file() && meta.len() > 0 && meta.len() <= (1 << 34) {
                    // Safety: read-only memory map for fast sequential scan
                    if let Ok(mmap) = unsafe { Mmap::map(&file) } {
                        #[cfg(unix)]
                        unsafe {
                            libc::madvise(
                                mmap.as_ptr() as *mut libc::c_void,
                                mmap.len(),
                                libc::MADV_SEQUENTIAL,
                            );
                        }
                        return Ok(Self::Mapped(mmap));
                    }
                }
                Ok(Self::Buffered(BufReader::with_capacity(
                    DEFAULT_BUF_SIZE,
                    Box::new(file),
                )))
            }
        }
    }

    /// Reads the entire input as a byte slice (zero-copy if `mmap` succeeded).
    pub fn into_bytes(self) -> io::Result<Vec<u8>> {
        match self {
            Self::Mapped(mmap) => Ok(mmap.to_vec()),
            Self::Buffered(mut r) => {
                let mut buf = Vec::new();
                r.read_to_end(&mut buf)?;
                Ok(buf)
            }
        }
    }

    /// Invokes a callback over chunks or the full mapped slice without extra heap copies.
    pub fn with_bytes<F, R>(&mut self, f: F) -> io::Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self {
            Self::Mapped(mmap) => Ok(f(&mmap[..])),
            Self::Buffered(r) => {
                let mut buf = Vec::new();
                r.read_to_end(&mut buf)?;
                Ok(f(&buf))
            }
        }
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self, Self::Mapped(_))
    }

    pub fn into_bufread(self) -> Box<dyn BufRead> {
        match self {
            Self::Mapped(mmap) => Box::new(io::Cursor::new(mmap)),
            Self::Buffered(r) => Box::new(r),
        }
    }
}

/// Returns true when `a` and `b` resolve to the same inode on the same device
/// (same path, `./` aliases, symlinks, or hard links).
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

/// Copies a file using macOS APFS `clonefile` (zero-copy copy-on-write) when available,
/// transparently falling back to `std::fs::copy`.
///
/// Refuses to copy a file onto itself: every strategy here truncates or unlinks
/// the destination first, which would destroy the source.
pub fn copy_file_fast(src: &Path, dst: &Path) -> io::Result<(u64, CopyMethod)> {
    if same_file(src, dst) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "'{}' and '{}' are the same file",
                src.display(),
                dst.display()
            ),
        ));
    }

    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        if let (Ok(c_src), Ok(c_dst)) = (
            CString::new(src.as_os_str().as_bytes()),
            CString::new(dst.as_os_str().as_bytes()),
        ) {
            // If destination already exists, unlink first so clonefile can succeed
            if dst.exists() {
                let _ = fs::remove_file(dst);
            }
            let ret = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
            if ret == 0 {
                let len = fs::metadata(dst).map(|m| m.len()).unwrap_or(0);
                return Ok((len, CopyMethod::ClonefileCoW));
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        // FICLONE ioctl (0x40049409) for instant Btrfs / XFS / bcachefs Copy-on-Write reflink.
        // One open of each file serves both the reflink attempt and the fallback.
        const FICLONE: libc::c_ulong = 0x40049409;
        let src_file = File::open(src)?;
        let meta = src_file.metadata()?;
        let dst_file = File::create(dst)?;
        let ret = unsafe { libc::ioctl(dst_file.as_raw_fd(), FICLONE as _, src_file.as_raw_fd()) };
        let (bytes, method) = if ret == 0 {
            (meta.len(), CopyMethod::FicloneCoW)
        } else {
            // std specializes File-to-File `io::copy` to copy_file_range(2) / sendfile(2).
            (
                io::copy(&mut &src_file, &mut &dst_file)?,
                CopyMethod::StandardCopy,
            )
        };
        dst_file.set_permissions(meta.permissions())?;
        Ok((bytes, method))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let bytes = fs::copy(src, dst)?;
        Ok((bytes, CopyMethod::StandardCopy))
    }
}

/// Recursively copies `src` to `dst`, recreating symlinks rather than following them.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ty = meta.file_type();
    if ty.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)
    } else if ty.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        fs::set_permissions(dst, meta.permissions())
    } else {
        copy_file_fast(src, dst).map(|_| ())
    }
}

/// Renames `src` to `dst`, falling back to copy + remove when they live on
/// different filesystems (`EXDEV`).
pub fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    match fs::rename(src, dst) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_tree(src, dst)?;
            if fs::symlink_metadata(src)?.is_dir() {
                fs::remove_dir_all(src)
            } else {
                fs::remove_file(src)
            }
        }
        res => res,
    }
}

/// Resets SIGPIPE to default OS behavior or ignores BrokenPipe gracefully.
pub fn install_sigpipe_handler() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Message written by the `SIGBUS` handler; set once by [`install_sigbus_handler`].
static SIGBUS_MSG: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

extern "C" fn on_sigbus(_: libc::c_int) {
    // Async-signal-safe only: write(2) a preformatted message, then _exit(2).
    if let Some(msg) = SIGBUS_MSG.get() {
        unsafe {
            libc::write(libc::STDERR_FILENO, msg.as_ptr().cast(), msg.len());
        }
    }
    unsafe { libc::_exit(1) }
}

/// Turns `SIGBUS` into a clean `tool: ...` error and exit status 1.
///
/// [`FastReader`] memory-maps regular files; if another process truncates a file
/// while it is mapped, touching the vanished pages raises `SIGBUS`, which would
/// otherwise kill the process with no explanation.
pub fn install_sigbus_handler(tool: &str) {
    let _ =
        SIGBUS_MSG.set(format!("{tool}: input file was truncated while being read\n").into_bytes());
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigbus as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGBUS, &action, std::ptr::null_mut());
    }
}

/// Helper to treat `ErrorKind::BrokenPipe` as a clean exit (`Ok(())`).
pub fn ignore_broken_pipe(res: io::Result<()>) -> io::Result<()> {
    match res {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e),
    }
}
