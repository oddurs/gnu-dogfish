---
id: 42
uid: 1bc74bae-3117-4f0c-8556-f13d5b1ecbb2
title: "cp: remaining GNU test failures"
type: bug
status: backlog
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

- `cp -in` prints 9.5's `-n is non-portable` warning; 9.4 says `not replacing 'd'` and exits 1.
- `--parents /bin/ls d` strips the `/` and stats `bin` relative to the cwd.
- EACCES while stat-ing through a symlink is reported as a dangling symlink.
- `cp --link -H symlink dst` links the symlink; under `-H` GNU links its target.
- `--preserve=links` with `-H`/`-L` does not link a file and a symlink that resolve to one inode.
- `cp -p --parents` does not re-apply modes to existing intermediate directories.
- `cp -au` breaks hard-link structure when a newer destination is skipped.
- `--reflink=always` falls back to a copy instead of failing when unsupported.
- `--sparse=always` writes a non-sparse file; `--reflink` with `--sparse=always|never` is not rejected.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/cp/cp-i.sh`
- `tests/cp/cp-parents.sh`
- `tests/cp/fail-perm.sh`
- `tests/cp/link-deref.sh`
- `tests/cp/link-preserve.sh`
- `tests/cp/parent-perm.sh`
- `tests/cp/preserve-link.sh`
- `tests/cp/reflink-auto.sh`
- `tests/cp/sparse.sh`

## What should happen

All listed tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/cp/cp-i.sh tests/cp/cp-parents.sh tests/cp/fail-perm.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
