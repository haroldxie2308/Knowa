//! `knowa schema --json` (DESIGN §6.4): the one document an agent reads to learn the
//! tool. Hand-written; it must stay accurate to DESIGN and to this crate's behavior.

use knowa_model::query::{DEFAULT_LIMIT, MAX_HOPS, MAX_LIMIT, MAX_PATH_HOPS};
use knowa_model::value::{MAX_SAFE_INT, MAX_TEXT_BYTES};
use knowa_model::{ErrorCode, vocab};
use serde_json::{Value, json};

use crate::SKIPPED_EXIT;
use crate::context::MAX_TEMPORAL_SCAN;
use crate::path::{MAX_SEARCH_STEPS, MAX_VISITED};

pub(crate) fn schema() -> Value {
    json!({
        "name": "knowa",
        "version": env!("CARGO_PKG_VERSION"),
        "formatVersion": vocab::FORMAT_VERSION,
        "claimVersion": vocab::CLAIM_VERSION,
        "overview": "Knowa is a local-first knowledge base of signed, append-only claims. Humans and agents write claims (subject, predicate, typed value) under their own actor keys; everything else (entities, status, search, graph) is derived at read time. Five primitives: write, blob.put, blob.get, query, feed. The claim format and the query/result documents only change additively: ignore unknown result fields.",
        "selection": selection(),
        "primitives": primitives(),
        "admin": admin(),
        "ids": ids(),
        "terms": "A term is ns:name with ns [a-z][a-z0-9-]* and name [A-Za-z][A-Za-z0-9_.-]*. Predicates and enum-like values are terms. The knowa namespace is reserved; any other namespace is free (e.g. exif:make).",
        "values": values(),
        "time": time(),
        "claim": claim(),
        "draft": draft(),
        "query": query(),
        "results": results(),
        "vocabulary": vocabulary(),
        "status": status(),
        "errors": errors(),
    })
}

fn selection() -> Value {
    json!({
        "space": "Writes, blob.put, feed and admin ops act on one space: --space / KNOWA_SPACE (alias or replica path), else the current space (knowa space use). Missing → usage.",
        "actor": "Writes are signed by the selected actor: --actor / KNOWA_ACTOR (actor name), else the current actor (knowa actor use). Missing → usage.",
        "readonly": "Spaces registered readonly (e.g. imported bundles) refuse writes with usage. To annotate someone else's content, write in your own space and refer to their ids.",
        "querySpaces": "A query document's spaces field: [] (default) = the selected space; [\"*\"] = every registered space (cannot be combined with other entries); otherwise aliases or replica paths. Several replicas of the same space count once (the first wins). A space is its id and genesis: selected replicas that share a space id but differ in owner or genesis → conflict."
    })
}

fn primitives() -> Value {
    json!({
        "write": {
            "in": {"drafts": "Draft[] (CLI: JSONL on stdin, one draft per line)"},
            "out": {"items": [{"id": "c_…", "created": "bool", "status": "status after the write"}], "placeholders": {"_:name": "e_…"}},
            "rules": [
                "All-or-nothing: every draft is validated and signed before anything is appended.",
                "items are in draft order, one per draft.",
                "Placeholders _:name may stand for an id in s, o.ref, q refs and src (not replaces). Each distinct placeholder becomes one fresh e_ entity id for the whole batch; the mapping is returned.",
                "Idempotent: a draft whose {s,p,o,q,valid} equals an active (accepted or proposed), non-stale claim of the same actor in the space returns that claim with created:false and appends nothing; superseded, rejected, retracted or stale equivalents do not block a new claim. A repeated draft within one batch returns the first one's id with created:false.",
                "The first time an actor writes in a space, a knowa:actorKind claim declaring its kind is prepended automatically (it is not listed in items). Only the system writes knowa:actorKind: a draft with that predicate → invalid.",
                "The system assigns v, space, actor, seq, prev, at, id and sig."
            ]
        },
        "blob.put": {
            "in": "bytes",
            "out": {"id": "b_…", "created": "bool (false if the bytes were already stored)", "size": "bytes"},
            "rules": ["Blobs are content-addressed and immutable. Describe them with claims (knowa put does this for files)."]
        },
        "blob.get": {
            "in": {"id": "b_… (a #fragment is ignored)", "spaces": "as in a query document"},
            "out": "the bytes",
            "rules": ["not_found if no selected space stores the blob."]
        },
        "query": {"in": "a query document (see query)", "out": "the op's result (see results)"},
        "feed": {
            "in": {"cursor": "opaque, as a previous feed returned it, or absent to start from the beginning", "limit": format!("1..{MAX_LIMIT}")},
            "out": {"claims": "ClaimView[] the cursor has not delivered, in this machine's local arrival order (all statuses)", "cursor": "opaque; pass it back to continue (unchanged when nothing new arrived)"},
            "rules": [
                "The cursor records the replica's rebuild generation and, per actor, the highest seq delivered, so it stays valid across reindex and on every machine that reads the replica; it delivers every claim exactly once, whichever of those indexes serves each page.",
                "A malformed cursor → invalid.",
                "If the replica's rebuild generation changed since the cursor was issued (a merge filled gaps in its logs) → cursor_expired: restart from the beginning without a cursor; writes are idempotent."
            ]
        }
    })
}

