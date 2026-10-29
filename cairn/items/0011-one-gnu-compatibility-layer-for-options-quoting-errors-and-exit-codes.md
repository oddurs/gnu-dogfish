---
id: 11
uid: a295696f-a7d8-480c-8926-bca8a42175a5
title: "One GNU compatibility layer for options, quoting, errors and exit codes"
type: chore
status: done
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
milestone: "v0.3"
depends_on:
- 10
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: xl
area: architecture
---

## Purpose

Tools now parse options four ways (clap, a getopt-style parser in `fs_ops.rs`,
another in `sys.rs`, and chmod's mode-aware pre-pass) and quote file names in
diagnostics two ways (`gnu_quote` in dogfish-core and the quoting styles in
`fs_ops.rs`). Each fix lands in one copy only. Remaining gaps that are really
about this layer: ambiguous long-option abbreviations (`cat --s` should list
the possibilities), `--vers` as `--version`, per-tool exit statuses.

## Approach

In dogfish-core:

- `gnu::getopt` — getopt_long semantics: permutation (or `+` to stop at the first
  operand), clustered short options, optional and required arguments, unique
  prefix matching with GNU's ambiguity message, `--`.
- `gnu::quote` — every quoting style ls supports, used for diagnostics too (locale-aware later, 0017).
- `gnu::error` — `tool: message` formatting, the `Try 'tool --help'` line, per-tool usage exit status.
- `gnu::args` — argmatch (`--color=WHEN`, `--sort=WORD`), size suffixes, block sizes, durations.

Migrate tool by tool; delete the old parsers; keep clap only if it still earns
its place (e.g. for `--help` text), otherwise remove it.

## Acceptance criteria

- [x] Exactly one option parser and one quoting implementation remain.
- [x] `cat --s` and `sort --vers` print GNU's ambiguity message; `cat --vers` prints the version.
- [x] GNU test-suite pass count does not drop; differential suites unchanged.
- [x] The parser-level GNU test failures found by 0008 are gone: unique-prefix
  long options (`cut --out=`), the last of a repeated option wins (`sort -o`
  twice, `shuf -n` twice), `--help`/`--version` among other operands
  (`tests/help/help-version-getopt.sh`, `tests/misc/nl.sh`), and non-UTF-8
  arguments (`cut -d $'\255'`, `date -d $'\xb0'`; today `std::env::args`
  aborts with exit 134). Tests: `tests/help/help-version-getopt.sh`, plus the
  parser cases inside `tests/cut/cut.pl`, `tests/sort/sort.pl`,
  `tests/shuf/shuf.sh`, `tests/misc/nl.sh`, `tests/date/date.pl`.

## Result

`common/getopt.rs` is a port of glibc 2.39 `getopt_long` (permutation, `+`/POSIXLY_CORRECT, clustering, optional arguments, unique-prefix abbreviation with glibc's ambiguity message, `--`); every tool's option table mirrors GNU 9.4's `long_options`; `common/tool_args`, `gnu_getopt`, the old `getopt` and clap are gone. Arguments are OsString end to end. Dogfish-only switches (`--stats`, `--color`, `--no-color`, `-J`) are declared per tool, exact-match only, and never given to echo, printf, true, false or yes. GNU tests now passing and added to the baseline: help/help-version-getopt, misc/false-status, misc/close-stdout, misc/yes, misc/write-errors, head/head-write-error, rm/dangling-symlink, rm/dir-no-w, rm/empty-inacc, rm/interactive-once, rm/isatty. About 60 new differential cases (abbreviations, ambiguity, --help anywhere, POSIXLY_CORRECT, /dev/full, per-tool switches, 0xff names) and 25 parser unit tests; `cargo xtask ci` passes (6,453 tests).
