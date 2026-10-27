//! Graph in-memory cost split (EPIC-005 ITEM-021, NFR-002).
//!
//! cargo run --release --locked --example graph_memory -- \
//!     [--smoke] [--nodes 100000] [--edges 1000000] [--queries 1000] [--seed 1] [--dir PATH]
//!
//! `--smoke` defaults to 10,000 nodes, 100,000 edges and 100 queries; explicit
//! flags override it. The workload, file layout and queries are those of
//! `graph_bench` (`graph_support/workload.rs`); this example measures heap
//! allocations instead of time, so latency runs stay uninstrumented. Progress
//! goes to stderr and the result is one JSON object on stdout (schema
//! `vlite.graph_memory.v1`). It contains no timings and, for the same
//! arguments, toolchain and target, the same values on every run.
//!
//! A benchmark-only global allocator (`graph_support/counting_alloc.rs`)
//! counts live requested heap bytes (`Layout::size`) and blocks for the whole
//! process, and the peak since a reset. Every measurement window runs on the
//! main thread while no other thread exists. Steps:
//!
//! 1. Build `<dir>/graph-memory-<seed>.vlite` with the public API, exactly
//!    as `graph_bench` does (log bytes come from file growth).
//! 2. Plan the same batches with the shared engine source, compiled into
//!    this example by `#[path]` (the engine is crate-private in `vlite`), the
//!    way `create_graph`/`write_batch` commit: validate, encode, apply.
//! 3. Check that the file holds exactly these frames: every encoded body,
//!    sequence, entry count and frame marker byte for byte (checksums
//!    excepted), back to back up to the end of the file.
//! 4. Replay the bodies into a fresh `GraphState` exactly as an open does
//!    (decode, apply in order) and count the retained bytes and the peak.
//!    Then `GraphState::release_components` drops the state one owned
//!    component at a time; the bytes freed by each drop are that
//!    component's bytes. Nothing is estimated: the components must add up
//!    to the retained bytes and the live count must return to its value
//!    before the replay.
//! 5. Count what `Database::open_read_only` on the file retains, and its
//!    peak. That includes the open database's own non-graph state, so it
//!    exceeds the replayed state by `beyond_replayed_state_bytes`.
//! 6. Through the public `GraphRef`, run the `graph_bench` queries one at a
//!    time and record, per call, the peak (temporary memory, including the
//!    result while it is built) and the bytes still held by the returned
//!    result. Each result is then dropped, and the live count must be back
//!    where it was.
//!
//! Every peak in the report (`peak_above_baseline_bytes`) is the highest live
//! count reached inside its window minus the live count when the window
//! opened. It is not a process peak: the planned frames, about the size of
//! the log, stay live through steps 4 and 5 and are not part of it. The
//! reference runner records the whole-process peak working set separately.
//!
//! Limitations: the numbers are requested heap bytes, not RSS, committed
//! memory or allocator overhead (headers, size classes, caching), and miss
//! memory that bypasses Rust's global allocator. A reallocation counts as
//! allocate, copy and free, so both blocks count toward a peak. The split
//! describes the state rebuilt by replay; a writer that has also planned and
//! rolled back batches can have a slightly different tree shape. Property
//! indexes do not exist (EPIC-008) and report zero; hybrid retrieval does
//! not exist (ITEM-039).

#[path = "graph_support/counting_alloc.rs"]
mod counting_alloc;
#[allow(dead_code, unused_imports)]
#[path = "../src/property_graph/engine/mod.rs"]
mod engine;
#[path = "graph_support/workload.rs"]
mod workload;

use std::error::Error;
use std::fmt::Write as _;
use std::path::Path;

use vlite::property_graph::{GraphOp, GraphRead, PathOutcome, ReadOptions};
use vlite::{Database, Metadata, MetadataValue};

use counting_alloc::{Counting, Usage};
use engine::{
    GraphBody, GraphEntry, GraphMutation, GraphState, MemoryComponent, Properties, PropertyValue,
};
use workload::{Args, BATCH, GRAPH, Workload, build, fresh_path, nearest_rank};

#[global_allocator]
static ALLOCATOR: Counting = Counting::new();

type Fallible<T> = Result<T, Box<dyn Error>>;

const SCHEMA: &str = "vlite.graph_memory.v1";