fn admin() -> Value {
    json!({
        "init": {
            "args": {"dir": "new or empty directory", "name": "space label (non-empty)", "alias": "registry alias, default the directory name (must match [a-z0-9][a-z0-9._-]{0,63})"},
            "does": "Creates the space: genesis knowa:owner by the selected actor, its knowa:actorKind, knowa:label = name on the space id, knowa:role knowa:Worker and knowa:autoAccept true for the built-in extract actor. Registers the alias; makes it current if no space is current.",
            "out": {"space": "s_…", "path": "absolute path", "alias": "string", "owner": "a_…", "extractActor": "a_…", "claims": "ids of every claim written"}
        },
        "put": {
            "args": {"paths": "files or directories", "noExtract": "skip extractors", "noStore": "catalog only: record the files without copying their bytes into the replica", "downloadEvicted": "read evicted files (macOS dataless, e.g. iCloud Optimize Mac Storage) so the OS downloads them first; without it they are skipped"},
            "does": "Directories are walked recursively; names starting with . are not walked and symlinks met during a walk are not followed (explicitly named paths are). Per file: blob.put, then knowa:name, knowa:source (absolute path), knowa:modified and knowa:created (when the OS reports one; whole seconds UTC) on the blob by the selected actor, then the built-in extractors, written by the extract actor with via.worker = <extractor>. A file's own failure skips that file, not the run: one whose size or modification time changed while being stored (conflict), that vanished (not_found), that cannot be read, or whose name is not UTF-8 or that is no longer a regular file (invalid) goes to skipped, the other files are still added, and the process exits 9. Failures of the replica itself (integrity, the lock, writing the space) stop the run; files already added stay, and a rerun is idempotent. Evicted files (macOS dataless: contents left in the cloud) are not read, because reading one downloads it: each is skipped as unsupported, unless downloadEvicted is set, which reads it so the OS downloads it and then takes its times. Extractor failures, and file times with no EDTF form, are reported per file and do not stop the run. With noStore each file is hashed in place and gets the same claims, extractors included (they read the source file, and the size and modification time check runs after them), but its bytes are not copied: blob.get is not_found for it, extract skips it, and a later put without noStore stores the bytes under the same id.",
            "out": {"items": [{"path": "absolute path", "blob": "b_…", "created": "bool", "stored": "bool: the replica holds the file's bytes after this item (always true without noStore; with noStore true only if an earlier put stored them)", "size": "bytes", "claims": "ids of the claims recording this file (including existing ones matched idempotently)", "errors": "per-file problems that did not stop the run: \"<worker>: <message>\" per failed extractor, or a file time with no EDTF form"}], "skipped": [{"path": "absolute path", "code": "error code of the file's own failure: conflict, not_found, invalid, unsupported (evicted), internal (another I/O failure reading it)", "message": "that failure's message"}]}
        },
        "review": {
            "args": {
                "decision": "accept | reject",
                "claims": "c_…[] to review (optional)",
                "by": "author filter: a_… ids or local actor names (selector, where.actors)",
                "about": "subject ids (selector, where.s)",
                "p": "predicate terms (selector, where.p)",
                "since": "RFC 3339 record-time lower bound, inclusive (selector, where.recordedAfter)",
                "until": "RFC 3339 record-time upper bound, exclusive (selector, where.recordedBefore)",
                "note": "rationale, written on every review"
            },
            "does": "Writes one knowa:review (knowa:Accept or knowa:Reject) per target, all in one write by the selected actor. Targets are the listed claims plus, when any selector is given, every proposed claim in the current space matching all selectors, oldest first; duplicates count once. Neither a claim nor a selector → usage: there is no review of everything. Preview a selection with pending and the same selectors. A review equal to an active one the actor already wrote is not written again (created:false); no target left → nothing is written.",
            "out": "as write: {items: [{id, created, status}], placeholders: {}}, one item per target"
        },
        "extract": {
            "args": {"all": "re-run every extractor on every blob (idempotent)"},
            "does": "Runs the built-in extractors on stored blobs that have no claim from them yet.",
            "out": {"blobs": "blobs processed", "claims": "claims newly created", "errors": "\"<blob>: <worker>: <message>\""},
            "extractors": {
                "media@1": "knowa:mediaType (sniffed from content first, file name second), knowa:size, knowa:type (knowa:Photo for image/*, else knowa:File)",
                "exif@1": "for JPEG/HEIC/HEIF/PNG/TIFF/WebP: knowa:when from DateTimeOriginal (+OffsetTimeOriginal), knowa:where from GPS, exif:make, exif:model, exif:width, exif:height, exif:orientation"
            }
        },
        "worker run": {
            "args": {"actor": "an existing actor of kind agent or worker (else usage)", "cmd": "program and arguments", "batch": format!("feed page size, 1..{MAX_LIMIT}"), "follow": "keep polling every 2 s when caught up"},
            "does": "Reads the feed from the cursor stored with this machine's index of the replica, in <home>/index/<space-id>/<location key>/cursors/<actor>.json, skipping claims written by the worker actor itself. For each batch it runs cmd with stdin {\"space\":\"s_…\",\"cursor\":\"the feed cursor after this batch\",\"claims\":[ClaimView…]} and env KNOWA_HOME, KNOWA_SPACE (replica path), KNOWA_ACTOR. The command's stdout is JSONL drafts, written by the worker actor with via.worker = <actor> (a draft naming a different via.worker → invalid). A non-zero exit stops the run (error detail carries stderr) and the cursor is not advanced. The stored cursor survives reindex. cursor_expired (a merge changed the replica's rebuild generation) names the cursor file to delete.",
            "out": {"batches": "commands run", "claimsIn": "claims passed to the command", "claimsOut": "claims newly created", "cursor": "final cursor"}
        },
        "export": {
            "args": {"out": "bundle file", "level": "knowa:Private (everything) | knowa:Shared | knowa:Public"},
            "does": "Writes a tar bundle (bundle.json, log segments, blobs). Below private it contains: non-policy claims whose effective visibility (claim's own, else its subject's, else the space default) is at least the level; trust claims (owner, role, autoAccept, actorKind, defaultVisibility) always, so their notes are effectively public; review/retract claims whose target is exported; no visibility claims. Blob bytes are included only when the blob's own effective visibility is at least the level. Such bundles are partial.",
            "out": {"path": "absolute path", "level": "visibility term", "partial": "bool", "claims": "count", "blobs": "count"}
        },
        "import": {
            "args": {"bundle": "bundle file", "dir": "replica directory, default <home>/replicas/<space-id>", "alias": "default: the space label slugified, else the space id"},
            "does": "Verifies every signature, blob hash and chain run, and that the genesis is a knowa:owner claim by the owner, merges into the replica (creating it) under the locks of the actors it writes, writes a new rebuild generation if it added anything (so every machine's index of the replica rebuilds), reindexes it and registers it readonly. Importing into a directory registered as a writable space → conflict. A space is its id and genesis: if a registered replica of the same space id has a different owner or genesis → conflict, and nothing is written (space add applies the same rule).",
            "out": {"space": "s_…", "path": "absolute path", "alias": "string", "added": "claims added", "skippedExisting": "claims already present", "blobsAdded": "count", "gaps": "number of missing seq runs (partial bundles)"}
        },
        "verify": {"does": "Under each actor's write lock in turn (taking it first truncates a torn final line left by a crashed append), checks every claim of that actor (id, signature, space, chain within contiguous runs, unique seq); then every blob hash and the genesis. Problems → integrity with the list in detail.", "out": {"claims": "count", "blobs": "count", "actors": "count", "gaps": "[[actor, first missing seq, last missing seq]]", "problems": "[]"}},
        "reindex": {"does": "Deletes and rebuilds this machine's index of the space from its logs; feed cursors stay valid.", "out": "the new index epoch"},
        "schema": {"does": "Prints this document."}
    })
}

