---
id: 57
uid: 46ce2cd4-2f8d-4475-995a-47b6891341b7
title: "Faster than GNU coreutils on every benchmark"
type: feature
status: doing
milestone: "v0.3"
assignee: claude
claimed: 2026-10-29
depends_on:
- 10
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: xl
area: performance
---

## Problem

"High-velocity" is a claim until it is measured against the reference on the
same machine and inputs. The earlier `dogfish bench` covered a handful of
cases and once compared against BSD tools; `seq` silently got 2x slower in
a rewrite (decision 0024).

## Proposal

`util/bench.py` runs the same command lines with GNU coreutils 9.4 and with
Dogfish (installed as a symlink named like the tool), alternating, on
generated inputs: per-invocation startup, streaming text (cat, wc, head,
tail, tac, tr, cut, uniq, nl, paste, sort, shuf, seq), checksums and
encodings, and file trees (cp, rm, ls, du, chmod, stat). It reports median
wall and CPU time (user + system) and the GNU/Dogfish ratio, with `LC_ALL=C`
on both sides. Optimise every benchmark where Dogfish loses on either
measure, without giving up GNU-exact output; then track the numbers in CI
(0022) so they cannot slip back.

## Acceptance criteria

- [ ] On Linux x86-64, every benchmark in `util/bench.py` has a wall-time and CPU-time ratio of at least 1.0 (Dogfish at least as fast as GNU), on a quiet machine with REPEAT=7.
- [ ] Startup (`true`, `basename`, `cat` on a small file, 200 calls) is no slower than GNU's.
- [ ] No optimisation changes output: `cargo xtask ci` and the GNU test suite pass unchanged.
- [ ] The README's performance section quotes these numbers and how to reproduce them.
