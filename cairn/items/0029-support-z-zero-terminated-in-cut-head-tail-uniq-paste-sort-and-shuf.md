---
id: 29
uid: 63ff42ae-af17-4add-b8ce-d50ffdfdb9b0
title: "Support -z/--zero-terminated in cut, head, tail, uniq, paste, sort and shuf"
type: bug
status: done
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
milestone: "v0.3"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: m
area: compat
---

## What happens

Seven line-oriented tools reject `-z` (`invalid option -- 'z'`). NUL-separated
records are how scripts pass arbitrary file names (`find -print0 | sort -z`).

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/cut/cut.pl`
- `tests/head/head.pl`
- `tests/tail/tail.pl`
- `tests/uniq/uniq.pl`
- `tests/misc/paste.pl`
- `tests/sort/sort.pl`
- `tests/sort/sort-debug-keys.sh`
- `tests/shuf/shuf.sh`

## What should happen

One record-separator abstraction in dogfish-core; every line-oriented tool
GNU gives `-z` accepts it and uses NUL for both input and output records.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/cut/cut.pl tests/head/head.pl tests/tail/tail.pl"`, as a non-root user.

## Acceptance criteria

- [x] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [x] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## Result

`-z/--zero-terminated` in cut, head, tail, uniq, paste, sort and shuf through one record-separator abstraction; the listed tests pass and are in the baseline.