fn ids() -> Value {
    json!({
        "claim": "c_ + base32(multihash sha2-256 of the claim's canonical bytes)",
        "blob": "b_ + base32(multihash sha2-256 of the bytes); in s, o.ref and src positions it may carry #xywh=x,y,w,h | #t=start,end | #page=n | #char=start,end",
        "actor": "a_ + base32(0xed01 ‖ ed25519 public key)",
        "entity": "e_ + lowercase ULID",
        "space": "s_ + lowercase ULID",
        "external": "x:<ns>:<key>, a well-known global key such as x:wd:Q42 or x:url:https://…",
        "placeholder": "_:name, only inside one write batch",
        "encoding": "base32 is RFC 4648, lowercase, no padding. Ids are opaque."
    })
}

fn values() -> Value {
    json!({
        "rule": "A value is a JSON object with exactly one discriminator key plus its allowed modifier; anything else is invalid.",
        "forms": {
            "text": {"json": {"text": "…", "lang": "en"}, "rule": format!("at most {MAX_TEXT_BYTES} bytes UTF-8; lang optional (BCP 47). Larger content goes in a blob.")},
            "term": {"json": {"term": "ns:Name"}},
            "ref": {"json": {"ref": "<any id>"}, "rule": "b_ ids may carry a fragment"},
            "int": {"json": {"int": 42}, "rule": format!("|n| ≤ {MAX_SAFE_INT}")},
            "dec": {"json": {"dec": "12.50"}, "rule": "string -?[0-9]+(\\.[0-9]+)?; floats never appear in claims"},
            "bool": {"json": {"bool": true}},
            "when": {"json": {"when": "<EDTF>"}, "rule": "see time"},
            "geo": {"json": {"geo": {"lat": "35.68", "lon": "139.76", "alt": "40", "r": "25"}}, "rule": "WGS84 decimal-degree strings; alt (m) and r (uncertainty radius, m) optional"},
            "qty": {"json": {"qty": "12.5", "unit": "kg"}, "rule": "UCUM unit string"}
        }
    })
}

