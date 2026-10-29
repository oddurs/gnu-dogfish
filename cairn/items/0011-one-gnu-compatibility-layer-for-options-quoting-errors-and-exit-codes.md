---
id: 11
uid: a295696f-a7d8-480c-8926-bca8a42175a5
title: "One GNU compatibility layer for options, quoting, errors and exit codes"
type: chore
status: planned
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

- [ ] Exactly one option parser and one quoting implementation remain.
- [ ] `cat --s` and `sort --vers` print GNU's ambiguity message; `cat --vers` prints the version.
- [ ] GNU test-suite pass count does not drop; differential suites unchanged.
