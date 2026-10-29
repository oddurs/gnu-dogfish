---
id: 59
uid: f16684ce-763f-41f4-b4ca-83630a77c87c
title: "Non-UTF-8 file names in the read-only tools"
type: bug
status: planned
milestone: "v0.3"
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: m
area: compat
---

## What happens

cat, head, tail, wc, ls, stat, du, sort and about 17 other tools turn an operand that is not valid UTF-8 into U+FFFD characters and then report the file as missing. cp, mv, ln, mkdir, mktemp and rm keep raw bytes (0011).

## What should happen

Every tool keeps operands as OsStr/bytes end to end, opens the file named, and prints the name as GNU would (quoted per the tool's quoting rules).

## Reproduction

1. `f=$(printf 'a\377b'); echo x > "$f"; dogfish cat "$f"` reports the file missing; GNU prints x.

## Acceptance criteria

- [ ] Every tool that takes file operands handles a name containing byte 0xff; differential cases cover each.
