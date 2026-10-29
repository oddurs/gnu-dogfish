//! `tail -f`: following files by descriptor or by name, by polling (GNU's
//! `tail_forever`), and the checks shared with the inotify loop.

use std::ffi::{OsStr, OsString};
use std::fs::Metadata;
use std::io::{self, SeekFrom};
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt;

use super::{
    COPY_A_BUFFER, COPY_TO_EOF, Desc, Fatal, R, Tail, is_fifo, is_reg, is_tailable_file_type,
};
use crate::common::interval::duration_from_secs;
use crate::common::quote::{qa, qf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FollowMode {
    /// Follow the name: reopen it when the file is renamed, removed or
    /// replaced (log rotation).
    Name,
    /// Follow the descriptor opened at the start, wherever the file goes.
    Descriptor,
}

/// GNU's `struct File_spec`: one operand and what tail last saw of it.
pub(super) struct FileSpec {
    /// The operand, or `-` for standard input.
    pub(super) name: OsString,
    pub(super) size: u64,
    pub(super) mtime: (i64, i64),
    pub(super) dev: u64,
    pub(super) ino: u64,
    pub(super) mode: u32,
    /// The name is never checked again (a directory, say).
    pub(super) ignore: bool,
    /// On a file system inotify cannot watch (`fremote`).
    pub(super) remote: bool,
    /// Exists, is readable and is of a type tail can follow.
    pub(super) tailable: bool,
    pub(super) fd: Option<Desc>,
    /// The errno last seen for it; 0 when open, -1 for tail's own reasons.
    pub(super) errnum: i32,
    /// 1 if `O_NONBLOCK` is clear, 0 if set, -1 if not known.
    pub(super) blocking: i8,
    /// The inotify watch on the file, and on its directory (by name).
    pub(super) wd: i32,
    pub(super) parent_wd: i32,
    /// Offset of the last component in `name`.
    pub(super) basename_start: usize,
    pub(super) n_unchanged_stats: u64,
}

impl FileSpec {
    pub(super) fn new(name: OsString) -> Self {
        FileSpec {
            name,
            size: 0,
            mtime: (0, 0),
            dev: 0,
            ino: 0,
            mode: 0,
            ignore: false,
            remote: false,
            tailable: false,
            fd: None,
            errnum: 0,
            blocking: -1,
            wd: -1,
            parent_wd: -1,
            basename_start: 0,
            n_unchanged_stats: 0,
        }
    }

    pub(super) fn is_stdin(&self) -> bool {
        self.name == "-"
    }

    /// The name in messages and headers.
    pub(super) fn pretty_name(&self) -> OsString {
        if self.is_stdin() {
            OsString::from("standard input")
        } else {
            self.name.clone()
        }
    }

    pub(super) fn raw_fd(&self) -> Option<RawFd> {
        self.fd.as_ref().map(Desc::raw)
    }

    /// GNU's `record_open_fd`.
    pub(super) fn record_open_fd(&mut self, d: Desc, size: u64, st: &Metadata, blocking: i8) {
        self.fd = Some(d);
        self.size = size;
        self.mtime = mtime(st);
        self.dev = st.dev();
        self.ino = st.ino();
        self.mode = st.mode();
        self.blocking = blocking;
        self.n_unchanged_stats = 0;
        self.ignore = false;
    }
}

pub(super) fn mtime(st: &Metadata) -> (i64, i64) {
    (st.mtime(), st.mtime_nsec())
}

/// `kill (pid, 0)`.
pub(super) fn kill0(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: kill(2) with signal 0 only checks that the process exists;
    // it takes no pointers.
    if unsafe { libc::kill(pid, 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether the `--pid` writer is gone: `kill` fails, other than for lack
/// of permission.
pub(super) fn writer_is_dead(pid: libc::pid_t) -> bool {
    pid != 0 && kill0(pid).is_err_and(|e| e.raw_os_error() != Some(libc::EPERM))
}

/// GNU's `fremote`: whether the file open on `fd` is on a file system that
/// inotify cannot be trusted with (remote, or of an unknown type).
#[cfg(target_os = "linux")]
pub(super) fn fremote(t: &mut Tail, fd: Option<RawFd>, name: &OsStr) -> bool {
    let Some(fd) = fd else {
        return true;
    };
    // SAFETY: plain old data: statfs is a C struct of integers, for which
    // all zeroes is a valid value.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: descriptor only: `fd` is open (owned by the FileSpec), and
    // fstatfs writes one statfs through a pointer to the live local `buf`.
    if unsafe { libc::fstatfs(fd, &mut buf) } != 0 {
        let e = io::Error::last_os_error();
        // fstatfs fails with ENOSYS on pipes on some kernels.
        if e.raw_os_error() != Some(libc::ENOSYS) {
            t.diag_io(
                &e,
                format_args!(
                    "cannot determine location of {}. reverting to polling",
                    qa(name)
                ),
            );
        }
        return true;
    }
    #[allow(clippy::unnecessary_cast)]
    let magic = buf.f_type as u64 & 0xFFFF_FFFF;
    !is_local_fs_type(magic)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn fremote(_: &mut Tail, _: Option<RawFd>, _: &OsStr) -> bool {
    true
}

/// gnulib's `is_local_fs_type` (from coreutils' `fs-is-local.h`): the Linux
/// `f_type` values known to be local file systems. Any other type, remote
/// or unknown, is polled.
#[cfg(target_os = "linux")]
fn is_local_fs_type(magic: u64) -> bool {
    const LOCAL: &[u64] = &[
        0x5A3C69F0, 0xADF5, 0xADFF, 0x09041934, 0x0187, 0x13661366, 0x42465331, 0x62646576,
        0x1BADFACE, 0x6C6F6F70, 0xCAFE4A11, 0x42494E4D, 0x9123683E, 0x73727279, 0x0027E0EB,
        0x63677270, 0x012FF7B7, 0x62656570, 0x28CD3D45, 0x453DCD28, 0x64646178, 0x64626720, 0x1373,
        0x454D444D, 0x1CD1, 0x444D4142, 0xF15F, 0xDE5E81E4, 0x00414A53, 0xE0F5E1E2, 0x2011BAB0,
        0x45584653, 0x5DF5, 0x137D, 0xEF53, 0xEF51, 0xF2F52010, 0x4006, 0x0BAD1DEA, 0x4244, 0x482B,
        0x4858, 0x00C0FFEE, 0xF995E849, 0x958458F6, 0x11307854, 0x2BAD1DEA, 0x9660, 0x4004, 0x4000,
        0x07C0, 0x72B6, 0x3153464A, 0xC97E8168, 0x5346314D, 0x137F, 0x138F, 0x2468, 0x2478, 0x4D5A,
        0x19800202, 0x4D44, 0x3434, 0x6E736673, 0x5346544E, 0x9FA1, 0xC7571590, 0x9FA0, 0x6165676C,
        0x002F, 0x68191122, 0x858458F6, 0x07655821, 0x52654973, 0x7275, 0x67596969, 0x5DCA2DF5,
        0x5345434D, 0x73636673, 0xF97CFF8C, 0x43415D53, 0x534F434B, 0x73717368, 0x62656572,
        0x012FF7B6, 0x012FF7B5, 0x01021994, 0x74726163, 0x24051905, 0x15013346, 0x00011954,
        0x54190100, 0x9FA2, 0x01021997, 0x565A4653, 0x53464846, 0xABBA1974, 0x012FF7B4, 0x58465342,
        0x012FD16D, 0x0033, 0x2FC12FC1, 0x5A4F4653, 0x58295829,
    ];
    LOCAL.contains(&magic)
}

/// Sets or clears `O_NONBLOCK` on `fd`.
fn set_blocking(fd: RawFd, blocking: bool) -> io::Result<()> {
    // SAFETY: descriptor only: fcntl(F_GETFL) on an open descriptor.
    let old = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if old < 0 {
        return Err(io::Error::last_os_error());
    }
    let new = if blocking {
        old
    } else {
        old | libc::O_NONBLOCK
    };
    // SAFETY: descriptor only: fcntl(F_SETFL) with integer flags.
    if new != old && unsafe { libc::fcntl(fd, libc::F_SETFL, new) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Tail {
    /// GNU's `recheck`: reopens `f`'s name and reports what changed.
    pub(super) fn recheck(&mut self, f: &mut FileSpec, blocking: bool) -> R<()> {
        let is_stdin = f.is_stdin();
        let was_tailable = f.tailable;
        let prev_errnum = f.errnum;
        let pretty = f.pretty_name();
        let opened = Desc::open(&f.name, is_stdin, !blocking);
        // If the open fails because the file doesn't exist, the file is not
        // tailable.
        f.tailable = !(self.a.reopen_inaccessible_files && opened.is_err());

        let mut ok = true;
        let mut new_stats = None;
        let is_symlink = !self.a.disable_inotify
            && std::fs::symlink_metadata(&f.name).is_ok_and(|m| m.file_type().is_symlink());
        if is_symlink {
            // A regular file replaced by a symlink: inotify avoids symlinks.
            ok = false;
            f.errnum = -1;
            f.ignore = true;
            self.diag(format_args!(
                "{} has been replaced with an untailable symbolic link",
                qa(&pretty)
            ));
        } else {
            match opened.as_ref().map_err(clone_err).and_then(Desc::fstat) {
                Err(e) => {
                    ok = false;
                    let errnum = e.raw_os_error().unwrap_or(libc::EIO);
                    f.errnum = errnum;
                    if !f.tailable {
                        if was_tailable {
                            self.diag_io(
                                &e,
                                format_args!("{} has become inaccessible", qa(&pretty)),
                            );
                        }
                    } else if prev_errnum != errnum {
                        self.diag_io(&e, qf(&pretty));
                    }
                }
                Ok(st) if !is_tailable_file_type(st.mode()) => {
                    ok = false;
                    f.errnum = -1;
                    f.tailable = false;
                    f.ignore = !(self.a.reopen_inaccessible_files
                        && self.a.follow_mode == FollowMode::Name);
                    if was_tailable || prev_errnum != f.errnum {
                        let giving_up = if f.ignore {
                            "; giving up on this name"
                        } else {
                            ""
                        };
                        self.diag(format_args!(
                            "{} has been replaced with an untailable file{giving_up}",
                            qa(&pretty)
                        ));
                    }
                }
                Ok(st) => {
                    let fd = opened.as_ref().ok().map(Desc::raw);
                    f.remote = fremote(self, fd, &pretty);
                    if f.remote && !self.a.disable_inotify {
                        ok = false;
                        f.errnum = -1;
                        self.diag(format_args!(
                            "{} has been replaced with an untailable remote file",
                            qa(&pretty)
                        ));
                        f.ignore = true;
                        f.remote = true;
                    } else {
                        f.errnum = 0;
                    }
                    new_stats = Some(st);
                }
            }
        }

        let mut new_file = false;
        let mut opened = opened.ok();
        if !ok {
            drop(opened.take());
            f.fd = None;
        } else if prev_errnum != 0 && prev_errnum != libc::ENOENT {
            new_file = true;
            self.diag(format_args!("{} has become accessible", qa(&pretty)));
        } else if f.fd.is_none() {
            // A new file even if the inode is the same: the file was
            // missing on the previous check.
            new_file = true;
            self.diag(format_args!(
                "{} has appeared;  following new file",
                qa(&pretty)
            ));
        } else if new_stats
            .as_ref()
            .is_some_and(|st| f.ino != st.ino() || f.dev != st.dev())
        {
            // Replaced (log rotation, say): tail the new one.
            new_file = true;
            self.diag(format_args!(
                "{} has been replaced;  following new file",
                qa(&pretty)
            ));
            f.fd = None;
        } else {
            // No change: close the new descriptor.
            drop(opened.take());
        }

        if new_file && let (Some(d), Some(st)) = (opened, new_stats) {
            // Start at the beginning of the file.
            if is_reg(st.mode()) {
                self.xlseek(&d, SeekFrom::Start(0), &pretty)?;
            }
            let blocking = if is_stdin { -1 } else { i8::from(blocking) };
            f.record_open_fd(d, 0, &st, blocking);
        }
        Ok(())
    }
}

fn clone_err(e: &io::Error) -> io::Error {
    match e.raw_os_error() {
        Some(n) => io::Error::from_raw_os_error(n),
        None => io::Error::new(e.kind(), e.to_string()),
    }
}

/// GNU's `any_live_files`: whether any file is open or may be reopened.
pub(super) fn any_live_files(t: &Tail, files: &[FileSpec]) -> bool {
    // With -F, ignored files may later be replaced with new ones.
    if t.a.reopen_inaccessible_files && t.a.follow_mode == FollowMode::Name {
        return true;
    }
    files
        .iter()
        .any(|f| f.fd.is_some() || (!f.ignore && t.a.reopen_inaccessible_files))
}

/// GNU's `ignore_fifo_and_pipe`: with -f, a `-` operand that is a pipe or
/// FIFO is not followed (POSIX). Returns the number of files left.
pub(super) fn ignore_fifo_and_pipe(files: &mut [FileSpec]) -> usize {
    let mut n_viable = 0;
    for f in files.iter_mut() {
        if f.is_stdin() && !f.ignore && f.fd.is_some() && is_fifo(f.mode) {
            f.fd = None;
            f.ignore = true;
        } else {
            n_viable += 1;
        }
    }
    n_viable
}

/// gnulib `iopoll (-1, STDOUT_FILENO, false)` for a broken output pipe:
/// whether standard output's reader has gone away.
pub(super) fn output_broken() -> bool {
    let mut pfd = libc::pollfd {
        fd: libc::STDOUT_FILENO,
        events: libc::POLLRDBAND,
        revents: 0,
    };
    loop {
        // SAFETY: poll(2) reads and writes one pollfd through a pointer to
        // the live local `pfd`.
        let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
        if ret < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return ret > 0 && pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0;
    }
}

/// GNU's `die_pipe`: dies of SIGPIPE, or exits 1 when it is ignored.
pub(super) fn die_pipe() -> Fatal {
    // SAFETY: raise(3) takes no pointers; SIGPIPE's disposition is the
    // default (the process ends) or ignored (raise returns).
    unsafe { libc::raise(libc::SIGPIPE) };
    Fatal(1)
}

/// GNU's `check_output_alive`.
fn check_output_alive(t: &Tail) -> R<()> {
    if t.monitor_output && output_broken() {
        return Err(die_pipe());
    }
    Ok(())
}

/// GNU's `tail_forever`: polls each file with fstat (and, by name, an
/// occasional open) and prints what was appended, sleeping between rounds
/// in which nothing changed.
pub(super) fn tail_forever(t: &mut Tail, files: &mut [FileSpec]) -> R<()> {
    let n_files = files.len();
    let pid = t.a.pid;
    // Blocking reads, when that is easy.
    let blocking = pid == 0
        && t.a.follow_mode == FollowMode::Descriptor
        && n_files == 1
        && files[0].fd.is_some()
        && !is_reg(files[0].mode);
    let mut last = n_files - 1;
    let mut writer_dead = false;
    loop {
        let mut any_input = false;
        for (i, f) in files.iter_mut().enumerate() {
            if f.ignore {
                continue;
            }
            let Some(fd) = f.raw_fd() else {
                t.recheck(f, blocking)?;
                continue;
            };
            let name = f.pretty_name();
            let mode = f.mode;

            if f.blocking != i8::from(blocking) {
                match set_blocking(fd, blocking) {
                    Ok(()) => f.blocking = i8::from(blocking),
                    // With the append-only attribute.
                    Err(e) if is_reg(f.mode) && e.raw_os_error() == Some(libc::EPERM) => {}
                    Err(e) => {
                        return Err(t.fatal_io(
                            &e,
                            format_args!("{}: cannot change nonblocking mode", qf(&name)),
                        ));
                    }
                }
            }

            let mut read_unchanged = false;
            let mut stats = None;
            // -1 (not known) counts as blocking, as in GNU.
            if f.blocking == 0 {
                let st = match f.fd.as_ref().map(Desc::fstat) {
                    Some(Ok(st)) => st,
                    Some(Err(e)) => {
                        f.fd = None;
                        f.errnum = e.raw_os_error().unwrap_or(libc::EIO);
                        t.diag_io(&e, qf(&name));
                        continue;
                    }
                    None => continue,
                };
                if f.mode == st.mode()
                    && (!is_reg(st.mode()) || f.size == st.size())
                    && f.mtime == mtime(&st)
                {
                    let unchanged = f.n_unchanged_stats;
                    f.n_unchanged_stats += 1;
                    if t.a.max_n_unchanged_stats_between_opens <= unchanged
                        && t.a.follow_mode == FollowMode::Name
                    {
                        let b = f.blocking != 0;
                        t.recheck(f, b)?;
                        f.n_unchanged_stats = 0;
                    }
                    if Some(fd) != f.raw_fd() || is_reg(st.mode()) || 1 < n_files {
                        continue;
                    }
                    read_unchanged = true;
                }

                // The file changed: print what we can.
                f.mtime = mtime(&st);
                f.mode = st.mode();
                if !read_unchanged {
                    f.n_unchanged_stats = 0;
                }
                // A heuristic: the file may also have been truncated and
                // written to.
                if is_reg(mode) && st.size() < f.size {
                    t.diag(format_args!("{}: file truncated", qf(&name)));
                    // Assume truncation to 0: output all "new" data.
                    if let Some(d) = &f.fd {
                        t.xlseek(d, SeekFrom::Start(0), &name)?;
                    }
                    f.size = 0;
                }
                if i != last {
                    if t.print_headers {
                        t.write_header(&name)?;
                    }
                    last = i;
                }
                stats = Some(st);
            }

            // Don't read more than st_size on network file systems.
            let bytes_to_read = if f.blocking != 0 {
                COPY_A_BUFFER
            } else if is_reg(mode)
                && f.remote
                && let Some(st) = &stats
            {
                st.size().saturating_sub(f.size)
            } else {
                COPY_TO_EOF
            };
            let Some(d) = &f.fd else {
                continue;
            };
            let bytes_read = t.dump_remainder(false, &name, d, bytes_to_read)?;
            if read_unchanged && bytes_read > 0 {
                f.n_unchanged_stats = 0;
            }
            any_input |= bytes_read != 0;
            f.size += bytes_read;
        }

        if !any_live_files(t, files) {
            t.diag("no files remaining");
            return Ok(());
        }
        if !any_input || blocking {
            t.out.flush()?;
        }
        check_output_alive(t)?;

        // If nothing was read, sleep and/or check for a dead writer.
        if !any_input {
            if writer_dead {
                return Ok(());
            }
            // Once the writer is dead, read the files once more to avoid a
            // race.
            writer_dead = writer_is_dead(pid);
            if !writer_dead {
                std::thread::sleep(duration_from_secs(t.a.sleep_interval));
            }
        }
    }
}

/// GNU's `check_fspec` (inotify): prints new data of `files[i]`; `prev` is
/// the file output came from last, for headers.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) fn check_fspec(
    t: &mut Tail,
    files: &mut [FileSpec],
    i: usize,
    prev: &mut usize,
) -> R<()> {
    let f = &mut files[i];
    let Some(d) = &f.fd else {
        return Ok(());
    };
    let name = f.pretty_name();
    let st = match d.fstat() {
        Ok(st) => st,
        Err(e) => {
            f.errnum = e.raw_os_error().unwrap_or(libc::EIO);
            f.fd = None;
            return Ok(());
        }
    };
    // A heuristic, as with polling; inotify usually reports the truncation
    // and the write separately.
    if is_reg(f.mode) && st.size() < f.size {
        t.diag(format_args!("{}: file truncated", qf(&name)));
        t.xlseek(d, SeekFrom::Start(0), &name)?;
        f.size = 0;
    } else if is_reg(f.mode) && st.size() == f.size && f.mtime == mtime(&st) {
        return Ok(());
    }
    let want_header = t.print_headers && i != *prev;
    let bytes_read = t.dump_remainder(want_header, &name, d, COPY_TO_EOF)?;
    f.size += bytes_read;
    if bytes_read > 0 {
        *prev = i;
        t.out.flush()?;
    }
    Ok(())
}
