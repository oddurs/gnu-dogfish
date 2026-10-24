use std::io::{self, BufRead, Write};

use serde::Serialize;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::context::DogfishContext;
use crate::format::{Style, format_size};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnAlign {
    Left,
    Right,
}

/// Unicode/ANSI-aware terminal table builder.
pub struct Table {
    headers: Vec<(String, ColumnAlign)>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: Vec<(&str, ColumnAlign)>) -> Self {
        Self {
            headers: headers
                .into_iter()
                .map(|(h, a)| (h.to_string(), a))
                .collect(),
            rows: Vec::new(),
        }
    }

    pub fn add_row(&mut self, row: Vec<String>) {
        self.rows.push(row);
    }

    pub fn render<W: Write>(&self, out: &mut W, style: Style) -> io::Result<()> {
        if self.headers.is_empty() {
            return Ok(());
        }
        let cols = self.headers.len();
        let mut widths: Vec<usize> = self.headers.iter().map(|(h, _)| visible_width(h)).collect();

        for row in &self.rows {
            for (i, cell) in row.iter().enumerate().take(cols) {
                let w = visible_width(cell);
                if w > widths[i] {
                    widths[i] = w;
                }
            }
        }

        // Header line
        let mut header_cells = Vec::with_capacity(cols);
        let mut rule_cells = Vec::with_capacity(cols);
        for (i, (title, align)) in self.headers.iter().enumerate() {
            let padded = pad_visible(title, widths[i], *align);
            header_cells.push(style.bold_cyan(&padded));
            rule_cells.push(style.dim(&"─".repeat(widths[i])));
        }
        writeln!(out, " {}", header_cells.join("  "))?;
        writeln!(out, " {}", rule_cells.join("──"))?;

        // Data rows
        for row in &self.rows {
            let mut cells = Vec::with_capacity(cols);
            for (i, (_, align)) in self.headers.iter().enumerate() {
                let val = row.get(i).map(|s| s.as_str()).unwrap_or("");
                cells.push(pad_visible(val, widths[i], *align));
            }
            writeln!(out, " {}", cells.join("  "))?;
        }
        Ok(())
    }
}

/// Strips ANSI escape sequences without heap allocation to compute accurate terminal column width.
pub fn visible_width(s: &str) -> usize {
    if !s.as_bytes().contains(&b'\x1b') {
        return UnicodeWidthStr::width(s);
    }
    let mut total = 0;
    let mut in_escape = false;
    let mut seg_start = 0;
    for (idx, ch) in s.char_indices() {
        if in_escape {
            if ch.is_ascii_alphabetic() {
                in_escape = false;
                seg_start = idx + ch.len_utf8();
            }
        } else if ch == '\x1b' {
            if idx > seg_start {
                total += UnicodeWidthStr::width(&s[seg_start..idx]);
            }
            in_escape = true;
        }
    }
    if !in_escape && seg_start < s.len() {
        total += UnicodeWidthStr::width(&s[seg_start..]);
    }
    total
}

pub fn pad_visible(s: &str, width: usize, align: ColumnAlign) -> String {
    let vis = visible_width(s);
    if vis >= width {
        return s.to_string();
    }
    let pad = " ".repeat(width - vis);
    match align {
        ColumnAlign::Left => format!("{}{}", s, pad),
        ColumnAlign::Right => format!("{}{}", pad, s),
    }
}

/// Attempts to read stdin lines and detect if they are NDJSON (or a JSON array) emitted by
/// an upstream Dogfish utility. Returns `Some(objects)` if structured JSON is detected, or
/// `None` if plain text.
pub fn read_ndjson_stdin(lines: &[String]) -> Option<Vec<Value>> {
    let non_empty: Vec<&str> = lines
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if non_empty.is_empty() {
        return None;
    }

    // Check if the entire input is a single JSON array
    let joined = non_empty.join("\n");
    if joined.starts_with('[') {
        if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&joined) {
            if arr.iter().all(|v| v.is_object()) {
                return Some(arr);
            }
        }
    }

    // Otherwise check if every line is a JSON object (NDJSON)
    if !non_empty[0].starts_with('{') {
        return None;
    }

    let mut objs = Vec::with_capacity(non_empty.len());
    for line in non_empty {
        match serde_json::from_str::<Value>(line) {
            Ok(val @ Value::Object(_)) => objs.push(val),
            _ => return None,
        }
    }
    Some(objs)
}

