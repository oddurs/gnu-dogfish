//! `dirname`

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{DogfishContext, RecordWriter, stdout};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, print_help, print_version, usage_error,
};
use crate::common::json_opt::{check_json_version, json_long, json_short};
use crate::common::path::last_component;

const DIRNAME_HELP: &str = "\
Usage: dirname [OPTION] NAME...
Output each NAME with its last non-slash component and trailing slashes
removed; if NAME contains no /'s, output '.' (meaning the current directory).

  -z, --zero     end each output line with NUL, not newline
  -J, --json[=VERSION]
                 one NDJSON record per NAME: path and dir (Dogfish)
      --help     display this help and exit
      --version  output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirnameOpt {
    Zero,
    Json,
    Help,
    Version,
}

/// GNU's `longopts` and `"z"`, plus the Dogfish `-J, --json`.
const SPECS: &[OptSpec<DirnameOpt>] = &[
    OptSpec::both(b'z', "zero", HasArg::No, DirnameOpt::Zero),
    OptSpec::long("help", HasArg::No, DirnameOpt::Help),
    OptSpec::long("version", HasArg::No, DirnameOpt::Version),
    json_short(DirnameOpt::Json),
    json_long(DirnameOpt::Json),
];

pub fn run_dirname(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    let (mut zero, mut json) = (false, false);
    while let Some(opt) = opts.next() {
        match opt {
            Ok((DirnameOpt::Zero, _)) => zero = true,
            Ok((DirnameOpt::Json, v)) => {
                if let Err(code) = check_json_version("dirname", v.as_deref()) {
                    return Ok(code);
                }
                json = true;
            }
            Ok((DirnameOpt::Help, _)) => return Ok(print_help(DIRNAME_HELP)),
            Ok((DirnameOpt::Version, _)) => return Ok(print_version("dirname")),
            Err(e) => return Ok(getopt_error("dirname", &e)),
        }
    }
    let operands = opts.operands();
    if operands.is_empty() {
        return Ok(usage_error("dirname", "missing operand"));
    }
    if json {
        // One record per NAME: `-z` changes only the text's terminator.
        let mut w = RecordWriter::stdout();
        for name in &operands {
            w.record(|r| {
                r.bytes("path", name.as_bytes())
                    .bytes("dir", compute_dirname(name.as_bytes()));
            })?;
            ctx.telemetry.add_items(1);
        }
        w.flush()?;
        return Ok(0);
    }
    let mut out = io::BufWriter::new(stdout::stdout());
    for name in &operands {
        out.write_all(compute_dirname(name.as_bytes()))?;
        out.write_all(if zero { b"\0" } else { b"\n" })?;
        ctx.telemetry.add_items(1);
    }
    out.flush()?;
    Ok(0)
}

/// GNU `dirname` (gnulib `dir_len`).
fn compute_dirname(path: &[u8]) -> &[u8] {
    let prefix = usize::from(path.first() == Some(&b'/'));
    let mut len = path.len() - last_component(path).len();
    while len > prefix && path[len - 1] == b'/' {
        len -= 1;
    }
    if len == 0 { b"." } else { &path[..len] }
}

/// dirname's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("dirname", DIRNAME_HELP, SPECS)
}
