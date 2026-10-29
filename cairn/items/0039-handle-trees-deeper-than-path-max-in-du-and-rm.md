---
id: 39
uid: ef57c427-0361-4fb4-bfc7-6598c4ade8f2
title: "Handle trees deeper than PATH_MAX in du and rm"
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

du and rm build full path names, so a tree deeper than PATH_MAX fails
(`du: cannot read directory '<long>'`, `rm: cannot remove '<long>'`). GNU
walks with `fts` and `openat`, relative to directory descriptors.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/du/long-from-unreadable.sh`
- `tests/rm/deep-2.sh`

## What should happen

Traversal works by descriptor (`openat`/`unlinkat`/`fstatat`), with no limit
on depth; both tests pass. Shares the walker with 0023's parallel traversal.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/du/long-from-unreadable.sh tests/rm/deep-2.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
