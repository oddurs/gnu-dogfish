//! Destination resolution and same-file checks that cp and mv share.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use dogfish_core::gnu_io_error;

use crate::common::getopt::usage_error;
use crate::common::path::{same_inode, same_name};
use crate::common::quote::qa;

/// What to do when the destination exists (GNU's `interactive` setting).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Clobber {
    /// Overwrite (the default).
    Yes,
    /// Ask first (`-i`).
    Ask,
    /// Skip silently (`cp -n`), or with a diagnostic and failure (`mv -n`).
    No,
    /// Skip silently and successfully (`--update=none`).
    Skip,
}

/// Splits operands into sources and a destination the way GNU `cp` and `mv` do.
/// `Ok(None)` means each source goes into the returned directory.
pub(crate) enum Targets {
    Into(String, Vec<String>),
    Single(String, String),
}

pub(crate) fn resolve_targets(
    tool: &str,
    mut files: Vec<String>,
    target_dir: Option<String>,
    no_target_dir: bool,
) -> std::result::Result<Targets, i32> {
    if let Some(dir) = target_dir {
        if no_target_dir {
            eprintln!(
                "{tool}: cannot combine --target-directory (-t) and --no-target-directory (-T)"
            );
            return Err(1);
        }
        let check = fs::metadata(&dir).and_then(|m| {
            if m.is_dir() {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(libc::ENOTDIR))
            }
        });
        if let Err(e) = check {
            eprintln!(
                "{tool}: target directory {}: {}",
                qa(&dir),
                gnu_io_error(&e)
            );
            return Err(1);
        }
        if files.is_empty() {
            return Err(usage_error(tool, "missing file operand"));
        }
        return Ok(Targets::Into(dir, files));
    }
    match files.len() {
        0 => return Err(usage_error(tool, "missing file operand")),
        1 => {
            return Err(usage_error(
                tool,
                format!("missing destination file operand after {}", qa(&files[0])),
            ));
        }
        _ => {}
    }
    if no_target_dir {
        if files.len() > 2 {
            return Err(usage_error(
                tool,
                format!("extra operand {}", qa(&files[2])),
            ));
        }
        let dst = files.pop().unwrap_or_default();
        return Ok(Targets::Single(files.remove(0), dst));
    }
    let last = files.last().cloned().unwrap_or_default();
    match fs::metadata(&last) {
        Ok(m) if m.is_dir() => {
            files.pop();
            Ok(Targets::Into(last, files))
        }
        res if files.len() > 2 => {
            let err = res
                .err()
                .unwrap_or_else(|| io::Error::from_raw_os_error(libc::ENOTDIR));
            eprintln!("{tool}: target {}: {}", qa(&last), gnu_io_error(&err));
            Err(1)
        }
        _ => {
            let dst = files.pop().unwrap_or_default();
            Ok(Targets::Single(files.remove(0), dst))
        }
    }
}

/// Absolute form of `target` with its parent resolved through symlinks, for
/// "is this inside that directory?" checks on paths that may not exist yet.
pub(crate) fn resolve_for_containment(target: &Path) -> PathBuf {
    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match (fs::canonicalize(parent), target.file_name()) {
        (Ok(p), Some(name)) => p.join(name),
        _ => std::path::absolute(target).unwrap_or_else(|_| target.to_path_buf()),
    }
}

/// The options GNU's `same_file_ok` looks at.
#[derive(Default)]
pub(crate) struct SameFileOpts {
    pub(crate) move_mode: bool,
    /// `--no-dereference` (or `-d`, `-P`, `-a`, `-R` without `-L`/`-H`).
    pub(crate) deref_never: bool,
    pub(crate) hard_link: bool,
    pub(crate) symbolic_link: bool,
    pub(crate) backup: bool,
    /// `--remove-destination`.
    pub(crate) unlink_dest_before_opening: bool,
}

/// A port of GNU's `same_file_ok` (copy.c): may `src` be copied or moved onto
/// the existing `dst` without losing data? `src_sb` and `dst_sb` are stat'ed
/// as GNU's `copy_internal` stats them. The second value is GNU's
/// `return_now`: the operation is already complete (or must stop) and
/// should report success or failure without doing anything.
pub(crate) fn same_file_ok(
    src: &Path,
    src_sb: &fs::Metadata,
    dst: &Path,
    dst_sb: &fs::Metadata,
    x: &SameFileOpts,
) -> (bool, bool) {
    let same = same_inode(src_sb, dst_sb);
    if same && x.hard_link {
        return (true, true);
    }
    let (src_link, dst_link, same_link);
    if x.deref_never {
        same_link = same;
        if src_sb.is_symlink() && dst_sb.is_symlink() {
            let sn = same_name(src, dst);
            if !sn {
                if x.backup {
                    return (true, false);
                }
                // Two hard links to one symlink: rename(2) would do nothing.
                if same_link {
                    return (!x.move_mode, true);
                }
            }
            return (!sn, false);
        }
        src_link = src_sb.clone();
        dst_link = dst_sb.clone();
    } else {
        if !same {
            return (true, false);
        }
        let (Ok(d), Ok(s)) = (dst.symlink_metadata(), src.symlink_metadata()) else {
            return (true, false);
        };
        same_link = same_inode(&s, &d);
        if s.is_symlink() && d.is_symlink() && x.unlink_dest_before_opening {
            return (true, false);
        }
        src_link = s;
        dst_link = d;
    }
    // A backup keeps the data, unless both names are one directory entry.
    if x.backup {
        if !same_link {
            // Backing up the referent of a dereferenced source symlink would
            // leave the source dangling.
            let dangles =
                !x.move_mode && !x.deref_never && src_link.is_symlink() && !dst_link.is_symlink();
            return (!dangles, false);
        }
        return (!same_name(src, dst), false);
    }
    if x.move_mode || x.unlink_dest_before_opening {
        // A destination symlink is replaced, never written through.
        if dst_link.is_symlink() {
            return (true, false);
        }
        // Distinct hard links to one file: racy, may lose data.
        if same_link && dst_link.nlink() > 1 && !same_name(src, dst) {
            return (!x.move_mode, false);
        }
    }
    if !src_link.is_symlink() && !dst_link.is_symlink() {
        if !same_inode(&src_link, &dst_link) {
            return (true, false);
        }
        if x.hard_link {
            return (true, true);
        }
    }
    // Moving a symlink onto its referent loses the file, except when the
    // referent has other hard links and the symlink names one of those.
    if x.move_mode
        && src_sb.is_symlink()
        && dst_link.nlink() > 1
        && let Ok(abs_src) = fs::canonicalize(src)
    {
        return (!same_name(&abs_src, dst), false);
    }
    // Recreating a destination symlink is fine.
    if x.symbolic_link && dst_link.is_symlink() {
        return (true, false);
    }
    if x.deref_never {
        let target = |link: &fs::Metadata, p: &Path| {
            if link.is_symlink() {
                fs::metadata(p).ok()
            } else {
                Some(link.clone())
            }
        };
        let (Some(ts), Some(td)) = (target(&src_link, src), target(&dst_link, dst)) else {
            return (true, false);
        };
        if !same_inode(&ts, &td) {
            return (true, false);
        }
        if x.hard_link {
            return (true, !dst_link.is_symlink());
        }
    }
    (false, false)
}