fn time() -> Value {
    json!({
        "recordTime": "at: RFC 3339 UTC with exactly milliseconds, 2006-01-02T15:04:05.000Z. Assigned by the system.",
        "worldTime": "valid and knowa:when use an EDTF subset: YYYY, YYYY-MM, YYYY-MM-DD; YYYY-MM-DDThh:mm[:ss[.fff]] with Z or ±hh:mm (without an offset: local time, zone unknown, range widened by ±14h); unspecified digits 19XX / 199X; qualifiers ? (uncertain) ~ (approximate) % (both) on dates; intervals A/B, A/.., ../B. Each indexes as a half-open range [lo, hi).",
        "durations": "context window: ISO 8601 PnW, PnD, PTnH, PTnM and combinations like P1DT12H; years and months are rejected."
    })
}

fn claim() -> Value {
    json!({
        "fields": {
            "v": "1", "space": "s_…", "actor": "a_…", "seq": "per-actor sequence in this space, from 1",
            "prev": "id of the actor's previous claim in this space (present iff seq > 1)", "at": "record time",
            "s": "subject: any id", "p": "predicate term", "o": "value",
            "q": "qualifiers {term: [value, …]}", "valid": "EDTF world time the claim holds",
            "conf": "dec in [0,1]", "src": "ids this claim was derived from", "replaces": "claim ids this claim supersedes",
            "via": "{tool, model, session, worker, …}: string values, keys [a-z][a-z0-9_]*", "note": "rationale, at most 4 KiB",
            "id": "c_…", "sig": "Ed25519 signature, base64url without padding"
        },
        "rules": [
            "Required: v, space, actor, seq, at, s, p, o, id, sig. Optional fields are omitted when empty; empty objects or arrays are invalid.",
            "Claims are immutable and append-only. Change is a new claim: replaces, knowa:retract or knowa:review.",
            "Canonical bytes are RFC 8785 JSON of the claim without id and sig; id hashes them, sig signs \"knowa/claim/v1\\n\" ‖ them."
        ]
    })
}

fn draft() -> Value {
    json!({
        "fields": "s, p, o (required); q, valid, conf, src, replaces, via, note (optional, omitted when empty)",
        "example": {"s": "_:n", "p": "knowa:label", "o": {"text": "Trip to Kyoto"}},
        "rules": ["Envelope fields (v, space, actor, seq, prev, at, id, sig) and unknown fields → invalid.", "Derived claims should cite their inputs in src: when an input is retracted, rejected or superseded, the derived claim becomes stale."]
    })
}

