---
id: 15
uid: da50d0db-cc2e-40ee-ad31-e5089b393e0d
title: "Upgrade the RustCrypto crates together"
type: chore
status: backlog
milestone: "v0.3"
depends_on:
- 4
created: 2026-10-29
updated: 2026-10-29
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
