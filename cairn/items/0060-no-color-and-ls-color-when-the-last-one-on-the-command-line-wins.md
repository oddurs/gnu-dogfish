---
id: 60
uid: 40720315-06b2-4371-8662-7e32259d6130
title: "--no-color and ls --color=WHEN: the last one on the command line wins"
type: bug
status: planned
milestone: "v0.3"
created: 2026-10-29
updated: 2026-10-29
priority: p3
effort: s
area: compat
---

## What happens

With both `--no-color` (Dogfish switch) and ls's `--color=WHEN` (GNU option) on one command line, whichever is applied after parsing wins, not whichever came last.

## What should happen

The later option wins, as with GNU's own repeated options.

## Reproduction

1. `ls --color=always --no-color` colours on a terminal.

## Acceptance criteria

- [ ] `ls --color=always --no-color` and `ls --no-color --color=always` behave as their last option says.
