//! echo

use std::io::Write;

use anyhow::Result;
use dogfish_core::{DogfishContext, io::ignore_broken_pipe, stdout};

use crate::common::escapes::unescape_echo_bytes;
use crate::common::legacy_getopt::print_help;

const ECHO_HELP: &str = "\
Usage: echo [SHORT-OPTION]... [STRING]...
  or:  echo LONG-OPTION
Echo the STRING(s) to standard output.

  -n             do not output the trailing newline
  -e             enable interpretation of backslash escapes
  -E             disable interpretation of backslash escapes (default)
      --help        display this help and exit
      --version     output version information and exit

If -e is in effect, the following sequences are recognized:

  \\\\      backslash
  \\a      alert (BEL)
  \\b      backspace
  \\c      produce no further output
  \\e      escape
  \\f      form feed
  \\n      new line
  \\r      carriage return
  \\t      horizontal tab
  \\v      vertical tab
  \\0NNN   byte with octal value NNN (1 to 3 digits)
  \\xHH    byte with hexadecimal value HH (1 to 2 digits)
";

pub fn run_echo(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    // Like GNU (without POSIXLY_CORRECT), only a lone `--help` is an option.
    if args.len() == 2 && args[1] == "--help" {
        return print_help(ECHO_HELP);
    }
    let mut no_newline = false;
    let mut interpret_escapes = false;
    let mut idx = 1;

    while idx < args.len() {
        let arg = &args[idx];
        if arg.starts_with('-')
            && arg.len() > 1
            && arg[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E'))
        {
            for ch in arg[1..].chars() {
                match ch {
                    'n' => no_newline = true,
                    'e' => interpret_escapes = true,
                    'E' => interpret_escapes = false,
                    _ => {}
                }
            }
            idx += 1;
        } else {
            break;
        }
    }

    let payload = args[idx..].join(" ");
    let mut out = stdout::stdout();

    let (rendered, stop) = if interpret_escapes {
        unescape_echo_bytes(payload.as_bytes(), false)
    } else {
        (payload.into_bytes(), false)
    };
    ignore_broken_pipe(out.write_all(&rendered))?;
    ctx.telemetry.add_written(rendered.len() as u64);
    if !no_newline && !stop {
        ignore_broken_pipe(out.write_all(b"\n"))?;
        ctx.telemetry.add_written(1);
    }
    Ok(0)
}
