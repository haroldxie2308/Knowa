# Knowa

Knowa is a universal, local-first and distributable knowledge-base format, plus a toolkit for it. Humans and agents write signed, append-only claims. Everything else is derived from them.

**`docs/DESIGN.md` is normative.** Read it before touching code. If the code and the design disagree, the code is wrong, unless the design is changed first and on purpose.

## Stack

- Rust, edition 2024. Public APIs are sync only. tokio exists solely inside `knowa-store`'s object-store backend, on a private runtime (DESIGN §8). Cargo workspace in `crates/*`.
- Crate boundaries and dependencies: DESIGN §8. The dependency direction is `model ← store ← index ← api ← web ← cli`. `extract` and `config` depend only on `model`.
- The only SQLite is `rusqlite` with the bundled feature (FTS5 and rtree available).

## Commands

```bash
cargo build
```

```bash
cargo test
```

```bash
cargo test -p knowa-model
```

```bash
cargo clippy --all-targets -- -D warnings
```

```bash
cargo run -q -p knowa-cli -- --help
```

When testing the CLI, run it against a scratch home: `KNOWA_HOME=$(mktemp -d)`. Never use the real `~/.knowa` in tests.

## Invariants (red lines)

- **Claims are immutable and append-only.** Never rewrite or delete a log line. A change is a new claim (`replaces` / `knowa:retract` / `knowa:review`).
- **Canonical bytes are JCS** with integers only. A claim id is `c_` + b32(multihash(sha256)) over the claim without `id`/`sig`. Changing canonicalization breaks every existing id, so don't.
- **The replica is canonical; the index is derived** (machine-local, under `$KNOWA_HOME/index/`, DESIGN §5.2). Nothing may be stored only in the index. `knowa reindex` must reproduce the same answers.
- **Status is computed, never stored in claims** (DESIGN §4.7). There is exactly one implementation of the status rules, and it lives in `knowa-index`.
- **No floats in canonical data.** Decimals are strings (`Dec`). Floats are allowed only in result fields such as `score` and `distanceKm`.
- **Every write goes through `knowa_api::Knowa::write`** (or the store's `merge` for imports). No other path appends to logs.
- **The `knowa:` namespace is reserved.** New kernel terms are added in `knowa-model/src/vocab.rs` and DESIGN §4.6 together.
- **Contract types are additive-only:** `knowa-model` `query.rs` / `view.rs`, the claim JSON, and the error codes.
- **Errors:** use `knowa_model::Error` with the right `ErrorCode`. No `unwrap()` / `expect()` outside tests, except on invariants that are truly impossible, with a comment saying why.
- **Agents' keys are separate actors.** Never sign agent output with a human's key.

## Testing

- Golden vectors for canonical JSON, ids and signatures live in `crates/knowa-model/tests/`. They are the conformance suite, and a change to them is a format change.
- Status/stale rules, chain/merge integrity, and bundle round-trips must have tests.
- CLI end-to-end tests live in `crates/knowa-cli/tests/` and use a temporary `KNOWA_HOME`.
