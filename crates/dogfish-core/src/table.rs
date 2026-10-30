use std::io::{self, Read, Write};

use serde_json::{Map, Value};
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

/// One record read from an upstream producer: the parsed object, and the
/// compact line it came as, so a consumer that passes it on writes it back
/// unchanged (field order and all).
#[derive(Debug, Clone, Default)]
pub struct InputRecord {
    pub value: Value,
    pub line: String,
}

impl InputRecord {
    fn new(value: Value, line: String) -> Self {
        Self { value, line }
    }

    /// The record with only `fields`, in that order. A selected field keeps
    /// its `<field>_bytes` twin (the raw bytes of a name that is not UTF-8).
    pub fn project(&self, fields: &[&str]) -> Self {
        let mut map = Map::new();
        let mut line = String::from("{");
        let Some(orig) = self.value.as_object() else {
            return Self::new(Value::Object(map), "{}".into());
        };
        let mut put = |k: &str, v: &Value, map: &mut Map<String, Value>| {
            if map.contains_key(k) {
                return;
            }
            if line.len() > 1 {
                line.push(',');
            }
            let mut key = Vec::new();
            crate::ndjson::push_json_str(&mut key, k);
            line.push_str(&String::from_utf8_lossy(&key));
            line.push(':');
            line.push_str(&v.to_string());
            map.insert(k.to_string(), v.clone());
        };
        for &k in fields {
            if let Some(v) = orig.get(k) {
                put(k, v, &mut map);
                let twin = format!("{k}_bytes");
                if let Some(b) = orig.get(&twin) {
                    put(&twin, b, &mut map);
                }
            }
        }
        line.push('}');
        Self::new(Value::Object(map), line)
    }

    /// The record with `key` set to `value` (appended, or replaced).
    pub fn with_field(mut self, key: &str, value: Value) -> Self {
        let Some(map) = self.value.as_object_mut() else {
            return self;
        };
        let replaced = map.insert(key.to_string(), value.clone()).is_some();
        if replaced {
            self.line = self.value.to_string();
        } else if let Some(body) = self.line.trim_end().strip_suffix('}') {
            let mut line = body.to_string();
            if map.len() > 1 {
                line.push(',');
            }
            let mut k = Vec::new();
            crate::ndjson::push_json_str(&mut k, key);
            line.push_str(&String::from_utf8_lossy(&k));
            line.push(':');
            line.push_str(&value.to_string());
            line.push('}');
            self.line = line;
        } else {
            self.line = self.value.to_string();
        }
        self
    }
}

/// Parses `input` as a stream of records from an upstream Dogfish producer
/// (or anything else that writes NDJSON): every non-blank line one JSON
/// object. A single JSON array of objects is taken too, for input from
/// other programs. `None` if the input is anything else; blank input is no
/// records.
pub fn parse_ndjson(input: &[u8]) -> Option<Vec<InputRecord>> {
    let text = String::from_utf8_lossy(input);
    let lines: Vec<&str> = text
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let Some(first) = lines.first() else {
        return Some(Vec::new());
    };
    if first.starts_with('[') {
        let joined = lines.join("\n");
        return match serde_json::from_str::<Value>(&joined) {
            Ok(Value::Array(arr)) if arr.iter().all(Value::is_object) => Some(
                arr.into_iter()
                    .map(|v| {
                        let line = v.to_string();
                        InputRecord::new(v, line)
                    })
                    .collect(),
            ),
            _ => None,
        };
    }
    let mut records = Vec::with_capacity(lines.len());
    for line in lines {
        if !line.starts_with('{') {
            return None;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(val @ Value::Object(_)) => records.push(InputRecord::new(val, line.to_string())),
            _ => return None,
        }
    }
    Some(records)
}

/// The field to sort or group records by when none is named: `name` if the
/// records have one, else `path`.
pub fn default_key(records: &[InputRecord]) -> &'static str {
    match records.first().and_then(|r| r.value.as_object()) {
        Some(o) if !o.contains_key("name") && o.contains_key("path") => "path",
        _ => "name",
    }
}

/// Writes records on to the next tool: NDJSON, each record as it came, or a
/// table on a terminal unless `json` (`-J`) asks for the records there too.
pub fn emit_records(records: &[InputRecord], ctx: &DogfishContext, json: bool) -> io::Result<()> {
    if !json && ctx.stdout_is_tty() {
        let values: Vec<&Value> = records.iter().map(|r| &r.value).collect();
        let mut out = io::BufWriter::new(crate::stdout::stdout());
        render_json_objects_as_table(&values, &mut out, ctx.style())?;
        return out.flush();
    }
    let mut w = crate::ndjson::RecordWriter::stdout();
    for r in records {
        w.line(r.line.as_bytes())?;
    }
    w.flush()
}

