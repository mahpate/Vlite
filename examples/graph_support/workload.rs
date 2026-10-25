//! Seeded synthetic graph workload shared by `graph_bench` (latency) and
//! `graph_memory` (allocations), EPIC-005 ITEM-021 (NFR-002/NFR-003).
//!
//! Nodes `n0..n{N-1}` carry alternating labels `Doc`/`Person` and one integer
//! property `w`. Edges are keyless, with uniformly random source and
//! destination (self-loops and parallel edges are possible), one of four
//! relationship types and an integer `w`. Everything is drawn from SplitMix64
//! in a fixed order and written to a fresh file with two-dimensional vectors
//! and no records, in atomic graph batches of at most 4,096 operations: all
//! node batches, then all edge batches. The same generator then draws the
//! read queries, so both examples issue identical writes and identical
//! queries for the same arguments.

// Each example uses a different subset of these helpers.
#![allow(dead_code)]

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vlite::property_graph::{GraphOp, Neighbors, NodeSelector, ShortestPath, Traverse};
use vlite::{CreateOptions, Database, Metadata, MetadataValue};

pub const BATCH: usize = 4_096;
pub const GRAPH: &str = "bench";
const TYPES: [&str; 4] = ["CITES", "LINKS", "MENTIONS", "OWNS"];
const LABELS: [&str; 2] = ["Doc", "Person"];

pub struct Args {
    pub nodes: u64,
    pub edges: u64,
    pub queries: usize,
    pub seed: u64,
    pub dir: Option<PathBuf>,
}

impl Default for Args {
    /// The NFR-003 size: 100,000 nodes, 1,000,000 edges, 1,000 queries per
    /// read operation, seed 1.
    fn default() -> Self {
        Self {
            nodes: 100_000,
            edges: 1_000_000,
            queries: 1_000,
            seed: 1,
            dir: None,
        }
    }
}

impl Args {
    /// Applies `--nodes`, `--edges`, `--queries`, `--seed` and `--dir`, each
    /// followed by its value, over `self`.
    pub fn parse(
        mut self,
        input: impl IntoIterator<Item = String>,
    ) -> Result<Self, Box<dyn Error>> {
        let mut input = input.into_iter();
        while let Some(flag) = input.next() {
            let value = input
                .next()
                .ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--nodes" => self.nodes = value.parse()?,
                "--edges" => self.edges = value.parse()?,
                "--queries" => self.queries = value.parse()?,
                "--seed" => self.seed = value.parse()?,
                "--dir" => self.dir = Some(PathBuf::from(value)),
                _ => return Err(format!("unknown flag {flag}").into()),
            }
        }
        if self.nodes == 0 || self.queries == 0 {
            return Err("--nodes and --queries must be positive".into());
        }
        Ok(self)
    }
}

/// SplitMix64.
pub struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

pub fn key(node: u64) -> String {
    format!("n{node}")
}

fn weight(rng: &mut Rng) -> Metadata {
    Metadata::from([(
        "w".to_owned(),
        MetadataValue::Integer(rng.below(100) as i64),
    )])
}

/// Generates the workload's graph batches in write order, then its queries.
pub struct Workload {
    rng: Rng,
    nodes: u64,
    edges: u64,
    next_node: u64,
    next_edge: u64,
}

impl Workload {
    pub fn new(args: &Args) -> Self {
        Self {
            rng: Rng(args.seed),
            nodes: args.nodes,
            edges: args.edges,
            next_node: 0,
            next_edge: 0,
        }
    }

    /// Replaces `ops` with the next batch of node creations; false once
    /// every node was generated.
    pub fn next_nodes(&mut self, ops: &mut Vec<GraphOp>) -> bool {
        ops.clear();
        while ops.len() < BATCH && self.next_node < self.nodes {
            let node = self.next_node;
            ops.push(GraphOp::CreateNode {
                key: key(node),
                labels: vec![LABELS[(node % 2) as usize].to_owned()],
                properties: weight(&mut self.rng),
            });
            self.next_node += 1;
        }
        !ops.is_empty()
    }

