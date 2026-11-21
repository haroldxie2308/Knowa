//! Incremental apply ≡ rebuild: random-but-seeded claim histories (reviews,
//! retracts incl. retract chains, replaces incl. replacement chains, role grants,
//! demotions and revocations, autoAccept on/off, actorKind re-declarations,
//! visibility and default visibility, src chains, a present text blob, several
//! actors), applied in batches and in shuffled arrival orders, must index to exactly
//! what one from-scratch build in log order produces — under several views. The
//! reference index also checks that the stored and computed read routes agree.

use std::collections::BTreeSet;

use knowa_model::query::{Order, View, Where};
use knowa_model::{ActorInfo, ActorKind, Claim, EntityView, Id, Status, Value, Visibility, vocab};
use unicode_normalization::UnicodeNormalization;

use super::evidence::{assert_routes_agree, route_queries};
use super::kit::*;

/// splitmix64
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
    fn pick<T: Clone>(&mut self, v: &[T]) -> T {
        v[self.below(v.len())].clone()
    }
}

const WORDS: [&str; 6] = ["alpha", "beta", "gamma", "delta", "東京タワー", "서울특별시"];
const ACTORS: usize = 6;

fn scenario(seed: u64, n: usize) -> (World, Vec<Claim>, MemBlobs) {
    let mut rng = Rng(seed);
    let mut w = World::new(ACTORS);
    let mut out = vec![w.genesis()];
    out.push(w.kind(1, vocab::KIND_HUMAN));
    out.push(w.kind(2, vocab::KIND_AGENT));
    out.push(w.kind(3, vocab::KIND_WORKER));
    out.push(w.kind(4, vocab::KIND_HUMAN));
    // actor 5 never declares a kind first; it may later
    let mut blobs = MemBlobs::default();
    // NFD Hangul, which the composed query word 서울특별시 matches (DESIGN §4.7)
    let blob = blobs.put(format!("blob {seed} alpha gamma 東京タワー {}", "서울특별시".nfd().collect::<String>()).as_bytes());
    let region = Id::parse(&format!("{}#xywh=1,2,3,4", blob.as_str())).unwrap();
    let mut subjects: Vec<Id> = (0..8).map(|_| ent()).collect();
    subjects.push(blob.clone());
    subjects.push(region);
    let roles = [vocab::ROLE_OWNER, vocab::ROLE_MEMBER, vocab::ROLE_AGENT, vocab::ROLE_WORKER, vocab::ROLE_VIEWER];
    let kinds = [vocab::KIND_HUMAN, vocab::KIND_AGENT, vocab::KIND_WORKER];
    let types = [vocab::T_NOTE, vocab::T_PHOTO, vocab::T_PERSON];
    let levels = [vocab::VIS_PRIVATE, vocab::VIS_SHARED, vocab::VIS_PUBLIC];

    for _ in 0..n {
        if rng.chance(20) {
            w.clock -= 1000; // same `at` as the previous claim: exercises (at, id) ties
        }
        let who = if rng.chance(30) { G } else { 1 + rng.below(ACTORS - 1) };
        let earlier: Vec<Id> = out.iter().map(|c| c.id.clone()).collect();
        let roll = rng.below(100);
        let c = if roll < 45 {
            let mut s = rng.pick(&subjects);
            let (p, o) = match rng.below(7) {
                0 => (vocab::LABEL, text(&format!("{} {}", rng.pick(&WORDS), rng.pick(&WORDS)))),
                1 => (vocab::TYPE, term(rng.pick(&types))),
                2 => (vocab::WHEN, when(&format!("20{}", 10 + rng.below(20)))),
                3 => (vocab::WHERE, geo(&format!("35.{}", rng.below(99)), &format!("139.{}", rng.below(99)))),
                4 => (vocab::BODY, text(rng.pick(&WORDS))),
                5 => (vocab::CONTENT, Value::Ref(blob.clone())),
                _ => (vocab::MEDIA_TYPE, text(if rng.chance(50) { "text/plain" } else { "image/png" })),
            };
            // mostly about the blob itself, so its text-ness flips with reviews and
            // retracts (which re-derives the documents of the claims linking it)
            if p == vocab::MEDIA_TYPE && rng.chance(70) {
                s = blob.clone();
            }
            let src: Vec<Id> = if rng.chance(40) { (0..1 + rng.below(2)).map(|_| rng.pick(&earlier)).collect() } else { vec![] };
            let replaces: Vec<Id> =
                if rng.chance(25) { (0..1 + rng.below(2)).map(|_| rng.pick(&earlier)).collect::<BTreeSet<_>>().into_iter().collect() } else { vec![] };
            let conf = rng.chance(20).then(|| knowa_model::Dec::parse(&format!("0.{}", rng.below(10))).unwrap());
            w.with(who, &s, p, o, |d| {
                d.src = src.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
                d.replaces = replaces;
                d.conf = conf;
            })
        } else if roll < 60 {
            let target = rng.pick(&out);
            w.review(who, &target, rng.chance(60))
        } else if roll < 73 {
            let target = rng.pick(&out);
            w.retract(who, &target)
        } else if roll < 85 {
            let grantor = if rng.chance(60) { G } else { who };
            let to = 1 + rng.below(ACTORS - 1);
            w.grant(grantor, to, rng.pick(&roles))
        } else if roll < 91 {
            let grantor = if rng.chance(60) { G } else { who };
            w.auto_accept(grantor, 1 + rng.below(ACTORS - 1), rng.chance(60))
        } else if roll < 94 {
            // re-declaration (or, for actor 5, maybe a first declaration)
            w.kind(1 + rng.below(ACTORS - 1), rng.pick(&kinds))
        } else if roll < 96 {
            let space = w.space.clone();
            w.say(if rng.chance(70) { G } else { who }, &space, vocab::DEFAULT_VISIBILITY, term(rng.pick(&levels)))
        } else {
            let s = if rng.chance(50) { rng.pick(&subjects) } else { rng.pick(&earlier) };
            w.say(who, &s, vocab::VISIBILITY, term(rng.pick(&levels)))
        };
        out.push(c);
    }
    (w, out, blobs)
}

