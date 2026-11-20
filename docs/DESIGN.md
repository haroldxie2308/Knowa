# Knowa — Design v0

Status: **draft, normative for v0**. This document is the contract the implementation is built against.
Sections marked *(v0)* ship now; *(later)* are reserved so v0 choices don't block them.

---

## 1. Vision

1. **Humans and agents build the base together.** Agents can write directly or through human review. Every write is logged with who, when, how and why.
2. **Explore other people's experience.** Shared spaces can be searched across ("cross-cluster"). Every hit comes with its context, background and provenance.
3. **Always evolving, always incremental.** Background agents roam the base, connect dots and write summaries and insights back as first-class, attributable knowledge.
4. **Stable outside, replaceable inside.** A small, omnipotent and stable interface fronts an implementation that can be replaced wholesale (storage, index, search, representation) without users noticing.

## 2. Critique → resolutions (decided)

| Risk in the vision | Resolution |
|---|---|
| A human review gate on agent writes becomes a queue nobody drains, and then gets rubber-stamped. | **Writes never block. Trust is computed at read time.** Agent claims land as `proposed`. Every read takes a *view* (status, actor kinds, confidence). Review is optional and asynchronous, and it is itself a claim. |
| "Find the dark points of another human" is a consent problem, and aggregation can reveal what was never shared. | Sharing is opt-in per entity or space (default **Private**). Insights *about* someone else's data live in **your** space, under your agent's key, and are private by default. Nobody writes into someone else's space. |
| Cross-space search needs entity alignment, which is the hard unsolved problem. | Lean on **universal join keys** that need no coordination: content hash (`b_`), time, place, and well-known external IDs (`x:`). `knowa:sameAs` covers the rest, as a claim. |
| Self-evolving agents end up citing other agents (an echo chamber), and errors compound. | Derived claims **must cite `src`**. Retracting or superseding an input makes everything derived from it **stale**, transitively. Views can exclude agent and worker kinds. Writes are idempotent. |
| An "omnipotent" interface leaks the implementation (SQL passthrough) or grows until it can't stay stable. | Stability lives in **documents**: the claim format and the query document, which only ever change additively. The CLI verbs are sugar over five primitives. No storage detail ever appears in the contract. |
| A self-evolving system needs its representation to change over time. | **Files are canonical and everything else is derived.** Any index can be deleted and rebuilt from the logs. A new storage or search engine is a rebuild, not a migration. |

## 3. Principles

1. **Three primitives.** *Blob* (immutable bytes, content-addressed), *Entity* (a stable ID with no content), *Claim* (a signed, immutable statement). Everything else is a claim with a kernel predicate.
2. **Assertions, not truth.** Contradictory claims coexist. Resolving them is a read-time policy.
3. **Append-only.** Nothing is edited. Change is expressed as a new claim that `replaces`, `retract`s or `review`s an old one.
4. **Every claim is attributable.** Each claim carries an actor key, record time, and optionally the tool, model or session (`via`), its inputs (`src`), a confidence and a rationale (`note`).
5. **Human, agent and machine output are always distinguishable.** Agent output is never silently promoted.
6. **Two times.** `at` is record time (when we learned it). `valid` and `knowa:when` are world time (when it was true). Both are first-class, and world time may be fuzzy (EDTF).
7. **Files are canonical, indexes are caches.** Delete `.index/`, rebuild, and you get the same answers.
8. **Local-first, distributable by construction.** Each writer has its own append-only log, so a replica is a union of logs and needs no coordinator.
9. **Kernel frozen, vocabulary open.** Only the `knowa:` namespace is reserved. Every other namespace is free. Evolution is additive only.
10. **Fail fast.** Invalid input, missing config and unsupported features return errors with codes. Nothing is silently defaulted.
11. **Original bytes are never lost.** Every derived fact traces back to a blob or an actor.
12. **Read with citations.** Every result carries the claim IDs that support it.

## 4. Data model *(v0)*

### 4.1 Identifiers

`b32` = RFC 4648 base32, **lowercase, no padding**. `mh(x)` = multihash sha2-256 = bytes `0x12 0x20` ‖ sha256(x).

