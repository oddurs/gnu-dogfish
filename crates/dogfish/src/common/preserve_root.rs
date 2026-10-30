//! The `--preserve-root` failsafe (chmod, chown, rm).

use std::ffi::OsStr;
use std::fs::Metadata;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use dogfish_core::same_file;

use crate::common::quote::qa;

/// The `--preserve-root` failsafe message for recursive `rm`, `chmod` and `chown`.
pub(crate) fn refuse_root(tool: &str, operand: impl AsRef<OsStr>) {
    let operand = operand.as_ref();
    if operand.as_bytes() == b"/" {
        eprintln!("{tool}: it is dangerous to operate recursively on '/'");
    } else {
        eprintln!(
            "{tool}: it is dangerous to operate recursively on {} (same as '/')",
            qa(operand)
        );
    }
    eprintln!("{tool}: use --no-preserve-root to override this failsafe");
}

/// The device and inode numbers of `/`, as GNU's `get_root_dev_ino` records them
/// for `chmod -R` and `chown -R` with `--preserve-root`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct DevIno {
    pub dev: u64,
    pub ino: u64,
}

impl DevIno {
    pub(crate) fn of(m: &Metadata) -> Self {
        DevIno {
            dev: m.dev(),
            ino: m.ino(),
        }
    }

    /// From a raw `stat` (the `fstatat` a directory-fd walk already makes).
    #[allow(clippy::unnecessary_cast)] // the field types differ between platforms
    pub(crate) fn of_stat(st: &libc::stat) -> Self {
        DevIno {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        }
    }
}

/// GNU `get_root_dev_ino`: stats `/` (following a symlink, as GNU does).
pub(crate) fn root_dev_ino() -> io::Result<DevIno> {
    Path::new("/").metadata().map(|m| DevIno::of(&m))
}

/// GNU `ROOT_DEV_INO_CHECK`: true when `--preserve-root` recorded the root's
/// identity in `root` and `file` is that same directory. The walks of
/// `chmod -R` and `chown -R` ask this of every entry they would enter, with
/// `file` from the stat the traversal itself uses, so a symlink to `/` that
/// `-H` or `-L` follows is caught, and one that is not followed stays just a link.
pub(crate) fn is_root_dev_ino(root: Option<DevIno>, file: DevIno) -> bool {
    root.is_some_and(|r| r == file)
}

/// True when `p` names the root directory itself (`/`, `//`, `/.`, `/usr/..`, a bind
/// of the same inode), as `--preserve-root` checks. A symlink pointing at `/` is not
/// the root: removing it only unlinks the link.
pub(crate) fn is_root_dir(p: &Path) -> bool {
    p.symlink_metadata().is_ok_and(|m| m.is_dir()) && same_file(p, Path::new("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    // `rm`'s preserve-root guard is tested through its predicate alone so that a
    // regression can never turn the test into an actual `rm -r /`.
    #[test]
    fn is_root_dir_matches_every_spelling_of_root() {
        // `/usr` rather than `/tmp`: on macOS `/tmp` is a symlink, so `/tmp/..` is `/private`.
        for p in ["/", "//", "/.", "/./", "/usr/..", "/usr/../."] {
            assert!(is_root_dir(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn root_dev_ino_check_matches_only_the_recorded_directory() {
        // A temporary directory stands in for `/`: the predicate never needs the real root.
        let tmp = tempfile::tempdir().unwrap();
        let fake_root = tmp.path().join("root");
        std::fs::create_dir(&fake_root).unwrap();
        let link = tmp.path().join("link");
        symlink(&fake_root, &link).unwrap();
        let id = |m: std::io::Result<Metadata>| DevIno::of(&m.unwrap());
        let root = Some(id(fake_root.metadata()));
        assert!(is_root_dev_ino(root, id(fake_root.metadata())));
        assert!(is_root_dev_ino(root, id(link.metadata())), "followed");
        assert!(!is_root_dev_ino(root, id(link.symlink_metadata())));
        assert!(!is_root_dev_ino(root, id(tmp.path().metadata())));
        assert!(!is_root_dev_ino(None, id(fake_root.metadata())));
        let real = root_dev_ino().unwrap();
        assert!(is_root_dev_ino(Some(real), id(Path::new("/").metadata())));
    }

    #[test]
    fn is_root_dir_rejects_non_root_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("root_link");
        symlink("/", &link).unwrap();
        assert!(!is_root_dir(&link), "a symlink to / is just a link");
        assert!(!is_root_dir(tmp.path()));
        assert!(!is_root_dir(Path::new("/nonexistent-dogfish-path")));
    }
}