/// Renders JSON objects as an aligned terminal table, sizes made readable.
/// The machine-only twins of a field (`path_bytes`, `mtime_ns`) are left out.
pub fn render_json_objects_as_table<W: Write>(
    objects: &[&Value],
    out: &mut W,
    style: Style,
) -> io::Result<()> {
    if objects.is_empty() {
        return Ok(());
    }
    let Some(first_obj) = objects[0].as_object() else {
        return Ok(());
    };
    let twin = |k: &str| {
        ["_bytes", "_ns"].iter().any(|suffix| {
            k.strip_suffix(suffix)
                .is_some_and(|stem| first_obj.contains_key(stem))
        })
    };
    let keys: Vec<String> = first_obj.keys().filter(|k| !twin(k)).cloned().collect();
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
        // Integers exactly: nanosecond times do not fit an f64.
        (Some(Value::Number(na)), Some(Value::Number(nb)))
            if na.as_i64().is_some() && nb.as_i64().is_some() =>
        {
            na.as_i64().cmp(&nb.as_i64())
        }
        (Some(Value::Number(na)), Some(Value::Number(nb)))
            if na.as_u64().is_some() && nb.as_u64().is_some() =>
        {
            na.as_u64().cmp(&nb.as_u64())
        }
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

/// All of standard input.
pub fn read_all_stdin() -> io::Result<Vec<u8>> {
    let mut all = Vec::new();
    io::stdin().lock().read_to_end(&mut all)?;
    Ok(all)
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
    fn test_parse_ndjson_objects_and_arrays() {
        let input = b"{\"name\":\"a.rs\",\"size\":120}\n\n{\"size\":450,\"name\":\"b.rs\"}\r\n";
        let parsed = parse_ndjson(input).unwrap();
        assert_eq!(parsed.len(), 2);
        // Lines are kept as they came, field order included.
        assert_eq!(parsed[1].line, r#"{"size":450,"name":"b.rs"}"#);
        assert_eq!(parsed[1].value["size"], 450);

        let arr = b"[\n  {\"name\":\"x\",\"size\":10},\n  {\"name\":\"y\",\"size\":20}\n]\n";
        let parsed_arr = parse_ndjson(arr).unwrap();
        assert_eq!(parsed_arr.len(), 2);
        assert_eq!(parsed_arr[0].line, r#"{"name":"x","size":10}"#);

        assert!(parse_ndjson(b"hello world\nnot json\n").is_none());
        assert!(parse_ndjson(b"{\"a\":1}\n[1]\n").is_none());
        assert!(parse_ndjson(b"[1, 2]").is_none());
        assert_eq!(parse_ndjson(b"\n \n").map(|r| r.len()), Some(0));
    }

    #[test]
    fn records_project_and_gain_fields_in_place() {
        let input = r#"{"path":"a�","path_bytes":"Yf8=","size":3,"type":"file"}"#;
        let r = &parse_ndjson(input.as_bytes()).unwrap()[0];
        let p = r.project(&["size", "path", "nope"]);
        assert_eq!(p.line, r#"{"size":3,"path":"a�","path_bytes":"Yf8="}"#);
        assert_eq!(p.value["path_bytes"], "Yf8=");
        let c = r.clone().with_field("count", json!(2));
        assert_eq!(
            c.line,
            r#"{"path":"a�","path_bytes":"Yf8=","size":3,"type":"file","count":2}"#
        );
        assert_eq!(c.value["count"], 2);
        let again = c.with_field("count", json!(5));
        assert_eq!(again.value["count"], 5);
        assert_eq!(
            serde_json::from_str::<Value>(&again.line).unwrap()["count"],
            5
        );
        let empty = parse_ndjson(b"{}").unwrap()[0]
            .clone()
            .with_field("count", json!(1));
        assert_eq!(empty.line, r#"{"count":1}"#);
        assert_eq!(
            default_key(&parse_ndjson(br#"{"path":"x"}"#).unwrap()),
            "path"
        );
        assert_eq!(
            default_key(&parse_ndjson(br#"{"name":"x","path":"y"}"#).unwrap()),
            "name"
        );
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
        // Nanosecond times differ in digits an f64 drops.
        let a = json!({"mtime_ns": 1_793_210_599_123_456_789_i64});
        let b = json!({"mtime_ns": 1_793_210_599_123_456_790_i64});
        assert_eq!(
            compare_json_field(&a, &b, "mtime_ns"),
            std::cmp::Ordering::Less
        );
    }
}
