//! Generation-owned physical accounting indexes, prepared before source admission.

use super::*;
use std::cell::Cell;
use std::cmp::Ordering;

#[derive(Clone, Copy)]
struct Row {
    node: u64,
    out: u32,
    incoming: u32,
}

pub(super) struct Index {
    rows: Box<[Row]>,
    keys: Box<[u64]>,
    out: Box<[u64]>,
    incoming: Box<[u64]>,
}

thread_local! {
    static VISITS: Cell<usize> = const { Cell::new(0) };
}

fn visit() {
    VISITS.with(|visits| visits.set(visits.get() + 1));
}

pub(super) fn take_visits() -> usize {
    VISITS.with(|visits| visits.replace(0))
}

impl Index {
    pub(super) fn empty() -> Self {
        Self {
            rows: Box::new([]),
            keys: Box::new([]),
            out: Box::new([]),
            incoming: Box::new([]),
        }
    }

    pub(super) fn build(view: &CompactGraphView) -> Self {
        let nodes: Vec<_> = view
            .node_ids()
            .filter(|&id| view.raw_node(id).is_some())
            .collect();
        let mut keys: Vec<_> = nodes
            .iter()
            .copied()
            .filter(|&id| view.raw_node(id).unwrap().key.is_some())
            .collect();
        keys.sort_unstable_by(|&a, &b| {
            view.raw_node(a)
                .unwrap()
                .key
                .cmp(&view.raw_node(b).unwrap().key)
        });
        let mut outgoing = Vec::new();
        let mut incoming = Vec::new();
        for id in view.edge_ids() {
            if let Some(edge) = view.raw_edge(id) {
                outgoing.push((edge.source, id));
                incoming.push((edge.target, id));
            }
        }
        let compare = |a: &(u64, u64), b: &(u64, u64)| {
            let left = view.raw_edge(a.1).unwrap();
            let right = view.raw_edge(b.1).unwrap();
            (a.0, left.relationship.as_str(), a.1).cmp(&(b.0, right.relationship.as_str(), b.1))
        };
        outgoing.sort_unstable_by(compare);
        incoming.sort_unstable_by(compare);
        let mut out_at = 0;
        let mut in_at = 0;
        let rows = nodes
            .into_iter()
            .map(|node| {
                let row = Row {
                    node,
                    out: u32::try_from(out_at).expect("admitted graph ordinal"),
                    incoming: u32::try_from(in_at).expect("admitted graph ordinal"),
                };
                while outgoing
                    .get(out_at)
                    .is_some_and(|&(owner, _)| owner == node)
                {
                    out_at += 1;
                }
                while incoming.get(in_at).is_some_and(|&(owner, _)| owner == node) {
                    in_at += 1;
                }
                row
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            rows,
            keys: keys.into_boxed_slice(),
            out: outgoing
                .into_iter()
                .map(|(_, id)| id)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            incoming: incoming
                .into_iter()
                .map(|(_, id)| id)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        }
    }

    /// Retained array elements and fixed index/Arc headers, excluding allocator metadata.
    pub(super) fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + 2 * std::mem::size_of::<usize>()
            + std::mem::size_of::<Arc<Self>>()
            + self.rows.len() * std::mem::size_of::<Row>()
            + (self.keys.len() + self.out.len() + self.incoming.len()) * std::mem::size_of::<u64>()
    }

    pub(super) fn projected_bytes(nodes: usize, keys: usize, edges: usize) -> usize {
        Self::empty().bytes()
            + nodes * std::mem::size_of::<Row>()
            + (keys + 2 * edges) * std::mem::size_of::<u64>()
    }
}

impl CompactGraphView {
    pub(crate) fn algorithm_node_by_key(&self, key: &str) -> Option<NodeRef<'_>> {
        let at = self
            .algorithm
            .keys
            .binary_search_by(|&id| {
                visit();
                self.raw_node(id).unwrap().key.as_deref().unwrap().cmp(key)
            })
            .ok()?;
        self.physical().node(NodeId(self.algorithm.keys[at]))
    }

    pub(crate) fn algorithm_adjacency<'g>(
        &'g self,
        node: NodeId,
        direction: Direction,
        after: Option<(&str, EdgeId)>,
    ) -> impl Iterator<Item = (&'g str, EdgeId)> + 'g {
        let index = &self.algorithm;
        let row = index
            .rows
            .binary_search_by(|row| {
                visit();
                row.node.cmp(&node.0)
            })
            .ok();
        let side = |entries: &'g [u64], outgoing: bool, enabled: bool| {
            let entries = row.filter(|_| enabled).map_or(&[][..], |at| {
                let offset = |row: &Row| if outgoing { row.out } else { row.incoming } as usize;
                let start = offset(&index.rows[at]);
                let end = index.rows.get(at + 1).map_or(entries.len(), offset);
                &entries[start..end]
            });
            let start = after.map_or(0, |after| {
                entries.partition_point(|&id| {
                    visit();
                    let edge = self.raw_edge(id).unwrap();
                    (edge.relationship.as_str(), EdgeId(id)) <= after
                })
            });
            Side {
                view: self,
                ids: entries[start..].iter(),
            }
            .peekable()
        };
        Adjacency {
            out: side(&index.out, true, direction != Direction::In),
            incoming: side(&index.incoming, false, direction != Direction::Out),
        }
    }
}

struct Side<'g> {
    view: &'g CompactGraphView,
    ids: std::slice::Iter<'g, u64>,
}

impl<'g> Iterator for Side<'g> {
    type Item = (&'g str, EdgeId);

    fn next(&mut self) -> Option<Self::Item> {
        let &id = self.ids.next()?;
        visit();
        let edge = self.view.raw_edge(id).expect("effective indexed edge");
        Some((edge.relationship.as_str(), EdgeId(id)))
    }
}

struct Adjacency<'g> {
    out: std::iter::Peekable<Side<'g>>,
    incoming: std::iter::Peekable<Side<'g>>,
}

impl<'g> Iterator for Adjacency<'g> {
    type Item = (&'g str, EdgeId);

    fn next(&mut self) -> Option<Self::Item> {
        match (self.out.peek(), self.incoming.peek()) {
            (Some(a), Some(b)) => match a.cmp(b) {
                Ordering::Less => self.out.next(),
                Ordering::Greater => self.incoming.next(),
                Ordering::Equal => {
                    self.incoming.next();
                    self.out.next()
                }
            },
            (Some(_), None) => self.out.next(),
            (None, Some(_)) => self.incoming.next(),
            (None, None) => None,
        }
    }
}
