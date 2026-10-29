## Summary
<!-- Describe what this PR adds, fixes, or optimizes -->

## Benchmarks / Telemetry (`--stats` or `dogfish bench`)
<!-- If this touches a hot path, paste before/after `dogfish bench` or `--stats` output -->

## Checklist
- [ ] PR title is a Conventional Commit (`feat: ...`, `fix(tool): ...`) — it becomes the squashed commit on `main` and drives release notes
- [ ] `cargo test` and `cargo test --release` pass
- [ ] Added tests (`tests/integration.rs`, GNU differential cases in `tests/gnu_compat*.rs`, data-safety cases in `tests/fs_safety.rs`)
- [ ] Preserves `--json` (`-J`) and `--stats` telemetry support where applicable
