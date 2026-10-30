//! A JSON Schema (draft 2020-12) validator for the keywords Dogfish's record
//! schemas use, and a checker that a schema file uses only those keywords,
//! correctly (`doc/schema/*.v1.json`).
//!
//! Small on purpose: the full validators on crates.io bring dozens of
//! dependencies and a newer MSRV, while the schemas need a dozen keywords.
//! The checker refuses any keyword outside that set, so a schema cannot
//! silently rely on one this validator ignores.

use std::collections::BTreeSet;

use base64::Engine as _;
use serde_json::{Map, Value};

pub const DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Keywords a schema may use, at any level.
const KEYWORDS: &[&str] = &[
    "$schema",
    "$id",
    "$ref",
    "$defs",
    "$comment",
    "title",
    "description",
    "type",
    "enum",
    "const",
    "properties",
    "required",
    "additionalProperties",
    "dependentRequired",
    "items",
    "minimum",
    "maximum",
    "minLength",
    "maxLength",
    "format",
    "contentEncoding",
    "anyOf",
    "oneOf",
    "allOf",
];

const TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "number", "integer", "string",
];

/// Problems with `schema` as a schema; empty if it is a valid one.
pub fn check_schema(schema: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    match schema.get("$schema").and_then(Value::as_str) {
        Some(DRAFT) => {}
        other => errs.push(format!("$schema must be {DRAFT}, not {other:?}")),
    }
    if !schema.get("$id").is_some_and(Value::is_string) {
        errs.push("$id: missing".into());
    }
    check_subschema(schema, schema, "", true, &mut errs);
    errs
}

fn check_subschema(root: &Value, s: &Value, at: &str, is_root: bool, errs: &mut Vec<String>) {
    let Some(o) = s.as_object() else {
        if !s.is_boolean() {
            errs.push(format!("{at}: a schema is an object or a boolean"));
        }
        return;
    };
    for (k, v) in o {
        let here = format!("{at}/{k}");
        if !KEYWORDS.contains(&k.as_str()) {
            errs.push(format!("{here}: unknown keyword"));
            continue;
        }
        match k.as_str() {
            "$schema" | "$id" if !is_root => errs.push(format!("{here}: only at the root")),
            "$schema" | "$id" | "$comment" | "title" | "description" if !v.is_string() => {
                errs.push(format!("{here}: must be a string"))
            }
            "$ref" => match v.as_str() {
                Some(r) if resolve(root, r).is_some() => {}
                _ => errs.push(format!("{here}: unresolved reference {v}")),
            },
            "$defs" | "properties" => match v.as_object() {
                Some(m) => {
                    for (name, sub) in m {
                        if k == "properties"
                            && !sub.get("description").is_some_and(Value::is_string)
                        {
                            errs.push(format!("{here}/{name}: every field needs a description"));
                        }
                        check_subschema(root, sub, &format!("{here}/{name}"), false, errs);
                    }
                }
                None => errs.push(format!("{here}: must be an object")),
            },
            "type" => {
                let names: Vec<&Value> = match v {
                    Value::Array(a) if !a.is_empty() => a.iter().collect(),
                    Value::String(_) => vec![v],
                    _ => vec![],
                };
                if names.is_empty()
                    || names
                        .iter()
                        .any(|n| !n.as_str().is_some_and(|n| TYPES.contains(&n)))
                {
                    errs.push(format!("{here}: not a type name or list of them: {v}"));
                }
            }
            "enum" if !v.as_array().is_some_and(|a| !a.is_empty()) => {
                errs.push(format!("{here}: must be a non-empty array"))
            }
            "required" => match string_set(v) {
                // Beside `properties`, a required name must be one of them
                // (a typo would make every record invalid, or none).
                Some(names) => {
                    if let Some(props) = o.get("properties").and_then(Value::as_object) {
                        for n in names {
                            if !props.contains_key(n) {
                                errs.push(format!("{here}: '{n}' is not in properties"));
                            }
                        }
                    }
                }
                None => errs.push(format!("{here}: must be an array of distinct strings")),
            },
            "dependentRequired" => match v.as_object() {
                Some(m) if m.values().all(|d| string_set(d).is_some()) => {}
                _ => errs.push(format!("{here}: must map names to arrays of names")),
            },
            "additionalProperties" | "items" => check_subschema(root, v, &here, false, errs),
            "anyOf" | "oneOf" | "allOf" => match v.as_array() {
                Some(a) if !a.is_empty() => {
                    for (i, sub) in a.iter().enumerate() {
                        check_subschema(root, sub, &format!("{here}/{i}"), false, errs);
                    }
                }
                _ => errs.push(format!("{here}: must be a non-empty array")),
            },
            "minimum" | "maximum" if !v.is_number() => {
                errs.push(format!("{here}: must be a number"))
            }
            "minLength" | "maxLength" if v.as_u64().is_none() => {
                errs.push(format!("{here}: must be a non-negative integer"))
            }
            "format" if v != "date-time" => errs.push(format!("{here}: unsupported format {v}")),
            "contentEncoding" if v != "base64" => {
                errs.push(format!("{here}: unsupported encoding {v}"))
            }
            _ => {}
        }
    }
}