| Kind | Form | Notes |
|---|---|---|
| claim | `c_` + b32(mh(canonical bytes)) | content-derived |
| blob | `b_` + b32(mh(bytes)) | content-derived. In subject, `ref` and `src` positions it may carry a `#fragment` |
| actor | `a_` + b32(`0xed 0x01` ‖ ed25519 pubkey) | key-derived (multicodec ed25519-pub) |
| entity | `e_` + ULID (lowercase Crockford) | random, time-sortable |
| space | `s_` + ULID (lowercase) | random |
| external | `x:<ns>:<key>` | well-known global keys, e.g. `x:wd:Q42`, `x:url:https://…`. ns `[a-z0-9-]+`, key non-empty with no whitespace |

Fragments (only after a `b_` id): `#xywh=x,y,w,h` (pixels) · `#t=start,end` (seconds) · `#page=n` · `#char=start,end`.

IDs are opaque. `x:` IDs are the one deliberate exception: they are foreign keys owned by another system.

### 4.2 Terms

A term is `ns:name`, with ns matching `[a-z][a-z0-9-]*` and name matching `[A-Za-z][A-Za-z0-9_.-]*`. Predicates are terms, and enum-like values are terms. The `knowa` namespace is reserved.

### 4.3 Values

A JSON object with **exactly one** discriminator key, plus the allowed modifier:

| Value | JSON | Rules |
|---|---|---|
| text | `{"text":"…","lang":"en"}` | ≤ 64 KiB UTF-8. `lang` is an optional BCP 47 tag. Anything larger goes in a blob |
| term | `{"term":"ns:Name"}` | |
| ref | `{"ref":"<any id>"}` | `b_` may have a fragment |
| int | `{"int":42}` | \|n\| ≤ 2^53−1 |
| dec | `{"dec":"12.50"}` | `-?[0-9]+(\.[0-9]+)?` as a string, because floats are banned in canonical form |
| bool | `{"bool":true}` | |
| when | `{"when":"<EDTF>"}` | see 4.5 |
| geo | `{"geo":{"lat":"35.68","lon":"139.76","alt":"40","r":"25"}}` | WGS84 decimal-degree strings. `alt` (metres) and `r` (uncertainty radius in metres) are optional |
| qty | `{"qty":"12.5","unit":"kg"}` | UCUM unit string |

### 4.4 Claim

```json
{
  "v": 1,
  "space": "s_…",
  "actor": "a_…",
  "seq": 42,
  "prev": "c_…",
  "at": "2026-11-21T10:00:00.123Z",
  "s": "<id>",
  "p": "ns:pred",
  "o": { "<value>": "…" },
  "q": { "ns:qual": [ { "<value>": "…" } ] },
  "valid": "<EDTF>",
  "conf": "0.8",
  "src": ["<id>", "b_…#t=3,9"],
  "replaces": ["c_…"],
  "via": { "tool": "knowa/0.1.0", "model": "…", "session": "…", "worker": "exif@1" },
  "note": "why this claim was made",
  "id": "c_…",
  "sig": "<base64url, no padding>"
}
```

- Required fields: `v, space, actor, seq, at, s, p, o, id, sig`. `prev` is required iff `seq > 1`, and it is the id of the same actor's previous claim **in this space**, forming a per-actor hash chain.
- Optional fields are **omitted when empty**. Empty objects and arrays are invalid, which keeps the encoding unique.
- `at` is RFC 3339 UTC with exactly millisecond precision, in the form `2006-01-02T15:04:05.000Z`.
- `conf` is a dec in [0, 1]. `note` is ≤ 4 KiB. `via` keys match `[a-z][a-z0-9_]*` and values are strings.
- `s` may be any id (a claim id, which makes this a claim about a claim, or a fragment-bearing blob id).
- **Canonical bytes** = RFC 8785 (JCS) serialization of the claim **without `id` and `sig`**. Number handling is trivial because only integers exist.
- `id` = `c_` + b32(mh(canonical bytes)).
- `sig` = base64url-nopad(Ed25519.Sign(priv, `"knowa/claim/v1\n"` ‖ canonical bytes)).
- **Log line** = JCS of the full claim (including `id` and `sig`) + `\n`.

**Drafts** are what writers submit: `s, p, o, q?, valid?, conf?, src?, replaces?, via?, note?`. The system assigns `v, space, actor, seq, prev, at, id, sig`. Within one write batch, a draft may use placeholders `_:name` wherever an id is expected (`s`, `o.ref`, `q` refs, `src`). Each distinct placeholder becomes one fresh `e_` id, and the write result returns the mapping.

