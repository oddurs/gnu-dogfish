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
                if meta.is_file() && meta.len() > 0 && meta.len() <= (1 << 33) {
                    // Safety: read-only memory map for fast sequential scan
                    if let Ok(mmap) = unsafe { Mmap::map(&file) } {
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

/// Copies a file using macOS APFS `clonefile` (zero-copy copy-on-write) when available,
/// transparently falling back to `std::fs::copy`.
pub fn copy_file_fast(src: &Path, dst: &Path) -> io::Result<(u64, CopyMethod)> {
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
        // FICLONE ioctl (0x40049409) for instant Btrfs / XFS / bcachefs Copy-on-Write reflink
        const FICLONE: libc::c_ulong = 0x40049409;
        if let (Ok(src_file), Ok(dst_file)) = (File::open(src), File::create(dst)) {
            let ret = unsafe { libc::ioctl(dst_file.as_raw_fd(), FICLONE as _, src_file.as_raw_fd()) };
            if ret == 0 {
                if let Ok(meta) = src_file.metadata() {
                    let _ = dst_file.set_permissions(meta.permissions());
                    return Ok((meta.len(), CopyMethod::FicloneCoW));
                }
            }
        }
    }

    let bytes = fs::copy(src, dst)?;
    Ok((bytes, CopyMethod::StandardCopy))
}

/// Resets SIGPIPE to default OS behavior or ignores BrokenPipe gracefully.
pub fn install_sigpipe_handler() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
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