fn string_set(v: &Value) -> Option<Vec<&str>> {
    let a = v.as_array()?;
    let names: Vec<&str> = a.iter().map(Value::as_str).collect::<Option<_>>()?;
    let distinct: BTreeSet<&str> = names.iter().copied().collect();
    (distinct.len() == names.len()).then_some(names)
}

fn resolve<'a>(root: &'a Value, r: &str) -> Option<&'a Value> {
    root.pointer(r.strip_prefix('#')?)
}

/// Problems with `v` against `schema` (empty if it is valid). With `strict`,
/// an object must not have properties its schema does not declare (the
/// schemas allow them, as later releases may add fields; Dogfish's own
/// output must not have undeclared ones).
pub fn validate(schema: &Value, v: &Value, strict: bool) -> Vec<String> {
    let mut errs = Vec::new();
    Validator {
        root: schema,
        strict,
    }
    .check(schema, v, "", &mut errs);
    errs
}

struct Validator<'a> {
    root: &'a Value,
    strict: bool,
}

impl Validator<'_> {
    fn check(&self, s: &Value, v: &Value, at: &str, errs: &mut Vec<String>) {
        let o = match s {
            Value::Bool(true) => return,
            Value::Bool(false) => return errs.push(format!("{at}: nothing is allowed here")),
            Value::Object(o) => o,
            _ => return errs.push(format!("{at}: bad schema")),
        };
        if let Some(r) = o.get("$ref").and_then(Value::as_str) {
            match resolve(self.root, r) {
                Some(target) => self.check(target, v, at, errs),
                None => errs.push(format!("{at}: unresolved {r}")),
            }
        }
        if let Some(t) = o.get("type") {
            let ok = match t {
                Value::String(name) => is_type(v, name),
                Value::Array(names) => names
                    .iter()
                    .any(|n| n.as_str().is_some_and(|n| is_type(v, n))),
                _ => false,
            };
            if !ok {
                errs.push(format!("{at}: {v} is not of type {t}"));
                return;
            }
        }
        if let Some(e) = o.get("enum").and_then(Value::as_array)
            && !e.contains(v)
        {
            errs.push(format!("{at}: {v} is not one of {e:?}"));
        }
        if let Some(c) = o.get("const")
            && c != v
        {
            errs.push(format!("{at}: {v} is not {c}"));
        }
        if let Some(n) = v.as_f64() {
            if let Some(min) = o.get("minimum").and_then(Value::as_f64)
                && n < min
            {
                errs.push(format!("{at}: {v} < {min}"));
            }
            if let Some(max) = o.get("maximum").and_then(Value::as_f64)
                && n > max
            {
                errs.push(format!("{at}: {v} > {max}"));
            }
        }
        if let Some(text) = v.as_str() {
            let len = text.chars().count() as u64;
            if o.get("minLength")
                .and_then(Value::as_u64)
                .is_some_and(|m| len < m)
            {
                errs.push(format!("{at}: {v} is too short"));
            }
            if o.get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|m| len > m)
            {
                errs.push(format!("{at}: {v} is too long"));
            }
            if o.get("format").is_some_and(|f| f == "date-time") && !is_rfc3339(text) {
                errs.push(format!("{at}: {v} is not an RFC 3339 date-time"));
            }
            if o.get("contentEncoding").is_some_and(|e| e == "base64")
                && base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .is_err()
            {
                errs.push(format!("{at}: {v} is not base64"));
            }
        }
        if let Some(obj) = v.as_object() {
            self.check_object(o, obj, at, errs);
        }
        if let (Some(items), Some(arr)) = (o.get("items"), v.as_array()) {
            for (i, item) in arr.iter().enumerate() {
                self.check(items, item, &format!("{at}/{i}"), errs);
            }
        }
        if let Some(all) = o.get("allOf").and_then(Value::as_array) {
            for sub in all {
                self.check(sub, v, at, errs);
            }
        }
        for (key, need) in [("anyOf", 1..=usize::MAX), ("oneOf", 1..=1)] {
            if let Some(subs) = o.get(key).and_then(Value::as_array) {
                let passing = subs
                    .iter()
                    .filter(|sub| {
                        let mut e = Vec::new();
                        self.check(sub, v, at, &mut e);
                        e.is_empty()
                    })
                    .count();
                if !need.contains(&passing) {
                    errs.push(format!("{at}: {passing} of the {key} schemas match {v}"));
                }
            }
        }
    }

    fn check_object(
        &self,
        o: &Map<String, Value>,
        obj: &Map<String, Value>,
        at: &str,
        errs: &mut Vec<String>,
    ) {
        let props = o.get("properties").and_then(Value::as_object);
        if let Some(req) = o.get("required").and_then(Value::as_array) {
            for name in req.iter().filter_map(Value::as_str) {
                if !obj.contains_key(name) {
                    errs.push(format!("{at}: missing required '{name}'"));
                }
            }
        }
        if let Some(deps) = o.get("dependentRequired").and_then(Value::as_object) {
            for (key, needs) in deps {
                if obj.contains_key(key) {
                    for n in needs
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        if !obj.contains_key(n) {
                            errs.push(format!("{at}: '{key}' needs '{n}'"));
                        }
                    }
                }
            }
        }
        for (k, val) in obj {
            let here = format!("{at}/{k}");
            match props.and_then(|p| p.get(k)) {
                Some(sub) => self.check(sub, val, &here, errs),
                None => match o.get("additionalProperties") {
                    Some(extra) => self.check(extra, val, &here, errs),
                    None if self.strict && props.is_some() => {
                        errs.push(format!("{here}: not declared in the schema"))
                    }
                    None => {}
                },
            }
        }
    }
}

