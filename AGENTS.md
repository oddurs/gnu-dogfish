<!-- cairn:begin -->
## Roadmap and issues

This project tracks its roadmap and issues with `cairn`. Every item is a Markdown file under `cairn/items`, described by the schema in `cairn.toml`.

**Do not create ad-hoc TODO, PLAN or NOTES files.** Create a cairn item instead, so the work appears on the board and in the generated roadmap.

### The loop

1. `cairn next --view next` — what is ready to start. It excludes anything blocked by unfinished dependencies and puts work already in progress first.
2. `cairn claim <ID>` — take it before you start, so no one duplicates the work. `cairn claim --next --view next` picks and claims the top-ranked unclaimed item in one step. Then read `cairn prompt <ID>`: the item with everything it rests on.
3. Do the work. Record what you learn: `cairn set <ID> <field>=<value>` for fields, `cairn note <ID> "<TEXT>"` for anything that needs a sentence — why you chose something, what you tried, what to watch for.
4. `cairn tick <ID> <N>` as each acceptance criterion becomes true — `cairn show <ID> --criteria` lists them numbered. Tick what is true, not what would let you close.
5. `cairn close <ID>` when it is done, or `cairn release <ID>` to hand it back.
6. `cairn check` before you report finished. It must pass.

### Schema

- **Types**: `feature`, `bug`, `chore`, `docs`, `decision`, `milestone`
- **Statuses**: `backlog` (open), `planned` (open), `doing` (active), `blocked` (open), `done` (done), `dropped` (dropped)
- **`priority`**: one of p0, p1, p2, p3 — p0: broken build or data loss; p1: next outcome; p2: useful; p3: optional
- **`effort`**: one of s, m, l, xl — Rough size, not an estimate
- **`area`**: free text — compat, safety, testing, architecture, performance, security, docs, release, or direction
- **Milestones**: `v0.2`, `v0.3`, `v1.0`
- **Saved views** (`cairn list --view NAME`): `now`, `next`, `waiting`, `later`, `decisions`

### Rules

1. Before starting work, find or create the item and set it to an active status.
2. Use the fields above rather than inventing new ones; add new fields to `cairn.toml` first.
3. Never hand-edit the generated roadmap file (`cairn/ROADMAP.md`) — change items and run `cairn render`.
4. `cairn check` must pass before the work is considered done.

<!-- cairn:end -->

## Working on GNU/Dogfish

- `ROADMAP.md` at the repository root is the hand-written list of **known differences from GNU**; keep it accurate when behaviour changes. The planned work lives in `cairn/items`.
- Decision 0024 records why the v0.3 work is ordered as it is. Work it in that order unless an item says otherwise.
- When output is not a terminal, every tool must match GNU coreutils 9.4 byte for byte (output, messages, exit status). Rich output is for terminals only.
- Before pushing: `cargo fmt --check`, Clippy with `-D warnings`, `cargo test` **and** `cargo test --release`, the test suites as a non-root user, and a macOS type-check. Item 0009 turns this into one command.
- Never run destructive commands outside a temporary directory, and never against `/`, even in tests. Test such guards through a pure predicate.
