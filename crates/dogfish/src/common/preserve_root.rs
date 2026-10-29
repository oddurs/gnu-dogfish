//! The `--preserve-root` failsafe (chmod, chown, rm).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
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
    fn is_root_dir_rejects_non_root_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("root_link");
        symlink("/", &link).unwrap();
        assert!(!is_root_dir(&link), "a symlink to / is just a link");
        assert!(!is_root_dir(tmp.path()));
        assert!(!is_root_dir(Path::new("/nonexistent-dogfish-path")));
    }
}
