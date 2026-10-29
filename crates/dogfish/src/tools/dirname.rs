//! `dirname`

use std::io::{self, Write};

use anyhow::Result;
use dogfish_core::{DogfishContext, io::ignore_broken_pipe};

use crate::common::path::last_component;
use crate::common::tool_args::{HasArg, ToolOpt, opt, parse_tool_args, usage_error};

const DIRNAME_HELP: &str = "\
Usage: dirname [OPTION] NAME...
Output each NAME with its last non-slash component and trailing slashes
removed; if NAME contains no /'s, output '.' (meaning the current directory).

  -z, --zero     end each output line with NUL, not newline
      --help     display this help and exit
      --version  output version information and exit
";

pub fn run_dirname(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    const SPECS: &[ToolOpt<()>] = &[opt("z", "zero", HasArg::No, ())];
    let parsed = match parse_tool_args("dirname", &args, SPECS, DIRNAME_HELP) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let zero = !parsed.opts.is_empty();
    if parsed.operands.is_empty() {
        return Ok(usage_error("dirname", "missing operand"));
    }
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for name in &parsed.operands {
        out.write_all(compute_dirname(name.as_bytes()))?;
        out.write_all(if zero { b"\0" } else { b"\n" })?;
        ctx.telemetry.add_items(1);
    }
    ignore_broken_pipe(out.flush())?;
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
