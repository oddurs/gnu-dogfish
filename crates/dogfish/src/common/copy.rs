//! Copying files and directories for cp and mv: a port of GNU's `copy.c`
//! (`copy_internal`, `copy_reg`, `copy_dir`, sparse copying), `cp-hash.c`
//! and gnulib's `force-link.c`, recorded to the UndoJournal.
//!
//! Undo: for each command-line operand, whatever the operation destroys or
//! rewrites at the destination is first saved in the journal's trash (moved
//! there when it is replaced, copied there when it is written into), and the
//! operation is recorded once it is done, so `dogfish undo` can reverse it.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};

use dogfish_core::outln;
use dogfish_core::{
    CopyMethod, DogfishContext, JournalAction, JournalEntry, Take, UndoJournal, crash_point,
    gnu_io_error, move_path,
};

use crate::common::backup::{Backup, BackupMode, make_backup};
use crate::common::mode::{CHMOD_MODE_BITS, S_IRWXUGO, S_ISGID, S_ISUID, S_ISVTX, strmode9};
use crate::common::path::{
    Deref, absolute, c_path, dir_name, dir_names, last_component, rename_noreplace, same_inode,
    same_name, stable_path,
};
use crate::common::prompt::ask;
use crate::common::quote::{qa, qf, quote_locale};
use crate::common::targets::{SameFileOpts, same_file_ok};

const S_IRWXU: u32 = 0o700;
const S_IRWXG: u32 = 0o070;
const S_IRWXO: u32 = 0o007;
const S_IWGRP: u32 = 0o020;
const S_IWOTH: u32 = 0o002;
const MODE_RW_UGO: u32 = 0o666;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFIFO: u32 = 0o010000;
const S_IFCHR: u32 = 0o020000;
const S_IFBLK: u32 = 0o060000;
const S_IFSOCK: u32 = 0o140000;
/// gnulib's `IO_BUFSIZE`.
const IO_BUFSIZE: u64 = 128 * 1024;

fn is_type(mode: u32, t: u32) -> bool {
    mode & S_IFMT == t
}

/// GNU's `enum Interactive`: what to do about an existing destination.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interactive {
    /// `-f`.
    AlwaysYes,
    /// `-n`: skip and fail.
    AlwaysNo,
    /// `--update=none`: skip and succeed.
    AlwaysSkip,
    /// `-i`.
    AskUser,
    Unspecified,
}

/// `--sparse`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sparse {
    Never,
    Auto,
    Always,
}

/// `--reflink`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reflink {
    Never,
    Auto,
    Always,
}

/// GNU's `struct cp_options`, as far as Dogfish supports it.
pub(crate) struct CopyOptions {
    pub(crate) backup: BackupMode,
    pub(crate) suffix: String,
    pub(crate) dereference: Deref,
    pub(crate) interactive: Interactive,
    pub(crate) sparse: Sparse,
    pub(crate) reflink: Reflink,
    pub(crate) copy_as_regular: bool,
    pub(crate) unlink_dest_before_opening: bool,
    pub(crate) unlink_dest_after_failed_open: bool,
    pub(crate) hard_link: bool,
    pub(crate) symbolic_link: bool,
    pub(crate) move_mode: bool,
    pub(crate) no_copy: bool,
    pub(crate) one_file_system: bool,
    pub(crate) preserve_ownership: bool,
    pub(crate) preserve_mode: bool,
    pub(crate) preserve_timestamps: bool,
    pub(crate) explicit_no_preserve_mode: bool,
    pub(crate) preserve_links: bool,
    pub(crate) data_copy_required: bool,
    pub(crate) require_preserve: bool,
    pub(crate) recursive: bool,
    pub(crate) update: bool,
    pub(crate) verbose: bool,
    pub(crate) stdin_tty: bool,
    pub(crate) open_dangling_dest_symlink: bool,
    /// `--keep-directory-symlink` (from coreutils 9.5): a directory is copied
    /// into what a destination symlink to a directory points to.
    pub(crate) keep_directory_symlink: bool,
    /// mv: this is the last operand.
    pub(crate) last_file: bool,
    /// mv: `Some(0)` when the source was already renamed, `Some(errno)` when
    /// that failed, `None` when no rename was tried.
    pub(crate) rename_errno: Option<i32>,
}

impl CopyOptions {
    /// `cp_options_default` with cp's settings (`cp_option_init`).
    pub(crate) fn for_cp() -> Self {
        CopyOptions {
            backup: BackupMode::None,
            suffix: "~".to_string(),
            dereference: Deref::Always,
            interactive: Interactive::Unspecified,
            sparse: Sparse::Auto,
            reflink: Reflink::Auto,
            copy_as_regular: true,
            unlink_dest_before_opening: false,
            unlink_dest_after_failed_open: false,
            hard_link: false,
            symbolic_link: false,
            move_mode: false,
            no_copy: false,
            one_file_system: false,
            preserve_ownership: false,
            preserve_mode: false,
            preserve_timestamps: false,
            explicit_no_preserve_mode: false,
            preserve_links: false,
            data_copy_required: true,
            require_preserve: false,
            recursive: false,
            update: false,
            verbose: false,
            stdin_tty: false,
            open_dangling_dest_symlink: std::env::var_os("POSIXLY_CORRECT").is_some(),
            keep_directory_symlink: false,
            last_file: false,
            rename_errno: None,
        }
    }

    /// mv's settings (mv.c's `cp_option_init`).
    pub(crate) fn for_mv(stdin_tty: bool) -> Self {
        CopyOptions {
            copy_as_regular: false,
            dereference: Deref::Never,
            move_mode: true,
            preserve_ownership: true,
            preserve_links: true,
            preserve_mode: true,
            preserve_timestamps: true,
            recursive: true,
            stdin_tty,
            open_dangling_dest_symlink: false,
            ..Self::for_cp()
        }
    }
}

/// What [`Copier::copy`] reports besides success.
#[derive(Default)]
pub(crate) struct Outcome {
    pub(crate) ok: bool,
    /// The source is the destination or one of its parents.
    pub(crate) copy_into_self: bool,
    /// mv: the source was renamed (or deliberately left alone), so it must
    /// not be removed.
    pub(crate) rename_succeeded: bool,
    /// mv: the source was renamed to the destination.
    pub(crate) renamed: bool,
}

/// What `dogfish undo` needs to reverse one command-line operand.
struct UndoPlan {
    /// The command-line source.
    src: PathBuf,
    /// Its device and inode before the operation.
    src_inode: Option<(u64, u64)>,
    /// The path the operation created or rewrote.
    created: PathBuf,
    /// Where the journal saved what `created` held before.
    backup: Option<PathBuf>,
    /// Whether `created` existed before the operation.
    existed: bool,
    /// The backup is a copy of `created`, which the operation writes into.
    in_place: bool,
    /// mv across file systems: the source is completely copied.
    copied: bool,
    /// mv across file systems: the hard link kept to the source's inode
    /// (see [`UndoJournal::keep_link`]).
    src_link: Option<PathBuf>,
    /// The pending journal entry (write-ahead), when it could be recorded.
    entry: Option<JournalEntry>,
}

/// GNU's file sets (`dest_info`, `src_info`): files by device and inode,
/// with the names they were seen under.
#[derive(Default)]
struct FileSet(HashMap<(u64, u64), Vec<PathBuf>>);

impl FileSet {
    fn seen(&self, name: &Path, sb: &fs::Metadata) -> bool {
        self.0
            .get(&(sb.dev(), sb.ino()))
            .is_some_and(|names| names.iter().any(|n| same_name(n, name)))
    }

    fn record(&mut self, name: &Path, sb: &fs::Metadata) {
        if !self.seen(name, sb) {
            self.0
                .entry((sb.dev(), sb.ino()))
                .or_default()
                .push(name.to_path_buf());
        }
    }
}

/// State for one cp or mv invocation.
pub(crate) struct Copier<'a> {
    pub(crate) x: CopyOptions,
    tool: &'static str,
    ctx: &'a DogfishContext,
    pub(crate) journal: Option<UndoJournal>,
    pub(crate) umask: u32,
    /// Running as root: chown and chmod failures are reported.
    privileged: bool,
    /// cp-hash.c: the destination made from each source inode.
    src_to_dest: HashMap<(u64, u64), PathBuf>,
    dest_info: Option<FileSet>,
    src_info: Option<FileSet>,
    top_level_src: PathBuf,
    top_level_dst: PathBuf,
    plan: Option<UndoPlan>,
    /// The `--backup` renames made so far, by backup name.
    backups: HashMap<PathBuf, Backup>,
    /// The target directory operand as given, and opened (resolved): GNU
    /// reaches every destination under it through that descriptor.
    target_dir: Option<(PathBuf, PathBuf)>,
    /// mv: the top-level source has been renamed.
    renamed: bool,
    /// A pending journal entry the next [`Self::copy`] continues.
    adopted: Option<JournalEntry>,
    /// How the data of the last regular file was copied (shown on a terminal).
    method: CopyMethod,
}

/// Arguments of one `copy_internal` level that change while recursing.
#[derive(Clone, Copy)]
struct Level<'p> {
    dereference: Deref,
    command_line_arg: bool,
    parent: Option<&'p fs::Metadata>,
}

fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(0)
}

fn os_err(errno: i32) -> io::Error {
    io::Error::from_raw_os_error(errno)
}

fn cvt(r: libc::c_int) -> io::Result<()> {
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn mode_t(mode: u32) -> libc::mode_t {
    // `mode_t` is narrower on macOS; every value passed fits.
    #[allow(clippy::unnecessary_cast)]
    let m = mode as libc::mode_t;
    m
}

fn open_path(p: &Path, flags: libc::c_int, mode: u32) -> io::Result<File> {
    crash_point();
    let c = c_path(p)?;
    // SAFETY: `c` is a NUL-terminated path that outlives the call; open(2) reads
    // nothing else through pointers.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            flags | libc::O_CLOEXEC,
            libc::c_uint::from(mode_t(mode)),
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: `fd` is a descriptor open(2) just returned, owned by nothing else.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn unlink_path(p: &Path, dir: bool) -> io::Result<()> {
    crash_point();
    let c = c_path(p)?;
    let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    cvt(unsafe { libc::unlinkat(libc::AT_FDCWD, c.as_ptr(), flags) })
}

fn link_path(src: &Path, dst: &Path, follow: bool) -> io::Result<()> {
    let (s, d) = (c_path(src)?, c_path(dst)?);
    let flags = if follow { libc::AT_SYMLINK_FOLLOW } else { 0 };
    // SAFETY: `s` and `d` are NUL-terminated paths that outlive the call.
    cvt(unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            s.as_ptr(),
            libc::AT_FDCWD,
            d.as_ptr(),
            flags,
        )
    })
}

fn chmod_path(p: &Path, mode: u32) -> io::Result<()> {
    let c = c_path(p)?;
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    cvt(unsafe { libc::chmod(c.as_ptr(), mode_t(mode & CHMOD_MODE_BITS)) })
}

fn fchmod_file(f: &File, mode: u32) -> io::Result<()> {
    // SAFETY: fchmod takes an open descriptor (owned by `f`) and no pointers.
    cvt(unsafe { libc::fchmod(f.as_raw_fd(), mode_t(mode & CHMOD_MODE_BITS)) })
}

/// The file mode bits of `p` (or the file open as `fd`).
fn chmod_either(fd: Option<&File>, p: &Path, mode: u32) -> io::Result<()> {
    match fd {
        Some(f) => fchmod_file(f, mode),
        None => chmod_path(p, mode),
    }
}

fn timespec(sec: i64, nsec: i64) -> libc::timespec {
    libc::timespec {
        tv_sec: sec as libc::time_t,
        tv_nsec: nsec as _,
    }
}

fn src_times(sb: &fs::Metadata) -> [libc::timespec; 2] {
    [
        timespec(sb.atime(), sb.atime_nsec()),
        timespec(sb.mtime(), sb.mtime_nsec()),
    ]
}

fn utimens_path(p: &Path, times: &[libc::timespec; 2], nofollow: bool) -> io::Result<()> {
    let c = c_path(p)?;
    let flags = if nofollow {
        libc::AT_SYMLINK_NOFOLLOW
    } else {
        0
    };
    // SAFETY: `c` is a NUL-terminated path and `times` an array of two
    // timespecs, as utimensat(2) requires; both outlive the call.
    cvt(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) })
}