fn views() -> Vec<View> {
    vec![
        all_statuses(),
        View::default(),
        View { status: vec![Status::Accepted], actor_kinds: vec![ActorKind::Human], ..View::default() },
        View { actor_kinds: vec![ActorKind::Agent, ActorKind::Worker], ..View::default() },
        View { status: vec![Status::Proposed], include_stale: false, ..View::default() },
        View { min_conf: Some("0.5".into()), ..View::default() },
    ]
}

#[derive(Debug)]
struct Snapshot {
    claims: Vec<(Id, Status, bool)>,
    /// per view
    entities: Vec<Vec<EntityView>>,
    actors: Vec<ActorInfo>,
    /// per (view, word)
    search: Vec<Vec<(Id, i64)>>,
    /// `equivalent` for every claim's (actor, key)
    equivalent: Vec<Option<Id>>,
    /// (claims, blobs) exported at shared and at public
    export: Vec<(Vec<Id>, Vec<Id>)>,
}

fn snapshot(idx: &crate::Index, claims: &[Claim], blobs: &MemBlobs) -> Snapshot {
    let mut ids: Vec<Id> = claims.iter().map(|c| c.id.clone()).collect();
    ids.sort();
    let claim_rows = idx.claims(&ids).unwrap().into_iter().map(|v| (v.claim.id, v.status, v.stale)).collect();
    let mut entities = Vec::new();
    let mut search = Vec::new();
    for view in views() {
        let mut list = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = idx.find_entities(&Where::default(), &view, Order::Recorded, 7, cursor.as_deref()).unwrap();
            list.extend(page.items);
            match page.cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        entities.push(list);
        for word in WORDS {
            let w = Where { text: Some(word.to_string()), ..Where::default() };
            search.push(
                idx.find_entities(&w, &view, Order::Relevance, 1000, None)
                    .unwrap()
                    .items
                    .into_iter()
                    // bm25 over identical documents is identical; rounding only guards
                    // against float noise in the last bits
                    .map(|e| (e.id, (e.score.unwrap() * 1e9).round() as i64))
                    .collect(),
            );
        }
    }
    let equivalent = claims
        .iter()
        .map(|c| idx.equivalent(&c.body.actor, &c.body.equiv_key().unwrap()).unwrap().map(|v| v.claim.id))
        .collect();
    let export = [Visibility::Shared, Visibility::Public]
        .into_iter()
        .map(|level| {
            let (mut c, b) = idx.export_set_with(level, blobs).unwrap();
            c.sort();
            (c, b)
        })
        .collect();
    Snapshot { claims: claim_rows, entities, actors: idx.actors().unwrap(), search, equivalent, export }
}

