//! Space selection (DESIGN §6.1 `spaces`) and opening replicas/indexes.

use std::collections::HashMap;
use std::path::PathBuf;

use knowa_config::{Home, SpaceRef};
use knowa_index::Index;
use knowa_model::{Error, Id, Result};
use knowa_store::{Location, Replica};

use crate::Sel;

/// One opened replica plus how it was selected.
pub(crate) struct Space {
    pub sref: SpaceRef,
    pub replica: Replica,
    /// this machine's state for the replica: its index and worker cursors
    pub index_dir: PathBuf,
}

impl Space {
    pub fn open(home: &Home, sref: SpaceRef) -> Result<Space> {
        let replica = open_replica(home, &sref.location, sref.blobs.as_deref()).map_err(|e| in_space(e, &sref))?;
        Space::new(home, sref, replica)
    }

    pub fn new(home: &Home, sref: SpaceRef, replica: Replica) -> Result<Space> {
        let index_dir = index_dir(home, &replica).map_err(|e| in_space(e, &sref))?;
        Ok(Space { sref, replica, index_dir })
    }

    pub fn id(&self) -> &Id {
        &self.replica.manifest().space
    }

    /// Alias when registered, else the replica's location; for messages.
    pub fn name(&self) -> String {
        describe(&self.sref)
    }

    /// Opens the index and catches it up with every actor's log, as a reader: it waits
    /// only for a writer appending to an actor it must catch up (DESIGN §5.4).
    pub fn index(&self) -> Result<Index> {
        Index::open(&self.replica, &self.index_dir)
    }

    /// Readonly replicas refuse local writes (DESIGN §7.3).
    pub fn ensure_writable(&self) -> Result<()> {
        if self.sref.readonly {
            return Err(Error::usage(format!(
                "space {} is registered readonly; write in your own space and refer to its ids",
                self.name()
            )));
        }
        Ok(())
    }
}

/// A selected space with its index open.
pub(crate) struct Opened {
    pub space: Space,
    pub index: Index,
}

/// How a query's `spaces` field selects replicas.
#[derive(Debug, PartialEq)]
pub(crate) enum Selection<'a> {
    /// `[]`: the caller's space (`--space` / `KNOWA_SPACE` / current)
    Current,
    /// `["*"]`: every registered space
    All,
    /// aliases or paths
    Named(&'a [String]),
}

pub(crate) fn classify(spaces: &[String]) -> Result<Selection<'_>> {
    if spaces.is_empty() {
        return Ok(Selection::Current);
    }
    if spaces.iter().any(|s| s == "*") {
        if spaces.len() != 1 {
            return Err(Error::invalid("spaces: \"*\" selects every registered space and cannot be combined with other entries"));
        }
        return Ok(Selection::All);
    }
    if let Some(blank) = spaces.iter().find(|s| s.trim().is_empty()) {
        return Err(Error::invalid(format!("spaces: empty entry {blank:?}")));
    }
    Ok(Selection::Named(spaces))
}

/// The caller's single space (writes, feed, admin ops).
pub(crate) fn current(home: &Home, sel: &Sel) -> Result<Space> {
    Space::open(home, home.resolve_space(sel.space.as_deref(), canonical)?)
}

/// A space selector that is not a registered alias, as the canonical location the
/// registry is keyed by (DESIGN §5.2).
fn canonical(selector: &str) -> Result<String> {
    Location::parse(selector).and_then(|l| l.canonical()).map_err(|mut e| {
        e.message = format!("{selector:?} is not a registered space alias, and as a location: {}", e.message);
        e
    })
}

/// Opens the replica registered (or selected) at the canonical `location`, with its
/// blobs at `blobs`.
pub(crate) fn open_replica(home: &Home, location: &str, blobs: Option<&str>) -> Result<Replica> {
    let location = Location::parse(location)?;
    let blobs = blobs.map(Location::parse).transpose()?;
    Replica::open(&location, blobs.as_ref(), &blob_cache(home))
}