fn view() -> Value {
    json!({
        "status": {"type": "status[]", "default": ["accepted", "proposed"]},
        "actorKinds": {"type": "[knowa:Human | knowa:Agent | knowa:Worker]", "default": "any"},
        "actors": {"type": "a_…[]", "default": "any"},
        "includeStale": {"type": "bool", "default": true},
        "minConf": {"type": "dec string in [0,1]", "default": "none; claims without conf count as 1"}
    })
}

fn query() -> Value {
    json!({
        "document": "{\"op\": \"find|get|context|path|log\", …fields of that op}. Each op accepts only its own fields; any other field → invalid.",
        "common": {
            "spaces": {"type": "string[]", "default": "[] = selected space", "rule": "[\"*\"] = all registered spaces; entries are aliases or replica paths"},
            "view": {"fields": view(), "rule": "trust filter over claims; not accepted by log"},
            "asOf": {"rule": "reserved for record-time travel: any non-null value → unsupported (not accepted by log)"}
        },
        "ops": {
            "find": find_op(),
            "get": {
                "fields": {"ids": {"type": "id[]", "required": true, "rule": "non-empty"}, "spaces": "common", "view": "common", "asOf": "common"},
                "result": "{items: GetItem[]}, one item per id in order",
                "rules": [
                    "c_ id → {kind: claim, claim: ClaimView}, returned whatever the view.",
                    "b_ id → {kind: entity, …} when claims about it pass the view (its claims include media type and size), else {kind: blob, blob: BlobView} when its bytes are stored.",
                    format!("other ids → {{kind: entity, entity: EntityView, claims: ClaimView[] with s = id passing the view, record time ascending}}; more than {MAX_LIMIT} such claims → unsupported (use find with target claims)."),
                    "Any id not found in the selected spaces → not_found; detail.missing lists them."
                ]
            },
            "context": {
                "fields": {
                    "id": {"type": "id", "required": true},
                    "hops": {"type": "int", "default": 1, "max": MAX_HOPS, "rule": "graph neighborhood depth (0 = none)"},
                    "window": {"type": "duration", "default": "P1D", "rule": "temporal neighbors overlap the entity's when widened by this on both sides"},
                    "km": {"type": "dec string", "default": "1", "rule": "spatial radius, ≥ 0"},
                    "limit": {"type": "int", "default": 20, "max": MAX_LIMIT, "rule": "cap per section"},
                    "spaces": "common", "view": "common", "asOf": "common"
                },
                "result": "ContextResult",
                "rules": [
                    "claims: s = id; inbound: o.ref = id; both pass the view, record time ascending, capped at limit.",
                    "neighbors: breadth-first over ref edges (both directions, policy predicates excluded) up to hops; each edge into a newly reached node is listed with its claim (via), predicate, direction and hop. Nodes no space describes appear with only their id.",
                    format!("temporal: entities whose when overlaps the anchor's range widened by window, excluding the anchor, nearest center first; empty if the anchor's when is open-ended or absent. More than {MAX_TEMPORAL_SCAN} overlapping entities → unsupported (narrow the window)."),
                    "spatial: entities within km of the anchor's where, excluding the anchor, nearest first; empty if the anchor has no where.",
                    "actors: authors of the listed claims and inbound claims, with kind, role and autoAccept from the first space that knows them; claims counts their claims shown here.",
                    "An id nothing describes or references → not_found."
                ]
            },
            "path": {
                "fields": {
                    "from": {"type": "id", "required": true}, "to": {"type": "id", "required": true, "rule": "must differ from from"},
                    "maxHops": {"type": "int", "default": 4, "min": 1, "max": MAX_PATH_HOPS},
                    "limit": {"type": "int", "default": 3, "max": MAX_LIMIT, "rule": "max paths"},
                    "spaces": "common", "view": "common", "asOf": "common"
                },
                "result": "{paths: PathStep[][]}",
                "rules": [
                    "Simple paths over ref edges of all selected spaces, traversed in either direction; shortest first, at most limit, at most maxHops edges. No path → paths: [].",
                    "from and to must each be described or referenced in a selected space, else not_found.",
                    format!("Exploring more than {MAX_VISITED} nodes, or more than {MAX_SEARCH_STEPS} enumeration steps → unsupported (never a silently truncated answer).")
                ]
            },
            "log": {
                "fields": {
                    "actors": {"type": "a_…[]", "default": "all"},
                    "since": {"type": "RFC 3339", "rule": "record-time lower bound, inclusive"},
                    "limit": {"type": "int", "default": DEFAULT_LIMIT, "max": MAX_LIMIT},
                    "cursor": {"type": "string", "rule": "single space only"},
                    "spaces": "common"
                },
                "result": "{items: ClaimView[], cursor?}",
                "rules": ["The raw audit trail by record time ascending, every status, no view.", "Multi-space log with a cursor → unsupported."]
            }
        }
    })
}

