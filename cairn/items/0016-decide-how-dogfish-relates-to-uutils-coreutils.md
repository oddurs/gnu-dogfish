---
id: 16
uid: 68555ce4-9e7a-4ea5-8852-3e471012af30
title: "Decide how Dogfish relates to uutils/coreutils"
type: decision
status: backlog
milestone: "v1.0"
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: m
area: direction
---

## Context

uutils/coreutils is a Rust implementation of coreutils that publishes its GNU
test-suite pass rate and has begun shipping as a distribution default. "Coreutils
in Rust" alone does not distinguish Dogfish. What does: rich terminal output,
NDJSON pipelines and `query`, undo and the trash, and byte-exact GNU behaviour
in pipes.

## Options and tradeoffs

- Compete: keep a full independent implementation; differentiate on the features above.
- Converge: upstream the GNU-compat fixes to uutils, build Dogfish's features as a layer over it.
- Hybrid: share crates (parsers, quoting) without merging projects.

Compare against 0008's pass rate and uutils' published one before deciding.

## Decision

## Revisit when

The GNU test-suite pass rate is known (0008).

## Acceptance criteria

- [ ] The choice, its evidence, and its consequences are recorded.
