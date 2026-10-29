---
id: 44
uid: f3dfedbb-b966-487c-b67e-3a92dd527d1f
title: "df and du: remaining GNU test failures"
type: bug
status: doing
assignee: claude
claimed: 2026-10-29
milestone: "v1.0"
depends_on:
- 8
created: 2026-10-29
updated: 2026-10-29
priority: p2
effort: m
area: compat
---

## What happens

- `df --output=target --total` prints `-` in the total row's target column; GNU prints `total`.
- `du -lL` follows a `dotdot -> ..` cycle; GNU's FTS_LOGICAL drops it silently.
- `du: cannot access 'x'` lacks the `: <strerror>` suffix (ELOOP, ENOENT).
- `du -t` errors spell the option as typed; GNU always says `invalid --threshold argument`.
- `du --time` / `--time-style` are not implemented.

GNU tests that fail on it ([run](https://github.com/oddurs/gnu-dogfish/actions/runs/40885586807)):

- `tests/df/df-output.sh`
- `tests/du/deref.sh`
- `tests/du/long-sloop.sh`
- `tests/du/threshold.sh`
- `tests/misc/time-style.sh`

## What should happen

All listed tests pass.

## Reproduction

1. `util/gnu-tests.sh` with `TESTS="tests/df/df-output.sh tests/du/deref.sh tests/du/long-sloop.sh"`, as a non-root user.

## Acceptance criteria

- [ ] The listed GNU tests pass and are added to `util/gnu-baseline.json`.
- [ ] Each fixed behaviour has a differential case against GNU in `crates/dogfish/tests/gnu_compat*.rs`.

## 2026-10-29

df: the total row prints `total` in the target column when no source column is shown (tests/df/df-output.sh passes, in the baseline); cells keep raw bytes, control characters become `?`, invalid UTF-8 cells are left unpadded in UTF-8 locales, as GNU. The du parts remain.
