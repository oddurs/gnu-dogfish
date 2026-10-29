//! echo

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use dogfish_core::{DogfishContext, io::ignore_broken_pipe, stdout};

use crate::common::escapes::unescape_echo_bytes;
use crate::common::getopt::{print_help, print_version};

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

pub fn run_echo(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    // Like GNU: options are only `-n`, `-e`, `-E` (clustered), and a lone
    // `--help` or `--version`. Under POSIXLY_CORRECT there are none, unless
    // the first argument is `-n`, and escapes are always interpreted.
    let posixly_correct = std::env::var_os("POSIXLY_CORRECT").is_some();
    let allow_options = !posixly_correct || args.get(1).is_some_and(|a| a == "-n");
    if allow_options && args.len() == 2 {
        if args[1] == "--help" {
            return Ok(print_help(ECHO_HELP));
        }
        if args[1] == "--version" {
            return Ok(print_version("echo"));
        }
    }
    let mut no_newline = false;
    let mut interpret_escapes = false;
    let mut idx = 1;

    while allow_options && idx < args.len() {
        let arg = args[idx].as_bytes();
        if arg.len() > 1
            && arg[0] == b'-'
            && arg[1..].iter().all(|c| matches!(c, b'n' | b'e' | b'E'))
        {
            for ch in &arg[1..] {
                match ch {
                    b'n' => no_newline = true,
                    b'e' => interpret_escapes = true,
                    b'E' => interpret_escapes = false,
                    _ => {}
                }
            }
            idx += 1;
        } else {
            break;
        }
    }

    let mut payload = Vec::new();
    for (i, arg) in args[idx..].iter().enumerate() {
        if i > 0 {
            payload.push(b' ');
        }
        payload.extend_from_slice(arg.as_bytes());
    }
    let mut out = stdout::stdout();

    let (rendered, stop) = if interpret_escapes || posixly_correct {
        unescape_echo_bytes(&payload, false)
    } else {
        (payload, false)
    };
    ignore_broken_pipe(out.write_all(&rendered))?;
    ctx.telemetry.add_written(rendered.len() as u64);
    if !no_newline && !stop {
        ignore_broken_pipe(out.write_all(b"\n"))?;
        ctx.telemetry.add_written(1);
    }
    Ok(0)
}
