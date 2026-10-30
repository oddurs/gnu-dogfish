---
id: 60
uid: 40720315-06b2-4371-8662-7e32259d6130
title: "--no-color and ls --color=WHEN: the last one on the command line wins"
type: bug
status: done
milestone: "v0.3"
created: 2026-10-29
updated: 2026-10-30
closed_at: 2026-10-30
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

- [x] `ls --color=always --no-color` and `ls --no-color --color=always` behave as their last option says.

## Notes

- 2026-10-30: ls's parser now records `--color=WHEN` in the context's switches when it meets it, where the getopt loop records `--no-color` in the same command-line order, and turns GNU's colouring off when the final mode is never. Tested on a pipe and on a pty (`script`) with both orders and the bare `--color` (`test_ls_color_and_no_color_last_wins` in integration.rs). The other tools' `--color` is a Dogfish switch parsed in the same loop, so it already behaved.