fn find_op() -> Value {
    json!({
        "fields": {
            "target": {"type": "entities | claims", "default": "entities"},
            "where": {
                "rule": "Conjunctive: every present field must match. For target entities, the claim-level filters (s, p, o, actors, recordedAfter, recordedBefore) match if one claim about the entity satisfies all of them together with the view.",
                "fields": {
                    "ids": "id[]", "s": "subject id[]", "p": "predicate term[]", "o": "value (exact match)",
                    "type": "term[]: entity has knowa:type with any of these",
                    "notType": "term[]: entity has knowa:type with none of these; an entity without a type is kept. Like type, only type claims passing the view count, and it applies wherever type does",
                    "text": "free text; tokens all match, the last token as a prefix (no FTS syntax)",
                    "when": "{from?: EDTF, to?: EDTF}: entity when overlaps [from.lo, to.hi)",
                    "near": "{lat, lon, km} decimal strings",
                    "nearId": "{id, km}: near the id's knowa:where (looked up across the selected spaces; none → invalid). Exclusive with near.",
                    "actors": "a_…[]", "recordedAfter": "RFC 3339", "recordedBefore": "RFC 3339"
                }
            },
            "order": {
                "default": "relevance",
                "values": {
                    "relevance": "text relevance (score, higher first) when where.text is set; otherwise -when for entities and -recorded for claims",
                    "when": "entity world time ascending", "-when": "descending",
                    "recorded": "record time ascending", "-recorded": "descending",
                    "distance": "nearest first; requires near or nearId (else invalid)"
                }
            },
            "limit": {"type": "int", "default": DEFAULT_LIMIT, "max": MAX_LIMIT},
            "cursor": {"type": "string", "rule": "from the previous page; single space only"},
            "spaces": "common", "view": "common", "asOf": "common"
        },
        "result": "{items: EntityView[] | ClaimView[], cursor?}",
        "multiSpace": [
            "A cursor with several selected spaces → unsupported: use a larger limit instead (no cursor is returned).",
            "Entities found in several spaces are merged by id: spaces, types and supporting claims unioned; label, when and where from the first space (in selection order) that has them; best score.",
            "Mergeable orders: entities by relevance, when, -when, distance; claims by recorded, -recorded, or relevance without text. Other combinations → unsupported."
        ]
    })
}

fn results() -> Value {
    json!({
        "EntityView": {
            "id": "id", "spaces": "s_…[] that describe it", "label": "display label: the display-chosen knowa:label, else the display-chosen knowa:name (omitted if neither)", "types": "term[]",
            "when": "{value: EDTF as written or the record timestamp, from: timestamp|null, to: timestamp|null (half-open, null = open), source: claim|file|recorded}; file means no knowa:when exists and the chosen knowa:modified stands in (accepted before proposed, then the earliest start, then claim id); recorded means neither exists, the entity is a knowa:Note and the earliest record time of its claims stands in. Any other entity without a knowa:when or knowa:modified has no when: it is not on the timeline and matches no when filter",
            "where": "{lat, lon, r?} numbers", "score": "text relevance (find with text)", "distanceKm": "distance (near queries)",
            "claims": "ids of the claims that supplied label, types, when and where",
            "displayRule": "Views apply to evidence: display fields come only from claims that pass the query view, and so does matching. Among them a single shown value is accepted before proposed, then latest at."
        },
        "ClaimView": "the full claim JSON plus status and stale",
        "BlobView": {"id": "b_…", "size": "bytes", "mediaType": "if known", "spaces": "s_…[] storing the bytes"},
        "GetItem": "{kind: entity, entity, claims} | {kind: claim, claim} | {kind: blob, blob}",
        "ContextResult": {"entity": "EntityView", "claims": "ClaimView[]", "inbound": "ClaimView[]", "neighbors": "[{entity: EntityView, via: c_…, p: term, dir: out|in, hop: int}]", "temporal": "EntityView[]", "spatial": "EntityView[]", "actors": "ActorInfo[]"},
        "ActorInfo": {"id": "a_…", "kind": "knowa:Human|knowa:Agent|knowa:Worker (if declared)", "role": "knowa:Owner|Member|Agent|Worker|Viewer (if granted)", "autoAccept": "bool", "claims": "count"},
        "PathStep": {"id": "node id", "via": "claim linking the previous step to this one (absent on the first step)", "p": "its predicate", "dir": "out if the claim points from the previous step to this one, in otherwise"},
        "citations": "Every result carries the claim ids that support it (EntityView.claims, ClaimView.id, via)."
    })
}

