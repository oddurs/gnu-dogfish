//! `tail -f` with inotify (Linux): GNU's `tail_forever_inotify`. It waits
//! for events on the files (and, by name, on their directories) instead of
//! polling, and returns to let the caller fall back to polling when inotify
//! cannot do the job.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

use super::follow::{FileSpec, FollowMode, check_fspec, die_pipe, writer_is_dead};
use super::{Fatal, R, Tail, is_fifo, is_reg};
use crate::common::quote::qa;

/// Whether inotify can follow these files (GNU's checks in `main`):
/// standard input, remote files, no open file, symlinks (inotify reports
/// targets, not names), devices (no events), or by descriptor after a file
/// failed to open, all mean polling.
pub(super) fn usable(t: &Tail, files: &[FileSpec], ok: bool) -> bool {
    let tailable_stdin = files.iter().any(|f| !f.ignore && f.is_stdin());
    let any_remote = files.iter().any(|f| f.fd.is_some() && f.remote);
    let any_non_remote = files.iter().any(|f| f.fd.is_some() && !f.remote);
    let any_symlinks = files
        .iter()
        .any(|f| std::fs::symlink_metadata(&f.name).is_ok_and(|m| m.file_type().is_symlink()));
    let any_non_regular_fifo = files
        .iter()
        .any(|f| f.fd.is_some() && !is_reg(f.mode) && !is_fifo(f.mode));
    !(tailable_stdin
        || any_remote
        || !any_non_remote
        || any_symlinks
        || any_non_regular_fifo
        || (!ok && t.a.follow_mode == FollowMode::Descriptor))
}

/// An inotify instance.
struct Inotify {
    fd: File,
}

impl Inotify {
    fn new() -> io::Result<Inotify> {
        // SAFETY: inotify_init1 takes only flags.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: descriptor only: `fd` was just returned by inotify_init1
        // and is owned by nothing else.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Inotify {
            fd: File::from(owned),
        })
    }

    fn add_watch(&self, path: &OsStr, mask: u32) -> io::Result<i32> {
        let c = CString::new(path.as_bytes())
            .map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;
        // SAFETY: C path: `c` is a NUL-terminated string that outlives the
        // call; the descriptor is the live inotify instance.
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), mask) };
        if wd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(wd)
        }
    }

    fn rm_watch(&self, wd: i32) {
        // SAFETY: descriptor only: the live inotify instance and a watch
        // number; an invalid one only makes the call fail.
        unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
    }
}

/// One decoded `struct inotify_event`.
struct Event<'a> {
    wd: i32,
    mask: u32,
    /// The name's field length (`ev->len`), 0 for an event on the watched
    /// object itself.
    len: u32,
    name: &'a [u8],
}

/// Decodes the event at `off` in `buf`; returns it and the next offset.
fn parse_event(buf: &[u8], off: usize) -> Option<(Event<'_>, usize)> {
    let head = buf.get(off..off + 16)?;
    let word = |i: usize| [head[i], head[i + 1], head[i + 2], head[i + 3]];
    let wd = i32::from_ne_bytes(word(0));
    let mask = u32::from_ne_bytes(word(4));
    let len = u32::from_ne_bytes(word(12));
    let name_field = buf.get(off + 16..off + 16 + len as usize)?;
    let name_len = name_field
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name_field.len());
    Some((
        Event {
            wd,
            mask,
            len,
            name: &name_field[..name_len],
        },
        off + 16 + len as usize,
    ))
}

/// gnulib's `dir_len`: the length of the directory part of `name`, without
/// the slashes before the last component (but keeping a leading `/`).
fn dir_len(name: &[u8]) -> usize {
    let prefix = usize::from(name.first() == Some(&b'/'));
    let mut length = last_component(name);
    while prefix < length && name[length - 1] == b'/' {
        length -= 1;
    }
    length
}

/// gnulib's `last_component`: the offset of the last component of `name`.
fn last_component(name: &[u8]) -> usize {
    let mut base = name.iter().take_while(|&&b| b == b'/').count();
    let mut last_was_slash = false;
    for (i, &b) in name.iter().enumerate().skip(base) {
        if b == b'/' {
            last_was_slash = true;
        } else if last_was_slash {
            base = i;
            last_was_slash = false;
        }
    }
    base
}

/// Sets up inotify and follows the files with it. Returns the errno to
/// report with `inotify cannot be used, reverting to polling` (0 when
/// inotify started but must give way to polling); `Err` ends tail.
pub(super) fn follow(t: &mut Tail, files: &mut [FileSpec]) -> R<i32> {
    let ino = match Inotify::new() {
        Ok(ino) => ino,
        Err(e) => return Ok(e.raw_os_error().unwrap_or(libc::EIO)),
    };
    // Flush the initial output now: the loop flushes only after writing.
    t.out.flush()?;
    tail_forever_inotify(t, &ino, files)?;
    Ok(0)
}

