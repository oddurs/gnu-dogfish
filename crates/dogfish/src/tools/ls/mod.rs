//! ls — Colors, grid, `--tree` and `--json` on a TTY; GNU `ls` otherwise

pub(crate) mod colors;
mod lister;
mod options;
mod rich;
pub(crate) mod version_sort;

pub(crate) use options::manual;

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use anyhow::Result;
use dogfish_core::{DogfishContext, RecordWriter, stdout};

use crate::common::uname_info::get_uname_info;
use crate::tools::ls::colors::{C_EXEC, C_MISSING, C_ORPHAN, LsColors};
use crate::tools::ls::lister::{Counted, Dired, Lister, file_escape};
use crate::tools::ls::options::{LsFormat, LsIgnore, parse_ls_args};
use crate::tools::ls::rich::render_tree;

pub fn run_ls(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    // `--color=WHEN` and the Dogfish `--no-color` each set both the GNU and
    // the rich output's colours; the later of the two wins.
    let (mut opts, operands) = match parse_ls_args(&args[1..], ctx.stdout_is_tty(), &ctx.switches) {
        Ok(v) => v,
        Err(status) => return Ok(status),
    };
    let targets: Vec<Vec<u8>> = if operands.is_empty() {
        vec![b".".to_vec()]
    } else {
        operands.into_iter().map(OsString::into_vec).collect()
    };
    let st = ctx.style();
    // -J lists files as records, and nothing else: no tree, no --dired offsets.
    if opts.json {
        opts.tree = false;
        opts.dired = false;
    }

    if opts.tree {
        ctx.telemetry.set_strategy("recursive visual tree renderer");
        let mut out = io::BufWriter::new(stdout::stdout());
        for t in &targets {
            writeln!(out, "{}", st.bold_blue(&String::from_utf8_lossy(t)))?;
            render_tree(
                Path::new(OsStr::from_bytes(t)),
                "",
                0,
                opts.level,
                opts.ignore != LsIgnore::Default,
                &mut out,
                st,
                ctx,
            )?;
        }
        out.flush()?;
        return Ok(0);
    }

    let rich = ctx.stdout_is_tty()
        && !opts.json
        && !opts.dired
        && matches!(
            opts.format,
            LsFormat::Long | LsFormat::OnePerLine | LsFormat::Columns
        );
    // GNU colours (on the GNU-format path): LS_COLORS, else the built-in table.
    if opts.color && !rich && !opts.json {
        opts.colors = LsColors::from_env();
    }
    if let Some(c) = &opts.colors {
        // GNU: no tabs in coloured output (some terminals mishandle them).
        opts.tab_size = 0;
        opts.check_symlink_mode = c.is_colored(C_ORPHAN)
            || (c.is_colored(C_EXEC) && c.referent)
            || (c.is_colored(C_MISSING) && opts.format == LsFormat::Long);
    }
    opts.check_symlink_mode |= opts.dirs_first;
    // GNU ignores a failure to get the host name (links then name no host).
    let mut hostname = Vec::new();
    if opts.hyperlink {
        let name = get_uname_info().map(|u| u.nodename).unwrap_or_default();
        file_escape(&mut hostname, name.as_bytes(), false);
    }
    let mut lister = Lister {
        opts: &opts,
        out: Counted::new(stdout::StdioBuf::new(stdout::stdout())),
        dired: Dired::default(),
        hostname,
        status: 0,
        first_header: true,
        rich: rich.then_some(st),
        json: opts
            .json
            .then(|| (RecordWriter::stdout(), Default::default())),
        active: Vec::new(),
        used_color: std::cell::Cell::new(false),
        time_cache: std::cell::RefCell::new(None),
    };
    let res = lister
        .run(&targets)
        .and_then(|()| lister.restore_default_color())
        .and_then(|()| lister.dump_dired());
    let res = res.and_then(|()| match &mut lister.json {
        Some((w, _)) => {
            ctx.telemetry.set_strategy("NDJSON records");
            w.flush()
        }
        None => Ok(()),
    });
    res?;
    let status = lister.status;
    lister.out.flush()?;
    Ok(status)
}