**Idempotent writes.** Before appending, the system computes the draft's *equivalence key* = mh(JCS of `{s,p,o,q,valid}`). If **the same actor** already has a claim in the space with the same key that is not retracted, the write is a no-op and returns the existing id with `created:false`.

### 4.5 Time (EDTF subset)

Accepted forms:
- `YYYY`, `YYYY-MM`, `YYYY-MM-DD`
- a datetime `YYYY-MM-DDThh:mm[:ss[.fff]]` followed by `Z` or `±hh:mm`. With no offset, it means *local time, zone unknown*: the range widens by ±14h and the result is flagged `localUnknown`.
- an unspecified year `19XX` or `199X`
- the qualifier suffixes `?` (uncertain), `~` (approximate) and `%` (both), on date forms
- the intervals `A/B`, `A/..` and `../B`

Each form indexes as a half-open range `[lo, hi)` in Unix milliseconds. The qualifiers are preserved but do not widen the range in v0.

### 4.6 Kernel vocabulary (`knowa:`)

**Policy.** Rules marked *effective only if* are enforced at status computation, not at write time.

| Predicate | s | o | Effective only if |
|---|---|---|---|
| `knowa:owner` | space id | ref actor | genesis claim: seq 1 of the owner, recorded in `knowa.json` |
| `knowa:role` | actor id | term `knowa:Owner\|Member\|Agent\|Worker\|Viewer` | author is an owner |
| `knowa:autoAccept` | actor id | bool | author is an owner |
| `knowa:actorKind` | actor id (= author) | term `knowa:Human\|Agent\|Worker` | self-declared. Written automatically as an actor's first claim in a space |
| `knowa:review` | claim id | term `knowa:Accept\|Reject` | author is an owner or member |
| `knowa:retract` | claim id | `{"bool":true}` | author is the target's author or an owner |
| `knowa:visibility` | any id | term `knowa:Private\|Shared\|Public` | author is an owner |
| `knowa:defaultVisibility` | space id | term (as above) | author is an owner. Absent means `knowa:Private` |

The **owner set** is the genesis owner plus every actor given `knowa:role knowa:Owner` by a member of the owner set, computed as a fixpoint. A role claim that has been retracted is ignored.

**Descriptive predicates.**

| Predicate | o | Meaning |
|---|---|---|
| `knowa:type` | term | e.g. `knowa:Note`, `knowa:Photo`, `knowa:File`, `knowa:Person`, `knowa:Place`, `knowa:Event`, `knowa:Insight` |
| `knowa:label` | text | human name |
| `knowa:body` | text | short body (notes, insights) |
| `knowa:summary` | text | condensed description, usually written by an agent |
| `knowa:content` | ref `b_` | entity's content bytes |
| `knowa:when` | when | world time of the subject. Drives timeline and temporal context |
| `knowa:where` | geo | place of the subject. Drives spatial context |
| `knowa:about` | ref | the subject is about the object |
| `knowa:mentions` | ref | the subject mentions the object |
| `knowa:related` | ref | generic association, usually written by an agent and paired with `note` |
| `knowa:sameAs` | ref | identity alignment (also across spaces) |
| `knowa:name` | text | original file name |
| `knowa:source` | text | where the bytes came from (path or URI) at import |
| `knowa:mediaType` | text | MIME type |
| `knowa:size` | int | bytes |

Extractor-specific vocabulary lives in its own namespace, for example `exif:make`, `exif:model`, `exif:width`.

### 4.7 Effective status *(computed, never stored in claims)*

Each claim `c` gets exactly one status, decided by the first rule that matches:

1. **retracted** if there is an effective `knowa:retract` on c.
2. **superseded** if some claim r lists c in `replaces` and r's own status is `accepted` or `superseded`. Cycles cannot occur, because ids are content hashes.
3. **rejected / accepted** if there is an effective review. The latest one wins, ordered by `at` and then by id.
4. **accepted** if c's author is an owner or member, or has an effective `autoAccept true`.
5. **proposed** otherwise.

Policy claims (`role`, `autoAccept`, `visibility`, `defaultVisibility`, `actorKind`, `owner`) and `review` / `retract` claims carry a status too. Their *effect*, however, depends only on the "effective only if" rule above and on not being retracted.

**stale** is a separate boolean. It is true iff some `c_` id in `c.src` is retracted, rejected, superseded, or itself stale.

**Default read view:** status ∈ {accepted, proposed}, stale included. Every result is annotated with `status` and `stale`.

