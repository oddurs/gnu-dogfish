---
id: 63
uid: a56f426b-b3e1-4eb0-8176-bf5db4a53b46
title: "undo_model: one failure under load in the cp/mv/rm sequence test"
type: bug
status: planned
milestone: "v0.3"
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: s
area: testing
---

## What happens

`undo_model::undo_restores_any_sequence_of_cp_mv_rm` failed once while a full GNU baseline ran at the same time (randomised cp/mv undo across /dev/shm), then passed three times alone and in CI. The failing seed was not kept.

## What should happen

Either the failure reproduces from its seed and the bug is fixed, or the test's dependence on load (timestamps, /dev/shm contents, a shared trash) is removed.

## Reproduction

1. Run `cargo test --test undo_model` with `DOGFISH_PROPTEST_CASES=2000` while `util/gnu-test-one.sh` runs the full baseline; keep proptest's failure persistence file.

## Acceptance criteria

- [ ] The failing case is found and fixed, or shown to be load-only and the test made independent of it.
