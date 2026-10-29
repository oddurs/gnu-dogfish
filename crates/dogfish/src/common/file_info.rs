//! Metadata helpers shared by ls and stat (and df, rm): device numbers, owners,
//! timestamps, extended attributes and mount points.

#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::common::strftime::Timespec;
use crate::sys::{get_groupname_by_gid, get_username_by_uid};

/// coreutils `find_mount_point`: the deepest ancestor of `path` (itself, if a
/// directory) on the same device.
pub(crate) fn find_mount_point(path: &Path, meta: &fs::Metadata) -> PathBuf {
    let start = if meta.is_dir() {
        fs::canonicalize(path)
    } else {
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        fs::canonicalize(parent)
    };
    let Ok(mut current) = start else {
        return PathBuf::from("?");
    };
    let dev = if meta.is_dir() {
        meta.dev()
    } else {
        match fs::metadata(&current) {
            Ok(m) => m.dev(),
            Err(_) => return current,
        }
    };
    while let Some(parent) = current.parent() {
        match fs::metadata(parent) {
            Ok(pm) if pm.dev() == dev => current = parent.to_path_buf(),
            _ => break,
        }
    }
    current
}

// ----- misc file helpers ---------------------------------------------------

/// `major(dev)` as glibc / Darwin define it.
pub(crate) fn dev_major(dev: u64) -> u64 {
    if cfg!(target_os = "macos") {
        (dev >> 24) & 0xff
    } else {
        ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)
    }
}

/// `minor(dev)` as glibc / Darwin define it.
pub(crate) fn dev_minor(dev: u64) -> u64 {
    if cfg!(target_os = "macos") {
        dev & 0xff_ffff
    } else {
        (dev & 0xff) | ((dev >> 12) & !0xff)
    }
}

/// gnulib `file_type`: the `%F` description of a file.
pub(crate) fn file_type_description(meta: &fs::Metadata) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    let ft = meta.file_type();
    if ft.is_file() {
        if meta.len() == 0 {
            "regular empty file"
        } else {
            "regular file"
        }
    } else if ft.is_dir() {
        "directory"
    } else if ft.is_symlink() {
        "symbolic link"
    } else if ft.is_fifo() {
        "fifo"
    } else if ft.is_socket() {
        "socket"
    } else if ft.is_char_device() {
        "character special file"
    } else if ft.is_block_device() {
        "block special file"
    } else {
        "weird file"
    }
}

pub(crate) fn user_name(uid: u32) -> Option<String> {
    get_username_by_uid(uid)
}

pub(crate) fn group_name(gid: u32) -> Option<String> {
    get_groupname_by_gid(gid)
}

pub(crate) fn meta_mtime(meta: &fs::Metadata) -> Timespec {
    Timespec {
        sec: meta.mtime(),
        nsec: meta.mtime_nsec(),
    }
}

pub(crate) fn meta_atime(meta: &fs::Metadata) -> Timespec {
    Timespec {
        sec: meta.atime(),
        nsec: meta.atime_nsec(),
    }
}

pub(crate) fn meta_ctime(meta: &fs::Metadata) -> Timespec {
    Timespec {
        sec: meta.ctime(),
        nsec: meta.ctime_nsec(),
    }
}

/// Birth time, when the platform and file system record it.
pub(crate) fn meta_birth(meta: &fs::Metadata) -> Option<Timespec> {
    meta.created().ok().map(Timespec::from_system)
}

/// Reads an extended attribute of `path` without following symlinks (Linux).
#[cfg(target_os = "linux")]
pub(crate) fn lgetxattr_bytes(path: &Path, name: &str) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let c_name = CString::new(name).ok()?;
    let mut buf = vec![0u8; 256];
    // SAFETY: valid C strings and a buffer of the advertised length.
    let len = unsafe {
        libc::lgetxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if len <= 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some(buf)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn lgetxattr_bytes(_path: &Path, _name: &str) -> Option<Vec<u8>> {
    None
}

/// The SELinux security context of `path`, if it has one.
pub(crate) fn security_context(path: &Path) -> Option<String> {
    let mut ctx = lgetxattr_bytes(path, "security.selinux")?;
    while ctx.last() == Some(&0) {
        ctx.pop();
    }
    Some(String::from_utf8_lossy(&ctx).into_owned())
}

pub(crate) fn path_bytes(p: &Path) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes()
}
