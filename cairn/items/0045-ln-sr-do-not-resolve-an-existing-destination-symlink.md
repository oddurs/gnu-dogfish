---
id: 45
uid: e67cd335-1d77-4c55-82a2-cd5eb5766d1c
title: "ln -sr: do not resolve an existing destination symlink"
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

`ln -srf here existing_link` canonicalizes the destination's last component,
giving `../../here` instead of `here`.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/ln/relative.sh`

## What should happen

Only the destination's directory is canonicalized; the test passes.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/ln/relative.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
