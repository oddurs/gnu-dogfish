---
id: 10
uid: fa67a9d2-7aa0-4387-be9f-55d7c95ef66b
title: "One module per tool"
type: chore
status: done
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
milestone: "v0.3"
depends_on:
- 9
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: l
area: architecture
---

## Purpose

`fs_ops.rs` passed 8,000 lines, `text.rs` and `sys.rs` are not far behind,
and parallel work on them meant merging into the same file. Review, ownership
and parallel work all want one module per utility.

## Approach

`crates/dogfish/src/tools/<tool>.rs` (or a directory per tool when it needs
helpers), each exporting `run(args, ctx)`; shared helpers move to dogfish-core
or a `common` module. Pure moves first, one commit per group, with the full
suite and the GNU test suite (0008) unchanged after each.

## Acceptance criteria

- [x] No tool module exceeds ~1,500 lines; no tool's code lives in another tool's file.
- [x] The move changes no behaviour: every test suite and the GNU pass count are identical before and after.

## Result

`crates/dogfish/src/tools/` holds one module per utility (ls as a directory:
options, lister, colors, rich, version_sort); `src/common/` holds what several
tools share (option parsing, quoting, modes, backups, the cp/mv destination
and `same_file_ok` logic, preserve-root, human sizes, strftime, ...). The
largest module is cp at 1,276 lines; no tool imports another. fs_ops.rs,
text.rs, sys.rs, hash.rs and meta.rs are gone. It was a pure move: all 654
top-level items exist exactly once, and the only code edits are added
`pub(crate)` and imports. Evidence: 1,936 tests before and after (and
`cargo xtask ci` green), 299 invocations across all 57 tools identical to the
previous binary, and all 417 GNU tests (util/gnu-test-one.sh) giving the same
result before and after. One integration test listed `src/` expecting three
files over 1K; it now lists `src/tools`.

## 2026-10-29

The move surfaced duplication for 0011 to fold together: three getopt engines
(`common/tool_args`, `common/gnu_getopt`, `common/getopt`) plus clap; two
strftime implementations (`common/strftime`, `common/date_format`); two
`filevercmp` (ls, sort); three date-string parsers (touch -d, date -d,
DateParser). Entry points keep their `run_<tool>` names.
