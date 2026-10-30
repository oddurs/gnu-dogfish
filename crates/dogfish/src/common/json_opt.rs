//! `-J, --json[=VERSION]`: the Dogfish option that turns a tool's output
//! into NDJSON records (`dogfish_core::ndjson`, `doc/ndjson.md`).
//!
//! `-J` takes no argument (so it bundles: `ls -lJ`); `--json` takes an
//! optional one, the record format version. Both mean the current version,
//! and `--json=1` names it; any other version is a usage error.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use crate::common::getopt::{HasArg, OptSpec, usage_error};

/// The option-table entry for `-J`; [`json_long`] goes with it, with the
/// same `key`.
pub(crate) const fn json_short<K: Copy>(key: K) -> OptSpec<K> {
    OptSpec::short(b'J', HasArg::No, key)
}

/// The option-table entry for `--json[=VERSION]`. It is a Dogfish
/// extension, so it is never matched by an abbreviation.
pub(crate) const fn json_long<K: Copy>(key: K) -> OptSpec<K> {
    OptSpec::long("json", HasArg::Optional, key).exact()
}

/// Checks the argument of `--json=VERSION` (`None` for `-J` or a bare
/// `--json`). `Err` is the exit status after a usage diagnostic.
pub(crate) fn check_json_version(tool: &str, value: Option<&OsStr>) -> Result<(), i32> {
    match dogfish_core::ndjson::parse_version(value.map(OsStr::as_bytes)) {
        Ok(_) => Ok(()),
        Err(e) => Err(usage_error(tool, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(check_json_version("ls", None), Ok(()));
        assert_eq!(check_json_version("ls", Some(OsStr::new("1"))), Ok(()));
        assert_eq!(check_json_version("ls", Some(OsStr::new("2"))), Err(2));
        assert_eq!(check_json_version("cut", Some(OsStr::new(""))), Err(1));
    }
}
