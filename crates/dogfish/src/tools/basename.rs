//! `basename`

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{DogfishContext, io::ignore_broken_pipe, stdout};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, Order, getopt_error, print_help, print_version, usage_error,
};
use crate::common::path::{base_len, last_component};
use crate::common::quote::quote_locale;

const BASENAME_HELP: &str = "\
Usage: basename NAME [SUFFIX]
  or:  basename OPTION... NAME...
Print NAME with any leading directory components removed.
If specified, also remove a trailing SUFFIX.

  -a, --multiple       support multiple arguments and treat each as a NAME
  -s, --suffix=SUFFIX  remove a trailing SUFFIX; implies -a
  -z, --zero           end each output line with NUL, not newline
      --help        display this help and exit
      --version     output version information and exit
";

#[derive(Clone, Copy, PartialEq, Eq)]
enum BasenameOpt {
    Multiple,
    Suffix,
    Zero,
    Help,
    Version,
}

/// GNU's `longopts` and `"+as:z"`: options end at the first NAME.
const SPECS: &[OptSpec<BasenameOpt>] = &[
    OptSpec::both(b'a', "multiple", HasArg::No, BasenameOpt::Multiple),
    OptSpec::both(b's', "suffix", HasArg::Required, BasenameOpt::Suffix),
    OptSpec::both(b'z', "zero", HasArg::No, BasenameOpt::Zero),
    OptSpec::long("help", HasArg::No, BasenameOpt::Help),
    OptSpec::long("version", HasArg::No, BasenameOpt::Version),
];

pub fn run_basename(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    use BasenameOpt::*;
    let mut opts = Getopt::with_order(
        args.into_iter().skip(1).collect(),
        SPECS,
        Order::StopAtOperand,
    );
    let (mut multiple, mut suffix, mut zero) = (false, None, false);
    while let Some(opt) = opts.next() {
        let (o, v) = match opt {
            Ok(o) => o,
            Err(e) => return Ok(getopt_error("basename", &e)),
        };
        match o {
            Multiple => multiple = true,
            Suffix => {
                multiple = true;
                suffix = v;
            }
            Zero => zero = true,
            Help => return Ok(print_help(BASENAME_HELP)),
            Version => return Ok(print_version("basename")),
        }
    }
    let names = opts.operands();
    if names.is_empty() {
        return Ok(usage_error("basename", "missing operand"));
    }
    let (names, suffix) = if multiple {
        (&names[..], suffix)
    } else {
        if names.len() > 2 {
            return Ok(usage_error(
                "basename",
                format!("extra operand {}", quote_locale(&names[2])),
            ));
        }
        (&names[..1], names.get(1).cloned())
    };
    let mut out = io::BufWriter::new(stdout::stdout());
    for name in names {
        out.write_all(&compute_basename(
            name.as_bytes(),
            suffix.as_deref().map(|s| s.as_bytes()),
        ))?;
        out.write_all(if zero { b"\0" } else { b"\n" })?;
        ctx.telemetry.add_items(1);
    }
    ignore_broken_pipe(out.flush())?;
    Ok(0)
}

/// GNU `basename`: the last component without trailing slashes, minus `suffix`
/// unless that would leave nothing.
fn compute_basename(path: &[u8], suffix: Option<&[u8]>) -> Vec<u8> {
    let base = last_component(path);
    let mut name = if base.is_empty() {
        path[..base_len(path)].to_vec()
    } else {
        let mut len = base_len(base);
        if base.get(len) == Some(&b'/') {
            len += 1;
        }
        base[..len].to_vec()
    };
    let keep = base_len(&name);
    name.truncate(keep);
    if let Some(suf) = suffix
        && name.first() != Some(&b'/')
        && name.len() > suf.len()
        && name.ends_with(suf)
    {
        name.truncate(name.len() - suf.len());
    }
    name
}
