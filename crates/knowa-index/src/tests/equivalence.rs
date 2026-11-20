//! Incremental apply ≡ rebuild: random-but-seeded claim histories (reviews,
//! retracts incl. retract chains, replaces, role grants and revocations, autoAccept,
//! src chains, several actors), applied in batches and in shuffled arrival orders,
//! must index to exactly what one from-scratch build in log order produces.

use std::collections::BTreeSet;

use knowa_model::query::{Order, View, Where};
use knowa_model::{ActorInfo, Claim, EntityView, Id, Status, Value, vocab};

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

fn scenario(seed: u64, n: usize) -> (World, Vec<Claim>) {
    let mut rng = Rng(seed);
    let mut w = World::new(ACTORS);
    let mut out = vec![w.genesis()];
    out.push(w.kind(1, vocab::KIND_HUMAN));
    out.push(w.kind(2, vocab::KIND_AGENT));
    out.push(w.kind(3, vocab::KIND_WORKER));
    out.push(w.kind(4, vocab::KIND_HUMAN));
    // actor 5 never declares a kind
    let blob = Id::blob_of(format!("blob {seed}").as_bytes());
    let region = Id::parse(&format!("{}#xywh=1,2,3,4", blob.as_str())).unwrap();
    let mut subjects: Vec<Id> = (0..8).map(|_| ent()).collect();
    subjects.push(blob.clone());
    subjects.push(region);
    let roles = [vocab::ROLE_OWNER, vocab::ROLE_MEMBER, vocab::ROLE_AGENT, vocab::ROLE_VIEWER];
    let types = [vocab::T_NOTE, vocab::T_PHOTO, vocab::T_PERSON];

    for _ in 0..n {
        if rng.chance(20) {
            w.clock -= 1000; // same `at` as the previous claim: exercises (at, id) ties
        }
        let who = if rng.chance(30) { G } else { 1 + rng.below(ACTORS - 1) };
        let earlier: Vec<Id> = out.iter().map(|c| c.id.clone()).collect();
        let roll = rng.below(100);
        let c = if roll < 45 {
            let s = rng.pick(&subjects);
            let (p, o) = match rng.below(7) {
                0 => (vocab::LABEL, text(&format!("{} {}", rng.pick(&WORDS), rng.pick(&WORDS)))),
                1 => (vocab::TYPE, term(rng.pick(&types))),
                2 => (vocab::WHEN, when(&format!("20{}", 10 + rng.below(20)))),
                3 => (vocab::WHERE, geo(&format!("35.{}", rng.below(99)), &format!("139.{}", rng.below(99)))),
                4 => (vocab::BODY, text(rng.pick(&WORDS))),
                5 => (vocab::CONTENT, Value::Ref(blob.clone())),
                _ => (vocab::MEDIA_TYPE, text(if rng.chance(50) { "text/plain" } else { "image/png" })),
            };
            let src: Vec<Id> = if rng.chance(40) { (0..1 + rng.below(2)).map(|_| rng.pick(&earlier)).collect() } else { vec![] };
            let replaces: Vec<Id> = if rng.chance(20) { vec![rng.pick(&earlier)] } else { vec![] };
            let conf = rng.chance(20).then(|| knowa_model::Dec::parse(&format!("0.{}", rng.below(10))).unwrap());
            w.with(who, &s, p, o, |d| {
                d.src = src.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
                d.replaces = replaces;
                d.conf = conf;
            })
        } else if roll < 60 {
            let target = rng.pick(&out);
            w.review(who, &target, rng.chance(60))
        } else if roll < 75 {
            let target = rng.pick(&out);
            w.retract(who, &target)
        } else if roll < 87 {
            let grantor = if rng.chance(60) { G } else { who };
            let to = 1 + rng.below(ACTORS - 1);
            w.grant(grantor, to, rng.pick(&roles))
        } else if roll < 93 {
            let grantor = if rng.chance(60) { G } else { who };
            w.auto_accept(grantor, 1 + rng.below(ACTORS - 1), rng.chance(70))
        } else {
            let s = rng.pick(&subjects);
            w.say(who, &s, vocab::VISIBILITY, term(vocab::VIS_SHARED))
        };
        out.push(c);
    }
    (w, out)
}

