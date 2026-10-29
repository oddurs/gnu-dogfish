//! install-links — Creates BusyBox-style symlinks for all 40 utilities

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use std::ffi::OsString;

use anyhow::Result;
use dogfish_core::{DogfishContext, outln};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::quote::quote_locale;

const INSTALL_LINKS_HELP: &str = "\
Create symlinks for all Dogfish utilities in TARGET_DIR

Usage: install-links [OPTIONS] <TARGET_DIR>

Arguments:
  <TARGET_DIR>  Directory to create symlinks in (e.g. ./bin or ~/.local/bin)

Options:
      --prefix <PREFIX>  Optional prefix for symlink names (e.g. `df-` creates `df-ls`, `df-cat`)
                         [default: \"\"]
  -h, --help             Print help
";

struct InstallLinksArgs {
    target_dir: OsString,
    prefix: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InstallLinksOpt {
    Prefix,
    Help,
    Version,
}

const SPECS: &[OptSpec<InstallLinksOpt>] = &[
    OptSpec::long("prefix", HasArg::Required, InstallLinksOpt::Prefix),
    OptSpec::both(b'h', "help", HasArg::No, InstallLinksOpt::Help),
    OptSpec::long("version", HasArg::No, InstallLinksOpt::Version),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a usage diagnostic.
fn parse_install_links_args(args: Vec<OsString>) -> std::result::Result<InstallLinksArgs, i32> {
    let mut prefix = String::new();
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("install-links", &e))?;
        match o {
            InstallLinksOpt::Prefix => prefix = v.as_deref().map(lossy).unwrap_or_default(),
            InstallLinksOpt::Help => return Err(print_help(INSTALL_LINKS_HELP)),
            InstallLinksOpt::Version => return Err(print_version("install-links")),
        }
    }
    let mut operands = opts.operands().into_iter();
    let Some(target_dir) = operands.next() else {
        return Err(usage_error("install-links", "missing operand"));
    };
    if let Some(extra) = operands.next() {
        return Err(usage_error(
            "install-links",
            format!("extra operand {}", quote_locale(&extra)),
        ));
    }
    Ok(InstallLinksArgs { target_dir, prefix })
}

pub fn run_install_links(
    args: Vec<OsString>,
    tool_names: &[&str],
    ctx: &DogfishContext,
) -> Result<i32> {
    let parsed = match parse_install_links_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let target = Path::new(&parsed.target_dir);
    fs::create_dir_all(target)?;
    let exe = std::env::current_exe()?;
    let st = ctx.style();

    let mut count = 0;
    for &tool in tool_names {
        if matches!(
            tool,
            "install-links" | "init" | "bench" | "undo" | "history"
        ) {
            continue;
        }
        let link_name = format!("{}{}", parsed.prefix, tool);
        let link_path = target.join(&link_name);
        if link_path.exists() || link_path.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&link_path);
        }
        symlink(&exe, &link_path)?;
        count += 1;
    }

    ctx.telemetry.add_items(count);
    outln!(
        "{} Installed {} utility symlinks in '{}' -> '{}'",
        st.bold_green("✔"),
        st.bold_cyan(&count.to_string()),
        target.display(),
        exe.display()
    );
    Ok(0)
}

/// install-links's option table and help text, for its manual page and shell
/// completions (`crate::manual`).
pub(crate) fn manual() -> crate::manual::Manual {
    crate::manual::Manual::new("install-links", INSTALL_LINKS_HELP, SPECS)
}
