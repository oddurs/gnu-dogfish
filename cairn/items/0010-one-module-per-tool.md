---
id: 10
uid: fa67a9d2-7aa0-4387-be9f-55d7c95ef66b
title: "One module per tool"
type: chore
status: planned
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

- [ ] No tool module exceeds ~1,500 lines; no tool's code lives in another tool's file.
- [ ] The move changes no behaviour: every test suite and the GNU pass count are identical before and after.
