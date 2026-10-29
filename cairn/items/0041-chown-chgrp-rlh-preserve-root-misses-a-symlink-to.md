---
id: 41
uid: 485efbbc-1b18-406d-8e93-ea4a5ab44768
title: "chown/chgrp -RLh --preserve-root misses a symlink to /"
type: bug
status: planned
milestone: "v0.3"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: s
area: safety
---

## What happens

`chown -RLh --preserve-root` does not notice a symlink to `/` inside the tree,
and `-h` after `-L` wrongly turns the traversal check off. `--preserve-root`
exists to stop exactly this.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/chown/preserve-root.sh`

## What should happen

The root check applies to every directory the walk would enter, whatever
the order of `-H/-L/-P/-h`. Tested through a pure predicate, never against
`/` (AGENTS.md).

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/chown/preserve-root.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
