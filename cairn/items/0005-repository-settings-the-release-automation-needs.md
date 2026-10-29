---
id: 5
uid: 25d80953-81af-4ff2-9a5a-64fc6370f17e
title: "Repository settings the release automation needs"
type: chore
status: blocked
milestone: "v0.2"
owner: oddurs
created: 2026-10-29
updated: 2026-10-29
priority: p1
effort: s
area: release
---

## Purpose

The workflows merged in #1 depend on settings only a maintainer can change.
Release Please's first run on `main` created its branch
(`release-please--branches--main--components--gnu-dogfish`, commit `47f804a`)
and then failed: `GitHub Actions is not permitted to create or approve pull
requests.`

## Approach

In the repository settings:

- Actions → General → Workflow permissions: allow GitHub Actions to create and approve pull requests.
- Branches: protect `main`, requiring Lint (ubuntu, macos), Test (ubuntu, macos), MSRV and PR Title.
- General → Pull Requests: allow squash merging (ideally only squash) and auto-merge.
- Optional secrets `RELEASE_PLEASE_TOKEN` and `AUTO_PR_TOKEN` so bot-opened PRs get CI.

## Acceptance criteria

- [ ] Re-running the Release Please workflow opens the release PR.
- [ ] A PR cannot merge to `main` with a failing or missing required check.
