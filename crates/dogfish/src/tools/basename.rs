//! `basename`

use std::io::{self, Write};

use anyhow::Result;
use dogfish_core::{DogfishContext, io::ignore_broken_pipe};

use crate::common::path::{base_len, last_component};
use crate::common::quote::quote_locale;
use crate::common::tool_args::{HasArg, ToolOpt, opt, parse_tool_args_posix, usage_error};

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
pub(crate) enum BasenameOpt {
    Multiple,
    Suffix,
    Zero,
}

pub fn run_basename(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    use BasenameOpt::*;
    const SPECS: &[ToolOpt<BasenameOpt>] = &[
        opt("a", "multiple", HasArg::No, Multiple),
        opt("s", "suffix", HasArg::Required, Suffix),
        opt("z", "zero", HasArg::No, Zero),
    ];
    let parsed = match parse_tool_args_posix("basename", &args, SPECS, BASENAME_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let (mut multiple, mut suffix, mut zero) = (false, None, false);
    for (o, v) in parsed.opts {
        match o {
            Multiple => multiple = true,
            Suffix => {
                multiple = true;
                suffix = v;
            }
            Zero => zero = true,
        }
    }
    let names = parsed.operands;
    if names.is_empty() {
        return Ok(usage_error("basename", "missing operand"));
    }
    let (names, suffix) = if multiple {
        (&names[..], suffix)
    } else {
        if names.len() > 2 {
            return Ok(usage_error(
                "basename",
                &format!("extra operand {}", quote_locale(&names[2])),
            ));
        }
        (&names[..1], names.get(1).cloned())
    };
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for name in names {
        out.write_all(&compute_basename(
            name.as_bytes(),
            suffix.as_deref().map(str::as_bytes),
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