/// Every component in release order, with its JSON name.
const COMPONENTS: [(MemoryComponent, &str); 12] = [
    (MemoryComponent::NameIndex, "name_index"),
    (MemoryComponent::AttachmentOwners, "attachment_owners"),
    (MemoryComponent::GraphTable, "graph_table"),
    (MemoryComponent::GraphName, "graph_name"),
    (MemoryComponent::NodeRecords, "node_records"),
    (MemoryComponent::EdgeRecords, "edge_records"),
    (MemoryComponent::Attachments, "attachments"),
    (MemoryComponent::NodeKeyIndex, "node_key_index"),
    (MemoryComponent::EdgeKeyIndex, "edge_key_index"),
    (MemoryComponent::OutAdjacency, "out_adjacency"),
    (MemoryComponent::InAdjacency, "in_adjacency"),
    (MemoryComponent::LabelIndex, "label_index"),
];

fn slot(component: MemoryComponent) -> usize {
    // Exhaustive, so a new engine component fails to compile here.
    match component {
        MemoryComponent::NameIndex => 0,
        MemoryComponent::AttachmentOwners => 1,
        MemoryComponent::GraphTable => 2,
        MemoryComponent::GraphName => 3,
        MemoryComponent::NodeRecords => 4,
        MemoryComponent::EdgeRecords => 5,
        MemoryComponent::Attachments => 6,
        MemoryComponent::NodeKeyIndex => 7,
        MemoryComponent::EdgeKeyIndex => 8,
        MemoryComponent::OutAdjacency => 9,
        MemoryComponent::InAdjacency => 10,
        MemoryComponent::LabelIndex => 11,
    }
}

fn since(before: Usage, after: Usage) -> Fallible<Usage> {
    match (
        after.bytes.checked_sub(before.bytes),
        after.blocks.checked_sub(before.blocks),
    ) {
        (Some(bytes), Some(blocks)) => Ok(Usage { bytes, blocks }),
        _ => Err("live heap shrank inside a measurement window".into()),
    }
}

// Step 2: frames planned with the engine source.

/// One graph frame the adapter is expected to have appended.
struct Frame {
    sequence: u64,
    count: u32,
    body: Vec<u8>,
}

fn properties(metadata: &Metadata) -> Properties {
    metadata
        .iter()
        .map(|(field, value)| {
            let value = match value {
                MetadataValue::String(value) => PropertyValue::String(value.clone()),
                MetadataValue::Integer(value) => PropertyValue::Integer(*value),
                MetadataValue::Float(value) => PropertyValue::Float(*value),
                MetadataValue::Boolean(value) => PropertyValue::Boolean(*value),
                MetadataValue::GeoPoint {
                    latitude,
                    longitude,
                } => PropertyValue::GeoPoint {
                    latitude: *latitude,
                    longitude: *longitude,
                },
            };
            (field.clone(), value)
        })
        .collect()
}

/// The engine mutation the adapter plans for a workload operation.
fn mutation(op: &GraphOp) -> GraphMutation {
    match op {
        GraphOp::CreateNode {
            key,
            labels,
            properties: values,
        } => GraphMutation::CreateNode {
            key: key.clone(),
            labels: labels.iter().cloned().collect(),
            properties: properties(values),
        },
        GraphOp::CreateEdge {
            key,
            source,
            relationship,
            destination,
            properties: values,
        } => GraphMutation::CreateEdge {
            key: key.clone(),
            source: source.clone(),
            relationship: relationship.clone(),
            destination: destination.clone(),
            properties: properties(values),
        },
        _ => unreachable!("the workload only creates nodes and edges"),
    }
}

/// Validates, encodes and applies `body` at the next sequence, like the
/// adapter's commit.
fn commit(state: &mut GraphState, frames: &mut Vec<Frame>, body: GraphBody) -> Fallible<()> {
    let sequence = frames.len() as u64 + 1;
    state.validate(sequence, &body)?;
    let encoded = engine::encode_body(&body)?;
    state.apply(sequence, &body)?;
    frames.push(Frame {
        sequence,
        count: u32::try_from(body.entries.len())?,
        body: encoded,
    });
    Ok(())
}

fn commit_batch(
    state: &mut GraphState,
    frames: &mut Vec<Frame>,
    graph: engine::GraphId,
    ops: &[GraphOp],
) -> Fallible<()> {
    let mut planner = state.planner(frames.len() as u64 + 1, graph)?;
    for op in ops {
        planner.mutate(&mutation(op))?;
    }
    let body = planner.finish();
    commit(state, frames, body)
}

/// The frames of a fresh file after `create_graph` and the workload's
/// batches (sequences 1, 2, ...), and the workload, ready for its queries.
fn plan_frames(args: &Args) -> Fallible<(Vec<Frame>, Workload)> {
    let mut state = GraphState::new();
    let mut frames = Vec::new();
    let graph = state.free_graph_id().ok_or("no free graph ID")?;
    let create = GraphBody {
        graph_id: graph,
        entries: vec![GraphEntry::CreateGraph { name: GRAPH.into() }],
    };
    commit(&mut state, &mut frames, create)?;
    let mut workload = Workload::new(args);
    let mut ops = Vec::with_capacity(BATCH);
    while workload.next_nodes(&mut ops) {
        commit_batch(&mut state, &mut frames, graph, &ops)?;
    }
    while workload.next_edges(&mut ops) {
        commit_batch(&mut state, &mut frames, graph, &ops)?;
    }
    Ok((frames, workload))
}

