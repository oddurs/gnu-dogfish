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
updated: 2026-10-30
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
- [x] No optimisation changes output: `cargo xtask ci` and the GNU test suite pass unchanged.
- [x] The README's performance section quotes these numbers and how to reproduce them.

## Notes

- 2026-10-30: sha256sum now runs its own assembly loop shaped like OpenSSL's AVX2 one (16- and 8-round loops; the fully unrolled Rust version was ~3,500 instructions and lost 7%), with an AVX-512VL schedule where present: 0.89-0.93x -> 1.05-1.10x. Hashing alone is level with OpenSSL (hashlib: 678 ms, Dogfish 685 ms for 256 MiB in memory); the margin is mmap instead of 8,000 reads.
- 2026-10-30: startup. libgcc_s.so.1 was back in DT_NEEDED: Cargo gives build.rs's rustc-link-lib only to the library target added for fuzzing, so the binary lost the static unwinder; fixed with `extern crate` and a test. `true` 428k -> 402k instructions, 87 -> 81 page faults, 0.78-0.87x -> 0.89-0.94x. What remains was measured with stand-in binaries (minimal Rust `main` = GNU's speed): ~2,900 R_X86_64_RELATIVE relocations (+38k instructions, +13 page faults: the 16 dirtied .data.rel.ro pages) cost ~3%, and ~135 more imports than GNU's 45 (+90k instructions in do_lookup_x etc.) ~2.5%; Dogfish's own startup code is negligible. Of the relocations, ~700 are panic Locations, ~370 fmt pieces, ~600 vtables/fn pointers, ~550 long-option names, 330 ALL_TOOLS; of the imports ~95 come from std. `-z pack-relative-relocs` was tried again: no gain on glibc 2.39 (its RELR loop costs as many instructions for this dense table; 77 vs 81 faults) and it would need glibc 2.36. Getting to parity would take relocation-free option tables and raw syscalls for ~40 trivial libc wrappers, each worth well under 1%.
- 2026-10-30: `ls -1 DIR` stat'ed DIR twice (the -R loop check ran always) and `ls -F` stat'ed symlink targets GNU does not; both fixed. chmod -R is kernel-bound and at parity (Dogfish executes 16M user instructions to GNU's 26M; same syscalls).
- 2026-10-30: quiet REPEAT=7 run on main 05c4d7d: 39/47 at least as fast as GNU in wall and CPU. Below 1.0: startup true/basename/cat x200 (0.91/0.85/0.98 wall), wc -c and -m (0.95, startup-bound 2 ms runs), ls -1 300 dirs (0.99 wall, 1.05 CPU), chmod -R (0.98), du -s (1.74 wall, 0.93 CPU: parallel walk). README's benchmark section now quotes this run and how to reproduce it. Criteria 1-2 stay open on the startup gap (loader work: ~135 extra imports, ~2,900 relocations; see the note above).