/// Compares section by section so a failure names the differing items only.
fn assert_same(got: &Snapshot, want: &Snapshot, ctx: &str) {
    let claims: Vec<_> = got.claims.iter().zip(&want.claims).filter(|(g, w)| g != w).collect();
    assert!(claims.is_empty() && got.claims.len() == want.claims.len(), "{ctx}: claim (id, status, stale) differ (got, want): {claims:?}");
    for (i, (g, w)) in got.entities.iter().zip(&want.entities).enumerate() {
        let diff: Vec<_> = g.iter().zip(w).filter(|(g, w)| g != w).collect();
        assert!(diff.is_empty() && g.len() == w.len(), "{ctx}: entities under view {i} differ (got, want): {diff:?}");
    }
    assert_eq!(got.actors, want.actors, "{ctx}: actors differ");
    assert_eq!(got.search, want.search, "{ctx}: search results differ");
    assert_eq!(got.equivalent, want.equivalent, "{ctx}: equivalents differ");
    assert_eq!(got.export, want.export, "{ctx}: export selection differs");
}

fn index_with(w: &World, blobs: &MemBlobs, batches: &[Vec<Claim>]) -> (TempDir, crate::Index) {
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    for b in batches {
        idx.apply_with(blobs, b).unwrap();
    }
    (dir, idx)
}

fn batched(rng: &mut Rng, claims: Vec<Claim>) -> Vec<Vec<Claim>> {
    let mut out = Vec::new();
    let mut it = claims.into_iter().peekable();
    while it.peek().is_some() {
        let n = 1 + rng.below(8);
        out.push(it.by_ref().take(n).collect());
    }
    out
}

#[test]
fn incremental_apply_equals_rebuild() {
    let mut seen_status: BTreeSet<Status> = BTreeSet::new();
    let mut seen_stale = false;
    let (mut views_differ, mut inactive_equivalents, mut routes_compared) = (false, false, 0);
    for seed in 0..12u64 {
        let (w, claims, blobs) = scenario(seed, 220);

        // from scratch: one batch in log replay order (actor id, seq)
        let mut replay = claims.clone();
        replay.sort_by(|a, b| (&a.body.actor, a.body.seq).cmp(&(&b.body.actor, b.body.seq)));
        let (_d0, reference) = index_with(&w, &blobs, &[replay]);
        let expected = snapshot(&reference, &claims, &blobs);
        for (_, s, st) in &expected.claims {
            seen_status.insert(*s);
            seen_stale |= *st;
        }
        views_differ |= expected.entities[1] != expected.entities[2];
        inactive_equivalents |= expected.equivalent.iter().zip(&claims).any(|(e, c)| e.is_none() && expected.claims.iter().any(|(id, s, _)| *id == c.id && *s != Status::Retracted));
        // the slowest check; a third of the seeds still compares thousands of results
        if seed % 3 == 0 {
            routes_compared += assert_routes_agree(&reference, &route_queries(&w.owner()), &format!("seed {seed}"));
        }

        let mut rng = Rng(seed ^ 0xdead_beef);
        let (_d1, in_order) = index_with(&w, &blobs, &batched(&mut rng, claims.clone()));
        assert_same(&snapshot(&in_order, &claims, &blobs), &expected, &format!("seed {seed}: authoring order, random batches"));

        let mut shuffled = claims.clone();
        for i in (1..shuffled.len()).rev() {
            shuffled.swap(i, rng.below(i + 1));
        }
        let (_d2, out_of_order) = index_with(&w, &blobs, &batched(&mut rng, shuffled));
        assert_same(&snapshot(&out_of_order, &claims, &blobs), &expected, &format!("seed {seed}: shuffled arrival, random batches"));
    }
    // the generator must actually exercise every rule
    assert_eq!(seen_status.len(), 5, "statuses seen: {seen_status:?}");
    assert!(seen_stale);
    assert!(views_differ, "no view ever changed an entity's evidence");
    assert!(inactive_equivalents, "no non-retracted claim was ever an inactive equivalent");
    assert!(routes_compared > 1000, "only {routes_compared} entity results compared across routes");
}