fn is_type(v: &Value, name: &str) -> bool {
    match name {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        "string" => v.is_string(),
        _ => false,
    }
}

/// RFC 3339 `date-time` (`YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`), with the
/// ranges of each field checked loosely (no month lengths).
pub fn is_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| {
        b.get(r.clone())
            .is_some_and(|d| d.iter().all(u8::is_ascii_digit))
    };
    let num =
        |r: std::ops::Range<usize>| -> u32 { s.get(r).and_then(|t| t.parse().ok()).unwrap_or(99) };
    if b.len() < 20
        || !digits(0..4)
        || b[4] != b'-'
        || !digits(5..7)
        || b[7] != b'-'
        || !digits(8..10)
        || !matches!(b[10], b'T' | b't')
        || !digits(11..13)
        || b[13] != b':'
        || !digits(14..16)
        || b[16] != b':'
        || !digits(17..19)
    {
        return false;
    }
    if !(1..=12).contains(&num(5..7))
        || !(1..=31).contains(&num(8..10))
        || num(11..13) > 23
        || num(14..16) > 59
        || num(17..19) > 60
    {
        return false;
    }
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    match &b[i..] {
        [b'Z' | b'z'] => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => [h1, h2, m1, m2].iter().all(|c| c.is_ascii_digit()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_validator_validates() {
        let s = json!({
            "$schema": DRAFT, "$id": "x",
            "type": "object",
            "properties": {
                "n": {"type": "integer", "minimum": 0, "description": "d"},
                "t": {"type": "string", "format": "date-time", "description": "d"},
                "p": {"type": "string", "description": "d"},
                "p_bytes": {"type": "string", "contentEncoding": "base64", "description": "d"},
                "k": {"type": "string", "enum": ["a", "b"], "description": "d"}
            },
            "required": ["n"],
            "dependentRequired": {"p_bytes": ["p"]}
        });
        assert!(check_schema(&s).is_empty(), "{:?}", check_schema(&s));
        let ok = json!({"n": 1, "t": "2026-10-29T18:03:19.123456789Z", "p": "a", "p_bytes": "YQ==", "k": "a"});
        assert!(validate(&s, &ok, true).is_empty());
        for bad in [
            json!({}),
            json!({"n": -1}),
            json!({"n": 1.5}),
            json!({"n": 1, "t": "2026-10-29 18:03:19"}),
            json!({"n": 1, "p_bytes": "YQ=="}),
            json!({"n": 1, "p": "a", "p_bytes": "*"}),
            json!({"n": 1, "k": "c"}),
            json!([1]),
        ] {
            assert!(!validate(&s, &bad, false).is_empty(), "{bad}");
        }
        assert!(validate(&s, &json!({"n": 1, "new": 2}), false).is_empty());
        assert!(!validate(&s, &json!({"n": 1, "new": 2}), true).is_empty());
    }

    #[test]
    fn the_checker_refuses_what_the_validator_would_ignore() {
        for bad in [
            json!({"$schema": DRAFT, "$id": "x", "pattern": "a"}),
            json!({"$schema": DRAFT, "$id": "x", "type": "str"}),
            json!({"$schema": DRAFT, "$id": "x", "properties": {"a": {"type": "string"}}}),
            json!({"$schema": DRAFT, "$id": "x", "properties": {}, "required": ["a"]}),
            json!({"$schema": DRAFT, "$id": "x", "$ref": "#/$defs/missing"}),
            json!({"$schema": "http://json-schema.org/draft-07/schema#", "$id": "x"}),
            json!({"$schema": DRAFT, "$id": "x", "format": "email"}),
        ] {
            assert!(!check_schema(&bad).is_empty(), "{bad}");
        }
    }

    #[test]
    fn rfc3339() {
        assert!(is_rfc3339("1970-01-01T00:00:00Z"));
        assert!(is_rfc3339("2026-10-29T18:03:19.123456789Z"));
        assert!(is_rfc3339("2026-10-29T18:03:19+02:00"));
        assert!(!is_rfc3339("2026-13-29T18:03:19Z"));
        assert!(!is_rfc3339("2026-10-29T18:03:19."));
        assert!(!is_rfc3339("2026-10-29"));
    }
}
