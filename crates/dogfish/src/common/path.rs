//! Path and file-name helpers shared by the file tools.

use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::{Path, PathBuf};

/// `p` made absolute (without resolving symlinks), as the undo journal
/// records it.
pub(crate) fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `p` absolute with its directory resolved (but not its last component, which
/// may be a symlink): a name for the same file that stays valid when the
/// operation moves something on the way to it. `mv c c/link` (`link` pointing
/// to `../a`) moves `c` to `a/c`, which `c/link/c` no longer names afterwards.
/// A trailing slash (which makes the name mean a directory) is kept as is.
pub(crate) fn stable_path(p: &Path) -> PathBuf {
    let abs = absolute(p);
    if p.as_os_str().as_bytes().ends_with(b"/") {
        return abs;
    }
    match (abs.parent(), abs.file_name()) {
        (Some(parent), Some(name)) => fs::canonicalize(parent)
            .map(|d| d.join(name))
            .unwrap_or(abs),
        _ => abs,
    }
}

/// gnulib `last_component`: the final name in `s`, ignoring trailing slashes
/// (empty for `/` or the empty string).
pub(crate) fn last_component(s: &[u8]) -> &[u8] {
    let mut base = s.iter().take_while(|&&b| b == b'/').count();
    let mut last_was_slash = false;
    for (i, &b) in s.iter().enumerate().skip(base) {
        if b == b'/' {
            last_was_slash = true;
        } else if last_was_slash {
            base = i;
            last_was_slash = false;
        }
    }
    &s[base..]
}

/// gnulib `base_len`: length of `s` without trailing slashes (but `/` stays).
pub(crate) fn base_len(s: &[u8]) -> usize {
    let mut len = s.len();
    while len > 1 && s[len - 1] == b'/' {
        len -= 1;
    }
    len
}

/// `dir/name` without doubling a trailing slash (gnulib `file_name_concat`).
pub(crate) fn concat_path(dir: &OsStr, name: &OsStr) -> PathBuf {
    let mut s = dir.to_os_string();
    if dir.as_bytes().last() != Some(&b'/') {
        s.push("/");
    }
    s.push(name);
    PathBuf::from(s)
}

/// Strips trailing slashes, keeping a lone `/`.
pub(crate) fn strip_trailing_slashes(s: &OsStr) -> OsString {
    let b = s.as_bytes();
    let mut len = b.len();
    while len > 0 && b[len - 1] == b'/' {
        len -= 1;
    }
    if len == 0 && !b.is_empty() {
        len = 1;
    }
    OsStr::from_bytes(&b[..len]).to_os_string()
}

/// True if `a` and `b` (already stat'ed) are the same inode.
pub(crate) fn same_inode(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// The names in `dir`: in `readdir` order (as GNU's `fts` visits them), or sorted
/// by inode number (as GNU `cp` copies them).
pub(crate) fn dir_names(dir: &Path, by_inode: bool) -> io::Result<Vec<OsString>> {
    let mut names = fs::read_dir(dir)?
        .map(|e| e.map(|e| (e.ino(), e.file_name())))
        .collect::<io::Result<Vec<_>>>()?;
    if by_inode {
        names.sort_by_key(|(ino, _)| *ino);
    }
    Ok(names.into_iter().map(|(_, name)| name).collect())
}

pub(crate) fn c_path(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// Byte offsets where each non-final component of `path` ends.
pub(crate) fn ancestor_ends(b: &[u8]) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' {
            i += 1;
            continue;
        }
        while i < b.len() && b[i] != b'/' {
            i += 1;
        }
        ends.push(i);
    }
    ends.pop();
    ends
}

/// gnulib `dir_name`: everything before the last component of `p`, without
/// trailing slashes (`.` when there is no directory part, `/` for `/x`).
pub(crate) fn dir_name(p: &Path) -> PathBuf {
    let s = p.as_os_str().as_bytes();
    let prefix = usize::from(s.first() == Some(&b'/'));
    let mut len = s.len() - last_component(s).len();
    while prefix < len && s[len - 1] == b'/' {
        len -= 1;
    }
    if len == 0 {
        PathBuf::from(".")
    } else {
        PathBuf::from(OsStr::from_bytes(&s[..len]))
    }
}

/// gnulib `same_name`: true when `a` and `b` name the same directory entry —
/// equal last components (ignoring trailing slashes) in the same directory.
/// The last components are compared as names, never followed; the
/// directories are stat'ed through symlinks, as `same_nameat` does
/// (`fstatat` without `AT_SYMLINK_NOFOLLOW`): with `l` a symlink to `.`,
/// `l/f` and `f` are one entry.
pub(crate) fn same_name(a: &Path, b: &Path) -> bool {
    let base = |p: &Path| {
        let last = last_component(p.as_os_str().as_bytes());
        last[..base_len(last)].to_vec()
    };
    if base(a) != base(b) {
        return false;
    }
    match (fs::metadata(dir_name(a)), fs::metadata(dir_name(b))) {
        (Ok(x), Ok(y)) => same_inode(&x, &y),
        _ => false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Deref {
    Never,
    CommandLine,
    Always,
}

/// `renameat2(RENAME_NOREPLACE)`, as gnulib's `renameatu`: fails with `EEXIST`
/// when `dst` exists (even as a dangling symlink). Where the kernel or file
/// system lacks the flag, an `lstat` check stands in for it.
pub(crate) fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    let (s, d) = (c_path(src)?, c_path(dst)?);
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `s` and `d` are NUL-terminated paths that outlive the call.
        let r = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                s.as_ptr(),
                libc::AT_FDCWD,
                d.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if !matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) {
            return Err(e);
        }
    }
    match fs::symlink_metadata(dst) {
        Ok(_) => Err(io::Error::from_raw_os_error(libc::EEXIST)),
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        Err(_) => {
            // SAFETY: `s` and `d` are NUL-terminated paths that outlive the call.
            if unsafe { libc::rename(s.as_ptr(), d.as_ptr()) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }
}

/// gnulib `file_name_concat`: `dir/base` (trailing slashes of `dir` dropped,
/// `/./base` under the root), and the offset where `base` starts in it.
pub(crate) fn file_name_concat(dir: &[u8], base: &[u8]) -> (PathBuf, usize) {
    let dirbase = last_component(dir);
    let dirbaselen = base_len(dirbase);
    let dirlen = dir.len() - dirbase.len() + dirbaselen;
    let mut s = dir[..dirlen].to_vec();
    if dirbaselen > 0 {
        if dir[dirlen - 1] != b'/' && base.first() != Some(&b'/') {
            s.push(b'/');
        }
    } else if base.first() == Some(&b'/') {
        s.push(b'.');
    }
    let offset = s.len();
    s.extend_from_slice(base);
    (PathBuf::from(OsString::from_vec(s)), offset)
}
