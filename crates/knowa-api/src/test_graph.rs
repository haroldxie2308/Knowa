//! Fake edge source for graph tests: an undirected multigraph of numbered nodes whose
//! edges are reported from either endpoint, the way `Index::edges` reports them.

use knowa_model::{Dir, Edge, Id, Term};

pub(crate) fn node(n: u32) -> Id {
    Id::parse(&format!("e_01arz3ndektsv4rrffq69g{n:04}")).unwrap()
}

pub(crate) fn claim(n: u32) -> Id {
    Id::claim_of(&n.to_be_bytes())
}

/// `graph` = (subject node, object node, claim number).
pub(crate) fn edges_of(graph: &[(u32, u32, u32)], at: &Id) -> Vec<Edge> {
    let p = Term::parse("knowa:related").unwrap();
    let mut out = Vec::new();
    for &(s, o, c) in graph {
        if node(s) == *at {
            out.push(Edge { from: node(s), to: node(o), p: p.clone(), via: claim(c), dir: Dir::Out });
        }
        if node(o) == *at {
            out.push(Edge { from: node(o), to: node(s), p: p.clone(), via: claim(c), dir: Dir::In });
        }
    }
    out
}
