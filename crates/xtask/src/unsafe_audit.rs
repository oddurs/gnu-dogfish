//! `cargo xtask unsafe-audit`: every `unsafe` in the workspace is listed in
//! doc/unsafe.md.
//!
//! The source side is a small Rust lexer (comments, strings and character
//! literals are skipped) that finds each `unsafe` keyword and names the item it
//! belongs to: the enclosing function for an `unsafe` block, the function's own
//! name for an `unsafe fn`, and `extern`, `impl`, `trait` or `attribute` for
//! the other forms. The doc side is one `### \`path\`` heading per file and a
//! table with one row per `unsafe`, in source order, whose first cell names the
//! same item. Items rather than line numbers, so that unrelated edits do not
//! invalidate the inventory.

use std::fs;
use std::path::Path;

/// The inventory, relative to the workspace root.
pub const DOC: &str = "doc/unsafe.md";

/// One `unsafe` keyword in the source.
#[derive(Debug, PartialEq, Eq)]
pub struct Site {
    pub line: usize,
    pub item: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Ident(String),
    Punct(char),
}

/// Identifiers and punctuation of `src`, with their line numbers, skipping
/// whitespace, comments, and string, byte-string and character literals.
fn tokens(src: &str) -> Vec<(usize, Token)> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line) = (0, 1);
    // Advances past chars[i], counting lines.
    let bump = |i: &mut usize, line: &mut usize| {
        if chars[*i] == '\n' {
            *line += 1;
        }
        *i += 1;
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            bump(&mut i, &mut line);
        } else if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    bump(&mut i, &mut line);
                }
            }
        } else if c == '"' {
            skip_string(&chars, &mut i, &mut line);
        } else if c == '\'' {
            // A character literal ('a', '\n', '\u{1F600}') or a lifetime ('a).
            if next == Some('\\') {
                // Past the quote, the backslash and the escaped character.
                i += 3;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
            } else if chars.get(i + 2) == Some(&'\'') {
                i += 3;
            } else {
                i += 1;
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let at = chars.get(i).copied();
            if matches!(word.as_str(), "r" | "br" | "cr") && matches!(at, Some('"' | '#')) {
                skip_raw_string(&chars, &mut i, &mut line);
            } else if matches!(word.as_str(), "b" | "c") && at == Some('"') {
                skip_string(&chars, &mut i, &mut line);
            } else if word == "b" && at == Some('\'') {
                // A byte literal: lex the quote as a character literal.
            } else {
                out.push((line, Token::Ident(word)));
            }
        } else {
            out.push((line, Token::Punct(c)));
            i += 1;
        }
    }
    out
}

/// Skips a `"..."` literal starting at `chars[*i]`.
fn skip_string(chars: &[char], i: &mut usize, line: &mut usize) {
    *i += 1;
    while *i < chars.len() {
        match chars[*i] {
            '\\' => {
                // The escaped character, which may be a line continuation.
                *i += 1;
                if chars.get(*i) == Some(&'\n') {
                    *line += 1;
                }
            }
            '"' => {
                *i += 1;
                return;
            }
            '\n' => *line += 1,
            _ => {}
        }
        *i += 1;
    }
}

/// Skips the rest of a raw string (`#*"..."#*`) starting at `chars[*i]`; a
/// lone identifier `r` followed by `#` (raw identifiers) is left alone.
fn skip_raw_string(chars: &[char], i: &mut usize, line: &mut usize) {
    let mut hashes = 0;
    let mut j = *i;
    while chars.get(j) == Some(&'#') {
        hashes += 1;
        j += 1;
    }
    if chars.get(j) != Some(&'"') {
        return;
    }
    j += 1;
    while j < chars.len() {
        if chars[j] == '"' && (1..=hashes).all(|k| chars.get(j + k) == Some(&'#')) {
            *i = j + 1 + hashes;
            return;
        }
        if chars[j] == '\n' {
            *line += 1;
        }
        j += 1;
    }
    *i = j;
}