    /// Replaces `ops` with the next batch of edge creations; false once
    /// every edge was generated. Call after the last node batch.
    pub fn next_edges(&mut self, ops: &mut Vec<GraphOp>) -> bool {
        assert_eq!(self.next_node, self.nodes, "nodes are generated first");
        ops.clear();
        while ops.len() < BATCH && self.next_edge < self.edges {
            // Fields are evaluated, and so drawn, in source order.
            ops.push(GraphOp::CreateEdge {
                key: None,
                source: key(self.rng.below(self.nodes)),
                relationship: TYPES[self.rng.below(4) as usize].to_owned(),
                destination: key(self.rng.below(self.nodes)),
                properties: weight(&mut self.rng),
            });
            self.next_edge += 1;
        }
        !ops.is_empty()
    }

    /// `neighbors` of a random node with limit 100 (outgoing, the default).
    pub fn neighbors(&mut self) -> Neighbors {
        let mut request = Neighbors::new(NodeSelector::Key(key(self.rng.below(self.nodes))));
        request.limit = Some(100);
        request
    }

    /// `traverse` from a random node with the defaults (2 hops, outgoing).
    pub fn traverse(&mut self) -> Traverse {
        Traverse::new(vec![NodeSelector::Key(key(self.rng.below(self.nodes)))])
    }

    /// `shortest_path` between two random nodes with the defaults.
    pub fn shortest_path(&mut self) -> ShortestPath {
        let source = NodeSelector::Key(key(self.rng.below(self.nodes)));
        let destination = NodeSelector::Key(key(self.rng.below(self.nodes)));
        ShortestPath::new(source, destination)
    }
}

pub fn file_len(path: &Path) -> Result<u64, Box<dyn Error>> {
    Ok(std::fs::metadata(path)?.len())
}

/// A database path `<name>-<seed>.vlite` that does not exist yet, in `--dir`
/// (created when missing) or else in the returned temporary directory, which
/// must outlive the run.
pub fn fresh_path(args: &Args, name: &str) -> Result<(tempfile::TempDir, PathBuf), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let dir = args.dir.clone().unwrap_or_else(|| temp.path().to_owned());
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}-{}.vlite", args.seed));
    if path.exists() {
        return Err(format!("{} already exists", path.display()).into());
    }
    Ok((temp, path))
}

/// File sizes in bytes, batch count and phase times of one [`build`].
pub struct Built {
    /// After creating the empty graph.
    pub empty: u64,
    /// After the node batches.
    pub with_nodes: u64,
    /// After the edge batches and closing the database.
    pub total: u64,
    pub batches: u64,
    pub node_time: Duration,
    pub edge_time: Duration,
    /// Node and edge batches plus closing the database.
    pub build_time: Duration,
}

/// Creates a graph-enabled database at `path` with the graph [`GRAPH`] and
/// writes every batch of `workload`, each with `write_batch`.
pub fn build(path: &Path, workload: &mut Workload) -> Result<Built, Box<dyn Error>> {
    let mut db = Database::create_with_graph(path, 2, CreateOptions::default())?;
    db.create_graph(GRAPH)?;
    let empty = file_len(path)?;

    let start = Instant::now();
    let mut batches = 0_u64;
    let mut ops = Vec::with_capacity(BATCH);
    while workload.next_nodes(&mut ops) {
        db.graph_mut(GRAPH)?.write_batch(&ops)?;
        batches += 1;
    }
    let node_time = start.elapsed();
    let with_nodes = file_len(path)?;
    let edge_start = Instant::now();
    while workload.next_edges(&mut ops) {
        db.graph_mut(GRAPH)?.write_batch(&ops)?;
        batches += 1;
    }
    let edge_time = edge_start.elapsed();
    drop(db);
    let total = file_len(path)?;
    Ok(Built {
        empty,
        with_nodes,
        total,
        batches,
        node_time,
        edge_time,
        build_time: start.elapsed(),
    })
}

/// The nearest-rank `p` quantile of ascending, non-empty `sorted`.
pub fn nearest_rank<T: Copy>(sorted: &[T], p: f64) -> T {
    let index = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[index]
}
