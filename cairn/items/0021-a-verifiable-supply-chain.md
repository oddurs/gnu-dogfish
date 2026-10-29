---
id: 21
uid: 7f3ec400-a450-4a45-a61b-b9684970467c
title: "A verifiable supply chain"
type: chore
status: doing
assignee: claude
claimed: 2026-10-29
milestone: "v1.0"
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: m
area: security
---

## Problem

A distribution default must be auditable and reproducible.

## Proposal

`cargo-deny` and `cargo-audit` in CI; sanitizer runs (ASan/UBSan via nightly)
over the test suites; an audited inventory of every `unsafe` block;
reproducible release builds; signed artifacts with build provenance
(Sigstore/GitHub attestations); an SBOM per release.

## Acceptance criteria

- [x] CI fails on a known-vulnerable or disallowed dependency.
- [x] Two independent builds of a release produce identical binaries.
- [ ] Release artifacts carry signatures, provenance and an SBOM.

## 2026-10-29

- `deny.toml` + `cargo xtask deny` + `.github/workflows/supply-chain.yml` (PRs, main, weekly): RustSec advisories, a license allow-list for GPL-3.0-or-later OR MIT distribution, no duplicate majors (one documented skip), crates.io only. A planted vulnerable dependency fails it.
- `doc/unsafe.md` inventories all 103 `unsafe` in 31 files; `cargo xtask unsafe-audit` (in `ci`) fails on an unlisted one; `clippy::undocumented_unsafe_blocks` and `unsafe_op_in_unsafe_fn` are denied workspace-wide.
- `cargo xtask asan`: every test under AddressSanitizer (nightly-2026-10-20, build-std), on main and weekly: no findings across 2,053 tests.
- `cargo xtask dist` / `repro`: path-remapped, SOURCE_DATE_EPOCH, deterministic tar + `gzip -n`; two builds in different directories are bit-identical (binary f7683be6…, archive 62567570…); a CI job checks it.
- release.yml: pinned toolchain, CycloneDX SBOM per archive, SHA256SUMS, and build-provenance + SBOM attestations from a publish job that alone holds write permissions; `doc/verify.md` explains checking them.
- Every workflow pins actions to commit SHAs, least privilege per job, `persist-credentials: false`, concurrency groups.

The third criterion (signatures, provenance and SBOM on release artifacts) is implemented and lint-clean but proven only by the next release (0006).
