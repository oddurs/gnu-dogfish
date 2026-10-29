---
id: 50
uid: 44184790-6ed5-493d-b8c2-ae557683924e
title: "nl: section delimiters, -p, -d and number range checks"
type: bug
status: done
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
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

Missing `-p/--no-renumber` and `-d/--section-delimiter`; `\:\:` sections are not
handled; `-v INTMAX_MAX` wraps negative instead of `line number overflow`;
`-v`/`-i` range checks are wrong; bad options print clap's help (0011).

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/misc/nl.sh`

## What should happen

The test passes.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/misc/nl.sh"`, as a non-root user.

## Acceptance criteria

- [x] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [x] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## Result

`-h`, `-f`, `-d/--section-delimiter`, `-l`, `-p/--no-renumber`, `\:` sections and GNU's range checks; misc/nl.sh passes.
