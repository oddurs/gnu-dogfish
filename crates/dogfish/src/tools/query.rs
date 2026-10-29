//! query / filter — Dedicated NDJSON SQL-like pipeline tool (`where`, `sort`, `select`, `limit`)

use anyhow::{Result, anyhow};
use clap::Parser;
use dogfish_core::{
    DogfishContext, emit_objects_or_table, eval_json_where, read_ndjson_stdin,
    table::{compare_json_field, read_all_stdin_lines},
};
use rayon::prelude::*;
use serde_json::{Map, Value};

#[derive(Parser)]
#[command(
    name = "query",
    visible_alias = "filter",
    about = "Filter, sort, and project NDJSON object streams (`dogfish ls -J | dogfish query -w \"size>1K\" -s size -d -f name,size`)"
)]
struct QueryArgs {
    /// Filter expression (`size>10K`, `kind=file`, `name~rs`, `lines>=100`)
    #[arg(short = 'w', long = "where")]
    where_expr: Vec<String>,

    /// Sort by field name
    #[arg(short = 's', long = "sort", visible_alias = "by")]
    sort_by: Option<String>,

    /// Sort descending
    #[arg(short = 'd', long = "desc", visible_short_alias = 'r')]
    desc: bool,

    /// Comma-separated fields to select/project (`name,size,kind`)
    #[arg(short = 'f', long = "select", visible_alias = "fields")]
    select: Option<String>,

    /// Limit number of output rows
    #[arg(short = 'n', long = "limit")]
    limit: Option<usize>,

    /// Emit raw JSON / NDJSON instead of auto-table on TTY
    #[arg(short = 'J', long = "json")]
    json: bool,
}

pub fn run_query(args: Vec<String>, ctx: &DogfishContext) -> Result<i32> {
    let parsed = QueryArgs::try_parse_from(args)?;
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
