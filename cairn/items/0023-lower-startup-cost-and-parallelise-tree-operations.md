---
id: 23
uid: a170b804-154e-438f-bffc-0b865edb4b05
title: "Lower startup cost and parallelise tree operations"
type: chore
status: backlog
milestone: "v1.0"
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: l
area: performance
---

## Problem

Every invocation pays multicall dispatch and initialisation (~2 ms, like GNU,
but nothing to spare for tiny files), and `du`/`cp -r`/`rm -r`/`chmod -R` walk
trees mostly serially.

## Proposal

Profile startup (lazy statics, avoiding eager locale/terminal probes); a
parallel tree walker shared by the recursive tools that preserves GNU's output
order where order is observable.

## Acceptance criteria

- [ ] `dogfish true` is no slower than GNU `true`.
- [ ] `cp -r` and `du` on a large tree are measurably faster than GNU with identical output.
