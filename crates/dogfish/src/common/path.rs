//! Path and file-name helpers shared by the file tools.

use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::{Path, PathBuf};

use dogfish_core::same_file;

pub(crate) fn absolute_lossy(p: &Path) -> String {
    std::path::absolute(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
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

/// The file name a source keeps inside a target directory (`d/` → `d`).
pub(crate) fn operand_base(src: &str) -> OsString {
    let last = last_component(src.as_bytes());
    OsString::from_vec(last[..base_len(last)].to_vec())
}

/// `dir/name` without doubling a trailing slash (gnulib `file_name_concat`).
pub(crate) fn concat_path(dir: &str, name: &OsStr) -> PathBuf {
    let mut s = OsString::from(dir);
    if !dir.ends_with('/') {
        s.push("/");
    }
    s.push(name);
    PathBuf::from(s)
}

/// Strips trailing slashes, keeping a lone `/`.
pub(crate) fn strip_trailing_slashes(s: &str) -> String {
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() && !s.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
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
pub(crate) fn ancestor_ends(path: &str) -> Vec<usize> {
    let b = path.as_bytes();
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

/// True when `a` and `b` name the same directory entry (same parent, same name).
pub(crate) fn same_name(a: &Path, b: &Path) -> bool {
    let parent = |p: &Path| match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    a.file_name() == b.file_name() && same_file(&parent(a), &parent(b))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Deref {
    Never,
    CommandLine,
    Always,
}