/// Emits a slice of JSON objects as:
/// - **NDJSON** when piped (`!ctx.stdout_is_tty`) or when `force_json` is false in a pipeline
/// - **Pretty JSON Array** if `--json` was explicitly passed on a TTY (`force_json && ctx.stdout_is_tty`)
/// - **Auto-formatted Terminal Table** if rendering an NDJSON stream to an interactive TTY!
pub fn emit_objects_or_table<T: Serialize>(
    items: &[T],
    ctx: &DogfishContext,
    explicit_json: bool,
    render_tty_table: bool,
) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    let values: Vec<Value> = items
        .iter()
        .filter_map(|item| serde_json::to_value(item).ok())
        .collect();

    if explicit_json && !ctx.stdout_is_tty {
        // Emit NDJSON into the pipe so downstream Dogfish utilities can stream-parse it!
        for val in &values {
            serde_json::to_writer(&mut out, val)?;
            writeln!(out)?;
        }
        return out.flush();
    }

    if explicit_json && ctx.stdout_is_tty && !render_tty_table {
        // Emit pretty JSON on terminal when explicitly asked via `-J`
        serde_json::to_writer_pretty(&mut out, &values)?;
        writeln!(out)?;
        return out.flush();
    }

    if ctx.stdout_is_tty {
        render_json_objects_as_table(&values, &mut out, ctx.style())?;
        return out.flush();
    }

    // Default piped NDJSON
    for val in &values {
        serde_json::to_writer(&mut out, val)?;
        writeln!(out)?;
    }
    out.flush()
}

/// Renders an arbitrary slice of JSON objects into a clean, aligned terminal table,
/// automatically formatting byte sizes and numeric columns!
pub fn render_json_objects_as_table<W: Write>(
    objects: &[Value],
    out: &mut W,
    style: Style,
) -> io::Result<()> {
    if objects.is_empty() {
        return Ok(());
    }
    let Some(first_obj) = objects[0].as_object() else {
        return Ok(());
    };
    let keys: Vec<String> = first_obj.keys().cloned().collect();
    if keys.is_empty() {
        return Ok(());
    }

    let headers: Vec<(&str, ColumnAlign)> = keys
        .iter()
        .map(|k| {
            let is_numeric = objects
                .iter()
                .filter_map(|o| o.get(k))
                .all(|v| v.is_number());
            let align = if is_numeric {
                ColumnAlign::Right
            } else {
                ColumnAlign::Left
            };
            (k.as_str(), align)
        })
        .collect();

    let mut table = Table::new(headers);
    for obj in objects {
        let mut row = Vec::with_capacity(keys.len());
        for k in &keys {
            let cell = match obj.get(k) {
                Some(Value::String(s)) => {
                    if k == "name" || k == "path" {
                        style.bold(s)
                    } else {
                        s.clone()
                    }
                }
                Some(Value::Number(n)) => {
                    if let Some(u) = n.as_u64() {
                        if k == "size" || k.ends_with("_bytes") {
                            style.green(&format_size(u, true))
                        } else {
                            u.to_string()
                        }
                    } else {
                        n.to_string()
                    }
                }
                Some(Value::Bool(b)) => {
                    if *b {
                        style.green("true")
                    } else {
                        style.dim("false")
                    }
                }
                Some(Value::Null) | None => style.dim("-"),
                Some(other) => other.to_string(),
            };
            row.push(cell);
        }
        table.add_row(row);
    }
    table.render(out, style)
}

