//! `path` (DESIGN §6.2): shortest-first simple paths between two ids over ref edges
//! of every selected space, traversed undirected with each step's direction recorded.

use std::collections::{HashMap, HashSet};

use knowa_config::Home;
use knowa_model::query::{MAX_PATH_HOPS, PathDoc, PathResult, PathStep, View};
use knowa_model::{Dir, Edge, Error, Id, Result};

use crate::Sel;
use crate::context::edges_all;
use crate::query::{limit_or, reject_as_of};
use crate::select::{Opened, open_all, select};

/// PathDoc docs: default `maxHops`.
const DEFAULT_MAX_HOPS: u32 = 4;
/// PathDoc docs: default number of paths.
const DEFAULT_PATHS: u32 = 3;
/// Breadth-first exploration fails past this many distinct nodes rather than answer
/// from a silently truncated graph.
pub(crate) const MAX_VISITED: usize = 50_000;
/// Enumerating paths longer than the shortest one can branch combinatorially inside
/// the explored graph; past this many search steps the call fails the same way.
pub(crate) const MAX_SEARCH_STEPS: usize = 1_000_000;

pub(crate) fn path(home: &Home, sel: &Sel, d: PathDoc) -> Result<PathResult> {
    reject_as_of(&d.as_of)?;
    let max_hops = d.max_hops.unwrap_or(DEFAULT_MAX_HOPS);
    if max_hops == 0 || max_hops > MAX_PATH_HOPS {
        return Err(Error::invalid(format!("maxHops must be between 1 and {MAX_PATH_HOPS}, got {max_hops}")));
    }
    let limit = limit_or(d.limit, DEFAULT_PATHS, "limit")?;
    if d.from == d.to {
        return Err(Error::invalid("path: from and to must differ"));
    }
    let opened = open_all(select(home, sel, &d.spaces)?)?;
    for id in [&d.from, &d.to] {
        if !known(&opened, id, &d.view)? {
            return Err(Error::not_found(format!(
                "path: nothing in the selected spaces describes or references {id} under the given view"
            )));
        }
    }
    let paths = find_paths(&d.from, &d.to, max_hops, limit as usize, |id| edges_all(&opened, id, &d.view))?;
    Ok(PathResult { paths })
}