#[derive(Debug)]
struct Snapshot {
    claims: Vec<(Id, Status, bool)>,
    entities: Vec<EntityView>,
    actors: Vec<ActorInfo>,
    search: Vec<Vec<(Id, i64)>>,
    /// claims exported at `shared`
    export: Vec<Id>,
}

fn snapshot(idx: &crate::Index, claims: &[Claim]) -> Snapshot {
    let mut ids: Vec<Id> = claims.iter().map(|c| c.id.clone()).collect();
    ids.sort();
    let claims = idx.claims(&ids).unwrap().into_iter().map(|v| (v.claim.id, v.status, v.stale)).collect();
    let mut entities = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = idx.find_entities(&Where::default(), &all_statuses(), Order::Recorded, 7, cursor.as_deref()).unwrap();
        entities.extend(page.items);
        match page.cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    let search = WORDS
        .iter()
        .map(|word| {
            let w = Where { text: Some(word.to_string()), ..Where::default() };
            idx.find_entities(&w, &View::default(), Order::Relevance, 1000, None)
                .unwrap()
                .items
                .into_iter()
                // bm25 over identical documents is identical; rounding only guards
                // against float noise in the last bits
                .map(|e| (e.id, (e.score.unwrap() * 1e9).round() as i64))
                .collect()
        })
        .collect();
    let mut export = idx.export_set_with(knowa_model::Visibility::Shared, &MemBlobs::default()).unwrap().0;
    export.sort();
    Snapshot { claims, entities, actors: idx.actors().unwrap(), search, export }
}

/// Compares section by section so a failure names the differing items only.
fn assert_same(got: &Snapshot, want: &Snapshot, ctx: &str) {
    let claims: Vec<_> = got.claims.iter().zip(&want.claims).filter(|(g, w)| g != w).collect();
    assert!(claims.is_empty() && got.claims.len() == want.claims.len(), "{ctx}: claim (id, status, stale) differ (got, want): {claims:?}");
    let entities: Vec<_> = got.entities.iter().zip(&want.entities).filter(|(g, w)| g != w).collect();
    assert!(entities.is_empty() && got.entities.len() == want.entities.len(), "{ctx}: entities differ (got, want): {entities:?}");
    assert_eq!(got.actors, want.actors, "{ctx}: actors differ");
    assert_eq!(got.search, want.search, "{ctx}: search results differ");
    assert_eq!(got.export, want.export, "{ctx}: export selection differs");
}

fn index_with(w: &World, batches: &[Vec<Claim>]) -> (TempDir, crate::Index) {
    let dir = TempDir::new();
    let mut idx = w.index(&dir);
    let blobs = MemBlobs::default();
    for b in batches {
        idx.apply_with(&blobs, b).unwrap();
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
    for seed in 0..12u64 {
        let (w, claims) = scenario(seed, 220);

        // from scratch: one batch in log replay order (actor id, seq)
        let mut replay = claims.clone();
        replay.sort_by(|a, b| (&a.body.actor, a.body.seq).cmp(&(&b.body.actor, b.body.seq)));
        let (_d0, reference) = index_with(&w, &[replay]);
        let expected = snapshot(&reference, &claims);
        for (_, s, st) in &expected.claims {
            seen_status.insert(*s);
            seen_stale |= *st;
        }

        let mut rng = Rng(seed ^ 0xdead_beef);
        let (_d1, in_order) = index_with(&w, &batched(&mut rng, claims.clone()));
        assert_same(&snapshot(&in_order, &claims), &expected, &format!("seed {seed}: authoring order, random batches"));

        let mut shuffled = claims.clone();
        for i in (1..shuffled.len()).rev() {
            shuffled.swap(i, rng.below(i + 1));
        }
        let (_d2, out_of_order) = index_with(&w, &batched(&mut rng, shuffled));
        assert_same(&snapshot(&out_of_order, &claims), &expected, &format!("seed {seed}: shuffled arrival, random batches"));
    }
    // the generator must actually exercise every rule
    assert_eq!(seen_status.len(), 5, "statuses seen: {seen_status:?}");
    assert!(seen_stale);
}
