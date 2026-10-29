---
id: 20
uid: 1fff0ef5-4744-4f8c-b9e9-96de8bde7882
title: "Man pages and shell completions"
type: docs
status: backlog
milestone: "v1.0"
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: m
area: docs
---

## Problem

Dogfish ships no man pages and no completions, and `--help` text differs from GNU's by design.

## Proposal

Generate man pages and bash/zsh/fish completions from the same option
definitions the parser uses (0011), install them from release archives and the
Homebrew formula.

## Acceptance criteria

- [ ] `man dogfish-ls` (or `man ls` with links installed) works from a release archive.
- [ ] Completions cover every option of every tool, generated in CI.