fn known(opened: &[Opened], id: &Id, view: &View) -> Result<bool> {
    for o in opened {
        if o.index.entity(id, view)?.is_some() || !o.index.edges(id, view)?.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The part of the graph explored so far.
struct Explored {
    /// BFS distance from `from`
    dist: HashMap<Id, u32>,
    /// full edge lists of expanded nodes
    adj: HashMap<Id, Vec<Edge>>,
    /// edges seen from the other endpoint, reversed; serves nodes not yet expanded
    rev: HashMap<Id, Vec<Edge>>,
}

impl Explored {
    fn neighbors(&self, v: &Id) -> &[Edge] {
        match self.adj.get(v) {
            Some(es) => es,
            // Not expanded and never seen from an expanded node: no known edges yet.
            None => self.rev.get(v).map_or(&[], Vec::as_slice),
        }
    }
}

/// Up to `limit` simple paths from `from` to `to` of at most `max_hops` edges,
/// shortest first. Explores breadth-first one depth at a time; after depth k it
/// knows every edge a path of length k can use (all its inner nodes are at BFS
/// distance < k, hence expanded), enumerates those paths, and stops as soon as
/// `limit` paths are found. `edges(u)` returns the edges touching `u` from `u`'s
/// perspective, like `Index::edges`.
pub(crate) fn find_paths(
    from: &Id,
    to: &Id,
    max_hops: u32,
    limit: usize,
    mut edges: impl FnMut(&Id) -> Result<Vec<Edge>>,
) -> Result<Vec<Vec<PathStep>>> {
    let mut g = Explored { dist: HashMap::from([(from.clone(), 0)]), adj: HashMap::new(), rev: HashMap::new() };
    let mut frontier = vec![from.clone()];
    let mut paths = Vec::new();
    let mut steps = 0usize;
    for depth in 1..=max_hops {
        let mut next = Vec::new();
        for u in &frontier {
            let mut es = edges(u)?;
            es.retain(|e| e.to != *u);
            for e in &es {
                if !g.dist.contains_key(&e.to) {
                    g.dist.insert(e.to.clone(), depth);
                    if g.dist.len() > MAX_VISITED {
                        return Err(Error::unsupported(format!(
                            "path: search visited more than {MAX_VISITED} nodes within {depth} hops; lower maxHops or choose less connected endpoints"
                        ))
                        .with_detail(serde_json::json!({ "visited": g.dist.len(), "depth": depth, "maxHops": max_hops })));
                    }
                    next.push(e.to.clone());
                }
                g.rev.entry(e.to.clone()).or_default().push(reversed(e));
            }
            g.adj.insert(u.clone(), es);
        }
        if g.dist.contains_key(to) {
            let mut search = Search {
                g: &g,
                from,
                want: limit - paths.len(),
                on_path: HashSet::from([to.clone()]),
                nodes: vec![to.clone()],
                links: Vec::new(),
                steps: &mut steps,
                found: Vec::new(),
            };
            search.dfs(to, depth)?;
            paths.extend(search.found);
            if paths.len() >= limit {
                break;
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(paths)
}

/// Backward depth-first enumeration of paths of one exact length, from `to` towards
/// `from`, pruned by BFS distance: a node u may sit r steps before `to`'s end only if
/// dist(u) ≤ r.
struct Search<'g> {
    g: &'g Explored,
    from: &'g Id,
    want: usize,
    on_path: HashSet<Id>,
    /// nodes from `to` backwards
    nodes: Vec<Id>,
    /// `links[i]` joins `nodes[i]` to `nodes[i+1]`, seen from `nodes[i]`
    links: Vec<Edge>,
    steps: &'g mut usize,
    found: Vec<Vec<PathStep>>,
}

impl Search<'_> {
    /// `remaining` = edges still needed to reach `from`.
    fn dfs(&mut self, v: &Id, remaining: u32) -> Result<()> {
        if remaining == 0 {
            if v == self.from {
                self.emit();
            }
            return Ok(());
        }
        for e in self.g.neighbors(v) {
            *self.steps += 1;
            if *self.steps > MAX_SEARCH_STEPS {
                return Err(Error::unsupported(format!(
                    "path: enumerating paths took more than {MAX_SEARCH_STEPS} steps; lower maxHops or limit"
                )));
            }
            let u = &e.to;
            let Some(&du) = self.g.dist.get(u) else { continue };
            // `from` can only be the last node of a simple path.
            if du > remaining - 1 || self.on_path.contains(u) || (u == self.from && remaining > 1) {
                continue;
            }
            self.on_path.insert(u.clone());
            self.nodes.push(u.clone());
            self.links.push(e.clone());
            self.dfs(u, remaining - 1)?;
            self.links.pop();
            self.nodes.pop();
            self.on_path.remove(u);
            if self.found.len() == self.want {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Turns the backward stack into forward steps: the step onto node v_i records the
    /// link from v_{i-1}'s perspective, i.e. the stored link with its direction flipped.
    fn emit(&mut self) {
        let mut steps = Vec::with_capacity(self.nodes.len());
        let last = self.nodes.len() - 1;
        steps.push(PathStep { id: self.nodes[last].clone(), via: None, p: None, dir: None });
        for i in (0..last).rev() {
            let link = &self.links[i];
            steps.push(PathStep {
                id: self.nodes[i].clone(),
                via: Some(link.via.clone()),
                p: Some(link.p.clone()),
                dir: Some(flip(link.dir)),
            });
        }
        self.found.push(steps);
    }
}

fn flip(d: Dir) -> Dir {
    match d {
        Dir::Out => Dir::In,
        Dir::In => Dir::Out,
    }
}

fn reversed(e: &Edge) -> Edge {
    Edge { from: e.to.clone(), to: e.from.clone(), p: e.p.clone(), via: e.via.clone(), dir: flip(e.dir) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_graph::{claim, edges_of, node};
    use knowa_model::ErrorCode;

    fn ids(p: &[PathStep]) -> Vec<Id> {
        p.iter().map(|s| s.id.clone()).collect()
    }

    fn run(g: &[(u32, u32, u32)], from: u32, to: u32, max_hops: u32, limit: usize) -> Result<Vec<Vec<PathStep>>> {
        find_paths(&node(from), &node(to), max_hops, limit, |n| Ok(edges_of(g, n)))
    }

    #[test]
    fn single_edge_records_direction_and_claim() {
        // claim 7: node 1 → node 0; walking 0 → 1 traverses it against its direction
        let paths = run(&[(1, 0, 7)], 0, 1, 4, 3).unwrap();
        assert_eq!(paths.len(), 1);
        let p = &paths[0];
        assert_eq!(ids(p), vec![node(0), node(1)]);
        assert_eq!(p[0].via, None);
        assert_eq!(p[0].dir, None);
        assert_eq!(p[1].via, Some(claim(7)));
        assert_eq!(p[1].dir, Some(Dir::In));
        assert_eq!(p[1].p.as_ref().map(|t| t.as_str()), Some("knowa:related"));
    }

    #[test]
    fn forward_edge_is_out() {
        let paths = run(&[(0, 1, 1), (1, 2, 2)], 0, 2, 4, 3).unwrap();
        assert_eq!(ids(&paths[0]), vec![node(0), node(1), node(2)]);
        assert_eq!(paths[0][1].dir, Some(Dir::Out));
        assert_eq!(paths[0][2].dir, Some(Dir::Out));
    }

    #[test]
    fn shortest_first_then_longer() {
        // 0-3 directly; 0-1-3; 0-1-2-3
        let g = [(0, 3, 1), (0, 1, 2), (1, 3, 3), (1, 2, 4), (2, 3, 5)];
        let paths = run(&g, 0, 3, 4, 10).unwrap();
        let lens: Vec<usize> = paths.iter().map(|p| p.len() - 1).collect();
        assert_eq!(lens, vec![1, 2, 3]);
        assert_eq!(ids(&paths[1]), vec![node(0), node(1), node(3)]);
        assert_eq!(ids(&paths[2]), vec![node(0), node(1), node(2), node(3)]);
    }

    #[test]
    fn limit_stops_early() {
        let g = [(0, 3, 1), (0, 1, 2), (1, 3, 3), (1, 2, 4), (2, 3, 5)];
        let paths = run(&g, 0, 3, 4, 2).unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0].len(), 2);
    }

    #[test]
    fn parallel_claims_are_distinct_paths() {
        let paths = run(&[(0, 1, 1), (1, 0, 2)], 0, 1, 4, 5).unwrap();
        assert_eq!(paths.len(), 2);
        let vias: Vec<_> = paths.iter().map(|p| p[1].via.clone().unwrap()).collect();
        assert!(vias.contains(&claim(1)) && vias.contains(&claim(2)));
    }

    #[test]
    fn paths_are_simple() {
        // triangle 0-1-2 plus 2-3: no path may revisit a node
        let g = [(0, 1, 1), (1, 2, 2), (2, 0, 3), (2, 3, 4)];
        let paths = run(&g, 0, 3, 6, 50).unwrap();
        for p in &paths {
            let mut seen = HashSet::new();
            assert!(p.iter().all(|s| seen.insert(s.id.clone())), "{:?}", ids(p));
        }
        let lens: Vec<usize> = paths.iter().map(|p| p.len() - 1).collect();
        assert_eq!(lens, vec![2, 3]);
    }

    #[test]
    fn max_hops_bounds_length() {
        let g = [(0, 1, 1), (1, 2, 2), (2, 3, 3)];
        assert!(run(&g, 0, 3, 2, 3).unwrap().is_empty());
        assert_eq!(run(&g, 0, 3, 3, 3).unwrap().len(), 1);
    }

    #[test]
    fn disconnected_is_empty() {
        assert!(run(&[(0, 1, 1), (2, 3, 2)], 0, 3, 6, 3).unwrap().is_empty());
    }

    #[test]
    fn visited_overflow_is_unsupported() {
        // a star of MAX_VISITED leaves around node 0; the target hangs off a leaf
        let n = MAX_VISITED as u32;
        let leaves: Vec<Edge> = (1..=n)
            .map(|i| Edge {
                from: node(0),
                to: Id::parse(&format!("e_01arz3ndektsv4rrffq69{i:05}")).unwrap(),
                p: knowa_model::Term::parse("knowa:related").unwrap(),
                via: claim(i),
                dir: Dir::Out,
            })
            .collect();
        let err = find_paths(&node(0), &node(9999), 3, 3, |at| Ok(if *at == node(0) { leaves.clone() } else { Vec::new() }))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert!(err.detail.is_some());
    }
}