**Display value.** When a single value is shown (for example an entity's label), the order is accepted before proposed, then the latest `at`. This is a display policy only: all values remain in the data.

### 4.8 Entity timeline position

An entity's `when` is the range of its display-chosen `knowa:when` claim. With no `knowa:when`, it falls back to the earliest `at` among claims whose subject is the entity, and is reported with `"source":"recorded"`. That fallback is correct for notes and observations, which are written when they happen, and the output always labels which source was used.

## 5. Space on disk *(v0)*

A **space** is identified by `s_…`. A **directory is one replica** of a space. Several replicas sync by exchanging logs.

```
<dir>/
  knowa.json                       {"format":1,"space":"s_…","owner":"a_…","genesis":"c_…"}
  log/<actor-id>/<000000000001>.jsonl   per-actor append-only segments; file name = first seq (12 digits); rotate at 10000 lines
  blobs/<id[6:8]>/<blob-id>        raw bytes (id[2:5] is always "ciq", the multihash header)
  .index/                          derived (SQLite etc.); never exported; deletable
  .lock                            advisory write lock
```

- **Write path:** take the lock, read the actor's head (last seq and id) from the log tail, build claims, validate, sign, append and fsync, apply them to the index, release the lock.
- **On open:** if the index lags the logs (crash, or files copied in), it catches up from each actor's last indexed seq. If the index is missing or has the wrong schema version, it rebuilds and gets a new epoch.
- **Replicas** may have *gaps* in an actor's seq when imported from a partial bundle. Chain verification covers contiguous runs only, and gaps are reported.

## 6. The interface (stable contract) *(v0)*

Five primitives. Everything else is sugar.

| Primitive | In | Out |
|---|---|---|
| **write** | `{space, drafts[]}` with the actor from the environment | `{items:[{id, created, status}], placeholders:{"_:x":"e_…"}}` |
| **blob.put** | bytes | `{id, created, size}`. The media type is not part of the bytes: it arrives as a `knowa:mediaType` claim from the `media` extractor |
| **blob.get** | `{id}` (in any selected space) | bytes |
| **query** | a query document (6.1) | the op's result (6.2) |
| **feed** | `{space, cursor?, limit}` | `{claims:[ClaimView], cursor}` in local arrival order. The cursor is `<epoch>:<n>`. If the epoch no longer matches, the call fails with `cursor_expired` |

Admin ops are `init, verify, reindex, export, import`. They are part of the CLI but not the query contract.

### 6.1 Query document

```json
{
  "op": "find | get | context | path | log",
  "spaces": ["<alias or s_id>"] ,
  "view": {"status": ["accepted","proposed"], "actorKinds": ["knowa:Human"], "actors": ["a_…"], "includeStale": true, "minConf": "0.5"},
  "asOf": null,

  "target": "entities | claims",
  "where": {
    "ids": [], "s": [], "p": [], "o": {"<value>":"…"}, "type": ["knowa:Photo"],
    "text": "free text",
    "when": {"from": "<EDTF>", "to": "<EDTF>"},
    "near": {"lat": "…", "lon": "…", "km": "…"},
    "nearId": {"id": "…", "km": "…"},
    "actors": [], "recordedAfter": "<RFC3339>", "recordedBefore": "<RFC3339>"
  },
  "order": "relevance | when | -when | recorded | -recorded | distance",
  "limit": 50,
  "cursor": null,

  "id": "…", "hops": 1, "window": "P1D", "km": "1",
  "from": "…", "to": "…", "maxHops": 4,
  "ids": []
}
```

- `spaces` defaults to the current space. `["*"]` means every registered space.
- `asOf` (record-time travel) is **reserved**. Setting it in v0 returns `unsupported`.
- Multi-space `find` with a `cursor` is `unsupported` in v0, so use a larger `limit`.
- Each op uses only its own fields. A field from another op is an `invalid` error, so typos fail fast.
- `limit` defaults to 50 and has a maximum of 1000. `hops` ≤ 3. `maxHops` ≤ 6.

### 6.2 Results

- **EntityView** `{id, spaces[], label?, types[], when?:{value, from?, to?, source:"claim"|"recorded"}, where?:{lat,lon,r?}, score?, distanceKm?, claims: [ids supporting the fields above]}`. `from`/`to` are canonical timestamps; absent means open.
- **ClaimView** = the full claim + `{status, stale, space}`
- `find`: `{items: EntityView[] | ClaimView[], cursor?}`
- `get`: `{items: (EntityView+claims | ClaimView | BlobView)[]}`
- `context`: `{entity: EntityView, claims: ClaimView[] (s = id), inbound: ClaimView[] (o.ref = id), neighbors: [{entity, via, p, dir:"out"|"in"}], temporal: EntityView[], spatial: EntityView[], actors: [{id, kind?, role?, claims}]}`
- `path`: `{paths: [[{id, via, p, dir}]]}` (shortest first)
- `log`: `{items: ClaimView[], cursor?}`. The raw audit trail, regardless of view.

### 6.3 Errors

With `--json`, errors go to stderr as `{"error":{"code":"…","message":"…","detail":{…}}}`.

| code | exit |
|---|---|
| internal | 1 |
| usage | 2 |
| not_found | 3 |
| invalid | 4 |
| conflict | 5 |
| unsupported | 6 |
| integrity | 7 |
| cursor_expired | 8 |

### 6.4 Stability rules

- The claim format and the query and result documents only evolve **additively**. Readers ignore unknown result fields, and writers never emit unknown claim fields.
- `knowa schema --json` prints the machine-readable contract: ops, document shapes, kernel vocabulary and error codes. This is the one call an agent needs in order to learn the tool.
- The CLI's text output is for humans and is **not** part of the contract. `--json` output is.

## 7. CLI *(v0)*

Global flags (each with an env override) are `--home` (`KNOWA_HOME`, default `~/.knowa`), `--space` (`KNOWA_SPACE`, an alias or path), `--actor` (`KNOWA_ACTOR`, an actor name) and `--json`. A missing current space or actor is a usage error that names the flag or env var to set.

| Verb | Maps to |
|---|---|
| `init <dir> --name N [--alias A]` | create space + genesis + actorKind + label; grant the built-in `extract` worker Worker role + autoAccept; register the alias |
| `space ls\|add <dir> [--alias] [--readonly]\|rm <alias>\|use <alias>` | registry |
| `actor ls\|new <name> --kind human\|agent\|worker\|use <name>\|show` | local keys |
| `write` (JSONL drafts on stdin) | **write** |
| `query [<json>\|-]` | **query** |
| `feed [--cursor C] [--limit N] [--follow]` | **feed** |
| `blob put <file>` / `blob cat <id>` | **blob.put / blob.get** |
| `put <paths…> [--no-extract]` | ingest: blob.put plus `knowa:name`/`knowa:source` by the actor, then extractors |
| `note <text\|-> [--title T] [--when EDTF] [--about id…]` | write a Note entity |
| `assert <s> <p> <value> [--q p=value…] [--valid] [--src…] [--conf] [--note] [--replaces…]` | write. Value syntax: `text:…`, `term:…`, `ref:…`, `int:…`, `dec:…`, `bool:…`, `when:…`, `geo:lat,lon[,r]`, `qty:12.5:kg`. A bare token is text |
| `retract <claim> [--note]` · `review <claim> accept\|reject [--note]` | write |
| `grant <actor> <role> [--auto-accept]` · `share <id> private\|shared\|public` | write (policy) |
| `pending` | find claims with status proposed |
| `search <text> [filters]` · `timeline [--from --to]` · `near <lat,lon\|id> [--km]` | find |
| `show <id…>` | get |
| `context <id> [--window P1D] [--km 1] [--hops 1]` | context |
| `path <a> <b>` | path |
| `log [--actor] [--since]` | log |
| `extract [--all]` | run built-in extractors over blobs missing their output |
| `worker run <actor-name> --cmd "<exe args>" [--batch N] [--follow]` | feed → external command → write |
| `export <file.knowa> [--level private\|shared\|public]` · `import <file.knowa> [--alias]` | bundles |
| `verify` · `reindex` · `schema` | admin |

### 7.1 Workers *(v0)*

`worker run` reads the feed from a cursor stored in `<space>/.index/cursors/<actor>.json`. For each batch it executes `--cmd`, with this JSON on stdin:
`{"space":"s_…","cursor":"…","claims":[ClaimView…]}`
and with `KNOWA_HOME`, `KNOWA_SPACE` and `KNOWA_ACTOR` set, so the command can call `knowa query` itself. The command's stdout is JSONL drafts, which are written under the worker's actor with `via.worker = <actor-name>`. A non-zero exit stops the run and does **not** advance the cursor. Any program can be a worker, and LLM agents are just one kind of worker. No LLM provider is linked into the core.

### 7.2 Built-in extractors *(v0)*

Built-in extractors are pure Rust with no external tools. They write as the `extract` worker actor, which is auto-created in the home directory and granted Worker + autoAccept at `init`. Each extractor has a version, recorded as `via.worker = "<name>@<n>"`.

- **media@1**: `knowa:mediaType` (sniffed from content first, extension second), `knowa:size`, and `knowa:type` (`knowa:Photo` for `image/*`, otherwise `knowa:File`).
- **exif@1**: finds the EXIF/TIFF block in JPEG, HEIC/HEIF, PNG (`eXIf`) and TIFF files. Writes `knowa:when` from DateTimeOriginal (+ OffsetTimeOriginal when present, otherwise local-unknown), `knowa:where` from GPS, and `exif:make`, `exif:model`, `exif:width`, `exif:height`, `exif:orientation`.

Text blobs (`text/*`, ≤ 1 MiB) are indexed for full-text search directly from the blob store. No claims are needed, because the text is derived.

### 7.3 Sharing *(v0)*

- `export --level shared` includes the claims whose effective visibility is ≥ `shared`. Effective visibility is the claim's own visibility setting, else its subject's, else the space default. The bundle also includes every policy claim and the referenced blobs, and is marked `"partial":true` unless the level is `private`, which exports everything.
- A bundle is a tar archive containing `bundle.json` (`{format, space, owner, genesis, level, partial, exportedBy, at}`), the `log/…` segments and `blobs/…`.
- `import` verifies every signature, blob hash and chain run, then merges into a replica directory (creating it if needed), registers it read-only, and reindexes.
- Readonly replicas refuse local writes. To annotate someone else's content, write in **your** space and refer to their ids.

## 8. Implementation (invisible) *(v0)*

- Rust (edition 2024), synchronous (no async runtime in v0), single static binary `knowa`.
- Crates (Cargo workspace, `crates/*`):

| Crate | Owns | Third-party deps |
|---|---|---|
| `knowa-model` | ids, values, claims, JCS, EDTF, signing, validation, kernel vocab, the query/result contract types, the shared `Error` | serde, serde_json, ed25519-dalek, sha2, data-encoding, getrandom |
| `knowa-store` | the replica directory: manifest, lock, logs, blobs, verify, bundles | tar, sha2 |
| `knowa-index` | the derived SQLite index of one replica: apply, status/stale, FTS5, geo, edges, arrival feed, single-replica reads | rusqlite (bundled) |
| `knowa-extract` | media sniffing, EXIF, the extractor registry | infer, kamadak-exif |
| `knowa-config` | home: actor keys, space registry | — |
| `knowa-api` | the five primitives and the admin ops. Composes store + index + extract across spaces: fan-out, merge, context, path | — |
| `knowa-cli` | the `knowa` binary: argument parsing and rendering only | clap |

- The index is one SQLite file per replica. Multi-space queries fan out to each replica's index and merge in `knowa-api`.
- Swapping the index engine means re-implementing `knowa-index`'s public surface and reindexing. Nothing else changes.
- Why Rust: one core that embeds into the CLI, an MCP server, Python/Node bindings and WASM (for the browser visualizer); native access to the later stack (tantivy, Lance, iroh/Willow, h3o); sum types for values and status.

## 9. Roadmap

- **v0 (now):** everything marked *(v0)*.
- **later:**
  - `asOf` time travel
  - vector search through a pluggable external embedder (cached in `.index`, keyed by content hash and model)
  - network sync using version vectors over per-actor logs
  - capability-based sharing (UCAN/Meadowcap-style)
  - encryption per collection, and crypto-shredding for real deletion
  - an MCP server
  - a web UI with graph, timeline, map, gallery and provenance views
  - GPX and Takeout connectors
  - PDF/OCR/ASR extractors
  - LLM summary and insight workers
  - per-worker budgets
  - golden-question retrieval evals
  - an OKF (markdown) round-trip

## 10. Known risks

- Local key separation is a convention: an agent on the same machine could use a human's key. *(later: OS keychain + per-actor prompts.)*
- Partial replicas have seq gaps, so their chain verification is weaker.
- SQLite with one index per replica is fine up to roughly 10^7 claims per space. Beyond that, the `index.Index` boundary is where a new engine plugs in.
