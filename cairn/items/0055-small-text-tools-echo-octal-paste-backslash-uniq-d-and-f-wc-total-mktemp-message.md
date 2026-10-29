---
id: 55
uid: 0b769c4a-cd56-495c-a5c7-aba9c859be03
title: "Small text tools: echo octal, paste backslash, uniq -D and -f, wc --total, mktemp messages"
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
effort: s
area: compat
---

## What happens

- echo: `\NNN` without a leading 0 (`\33`) is not interpreted.
- paste: `-d'\'` is accepted; GNU says `delimiter list ends with an unescaped backslash`.
- uniq: bare `-D/--all-repeated` is missing; `-f 10000000000` iterates the count (10 s+).
- wc: `--total=auto|always|only|never` is missing.
- mktemp: `-q` suppresses `too few X's in template`; with `--suffix` the error quotes the suffix.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/misc/echo.sh`
- `tests/misc/paste.pl`
- `tests/uniq/uniq.pl`
- `tests/uniq/uniq-perf.sh`
- `tests/wc/wc-total.sh`
- `tests/misc/mktemp.pl`

## What should happen

The listed tests pass once 0029 and 0031 (their other causes) are done.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/misc/echo.sh tests/misc/paste.pl tests/uniq/uniq.pl"`, as a non-root user.

## Acceptance criteria

- [x] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [x] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## Result

echo `\NNN` octal, paste's unescaped-backslash error, uniq `-D`/`--all-repeated[=METHOD]`/`--group` and `-f HUGE` without iterating, wc `--total=WHEN`, mktemp `-q`/`--suffix` messages; the listed tests pass.