/// Every `unsafe` in `src`, with the item it belongs to.
pub fn scan(src: &str) -> Vec<Site> {
    let toks = tokens(src);
    let mut sites = Vec::new();
    // Brace nesting: Some(name) for a function body, None for any other block.
    let mut stack: Vec<Option<String>> = Vec::new();
    // A `fn NAME` whose body has not opened yet, and the ()/[] depth it was
    // declared at: a `;` inside its signature (`&[u8; 2]`) does not end it.
    let mut pending_fn: Option<(String, usize)> = None;
    let mut depth = 0usize;
    for (k, (line, tok)) in toks.iter().enumerate() {
        match tok {
            Token::Ident(w) if w == "fn" => {
                if let Some((_, Token::Ident(name))) = toks.get(k + 1) {
                    pending_fn = Some((name.clone(), depth));
                }
            }
            Token::Ident(w) if w == "unsafe" => {
                let item = match toks.get(k + 1) {
                    Some((_, Token::Ident(n))) if n == "fn" => match toks.get(k + 2) {
                        Some((_, Token::Ident(name))) => name.clone(),
                        _ => "fn".into(),
                    },
                    // `unsafe extern "C" fn name` (the ABI string is not a
                    // token) is a function; `unsafe extern "C" {` a block.
                    Some((_, Token::Ident(n))) if n == "extern" => match toks.get(k + 2..k + 4) {
                        Some([(_, Token::Ident(f)), (_, Token::Ident(name))]) if f == "fn" => {
                            name.clone()
                        }
                        _ => "extern".into(),
                    },
                    Some((_, Token::Ident(n))) if n == "impl" => "impl".into(),
                    Some((_, Token::Ident(n))) if n == "trait" => "trait".into(),
                    Some((_, Token::Punct('('))) => "attribute".into(),
                    _ => stack
                        .iter()
                        .rev()
                        .find_map(Clone::clone)
                        .unwrap_or_else(|| "-".into()),
                };
                sites.push(Site { line: *line, item });
            }
            Token::Punct('{') => stack.push(pending_fn.take().map(|(name, _)| name)),
            Token::Punct('}') => {
                stack.pop();
            }
            Token::Punct('(' | '[') => depth += 1,
            Token::Punct(')' | ']') => depth = depth.saturating_sub(1),
            // A function declaration without a body (`fn f();` in an extern
            // block or a trait).
            Token::Punct(';') if pending_fn.as_ref().is_some_and(|(_, d)| *d == depth) => {
                pending_fn = None;
            }
            _ => {}
        }
    }
    sites
}

/// `(path, items)` for each `### \`path\`` section of the inventory, in order.
pub fn parse_doc(doc: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    let mut header_seen = false;
    for (n, line) in doc.lines().enumerate() {
        if let Some(rest) = line.strip_prefix("### ") {
            let path = rest.trim().trim_matches('`');
            if sections.iter().any(|(p, _)| p == path) {
                return Err(format!("{DOC}:{}: {path} listed twice", n + 1));
            }
            sections.push((path.to_string(), Vec::new()));
            header_seen = false;
        } else if line.starts_with("## ") || line.starts_with("# ") {
            // Prose sections after the inventory end the last file's table.
            if !sections.is_empty() {
                sections.push((String::new(), Vec::new()));
            }
        } else if let Some(row) = line.strip_prefix('|') {
            let Some((_, items)) = sections.last_mut() else {
                continue;
            };
            let first = row.split('|').next().unwrap_or("").trim();
            if first.starts_with("---") || first.starts_with(":--") {
                continue;
            }
            if !header_seen {
                header_seen = true;
                continue;
            }
            items.push(first.trim_matches('`').to_string());
        }
    }
    sections.retain(|(p, _)| !p.is_empty());
    Ok(sections)
}