fn lchown_ids(fd: Option<&File>, p: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
    let u = uid.unwrap_or(u32::MAX) as libc::uid_t;
    let g = gid.unwrap_or(u32::MAX) as libc::gid_t;
    match fd {
        // SAFETY: fchown takes an open descriptor (owned by `f`) and no pointers.
        Some(f) => cvt(unsafe { libc::fchown(f.as_raw_fd(), u, g) }),
        None => {
            let c = c_path(p)?;
            // SAFETY: `c` is a NUL-terminated path that outlives the call.
            cvt(unsafe { libc::lchown(c.as_ptr(), u, g) })
        }
    }
}

fn mknod_path(p: &Path, mode: u32, dev: u64) -> io::Result<()> {
    let c = c_path(p)?;
    let ty = mode & S_IFMT;
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let m = (ty | (mode & CHMOD_MODE_BITS)) as libc::mode_t;
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let d = dev as libc::dev_t;
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    cvt(unsafe { libc::mknod(c.as_ptr(), m, d) })
}

fn mkfifo_path(p: &Path, mode: u32) -> io::Result<()> {
    let c = c_path(p)?;
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    cvt(unsafe { libc::mkfifo(c.as_ptr(), mode_t(mode)) })
}

/// The process's effective IDs may write `p` (faccessat with `AT_EACCESS`).
fn eaccess_ok(p: &Path, how: libc::c_int) -> bool {
    let Ok(c) = c_path(p) else { return false };
    // SAFETY: `c` is a NUL-terminated path that outlives the call.
    unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), how, libc::AT_EACCESS) == 0 }
}

/// gnulib `dot_or_dotdot`.
fn dot_or_dotdot(name: &[u8]) -> bool {
    if name.first() != Some(&b'.') {
        return false;
    }
    let sep = name
        .get(1 + usize::from(name.get(1) == Some(&b'.')))
        .copied();
    sep.is_none_or(|c| c == b'/')
}

/// A temporary name in the directory of `dst` (gnulib `samedir_template` and
/// `try_tempname`): `dir/CuXXXXXX` with six random letters and digits.
fn samedir_temp(dst: &Path, attempt: u64) -> PathBuf {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let bytes = dst.as_os_str().as_bytes();
    let dir_len = bytes.len() - last_component(bytes).len();
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
        ^ (u64::from(std::process::id()) << 32)
        ^ attempt.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut name = bytes[..dir_len].to_vec();
    name.extend_from_slice(b"Cu");
    for _ in 0..6 {
        // xorshift64*
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        let r = seed.wrapping_mul(0x2545_F491_4F6C_DD1D);
        name.push(CHARS[(r >> 58) as usize % CHARS.len()]);
    }
    PathBuf::from(OsStr::from_bytes(&name))
}

/// What a forced link replaced.
#[derive(PartialEq, Eq)]
pub(crate) enum Linked {
    /// The destination did not exist.
    New,
    /// An existing destination was replaced.
    Replaced,
}

/// Creates a link at `dst` with `make`, retrying under temporary names.
fn with_temp_name(dst: &Path, make: &dyn Fn(&Path) -> io::Result<()>) -> io::Result<PathBuf> {
    for attempt in 0..1000 {
        let tmp = samedir_temp(dst, attempt);
        match make(&tmp) {
            Ok(()) => return Ok(tmp),
            Err(e) if errno(&e) == libc::EEXIST => {}
            Err(e) => return Err(e),
        }
    }
    Err(os_err(libc::EEXIST))
}

/// gnulib `force_linkat`: a hard link `dst` to `src`; with `force`, an
/// existing `dst` is replaced atomically (link to a temporary name, rename).
/// `tried` is the result of an earlier attempt at the plain link.
pub(crate) fn force_linkat(
    src: &Path,
    dst: &Path,
    follow: bool,
    force: bool,
    tried: Option<io::Result<()>>,
) -> io::Result<Linked> {
    crash_point();
    let first = tried.unwrap_or_else(|| link_path(src, dst, follow));
    match first {
        Ok(()) => return Ok(Linked::New),
        Err(e) if !force || errno(&e) != libc::EEXIST => return Err(e),
        Err(_) => {}
    }
    let tmp = with_temp_name(dst, &|t| link_path(src, t, follow))?;
    let res = fs::rename(&tmp, dst);
    // Removed even after a successful rename, in case `tmp` and `dst` were
    // already links to one file and the rename did nothing.
    let _ = unlink_path(&tmp, false);
    res.map(|()| Linked::Replaced)
}

/// gnulib `force_symlinkat`: a symlink `dst` containing `target`, replacing
/// an existing `dst` atomically with `force`.
pub(crate) fn force_symlinkat(
    target: &Path,
    dst: &Path,
    force: bool,
    tried: Option<io::Result<()>>,
) -> io::Result<Linked> {
    let first = tried.unwrap_or_else(|| std::os::unix::fs::symlink(target, dst));
    match first {
        Ok(()) => return Ok(Linked::New),
        Err(e) if !force || errno(&e) != libc::EEXIST => return Err(e),
        Err(_) => {}
    }
    let tmp = with_temp_name(dst, &|t| std::os::unix::fs::symlink(target, t))?;
    if let Err(e) = fs::rename(&tmp, dst) {
        let _ = unlink_path(&tmp, false);
        return Err(e);
    }
    Ok(Linked::Replaced)
}

/// gnulib `io_blksize`.
fn io_blksize(sb: &fs::Metadata) -> u64 {
    let mut bs = if sb.blksize() == 0 {
        IO_BUFSIZE
    } else {
        sb.blksize()
    };
    bs += (IO_BUFSIZE - 1) - (IO_BUFSIZE - 1) % bs;
    if sb.file_type().is_file() && !bs.is_power_of_two() {
        bs = bs.checked_next_power_of_two().unwrap_or(bs);
    }
    bs
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// gnulib `buffer_lcm`.
fn buffer_lcm(a: u64, b: u64, max: u64) -> u64 {
    if a == 0 {
        return if b == 0 { 8 * 1024 } else { b };
    }
    if b == 0 {
        return a;
    }
    let lcm = a / gcd(a, b) * b;
    if lcm <= max { lcm } else { a }
}

/// How to read a source when looking for holes (GNU's `enum scantype`).
enum ScanType {
    Error(io::Error),
    Plain,
    Zero,
    /// `SEEK_DATA` works: the offset of the first data, or -1.
    Lseek(i64),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn seek_data(f: &File, off: i64, hole: bool) -> io::Result<i64> {
    let whence = if hole {
        libc::SEEK_HOLE
    } else {
        libc::SEEK_DATA
    };
    // SAFETY: lseek takes an open descriptor (owned by `f`) and no pointers.
    let r = unsafe { libc::lseek(f.as_raw_fd(), off as libc::off_t, whence) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as i64)
    }
}

fn lseek(f: &File, off: i64, whence: libc::c_int) -> io::Result<i64> {
    // SAFETY: lseek takes an open descriptor (owned by `f`) and no pointers.
    let r = unsafe { libc::lseek(f.as_raw_fd(), off as libc::off_t, whence) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r as i64)
    }
}

/// GNU `infer_scantype`: only a file with fewer blocks than its size needs is
/// scanned for holes with `SEEK_DATA`.
fn infer_scantype(f: &File, sb: &fs::Metadata) -> ScanType {
    if !(sb.file_type().is_file() && sb.blocks() < sb.size() / 512) {
        return ScanType::Plain;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    match seek_data(f, 0, false) {
        Ok(off) => return ScanType::Lseek(off),
        Err(e) if errno(&e) == libc::ENXIO => return ScanType::Lseek(-1),
        Err(e) if !matches!(errno(&e), libc::EINVAL | libc::ENOTSUP) => {
            return ScanType::Error(e);
        }
        Err(_) => {}
    }
    let _ = f;
    ScanType::Zero
}

/// GNU `punch_hole`: deallocates a range (failure to support it is fine).
fn punch_hole(f: &File, offset: i64, length: i64) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: fallocate takes an open descriptor (owned by `f`) and no pointers.
        let r = unsafe {
            libc::fallocate(
                f.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                offset as libc::off_t,
                length as libc::off_t,
            )
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            if !matches!(errno(&e), libc::ENOTSUP | libc::ENOSYS) {
                return Err(e);
            }
        }
    }
    let _ = (f, offset, length);
    Ok(())
}

fn is_nul(buf: &[u8]) -> bool {
    buf.iter().all(|&b| b == 0)
}

/// GNU `is_terminal_error` (for FICLONE and copy_file_range).
fn is_terminal_error(e: i32) -> bool {
    matches!(e, libc::EIO | libc::ENOMEM | libc::ENOSPC | libc::EDQUOT)
}

/// GNU `is_CLONENOTSUP` (for copy_file_range, which only Linux has).
#[cfg(target_os = "linux")]
fn is_clone_notsup(e: i32) -> bool {
    matches!(
        e,
        libc::ENOSYS
            | libc::ENOTTY
            | libc::ENOTSUP
            | libc::EINVAL
            | libc::EBADF
            | libc::EXDEV
            | libc::ETXTBSY
            | libc::EPERM
            | libc::EACCES
    )
}

/// FICLONE: an instant copy-on-write clone (Btrfs, XFS, bcachefs).
fn clone_file(dst: &File, src: &File) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        const FICLONE: libc::c_ulong = 0x4004_9409;
        // SAFETY: FICLONE takes two open descriptors (owned by `dst` and `src`)
        // and no pointers.
        cvt(unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (dst, src);
        Err(os_err(libc::ENOTSUP))
    }
}

/// Where a sparse copy is in its output.
struct SparseState {
    total_n_read: u64,
    last_write_made_hole: bool,
}

impl<'a> Copier<'a> {
    pub(crate) fn new(
        tool: &'static str,
        x: CopyOptions,
        journal: Option<UndoJournal>,
        umask: u32,
        ctx: &'a DogfishContext,
    ) -> Self {
        Copier {
            x,
            tool,
            ctx,
            journal,
            umask,
            // SAFETY: geteuid has no preconditions and cannot fail.
            privileged: unsafe { libc::geteuid() } == 0,
            src_to_dest: HashMap::new(),
            dest_info: None,
            src_info: None,
            top_level_src: PathBuf::new(),
            top_level_dst: PathBuf::new(),
            plan: None,
            backups: HashMap::new(),
            target_dir: None,
            renamed: false,
            adopted: None,
            method: CopyMethod::StandardCopy,
        }
    }

    /// The destinations are under the target directory `dir`.
    pub(crate) fn set_target_dir(&mut self, dir: &Path) {
        let opened = fs::canonicalize(dir).unwrap_or_else(|_| absolute(dir));
        self.target_dir = Some((dir.to_path_buf(), opened));
    }

    /// The name by which GNU reaches `dst`: under the target directory,
    /// through its descriptor (so a rename of a symlink on the way to it
    /// does not change what it names), else as spelled.
    fn through_target_dir(&self, dst: &Path) -> PathBuf {
        match &self.target_dir {
            Some((dir, opened)) => match dst.strip_prefix(dir) {
                Ok(rel) if !rel.as_os_str().is_empty() => opened.join(rel),
                _ => dst.to_path_buf(),
            },
            None => dst.to_path_buf(),
        }
    }

    /// Remember created destinations (`dest_info_init`).
    pub(crate) fn track_destinations(&mut self) {
        self.dest_info = Some(FileSet::default());
    }

    /// Warn about repeated sources (`src_info_init`).
    pub(crate) fn track_sources(&mut self) {
        self.src_info = Some(FileSet::default());
    }

    fn diag(&self, msg: &str) {
        eprintln!("{}: {msg}", self.tool);
    }

    fn diag_err(&self, msg: &str, e: &io::Error) {
        eprintln!("{}: {msg}: {}", self.tool, gnu_io_error(e));
    }

    fn chown_failure_ok(&self, e: &io::Error) -> bool {
        matches!(errno(e), libc::EPERM | libc::EINVAL) && !self.privileged
    }

    fn emit_verbose(&self, prefix: &str, src: &Path, dst: &Path, backup: Option<&Path>) {
        match backup {
            Some(b) => outln!("{prefix}{} -> {} (backup: {})", qa(src), qa(dst), qa(b)),
            None => outln!("{prefix}{} -> {}", qa(src), qa(dst)),
        }
    }