/// `<home>/index/<s_id>/<key>/`: this machine's index and worker cursors for a replica
/// (DESIGN §5.2). Every registration of one location on one machine shares it.
pub(crate) fn index_dir(home: &Home, replica: &Replica) -> Result<PathBuf> {
    Ok(home.root().join("index").join(replica.manifest().space.as_str()).join(replica.location_key()?))
}

/// `<home>/cache/blobs/`: this machine's cache of object-store blob bytes, staged for
/// upload or downloaded for reading (DESIGN §5.2).
pub(crate) fn blob_cache(home: &Home) -> PathBuf {
    home.root().join("cache").join("blobs")
}

/// Resolves a `spaces` selection in order. Several replicas of the same space (e.g. an
/// own replica and an imported copy) count once: the first one wins. A space is its id
/// *and* genesis (DESIGN §7.3), so selected replicas that share an id but not their
/// manifest are different spaces claiming one id → `conflict`; letting the first win
/// would let one shadow the other.
pub(crate) fn select(home: &Home, sel: &Sel, spaces: &[String]) -> Result<Vec<Space>> {
    let refs = match classify(spaces)? {
        Selection::Current => vec![home.resolve_space(sel.space.as_deref(), canonical)?],
        Selection::All => {
            let all = home.all_spaces()?;
            if all.is_empty() {
                return Err(Error::usage(
                    "spaces: [\"*\"] but no spaces are registered (knowa init / knowa space add)",
                ));
            }
            all
        }
        Selection::Named(names) => {
            names.iter().map(|n| home.resolve_space(Some(n), canonical)).collect::<Result<Vec<_>>>()?
        }
    };
    let mut first: HashMap<Id, usize> = HashMap::new();
    let mut out: Vec<Space> = Vec::with_capacity(refs.len());
    for r in refs {
        let space = Space::open(home, r)?;
        match first.get(space.id()) {
            None => {
                first.insert(space.id().clone(), out.len());
                out.push(space);
            }
            Some(&i) if out[i].replica.manifest() == space.replica.manifest() => {}
            Some(&i) => {
                let (a, b) = (out[i].replica.manifest(), space.replica.manifest());
                return Err(Error::conflict(format!(
                    "spaces: {} and {} both claim space {} but differ (owner {} / {}, genesis {} / {}); a space is its id and genesis, so one of them is not that space — unregister it (knowa space rm)",
                    out[i].name(),
                    space.name(),
                    a.space,
                    a.owner,
                    b.owner,
                    a.genesis,
                    b.genesis
                ))
                .with_detail(serde_json::json!({ "space": a.space, "manifests": [a, b] })));
            }
        }
    }
    Ok(out)
}

pub(crate) fn open_all(spaces: Vec<Space>) -> Result<Vec<Opened>> {
    spaces
        .into_iter()
        .map(|space| {
            let index = space.index().map_err(|e| in_space(e, &space.sref))?;
            Ok(Opened { space, index })
        })
        .collect()
}

fn describe(sref: &SpaceRef) -> String {
    match &sref.alias {
        Some(a) => format!("'{a}'"),
        None => sref.location.clone(),
    }
}

/// Prefixes an error with the space it came from; code and detail are kept.
fn in_space(mut e: Error, sref: &SpaceRef) -> Error {
    e.message = format!("space {}: {}", describe(sref), e.message);
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use knowa_model::ErrorCode;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn empty_selects_current_space() {
        assert_eq!(classify(&[]).unwrap(), Selection::Current);
    }

    #[test]
    fn star_alone_selects_all() {
        assert_eq!(classify(&s(&["*"])).unwrap(), Selection::All);
    }

    #[test]
    fn star_mixed_with_names_is_invalid() {
        assert_eq!(classify(&s(&["*", "notes"])).unwrap_err().code, ErrorCode::Invalid);
    }

    #[test]
    fn blank_entry_is_invalid() {
        assert_eq!(classify(&s(&["notes", " "])).unwrap_err().code, ErrorCode::Invalid);
    }

    #[test]
    fn names_pass_through_in_order() {
        let v = s(&["b", "a"]);
        assert_eq!(classify(&v).unwrap(), Selection::Named(&v));
    }
}
