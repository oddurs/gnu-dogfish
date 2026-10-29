---
id: 46
uid: 4e2d6f2e-fb74-4589-a1ad-390812071a8a
title: "ls: remaining GNU test failures (--dired, hyperlinks, LS_COLORS *.ext, ...)"
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

- `--dired` is not implemented (three tests).
- `LS_COLORS='*.foo=...'` extension entries are ignored, and no `\e[K` follows a coloured name that wraps.
- `--hyperlink` emits no OSC 8 `file://` links.
- `ls -l --color` calls getxattr per file; GNU caches ENOTSUP per file system.
- ls stats every entry even when the active colours need only `d_type`.
- `-T0x10` / `-w010` need base-0 number parsing.
- `--quoting-style=shell` quotes names with control characters (`'q'`); GNU prints `q\a` (or `q?` with `-q`).
- `-w4 -x -T0 a b` wraps `a  b`, which fits exactly (off-by-one).

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/ls/dired.sh`
- `tests/ls/stat-failed.sh`
- `tests/ls/zero-option.sh`
- `tests/ls/color-clear-to-eol.sh`
- `tests/ls/quote-align.sh`
- `tests/ls/hyperlink.sh`
- `tests/ls/getxattr-speedup.sh`
- `tests/ls/stat-free-color.sh`
- `tests/ls/hex-option.sh`
- `tests/ls/ls-misc.pl`
- `tests/ls/w-option.sh`

## What should happen

All listed tests pass; ROADMAP.md loses its `--dired` and `--hyperlink` lines.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/ls/dired.sh tests/ls/stat-failed.sh tests/ls/zero-option.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