    // ------------------------------------------------------------- journal

    /// Whether the journal may save what is at `dst`: a top-level operand
    /// whose previous state is not saved yet, in a directory the operation
    /// could change at all (otherwise it is about to fail anyway, with GNU's
    /// message). Device nodes are not saved (recreating one across file
    /// systems needs privileges); FIFOs and sockets are moved, never opened.
    fn journal_wants(&self, dst: &Path, top: bool) -> Option<fs::Metadata> {
        if !top || self.journal.is_none() || self.plan.as_ref().is_none_or(|p| p.backup.is_some()) {
            return None;
        }
        let m = fs::symlink_metadata(dst).ok()?;
        let ft = m.file_type();
        if std::os::unix::fs::FileTypeExt::is_block_device(&ft)
            || std::os::unix::fs::FileTypeExt::is_char_device(&ft)
        {
            return None;
        }
        eaccess_ok(&dir_name(dst), libc::W_OK | libc::X_OK).then_some(m)
    }

    /// The pending entry's action, as the plan now stands.
    fn plan_action(&self, p: &UndoPlan) -> JournalAction {
        if self.x.move_mode {
            JournalAction::Move {
                src: absolute(&p.src),
                dst: absolute(&p.created),
                backup_path: p.backup.clone(),
                copied: p.copied,
                existed: p.existed,
                src_inode: p.src_inode,
                src_link: p.src_link.clone(),
            }
        } else {
            JournalAction::CopyCreate {
                src: absolute(&p.src),
                created_path: absolute(&p.created),
                backup_path: p.backup.clone(),
                in_place: p.in_place,
                existed: p.existed,
            }
        }
    }

    /// Writes the plan into the pending journal entry, so an interrupted
    /// operation can be recovered from what it had done.
    fn sync_entry(&mut self) {
        let Some(p) = &self.plan else { return };
        let action = self.plan_action(p);
        if let Some(j) = &self.journal
            && let Some(p) = &mut self.plan
            && let Some(e) = &mut p.entry
        {
            e.action = action;
            let _ = j.update(e);
        }
    }

    /// Saves the top-level `dst` in the trash, as `how` says (see [`Take`]).
    /// The trash slot is in the journal before anything moves.
    fn journal_save(&mut self, dst: &Path, how: Take) -> io::Result<bool> {
        let by_copy = how == Take::Copy;
        let Some(p) = &mut self.plan else {
            return Ok(false);
        };
        p.created = stable_path(dst);
        p.in_place = by_copy;
        let Some(p) = &self.plan else {
            return Ok(false);
        };
        let action = self.plan_action(p);
        let (Some(j), Some(p)) = (&self.journal, &mut self.plan) else {
            return Ok(false);
        };
        let Some(entry) = &mut p.entry else {
            return Err(io::Error::other("cannot record the operation for undo"));
        };
        entry.action = action;
        match j.take(entry, dst, how)? {
            Some(b) => {
                p.backup = Some(b);
                Ok(true)
            }
            None => {
                p.in_place = false;
                Ok(false)
            }
        }
    }

    /// Moves the top-level `dst` into the trash instead of removing it.
    /// `Ok(true)` when it was saved, `Ok(false)` when there is nothing to save.
    fn journal_take(&mut self, dst: &Path, top: bool) -> io::Result<bool> {
        if self.journal_wants(dst, top).is_none() {
            return Ok(false);
        }
        self.journal_save(dst, Take::Move)
    }

    /// As [`Self::journal_take`], but `dst` stays in place for a `rename` to
    /// replace atomically.
    fn journal_take_linked(&mut self, dst: &Path, top: bool) -> io::Result<bool> {
        if self.journal_wants(dst, top).is_none() {
            return Ok(false);
        }
        self.journal_save(dst, Take::Link)
    }

    /// Puts a destination saved by [`Self::journal_take`] back after the
    /// operation that was to replace it failed.
    fn journal_untake(&mut self, dst: &Path) {
        if let Some(p) = &mut self.plan
            && let Some(b) = p.backup.clone()
            && fs::symlink_metadata(dst).is_err()
            && move_path(Path::new(&b), dst).is_ok()
        {
            p.backup = None;
            self.sync_entry();
        }
    }

    /// Saves a copy of `real` (a regular file or a directory) before the
    /// operation writes into it.
    fn journal_copy(&mut self, real: &Path) -> io::Result<()> {
        self.journal_save(real, Take::Copy).map(|_| ())
    }

    fn journal_failed(&self, dst: &Path, e: &io::Error) {
        self.diag_err(&format!("cannot back up {} before overwriting", qa(dst)), e);
    }

