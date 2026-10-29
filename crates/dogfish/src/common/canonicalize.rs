//! gnulib `canonicalize` modes and relative paths (ln, realpath).

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// How much of a path must exist (gnulib `canonicalize_filename_mode`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CanMode {
    Existing,
    AllButLast,
    Missing,
}

/// Makes `path` absolute, resolving `.`, `..` and (unless `nolinks`) symbolic
/// links, requiring components to exist as `mode` says. A port of gnulib's
/// `canonicalize_filename_mode`, quirks included: without symlink resolution,
/// only the last component, or one followed by `/..`, a final `/.` or a
/// trailing slash, is checked.
pub(crate) fn canonicalize_mode(path: &Path, mode: CanMode, nolinks: bool) -> io::Result<PathBuf> {
    const MAX_LINKS: usize = 40;
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    let mut resolved = if bytes[0] == b'/' {
        PathBuf::from("/")
    } else {
        std::env::current_dir()?
    };
    let split = |b: &[u8]| -> VecDeque<OsString> {
        b.split(|&c| c == b'/')
            .filter(|c| !c.is_empty())
            .map(|c| OsString::from_vec(c.to_vec()))
            .collect()
    };
    let trailing_slash = bytes.len() > 1 && bytes.ends_with(b"/");
    // gnulib `suffix_requires_dir_check`: must the path so far be a directory?
    let needs_dir = |rest: &VecDeque<OsString>| {
        for (i, c) in rest.iter().enumerate() {
            match c.as_bytes() {
                b".." => return true,
                b"." if i + 1 == rest.len() => return true,
                b"." => {}
                _ => return false,
            }
        }
        trailing_slash
    };
    let errno = |e: &io::Error| e.raw_os_error().unwrap_or(0);
    let mut pending = split(bytes);
    let mut seen: HashSet<(PathBuf, Vec<u8>)> = HashSet::new();
    let mut links = 0;
    while let Some(comp) = pending.pop_front() {
        if comp == "." {
            continue;
        }
        if comp == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&comp);
        if mode == CanMode::Missing && nolinks {
            resolved = candidate;
            continue;
        }
        let mut link_err = None;
        if !nolinks {
            match fs::read_link(&candidate) {
                Ok(target) => {
                    links += 1;
                    // The same link reached with the same remaining path is a loop.
                    let mut rest = comp.as_bytes().to_vec();
                    for c in &pending {
                        rest.push(b'/');
                        rest.extend_from_slice(c.as_bytes());
                    }
                    if links > MAX_LINKS || !seen.insert((resolved.clone(), rest)) {
                        if mode == CanMode::Missing {
                            resolved = candidate;
                            continue;
                        }
                        return Err(io::Error::from_raw_os_error(libc::ELOOP));
                    }
                    let tb = target.as_os_str().as_bytes();
                    if tb.first() == Some(&b'/') {
                        resolved = PathBuf::from("/");
                    }
                    for c in split(tb).into_iter().rev() {
                        pending.push_front(c);
                    }
                    continue;
                }
                Err(e) => link_err = Some(e),
            }
        }
        if mode != CanMode::Missing {
            let check = if needs_dir(&pending) {
                match fs::metadata(&candidate) {
                    Ok(m) if m.is_dir() => Ok(()),
                    Ok(_) => Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
                    Err(e) => Err(e),
                }
            } else if let Some(e) = link_err {
                // readlink(2) fails with EINVAL on anything that is not a symlink.
                if errno(&e) == libc::EINVAL {
                    Ok(())
                } else {
                    Err(e)
                }
            } else if !pending.is_empty() {
                Ok(())
            } else {
                fs::metadata(&candidate).map(|_| ())
            };
            if let Err(e) = check
                && !(mode == CanMode::AllButLast && errno(&e) == libc::ENOENT && pending.is_empty())
            {
                return Err(e);
            }
        }
        resolved = candidate;
    }
    Ok(resolved)
}

/// `path` relative to the directory `base` (both absolute and canonical).
pub(crate) fn relpath(path: &Path, base: &Path) -> PathBuf {
    let p: Vec<_> = path.components().collect();
    let b: Vec<_> = base.components().collect();
    let common = p.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &p[common..] {
        out.push(c);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}
