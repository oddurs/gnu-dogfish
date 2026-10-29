---
id: 41
uid: 485efbbc-1b18-406d-8e93-ea4a5ab44768
title: "chown/chgrp -RLh --preserve-root misses a symlink to /"
type: bug
status: done
assignee: claude
claimed: 2026-10-29
closed_at: 2026-10-29
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

- [x] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [x] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## Result

Worse than filed: with `-RH`/`-RHh` on a command-line symlink to `/`, chown walked all of `/`, and `chmod -R --preserve-root` did the same through a command-line symlink (it checked only the link). chown and chmod now compare every directory the walk would enter with `/`'s device and inode, using the same stat the walk does (`-L` every link, `-H` command-line links only; `-h` no longer turns traversal off), as GNU's ROOT_DEV_INO_CHECK does. The predicate `is_root_dev_ino` (common/preserve_root.rs) is unit-tested by walking a temporary directory standing in for `/`; nothing touches the real root. tests/chown/preserve-root.sh passes and is in the baseline.
