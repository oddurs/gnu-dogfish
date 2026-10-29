---
id: 21
uid: 7f3ec400-a450-4a45-a61b-b9684970467c
title: "A verifiable supply chain"
type: chore
status: backlog
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

- [ ] CI fails on a known-vulnerable or disallowed dependency.
- [ ] Two independent builds of a release produce identical binaries.
- [ ] Release artifacts carry signatures, provenance and an SBOM.
