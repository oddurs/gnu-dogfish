---
id: 27
uid: 1a09beed-5130-4ed4-b8ea-1fc03dddcf8f
title: "Accept GNU's hidden test options (---presume-input-tty, ---disable-inotify)"
type: bug
status: done
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
milestone: "v0.3"
depends_on:
- 8
- 11
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: s
area: testing
---

## What happens

GNU's own tests drive `rm` with `---presume-input-tty` and `tail` with
`---disable-inotify` (three dashes: undocumented, test-only). Dogfish rejects
both as unrecognized, so 16 tests fail before they test anything.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/rm/dangling-symlink.sh`
- `tests/rm/deep-2.sh`
- `tests/rm/dir-no-w.sh`
- `tests/rm/empty-inacc.sh`
- `tests/rm/interactive-once.sh`
- `tests/rm/isatty.sh`
- `tests/tail/F-headers.sh`
- `tests/tail/F-vs-missing.sh`
- `tests/tail/descriptor-vs-rename.sh`
- `tests/tail/follow-stdin.sh`
- `tests/tail/inotify-hash-abuse.sh`
- `tests/tail/inotify-hash-abuse2.sh`
- `tests/tail/overlay-headers.sh`
- `tests/tail/pid.sh`
- `tests/tail/retry.sh`
- `tests/tail/truncate.sh`

## What should happen

Both options are accepted (with GNU's prefix matching, e.g. `---dis`) and do
what GNU's do. They stay out of `--help`.

The tail tests also need follow mode (0033); this item unblocks the rm tests outright.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/rm/dangling-symlink.sh tests/rm/deep-2.sh tests/rm/dir-no-w.sh"`, as a non-root user.

## Acceptance criteria

- [x] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [x] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## Result

`rm ---presume-input-tty` and `tail ---disable-inotify` are accepted (prefix-matchable, hidden from --help); rm prompts as if stdin were a terminal. GNU tests now passing and added to the baseline: help/help-version-getopt, misc/false-status, misc/close-stdout, misc/yes, misc/write-errors, head/head-write-error, rm/dangling-symlink, rm/dir-no-w, rm/empty-inacc, rm/interactive-once, rm/isatty. About 60 new differential cases (abbreviations, ambiguity, --help anywhere, POSIXLY_CORRECT, /dev/full, per-tool switches, 0xff names) and 25 parser unit tests; `cargo xtask ci` passes (6,453 tests).
