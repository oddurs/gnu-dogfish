---
id: 56
uid: 7156fd1c-5356-4ddf-bac0-84f247233133
title: "System tools: env -S hint, id -Gz, sleep suffixes, stat -f -, tee, timeout, touch -"
type: bug
status: backlog
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

- env: no `env: use -[v]S to pass options in shebang lines` hint after an error involving a space.
- id: `-Gz` separates with spaces, and multi-user output is not NUL-terminated.
- sleep: `42day` is accepted; only single-letter suffixes are valid.
- stat: `stat -f -` prints file-system info; GNU refuses `-` there.
- tee: on a non-blocking stdout, fails with EAGAIN; GNU 9.4 waits until writable.
- timeout: with `trap '' CHLD` inherited, fails with `No child processes` (GNU resets SIGCHLD); `-v -s0` names the signal `0`, GNU `EXIT`.
- touch: `touch -` creates a file named `-`; GNU touches stdout. `touch /` reports the open error, not `setting times of '/'`.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/env/env-S.pl`
- `tests/id/zero.sh`
- `tests/misc/sleep.sh`
- `tests/stat/stat-hyphen.sh`
- `tests/misc/tee.sh`
- `tests/timeout/timeout.sh`
- `tests/touch/empty-file.sh`
- `tests/touch/read-only.sh`
- `tests/touch/not-owner.sh`

## What should happen

All listed tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/env/env-S.pl tests/id/zero.sh tests/misc/sleep.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
