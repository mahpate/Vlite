//! Graph workload latency benchmark (EPIC-005 ITEM-021, NFR-002/NFR-003).
//!
//! cargo run --release --locked --example graph_bench -- \
//!     [--nodes 100000] [--edges 1000000] [--queries 1000] [--seed 1] [--dir PATH]
//!
//! Builds the seeded synthetic graph of `graph_support/workload.rs`
//! (uniformly random keyless edges of four relationship types, two labels,
//! one integer property per node and edge) in atomic graph batches of at most
//! 4,096 operations, then measures: build time, file bytes per node and per
//! edge, bytes appended per graph batch, read-only open/replay time, and
//! p50/p99 of `neighbors` (limit 100), `traverse` (2 hops, default limits)
//! and `shortest_path` (default limits) from seeded random nodes.
//!
//! The in-memory split of NFR-002 (records, adjacency, indexes, temporary
//! traversal memory) is measured separately by the allocation-counting
//! `graph_memory` example on the same workload, so no allocator
//! instrumentation slows these timings. Hybrid retrieval does not exist yet.
//! Results are only comparable on the DO-005 reference machine.

#[path = "graph_support/workload.rs"]
mod workload;

use std::error::Error;
use std::hint::black_box;
use std::time::{Duration, Instant};

use vlite::Database;
use vlite::property_graph::{PathOutcome, ReadOptions};

use workload::{Args, BATCH, GRAPH, Workload, build, fresh_path, nearest_rank};

/// p50 and p99 in microseconds (nearest rank).
fn percentiles(samples: &mut [Duration]) -> (f64, f64) {
    samples.sort_unstable();
    let rank = |p: f64| nearest_rank(samples, p).as_secs_f64() * 1e6;
    (rank(0.50), rank(0.99))
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::default().parse(std::env::args().skip(1))?;
    let (_temp, path) = fresh_path(&args, "graph-bench")?;
    println!(
        "graph_bench nodes={} edges={} queries={} seed={} batch={BATCH}",
        args.nodes, args.edges, args.queries, args.seed
    );

    let mut workload = Workload::new(&args);
    let built = build(&path, &mut workload)?;
    let (empty, total, batches) = (built.empty, built.total, built.batches);
    let node_bytes = built.with_nodes - empty;
    let edge_bytes = total - built.with_nodes;
    println!(
        "build_s={:.3} nodes_s={:.3} edges_s={:.3} batches={batches}",
        built.build_time.as_secs_f64(),
        built.node_time.as_secs_f64(),
        built.edge_time.as_secs_f64()
    );
    println!(
        "file_bytes={total} log_bytes_per_node={:.1} log_bytes_per_edge={:.1} \
         bytes_per_batch={:.0} bytes_per_op={:.1}",
        node_bytes as f64 / args.nodes as f64,
        edge_bytes as f64 / args.edges.max(1) as f64,
        (total - empty) as f64 / batches as f64,
        (total - empty) as f64 / (args.nodes + args.edges) as f64
    );

    let mut opens = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        let db = Database::open_read_only(&path)?;
        opens.push(start.elapsed());
        black_box(db.graph(GRAPH)?.view_sequence());
    }
    opens.sort_unstable();
    let open = opens[1].as_secs_f64();
    println!(
        "open_replay_s_median_of_3={open:.3} replay_s_per_million_edges={:.3}",
        open * 1e6 / args.edges.max(1) as f64
    );

    let db = Database::open_read_only(&path)?;
    let graph = db.graph(GRAPH)?;
    let options = ReadOptions::default();

    let mut samples = Vec::with_capacity(args.queries);
    let mut incomplete = 0;
    for _ in 0..args.queries {
        let request = workload.neighbors();
        let start = Instant::now();
        let read = graph.neighbors(&request, &options)?;
        samples.push(start.elapsed());
        incomplete += usize::from(!read.complete);
        black_box(read);
    }
    let (p50, p99) = percentiles(&mut samples);
    println!("neighbors_limit100_us p50={p50:.1} p99={p99:.1} incomplete={incomplete}");

    let mut samples = Vec::with_capacity(args.queries);
    let (mut incomplete, mut returned) = (0, 0);
    for _ in 0..args.queries {
        let request = workload.traverse();
        let start = Instant::now();
        let read = graph.traverse(&request, &options)?;
        samples.push(start.elapsed());
        incomplete += usize::from(!read.complete);
        returned += read.output.nodes.len();
        black_box(read);
    }
    let (p50, p99) = percentiles(&mut samples);
    println!(
        "traverse_2hop_defaults_us p50={p50:.1} p99={p99:.1} incomplete={incomplete} \
         mean_nodes={:.1}",
        returned as f64 / args.queries as f64
    );

    let mut samples = Vec::with_capacity(args.queries);
    let (mut found, mut none, mut exhausted) = (0, 0, 0);
    for _ in 0..args.queries {
        let request = workload.shortest_path();
        let start = Instant::now();
        let read = graph.shortest_path(&request, &options)?;
        samples.push(start.elapsed());
        match read.output {
            PathOutcome::Found { .. } => found += 1,
            PathOutcome::NoPathWithinHops => none += 1,
            _ => exhausted += 1,
        }
    }
    let (p50, p99) = percentiles(&mut samples);
    println!(
        "shortest_path_defaults_us p50={p50:.1} p99={p99:.1} found={found} \
         no_path={none} exhausted={exhausted}"
    );
    println!("memory_split=see_graph_memory_example; hybrid_retrieve=not_available");
    Ok(())
}