fn pred(s: &str, o: &str, rule: &str) -> Value {
    json!({"s": s, "o": o, "effectiveOnlyIf": rule})
}

fn vocabulary() -> Value {
    let mut policy = serde_json::Map::new();
    policy.insert(vocab::OWNER.into(), pred("space id", "ref actor", "genesis claim: seq 1 of the owner, recorded in the replica manifest"));
    policy.insert(vocab::ROLE.into(), pred("actor id", "term knowa:Owner|Member|Agent|Worker|Viewer", "author is an owner"));
    policy.insert(vocab::AUTO_ACCEPT.into(), pred("actor id", "bool", "author is an owner"));
    policy.insert(vocab::ACTOR_KIND.into(), pred("actor id (= author)", "term knowa:Human|Agent|Worker", "self-declared; written automatically as an actor's first claim in a space"));
    policy.insert(vocab::REVIEW.into(), pred("claim id", "term knowa:Accept|Reject", "author is an owner or member"));
    policy.insert(vocab::RETRACT.into(), pred("claim id", "{\"bool\": true}", "author is the target's author or an owner"));
    policy.insert(vocab::VISIBILITY.into(), pred("any id", "term knowa:Private|Shared|Public", "author is an owner"));
    policy.insert(vocab::DEFAULT_VISIBILITY.into(), pred("space id", "visibility term; absent = knowa:Private", "author is an owner"));

    let mut descriptive = serde_json::Map::new();
    for (p, o, meaning) in [
        (vocab::TYPE, "term", "kind of thing, e.g. knowa:Note, knowa:Photo"),
        (vocab::LABEL, "text", "human name"),
        (vocab::BODY, "text", "short body (notes, insights)"),
        (vocab::SUMMARY, "text", "condensed description, usually by an agent"),
        (vocab::CONTENT, "ref b_", "the entity's content bytes"),
        (vocab::WHEN, "when", "world time of the subject; drives timeline and temporal context"),
        (vocab::WHERE, "geo", "place of the subject; drives spatial context"),
        (vocab::ABOUT, "ref", "the subject is about the object"),
        (vocab::MENTIONS, "ref", "the subject mentions the object"),
        (vocab::RELATED, "ref", "generic association, usually by an agent with a note"),
        (vocab::SAME_AS, "ref", "identity alignment, also across spaces"),
        (vocab::NAME, "text", "original file name"),
        (vocab::SOURCE, "text", "where the bytes came from at import (path or URI)"),
        (vocab::MEDIA_TYPE, "text", "MIME type"),
        (vocab::SIZE, "int", "bytes"),
        (vocab::MODIFIED, "when", "filesystem modification time of the source file, read at import; the timeline fallback when no knowa:when exists"),
        (vocab::CREATED, "when", "filesystem creation time of the source file, read at import when the OS reports one"),
    ] {
        descriptive.insert(p.into(), json!({"o": o, "meaning": meaning}));
    }

    json!({
        "namespace": vocab::NS,
        "policy": policy,
        "policyNote": "Policy rules are enforced when status is computed, not at write time. The owner set is the genesis owner plus every actor given knowa:role knowa:Owner by an owner (fixpoint); retracted role claims are ignored.",
        "descriptive": descriptive,
        "valueTerms": {
            "roles": [vocab::ROLE_OWNER, vocab::ROLE_MEMBER, vocab::ROLE_AGENT, vocab::ROLE_WORKER, vocab::ROLE_VIEWER],
            "actorKinds": [vocab::KIND_HUMAN, vocab::KIND_AGENT, vocab::KIND_WORKER],
            "reviews": [vocab::REVIEW_ACCEPT, vocab::REVIEW_REJECT],
            "visibility": [vocab::VIS_PRIVATE, vocab::VIS_SHARED, vocab::VIS_PUBLIC],
            "types": [vocab::T_NOTE, vocab::T_PHOTO, vocab::T_FILE, vocab::T_PERSON, vocab::T_PLACE, vocab::T_EVENT, vocab::T_INSIGHT]
        },
        "extensions": "Extractors and agents use their own namespaces, e.g. exif:make, exif:model, exif:width."
    })
}

