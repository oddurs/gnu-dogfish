//! `dogfish schema [TOOL]`: the JSON Schema of each tool's `-J` records
//! (`doc/schema/<tool>.v1.json`, built into the binary).

use dogfish_core::stdout;

/// Each record producer and the schema of its version-1 records, and
/// `error`: the error record every producer's stream may hold.
pub(crate) const SCHEMAS: &[(&str, &str)] = &[
    ("b2sum", include_str!("../schema/b2sum.v1.json")),
    ("basename", include_str!("../schema/basename.v1.json")),
    ("bench", include_str!("../schema/bench.v1.json")),
    ("chmod", include_str!("../schema/chmod.v1.json")),
    ("chown", include_str!("../schema/chown.v1.json")),
    ("cp", include_str!("../schema/cp.v1.json")),
    ("date", include_str!("../schema/date.v1.json")),
    ("df", include_str!("../schema/df.v1.json")),
    ("dirname", include_str!("../schema/dirname.v1.json")),
    ("du", include_str!("../schema/du.v1.json")),
    ("env", include_str!("../schema/env.v1.json")),
    ("error", include_str!("../schema/error.v1.json")),
    ("history", include_str!("../schema/history.v1.json")),
    ("hostname", include_str!("../schema/hostname.v1.json")),
    ("id", include_str!("../schema/id.v1.json")),
    (
        "install-links",
        include_str!("../schema/install-links.v1.json"),
    ),
    ("ln", include_str!("../schema/ln.v1.json")),
    ("ls", include_str!("../schema/ls.v1.json")),
    ("md5sum", include_str!("../schema/md5sum.v1.json")),
    ("mkdir", include_str!("../schema/mkdir.v1.json")),
    ("mktemp", include_str!("../schema/mktemp.v1.json")),
    ("mv", include_str!("../schema/mv.v1.json")),
    ("nproc", include_str!("../schema/nproc.v1.json")),
    ("printenv", include_str!("../schema/printenv.v1.json")),
    ("realpath", include_str!("../schema/realpath.v1.json")),
    ("rm", include_str!("../schema/rm.v1.json")),
    ("seq", include_str!("../schema/seq.v1.json")),
    ("sha256sum", include_str!("../schema/sha256sum.v1.json")),
    ("stat", include_str!("../schema/stat.v1.json")),
    ("tee", include_str!("../schema/tee.v1.json")),
    ("touch", include_str!("../schema/touch.v1.json")),
    ("uname", include_str!("../schema/uname.v1.json")),
    ("undo", include_str!("../schema/undo.v1.json")),
    ("wc", include_str!("../schema/wc.v1.json")),
    ("whoami", include_str!("../schema/whoami.v1.json")),
];

/// The tools whose `-J` passes on the records they read (with a field
/// selected away, or `count` added) rather than writing their own.
const CONSUMERS: &[&str] = &[
    "cut", "filter", "head", "query", "shuf", "sort", "tail", "uniq",
];

/// The tools whose output is the data itself (byte filters, and the tools
/// that print what they are given): they take no `-J` and write no records.
const BYTE_FILTERS: &[&str] = &[
    "base64", "basenc", "cat", "echo", "nl", "paste", "printf", "tac", "tr", "yes",
];

/// The schema of `tool`'s records, if it writes any of its own.
pub(crate) fn schema_of(tool: &str) -> Option<&'static str> {
    SCHEMAS.iter().find(|(t, _)| *t == tool).map(|(_, s)| *s)
}

/// Runs `dogfish schema ARGS`: with no argument, lists the tools that have
/// a schema; with a tool's name, prints its schema. Returns the exit status.
pub(crate) fn run(args: &[std::ffi::OsString]) -> i32 {
    let usage = "Usage: dogfish schema [TOOL]";
    match args {
        [] => {
            for (tool, _) in SCHEMAS {
                stdout::print_str(tool);
                stdout::print_str("\n");
            }
            0
        }
        [arg] if arg == "--help" || arg == "-h" => {
            stdout::print_str(&format!(
                "{usage}\nPrint the JSON Schema of TOOL's -J records (version 1), or list the \
                 tools that have one; `error` is the error record any of them may write.\n"
            ));
            0
        }
        [tool] => {
            let tool = tool.to_string_lossy();
            match schema_of(&tool) {
                Some(s) => {
                    stdout::print_str(s);
                    0
                }
                None if CONSUMERS.contains(&tool.as_ref()) => {
                    eprintln!(
                        "dogfish: schema: {tool} -J writes the records it reads; \
                         see the schema of the tool that wrote them"
                    );
                    1
                }
                None if BYTE_FILTERS.contains(&tool.as_ref()) => {
                    eprintln!(
                        "dogfish: schema: {tool} writes no records: its output is the data \
                         (doc/ndjson.md, \"Tools without records\")"
                    );
                    1
                }
                None => {
                    eprintln!(
                        "dogfish: schema: no schema for '{tool}' (`dogfish schema` lists them)"
                    );
                    1
                }
            }
        }
        _ => {
            eprintln!("dogfish: schema: extra operand\n{usage}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_is_json_for_its_own_tool() {
        for (tool, text) in SCHEMAS {
            let v: serde_json::Value = serde_json::from_str(text).expect(tool);
            let id = v["$id"].as_str().unwrap_or_default();
            assert!(id.ends_with(&format!("/{tool}.v1.json")), "{tool}: {id}");
            assert!(
                *tool == "error" || crate::ALL_TOOLS.iter().any(|(n, _, _)| n == tool),
                "{tool}"
            );
        }
        for tool in CONSUMERS.iter().chain(BYTE_FILTERS) {
            assert!(schema_of(tool).is_none());
        }
        for tool in BYTE_FILTERS {
            assert!(crate::ALL_TOOLS.iter().any(|(n, _, _)| n == tool), "{tool}");
        }
    }
}
