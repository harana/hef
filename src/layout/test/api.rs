use super::*;
use hashbrown::HashMap;
use std::cell::RefCell;

/// An in-memory graph that records every batch it is asked.
struct GraphDouble {
    asked: RefCell<Vec<Vec<u64>>>,
    parents: HashMap<u64, Vec<u64>>,
}

impl GraphDouble {
    fn new(edges: &[(u64, &[u64])]) -> Self {
        Self {
            asked: RefCell::new(Vec::new()),
            parents: edges.iter().map(|(sn, parents)| (*sn, parents.to_vec())).collect(),
        }
    }
}

impl EventGraph for GraphDouble {
    fn parents_of(&self, subject: &[u8], sns: &[u64], kind: EdgeKind) -> Result<Vec<Vec<u64>>, FormatError> {
        assert_eq!((subject, kind), (&b"room"[..], EdgeKind::Prev));
        self.asked.borrow_mut().push(sns.to_vec());
        if sns.contains(&666) {
            return Err(FormatError::RefOutOfRange { what: "test" });
        }
        Ok(sns
            .iter()
            .map(|sn| self.parents.get(sn).cloned().unwrap_or_default())
            .collect())
    }

    fn children_of(&self, _: &[u8], sns: &[u64], _: EdgeKind) -> Result<Vec<Vec<u64>>, FormatError> {
        Ok(sns.iter().map(|sn| vec![sn + 1]).collect())
    }
}

/// A diamond over a chain: 10 -> {8, 9} -> 7 -> 6 -> 5.
fn diamond() -> GraphDouble {
    GraphDouble::new(&[(10, &[9, 8]), (9, &[7]), (8, &[7]), (7, &[6]), (6, &[5])])
}

#[test]
fn a_walk_returns_ancestors_level_by_level_with_one_batch_per_level() {
    let graph = diamond();
    let levels: Vec<Vec<u64>> = graph
        .walk(b"room", 10, EdgeKind::Prev, 100)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(levels, [vec![8, 9], vec![7], vec![6], vec![5]]);
    assert_eq!(
        *graph.asked.borrow(),
        [vec![10], vec![8, 9], vec![7], vec![6], vec![5]],
        "each level is asked as one batch, and an ancestor reached twice is asked once"
    );
}

#[test]
fn a_walk_stops_at_its_limit() {
    let graph = diamond();
    let levels: Vec<Vec<u64>> = graph
        .walk(b"room", 10, EdgeKind::Prev, 3)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(levels, [vec![8, 9], vec![7]]);
    assert_eq!(graph.asked.borrow().len(), 2, "no level is fetched past the limit");
}

#[test]
fn a_walk_ends_after_an_error() {
    let graph = GraphDouble::new(&[(10, &[666]), (666, &[1])]);
    let mut walk = graph.walk(b"room", 10, EdgeKind::Prev, 100);
    assert_eq!(walk.next().unwrap().unwrap(), [666]);
    assert!(walk.next().unwrap().is_err());
    assert!(walk.next().is_none());
}

#[test]
fn single_event_lookups_use_the_batch_forms() {
    let graph = diamond();
    assert_eq!(graph.parents(b"room", 10, EdgeKind::Prev).unwrap(), [9, 8]);
    assert!(graph.parents(b"room", 5, EdgeKind::Prev).unwrap().is_empty());
    assert_eq!(graph.children(b"room", 5, EdgeKind::Prev).unwrap(), [6]);
}