fn status() -> Value {
    json!({
        "values": ["accepted", "proposed", "rejected", "retracted", "superseded"],
        "semantics": "Status is computed at read time and never stored. The first matching rule wins: retracted if an effective knowa:retract targets the claim; superseded if a claim whose own status is accepted or superseded lists it in replaces; otherwise the latest effective knowa:review (by at, then id) makes it accepted or rejected; otherwise accepted if its author is an owner or member or has an effective knowa:autoAccept true; otherwise proposed (typical for agent output). stale is a separate flag: true when some c_ id in src is retracted, rejected, superseded or itself stale. Reads default to accepted + proposed with stale included, and every result carries status and stale."
    })
}

fn errors() -> Value {
    let codes = [
        (ErrorCode::Internal, "unexpected failure (I/O, a failed worker command)"),
        (ErrorCode::Usage, "missing selection or configuration (space, actor), readonly space, wrong actor kind"),
        (ErrorCode::NotFound, "an id, file, space or actor does not exist"),
        (ErrorCode::Invalid, "malformed input: draft, value, id, query field, cursor"),
        (ErrorCode::Conflict, "the operation contradicts existing state (alias taken, chain conflict)"),
        (ErrorCode::Unsupported, "valid but not supported in v0 (asOf, multi-space cursor, search beyond its bounds)"),
        (ErrorCode::Integrity, "signature, hash or chain verification failed"),
        (ErrorCode::CursorExpired, "the replica's rebuild generation changed since the feed cursor was issued (a merge filled gaps); restart without it"),
    ];
    json!({
        "envelope": "With --json, errors go to stderr as {\"error\": {\"code\", \"message\", \"detail\"?}} and the process exits with the code's exit status.",
        "codes": codes.iter().map(|(c, m)| json!({"code": c.as_str(), "exit": c.exit_code(), "meaning": m})).collect::<Vec<_>>(),
        "skipped": {"exit": SKIPPED_EXIT, "meaning": "not an error and not an error code: the command finished but skipped some of its inputs; its result, printed as usual, lists them in skipped. Only put uses it."}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_every_error_code_with_its_exit() {
        let s = schema();
        let codes = s["errors"]["codes"].as_array().unwrap();
        let got: Vec<(String, i64)> =
            codes.iter().map(|c| (c["code"].as_str().unwrap().to_string(), c["exit"].as_i64().unwrap())).collect();
        assert_eq!(
            got,
            vec![
                ("internal".into(), 1),
                ("usage".into(), 2),
                ("not_found".into(), 3),
                ("invalid".into(), 4),
                ("conflict".into(), 5),
                ("unsupported".into(), 6),
                ("integrity".into(), 7),
                ("cursor_expired".into(), 8)
            ]
        );
        assert_eq!(s["errors"]["skipped"]["exit"], 9, "exit 9 is listed apart from the error codes");
    }

    #[test]
    fn describes_every_kernel_predicate() {
        let s = schema();
        let v = &s["vocabulary"];
        for p in vocab::POLICY_PREDICATES {
            assert!(v["policy"].get(*p).is_some(), "{p}");
        }
        for p in [vocab::TYPE, vocab::LABEL, vocab::WHEN, vocab::WHERE, vocab::SIZE, vocab::SAME_AS] {
            assert!(v["descriptive"].get(p).is_some(), "{p}");
        }
    }

    #[test]
    fn covers_primitives_and_query_ops() {
        let s = schema();
        for p in ["write", "blob.put", "blob.get", "query", "feed"] {
            assert!(s["primitives"].get(p).is_some(), "{p}");
        }
        for op in ["find", "get", "context", "path", "log"] {
            assert!(s["query"]["ops"].get(op).is_some(), "{op}");
        }
        assert_eq!(s["query"]["ops"]["find"]["fields"]["limit"]["max"], MAX_LIMIT);
    }
}