/// Every `.rs` file under `dir`, relative to `root`, sorted.
fn rust_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let ty = e
            .file_type()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if ty.is_dir() && e.file_name() != "target" {
            rust_files(root, &path, out)?;
        } else if ty.is_file() && path.extension().is_some_and(|x| x == "rs") {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Compares the source with the inventory; on success, a one-line summary.
pub fn audit(root: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    rust_files(root, &root.join("crates"), &mut files)?;
    let mut found: Vec<(String, Vec<Site>)> = Vec::new();
    for f in files {
        let src = fs::read_to_string(root.join(&f)).map_err(|e| format!("{f}: {e}"))?;
        let sites = scan(&src);
        if !sites.is_empty() {
            found.push((f, sites));
        }
    }
    let doc = fs::read_to_string(root.join(DOC)).map_err(|e| format!("{DOC}: {e}"))?;
    let listed = parse_doc(&doc)?;
    let problems = compare(&found, &listed);
    if !problems.is_empty() {
        return Err(format!(
            "{DOC} does not match the source:\n{}\nList every `unsafe` in source order, \
             one table row each, first cell the item named above; see the top of {DOC}.",
            problems.join("\n")
        ));
    }
    let total: usize = found.iter().map(|(_, s)| s.len()).sum();
    Ok(format!(
        "{total} `unsafe` in {} files, all listed in {DOC}",
        found.len()
    ))
}

fn compare(found: &[(String, Vec<Site>)], listed: &[(String, Vec<String>)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (file, sites) in found {
        let doc_items = listed
            .iter()
            .find(|(p, _)| p == file)
            .map(|(_, items)| items.as_slice());
        let src_items: Vec<&str> = sites.iter().map(|s| s.item.as_str()).collect();
        if doc_items.is_some_and(|d| d.iter().map(String::as_str).eq(src_items.iter().copied())) {
            continue;
        }
        let at: Vec<String> = sites
            .iter()
            .map(|s| format!("{file}:{} ({})", s.line, s.item))
            .collect();
        match doc_items {
            None => problems.push(format!("  {file}: not listed; unsafe at {}", at.join(", "))),
            Some(d) => problems.push(format!(
                "  {file}: listed [{}], source has {}",
                d.join(", "),
                at.join(", ")
            )),
        }
    }
    for (file, _) in listed {
        if !found.iter().any(|(f, _)| f == file) {
            problems.push(format!("  {file}: listed, but has no `unsafe`"));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(src: &str) -> Vec<(usize, String)> {
        scan(src).into_iter().map(|s| (s.line, s.item)).collect()
    }

    #[test]
    fn names_the_enclosing_function() {
        let src = concat!(
            "fn outer() {\n",
            "    let f = || unsafe { g() };\n",
            "    if x { unsafe { h() } }\n",
            "}\n",
            "fn next() {}\n",
            "fn arrays(t: &[u8; 2]) -> [i32; 2] {\n",
            "    unsafe { k() }\n",
            "}\n",
        );
        assert_eq!(
            items(src),
            vec![
                (2, "outer".to_string()),
                (3, "outer".to_string()),
                (7, "arrays".to_string()),
            ]
        );
    }

    #[test]
    fn names_other_forms() {
        let src = "unsafe extern \"C\" {\n    fn tzset();\n}\nunsafe fn raw() {}\nunsafe impl Send for X {}\n#[unsafe(no_mangle)]\nfn main() { unsafe { tzset() } }\n\
                   pub unsafe extern \"C\" fn entry(argc: i32) -> i32 { 0 }\n";
        assert_eq!(
            items(src),
            vec![
                (1, "extern".to_string()),
                (4, "raw".to_string()),
                (5, "impl".to_string()),
                (6, "attribute".to_string()),
                (7, "main".to_string()),
                (8, "entry".to_string()),
            ]
        );
    }

    #[test]
    fn skips_comments_strings_and_chars() {
        let src = concat!(
            "// unsafe { }\n",
            "/* unsafe /* nested unsafe */ unsafe */\n",
            "fn f<'a>(x: &'a str) {\n",
            "    let s = \"unsafe { \\\" }\";\n",
            "    let r = r#\"unsafe \"{\" \"#;\n",
            "    let b = b\"unsafe\";\n",
            "    let c = '{'; let q = '\\''; let e = b'}';\n",
            "    unsafe { x() }\n",
            "}\n",
        );
        assert_eq!(items(src), vec![(8, "f".to_string())]);
    }

    #[test]
    fn counts_lines_inside_multiline_literals() {
        let src = "fn f() {\n    let s = \"a\nb\";\n    /* x\n y */\n    unsafe {}\n}\n";
        assert_eq!(items(src), vec![(6, "f".to_string())]);
    }

    #[test]
    fn reads_the_inventory_tables() {
        let doc = "# Unsafe\n\n| Item | x |\n|---|---|\n| `ignored` | before any file |\n\n\
                   ## Inventory\n\n### `crates/a.rs`\n\n| Item | Operation |\n| --- | --- |\n\
                   | `f` | one |\n| `g` | two |\n\n### `crates/b.rs`\n\n| Item | Op |\n|---|---|\n\
                   | `extern` | decl |\n\n## Afterwards\n\n| Not | a file |\n|---|---|\n| `x` | y |\n";
        assert_eq!(
            parse_doc(doc).unwrap(),
            vec![
                (
                    "crates/a.rs".to_string(),
                    vec!["f".to_string(), "g".to_string()]
                ),
                ("crates/b.rs".to_string(), vec!["extern".to_string()]),
            ]
        );
    }

    #[test]
    fn reports_unlisted_extra_and_stale_entries() {
        let found = vec![
            (
                "crates/a.rs".to_string(),
                vec![Site {
                    line: 3,
                    item: "f".into(),
                }],
            ),
            (
                "crates/b.rs".to_string(),
                vec![
                    Site {
                        line: 1,
                        item: "g".into(),
                    },
                    Site {
                        line: 9,
                        item: "h".into(),
                    },
                ],
            ),
        ];
        let listed = vec![
            ("crates/b.rs".to_string(), vec!["g".to_string()]),
            ("crates/c.rs".to_string(), vec!["k".to_string()]),
        ];
        assert_eq!(
            compare(&found, &listed),
            vec![
                "  crates/a.rs: not listed; unsafe at crates/a.rs:3 (f)",
                "  crates/b.rs: listed [g], source has crates/b.rs:1 (g), crates/b.rs:9 (h)",
                "  crates/c.rs: listed, but has no `unsafe`",
            ]
        );
        assert!(compare(&found[..1], &[("crates/a.rs".into(), vec!["f".into()])]).is_empty());
    }

    #[test]
    fn the_workspace_matches_its_inventory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        audit(root).unwrap();
    }
}
