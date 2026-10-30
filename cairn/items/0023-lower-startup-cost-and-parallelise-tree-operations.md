---
id: 23
uid: a170b804-154e-438f-bffc-0b865edb4b05
title: "Lower startup cost and parallelise tree operations"
type: chore
status: doing
milestone: "v1.0"
assignee: claude
claimed: 2026-10-30
created: 2026-10-29
updated: 2026-10-30
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
- [x] `cp -r` and `du` on a large tree are measurably faster than GNU with identical output.

## Notes

- 2026-10-30: `cp -r 30k files` was at parity (REPEAT=9: 1.00x wall, 0.99x CPU; du -s 1.84x wall, 0.98x CPU). strace showed both programs make the same calls (Dogfish 90k statx against GNU's 60k fstat + 30k newfstatat). 80% of the time goes to the destination `openat(O_CREAT)`, at 350-450 us each: the bench removes the previous copy just before each run, and ext4 without a journal scans past recently deleted inodes (`recently_deleted()`) on every allocation for ~30 s. No syscall saving could reach 10%. A C test (300 dirs x 100 files) showed creates in different directories run in parallel (4 threads: 6.5 s vs 8.4-12.4 s serial), and in one directory they contend (15.7 s).
- 2026-10-30: cp now copies regular files going into directories the run created on worker threads, one directory's files per batch (`Parallel` in common/copy.rs). The walk, every stat, mkdir and decision stays on the main thread in GNU's order. Workers run the unchanged `copy_reg` with their diagnostics captured. The walk waits for them (and writes their messages, in walk order) before it reports anything itself, before restoring a directory's mode (read-only source dirs), and at the end of each operand, so exit status and messages are GNU's. Off for -v, -J, --debug, -i, -u, --backup, -l, -s, -p/-a/--preserve, -L, mv, and under DOGFISH_CRASH_AT. Undo is unaffected: its per-operand entry is committed after the drain.
- 2026-10-30: after (REPEAT=9, 4-core VM): cp -r 30k files 2.37x wall, 1.15x CPU; du -s 2.02x wall, 1.13x CPU; du -ah 1.82x / 1.03x. Worker count is the CPU trade: 3 workers gave 2.8-3.3x wall but 0.90-1.12x CPU, 4 workers 2.86x wall / 0.75x CPU, so it is capped at 2. On tmpfs, where creates are cheap, cp -r is 1.65x wall but 0.70x CPU. Checked: byte-identical stderr/exit status against GNU on /proc/1 (452 messages), /sys/bus, /sys/devices/system; new test `cp_recursive_parallel_messages_in_gnu_order` (as nobody: unreadable files and dirs across 40 dirs); GNU tests/cp: 48 PASS, 1 SKIP.
