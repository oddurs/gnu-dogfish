---
id: 30
uid: ee4d9658-4eca-4b38-9751-5d5805e33a9e
title: "Support --files0-from in du, wc and sort"
type: bug
status: doing
assignee: claude
claimed: 2026-10-29
milestone: "v1.0"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: m
area: compat
---

## What happens

`--files0-from=F` (read NUL-separated operands from F, or stdin for `-`) is
missing from all three tools.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/du/files0-from-dir.sh`
- `tests/du/files0-from.pl`
- `tests/sort/sort-files0-from.pl`
- `tests/wc/wc-files0-from.pl`
- `tests/wc/wc-files0.sh`

## What should happen

One shared reader with GNU's diagnostics (`file operands cannot be combined
with --files0-from`, `invalid zero-length file name`, `when reading file
names from stdin, no file name of '-' allowed`, ...).

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/du/files0-from-dir.sh tests/du/files0-from.pl tests/sort/sort-files0-from.pl"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## 2026-10-29

`--files0-from` for wc and sort (common/files0.rs, GNU's diagnostics); wc-files0-from.pl, wc-files0.sh and sort-files0-from.pl pass. du remains (the ls/du/rm work).
