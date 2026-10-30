---
id: 63
uid: a56f426b-b3e1-4eb0-8176-bf5db4a53b46
title: "undo_model: one failure under load in the cp/mv/rm sequence test"
type: bug
status: done
milestone: "v0.3"
created: 2026-10-29
updated: 2026-10-30
closed_at: 2026-10-30
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

- [x] The failing case is found and fixed, or shown to be load-only and the test made independent of it.

## Notes

- 2026-10-30: Not load: a real bug, deterministic once found. `cp` writes into an existing `<shm>/a` hard-linked as `<shm>/b`; `mv <shm>/a a` copies across file systems and unlinks it. Undo of the move copied it back as a new inode, so undoing the `cp` restored only that one and `<shm>/b` kept the copied contents (both directions fail). The test's link exemption for cross-device moves hid the split link but not the content.
- Fix: before unlinking a hard-linked non-directory, mv keeps a link to it in the trash on its file system, named in the journal entry first (`src_link`); undo renames it back unless the copy or the original changed since (then it copies back as before, keeping both versions). Directories moved across file systems still lose outside links (documented in SECURITY.md); the model test now exempts only those, checking that nothing is lost instead.
- The test now saves failing seeds in `crates/dogfish/tests/undo_model.proptest-regressions` and replays them; proptest prints the `cc` line to add from a CI log. A 5000-case run found a second bug (backups through a symlinked directory: journal and un-backup used the path as spelled), fixed on the same branch.
- Runs: sequence test 5000 cases release and 5000 debug, crash test 2000 cases, all passing.
