---
id: 34
uid: 1aa76b63-d05a-4f52-888e-e500077e01f2
title: "Refuse to overwrite just-created destinations in cp and mv"
type: bug
status: backlog
milestone: "v1.0"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: m
area: safety
---

## What happens

`cp a/f b/f c` and `mv` with two sources of the same name overwrite the
first copy with the second and exit 0. GNU remembers what it created this run
and refuses: `will not overwrite just-created 'c/f' with 'b/f'`, `will not
copy 'b/1' through just-created symlink 'c/1'`. `mv dup-source` prints `b`
where GNU prints `./b`.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/cp/abuse.sh`
- `tests/mv/childproof.sh`
- `tests/mv/dup-source.sh`

## What should happen

cp and mv track the destinations created in this invocation and give GNU's
messages and exit status.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/cp/abuse.sh tests/mv/childproof.sh tests/mv/dup-source.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.
