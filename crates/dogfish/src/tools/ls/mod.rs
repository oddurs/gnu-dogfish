//! ls — Colors, grid, `--tree` and `--json` on a TTY; GNU `ls` otherwise

pub(crate) mod colors;
mod lister;
mod options;
mod rich;
pub(crate) mod version_sort;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;

use anyhow::Result;
use dogfish_core::{
    ColorMode, DogfishContext, emit_objects_or_table, io::ignore_broken_pipe, stdout,
};

use crate::common::getopt::lossy_all;

use crate::tools::ls::colors::{C_EXEC, C_MISSING, C_ORPHAN, LsColors};
use crate::tools::ls::lister::Lister;
use crate::tools::ls::options::{LsFormat, LsIgnore, parse_ls_args};
use crate::tools::ls::rich::render_tree;

pub fn run_ls(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let (mut opts, operands) = match parse_ls_args(&args[1..], ctx.stdout_is_tty()) {
        Ok(v) => v,
        Err(status) => return Ok(status),
    };
    // `--color=WHEN` also sets the rich output's colours; the Dogfish
    // `--no-color` turns both off.
    if let Some(mode) = opts.color_mode {
        ctx.switches.set_color(mode);
    }
    if ctx.color_mode() == ColorMode::Never {
        opts.color = false;
    }
    let targets = if operands.is_empty() {
        vec![".".to_string()]
    } else {
        lossy_all(operands)
    };
    let st = ctx.style();

    if opts.tree {
        ctx.telemetry.set_strategy("recursive visual tree renderer");
        let mut out = io::BufWriter::new(stdout::stdout());
        for t in &targets {
            writeln!(out, "{}", st.bold_blue(t))?;
            render_tree(
                Path::new(t),
                "",
                0,
                opts.level,
                opts.ignore != LsIgnore::Default,
                &mut out,
                st,
                ctx,
            )?;
        }
        ignore_broken_pipe(out.flush())?;
        return Ok(0);
    }

    let rich = ctx.stdout_is_tty()
        && !opts.json
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
    let mut lister = Lister {
        opts: &opts,
        out: io::BufWriter::new(stdout::stdout()),
        status: 0,
        first_header: true,
        rich: rich.then_some(st),
        json: Vec::new(),
        active: Vec::new(),
        used_color: std::cell::Cell::new(false),
        time_cache: std::cell::RefCell::new(None),
    };
    let res = lister
        .run(&targets)
        .and_then(|()| lister.restore_default_color());
    match res {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(lister.status),
        res => res?,
    }
    ctx.telemetry.add_items(lister.json.len() as u64);
    if opts.json {
        ctx.telemetry
            .set_strategy("NDJSON structured object stream");
        let records = std::mem::take(&mut lister.json);
        drop(lister);
        emit_objects_or_table(&records, ctx, true, false)?;
        return Ok(0);
    }
    let status = lister.status;
    ignore_broken_pipe(lister.out.flush())?;
    Ok(status)
}