// Step 3: the file holds exactly these frames.

/// `format::PREFIX_SIZE`: marker, op, collection, flags, sequence, ID, count,
/// payload length and a prefix checksum.
const FRAME_PREFIX: usize = 36;
/// `format::FOOTER_SIZE`: commit marker, checksum and sequence.
const FRAME_FOOTER: usize = 16;

/// The frame prefix without its checksum, as `format::graph_frame` writes it.
fn frame_prefix(frame: &Frame) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(FRAME_PREFIX - 4);
    prefix.extend_from_slice(b"VREC");
    prefix.push(engine::codec::OP_GRAPH);
    prefix.extend_from_slice(&0_u16.to_le_bytes());
    prefix.push(0);
    prefix.extend_from_slice(&frame.sequence.to_le_bytes());
    prefix.extend_from_slice(&0_u64.to_le_bytes());
    prefix.extend_from_slice(&frame.count.to_le_bytes());
    prefix.extend_from_slice(&(frame.body.len() as u32).to_le_bytes());
    prefix
}

/// Checks that `file` ends with exactly `frames`, back to back, and returns
/// the offset of the first one.
fn verify_frames(file: &[u8], frames: &[Frame]) -> Fallible<usize> {
    let first = frame_prefix(frames.first().ok_or("no frames")?);
    let start = file
        .windows(first.len())
        .position(|window| window == first)
        .ok_or("the create-graph frame is not in the file")?;
    let mut at = start;
    for frame in frames {
        let length = frame.body.len();
        let bytes = file
            .get(at..at + FRAME_PREFIX + length + FRAME_FOOTER)
            .ok_or_else(|| format!("frame {} is cut short", frame.sequence))?;
        let footer = &bytes[FRAME_PREFIX + length..];
        if bytes[..FRAME_PREFIX - 4] != frame_prefix(frame)[..]
            || bytes[FRAME_PREFIX..FRAME_PREFIX + length] != frame.body[..]
            || footer[..4] != *b"VCMT"
            || footer[8..] != frame.sequence.to_le_bytes()
        {
            return Err(format!("frame {} differs from the plan", frame.sequence).into());
        }
        at += bytes.len();
    }
    if at != file.len() {
        return Err("the file continues after the last planned frame".into());
    }
    Ok(start)
}

// Step 4: replay split.

/// Bytes freed by each component, in [`COMPONENTS`] order.
struct Released {
    components: [Usage; 12],
    /// Live usage after the whole state was released.
    after: Usage,
    /// False when a component drop increased the live count.
    monotonic: bool,
}

/// Releases `state`, whose allocations were live at `held`, component by
/// component.
fn release(state: GraphState, held: Usage) -> Released {
    let mut components = [Usage::default(); 12];
    let mut last = held;
    let mut monotonic = true;
    // The observer neither allocates nor frees.
    state.release_components(|component| {
        let now = ALLOCATOR.usage();
        match (
            last.bytes.checked_sub(now.bytes),
            last.blocks.checked_sub(now.blocks),
        ) {
            (Some(bytes), Some(blocks)) => {
                let slot = &mut components[slot(component)];
                slot.bytes += bytes;
                slot.blocks += blocks;
            }
            _ => monotonic = false,
        }
        last = now;
    });
    Released {
        components,
        after: ALLOCATOR.usage(),
        monotonic,
    }
}

struct Replay {
    retained: Usage,
    peak: usize,
    nodes: usize,
    edges: usize,
    released: Released,
    /// Live usage before the replay.
    before: Usage,
}

fn replay(frames: &[Frame]) -> Fallible<Replay> {
    let before = ALLOCATOR.usage();
    ALLOCATOR.reset_peak();
    let mut state = GraphState::new();
    for frame in frames {
        let body = engine::decode_body(&frame.body, frame.count)?;
        state.apply(frame.sequence, &body)?;
    }
    let held = ALLOCATOR.usage();
    let peak = ALLOCATOR.peak_bytes().saturating_sub(before.bytes);
    let graph = state.graph_by_name(GRAPH).ok_or("the graph is missing")?;
    let (nodes, edges) = (graph.node_count(), graph.edge_count());
    Ok(Replay {
        retained: since(before, held)?,
        peak,
        nodes,
        edges,
        released: release(state, held),
        before,
    })
}

