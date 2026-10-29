//! printenv — Print values of all or specified environment variables

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;

use anyhow::Result;
use clap::{ArgAction, Parser};
use dogfish_core::{DogfishContext, io::ignore_broken_pipe};

#[derive(Parser)]
#[command(
    name = "printenv",
    about = "Print the values of the specified environment VARIABLE(s). \
             If no VARIABLE is specified, print name and value pairs for them all.",
    disable_help_flag = true,
    infer_long_args = true
)]
pub(crate) struct PrintenvArgs {
    /// End each output line with NUL, not newline
    #[arg(short = '0', long = "null")]
    pub(crate) null: bool,

    #[arg(short = 'J', long = "json")]
    pub(crate) json: bool,

    /// Display this help and exit
    #[arg(long, action = ArgAction::Help)]
    pub(crate) help: Option<bool>,

    vars: Vec<String>,
}

pub fn run_printenv(args: Vec<String>, _ctx: &DogfishContext) -> Result<i32> {
    let parsed = PrintenvArgs::try_parse_from(args)?;
    let term: &[u8] = if parsed.null { b"\0" } else { b"\n" };
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    if parsed.vars.is_empty() {
        if parsed.json {
            let map: std::collections::BTreeMap<String, String> = std::env::vars_os()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
                .collect();
            writeln!(out, "{}", serde_json::to_string_pretty(&map)?)?;
        } else {
            // Environment order, like GNU (which walks `environ`).
            for (k, v) in std::env::vars_os() {
                out.write_all(k.as_bytes())?;
                out.write_all(b"=")?;
                out.write_all(v.as_bytes())?;
                out.write_all(term)?;
            }
        }
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    let mut exit_code = 0;
    for name in &parsed.vars {
        let value = (!name.contains('='))
            .then(|| std::env::var_os(name))
            .flatten();
        match value {
            Some(val) => {
                out.write_all(val.as_bytes())?;
                out.write_all(term)?;
            }
            None => exit_code = 1,
        }
    }
    ignore_broken_pipe(out.flush())?;
    Ok(exit_code)
}
