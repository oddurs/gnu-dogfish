//! whoami

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{DogfishContext, outln};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, print_help, print_version, usage_error,
};
use crate::common::quote::quote_locale;
use crate::common::users::get_username_by_uid;

const WHOAMI_HELP: &str = "\
Print the user name associated with the current effective user ID

Usage: whoami [OPTIONS]

Options:
      --help  Display this help and exit
  -J, --json
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum WhoamiOpt {
    Json,
    Help,
    Version,
}

/// gnulib's `parse_gnu_standard_options_only`, plus the Dogfish `-J, --json`.
const SPECS: &[OptSpec<WhoamiOpt>] = &[
    OptSpec::long("help", HasArg::No, WhoamiOpt::Help),
    OptSpec::long("version", HasArg::No, WhoamiOpt::Version),
    OptSpec::both(b'J', "json", HasArg::No, WhoamiOpt::Json).exact(),
];

pub fn run_whoami(args: Vec<OsString>, _ctx: &DogfishContext) -> Result<i32> {
    let mut json = false;
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        match opt {
            Ok((WhoamiOpt::Json, _)) => json = true,
            Ok((WhoamiOpt::Help, _)) => return Ok(print_help(WHOAMI_HELP)),
            Ok((WhoamiOpt::Version, _)) => return Ok(print_version("whoami")),
            Err(e) => return Ok(getopt_error("whoami", &e)),
        }
    }
    if let Some(extra) = opts.operands().first() {
        return Ok(usage_error(
            "whoami",
            format!("extra operand {}", quote_locale(extra)),
        ));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let Some(username) = get_username_by_uid(uid) else {
        eprintln!("whoami: cannot find name for user ID {uid}");
        return Ok(1);
    };
    if json {
        outln!(
            "{}",
            serde_json::json!({ "uid": uid, "username": username })
        );
    } else {
        outln!("{}", username);
    }
    Ok(0)
}

/// whoami's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("whoami", WHOAMI_HELP, SPECS)
}