    /// Unlinks `dst`, or for a top-level operand moves it into the trash.
    fn remove_dst(&mut self, dst: &Path, dir: bool, top: bool) -> io::Result<()> {
        match self.journal_take(dst, top) {
            Ok(true) => Ok(()),
            Ok(false) => {
                crash_point();
                unlink_path(dst, dir)?;
                self.note_removed(top);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// The top-level destination was removed without saving it (it held no
    /// data, or a `--backup` file keeps it): what is there afterwards is new.
    fn note_removed(&mut self, top: bool) {
        if top
            && let Some(p) = &mut self.plan
            && p.existed
        {
            p.existed = false;
            self.sync_entry();
        }
    }

    /// The next top-level operation continues `entry`, which the caller
    /// recorded before starting it.
    pub(crate) fn adopt_entry(&mut self, entry: Option<JournalEntry>) {
        self.adopted = entry;
    }

    /// mv across file systems: `src` is completely copied, and about to be
    /// removed. From here on, undo moves the copy back even if part of `src`
    /// is left. A non-directory with other hard links is first linked into
    /// the trash, so undo can put its inode back.
    pub(crate) fn mark_copied(&mut self) {
        let Some(p) = &mut self.plan else { return };
        p.copied = true;
        self.sync_entry();
        if let Some(j) = &self.journal
            && let Some(p) = &mut self.plan
            && let Some(e) = &mut p.entry
            && let Ok(Some(link)) = j.keep_link(e, &p.src)
        {
            p.src_link = Some(link);
        }
    }

    /// Records the top-level operation in the journal; `moved` when it was a
    /// move that renamed or completely copied its source.
    pub(crate) fn commit_top(&mut self, moved: bool) {
        let Some(plan) = self.plan.take() else { return };
        let Some(j) = &self.journal else { return };
        let exists = fs::symlink_metadata(&plan.created).is_ok();
        if !(moved && exists)
            && let Some(link) = &plan.src_link
            && dogfish_core::journal::inode(link)
                .is_some_and(|i| dogfish_core::journal::inode(&plan.src) == Some(i))
        {
            // `src` was not removed after all: the link kept to it is spare.
            let _ = fs::remove_file(link);
        }
        let action = match moved {
            // What is left at the source (a removal that failed part-way) is
            // a remnant of what is now at the destination.
            true if exists => JournalAction::Move {
                src: absolute(&plan.src),
                dst: absolute(&plan.created),
                backup_path: plan.backup,
                copied: fs::symlink_metadata(&plan.src).is_ok(),
                existed: false,
                src_inode: None,
                src_link: plan.src_link,
            },
            _ if plan.backup.is_some() || (!plan.existed && exists) => JournalAction::CopyCreate {
                src: absolute(&plan.src),
                created_path: absolute(&plan.created),
                backup_path: plan.backup,
                in_place: plan.in_place,
                existed: false,
            },
            _ => {
                if let Some(mut e) = plan.entry {
                    let _ = j.cancel(&mut e);
                }
                return;
            }
        };
        match plan.entry {
            Some(mut e) => {
                e.action = action;
                let _ = j.commit(&mut e);
            }
            None => {
                let _ = j.record(self.tool, action);
            }
        }
    }

    // ---------------------------------------------------------------- copy

    /// GNU `copy`: copies (or with `move_mode`, moves) the command-line
    /// operand `src` to `dst`. `nonexistent_dst` is positive when `dst` is
    /// known not to exist, negative when it may only be a dangling symlink.
    pub(crate) fn copy(&mut self, src: &Path, dst: &Path, nonexistent_dst: i32) -> Outcome {
        self.top_level_src = src.to_path_buf();
        self.top_level_dst = dst.to_path_buf();
        let mut plan = UndoPlan {
            src_inode: dogfish_core::journal::inode(src),
            src: stable_path(src),
            created: stable_path(dst),
            backup: None,
            existed: fs::symlink_metadata(dst).is_ok(),
            in_place: false,
            copied: false,
            src_link: None,
            entry: None,
        };
        if let Some(e) = self.adopted.take() {
            // Begun by the caller before it renamed `src` to `dst`.
            if let JournalAction::Move { src_inode, .. } = &e.action {
                plan.src_inode = *src_inode;
            }
            plan.existed = false;
            plan.entry = Some(e);
        } else if let Some(j) = &self.journal {
            // Write-ahead: the operation is in the journal before it starts.
            plan.entry = j.begin(self.tool, self.plan_action(&plan)).ok();
        }
        self.plan = Some(plan);
        let mut first_dir_created = false;
        let mut out = Outcome::default();
        let level = Level {
            dereference: self.x.dereference,
            command_line_arg: true,
            parent: None,
        };
        out.ok = self.copy_internal(
            src,
            dst,
            nonexistent_dst,
            level,
            &[],
            &mut first_dir_created,
            &mut out.copy_into_self,
            &mut out.rename_succeeded,
        );
        out.renamed = std::mem::take(&mut self.renamed);
        out
    }

    fn remember_copied(&mut self, name: &Path, sb: &fs::Metadata) -> Option<PathBuf> {
        match self.src_to_dest.entry((sb.dev(), sb.ino())) {
            std::collections::hash_map::Entry::Occupied(e) => Some(e.get().clone()),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(name.to_path_buf());
                None
            }
        }
    }

    fn forget_created(&mut self, sb: Option<&fs::Metadata>) {
        if let Some(sb) = sb {
            self.src_to_dest.remove(&(sb.dev(), sb.ino()));
        }
    }

    /// GNU `create_hard_link`.
    fn create_hard_link(
        &mut self,
        src: &Path,
        dst: &Path,
        replace: bool,
        verbose: bool,
        dereference: bool,
        top: bool,
    ) -> bool {
        let saved = match self.journal_take_if_exists(dst, top && replace) {
            Ok(s) => s,
            Err(e) => {
                self.journal_failed(dst, &e);
                return false;
            }
        };
        match force_linkat(src, dst, dereference, replace, None) {
            Ok(linked) => {
                if (linked == Linked::Replaced || saved) && verbose {
                    outln!("removed {}", qa(dst));
                }
                true
            }
            Err(e) => {
                if saved {
                    self.journal_untake(dst);
                }
                self.diag_err(
                    &format!("cannot create hard link {} to {}", qa(dst), qa(src)),
                    &e,
                );
                false
            }
        }
    }

    fn journal_take_if_exists(&mut self, dst: &Path, top: bool) -> io::Result<bool> {
        if top && fs::symlink_metadata(dst).is_ok() {
            self.journal_take(dst, true)
        } else {
            Ok(false)
        }
    }

    /// `force_symlinkat` with the journal: `Some(err)` on failure.
    fn make_symlink(
        &mut self,
        target: &Path,
        dst: &Path,
        force: bool,
        top: bool,
    ) -> Option<io::Error> {
        let first = std::os::unix::fs::symlink(target, dst);
        let mut saved = false;
        let res = match first {
            Err(e) if force && errno(&e) == libc::EEXIST => match self.journal_take(dst, top) {
                Ok(true) => {
                    saved = true;
                    std::os::unix::fs::symlink(target, dst).map(|()| Linked::New)
                }
                Ok(false) => force_symlinkat(target, dst, true, Some(Err(e))),
                Err(je) => {
                    self.journal_failed(dst, &je);
                    return Some(je);
                }
            },
            first => force_symlinkat(target, dst, force, Some(first)),
        };
        match res {
            Ok(_) => None,
            Err(e) => {
                if saved {
                    self.journal_untake(dst);
                }
                Some(e)
            }
        }
    }

    /// GNU `writable_destination`.
    fn writable_destination(&self, dst: &Path, mode: u32) -> bool {
        is_type(mode, S_IFLNK) || self.privileged || eaccess_ok(dst, libc::W_OK)
    }

    /// GNU `overwrite_ok`: asks whether to replace `dst`.
    fn overwrite_ok(&self, dst: &Path, dst_sb: &fs::Metadata) -> bool {
        let mode = dst_sb.mode();
        let q = if !self.writable_destination(dst, mode) {
            let bits = mode & CHMOD_MODE_BITS;
            if self.x.move_mode
                || self.x.unlink_dest_before_opening
                || self.x.unlink_dest_after_failed_open
            {
                format!(
                    "replace {}, overriding mode {bits:04o} ({})? ",
                    qa(dst),
                    strmode9(mode)
                )
            } else {
                format!(
                    "unwritable {} (mode {bits:04o}, {}); try anyway? ",
                    qa(dst),
                    strmode9(mode)
                )
            }
        } else {
            format!("overwrite {}? ", qa(dst))
        };
        ask(self.tool, &q)
    }

    /// GNU `abandon_move`.
    fn abandon_move(&self, dst: &Path, dst_sb: Option<&fs::Metadata>) -> bool {
        match self.x.interactive {
            Interactive::AlwaysNo | Interactive::AlwaysSkip => true,
            i => {
                let Some(sb) = dst_sb else { return false };
                (i == Interactive::AskUser
                    || (i == Interactive::Unspecified
                        && self.x.stdin_tty
                        && !self.writable_destination(dst, sb.mode())))
                    && !self.overwrite_ok(dst, sb)
            }
        }
    }

    /// GNU `source_is_dst_backup`: is `src` (base name `srcbase`) the simple
    /// backup of `dst`?
    fn source_is_dst_backup(&self, srcbase: &[u8], src_sb: &fs::Metadata, dst: &Path) -> bool {
        let dstbase = last_component(dst.as_os_str().as_bytes());
        let suffix = self.x.suffix.as_bytes();
        if !(srcbase.len() == dstbase.len() + suffix.len()
            && srcbase.starts_with(dstbase)
            && &srcbase[dstbase.len()..] == suffix)
        {
            return false;
        }
        let mut back = dst.as_os_str().to_os_string();
        back.push(OsStr::from_bytes(suffix));
        fs::metadata(&back).is_ok_and(|m| same_inode(src_sb, &m))
    }

    /// GNU `set_owner`: 1 when ownership was set, 0 when failing is fine,
    /// -1 on failure.
    fn set_owner(
        &self,
        dst: &Path,
        fd: Option<&File>,
        src_sb: &fs::Metadata,
        new_dst: bool,
        dst_sb: Option<&fs::Metadata>,
    ) -> i32 {
        let (uid, gid) = (src_sb.uid(), src_sb.gid());
        if !new_dst
            && (self.x.preserve_mode || self.x.move_mode)
            && let Some(dsb) = dst_sb
        {
            // Change to a restrictive mode first, so the file is never more
            // accessible to the new owner than it should be. (GNU's ACL-enabled
            // build always does this.)
            let restrictive = dsb.mode() & src_sb.mode() & S_IRWXU;
            if let Err(e) = chmod_either(fd, dst, restrictive) {
                if !(matches!(errno(&e), libc::EPERM | libc::EINVAL) && !self.privileged) {
                    self.diag_err(&format!("clearing permissions for {}", qa(dst)), &e);
                }
                return -i32::from(self.x.require_preserve);
            }
        }
        let err = match lchown_ids(fd, dst, Some(uid), Some(gid)) {
            Ok(()) => return 1,
            Err(e) => e,
        };
        if matches!(errno(&err), libc::EPERM | libc::EINVAL) {
            // Failed to set both; set just the group, ignoring failure.
            let _ = lchown_ids(fd, dst, None, Some(gid));
        }
        if !self.chown_failure_ok(&err) {
            self.diag_err(
                &format!("failed to preserve ownership for {}", qa(dst)),
                &err,
            );
            if self.x.require_preserve {
                return -1;
            }
        }
        0
    }

    /// GNU `copy_acl` without ACLs: give `dst` the mode `mode`.
    fn copy_mode(&self, dst: &Path, fd: Option<&File>, mode: u32) -> bool {
        match chmod_either(fd, dst, mode) {
            Ok(()) => true,
            Err(e) => {
                self.diag_err(
                    &format!("preserving permissions for {}", quote_locale(dst)),
                    &e,
                );
                false
            }
        }
    }

    /// GNU `set_acl` without ACLs.
    fn set_mode(&self, dst: &Path, fd: Option<&File>, mode: u32) -> bool {
        match chmod_either(fd, dst, mode) {
            Ok(()) => true,
            Err(e) => {
                self.diag_err(
                    &format!("setting permissions for {}", quote_locale(dst)),
                    &e,
                );
                false
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_internal(
        &mut self,
        src: &Path,
        dst: &Path,
        mut nonexistent_dst: i32,
        level: Level<'_>,
        ancestors: &[(u64, u64)],
        first_dir_created: &mut bool,
        copy_into_self: &mut bool,
        rename_succeeded: &mut bool,
    ) -> bool {
        let x_move = self.x.move_mode;
        let deref_level = level.dereference;
        let command_line_arg = level.command_line_arg;
        let top = command_line_arg;
        let mut new_dst = nonexistent_dst > 0;
        let mut restore_dst_mode = false;
        let mut dst_mode: u32 = 0;
        let mut dst_backup: Option<PathBuf> = None;
        let mut earlier_file: Option<PathBuf> = None;
        let mut copied_as_regular = false;
        let mut dest_is_symlink = false;
        let mut have_dst_lstat = false;
        *copy_into_self = false;

        let mut rename_errno: i32 = self.x.rename_errno.unwrap_or(-1);
        // Where `dst` is once renamed: GNU reaches it through the target
        // directory's descriptor, which still works when the rename moved what
        // the path went through (`mv b a b`, with `b` a symlink to a directory).
        let mut renamed_dst = dst.to_path_buf();
        if x_move {
            if rename_errno < 0 {
                renamed_dst = stable_path(dst);
                rename_errno = match rename_noreplace(src, dst) {
                    Ok(()) => 0,
                    Err(e) => errno(&e),
                };
            }
            new_dst = rename_errno == 0;
            *rename_succeeded = new_dst;
            self.renamed |= top && new_dst;
            nonexistent_dst = i32::from(new_dst);
        }
        let skip_no = matches!(
            self.x.interactive,
            Interactive::AlwaysNo | Interactive::AlwaysSkip
        );

        let mut src_sb: Option<fs::Metadata> = None;
        let stat_src = if rename_errno == 0 {
            !self.x.last_file
        } else {
            rename_errno != libc::EEXIST || !skip_no
        };
        if stat_src {
            let (name, at) = if rename_errno == 0 {
                (dst, renamed_dst.as_path())
            } else {
                (src, src)
            };
            let res = if deref_level == Deref::Never {
                fs::symlink_metadata(at)
            } else {
                fs::metadata(at)
            };
            match res {
                Ok(m) => src_sb = Some(m),
                Err(e) => {
                    self.diag_err(&format!("cannot stat {}", qa(name)), &e);
                    return false;
                }
            }
            if src_sb.as_ref().is_some_and(|m| m.is_dir()) && !self.x.recursive {
                self.diag(&format!("-r not specified; omitting directory {}", qa(src)));
                return false;
            }
        }
        let mut src_mode = src_sb.as_ref().map_or(0, MetadataExt::mode);
        let src_is_dir = is_type(src_mode, S_IFDIR);

        if command_line_arg
            && let Some(sb) = &src_sb
            && self.src_info.is_some()
        {
            let seen = self.src_info.as_ref().is_some_and(|s| s.seen(src, sb));
            if !src_is_dir && self.x.backup == BackupMode::None && seen {
                self.diag(&format!(
                    "warning: source file {} specified more than once",
                    qa(src)
                ));
                return true;
            }
            if let Some(s) = &mut self.src_info {
                s.record(src, sb);
            }
        }

        let dereference =
            deref_level == Deref::Always || (deref_level == Deref::CommandLine && command_line_arg);

        let mut dst_sb: Option<fs::Metadata> = None;
        if nonexistent_dst <= 0 {
            if !(rename_errno == libc::EEXIST && skip_no) {
                // Regular files are written through symlinks; everything else
                // replaces the destination itself.
                let use_lstat = (!is_type(src_mode, S_IFREG)
                    && (!self.x.copy_as_regular || src_is_dir || is_type(src_mode, S_IFLNK)))
                    || x_move
                    || self.x.symbolic_link
                    || self.x.hard_link
                    || self.x.backup != BackupMode::None
                    || self.x.unlink_dest_before_opening;
                let use_lstat = use_lstat && !(self.x.keep_directory_symlink && src_is_dir);
                if !use_lstat && nonexistent_dst < 0 {
                    new_dst = true;
                } else {
                    let res = if use_lstat {
                        fs::symlink_metadata(dst)
                    } else {
                        fs::metadata(dst)
                    };
                    match res {
                        Ok(m) => {
                            dst_sb = Some(m);
                            have_dst_lstat = use_lstat;
                            rename_errno = libc::EEXIST;
                        }
                        Err(e) => {
                            if errno(&e) == libc::ELOOP && self.x.unlink_dest_after_failed_open {
                                // Leave new_dst false: unlinked later.
                            } else if errno(&e) != libc::ENOENT {
                                self.diag_err(&format!("cannot stat {}", qa(dst)), &e);
                                return false;
                            } else {
                                new_dst = true;
                            }
                        }
                    }
                }
            }

            if rename_errno == libc::EEXIST {
                let mut return_now = false;
                let mut return_val = true;
                let mut skipped = false;

                if !skip_no && let (Some(ssb), Some(dsb)) = (&src_sb, &dst_sb) {
                    let opts = SameFileOpts {
                        move_mode: x_move,
                        deref_never: deref_level == Deref::Never,
                        hard_link: self.x.hard_link,
                        symbolic_link: self.x.symbolic_link,
                        backup: self.x.backup != BackupMode::None,
                        unlink_dest_before_opening: self.x.unlink_dest_before_opening,
                    };
                    let (ok, now) = same_file_ok(src, ssb, dst, dsb, &opts);
                    if !ok {
                        self.diag(&format!("{} and {} are the same file", qa(src), qa(dst)));
                        return false;
                    }
                    return_now = now;
                }

                let mut update_skip = false;
                if self.x.update
                    && !src_is_dir
                    && let (Some(ssb), Some(dsb)) = (&src_sb, &dst_sb)
                    && (dsb.mtime(), dsb.mtime_nsec()) >= (ssb.mtime(), ssb.mtime_nsec())
                {
                    // The destination is not older: leave it, but remember the
                    // pair in case another link to this source comes later.
                    *rename_succeeded = true;
                    let ssb = ssb.clone();
                    earlier_file = self.remember_copied(dst, &ssb);
                    if let Some(e) = earlier_file.clone()
                        && !self.create_hard_link(&e, dst, true, self.x.verbose, dereference, top)
                    {
                        return self.un_backup(None, &earlier_file, dst, src_sb.as_ref());
                    }
                    skipped = true;
                    update_skip = true;
                }

                if !update_skip {
                    if x_move {
                        if self.abandon_move(dst, dst_sb.as_ref()) {
                            *rename_succeeded = true;
                            skipped = true;
                            return_val = self.x.interactive == Interactive::AlwaysSkip;
                        }
                    } else if !src_is_dir
                        && (skip_no
                            || (self.x.interactive == Interactive::AskUser
                                && dst_sb.as_ref().is_some_and(|d| !self.overwrite_ok(dst, d))))
                    {
                        skipped = true;
                        return_val = self.x.interactive == Interactive::AlwaysSkip;
                    }
                }

                if skipped {
                    if self.x.interactive == Interactive::AlwaysNo {
                        self.diag(&format!("not replacing {}", qa(dst)));
                    }
                    return_now = true;
                }
                if return_now {
                    return return_val;
                }

                // The destination vanished since it was examined: carry on as if new.
                'exists: {
                    let Some(dsb) = dst_sb.clone() else {
                        new_dst = true;
                        break 'exists;
                    };
                    let dst_is_dir = dsb.is_dir();
                    if !dst_is_dir {
                        if src_is_dir && !(x_move && self.x.backup != BackupMode::None) {
                            self.diag(&format!(
                                "cannot overwrite non-directory {} with directory {}",
                                qa(dst),
                                qa(src)
                            ));
                            return false;
                        }
                        // Never let a later operand overwrite an earlier one's copy:
                        // with `mv a/f b/f c`, b/f's data would be lost.
                        if command_line_arg
                            && self.x.backup != BackupMode::Numbered
                            && self.dest_info.as_ref().is_some_and(|d| d.seen(dst, &dsb))
                        {
                            self.diag(&format!(
                                "will not overwrite just-created {} with {}",
                                qa(dst),
                                qa(src)
                            ));
                            return false;
                        }
                    }
                    if !src_is_dir && dst_is_dir && !(x_move && self.x.backup != BackupMode::None) {
                        self.diag(&format!(
                            "cannot overwrite directory {} with non-directory",
                            qa(dst)
                        ));
                        return false;
                    }
                    if x_move && src_is_dir && !dst_is_dir && self.x.backup == BackupMode::None {
                        self.diag(&format!(
                            "cannot move directory onto non-directory: {} -> {}",
                            qf(src),
                            qf(dst)
                        ));
                        return false;
                    }

                    let srcbase = last_component(src.as_os_str().as_bytes());
                    if self.x.backup != BackupMode::None
                        && !dot_or_dotdot(srcbase)
                        && (x_move || !dst_is_dir)
                    {
                        if self.x.backup != BackupMode::Numbered
                            && let Some(ssb) = &src_sb
                            && self.source_is_dst_backup(srcbase, ssb, dst)
                        {
                            let what = if x_move { "moved" } else { "copied" };
                            self.diag(&format!(
                                "backing up {} might destroy source;  {} not {what}",
                                qa(dst),
                                qa(src)
                            ));
                            return false;
                        }
                        match make_backup(
                            dst,
                            &self.through_target_dir(dst),
                            self.x.backup,
                            &self.x.suffix,
                            self.journal.as_ref(),
                            self.tool,
                        ) {
                            Ok(b) => {
                                dst_backup = Some(b.name.clone());
                                self.backups.insert(b.name.clone(), b);
                                self.note_removed(top);
                            }
                            Err(e) if errno(&e) == libc::ENOENT => {}
                            Err(e) => {
                                self.diag_err(&format!("cannot backup {}", qa(dst)), &e);
                                return false;
                            }
                        }
                        new_dst = true;
                    } else if !dst_is_dir
                        && !x_move
                        && (self.x.unlink_dest_before_opening
                            || (self.x.data_copy_required
                                && ((self.x.preserve_links && dsb.nlink() > 1)
                                    || (deref_level == Deref::Never
                                        && !is_type(src_mode, S_IFREG)))))
                    {
                        if let Err(e) = self.remove_dst(dst, false, top)
                            && errno(&e) != libc::ENOENT
                        {
                            self.diag_err(&format!("cannot remove {}", qa(dst)), &e);
                            return false;
                        }
                        new_dst = true;
                        if self.x.verbose {
                            outln!("removed {}", qa(dst));
                        }
                    }
                }
            }
        }

        // Never copy through a symlink created earlier in this run.
        if command_line_arg
            && self.dest_info.is_some()
            && !x_move
            && self.x.backup == BackupMode::None
        {
            let lst = if have_dst_lstat {
                dst_sb.clone()
            } else {
                fs::symlink_metadata(dst).ok()
            };
            if let Some(l) = lst
                && l.file_type().is_symlink()
                && self.dest_info.as_ref().is_some_and(|d| d.seen(dst, &l))
            {
                self.diag(&format!(
                    "will not copy {} through just-created symlink {}",
                    qa(src),
                    qa(dst)
                ));
                return false;
            }
        }

        // On a terminal, a regular copy is announced once it is done, with
        // how the data was copied (a Dogfish extra).
        let rich_verbose = self.ctx.stdout_is_tty()
            && !self.x.hard_link
            && !self.x.symbolic_link
            && (is_type(src_mode, S_IFREG)
                || (self.x.copy_as_regular && !is_type(src_mode, S_IFLNK)));
        let mut verbose_pending = false;
        if self.x.verbose && !x_move && !src_is_dir {
            if rich_verbose {
                verbose_pending = true;
            } else {
                self.emit_verbose("", src, dst, dst_backup.as_deref());
            }
        }

        // Link the destinations of hard-linked sources (see GNU's comment).
        if rename_errno == 0 {
            earlier_file = None;
        } else if let Some(ssb) = src_sb.clone() {
            if self.x.recursive && src_is_dir {
                earlier_file = if command_line_arg {
                    self.remember_copied(dst, &ssb)
                } else {
                    self.src_to_dest.get(&(ssb.dev(), ssb.ino())).cloned()
                };
            } else if x_move && ssb.nlink() == 1 {
                earlier_file = self.src_to_dest.get(&(ssb.dev(), ssb.ino())).cloned();
            } else if self.x.preserve_links
                && !self.x.hard_link
                && (ssb.nlink() > 1
                    || (command_line_arg && deref_level == Deref::CommandLine)
                    || deref_level == Deref::Always)
            {
                earlier_file = self.remember_copied(dst, &ssb);
            }
        }

        if let Some(earlier) = earlier_file.clone() {
            if src_is_dir {
                if same_name(src, &earlier) {
                    self.diag(&format!(
                        "cannot copy a directory, {}, into itself, {}",
                        qa(&self.top_level_src),
                        qa(&self.top_level_dst)
                    ));
                    *copy_into_self = true;
                    return self.un_backup(
                        dst_backup.as_deref(),
                        &earlier_file,
                        dst,
                        src_sb.as_ref(),
                    );
                } else if same_name(dst, &earlier) {
                    self.diag(&format!(
                        "warning: source directory {} specified more than once",
                        qa(&self.top_level_src)
                    ));
                    if x_move {
                        *rename_succeeded = true;
                    }
                    return true;
                } else if deref_level == Deref::Always
                    || (command_line_arg && deref_level == Deref::CommandLine)
                {
                    // A directory reached again through symlinks: copy it again.
                } else {
                    self.diag(&format!(
                        "will not create hard link {} to directory {}",
                        qa(dst),
                        qa(&earlier)
                    ));
                    return self.un_backup(
                        dst_backup.as_deref(),
                        &earlier_file,
                        dst,
                        src_sb.as_ref(),
                    );
                }
            } else {
                if verbose_pending {
                    self.emit_verbose("", src, dst, dst_backup.as_deref());
                }
                if !self.create_hard_link(&earlier, dst, true, self.x.verbose, dereference, top) {
                    return self.un_backup(
                        dst_backup.as_deref(),
                        &earlier_file,
                        dst,
                        src_sb.as_ref(),
                    );
                }
                return true;
            }
        }

        // Dogfish refuses to copy a directory into itself before copying
        // anything; GNU notices only once part of the tree is copied. (The
        // message and the state GNU keeps for later operands are the same.)
        if !x_move
            && command_line_arg
            && src_is_dir
            && dst_sb.as_ref().is_none_or(fs::Metadata::is_dir)
            && let Ok(abs_src) = fs::canonicalize(src)
            && resolve_for_containment(dst).starts_with(&abs_src)
        {
            self.diag(&format!(
                "cannot copy a directory, {}, into itself, {}",
                qa(&self.top_level_src),
                qa(&self.top_level_dst)
            ));
            *copy_into_self = true;
            return false;
        }

        if x_move {
            let mut saved = false;
            if rename_errno == libc::EEXIST {
                // Keep what the rename replaces, for `dogfish undo`. A non-empty
                // directory cannot be replaced: let the rename say so.
                let replaceable = dst_sb.as_ref().is_none_or(|d| {
                    !d.is_dir() || fs::read_dir(dst).is_ok_and(|mut r| r.next().is_none())
                });
                if replaceable {
                    match self.journal_take_linked(dst, top) {
                        Ok(s) => saved = s,
                        Err(e) => {
                            self.journal_failed(dst, &e);
                            self.forget_created(src_sb.as_ref());
                            return false;
                        }
                    }
                }
                // After a backup, by the name actually freed (see `Backup::real`).
                let to = dst_backup
                    .as_ref()
                    .and_then(|b| self.backups.get(b))
                    .map_or(dst, |b| b.real_dst.as_path());
                crash_point();
                rename_errno = match fs::rename(src, to) {
                    Ok(()) => 0,
                    Err(e) => errno(&e),
                };
                if rename_errno != 0 && rename_errno != libc::EXDEV && saved {
                    self.journal_untake(dst);
                }
            }

            if rename_errno == 0 {
                if self.x.verbose {
                    self.emit_verbose("renamed ", src, dst, dst_backup.as_deref());
                }
                *rename_succeeded = true;
                self.renamed |= top;
                if command_line_arg
                    && !self.x.last_file
                    && let Some(ssb) = &src_sb
                    && let Some(d) = &mut self.dest_info
                {
                    d.record(dst, ssb);
                }
                return true;
            }

            if rename_errno == libc::EINVAL {
                self.diag(&format!(
                    "cannot move {} to a subdirectory of itself, {}",
                    qa(&self.top_level_src),
                    qa(&self.top_level_dst)
                ));
                *copy_into_self = true;
                return true;
            }

            if rename_errno != libc::EXDEV || self.x.no_copy {
                let e = os_err(rename_errno);
                let msg = match rename_errno {
                    libc::EDQUOT
                    | libc::EEXIST
                    | libc::EISDIR
                    | libc::EMLINK
                    | libc::ENOSPC
                    | libc::ETXTBSY
                    | libc::ENOTEMPTY => format!("cannot overwrite {}", qa(dst)),
                    _ => format!("cannot move {} to {}", qa(src), qa(dst)),
                };
                self.diag_err(&msg, &e);
                self.forget_created(src_sb.as_ref());
                return false;
            }

            // Across file systems: remove the destination, as rename would.
            let removed = unlink_path(dst, src_is_dir);
            if removed.is_ok() {
                self.note_removed(top);
            }
            if let Err(e) = removed
                && errno(&e) != libc::ENOENT
            {
                self.diag_err(
                    &format!(
                        "inter-device move failed: {} to {}; unable to remove target",
                        qa(src),
                        qa(dst)
                    ),
                    &e,
                );
                self.forget_created(src_sb.as_ref());
                return false;
            }
            if self.x.verbose && !src_is_dir {
                self.emit_verbose("copied ", src, dst, dst_backup.as_deref());
            }
            new_dst = true;
        }

        let Some(ssb) = src_sb.clone() else {
            return false;
        };

        // Withhold permissions others could use before the file is ready.
        let dst_mode_bits = src_mode & CHMOD_MODE_BITS;
        let mut omitted = dst_mode_bits
            & if self.x.preserve_ownership {
                S_IRWXG | S_IRWXO
            } else if src_is_dir {
                S_IWGRP | S_IWOTH
            } else {
                0
            };
        let mut delayed_ok = true;

        if src_is_dir {
            let key = (ssb.dev(), ssb.ino());
            if ancestors.contains(&key) {
                self.diag(&format!("cannot copy cyclic symbolic link {}", qa(src)));
                return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
            }
            let mut dirs = ancestors.to_vec();
            dirs.push(key);

            if new_dst || !dst_sb.as_ref().is_some_and(fs::Metadata::is_dir) {
                let mode = dst_mode_bits & !omitted;
                let res =
                    // SAFETY: `c` is a NUL-terminated path that outlives the call.
                    c_path(dst).and_then(|c| cvt(unsafe { libc::mkdir(c.as_ptr(), mode_t(mode)) }));
                if let Err(e) = res {
                    self.diag_err(&format!("cannot create directory {}", qa(dst)), &e);
                    return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
                }
                match fs::symlink_metadata(dst) {
                    Err(e) => {
                        self.diag_err(&format!("cannot stat {}", qa(dst)), &e);
                        return self.un_backup(
                            dst_backup.as_deref(),
                            &earlier_file,
                            dst,
                            Some(&ssb),
                        );
                    }
                    Ok(m) => {
                        if m.mode() & S_IRWXU != S_IRWXU {
                            dst_mode = m.mode();
                            restore_dst_mode = true;
                            if let Err(e) = chmod_path(dst, dst_mode | S_IRWXU) {
                                self.diag_err(&format!("setting permissions for {}", qa(dst)), &e);
                                return self.un_backup(
                                    dst_backup.as_deref(),
                                    &earlier_file,
                                    dst,
                                    Some(&ssb),
                                );
                            }
                        }
                        if !*first_dir_created {
                            self.remember_copied(dst, &m);
                            *first_dir_created = true;
                        }
                        dst_sb = Some(m);
                    }
                }
                if self.x.verbose {
                    if x_move {
                        outln!("created directory {}", qa(dst));
                    } else {
                        self.emit_verbose("", src, dst, None);
                    }
                }
            } else {
                omitted = 0;
                // Merging into an existing directory: keep a copy for undo.
                if top
                    && self.journal_wants(dst, top).is_some()
                    && let Err(e) = self.journal_copy(dst)
                {
                    self.journal_failed(dst, &e);
                    return false;
                }
            }

            let crosses =
                self.x.one_file_system && level.parent.is_some_and(|p| p.dev() != ssb.dev());
            if !crosses {
                delayed_ok = self.copy_dir(
                    src,
                    dst,
                    new_dst,
                    &ssb,
                    &dirs,
                    level,
                    first_dir_created,
                    copy_into_self,
                );
            }
        } else if self.x.symbolic_link {
            dest_is_symlink = true;
            if !src.as_os_str().as_bytes().starts_with(b"/") {
                let parent = dir_name(dst);
                let in_current_dir = parent.as_os_str() == "."
                    || match (fs::metadata("."), fs::metadata(&parent)) {
                        (Ok(d), Ok(p)) => same_inode(&d, &p),
                        _ => true,
                    };
                if !in_current_dir {
                    self.diag(&format!(
                        "{}: can make relative symbolic links only in current directory",
                        qf(dst)
                    ));
                    return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
                }
            }
            if let Some(e) = self.make_symlink(src, dst, self.x.unlink_dest_after_failed_open, top)
            {
                self.diag_err(
                    &format!("cannot create symbolic link {} to {}", qa(dst), qa(src)),
                    &e,
                );
                return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
            }
        } else if self.x.hard_link {
            let replace =
                self.x.unlink_dest_after_failed_open || self.x.interactive == Interactive::AskUser;
            if !self.create_hard_link(src, dst, replace, false, dereference, top) {
                return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
            }
        } else if is_type(src_mode, S_IFREG)
            || (self.x.copy_as_regular && !is_type(src_mode, S_IFLNK))
        {
            copied_as_regular = true;
            let ok = self.copy_reg(
                src,
                dst,
                dst_mode_bits & S_IRWXUGO,
                omitted,
                &mut new_dst,
                &ssb,
                deref_level,
                top,
            );
            if !ok {
                return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
            }
            if verbose_pending {
                let method = self.method;
                match &dst_backup {
                    Some(b) => outln!(
                        "{} -> {} (backup: {}) ({})",
                        qa(src),
                        qa(dst),
                        qa(b),
                        method.as_str()
                    ),
                    None => outln!("{} -> {} ({})", qa(src), qa(dst), method.as_str()),
                }
            }
        } else if is_type(src_mode, S_IFIFO) {
            let mode = src_mode & !omitted;
            if let Err(e) = mknod_path(dst, S_IFIFO | (mode & CHMOD_MODE_BITS), 0)
                .or_else(|_| mkfifo_path(dst, mode & CHMOD_MODE_BITS))
            {
                self.diag_err(&format!("cannot create fifo {}", qa(dst)), &e);
                return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
            }
        } else if is_type(src_mode, S_IFBLK)
            || is_type(src_mode, S_IFCHR)
            || is_type(src_mode, S_IFSOCK)
        {
            let mode = src_mode & !omitted;
            if let Err(e) = mknod_path(
                dst,
                (src_mode & S_IFMT) | (mode & CHMOD_MODE_BITS),
                ssb.rdev(),
            ) {
                self.diag_err(&format!("cannot create special file {}", qa(dst)), &e);
                return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
            }
        } else if is_type(src_mode, S_IFLNK) {
            dest_is_symlink = true;
            let target = match fs::read_link(src) {
                Ok(t) => t,
                Err(e) => {
                    self.diag_err(&format!("cannot read symbolic link {}", qa(src)), &e);
                    return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
                }
            };
            let mut err =
                self.make_symlink(&target, dst, self.x.unlink_dest_after_failed_open, top);
            // With --update, an identical existing symlink is fine.
            if err.is_some()
                && self.x.update
                && !new_dst
                && let Some(d) = &dst_sb
                && d.file_type().is_symlink()
                && d.size() == target.as_os_str().len() as u64
                && fs::read_link(dst).is_ok_and(|t| t == target)
            {
                err = None;
            }
            if let Some(e) = err {
                self.diag_err(&format!("cannot create symbolic link {}", qa(dst)), &e);
                return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
            }
            if self.x.preserve_ownership
                && let Err(e) = lchown_ids(None, dst, Some(ssb.uid()), Some(ssb.gid()))
                && !self.chown_failure_ok(&e)
            {
                // GNU leaves this name unquoted.
                eprintln!(
                    "{}: failed to preserve ownership for {}: {}",
                    self.tool,
                    dst.display(),
                    gnu_io_error(&e)
                );
                if self.x.require_preserve {
                    return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
                }
            }
        } else {
            self.diag(&format!("{} has unknown file type", qa(src)));
            return self.un_backup(dst_backup.as_deref(), &earlier_file, dst, Some(&ssb));
        }

        if command_line_arg
            && let Ok(m) = fs::symlink_metadata(dst)
            && let Some(d) = &mut self.dest_info
        {
            d.record(dst, &m);
        }
        if self.x.hard_link && !src_is_dir {
            return delayed_ok;
        }
        if copied_as_regular {
            return delayed_ok;
        }

        if self.x.preserve_timestamps
            && let Err(e) = utimens_path(dst, &src_times(&ssb), dest_is_symlink)
        {
            self.diag_err(&format!("preserving times for {}", qa(dst)), &e);
            if self.x.require_preserve {
                return false;
            }
        }

        if !dest_is_symlink
            && self.x.preserve_ownership
            && (new_dst
                || !dst_sb
                    .as_ref()
                    .is_some_and(|d| d.uid() == ssb.uid() && d.gid() == ssb.gid()))
        {
            match self.set_owner(dst, None, &ssb, new_dst, dst_sb.as_ref()) {
                -1 => return false,
                0 => src_mode &= !(S_ISUID | S_ISGID | S_ISVTX),
                _ => {}
            }
        }

        if dest_is_symlink {
            return delayed_ok;
        }

        if self.x.preserve_mode || x_move {
            if !self.copy_mode(dst, None, src_mode) && self.x.require_preserve {
                return false;
            }
        } else if self.x.explicit_no_preserve_mode && new_dst {
            let default = if src_is_dir || is_type(src_mode, S_IFSOCK) {
                S_IRWXUGO
            } else {
                MODE_RW_UGO
            };
            if !self.set_mode(dst, None, default & !self.umask) {
                return false;
            }
        } else {
            if omitted != 0 {
                omitted &= !self.umask;
                if omitted != 0 && !restore_dst_mode {
                    if new_dst {
                        match fs::symlink_metadata(dst) {
                            Ok(m) => dst_sb = Some(m),
                            Err(e) => {
                                self.diag_err(&format!("cannot stat {}", qa(dst)), &e);
                                return false;
                            }
                        }
                    }
                    dst_mode = dst_sb.as_ref().map_or(0, MetadataExt::mode);
                    if omitted & !dst_mode != 0 {
                        restore_dst_mode = true;
                    }
                }
            }
            if restore_dst_mode && let Err(e) = chmod_path(dst, dst_mode | omitted) {
                self.diag_err(&format!("preserving permissions for {}", qa(dst)), &e);
                if self.x.require_preserve {
                    return false;
                }
            }
        }
        delayed_ok
    }

    /// The backup `b` of `dst` was renamed back: withdraw its journal entry,
    /// once the older backup file it displaced (if any) is back too.
    fn forget_backup(&mut self, b: &Path, dst: &Path) {
        if let Some(Backup {
            entry: Some(mut e), ..
        }) = self.backups.remove(b)
            && let Some(j) = &self.journal
        {
            // The older backup file goes back where the journal took it from.
            let older = match &e.action {
                JournalAction::Move {
                    backup_path: Some(o),
                    dst,
                    ..
                } => Some((o.clone(), dst.clone())),
                _ => None,
            };
            if older.is_none_or(|(o, at)| move_path(&o, &at).is_ok()) {
                let _ = j.cancel(&mut e);
            }
        }
        if let Some(p) = &mut self.plan
            && p.created == stable_path(dst)
            && !p.existed
        {
            p.existed = true;
            self.sync_entry();
        }
    }

    /// GNU's `un_backup` label: forget the source and put a backup back after
    /// failing to create the destination.
    fn un_backup(
        &mut self,
        dst_backup: Option<&Path>,
        earlier_file: &Option<PathBuf>,
        dst: &Path,
        src_sb: Option<&fs::Metadata>,
    ) -> bool {
        if earlier_file.is_none() {
            self.forget_created(src_sb);
        }
        if let Some(b) = dst_backup {
            // By the names actually renamed (see `Backup::real`).
            let (from, to) = self
                .backups
                .get(b)
                .map_or((b, dst), |r| (r.real.as_path(), r.real_dst.as_path()));
            crash_point();
            match fs::rename(from, to) {
                Err(e) => self.diag_err(&format!("cannot un-backup {}", qa(dst)), &e),
                Ok(()) => {
                    if self.x.verbose {
                        outln!("{} -> {} (unbackup)", qa(b), qa(dst));
                    }
                    self.forget_backup(b, dst);
                }
            }
        }
        false
    }

    /// GNU `copy_dir`: copies the entries of `src` (in inode order) into `dst`.
    #[allow(clippy::too_many_arguments)]
    fn copy_dir(
        &mut self,
        src: &Path,
        dst: &Path,
        new_dst: bool,
        src_sb: &fs::Metadata,
        ancestors: &[(u64, u64)],
        level: Level<'_>,
        first_dir_created: &mut bool,
        copy_into_self: &mut bool,
    ) -> bool {
        let names = match dir_names(src, true) {
            Ok(n) => n,
            Err(e) => {
                self.diag_err(&format!("cannot access {}", qa(src)), &e);
                return false;
            }
        };
        // -H follows command-line symlinks only.
        let child = Level {
            dereference: if level.dereference == Deref::CommandLine {
                Deref::Never
            } else {
                level.dereference
            },
            command_line_arg: false,
            parent: Some(src_sb),
        };
        let mut ok = true;
        let mut new_first = false;
        for name in names {
            let s = join(src, &name);
            let d = join(dst, &name);
            let mut first = *first_dir_created;
            let mut local_into_self = false;
            let mut renamed = false;
            ok &= self.copy_internal(
                &s,
                &d,
                i32::from(new_dst),
                child,
                ancestors,
                &mut first,
                &mut local_into_self,
                &mut renamed,
            );
            *copy_into_self |= local_into_self;
            // Copying into itself: going on would never end.
            if local_into_self {
                break;
            }
            new_first |= first;
        }
        *first_dir_created = new_first;
        ok
    }

    /// GNU `copy_reg`: copies the regular file (or, with `--copy-contents`,
    /// the contents of a special file) `src` to `dst`.
    #[allow(clippy::too_many_arguments)]
    fn copy_reg(
        &mut self,
        src: &Path,
        dst: &Path,
        dst_mode: u32,
        mut omitted: u32,
        new_dst: &mut bool,
        src_sb: &fs::Metadata,
        dereference: Deref,
        top: bool,
    ) -> bool {
        let mut src_mode = src_sb.mode();
        let mut data_copy_required = self.x.data_copy_required;
        self.method = CopyMethod::StandardCopy;

        let nofollow = if dereference == Deref::Never {
            libc::O_NOFOLLOW
        } else {
            0
        };
        let source = match open_path(src, libc::O_RDONLY | nofollow, 0) {
            Ok(f) => f,
            Err(e) => {
                self.diag_err(&format!("cannot open {} for reading", qa(src)), &e);
                return false;
            }
        };
        let src_open_sb = match source.metadata() {
            Ok(m) => m,
            Err(e) => {
                self.diag_err(&format!("cannot fstat {}", qa(src)), &e);
                return false;
            }
        };
        if !same_inode(src_sb, &src_open_sb) {
            self.diag(&format!(
                "skipping file {}, as it was replaced while being copied",
                qa(src)
            ));
            return false;
        }

        let mut dest: Option<File> = None;
        let mut dest_errno = 0;
        let mut journaled = false;
        if !*new_dst {
            // With the journal, a regular file written into is saved first:
            // open without truncating, save a copy, then truncate.
            let save = top
                && self.journal.is_some()
                && self.plan.as_ref().is_some_and(|p| p.backup.is_none())
                && fs::metadata(dst).is_ok_and(|m| m.is_file());
            let trunc = if data_copy_required && !save {
                libc::O_TRUNC
            } else {
                0
            };
            match open_path(dst, libc::O_WRONLY | trunc, 0) {
                Ok(f) => {
                    if save {
                        let real = fs::canonicalize(dst).unwrap_or_else(|_| dst.to_path_buf());
                        if let Err(e) = self.journal_copy(&real) {
                            self.journal_failed(dst, &e);
                            return false;
                        }
                        journaled = true;
                        if data_copy_required && let Err(e) = f.set_len(0) {
                            self.diag_err(&format!("cannot create regular file {}", qa(dst)), &e);
                            return false;
                        }
                    }
                    dest = Some(f);
                }
                Err(e) => dest_errno = errno(&e),
            }
            if dest.is_none() && dest_errno != libc::ENOENT && self.x.unlink_dest_after_failed_open
            {
                match self.remove_dst(dst, false, top) {
                    Ok(()) => {
                        if self.x.verbose {
                            outln!("removed {}", qa(dst));
                        }
                    }
                    Err(e) if errno(&e) == libc::ENOENT => {}
                    Err(e) => {
                        self.diag_err(&format!("cannot remove {}", qa(dst)), &e);
                        return false;
                    }
                }
                dest_errno = libc::ENOENT;
            }
            if dest.is_none() && dest_errno == libc::ENOENT {
                *new_dst = true;
            }
        }

        if *new_dst {
            #[cfg(target_os = "macos")]
            if data_copy_required && self.x.reflink != Reflink::Never {
                match self.try_clonefile(src, dst, src_mode, dst_mode) {
                    Some(true) => return true,
                    Some(false) => return false,
                    None => {}
                }
            }
            let open_mode = dst_mode & !omitted;
            let flags = libc::O_WRONLY | libc::O_CREAT;
            match open_path(dst, flags | libc::O_EXCL, open_mode) {
                Ok(f) => dest = Some(f),
                Err(e) => dest_errno = errno(&e),
            }
            // Writing through a dangling symlink cannot be done safely; only
            // POSIXLY_CORRECT asks for it.
            if dest.is_none()
                && dest_errno == libc::EEXIST
                && !self.x.move_mode
                && fs::read_link(dst).is_ok()
            {
                if self.x.open_dangling_dest_symlink {
                    match open_path(dst, flags, open_mode) {
                        Ok(f) => {
                            if let Some(p) = &mut self.plan
                                && top
                            {
                                p.created =
                                    fs::canonicalize(dst).unwrap_or_else(|_| dst.to_path_buf());
                                p.existed = false;
                                self.sync_entry();
                            }
                            dest = Some(f);
                        }
                        Err(e) => dest_errno = errno(&e),
                    }
                } else {
                    self.diag(&format!("not writing through dangling symlink {}", qa(dst)));
                    return false;
                }
            }
            if dest.is_none()
                && dest_errno == libc::EISDIR
                && dst.as_os_str().as_bytes().ends_with(b"/")
            {
                dest_errno = libc::ENOTDIR;
            }
        } else {
            omitted = 0;
        }

        let Some(dest) = dest else {
            self.diag_err(
                &format!("cannot create regular file {}", qa(dst)),
                &os_err(dest_errno),
            );
            return false;
        };

        // --attributes-only overrides --reflink.
        if data_copy_required && self.x.reflink != Reflink::Never {
            match clone_file(&dest, &source) {
                Ok(()) => {
                    data_copy_required = false;
                    self.method = CopyMethod::FicloneCoW;
                    let len = src_open_sb.len();
                    let t = &self.ctx.telemetry;
                    t.add_read(len);
                    t.add_written(len);
                }
                Err(e) => {
                    if !self.handle_clone_fail(src, dst, &dest, *new_dst, &e) {
                        return false;
                    }
                }
            }
        }

        let sb = if data_copy_required || self.x.preserve_ownership {
            match dest.metadata() {
                Ok(m) => Some(m),
                Err(e) => {
                    self.diag_err(&format!("cannot fstat {}", qa(dst)), &e);
                    return false;
                }
            }
        } else {
            None
        };

        let mut return_val = true;
        if data_copy_required
            && let Some(sb) = &sb
            && !self.copy_data(&source, &dest, src, dst, &src_open_sb, sb)
        {
            // Never leave a file the journal saved half-written.
            if journaled {
                self.restore_saved();
            }
            return false;
        }

        if self.x.preserve_timestamps {
            let times = src_times(src_sb);
            // SAFETY: `dest` is open and `times` is an array of two timespecs, as
            // futimens(2) requires.
            let r = cvt(unsafe { libc::futimens(dest.as_raw_fd(), times.as_ptr()) });
            if let Err(e) = r {
                self.diag_err(&format!("preserving times for {}", qa(dst)), &e);
                if self.x.require_preserve {
                    return false;
                }
            }
        }

        if self.x.preserve_ownership
            && let Some(sb) = &sb
            && !(src_sb.uid() == sb.uid() && src_sb.gid() == sb.gid())
        {
            match self.set_owner(dst, Some(&dest), src_sb, *new_dst, Some(sb)) {
                -1 => return false,
                0 => src_mode &= !(S_ISUID | S_ISGID | S_ISVTX),
                _ => {}
            }
        }

        if self.x.preserve_mode || self.x.move_mode {
            if !self.copy_mode(dst, Some(&dest), src_mode) && self.x.require_preserve {
                return_val = false;
            }
        } else if self.x.explicit_no_preserve_mode && *new_dst {
            if !self.set_mode(dst, Some(&dest), MODE_RW_UGO & !self.umask) {
                return_val = false;
            }
        } else if omitted != 0 {
            omitted &= !self.umask;
            if omitted != 0
                && let Err(e) = fchmod_file(&dest, dst_mode & !self.umask)
            {
                self.diag_err(&format!("preserving permissions for {}", qa(dst)), &e);
                if self.x.require_preserve {
                    return_val = false;
                }
            }
        }

        // SAFETY: into_raw_fd hands over the descriptor, so it is closed exactly once.
        if unsafe { libc::close(dest.into_raw_fd()) } < 0 {
            let e = io::Error::last_os_error();
            self.diag_err(&format!("failed to close {}", qa(dst)), &e);
            return_val = false;
        }
        self.ctx.telemetry.set_strategy(self.method.as_str());
        if !self.x.move_mode {
            self.ctx.telemetry.add_items(1);
        }
        return_val
    }

    /// After a failed copy into a file the journal saved: put it back.
    fn restore_saved(&mut self) {
        if let Some(p) = &mut self.plan
            && let Some(b) = p.backup.clone()
        {
            if p.in_place {
                // Written into: put the old contents back into the same inode.
                if let Ok(data) = fs::read(&b)
                    && fs::write(&p.created, data).is_ok()
                {
                    let _ = fs::remove_file(&b);
                    p.backup = None;
                }
            } else {
                let _ = fs::remove_file(&p.created);
                if move_path(Path::new(&b), &p.created).is_ok() {
                    p.backup = None;
                }
            }
            if p.backup.is_none() {
                p.in_place = false;
                self.sync_entry();
            }
        }
    }

    /// GNU `handle_clone_fail`: false when the copy must fail.
    fn handle_clone_fail(
        &self,
        src: &Path,
        dst: &Path,
        dest: &File,
        new_dst: bool,
        e: &io::Error,
    ) -> bool {
        let report = is_terminal_error(errno(e));
        let always = self.x.reflink == Reflink::Always;
        if always || report {
            self.diag_err(&format!("failed to clone {} from {}", qa(dst), qa(src)), e);
        }
        // Remove a destination --reflink=always created but cloned nothing into.
        if new_dst
            && always
            && (!report || lseek(dest, 0, libc::SEEK_END).is_ok_and(|n| n == 0))
            && let Err(e) = unlink_path(dst, false)
            && errno(&e) != libc::ENOENT
        {
            self.diag_err(&format!("cannot remove {}", qa(dst)), &e);
        }
        !(always || report)
    }

    /// The data part of `copy_reg`: sparse detection, holes, buffer sizes.
    fn copy_data(
        &mut self,
        source: &File,
        dest: &File,
        src: &Path,
        dst: &Path,
        src_open_sb: &fs::Metadata,
        sb: &fs::Metadata,
    ) -> bool {
        let mut buf_size = io_blksize(sb);
        let hole_size = sb.blksize();
        let scantype = infer_scantype(source, src_open_sb);
        if let ScanType::Error(e) = &scantype {
            self.diag_err(&format!("cannot lseek {}", qa(src)), e);
            return false;
        }
        let make_holes = sb.file_type().is_file()
            && (self.x.sparse == Sparse::Always
                || (self.x.sparse == Sparse::Auto && !matches!(scantype, ScanType::Plain)));
        if !make_holes {
            let blcm_max = isize::MAX as u64;
            let blcm = buffer_lcm(io_blksize(src_open_sb), buf_size, blcm_max);
            if src_open_sb.file_type().is_file() && src_open_sb.size() < buf_size {
                buf_size = src_open_sb.size() + 1;
            }
            buf_size += blcm - 1;
            buf_size -= buf_size % blcm;
            if buf_size == 0 || blcm_max < buf_size {
                buf_size = blcm;
            }
        }
        let buf_size = usize::try_from(buf_size).unwrap_or(1 << 20).max(1);
        let allow_reflink = self.x.reflink != Reflink::Never;
        let mut buf = Vec::new();
        match scantype {
            ScanType::Lseek(ext_start) => self.lseek_copy(
                source,
                dest,
                &mut buf,
                buf_size,
                hole_size,
                ext_start,
                src_open_sb.size(),
                if make_holes {
                    self.x.sparse
                } else {
                    Sparse::Never
                },
                allow_reflink,
                src,
                dst,
            ),
            _ => {
                let mut st = SparseState {
                    total_n_read: 0,
                    last_write_made_hole: false,
                };
                self.sparse_copy(
                    source,
                    dest,
                    &mut buf,
                    buf_size,
                    if make_holes { hole_size } else { 0 },
                    self.x.sparse == Sparse::Always,
                    allow_reflink,
                    src,
                    dst,
                    u64::MAX,
                    &mut st,
                ) && {
                    if st.last_write_made_hole
                        && let Err(e) = dest.set_len(st.total_n_read)
                    {
                        self.diag_err(&format!("failed to extend {}", qa(dst)), &e);
                        false
                    } else {
                        true
                    }
                }
            }
        }
    }

    /// GNU `create_hole`: a hole of `size` bytes at the current offset.
    fn create_hole(&self, dest: &File, dst: &Path, punch: bool, size: u64) -> bool {
        let end = match lseek(dest, size as i64, libc::SEEK_CUR) {
            Ok(e) => e,
            Err(e) => {
                self.diag_err(&format!("cannot lseek {}", qa(dst)), &e);
                return false;
            }
        };
        if punch && let Err(e) = punch_hole(dest, end - size as i64, size as i64) {
            self.diag_err(&format!("error deallocating {}", qa(dst)), &e);
            return false;
        }
        true
    }

    /// GNU `sparse_copy`: copies at most `max_n_read` bytes, turning runs of
    /// `hole_size` zero bytes into holes when `hole_size` is nonzero.
    #[allow(clippy::too_many_arguments)]
    fn sparse_copy(
        &mut self,
        source: &File,
        dest: &File,
        buf: &mut Vec<u8>,
        buf_size: usize,
        hole_size: u64,
        punch_holes: bool,
        allow_reflink: bool,
        src: &Path,
        dst: &Path,
        mut max_n_read: u64,
        st: &mut SparseState,
    ) -> bool {
        st.last_write_made_hole = false;
        st.total_n_read = 0;
        let t = std::sync::Arc::clone(&self.ctx.telemetry);

        // Not looking for holes: let the kernel copy (copy_file_range).
        #[cfg(target_os = "linux")]
        if hole_size == 0 && allow_reflink {
            while max_n_read > 0 {
                let copy_max: u64 = (isize::MAX as u64) >> 30 << 30;
                // SAFETY: both descriptors are open (owned by `source` and `dest`); null
                // offsets mean "use and update the file offsets".
                let n = unsafe {
                    libc::copy_file_range(
                        source.as_raw_fd(),
                        std::ptr::null_mut(),
                        dest.as_raw_fd(),
                        std::ptr::null_mut(),
                        max_n_read.min(copy_max) as usize,
                        0,
                    )
                };
                if n == 0 {
                    // /proc files claim to be empty here: fall back to read.
                    if st.total_n_read == 0 {
                        break;
                    }
                    return true;
                }
                if n < 0 {
                    let e = io::Error::last_os_error();
                    let en = errno(&e);
                    if st.total_n_read == 0 && (is_clone_notsup(en) || en == libc::ENOENT) {
                        break;
                    }
                    if en == libc::EINTR {
                        continue;
                    }
                    self.diag_err(&format!("error copying {} to {}", qa(src), qa(dst)), &e);
                    return false;
                }
                let n = n as u64;
                max_n_read -= n;
                st.total_n_read += n;
                t.add_read(n);
                t.add_written(n);
            }
        }
        let _ = allow_reflink;

        let mut make_hole = false;
        let mut psize: u64 = 0;
        let mut reader = source;
        while max_n_read > 0 {
            if buf.is_empty() {
                buf.resize(buf_size, 0);
            }
            let want = usize::try_from(max_n_read).map_or(buf_size, |m| m.min(buf_size));
            let n_read = match reader.read(&mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.diag_err(&format!("error reading {}", qa(src)), &e);
                    return false;
                }
            };
            max_n_read -= n_read as u64;
            st.total_n_read += n_read as u64;
            t.add_read(n_read as u64);

            // Walk the buffer in chunks of hole_size, coalescing writes and seeks.
            let mut csize = if hole_size > 0 {
                usize::try_from(hole_size).unwrap_or(buf_size)
            } else {
                buf_size
            };
            let mut n_left = n_read;
            let mut cpos = 0usize;
            let mut ppos = 0usize;
            while n_left > 0 {
                let prev_hole = make_hole;
                csize = csize.min(n_left);
                if hole_size > 0 && csize > 0 {
                    make_hole = is_nul(&buf[cpos..cpos + csize]);
                }
                let transition = make_hole != prev_hole && psize > 0;
                let last_chunk = (n_left == csize && !make_hole) || csize == 0;
                if transition || last_chunk {
                    if !transition {
                        psize += csize as u64;
                    }
                    if !prev_hole {
                        let end = ppos + psize as usize;
                        crash_point();
                        if let Err(e) = (&*dest).write_all(&buf[ppos..end]) {
                            self.diag_err(&format!("error writing {}", qa(dst)), &e);
                            return false;
                        }
                        t.add_written(psize);
                    } else if !self.create_hole(dest, dst, punch_holes, psize) {
                        return false;
                    }
                    ppos = cpos;
                    psize = csize as u64;
                    if last_chunk {
                        if csize == 0 {
                            n_left = 0;
                        }
                        if transition {
                            csize = 0;
                        } else {
                            psize = 0;
                        }
                    }
                } else {
                    psize += csize as u64;
                }
                n_left -= csize;
                cpos += csize;
            }
            st.last_write_made_hole = make_hole;
        }
        // A trailing hole, so later copies start at the right offset.
        if make_hole && !self.create_hole(dest, dst, punch_holes, psize) {
            return false;
        }
        true
    }

    /// GNU `lseek_copy`: copies extent by extent using `SEEK_DATA`/`SEEK_HOLE`.
    #[allow(clippy::too_many_arguments)]
    fn lseek_copy(
        &mut self,
        source: &File,
        dest: &File,
        buf: &mut Vec<u8>,
        buf_size: usize,
        hole_size: u64,
        mut ext_start: i64,
        src_total_size: u64,
        sparse_mode: Sparse,
        allow_reflink: bool,
        src: &Path,
        dst: &Path,
    ) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let mut src_total_size = src_total_size as i64;
            let (mut last_ext_start, mut last_ext_len, mut dest_pos) = (0i64, 0i64, 0i64);
            let mut wrote_hole_at_eof = true;
            let cannot_lseek = |me: &Self, e: &io::Error| {
                me.diag_err(&format!("cannot lseek {}", qa(src)), e);
                false
            };
            while ext_start >= 0 {
                let mut ext_end = match seek_data(source, ext_start, true) {
                    Ok(e) => e,
                    Err(e) if errno(&e) != libc::ENXIO => return cannot_lseek(self, &e),
                    Err(_) => {
                        let mut end = src_total_size;
                        if end <= ext_start {
                            // The input grew: get its current size.
                            src_total_size = match lseek(source, 0, libc::SEEK_END) {
                                Ok(s) => s,
                                Err(e) => return cannot_lseek(self, &e),
                            };
                            if src_total_size <= ext_start {
                                break;
                            }
                            end = src_total_size;
                        }
                        end
                    }
                };
                if src_total_size < ext_end {
                    src_total_size = ext_end;
                }
                if let Err(e) = lseek(source, ext_start, libc::SEEK_SET) {
                    return cannot_lseek(self, &e);
                }
                wrote_hole_at_eof = false;
                let ext_hole_size = ext_start - last_ext_start - last_ext_len;
                if ext_hole_size > 0 {
                    if sparse_mode != Sparse::Never {
                        if !self.create_hole(
                            dest,
                            dst,
                            sparse_mode == Sparse::Always,
                            ext_hole_size as u64,
                        ) {
                            return false;
                        }
                        wrote_hole_at_eof = true;
                    } else if let Err(e) = write_zeros(dest, ext_hole_size as u64) {
                        self.diag_err(&format!("{}: write failed", qf(dst)), &e);
                        return false;
                    }
                }
                if ext_end < ext_start {
                    ext_end = ext_start;
                }
                let ext_len = ext_end - ext_start;
                last_ext_start = ext_start;
                last_ext_len = ext_len;
                let mut st = SparseState {
                    total_n_read: 0,
                    last_write_made_hole: false,
                };
                if !self.sparse_copy(
                    source,
                    dest,
                    buf,
                    buf_size,
                    if sparse_mode != Sparse::Always {
                        0
                    } else {
                        hole_size
                    },
                    true,
                    allow_reflink,
                    src,
                    dst,
                    ext_len as u64,
                    &mut st,
                ) {
                    return false;
                }
                let n_read = st.total_n_read as i64;
                dest_pos = ext_start + n_read;
                if n_read > 0 {
                    wrote_hole_at_eof = st.last_write_made_hole;
                }
                if n_read < ext_len {
                    // The input shrank.
                    src_total_size = dest_pos;
                    break;
                }
                ext_start = match seek_data(source, dest_pos, false) {
                    Ok(s) => s,
                    Err(e) if errno(&e) == libc::ENXIO => -1,
                    Err(e) => return cannot_lseek(self, &e),
                };
            }
            // A hole at the end of the source: extend the copy to its size.
            if dest_pos < src_total_size || wrote_hole_at_eof {
                let res = if sparse_mode == Sparse::Never {
                    write_zeros(dest, (src_total_size - dest_pos).max(0) as u64)
                } else {
                    dest.set_len(src_total_size as u64)
                };
                if let Err(e) = res {
                    self.diag_err(&format!("failed to extend {}", qa(dst)), &e);
                    return false;
                }
            }
            if sparse_mode == Sparse::Always
                && dest_pos < src_total_size
                && let Err(e) = punch_hole(dest, dest_pos, src_total_size - dest_pos)
            {
                self.diag_err(&format!("error deallocating {}", qa(dst)), &e);
                return false;
            }
            true
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (
                source,
                dest,
                buf,
                buf_size,
                hole_size,
                ext_start,
                src_total_size,
                sparse_mode,
                allow_reflink,
                src,
                dst,
            );
            false
        }
    }

    /// macOS: a new file as an APFS clone (GNU's `fclonefileat` path).
    /// `None` when cloning was not possible and a plain copy should follow.
    #[cfg(target_os = "macos")]
    fn try_clonefile(
        &mut self,
        src: &Path,
        dst: &Path,
        src_mode: u32,
        dst_mode: u32,
    ) -> Option<bool> {
        let cloned_mode = src_mode & (S_ISVTX | S_IRWXUGO);
        let desired = if self.x.preserve_mode {
            src_mode & CHMOD_MODE_BITS
        } else {
            (if self.x.explicit_no_preserve_mode {
                MODE_RW_UGO
            } else {
                dst_mode
            }) & !self.umask
        };
        if cloned_mode & !desired != 0 {
            return None;
        }
        let (s, d) = (c_path(src).ok()?, c_path(dst).ok()?);
        const CLONE_NOFOLLOW: u32 = 0x0001;
        const CLONE_NOOWNERCOPY: u32 = 0x0002;
        let flags = CLONE_NOFOLLOW
            | if self.x.preserve_ownership {
                0
            } else {
                CLONE_NOOWNERCOPY
            };
        // SAFETY: `s` and `d` are NUL-terminated paths that outlive the call.
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), flags) } != 0 {
            let e = io::Error::last_os_error();
            let report = is_terminal_error(errno(&e));
            if self.x.reflink == Reflink::Always || report {
                self.diag_err(&format!("failed to clone {} from {}", qa(dst), qa(src)), &e);
                return Some(false);
            }
            return None;
        }
        self.method = CopyMethod::ClonefileCoW;
        if !self.x.preserve_timestamps {
            let now = [timespec(0, libc::UTIME_NOW), timespec(0, libc::UTIME_NOW)];
            if let Err(e) = utimens_path(dst, &now, true) {
                self.diag_err(&format!("updating times for {}", qa(dst)), &e);
                return Some(false);
            }
        }
        if desired & !cloned_mode != 0 && !self.copy_mode(dst, None, desired) {
            return Some(false);
        }
        let t = &self.ctx.telemetry;
        t.set_strategy(self.method.as_str());
        t.add_items(1);
        Some(true)
    }
}

