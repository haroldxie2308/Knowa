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
7. **Files are canonical, indexes are caches.** Delete an index, rebuild it, and you get the same answers.
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

**Idempotent writes.** Before appending, the system computes the draft's *equivalence key* = mh(JCS of `{s,p,o,q,valid}`). If **the same actor** already has a claim in the space with the same key that is **active (accepted or proposed) and not stale**, the write is a no-op and returns the existing id with `created:false`. A superseded, rejected, retracted or stale equivalent does not block a new claim, so re-asserting a reverted value or re-deriving from new inputs creates a new claim.

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
| `knowa:role` | actor id | term `knowa:Owner\|Member\|Agent\|Worker\|Viewer` | author is an owner. **Single-valued:** the latest effective role claim (by `at`, then id) is the actor's role, so a later grant demotes. Retract it to remove the role |
| `knowa:autoAccept` | actor id | bool | author is an owner. The latest effective value wins, so `false` revokes |
| `knowa:actorKind` | actor id (= author) | term `knowa:Human\|Agent\|Worker` | self-declared, and only the actor's **first** actorKind claim in the space (lowest seq) is effective. It is written automatically, and `write` rejects explicit actorKind drafts |
| `knowa:review` | claim id | term `knowa:Accept\|Reject` | author is an owner or member |
| `knowa:retract` | claim id | `{"bool":true}` | author is the target's author or an owner |
| `knowa:visibility` | any id | term `knowa:Private\|Shared\|Public` | author is an owner |
| `knowa:defaultVisibility` | space id | term (as above) | author is an owner. Absent means `knowa:Private` |

The **owner set** is the genesis owner plus every actor whose effective role, set by a member of the owner set, is `knowa:Owner`. It is computed as the well-founded fixpoint, so owners who revoke each other in a cycle are both not owners. A role claim that has been retracted is ignored. `visibility` and `defaultVisibility` are also latest-wins.

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
| `knowa:modified` | when | filesystem modification time of the bytes at `knowa:source`, read at import, whole seconds UTC. A fact about one copy on one disk, not a world time: copies, checkouts and exports reset it. Timeline fallback (§4.8) |
| `knowa:created` | when | filesystem creation (birth) time of the file at `knowa:source`, read at import, whole seconds UTC. Written only when the OS reports one. Usually when the file arrived on that disk. Not a timeline input |

Extractor-specific vocabulary lives in its own namespace, for example `exif:make`, `exif:model`, `exif:width`.

### 4.7 Effective status *(computed, never stored in claims)*

Each claim `c` gets exactly one status, decided by the first rule that matches:

1. **retracted** if there is an effective `knowa:retract` on c.
2. **superseded** if some claim r lists c in `replaces` and r's **base status** is `accepted`. The base status is the status r would get if rule 2 were skipped, using rules 1, 3, 4 and 5. A replacement that is merely proposed, or only superseded itself, supersedes nothing. Cycles cannot occur, because ids are content hashes.
3. **rejected / accepted** if there is an effective review. The latest one wins, ordered by `at` and then by id.
4. **accepted** if c's author is an owner or member, or has an effective `autoAccept true`.
5. **proposed** otherwise.

Policy claims (`role`, `autoAccept`, `visibility`, `defaultVisibility`, `actorKind`, `owner`) and `review` / `retract` claims carry a status too. Their *effect*, however, depends only on the "effective only if" rule above and on not being retracted.

**stale** is a separate boolean. It is true iff some `c_` id in `c.src` is retracted, rejected, superseded, or itself stale.

**Default read view:** status ∈ {accepted, proposed}, stale included. Every result is annotated with `status` and `stale`.

**Views apply to evidence.** A view filters claims, and everything about an entity is derived from the claims that pass it:
- whether the entity matches `text`, `type`, `when` or `near`;
- its display fields (label, types, when, where).

So output from a kind of actor (or a status) the view excludes can never make an entity match, or change how it looks. For `text`, an entity matches when every query token is found in the text of at least one claim that passes the view. The tokens may come from different claims.

**Text matching is normalization-insensitive.** Indexed text and query text are both brought to Unicode NFKC before tokenizing, so canonically or compatibly equivalent spellings match: decomposed Hangul from macOS file names (NFD) and composed Hangul, full-width and half-width forms, ligatures, `Ⅱ` and `II`. This is an index rule only. Claims, ids and signatures keep the exact bytes that were written, and display fields show them as written.

