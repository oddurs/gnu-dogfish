---
id: 49
uid: 88c8af2e-30a2-4c36-9b8a-35c7f4b5ceeb
title: "sort: merge mode, buffer and parallel options, key validation"
type: bug
status: backlog
milestone: "v1.0"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: l
area: compat
---

## What happens

Missing: `-m/--merge` (with `--batch-size` and fd-limit handling), `-S/--buffer-size`,
`--compress-program`, `--parallel`, `-C`. Invalid keys (`-k1.1,-k0`, `-k 2.,3`)
are accepted; incompatibility checks are off (`-i` is listed among `-dfgMnR`;
`-c -o`, `-cC`, `-Co` are not diagnosed). `-g` compares as f64, so long-double
values (±3.36e-4932) collapse to ±0 (see 0018).

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/sort/sort-NaN-infloop.sh`
- `tests/sort/sort-compress.sh`
- `tests/sort/sort-continue.sh`
- `tests/sort/sort-merge-fdlimit.sh`
- `tests/sort/sort-merge.pl`
- `tests/sort/sort-unique-segv.sh`
- `tests/sort/sort.pl`
- `tests/sort/sort-float.sh`

## What should happen

All listed tests pass; `sort -m` merges presorted inputs without loading them.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/sort/sort-NaN-infloop.sh tests/sort/sort-compress.sh tests/sort/sort-continue.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
