# Knowa

A local-first knowledge-base format and a Rust command-line toolkit for it. People and their agents write signed, append-only claims that record who, when, how and why. Search, timelines and everything else are derived from those claims.

> **Status: design v0, no release yet.** The format and CLI in [`docs/DESIGN.md`](docs/DESIGN.md) are implemented and tested. Object-store replicas (S3) and the web UI are experimental and may change.

## Why

- **Agents write alongside you, never over you.** Writes are never blocked. An agent's claims arrive as *proposed*, and a review is just another claim. Each read decides what it trusts.
- **Every fact can be traced to its author.** A claim carries the key that signed it and when it was recorded. It can also name the tool or model that produced it, its sources, a confidence and a note. When a source is retracted, everything derived from it is marked stale.
- **The files are the source of truth.** Each writer has its own append-only log, and file contents are stored by hash. The SQLite index is only a cache: delete it, rebuild it, and you get the same answers.
- **Private by default.** You share an entity or a whole space only when you choose to, and a shared export leaves out everything that is still private.

## Concepts

| Term | What it is |
|---|---|
| **Blob** | Immutable bytes, addressed by their hash (`b_…`). |
| **Entity** | A stable id with no content of its own (`e_…`). |
| **Claim** | A signed, immutable statement about an entity, a blob or another claim (`c_…`). |
| **Actor** | A key that signs claims: a human, an agent or a worker (`a_…`). |
| **Space** | A set of claims with one owner and its own policies (`s_…`). A *replica* is a copy of a space. |

## Install

You need Rust 1.90 or later.

```bash
git clone https://github.com/haroldxie2308/Knowa.git
```

```bash
cd Knowa && cargo install --path crates/knowa-cli
```

## Quick start

Create a key, then a space, and select both:

```bash
knowa actor new me --kind human
knowa actor use me
knowa init ~/notes.knowa --name "Notes" --alias notes
knowa space use notes
```

Write a note, add some files, and look things up:

```bash
knowa note "Ferry to Cacilhas, back mid-afternoon." --title "Lisbon ferry" --when 2026-05-17
knowa put ~/Pictures/lisbon
knowa search ferry
knowa timeline
```

`put` copies the files into the space. The built-in extractors then record each file's type and size, and for photos the date and place from the EXIF data.

An agent gets its own key, and what it writes waits for review:

```bash
knowa actor new trip-agent --kind agent
knowa --actor trip-agent note "Day trip by ferry to Cacilhas." --title "Trip summary"
knowa pending
knowa review accept --by trip-agent --note "Checked against the photos."
```

`knowa serve` opens a web UI for this machine only, with search, timeline, gallery and review. It is experimental.

## For agents and tools

- **`knowa schema --json`** describes every verb, document and error code. Agents read it to learn the tool.
- **`--json`** on any command gives machine-readable output.
- **`knowa query`** takes a query document, and **`knowa write`** takes claim drafts as JSON lines on stdin.
- **`knowa worker run <actor> --cmd "<program>"`** passes new claims to any program and writes what the program prints back under the worker's own key. LLM agents are one kind of worker. No LLM provider is built in.

## Sharing

`knowa export trip.knowa --level shared` writes a bundle of what is visible at that level. `knowa import` checks every signature and hash, then adds the bundle as a read-only replica. To annotate someone else's space, write in your own space and refer to their ids.

## Storage (experimental)

A space can live in a directory or at `s3://bucket/prefix` on any S3-compatible store, and `--blobs` can keep the file contents at a separate location. Credentials come from the standard `AWS_*` environment variables, and Knowa never stores them. The index always stays on the local machine, under `$KNOWA_HOME/index/`. See [DESIGN §5](docs/DESIGN.md#5-replicas-and-storage-experimental).

## Repository

| Path | What it holds |
|---|---|
| `crates/knowa-model` | The claim format: canonical JSON, ids, signatures, and the query, result and error contracts |
| `crates/knowa-store` | Replicas: logs, blobs, locks and bundles, on a directory or an object store |
| `crates/knowa-index` | The SQLite index derived from a replica: status rules, full-text search, places and the feed |
| `crates/knowa-extract` | The built-in extractors (media type and EXIF) |
| `crates/knowa-config` | The home directory: actor keys and the list of registered spaces |
| `crates/knowa-api` | The five primitives (write, blob.put, blob.get, query, feed) and the admin operations |
| `crates/knowa-web` | The local web UI behind `knowa serve` |
| `crates/knowa-cli` | The `knowa` binary |
| `docs/DESIGN.md` | The specification |
| `site/` | The static landing page |

## Development

```bash
cargo test
```

```bash
cargo clippy --all-targets -- -D warnings
```

Run the CLI against a throwaway home so your real `~/.knowa` is never touched:

```bash
KNOWA_HOME=$(mktemp -d) cargo run -q -p knowa-cli -- --help
```

[`docs/DESIGN.md`](docs/DESIGN.md) is normative: where the code and the design disagree, the code is wrong. The golden vectors in `crates/knowa-model/tests/` are the conformance suite.
