---
id: 15
uid: da50d0db-cc2e-40ee-ad31-e5089b393e0d
title: "Upgrade the RustCrypto crates together"
type: chore
status: doing
assignee: claude
claimed: 2026-10-30
milestone: "v0.3"
depends_on:
- 4
created: 2026-10-29
updated: 2026-10-30
priority: p2
effort: s
area: release
---

## Purpose

digest, md-5 and blake2 0.11 must move as one: hash.rs implements digest's
traits for the `ring` SHA-256 adapter and uses `Blake2bVar`/`VariableOutput`.
Dependabot now groups them (#11).

## Approach

Port `hash.rs` to the 0.11 APIs in one change; check `b2sum -l`, the md5/sha
`-c` paths and the bench numbers.

## Acceptance criteria

- [ ] The grouped Dependabot PR (or a manual one) merges with the full suite green.

## 2026-10-29

Dependabot's grouped PR #13 (digest 0.11.3, blake2 0.11.0) does not compile:
blake2 0.11 removed `Blake2bVar` (b2sum `-l` needs runtime-length BLAKE2b),
md-5 0.11 removed the `asm` feature (so it could not join the group), and
the ring SHA-256 adapter implements digest 0.10's traits. Closed #13 and
added a Dependabot `ignore` for the four crates' 0.11+ in
`.github/dependabot.yml`; remove it as part of this item. Bench md-5 without
`asm` before switching.

## 2026-10-30

Ported on branch `rustcrypto-0.11`: digest 0.11.3, blake2 0.11.0, md-5
0.11.0 (sha2 is not a dependency; ring and the AVX2 code do SHA-256), also in
`fuzz/Cargo.lock`. The code is `crates/dogfish/src/tools/checksum.rs` (the
notes above call it `hash.rs`). The ring adapter compiles unchanged against
0.11's `HashMarker`/`OutputSizeUser`/`Update`/`FixedOutput`. With
`Blake2bVar` and `VariableOutput` gone, `Blake2bRt` drives blake2's public
`Blake2bVarCore` (`VariableOutputCore::new(len)` puts the length in the
parameter block) behind a lazy `block_api::Buffer` and truncates on the
left, as the old wrapper did; unit tests check it against `Blake2b512/256/128`
around the 128-byte block boundary and against RFC 7693's "abc" vector.
`b2sum -l 8..512` (with and without `--tag`), `-c`, md5sum/sha256sum `--tag`
and `-c`, and `sort -R --random-source` match GNU byte for byte. GNU tests
pass: tests/cksum/{b2sum,md5sum-bsd,md5sum-newline,md5sum-parallel,md5sum,
sha256sum,cksum,cksum-a,cksum-c}, tests/sort/sort-rand.sh. Dependabot's
`ignore` for the four crates is gone.

md5sum regresses: `util/bench.py -k md5` (256 MB, GNU/Dogfish, alternating
the old and new binaries, REPEAT=9) went from 1.01/1.03 wall (1.02/1.03 CPU)
with md-5 0.10 + `asm` to 0.82/0.83 wall (0.84/0.82 CPU) with md-5 0.11,
which has no x86-64 assembly backend (only `soft` and loongarch64). b2sum
(1.15) and sha256sum (0.90, unchanged, a separate matter) did not move.

Fixed on the same branch: `crate::common::md5` calls
`md5_asm::compress` directly on x86/x86-64 (md5-asm 0.5.2, MIT, a
target-specific dependency, so the aarch64 macOS type-check never sees it)
with its own RFC 1321 padding, and falls back to md-5 0.11 elsewhere; sort
-R keeps md-5. Tested against md-5 0.11 for lengths 0..=200, the padding
boundaries, 1 MiB and odd-sized streaming chunks. Bench, three rounds
alternating the three binaries (REPEAT=9, wall/CPU): md-5 0.10 + asm
1.02/1.01, 1.03/1.03, 1.03/1.02; md-5 0.11 0.80/0.82, 0.83/0.83, 0.83/0.82;
md5-asm direct 1.00/1.01, 1.01/1.01, 1.00/1.02. Back at parity, so the
README's table is restored.

Criterion left unticked until the PR merges with the full suite green.