/// Extracts a comparable key from a JSON `Value` by field name (`--by <field>`).
pub fn compare_json_field(a: &Value, b: &Value, field: &str) -> std::cmp::Ordering {
    let va = a.get(field);
    let vb = b.get(field);
    match (va, vb) {
        (Some(Value::Number(na)), Some(Value::Number(nb))) => {
            let fa = na.as_f64().unwrap_or(0.0);
            let fb = nb.as_f64().unwrap_or(0.0);
            fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Some(Value::String(sa)), Some(Value::String(sb))) => sa.cmp(sb),
        (Some(Value::Bool(ba)), Some(Value::Bool(bb))) => ba.cmp(bb),
        (Some(xa), Some(xb)) => xa.to_string().cmp(&xb.to_string()),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Parses a mini filter expression like `size>10K`, `kind=file`, `name~rs`, `lines>=100`
/// and evaluates it against a JSON `Value`.
pub fn eval_json_where(obj: &Value, expr: &str) -> bool {
    let ops = [">=", "<=", "!=", "==", "=", ">", "<", "~"];
    for op in ops {
        if let Some((lhs, rhs)) = expr.split_once(op) {
            let field = lhs.trim();
            let rhs = rhs.trim().trim_matches(|c| c == '\'' || c == '"');
            let Some(val) = obj.get(field) else {
                return false;
            };
            if op == "~" {
                let s = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                return s.to_ascii_lowercase().contains(&rhs.to_ascii_lowercase());
            }
            // Numeric comparison (supports human size suffixes like `10K`, `2M` on the RHS!)
            if let Some(num_lhs) = val.as_f64().or_else(|| val.as_u64().map(|u| u as f64)) {
                let num_rhs = crate::format::parse_size_str(rhs)
                    .map(|u| u as f64)
                    .or_else(|_| rhs.parse::<f64>())
                    .unwrap_or(0.0);
                return match op {
                    ">" => num_lhs > num_rhs,
                    ">=" => num_lhs >= num_rhs,
                    "<" => num_lhs < num_rhs,
                    "<=" => num_lhs <= num_rhs,
                    "=" | "==" => (num_lhs - num_rhs).abs() < 1e-9,
                    "!=" => (num_lhs - num_rhs).abs() >= 1e-9,
                    _ => false,
                };
            }
            // String / bool comparison
            let s_lhs = match val {
                Value::String(s) => s.as_str(),
                Value::Bool(b) => {
                    if *b {
                        "true"
                    } else {
                        "false"
                    }
                }
                _ => "",
            };
            return match op {
                "=" | "==" => s_lhs.eq_ignore_ascii_case(rhs),
                "!=" => !s_lhs.eq_ignore_ascii_case(rhs),
                ">" => s_lhs > rhs,
                ">=" => s_lhs >= rhs,
                "<" => s_lhs < rhs,
                "<=" => s_lhs <= rhs,
                _ => false,
            };
        }
    }
    true
}

pub fn read_all_stdin_lines() -> io::Result<Vec<String>> {
    let stdin = io::stdin();
    let reader = io::BufReader::new(stdin.lock());
    reader.lines().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_visible_width_strips_ansi() {
        let st = Style::new(true);
        let colored = st.bold_cyan("dogfish");
        assert!(colored.len() > 7);
        assert_eq!(visible_width(&colored), 7);
    }

    #[test]
    fn test_read_ndjson_stdin_objects_and_arrays() {
        let lines = vec![
            r#"{"name":"a.rs","size":120}"#.to_string(),
            r#"{"name":"b.rs","size":450}"#.to_string(),
        ];
        let parsed = read_ndjson_stdin(&lines).unwrap();
        assert_eq!(parsed.len(), 2);

        let arr_lines = vec![
            "[".to_string(),
            r#"  {"name":"x","size":10},"#.to_string(),
            r#"  {"name":"y","size":20}"#.to_string(),
            "]".to_string(),
        ];
        let parsed_arr = read_ndjson_stdin(&arr_lines).unwrap();
        assert_eq!(parsed_arr.len(), 2);

        let plain_lines = vec!["hello world".to_string(), "not json".to_string()];
        assert!(read_ndjson_stdin(&plain_lines).is_none());
    }

    #[test]
    fn test_eval_json_where_operators_and_human_units() {
        let obj = json!({
            "name": "fs_ops.rs",
            "kind": "file",
            "size": 20480,
            "executable": false
        });
        assert!(eval_json_where(&obj, "size>10K"));
        assert!(eval_json_where(&obj, "size>=20K"));
        assert!(!eval_json_where(&obj, "size>50K"));
        assert!(eval_json_where(&obj, "kind=file"));
        assert!(eval_json_where(&obj, "kind!=dir"));
        assert!(eval_json_where(&obj, "name~ops"));
        assert!(!eval_json_where(&obj, "name~main"));
        assert!(eval_json_where(&obj, "executable=false"));
    }

    #[test]
    fn test_compare_json_field_numeric_and_string() {
        let a = json!({"name": "alpha", "size": 10});
        let b = json!({"name": "beta", "size": 200});
        assert_eq!(compare_json_field(&a, &b, "size"), std::cmp::Ordering::Less);
        assert_eq!(compare_json_field(&a, &b, "name"), std::cmp::Ordering::Less);
    }
}
