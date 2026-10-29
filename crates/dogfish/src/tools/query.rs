//! query / filter — Dedicated NDJSON SQL-like pipeline tool (`where`, `sort`, `select`, `limit`)

use std::ffi::OsString;

use anyhow::{Result, anyhow};
use dogfish_core::{
    DogfishContext, emit_objects_or_table, eval_json_where, read_ndjson_stdin,
    table::{compare_json_field, read_all_stdin_lines},
};
use rayon::prelude::*;
use serde_json::{Map, Value};

use crate::common::getopt::{
    Getopt, HasArg, OptSpec, getopt_error, lossy, print_help, print_version, usage_error,
};
use crate::common::quote::quote_locale;

const QUERY_HELP: &str = "\
Filter, sort, and project NDJSON object streams (`dogfish ls -J | dogfish query -w \"size>1K\" -s size
-d -f name,size`)

Usage: query [OPTIONS]

Options:
  -w, --where <WHERE_EXPR>  Filter expression (`size>10K`, `kind=file`, `name~rs`, `lines>=100`)
  -s, --sort <SORT_BY>      Sort by field name [alias: --by]
  -d, --desc                Sort descending [alias: -r]
  -f, --select <SELECT>     Comma-separated fields to select/project (`name,size,kind`) [alias:
                            --fields]
  -n, --limit <LIMIT>       Limit number of output rows
  -J, --json                Emit raw JSON / NDJSON instead of auto-table on TTY
  -h, --help                Print help
";

#[derive(Default)]
struct QueryArgs {
    where_expr: Vec<String>,
    sort_by: Option<String>,
    desc: bool,
    select: Option<String>,
    limit: Option<usize>,
    json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueryOpt {
    Where,
    Sort,
    Desc,
    Select,
    Limit,
    Json,
    Help,
    Version,
}

const SPECS: &[OptSpec<QueryOpt>] = &[
    OptSpec::both(b'w', "where", HasArg::Required, QueryOpt::Where),
    OptSpec::both(b's', "sort", HasArg::Required, QueryOpt::Sort),
    OptSpec::long("by", HasArg::Required, QueryOpt::Sort),
    OptSpec::both(b'd', "desc", HasArg::No, QueryOpt::Desc),
    OptSpec::both(b'f', "select", HasArg::Required, QueryOpt::Select),
    OptSpec::long("fields", HasArg::Required, QueryOpt::Select),
    OptSpec::both(b'n', "limit", HasArg::Required, QueryOpt::Limit),
    OptSpec::both(b'J', "json", HasArg::No, QueryOpt::Json),
    OptSpec::both(b'h', "help", HasArg::No, QueryOpt::Help),
    OptSpec::long("version", HasArg::No, QueryOpt::Version),
    OptSpec::short(b'r', HasArg::No, QueryOpt::Desc),
];

/// Parses the command line; `Err` is the exit status after `--help`,
/// `--version` or a diagnostic.
fn parse_query_args(args: Vec<OsString>) -> std::result::Result<QueryArgs, i32> {
    // `filter` is the same tool under another name; its help says so.
    let name = args.first().map(|a| lossy(a)).unwrap_or_default();
    let mut p = QueryArgs::default();
    let mut opts = Getopt::new(args.into_iter().skip(1).collect(), SPECS);
    while let Some(opt) = opts.next() {
        let (o, v) = opt.map_err(|e| getopt_error("query", &e))?;
        let v = v.as_deref().map(lossy).unwrap_or_default();
        match o {
            QueryOpt::Where => p.where_expr.push(v),
            QueryOpt::Sort => p.sort_by = Some(v),
            QueryOpt::Desc => p.desc = true,
            QueryOpt::Select => p.select = Some(v),
            QueryOpt::Limit => match v.parse() {
                Ok(n) => p.limit = Some(n),
                Err(_) => {
                    return Err(usage_error(
                        "query",
                        format!("invalid argument {} for '--limit'", quote_locale(&v)),
                    ));
                }
            },
            QueryOpt::Json => p.json = true,
            QueryOpt::Help => {
                let usage = format!("Usage: {name} ");
                return Err(print_help(&QUERY_HELP.replacen("Usage: query ", &usage, 1)));
            }
            QueryOpt::Version => return Err(print_version(&name)),
        }
    }
    if let Some(extra) = opts.operands().first() {
        return Err(usage_error(
            "query",
            format!("extra operand {}", quote_locale(extra)),
        ));
    }
    Ok(p)
}

pub fn run_query(args: Vec<OsString>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = match parse_query_args(args) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    ctx.telemetry
        .set_strategy("NDJSON query engine (where + sort + select)");

    let all_lines = read_all_stdin_lines()?;
    let Some(mut objs) = read_ndjson_stdin(&all_lines) else {
        return Err(anyhow!(
            "query: stdin is not an NDJSON object stream. Tip: pass `-J` to the upstream command (e.g. `dogfish ls -J | dogfish query ...`)"
        ));
    };

    for expr in &parsed.where_expr {
        objs.retain(|o| eval_json_where(o, expr));
    }

    if let Some(ref field) = parsed.sort_by {
        objs.par_sort_by(|a, b| {
            let ord = compare_json_field(a, b, field);
            if parsed.desc { ord.reverse() } else { ord }
        });
    }

    if let Some(lim) = parsed.limit {
        objs.truncate(lim);
    }

    if let Some(ref f_spec) = parsed.select {
        let requested: Vec<&str> = f_spec.split(',').map(|s| s.trim()).collect();
        objs = objs
            .into_iter()
            .map(|obj| {
                let mut m = Map::new();
                if let Some(orig) = obj.as_object() {
                    for &k in &requested {
                        if let Some(v) = orig.get(k) {
                            m.insert(k.to_string(), v.clone());
                        }
                    }
                }
                Value::Object(m)
            })
            .collect();
    }

    ctx.telemetry.add_items(objs.len() as u64);
    emit_objects_or_table(&objs, ctx, parsed.json, !parsed.json)?;
    Ok(0)
}