/// GNU's `tail_forever_inotify`: `Ok` means revert to polling.
fn tail_forever_inotify(t: &mut Tail, ino: &Inotify, f: &mut [FileSpec]) -> R<()> {
    let n_files = f.len();
    let by_name = t.a.follow_mode == FollowMode::Name;
    let pid = t.a.pid;
    let mut wd_to_name: HashMap<i32, usize> = HashMap::with_capacity(n_files);
    let mut found_watchable_file = false;
    let mut tailed_but_unwatchable = false;
    let mut found_unwatchable_dir = false;
    let mut no_inotify_resources = false;
    let mut writer_dead = false;

    // The events watched on files (not directories).
    let mut wd_mask = libc::IN_MODIFY;
    if by_name {
        wd_mask |= libc::IN_ATTRIB | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;
    }

    // Watch each file and, by name, its directory too, so that a file that
    // reappears can be watched again.
    for (i, fi) in f.iter_mut().enumerate() {
        if fi.ignore {
            continue;
        }
        fi.wd = -1;
        if by_name {
            let name = fi.name.as_bytes();
            let dirlen = dir_len(name);
            fi.basename_start = last_component(name);
            let dir = if dirlen > 0 {
                OsStr::from_bytes(&name[..dirlen])
            } else {
                OsStr::new(".")
            };
            // The same directory may be added more than once: it gets the
            // same watch.
            let dir_mask = libc::IN_CREATE
                | libc::IN_DELETE
                | libc::IN_MOVED_TO
                | libc::IN_ATTRIB
                | libc::IN_DELETE_SELF;
            match ino.add_watch(dir, dir_mask) {
                Ok(wd) => fi.parent_wd = wd,
                Err(e) => {
                    fi.parent_wd = -1;
                    if e.raw_os_error() != Some(libc::ENOSPC) {
                        let msg = format!("cannot watch parent directory of {}", qa(&fi.name));
                        t.diag_io(&e, msg);
                    } else {
                        t.diag("inotify resources exhausted");
                    }
                    found_unwatchable_dir = true;
                    break;
                }
            }
        }
        match ino.add_watch(&fi.name, wd_mask) {
            Ok(wd) => {
                fi.wd = wd;
                wd_to_name.entry(wd).or_insert(i);
                found_watchable_file = true;
            }
            Err(e) => {
                if fi.fd.is_some() {
                    tailed_but_unwatchable = true;
                }
                let errnum = e.raw_os_error().unwrap_or(libc::EIO);
                if errnum == libc::ENOSPC || errnum == libc::ENOMEM {
                    no_inotify_resources = true;
                    t.diag("inotify resources exhausted");
                    break;
                } else if errnum != fi.errnum {
                    let msg = format!("cannot watch {}", qa(&fi.name));
                    t.diag_io(&e, msg);
                }
            }
        }
    }

    // Revert to polling without inotify resources, when a directory cannot
    // be watched, or when a file tailed by descriptor cannot be watched.
    if no_inotify_resources || found_unwatchable_dir || (!by_name && tailed_but_unwatchable) {
        return Ok(());
    }
    if !by_name && !found_watchable_file {
        return Err(Fatal(1));
    }

    let mut prev = n_files - 1;

    // Check the files again: new files or data may have come before the
    // watches.
    for i in 0..n_files {
        if f[i].ignore {
            continue;
        }
        if by_name {
            t.recheck(&mut f[i], false)?;
        } else if f[i].fd.is_some() {
            // A file replaced since it was tailed: the watch is on the new
            // one, so poll the original descriptor instead.
            if let Ok(st) = std::fs::metadata(&f[i].name) {
                use std::os::unix::fs::MetadataExt;
                if f[i].dev != st.dev() || f[i].ino != st.ino() {
                    let msg = format!("{} was replaced", qa(f[i].pretty_name()));
                    t.diag(msg);
                    return Ok(());
                }
            }
        }
        check_fspec(t, f, i, &mut prev)?;
    }

    let mut evbuf = vec![0u8; 64 * 1024];
    let mut len = 0usize;
    let mut off = 0usize;
    loop {
        // By name without --retry, once the last file is gone, stop.
        if by_name && !t.a.reopen_inaccessible_files && wd_to_name.is_empty() {
            t.diag("no files remaining");
            return Err(Fatal(1));
        }

        if len <= off {
            // Wait for events; with --pid, wake up to check the writer, and
            // with a pipe on standard output, notice its reader leaving.
            let mut pfd = [
                libc::pollfd {
                    fd: ino.fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: libc::STDOUT_FILENO,
                    events: 0,
                    revents: 0,
                },
            ];
            let nfds = 1 + libc::nfds_t::from(t.monitor_output);
            loop {
                let mut delay: libc::c_int = -1;
                if pid != 0 {
                    if writer_dead {
                        return Err(Fatal(0));
                    }
                    writer_dead = writer_is_dead(pid);
                    let s = t.a.sleep_interval;
                    if writer_dead || s <= 0.0 {
                        delay = 0;
                    } else if s < f64::from(i32::MAX / 1000 - 1) {
                        delay = (s * 1000.0).ceil() as libc::c_int;
                    }
                }
                pfd[1].revents = 0;
                // SAFETY: poll(2) reads and writes `nfds` (at most 2)
                // pollfds in the live local array `pfd`.
                let ret = unsafe { libc::poll(pfd.as_mut_ptr(), nfds, delay) };
                if ret < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(t.fatal_io(&e, "error waiting for inotify and output events"));
                }
                if ret > 0 {
                    break;
                }
            }
            if pfd[1].revents != 0 {
                return Err(die_pipe());
            }
            len = loop {
                match (&ino.fd).read(&mut evbuf) {
                    Ok(n) if n > 0 => break n,
                    Ok(_) => {
                        let e = io::Error::from_raw_os_error(libc::EINVAL);
                        return Err(t.fatal_io(&e, "error reading inotify event"));
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(t.fatal_io(&e, "error reading inotify event")),
                }
            };
            off = 0;
        }

        let Some((ev, next)) = parse_event(&evbuf[..len], off) else {
            off = len;
            continue;
        };
        off = next;

        // A watched directory was removed (IN_DELETE_SELF with no name):
        // its files cannot be watched any more, so poll.
        if ev.mask & libc::IN_DELETE_SELF != 0
            && ev.len == 0
            && f.iter().any(|s| s.parent_wd == ev.wd)
        {
            t.diag("directory containing watched file was removed");
            return Ok(());
        }

        let fspec = if ev.len > 0 {
            // An event on a name in a watched directory.
            let Some(j) = (0..n_files).find(|&j| {
                f[j].parent_wd == ev.wd && &f[j].name.as_bytes()[f[j].basename_start..] == ev.name
            }) else {
                continue;
            };
            let deleting = ev.mask & libc::IN_DELETE != 0;
            let mut new_wd = -1;
            if !deleting {
                // Adding the same inode again finds its existing watch.
                match ino.add_watch(&f[j].name, wd_mask) {
                    Ok(wd) => new_wd = wd,
                    Err(e) => {
                        let errnum = e.raw_os_error().unwrap_or(libc::EIO);
                        if errnum == libc::ENOSPC || errnum == libc::ENOMEM {
                            t.diag("inotify resources exhausted");
                            return Ok(());
                        }
                        // ENOENT for a dangling symlink, say.
                        let msg = format!("cannot watch {}", qa(&f[j].name));
                        t.diag_io(&e, msg);
                    }
                }
            }
            // False when only the file's attributes changed.
            let new_watch = !deleting && (f[j].wd < 0 || new_wd != f[j].wd);
            if new_watch {
                if f[j].wd >= 0 {
                    ino.rm_watch(f[j].wd);
                    wd_to_name.remove(&f[j].wd);
                }
                f[j].wd = new_wd;
                if new_wd == -1 {
                    continue;
                }
                // A file moved onto this name keeps its watch: the entry
                // for that watch is another name's.
                if let Some(p) = wd_to_name.remove(&new_wd)
                    && p != j
                {
                    if by_name {
                        t.recheck(&mut f[p], false)?;
                    }
                    f[p].wd = -1;
                    if f[p].fd.take().is_some() && f[p].errnum == 0 {
                        f[p].errnum = libc::EBADF;
                    }
                }
                wd_to_name.entry(new_wd).or_insert(j);
            }
            if by_name {
                t.recheck(&mut f[j], false)?;
            }
            j
        } else {
            match wd_to_name.get(&ev.wd) {
                Some(&i) => i,
                None => continue,
            }
        };

        let attrib_mask =
            libc::IN_ATTRIB | libc::IN_DELETE | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;
        if ev.mask & attrib_mask != 0 {
            // For IN_MOVE_SELF the watch stays: the file may still be under
            // one of the watched names.
            if ev.mask & libc::IN_DELETE_SELF != 0 {
                ino.rm_watch(f[fspec].wd);
                wd_to_name.remove(&f[fspec].wd);
            }
            // unlink gives IN_ATTRIB (st_nlink drops); the IN_DELETE on the
            // directory then handles the removed file.
            t.recheck(&mut f[fspec], false)?;
            continue;
        }
        check_fspec(t, f, fspec, &mut prev)?;
    }
}