**Display value.** When a single value is shown (for example an entity's label), the order is accepted before proposed, then the latest `at`. This is a display policy only: all values remain in the data. An entity's **label** is its display-chosen `knowa:label`, else its display-chosen `knowa:name`, so a file shows the name it was added under until someone labels it.

### 4.8 Entity timeline position

An entity's `when` is, first match wins:
1. the range of its display-chosen `knowa:when` claim, `"source":"claim"`;
2. else the range of its chosen `knowa:modified` claim, `"source":"file"`. Chosen = accepted before proposed, then the **earliest** range start, then claim id. Earliest, because a blob is one exact byte sequence: when the same bytes were seen at several paths, later copies only carry later times, so the earliest observation is the best estimate of when that version was written;
3. else, **if the entity is a `knowa:Note`** (a `knowa:type knowa:Note` claim about it passes the view), the earliest `at` among claims whose subject is the entity, `"source":"recorded"`. A note is written when it happens, so its record time is its world time;
4. else the entity has no `when`: it is not on the timeline, never matches a `when` filter, and has no temporal context.

Record time is world time only for notes. An agent that creates an entity for a project, a person or a concept while reading has recorded when it *learned of* the thing, not when the thing happened; dating such entities by record time put every one of them on the timeline at the moment of reading. An actor recording any other event or observation states its `knowa:when`.

`knowa:created` never feeds `when`: a copy or download gets a fresh birth time, so it usually means "arrived on this disk". Like every display field, only claims that pass the view count (§4.7, *Views apply to evidence*). Across spaces (§6), `claim` beats `file` beats `recorded`; between two `claim`s the first space wins; between two `file`s or two `recorded`s the earlier start wins.

## 5. Replicas and storage *(experimental)*

> **Experimental.** Rewritten on 2026-11-22 to separate the claim logs from blob storage and to allow object storage. Until it has been tested against S3 and against a JuiceFS mount shared by several machines, anything in this section may still change incompatibly. The claim format, ids and query contract (§4, §6) do not depend on it. Directory replicas written before this change need nothing except an index rebuild, which happens on open. Their old `.index/` directory is no longer read and can be deleted, and worker cursors stored there are not carried over: a worker starts from the beginning of the feed again, which is safe because writes are idempotent.

A **space** is identified by `s_…`. A **replica** is one copy of it, made of three parts:
- the **manifest** `knowa.json`, `{"format":1,"space":"s_…","owner":"a_…","genesis":"c_…"}`, written once and never changed;
- the **claim logs**, one append-only chain per actor;
- the **blobs**, raw bytes addressed by their hash.

The manifest and the logs live at the replica's **location**. The blobs live there too, unless the replica is registered with a separate **blob location**. That way the logs, which are small and written often, can sit on low-latency storage while the bytes sit on cheap bulk storage. A replica is canonical. Everything derived from it (the index, feed cursors, cached blob bytes) is **machine-local** state under the home directory of the machine reading the replica, never inside the replica. Several replicas sync by exchanging logs. Several machines may also read and write one shared replica (§5.3, §5.4).

### 5.1 Layout

Every location kind uses the same relative layout:

```
knowa.json
log/<actor-id>/<000000000001>.jsonl   a segment: name = its first seq (12 digits); lines in seq order
blobs/<id[6:8]>/<blob-id>             raw bytes (id[2:5] is always "ciq", the multihash header)
generation                            rebuild generation (§5.4); absent until the first merge
```

Directory locations add `log/<actor-id>/.lock`, the actor's advisory lock (§5.3). Segment names sort by seq, so a reader can list one actor's segments starting after a known one.

### 5.2 Locations

| Location | Form | Status |
|---|---|---|
| directory | a path; also a mounted shared filesystem (JuiceFS, NFS) | local disk: as v0. Shared by several machines: experimental |
| object store | `s3://bucket/prefix`: S3 and S3-compatible stores (R2, MinIO, …) | experimental |

- **Credentials** and the endpoint of an object store come from the environment, in the S3 client's standard variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`, `AWS_ENDPOINT` for stores other than AWS, …). They are never written to the home config or to a replica. When one is missing, opening the replica fails with a `usage` error that names it.
- **`init <location>` and `space add <location>`** take either form, plus `--blobs <location>`. The blob location belongs to a machine's registration of the replica, not to the space. Every machine that registers the replica must give the same blob location.
- **The index and the feed cursors** of a replica live in `$KNOWA_HOME/index/<s_id>/<key>/`. `<key>` is the first 16 characters of b32(sha256(canonical location)), where the canonical location is the absolute path of a directory or the URL without a trailing `/`. Two registrations of one location on one machine therefore share one index.
- **Object-store blob bytes** staged for upload or downloaded for reading are cached in `$KNOWA_HOME/cache/blobs/`. *(later: cache eviction.)*

### 5.3 Writing

- **Write path:**
  1. Take the actor's write exclusivity.
  2. Bring the local index up to date for that actor.
  3. Read the actor's head (last seq and id).
  4. Build, validate and sign the claims.
  5. Append them as one batch.
  6. Apply them to the local index.

  Exclusivity is **per actor**. Writers for different actors never wait for each other, and readers wait only for a writer of the actor whose log they are reading.
  A writer whose local index must be rebuilt rebuilds it before taking its actor locks, because a rebuild reads every actor's log and may wait for other writers. The status a write returns is computed by this machine's index, which may not yet hold claims written on other machines; it converges when they are caught up.
- **Lock order:** a writer holds its actor lock and then takes the index lock. A reader therefore never waits for an actor lock while it holds the index lock. It reads an actor's new lines first, releases that actor's lock, and then applies the lines under the index lock, skipping any (actor, seq) already applied.
- **Directory.** Exclusivity is an exclusive advisory lock on `log/<actor>/.lock`.
  - A batch is appended to the actor's newest segment and fsynced before the write returns. Segments rotate at 10000 lines.
  - A reader takes the same lock **shared** while reading the actor's new lines. It can therefore never read a batch that is still being written, or one that a failed write will truncate.
  - A write that fails part-way is truncated back to the state before it.
  - Torn-tail recovery: an unterminated final line found under the exclusive lock is an unacknowledged write left by a crash, and it is truncated.
- **Object store.** There is no lock.
  - A batch is one new segment object, `log/<actor>/<first seq>.jsonl`, created only if absent (`If-None-Match: *`). Two writers racing for an actor's next seq create the same key, and exactly one succeeds.
  - The loser reads the head again, rebuilds and re-signs its claims, and retries. After 3 attempts in total it fails with `conflict`.
  - Segment objects are never modified, and nothing rotates. *(later: compaction of small segments)*
- **One writer per actor is the norm.** Racing writes of one actor stay correct but waste work, so give each agent process its own actor.
- **Blob path.** Bytes are hashed while they are staged. They become visible under an id only once their hash is known and equals that id, so a source that changes during the copy can never be stored under the wrong id.
  - *Directory.* The bytes go to a temp file inside `blobs/`, which is hashed and then renamed to its id. If the id is already present, the temp file is discarded, so each byte sequence is stored once per replica. `put` creates the temp file as a copy-on-write clone of the source where the filesystem supports it (APFS, same volume); elsewhere it is a byte copy.
  - *Object store.* The bytes are first staged into this machine's blob cache (§5.2) while they are hashed, so the upload reads a stable local copy that already has its id. An id already present in the store is not uploaded again; otherwise the staged file is uploaded, in parts when large. Because the key is the hash of the bytes, two machines uploading the same blob at once write identical objects, so blobs need no conditional write. Bytes downloaded into the cache are hashed and must equal their id before they are used.
- **Catalog-only blobs.** A replica may hold claims about a blob whose bytes it does not store. This covers files put with `--no-store`, and blobs left out of a partial import. Such a blob is still identified by its hash, and its `knowa:source` says where the bytes were.
  - `blob.get` is `not_found` for it.
  - It has no full-text content (§7.2).
  - `extract` skips it, because extractors need bytes. Catalog-only files get extractor output at `put` time only.
  - Exports carry its claims but not its bytes.

  `put` without `--no-store` stores the bytes later, and nothing else changes: the id is the same.
- **Logs are never trusted.** Every claim read from a log (catch-up, rebuild, export) is verified, both id and signature, before use. A forged or corrupted line is an `integrity` error.
- **Replicas** may have *gaps* in an actor's seq when imported from a partial bundle. Chain verification covers contiguous runs only, and gaps are reported.

### 5.4 Reading and the local index

- **Catch-up.** On open, a machine brings its index up to date actor by actor, from that actor's last indexed seq, reading only the segments after it. It takes no replica-wide lock, only the index's own. Processes on one machine therefore serialize their catch-up, and nothing else.
- **Rebuild generation.** Catch-up cannot see segments that fill gaps below an indexed head.
  - `merge` (bundle import) writes a fresh `generation` (a new ULID) before its first segment and again after its last.
  - An index records the generation it was built against. It rebuilds when the replica's generation differs.
  - This replaces v0's `.index/needs-rebuild` marker, which only the importing machine could see.
- **Rebuild on mismatch.** An index that is missing or has the wrong schema version is rebuilt and gets a new epoch. A rebuild resets the index under the index lock, then fills it by ordinary catch-up without holding that lock, so a crash part-way is finished by the next catch-up.
- **`verify`** reads every blob in full. On an object store, that downloads the whole store.

## 6. The interface (stable contract) *(v0)*

Five primitives. Everything else is sugar.

| Primitive | In | Out |
|---|---|---|
| **write** | `{space, drafts[]}` with the actor from the environment | `{items:[{id, created, status}], placeholders:{"_:x":"e_…"}}` |
| **blob.put** | bytes | `{id, created, size}`. The media type is not part of the bytes: it arrives as a `knowa:mediaType` claim from the `media` extractor |
| **blob.get** | `{id}` (in any selected space) | bytes |
| **query** | a query document (6.1) | the op's result (6.2) |
| **feed** | `{space, cursor?, limit}` | `{claims:[ClaimView], cursor}` in local arrival order. The cursor is opaque. It records the replica's rebuild generation and, per actor, the highest seq already delivered, so it stays valid across a reindex and on every machine that reads the replica *(experimental, §5)*. If the replica's generation has changed since (a merge filled gaps), the call fails with `cursor_expired` |

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
    "ids": [], "s": [], "p": [], "o": {"<value>":"…"}, "type": ["knowa:Photo"], "notType": ["knowa:File"],
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
- `type` keeps entities that have any of the listed `knowa:type`s; `notType` drops entities that have any of them (an entity with no type is kept). Both count only claims that pass the view, and both apply wherever `type` applies.

### 6.2 Results

- **EntityView** `{id, spaces[], label?, types[], when?:{value, from?, to?, source:"claim"|"file"|"recorded"}, where?:{lat,lon,r?}, score?, distanceKm?, claims: [ids supporting the fields above]}`. `from`/`to` are canonical timestamps; absent means open.
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

Exit **9** is not an error: the command finished but skipped some of its inputs, and its result (printed as usual) lists them in `skipped`. Only `put` uses it today.

### 6.4 Stability rules

- The claim format and the query and result documents only evolve **additively**. Readers ignore unknown result fields, and writers never emit unknown claim fields.
- `knowa schema --json` prints the machine-readable contract: ops, document shapes, kernel vocabulary and error codes. This is the one call an agent needs in order to learn the tool.
- The CLI's text output is for humans and is **not** part of the contract. `--json` output is.

## 7. CLI *(v0)*

Global flags (each with an env override) are `--home` (`KNOWA_HOME`, default `~/.knowa`), `--space` (`KNOWA_SPACE`, an alias or path), `--actor` (`KNOWA_ACTOR`, an actor name) and `--json`. A missing current space or actor is a usage error that names the flag or env var to set.

| Verb | Maps to |
|---|---|
| `init <location> --name N [--alias A] [--blobs <location>]` | create space + genesis + actorKind + label; grant the built-in `extract` worker Worker role + autoAccept; register the alias |
| `space ls\|add <location> [--alias] [--readonly] [--blobs <location>]\|rm <alias>\|use <alias>` | registry. A location is a directory or an object-store URL (§5.2) |
| `actor ls\|new <name> --kind human\|agent\|worker\|use <name>\|show` | local keys |
| `write` (JSONL drafts on stdin) | **write** |
| `query [<json>\|-]` | **query** |
| `feed [--cursor C] [--limit N] [--follow]` | **feed** |
| `blob put <file>` / `blob cat <id>` | **blob.put / blob.get** |
| `put <paths…> [--no-extract] [--no-store] [--download-evicted]` | ingest: blob.put plus `knowa:name`/`knowa:source`/`knowa:modified`/`knowa:created` by the actor, then extractors. Times are read before and after the bytes are stored; a file whose size or modification time changed in between is a `conflict` (its bytes and times would not belong together). `--no-store` is **catalog only**: each file is hashed in place and gets the same claims, extractors included (they read the source), but its bytes are not copied into the replica; the result reports `stored` per file. **A file's own failure skips that file, not the run:** a file that changed while being added (`conflict`), vanished (`not_found`), cannot be read, or has a name that is not UTF-8 (`invalid`) goes to the result's `skipped` as `{path, code, message}` and the run continues, exiting 9. Failures of the replica itself (integrity, the lock, writing the space) still stop the run. **Evicted files** (macOS `dataless`: contents left in the cloud, e.g. iCloud "Optimize Mac Storage") are not read by default, because reading one downloads it; each is skipped as `unsupported` with a message saying so. `--download-evicted` reads them, so the OS downloads them first, and their times are read after the download |
| `note <text\|-> [--title T] [--when EDTF] [--about id…]` | write a Note entity |
| `assert <s> <p> <value> [--q p=value…] [--valid] [--src…] [--conf] [--note] [--replaces…]` | write. Value syntax: `text:…`, `term:…`, `ref:…`, `int:…`, `dec:…`, `bool:…`, `when:…`, `geo:lat,lon[,r]`, `qty:12.5:kg`. A bare token is text |
| `retract <claim> [--note]` | write |
| `review accept\|reject [<claim>…] [--by A…] [--about ID…] [--p TERM…] [--since T] [--until T] [--note N]` | write one `knowa:review` per target, all in one write. Targets are the listed claims plus, when any selector is given, every **proposed** claim in the current space that matches all selectors: `--by` (author: an `a_` id or a local actor name) ↔ `where.actors`, `--about` ↔ `where.s`, `--p` ↔ `where.p`, `--since`/`--until` (RFC 3339 record time) ↔ `recordedAfter`/`recordedBefore`. Neither a claim nor a selector is a `usage` error: there is no "review everything". Preview a selection with `pending` and the same selectors. A review equal to one the actor already wrote is not written again (`created:false`) |
| `grant <actor> <role> [--auto-accept]` · `share <id> private\|shared\|public` | write (policy) |
| `pending [--by…] [--about…] [--p…] [--since] [--until] [--limit] [--cursor]` | find claims with status proposed; the selectors are `review`'s |
| `search <text> [filters]` · `timeline [--from --to]` · `near <lat,lon\|id> [--km]` | find. `search` and `timeline` take `--type` and `--not-type`, and `--cursor` to continue a page |
| `show <id…>` | get |
| `context <id> [--window P1D] [--km 1] [--hops 1]` | context |
| `path <a> <b>` | path |
| `log [--actor] [--since]` | log |
| `extract [--all]` | run built-in extractors over blobs missing their output |
| `worker run <actor-name> --cmd "<exe args>" [--batch N] [--follow]` | feed → external command → write |
| `export <file.knowa> [--level private\|shared\|public]` · `import <file.knowa> [--alias]` | bundles |
| `verify` · `reindex` · `schema` | admin |
| `serve [--port N]` | the local web UI (§7.4) |

### 7.1 Workers *(v0)*

`worker run` reads the feed from a cursor stored with the machine-local index, in `$KNOWA_HOME/index/<s_id>/<key>/cursors/<actor>.json` (§5.2). For each batch it executes `--cmd`, with this JSON on stdin:
`{"space":"s_…","cursor":"…","claims":[ClaimView…]}`
and with `KNOWA_HOME`, `KNOWA_SPACE` and `KNOWA_ACTOR` set, so the command can call `knowa query` itself. The command's stdout is JSONL drafts, which are written under the worker's actor with `via.worker = <actor-name>`. A non-zero exit stops the run and does **not** advance the cursor. Any program can be a worker, and LLM agents are just one kind of worker. No LLM provider is linked into the core.

### 7.2 Built-in extractors *(v0)*

Built-in extractors are pure Rust with no external tools. They write as the `extract` worker actor, which is auto-created in the home directory and granted Worker + autoAccept at `init`. Each extractor has a version, recorded as `via.worker = "<name>@<n>"`.

- **media@1**: `knowa:mediaType` (sniffed from content first, extension second), `knowa:size`, and `knowa:type` (`knowa:Photo` for `image/*`, otherwise `knowa:File`).
- **exif@1**: finds the EXIF/TIFF block in JPEG, HEIC/HEIF, PNG (`eXIf`) and TIFF files. Writes `knowa:when` from DateTimeOriginal (+ OffsetTimeOriginal when present, otherwise local-unknown), `knowa:where` from GPS, and `exif:make`, `exif:model`, `exif:width`, `exif:height`, `exif:orientation`.

Text blobs (`text/*`, ≤ 1 MiB) are indexed for full-text search directly from the blob store. No claims are needed, because the text is derived. A blob the replica does not store (catalog only, §5) has no indexed content.

### 7.3 Sharing *(v0)*

- `export --level L` (shared or public) exports these **claims**:
  - (a) non-policy claims whose effective visibility is ≥ L. Effective visibility is the claim's own visibility setting, else its subject's, else the space default, else Private;
  - (b) **trust claims**: `owner`, `role`, `autoAccept`, `actorKind`, `defaultVisibility`. These are always exported, so the receiver can compute statuses, which means their `note`s are effectively public;
  - (c) `review` / `retract` claims whose target is exported.
  `visibility` claims are not exported.
- A **blob's bytes** are exported iff the blob's own effective visibility (a visibility claim on its `b_` id, else the space default) is ≥ L. Being referenced by an exported claim is not enough, and a reference to an unexported blob stays dangling.
- The bundle is marked `"partial":true` unless the level is `private`, which exports everything.
- A space is identified by its space id **and** genesis id. `import`, and registering a second replica of a known space id, refuse with `conflict` when the owner or genesis differ. Every query that selects spaces requires replicas sharing a space id to have identical manifests.
- A bundle is a tar archive containing `bundle.json` (`{format, space, owner, genesis, level, partial, exportedBy, at}`), the `log/…` segments and `blobs/…`.
- `import` verifies every signature, blob hash and chain run, then merges into a replica directory (creating it if needed), registers it read-only, and reindexes.
- Readonly replicas refuse local writes. To annotate someone else's content, write in **your** space and refer to their ids.

### 7.4 Web UI *(experimental)*

> **Experimental** until it has been used in a browser by its owner: views and endpoints may change. The HTTP API is the five primitives' documents over HTTP; it adds no new semantics.

`knowa serve [--port N]` runs a web UI over the home directory's spaces, for this machine only.

- **Local only.**
  - The server binds `127.0.0.1`. The default port is 4747; `--port 0` picks a free one.
  - It prints its URL with a random 256-bit token in the fragment: `http://127.0.0.1:<port>/#k=<token>`. The app sends the token as `Authorization: Bearer <token>` on every API call. A fragment is never sent to a server, so the token does not appear in request logs.
  - An `/api/` request without the right token gets 401.
  - A request whose `Host` is not `127.0.0.1:<port>` or `localhost:<port>` gets 403, which defeats DNS rebinding.
  - The server sends no CORS headers.
  - It serves nothing from disk except its embedded assets and blob bytes by id.
- **API.** JSON in and out, with the same documents as `--json`. An error is the §6.3 error object, with an HTTP status by code:

  | Code | HTTP status |
  |---|---|
  | `usage`, `invalid` | 400 |
  | `not_found` | 404 |
  | `conflict` | 409 |
  | `cursor_expired` | 410 |
  | `unsupported` | 422 |
  | `integrity`, `internal` | 500 |

  | Endpoint | Maps to |
  |---|---|
  | `GET /api/schema` | `schema` |
  | `GET /api/spaces` | the registry: `[{alias, space, readonly, current}]` |
  | `POST /api/query` | **query** (a query document) |
  | `GET /api/feed?space=&cursor=&limit=` | **feed** |
  | `GET /api/blob/<b_…>` | **blob.get** |
  | `POST /api/review` `{"space"?, "decision":"accept"\|"reject", "claims":["c_…"], "note"?}` | `review` with explicit claims only: the UI reviews exactly the claims it shows. It is the only write, and it is signed by the serving actor (`--actor` / `KNOWA_ACTOR`). Without one, the UI is read-only, and this endpoint is a `usage` error naming the variable to set |

- **Blob bytes are untrusted content**, because a blob can be HTML or SVG with scripts. Every blob response carries:
  - `Content-Security-Policy: sandbox; default-src 'none'; img-src 'self'; style-src 'unsafe-inline'`;
  - `X-Content-Type-Options: nosniff`;
  - a `Content-Type` chosen as follows: the accepted `knowa:mediaType` when it is `image/jpeg`, `image/png`, `image/gif`, `image/webp`, `image/avif` or `text/plain` (sent as `text/plain; charset=utf-8`); otherwise `application/octet-stream` with `Content-Disposition: attachment`.

  SVG and HTML are never rendered inline.
- **The app** is embedded in the binary: plain HTML, CSS and JavaScript modules, with no build step and no third-party code. It makes no request outside its own origin: no web fonts, CDNs, analytics or map tiles. It works offline and leaks nothing.
- **Views:**
  - **Search.** Free text, type and not-type filters, and the view (statuses; actor kinds). It returns entities.
  - **Entity**, built on the `context` op:
    - label, types, `when` with its source, and `where`;
    - its claims, grouped by predicate. Each claim shows status, stale, author and author kind, `via`, `conf` and `note`, and links to its `src`;
    - inbound claims, neighbors, and temporal neighbors;
    - a blob entity also shows its image, or text up to 1 MiB.

    Provenance is part of every claim, not a separate page.
  - **Timeline.** Entities by `when`, grouped by period. A visible toggle hides files (`notType: knowa:File`).
  - **Gallery.** `knowa:Photo` entities as a grid, ordered by `when`.
  - **Review.** Proposed claims, grouped by author, then by `via.session` (or by record day when there is none), then by subject. Each group shows its claims in full. Accept all and Reject all act on exactly those claims, and each claim can also be reviewed on its own.
  - Map and graph views are *(later)*. A map needs tiles from a third party, which would leak locations.

## 8. Implementation (invisible) *(v0)*

- Rust (edition 2024), single static binary `knowa`. Every public API is synchronous. The object-store backend runs its client on a private current-thread tokio runtime and blocks on it; no async type crosses a crate boundary.
- Crates (Cargo workspace, `crates/*`):

| Crate | Owns | Third-party deps |
|---|---|---|
| `knowa-model` | ids, values, claims, JCS, EDTF, signing, validation, kernel vocab, the query/result contract types, the shared `Error` | serde, serde_json, ed25519-dalek, sha2, data-encoding, getrandom |
| `knowa-store` | the replica: manifest, logs, blobs, locks, verify, bundles; directory and object-store backends (§5) | tar, sha2, object_store (aws), tokio (rt) |
| `knowa-index` | the derived SQLite index of one replica: apply, status/stale, FTS5, geo, edges, arrival feed, single-replica reads | rusqlite (bundled) |
| `knowa-extract` | media sniffing, EXIF, the extractor registry | infer, kamadak-exif |
| `knowa-config` | home: actor keys, space registry | — |
| `knowa-api` | the five primitives and the admin ops. Composes store + index + extract across spaces: fan-out, merge, context, path | — |
| `knowa-web` | the local web UI (§7.4): HTTP server, routes over the primitives, the embedded app | tiny_http |
| `knowa-cli` | the `knowa` binary: argument parsing and rendering only | clap |

- The index is one SQLite file per replica per machine, kept under the home directory (§5.2). Multi-space queries fan out to each replica's index and merge in `knowa-api`.
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
  - map and graph views in the web UI (§7.4)
  - GPX and Takeout connectors
  - PDF/OCR/ASR extractors
  - LLM summary and insight workers
  - per-worker budgets
  - golden-question retrieval evals
  - an OKF (markdown) round-trip

## 10. Known risks

- Local key separation is a convention: an agent on the same machine could use a human's key. *(later: OS keychain + per-actor prompts.)*
- Partial replicas have seq gaps, so their chain verification is weaker.
- Object-store replicas and directory replicas shared by several machines are experimental (§5): the conditional-write and shared-lock behaviour they rely on is untested against real S3 and JuiceFS.
- SQLite with one index per replica is fine up to roughly 10^7 claims per space. Beyond that, the `index.Index` boundary is where a new engine plugs in.