// Step 6: reads.

#[derive(Default)]
struct ReadMemory {
    peaks: Vec<usize>,
    retained: Vec<usize>,
    unbalanced: usize,
}

impl ReadMemory {
    fn with_capacity(calls: usize) -> Self {
        Self {
            peaks: Vec::with_capacity(calls),
            retained: Vec::with_capacity(calls),
            unbalanced: 0,
        }
    }

    /// Measures one read; `inspect` sees the result before it is dropped
    /// and must not allocate.
    fn measure<T>(
        &mut self,
        read: impl FnOnce() -> Result<GraphRead<T>, vlite::Error>,
        inspect: impl FnOnce(&GraphRead<T>),
    ) -> Fallible<()> {
        let before = ALLOCATOR.usage();
        ALLOCATOR.reset_peak();
        let result = read()?;
        let held = ALLOCATOR.usage();
        let peak = ALLOCATOR.peak_bytes().saturating_sub(before.bytes);
        inspect(&result);
        drop(result);
        let after = ALLOCATOR.usage();
        self.peaks.push(peak);
        self.retained.push(since(before, held)?.bytes);
        self.unbalanced += usize::from(after != before);
        Ok(())
    }

    fn json(mut self, mut outcomes: Vec<(&'static str, Json)>) -> Json {
        self.peaks.sort_unstable();
        self.retained.sort_unstable();
        let quantiles = |sorted: &[usize]| {
            Json::Object(vec![
                ("p50", Json::from(nearest_rank(sorted, 0.50))),
                ("p99", Json::from(nearest_rank(sorted, 0.99))),
                ("max", Json::from(nearest_rank(sorted, 1.0))),
            ])
        };
        let mut fields = vec![
            ("calls", Json::from(self.peaks.len())),
            ("unbalanced_calls", Json::from(self.unbalanced)),
            ("peak_above_baseline_bytes", quantiles(&self.peaks)),
            ("retained_output_bytes", quantiles(&self.retained)),
        ];
        fields.append(&mut outcomes);
        Json::Object(fields)
    }
}

// Output.

/// A JSON value with static keys and strings that need no escaping.
#[derive(Debug, PartialEq)]
enum Json {
    Unsigned(u64),
    Signed(i64),
    /// One decimal, or `null` for a ratio over zero items.
    Ratio(Option<f64>),
    Text(&'static str),
    Bool(bool),
    Object(Vec<(&'static str, Json)>),
}

impl From<usize> for Json {
    fn from(value: usize) -> Self {
        Json::Unsigned(value as u64)
    }
}

impl From<u64> for Json {
    fn from(value: u64) -> Self {
        Json::Unsigned(value)
    }
}

fn ratio(bytes: usize, items: u64) -> Json {
    Json::Ratio((items > 0).then(|| bytes as f64 / items as f64))
}

fn usage(value: Usage) -> Json {
    Json::Object(vec![
        ("bytes", Json::from(value.bytes)),
        ("blocks", Json::from(value.blocks)),
    ])
}

impl Json {
    /// The value at `path` of nested object keys.
    #[cfg(test)]
    fn at(&self, path: &[&str]) -> &Json {
        path.iter().fold(self, |value, key| match value {
            Json::Object(fields) => {
                &fields
                    .iter()
                    .find(|(name, _)| name == key)
                    .unwrap_or_else(|| panic!("no field {key}"))
                    .1
            }
            _ => panic!("{key} is not inside an object"),
        })
    }

    fn write(&self, out: &mut String, indent: usize) {
        match self {
            Json::Unsigned(value) => write!(out, "{value}").unwrap(),
            Json::Signed(value) => write!(out, "{value}").unwrap(),
            Json::Ratio(Some(value)) => write!(out, "{value:.1}").unwrap(),
            Json::Ratio(None) => out.push_str("null"),
            Json::Text(text) => {
                debug_assert!(!text.contains(['"', '\\']) && !text.contains(char::is_control));
                write!(out, "\"{text}\"").unwrap()
            }
            Json::Bool(value) => write!(out, "{value}").unwrap(),
            Json::Object(fields) => {
                out.push('{');
                for (index, (name, value)) in fields.iter().enumerate() {
                    out.push_str(if index == 0 { "\n" } else { ",\n" });
                    out.push_str(&"  ".repeat(indent + 1));
                    write!(out, "\"{name}\": ").unwrap();
                    value.write(out, indent + 1);
                }
                if !fields.is_empty() {
                    out.push('\n');
                    out.push_str(&"  ".repeat(indent));
                }
                out.push('}');
            }
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out.push('\n');
        out
    }
}

/// Runs every step on a fresh file and returns the report and whether every
/// accounting check passed.
fn run(args: &Args) -> Fallible<(Json, bool)> {
    let (_temp, path) = fresh_path(args, "graph-memory")?;
    eprintln!("graph_memory: building {}", path.display());
    let built = build(&path, &mut Workload::new(args))?;

    eprintln!("graph_memory: planning frames with the engine source");
    let (frames, mut workload) = plan_frames(args)?;
    let first_frame_offset = verify_frames(&std::fs::read(&path)?, &frames)?;

    eprintln!("graph_memory: measuring replay and component split");
    let replay = replay(&frames)?;
    drop(frames);
    if (replay.nodes as u64, replay.edges as u64) != (args.nodes, args.edges) {
        return Err("the replayed graph does not hold the workload".into());
    }

    eprintln!("graph_memory: measuring read-only open and reads");
    let (open, open_peak, reads) = open_and_read(&path, args, &mut workload)?;

    let components = replay.released.components;
    let bytes = |component: MemoryComponent| components[slot(component)].bytes;
    let attributed = components.iter().fold(Usage::default(), |sum, part| Usage {
        bytes: sum.bytes + part.bytes,
        blocks: sum.blocks + part.blocks,
    });
    let unattributed_bytes = replay.retained.bytes as i64 - attributed.bytes as i64;
    let unattributed_blocks = replay.retained.blocks as i64 - attributed.blocks as i64;
    let released_to_baseline = replay.released.after == replay.before;
    let unbalanced_reads: usize = reads.iter().map(|(_, memory, _)| memory.unbalanced).sum();
    let ok = unattributed_bytes == 0
        && unattributed_blocks == 0
        && replay.released.monotonic
        && released_to_baseline
        && unbalanced_reads == 0;

    let (nodes, edges) = (args.nodes, args.edges);
    let node_key = bytes(MemoryComponent::NodeKeyIndex);
    let labels = bytes(MemoryComponent::LabelIndex);
    let edge_key = bytes(MemoryComponent::EdgeKeyIndex);
    let adjacency = bytes(MemoryComponent::OutAdjacency) + bytes(MemoryComponent::InAdjacency);
    let graph_overhead = bytes(MemoryComponent::NameIndex)
        + bytes(MemoryComponent::GraphTable)
        + bytes(MemoryComponent::GraphName);
    let logged = (built.total - built.empty) as usize;
    let node_log = (built.with_nodes - built.empty) as usize;
    let edge_log = (built.total - built.with_nodes) as usize;

    let report = Json::Object(vec![
        ("schema", Json::Text(SCHEMA)),
        (
            "workload",
            Json::Object(vec![
                ("nodes", Json::from(nodes)),
                ("edges", Json::from(edges)),
                ("queries", Json::from(args.queries)),
                ("seed", Json::from(args.seed)),
                ("batch", Json::from(BATCH)),
                ("graph_batches", Json::from(built.batches)),
            ]),
        ),
        (
            "allocator",
            Json::Object(vec![
                ("unit", Json::Text("requested_heap_bytes")),
                ("scope", Json::Text("process")),
                ("peak", Json::Text("above_live_bytes_at_window_start")),
                ("realloc", Json::Text("alloc_copy_free")),
            ]),
        ),
        (
            "log",
            Json::Object(vec![
                ("file_bytes", Json::from(built.total)),
                ("empty_graph_file_bytes", Json::from(built.empty)),
                ("node_batch_bytes", Json::from(node_log)),
                ("edge_batch_bytes", Json::from(edge_log)),
                ("bytes_per_node", ratio(node_log, nodes)),
                ("bytes_per_edge", ratio(edge_log, edges)),
                ("bytes_per_batch", ratio(logged, built.batches)),
            ]),
        ),
        (
            "frames",
            Json::Object(vec![
                ("count", Json::from(built.batches + 1)),
                ("first_offset", Json::from(first_frame_offset)),
                ("verified", Json::Bool(true)),
            ]),
        ),
        (
            "replayed_state",
            Json::Object(vec![
                ("retained", usage(replay.retained)),
                ("peak_above_baseline_bytes", Json::from(replay.peak)),
                (
                    "components",
                    Json::Object(
                        COMPONENTS
                            .iter()
                            .map(|(component, name)| (*name, usage(components[slot(*component)])))
                            .collect(),
                    ),
                ),
                ("unattributed_bytes", Json::Signed(unattributed_bytes)),
                ("unattributed_blocks", Json::Signed(unattributed_blocks)),
                ("released_to_baseline", Json::Bool(released_to_baseline)),
            ]),
        ),
        (
            "split",
            Json::Object(vec![
                (
                    "records",
                    Json::Object(vec![
                        (
                            "node_bytes",
                            Json::from(bytes(MemoryComponent::NodeRecords)),
                        ),
                        (
                            "edge_bytes",
                            Json::from(bytes(MemoryComponent::EdgeRecords)),
                        ),
                        (
                            "bytes_per_node",
                            ratio(bytes(MemoryComponent::NodeRecords), nodes),
                        ),
                        (
                            "bytes_per_edge",
                            ratio(bytes(MemoryComponent::EdgeRecords), edges),
                        ),
                    ]),
                ),
                (
                    "adjacency",
                    Json::Object(vec![
                        ("bytes", Json::from(adjacency)),
                        ("bytes_per_edge", ratio(adjacency, edges)),
                    ]),
                ),
                (
                    "key_label_indexes",
                    Json::Object(vec![
                        ("node_key_bytes", Json::from(node_key)),
                        ("label_bytes", Json::from(labels)),
                        ("edge_key_bytes", Json::from(edge_key)),
                        ("bytes_per_node", ratio(node_key + labels, nodes)),
                        ("bytes_per_edge", ratio(edge_key, edges)),
                    ]),
                ),
                (
                    "property_indexes",
                    Json::Object(vec![
                        ("bytes", Json::Unsigned(0)),
                        ("status", Json::Text("not_implemented_epic_008")),
                    ]),
                ),
                (
                    "attachments",
                    Json::Object(vec![
                        ("bytes", Json::from(bytes(MemoryComponent::Attachments))),
                        (
                            "owner_index_bytes",
                            Json::from(bytes(MemoryComponent::AttachmentOwners)),
                        ),
                    ]),
                ),
                ("graph_overhead_bytes", Json::from(graph_overhead)),
                ("total_bytes", Json::from(replay.retained.bytes)),
            ]),
        ),
        (
            "open_read_only",
            Json::Object(vec![
                ("retained", usage(open)),
                ("peak_above_baseline_bytes", Json::from(open_peak)),
                (
                    "beyond_replayed_state_bytes",
                    Json::Signed(open.bytes as i64 - replay.retained.bytes as i64),
                ),
            ]),
        ),
        (
            "reads",
            Json::Object(
                reads
                    .into_iter()
                    .map(|(name, memory, outcomes)| (name, memory.json(outcomes)))
                    .collect(),
            ),
        ),
        ("hybrid_retrieve", Json::Text("not_available")),
        ("ok", Json::Bool(ok)),
    ]);
    Ok((report, ok))
}

type Reads = Vec<(&'static str, ReadMemory, Vec<(&'static str, Json)>)>;

/// Step 5 and 6: retained bytes and peak of a read-only open, then the
/// per-call memory of the `graph_bench` reads.
fn open_and_read(
    path: &Path,
    args: &Args,
    workload: &mut Workload,
) -> Fallible<(Usage, usize, Reads)> {
    let before = ALLOCATOR.usage();
    ALLOCATOR.reset_peak();
    let db = Database::open_read_only(path)?;
    let open = since(before, ALLOCATOR.usage())?;
    let open_peak = ALLOCATOR.peak_bytes().saturating_sub(before.bytes);

    let graph = db.graph(GRAPH)?;
    let options = ReadOptions::default();
    let queries = args.queries;

    let mut neighbors = ReadMemory::with_capacity(queries);
    let mut incomplete = 0_usize;
    for _ in 0..queries {
        let request = workload.neighbors();
        neighbors.measure(
            || graph.neighbors(&request, &options),
            |read| incomplete += usize::from(!read.complete),
        )?;
    }
    let neighbors_outcomes = vec![("incomplete", Json::from(incomplete))];

    let mut traverse = ReadMemory::with_capacity(queries);
    let (mut incomplete, mut returned) = (0_usize, 0_usize);
    for _ in 0..queries {
        let request = workload.traverse();
        traverse.measure(
            || graph.traverse(&request, &options),
            |read| {
                incomplete += usize::from(!read.complete);
                returned += read.output.nodes.len();
            },
        )?;
    }
    let traverse_outcomes = vec![
        ("incomplete", Json::from(incomplete)),
        ("mean_nodes", ratio(returned, queries as u64)),
    ];

    let mut shortest = ReadMemory::with_capacity(queries);
    let (mut found, mut none, mut exhausted) = (0_usize, 0_usize, 0_usize);
    for _ in 0..queries {
        let request = workload.shortest_path();
        shortest.measure(
            || graph.shortest_path(&request, &options),
            |read| match read.output {
                PathOutcome::Found { .. } => found += 1,
                PathOutcome::NoPathWithinHops => none += 1,
                _ => exhausted += 1,
            },
        )?;
    }
    let shortest_outcomes = vec![
        ("found", Json::from(found)),
        ("no_path", Json::from(none)),
        ("exhausted", Json::from(exhausted)),
    ];

    Ok((
        open,
        open_peak,
        vec![
            ("neighbors_limit100", neighbors, neighbors_outcomes),
            ("traverse_2hop_defaults", traverse, traverse_outcomes),
            ("shortest_path_defaults", shortest, shortest_outcomes),
        ],
    ))
}

fn main() -> Fallible<()> {
    let mut input: Vec<String> = std::env::args().skip(1).collect();
    let smoke = input.iter().position(|flag| flag == "--smoke");
    let defaults = match smoke {
        Some(index) => {
            input.remove(index);
            Args {
                nodes: 10_000,
                edges: 100_000,
                queries: 100,
                ..Args::default()
            }
        }
        None => Args::default(),
    };
    let args = defaults.parse(input)?;
    let (report, ok) = run(&args)?;
    print!("{}", report.render());
    if !ok {
        return Err("allocation accounting checks failed (see the report)".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET_CHILD: &str = "VLITE_GRAPH_MEMORY_QUIET_CHILD";

    /// The allocation counters are process-wide and other tests allocate
    /// concurrently, so exact checks run alone in a child process. Returns
    /// true inside that child, which runs the checks.
    fn in_quiet_child(test: &str) -> bool {
        if std::env::var_os(QUIET_CHILD).is_some() {
            return true;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--test-threads=1", "--nocapture"])
            .env(QUIET_CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{test} failed in its child process:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }

    fn args(nodes: u64, edges: u64, queries: usize) -> Args {
        Args {
            nodes,
            edges,
            queries,
            seed: 7,
            dir: None,
        }
    }

    fn unsigned(report: &Json, path: &[&str]) -> u64 {
        match report.at(path) {
            Json::Unsigned(value) => *value,
            other => panic!("{path:?} is {other:?}"),
        }
    }

    fn component(report: &Json, name: &str) -> u64 {
        unsigned(report, &["replayed_state", "components", name, "bytes"])
    }

    /// Graph "g" with two labelled nodes, a keyed edge and an attachment;
    /// every temporary is dropped on return.
    fn keyed_state() -> GraphState {
        let mut state = GraphState::new();
        let mut frames = Vec::new();
        let graph = engine::GraphId(0);
        let create = GraphBody {
            graph_id: graph,
            entries: vec![GraphEntry::CreateGraph { name: "g".into() }],
        };
        commit(&mut state, &mut frames, create).unwrap();
        let mutations = [
            GraphMutation::CreateNode {
                key: "a".into(),
                labels: ["L".to_owned()].into(),
                properties: Properties::new(),
            },
            GraphMutation::CreateNode {
                key: "b".into(),
                labels: ["L".to_owned()].into(),
                properties: Properties::new(),
            },
            GraphMutation::CreateEdge {
                key: Some("e".into()),
                source: "a".into(),
                relationship: "R".into(),
                destination: "b".into(),
                properties: Properties::new(),
            },
            GraphMutation::Attach {
                node: "a".into(),
                collection: 0,
                record: 7,
            },
        ];
        let mut planner = state.planner(2, graph).unwrap();
        for mutation in &mutations {
            planner.mutate(mutation).unwrap();
        }
        let body = planner.finish();
        commit(&mut state, &mut frames, body).unwrap();
        state
    }

    #[test]
    fn workload_split_attributes_every_replayed_byte() {
        if !in_quiet_child("tests::workload_split_attributes_every_replayed_byte") {
            return;
        }
        // Two node and two edge batches.
        let (report, ok) = run(&args(4_200, 5_000, 12)).unwrap();
        assert!(ok, "{}", report.render());
        assert_eq!(report.at(&["ok"]), &Json::Bool(true));
        assert_eq!(unsigned(&report, &["frames", "count"]), 5);
        assert_eq!(
            report.at(&["replayed_state", "unattributed_bytes"]),
            &Json::Signed(0)
        );
        assert_eq!(
            report.at(&["replayed_state", "unattributed_blocks"]),
            &Json::Signed(0)
        );
        let sum: u64 = COMPONENTS
            .iter()
            .map(|(_, name)| component(&report, name))
            .sum();
        assert_eq!(
            sum,
            unsigned(&report, &["replayed_state", "retained", "bytes"])
        );
        // A window's peak is never below what it still holds at the end.
        for window in ["replayed_state", "open_read_only"] {
            assert!(
                unsigned(&report, &[window, "peak_above_baseline_bytes"])
                    >= unsigned(&report, &[window, "retained", "bytes"]),
                "{window}"
            );
        }
        for name in [
            "graph_table",
            "graph_name",
            "name_index",
            "node_records",
            "edge_records",
            "node_key_index",
            "out_adjacency",
            "in_adjacency",
            "label_index",
        ] {
            assert!(component(&report, name) > 0, "{name}");
        }
        // Keyless edges and no attachments allocate nothing in these.
        for name in ["edge_key_index", "attachments", "attachment_owners"] {
            assert_eq!(component(&report, name), 0, "{name}");
        }
        assert_eq!(
            unsigned(&report, &["split", "property_indexes", "bytes"]),
            0
        );
        for read in [
            "neighbors_limit100",
            "traverse_2hop_defaults",
            "shortest_path_defaults",
        ] {
            assert_eq!(unsigned(&report, &["reads", read, "calls"]), 12);
            assert_eq!(unsigned(&report, &["reads", read, "unbalanced_calls"]), 0);
            assert!(
                unsigned(
                    &report,
                    &["reads", read, "peak_above_baseline_bytes", "max"]
                ) > 0
            );
        }
        // An open holds the replayed state plus the database itself.
        let Json::Signed(beyond) = report.at(&["open_read_only", "beyond_replayed_state_bytes"])
        else {
            panic!("beyond_replayed_state_bytes is not signed")
        };
        assert!(*beyond > 0);
    }

    #[test]
    fn zero_edges_leave_edge_components_empty() {
        if !in_quiet_child("tests::zero_edges_leave_edge_components_empty") {
            return;
        }
        let (report, ok) = run(&args(10, 0, 3)).unwrap();
        assert!(ok, "{}", report.render());
        assert!(component(&report, "node_records") > 0);
        for name in [
            "edge_records",
            "edge_key_index",
            "out_adjacency",
            "in_adjacency",
        ] {
            assert_eq!(component(&report, name), 0, "{name}");
        }
        assert_eq!(unsigned(&report, &["frames", "count"]), 2);
        for path in [
            &["log", "bytes_per_edge"][..],
            &["split", "records", "bytes_per_edge"],
            &["split", "adjacency", "bytes_per_edge"],
        ] {
            assert_eq!(report.at(path), &Json::Ratio(None), "{path:?}");
        }
    }

    #[test]
    fn keyed_edges_and_attachments_are_attributed_and_empty_states_are_zero() {
        if !in_quiet_child(
            "tests::keyed_edges_and_attachments_are_attributed_and_empty_states_are_zero",
        ) {
            return;
        }
        // Warm up: one-time lazy runtime allocations (seen on macOS stable)
        // stay live forever and must not count as graph state; a real leak
        // recurs on the measured build below.
        drop(keyed_state());
        let before = ALLOCATOR.usage();
        let empty = release(GraphState::new(), before);
        assert!(empty.monotonic);
        assert_eq!(empty.components, [Usage::default(); 12]);
        assert_eq!(empty.after, before);

        let state = keyed_state();
        let held = ALLOCATOR.usage();
        let retained = since(before, held).unwrap();
        let released = release(state, held);
        assert!(released.monotonic);
        assert_eq!(released.after, before);
        let sum = released
            .components
            .iter()
            .fold(Usage::default(), |sum, part| Usage {
                bytes: sum.bytes + part.bytes,
                blocks: sum.blocks + part.blocks,
            });
        assert_eq!(sum, retained);
        for (component, name) in COMPONENTS {
            // Every component allocates in this state, properties excepted
            // (empty maps allocate nothing).
            assert!(released.components[slot(component)].bytes > 0, "{name}");
        }
    }

    #[test]
    fn json_output_is_stable() {
        let report = Json::Object(vec![
            ("schema", Json::Text(SCHEMA)),
            (
                "nested",
                Json::Object(vec![
                    ("count", Json::Unsigned(3)),
                    ("delta", Json::Signed(-2)),
                    ("ratio", ratio(10, 4)),
                    ("none", ratio(10, 0)),
                    ("ok", Json::Bool(true)),
                    ("empty", Json::Object(Vec::new())),
                ]),
            ),
        ]);
        assert_eq!(
            report.render(),
            "{\n  \"schema\": \"vlite.graph_memory.v1\",\n  \"nested\": {\n    \
             \"count\": 3,\n    \"delta\": -2,\n    \"ratio\": 2.5,\n    \"none\": null,\n    \
             \"ok\": true,\n    \"empty\": {}\n  }\n}\n"
        );
        assert_eq!(report.at(&["nested", "count"]), &Json::Unsigned(3));
    }
}