/// Absolute form of `target` with its parent resolved through symlinks, for
/// "is this inside that directory?" checks on paths that may not exist yet.
fn resolve_for_containment(target: &Path) -> PathBuf {
    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match (fs::canonicalize(parent), target.file_name()) {
        (Ok(p), Some(name)) => p.join(name),
        _ => std::path::absolute(target).unwrap_or_else(|_| target.to_path_buf()),
    }
}

/// GNU `write_zeros`.
fn write_zeros(dest: &File, mut n: u64) -> io::Result<()> {
    let zeros = vec![0u8; IO_BUFSIZE as usize];
    while n > 0 {
        let k = n.min(IO_BUFSIZE) as usize;
        (&*dest).write_all(&zeros[..k])?;
        n -= k as u64;
    }
    Ok(())
}

/// gnulib `file_name_concat` for directory entries: `dir/name`, without
/// doubling a trailing slash.
fn join(dir: &Path, name: &OsStr) -> PathBuf {
    let mut s = dir.as_os_str().as_bytes().to_vec();
    if !s.ends_with(b"/") {
        s.push(b'/');
    }
    s.extend_from_slice(name.as_bytes());
    PathBuf::from(OsStr::from_bytes(&s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_or_dotdot_matches_gnulib() {
        assert!(dot_or_dotdot(b"."));
        assert!(dot_or_dotdot(b".."));
        assert!(dot_or_dotdot(b"../"));
        assert!(dot_or_dotdot(b"./"));
        assert!(!dot_or_dotdot(b".a"));
        assert!(!dot_or_dotdot(b"..."));
        assert!(!dot_or_dotdot(b"a"));
    }

    #[test]
    fn temp_names_stay_in_the_destination_directory() {
        let t = samedir_temp(Path::new("d/sub/f"), 0);
        let s = t.to_str().unwrap();
        assert!(s.starts_with("d/sub/Cu") && s.len() == "d/sub/Cu".len() + 6);
        let t = samedir_temp(Path::new("f"), 1);
        assert!(t.to_str().unwrap().starts_with("Cu"));
    }

    #[test]
    fn buffer_lcm_caps() {
        assert_eq!(buffer_lcm(4096, 131072, u64::MAX), 131072);
        assert_eq!(buffer_lcm(3, 4, 10), 3);
        assert_eq!(buffer_lcm(0, 0, 10), 8192);
    }
}
