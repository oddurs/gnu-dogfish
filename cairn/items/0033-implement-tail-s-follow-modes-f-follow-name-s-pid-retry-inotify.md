---
id: 33
uid: f40e8107-12cc-416b-9773-755f8f5229bc
title: "Implement tail's follow modes (-F, --follow=name, -s, --pid, --retry, inotify)"
type: bug
status: planned
milestone: "v0.3"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: xl
area: compat
---

## What happens

`tail` supports only `-n -c -f -q -v`. Missing: `-F`, `--follow=name|descriptor`,
`-s/--sleep-interval`, `--pid`, `--retry`, `--max-unchanged-stats`, inotify
with the fallback to polling, and GNU's follow diagnostics (`has become
inaccessible`, `has been replaced; following new file`, `no files remaining`).
Log rotation (`tail -F` on a log) is the main real-world use.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/tail/F-headers.sh`
- `tests/tail/F-vs-missing.sh`
- `tests/tail/F-vs-rename.sh`
- `tests/tail/assert-2.sh`
- `tests/tail/assert.sh`
- `tests/tail/descriptor-vs-rename.sh`
- `tests/tail/flush-initial.sh`
- `tests/tail/follow-name.sh`
- `tests/tail/follow-stdin.sh`
- `tests/tail/inotify-dir-recreate.sh`
- `tests/tail/inotify-hash-abuse.sh`
- `tests/tail/inotify-hash-abuse2.sh`
- `tests/tail/inotify-rotate-resources.sh`
- `tests/tail/inotify-rotate.sh`
- `tests/tail/overlay-headers.sh`
- `tests/tail/pid.sh`
- `tests/tail/pipe-f2.sh`
- `tests/tail/retry.sh`
- `tests/tail/symlink.sh`
- `tests/tail/tail-n0f.sh`
- `tests/tail/truncate.sh`
- `tests/tail/wait.sh`

## What should happen

Follow by name and by descriptor behave as GNU's, with inotify on Linux and
polling elsewhere (kqueue on macOS is optional). All 22 tests pass.

tail.pl is tracked by 0028 (obsolete syntax) and 0029 (`-z`).

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/tail/F-headers.sh tests/tail/F-vs-missing.sh tests/tail/F-vs-rename.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
