---
id: 13
uid: 4844e1a7-374f-439c-8c60-5384a35a33a9
title: "Test cp, mv, rm and undo against a model, with crash injection"
type: feature
status: planned
milestone: "v0.3"
depends_on:
- 11
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: l
area: safety
---

## Problem

The undo journal changed shape three times during the quality sprint (JSON
array → append-only NDJSON, rename backups → copy backups, unbounded →
retention), each time because a hand-written test found a hole. The safety
guarantees are stated but not checked systematically.

## Proposal

Write the invariants down (undo never destroys data; undo never clobbers;
the trash is bounded; every journal entry is reversible or explicitly marked
otherwise) and check them with proptest: generate sequences of
`cp`/`mv`/`rm --safe`/`undo` on a small tree, run them, and compare against an
in-memory model. Add crash injection (kill the process at each syscall
boundary via a test hook) and check the journal recovers.

## Acceptance criteria

- [ ] The invariants are documented in SECURITY.md or a design note.
- [ ] Property tests and crash-injection tests run in CI.
- [ ] Every counterexample found is fixed and kept as a regression case.
