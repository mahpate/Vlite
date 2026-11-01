//! The memory the change feed and log export hold to check a precision
//! log as full replay checks it (the P3a review of 6e26f685), counted in
//! requested heap bytes (`Layout::size`) of the blocks that the threads of
//! a measuring session allocate, by the lib test binary's only global
//! allocator (`crate::test_globals`).
//! Beside the frame being read, the check holds replay's graph state for
//! the prefix read, built by replay's own code (`SnapshotBuilder`, then
//! `frame::replay`), the live record IDs of each collection, and
//! bookkeeping that does not grow with records or graph entries. A graph
//! snapshot spread over many frames is loaded body by body, so no reader
//! holds the decoded snapshot beside the state built from it.
//!
//! These are regression guards for that accounting, not fail-first
//! tests. The counts exclude allocator headers and size-class rounding,
//! reported as the most live blocks; the record index bound relies on the
//! node layout and minimum fill of the standard library's `BTreeMap`,
//! which a toolchain may change, failing these guards.

use super::limits::{committed, imaged, record, store, with};
use super::*;
use crate::changes::read_changes;
use crate::property_graph::NodePatch;
use crate::test_globals::{BLOCKS, LIVE, MOST_BLOCKS, PEAK, measuring};
use crate::{MAX_BATCH_OPERATIONS, MetadataValue};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering::Relaxed;

/// What a measured call allocated, above the live bytes and blocks when
/// it started.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Use {
    /// The most live bytes at any point.
    pub(super) peak: isize,
    /// The live bytes when it returned, its result included.
    pub(super) retained: isize,
    /// The most live blocks at any point.
    blocks: isize,
}

/// `f`'s result and what it allocated. Measurements nest.
pub(super) fn measured<T>(f: impl FnOnce() -> T) -> (T, Use) {
    let (live, blocks) = (LIVE.load(Relaxed), BLOCKS.load(Relaxed));
    let outer = (PEAK.swap(live, Relaxed), MOST_BLOCKS.swap(blocks, Relaxed));
    let value = f();
    let used = Use {
        peak: PEAK.fetch_max(outer.0, Relaxed) - live,
        retained: LIVE.load(Relaxed) - live,
        blocks: MOST_BLOCKS.fetch_max(outer.1, Relaxed) - blocks,
    };
    (value, used)
}

/// The live bytes `value` holds: what dropping it frees.
pub(super) fn held<T>(value: T) -> isize {
    -measured(move || drop(value)).1.retained
}

/// `f` with the precision gate open, run on a thread of a rayon pool whose
/// threads all count for one measuring session, so replay's parallel
/// decoding is counted too; one measuring test at a time.
pub(super) fn counting<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    counting_on(4, f)
}

/// `counting` on a pool of `threads`. Each thread enrolls in a job, once
/// it has started and joined crossbeam's epoch, which it does as it first
/// looks for work, so neither counts. An idle thread of a larger pool
/// still counts what crossbeam's epoch allocates as it looks for work,
/// such as a 2072-byte node for its bag of deferred frees, so a
/// measurement of serial code that must be exact, such as `serially`'s
/// or `canonical.rs`'s of float16 writes, uses one.
pub(super) fn counting_on<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
    let open = measuring();
    let session = open.session();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();
    pool.broadcast(|_| session.enroll());
    pool.install(|| {
        let _gate = Activation::on();
        f()
    })
}

/// The bytes a `BTreeMap<u64, bool>` of `records` entries may hold: at
/// most `(records + 4) / 5` nodes, as every node but the root holds at
/// least five entries; of 112 bytes, and at most `(nodes + 3) / 6` of them
/// internal, of 96 bytes more, as every internal node but the root has at
/// least six children: `25.6 * records + 150.4`.
fn index_bound(records: usize) -> isize {
    ((128 * records + 755) / 5) as isize
}

/// The fewest bytes a `BTreeMap<u64, bool>` of `records` entries holds:
/// a node of at least 112 bytes for every 11 entries.
fn index_floor(records: usize) -> isize {
    (112 * records.div_ceil(11)) as isize
}

/// `f` measured exactly, as serial code needs: `counting` on a pool of
/// one thread, so no idle thread of the pool does crossbeam's epoch
/// bookkeeping meanwhile.
fn serially<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    counting_on(1, f)
}

/// The live bytes `value` holds, measured by a drop that, as any drop of
/// it must, allocates nothing: a measurement that saw an allocation, or a
/// negative hold, was not of `value` alone.
fn held_alone<T>(value: T) -> isize {
    let ((), used) = measured(move || drop(value));
    assert_eq!(used.peak, 0, "dropping allocated: {used:?}");
    -used.retained
}

/// Record `i` under a sparse ID, scattered over `0..2^62`.
fn sparse(i: u64) -> u64 {
    i.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15) & ((1 << 62) - 1)
}

#[test]
fn record_index_stays_within_its_node_bound() {
    serially(|| {
        let insert = |keys: &mut dyn Iterator<Item = u64>| {
            let mut index = BTreeMap::new();
            for key in keys {
                index.insert(key, false);
            }
            index
        };
        for records in [0, 1, 5, 11, 12, 67, 1_000, 4_096, 25_000] {
            let n = records as u64;
            let mut churned = insert(&mut (0..4 * n).map(sparse));
            for i in (0..4 * n).filter(|i| i % 4 != 0) {
                churned.remove(&sparse(i));
            }
            let mut drained = insert(&mut (0..4 * n));
            for key in 0..3 * n {
                drained.remove(&key);
            }
            let indexes = [
                ("ascending", insert(&mut (0..n))),
                ("descending", insert(&mut (0..n).rev())),
                ("scattered", insert(&mut (0..n).map(sparse))),
                ("churned", churned),
                ("drained", drained),
            ];
            for (pattern, index) in indexes {
                assert_eq!(index.len(), records);
                let bytes = held_alone(index);
                assert!(
                    (index_floor(records)..=index_bound(records)).contains(&bytes),
                    "{pattern} {records}: {bytes} bytes"
                );
            }
        }
    });
}

/// Idle threads of a counting pool search for work, pinning crossbeam's
/// process-wide epoch, and every 128th pin collects: it unlinks the epoch
/// participants of threads that exited, deferring their frees, and once
/// its thread's bag of 64 deferred frees is full, allocates a 2072-byte
/// queue node for the bag. A serial measurement, such as of the record
/// index, sees only what it measures meanwhile, whichever threads, never
/// enrolled, do the same (the review of 6db4d33d).
#[test]
fn serial_measurements_exclude_idle_workers_epoch_work() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    /// Threads that each wait to search for work, then search until
    /// stopped.
    #[derive(Default)]
    struct Searching {
        started: AtomicUsize,
        go: AtomicBool,
        searches: AtomicUsize,
        stop: AtomicBool,
    }
    impl Searching {
        fn search(&self) {
            self.started.fetch_add(1, SeqCst);
            while !self.go.load(SeqCst) {
                std::thread::yield_now();
            }
            while !self.stop.load(SeqCst) {
                rayon::yield_now();
                self.searches.fetch_add(1, SeqCst);
            }
        }

        /// Once `threads` started: lets them search, `1_024` times each.
        fn searched(&self, threads: usize) {
            while self.started.load(SeqCst) < threads {
                std::thread::yield_now();
            }
            self.go.store(true, SeqCst);
            while self.searches.load(SeqCst) < 1_024 * threads {
                std::thread::yield_now();
            }
        }
    }
    // The threads of another pool, never enrolled.
    let others = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    let theirs = Arc::new(Searching::default());
    others.spawn_broadcast({
        let theirs = theirs.clone();
        move |_| theirs.search()
    });
    serially(|| {
        retire_epoch_participants(128);
        let index = BTreeMap::from([(1u64, false)]);
        // The idle threads of the counting pool, if any.
        let (idle, ours) = (rayon::current_num_threads() - 1, Searching::default());
        rayon::scope(|scope| {
            for _ in 0..idle {
                scope.spawn(|_| ours.search());
            }
            let ((), used) = measured(|| {
                drop(index);
                ours.searched(idle);
                theirs.searched(others.current_num_threads());
            });
            ours.stop.store(true, SeqCst);
            theirs.stop.store(true, SeqCst);
            assert_eq!((used.peak, used.retained), (0, -112), "{used:?}");
        });
    });
}

/// Threads never enrolled join crossbeam's epoch, `participants` of them
/// at once, and exit.
fn retire_epoch_participants(participants: usize) {
    std::thread::spawn(move || {
        let mut threads = Vec::new();
        let pools: Vec<_> = (0..participants.div_ceil(4))
            .map(|_| {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(4)
                    .spawn_handler(|thread| {
                        threads.push(std::thread::spawn(move || thread.run()));
                        Ok(())
                    })
                    .build()
                    .unwrap();
                pool.broadcast(|_| ());
                pool
            })
            .collect();
        drop(pools);
        for thread in threads {
            thread.join().unwrap();
        }
    })
    .join()
    .unwrap();
}

/// Freeing blocks the measuring session never counted, allocated by a
/// thread not enrolled or counted by an earlier session, counts nothing.
#[test]
fn freeing_blocks_the_session_never_counted_counts_nothing() {
    let before = vec![0u8; 1_000];
    let earlier = counting_on(1, || vec![0u8; 2_000]);
    counting_on(1, move || {
        let ((), used) = measured(|| drop((before, earlier)));
        assert_eq!((used.peak, used.retained), (0, 0));
    });
}

/// A counted block that a thread not enrolled frees is no longer counted.
/// The block passes between the threads through an atomic pointer, not a
/// lock: on macOS, the standard library's `Mutex` boxes its 64-byte
/// `pthread_mutex_t` on its first lock, which the measured thread took,
/// and keeps it until the mutex drops, so a lock here measured
/// `(4064, 64)` (`b522054a`'s CI).
#[test]
fn a_counted_block_freed_on_another_thread_stops_counting() {
    use std::ptr::null_mut;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering::SeqCst};
    struct Done<'a>(&'a AtomicBool);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.store(true, SeqCst);
        }
    }
    let (slot, freed, done) = (
        AtomicPtr::new(null_mut::<[u8; 4_000]>()),
        AtomicBool::new(false),
        AtomicBool::new(false),
    );
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(SeqCst) {
                let block = slot.swap(null_mut(), SeqCst);
                if block.is_null() {
                    std::thread::yield_now();
                } else {
                    // SAFETY: `block` came from `Box::into_raw`, and the
                    // swap took it from the slot, its only other holder.
                    drop(unsafe { Box::from_raw(block) });
                    freed.store(true, SeqCst);
                }
            }
        });
        let _done = Done(&done);
        counting_on(1, || {
            let ((), used) = measured(|| {
                slot.store(Box::into_raw(Box::new([0u8; 4_000])), SeqCst);
                while !freed.load(SeqCst) {
                    std::thread::yield_now();
                }
            });
            assert_eq!((used.peak, used.retained), (4_000, 0));
        });
    });
}

/// What the measured thread allocates for state built on its first use,
/// such as macOS's boxed `pthread_mutex_t` above, counts in the window of
/// that use and stays held while the state lives; a later use allocates
/// nothing. A fixture must build such state before it measures.
#[test]
fn state_built_on_first_use_in_a_window_counts_there() {
    use std::sync::OnceLock;
    let lazy = OnceLock::<Box<[u8; 64]>>::new();
    counting_on(1, || {
        let first = || {
            lazy.get_or_init(|| Box::new([0; 64]));
            drop(vec![0u8; 4_000]);
        };
        let ((), used) = measured(first);
        assert_eq!((used.peak, used.retained), (4_064, 64));
        let ((), used) = measured(first);
        assert_eq!((used.peak, used.retained), (4_000, 0));
    });
}

/// Rayon's workers pin crossbeam's process-wide epoch whenever they look
/// for work, and a pin may run the deferred frees of garbage that other
/// threads left, such as the epoch participant and last sealed bag of
/// each thread that exited: frees of blocks this session never counted.
/// Such frees by idle workers inside a measurement failed the record
/// index bound (`f6a45564`'s CI); a window that frees nothing of its own
/// must not see them.
#[test]
fn workers_freeing_other_threads_epoch_garbage_count_nothing() {
    counting(|| {
        // Threads never enrolled use the epoch and exit.
        std::thread::spawn(|| {
            for _ in 0..8 {
                let mut threads = Vec::new();
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(4)
                    .spawn_handler(|thread| {
                        threads.push(std::thread::spawn(move || thread.run()));
                        Ok(())
                    })
                    .build()
                    .unwrap();
                pool.broadcast(|_| ());
                drop(pool);
                for thread in threads {
                    thread.join().unwrap();
                }
            }
        })
        .join()
        .unwrap();
        let ((), used) = measured(|| {
            rayon::broadcast(|_| {
                for _ in 0..512 {
                    rayon::yield_now();
                }
            });
        });
        assert!(used.retained >= 0, "{used:?}");
    });
}

/// Checking one frame of a log.
#[derive(Clone, Copy, Debug)]
struct Step {
    operation: u8,
    /// Its length on disk.
    length: usize,
    /// The live bytes above the scan's start before it was read.
    before: isize,
    used: Use,
    /// The check held a graph state after it.
    graph: bool,
}

impl Step {
    /// The most live bytes above the scan's start while it was checked.
    fn top(&self) -> isize {
        self.before + self.used.peak
    }

    /// The bytes it allocated beyond what it kept: the frame read and
    /// decoded, and any transient of changing the state.
    fn transient(&self) -> isize {
        self.used.peak - self.used.retained.max(0)
    }
}

/// The image check of a whole log, as the feed and export run it.
struct Scan {
    steps: Vec<Step>,
    /// What the check held after the last frame: replay's graph state,
    /// each collection's live record count and record index, and the rest.
    graph: isize,
    indexes: Vec<(usize, isize)>,
    rest: isize,
    /// The most live blocks above the start.
    blocks: isize,
}

impl Scan {
    fn peak(&self) -> isize {
        self.steps.iter().map(Step::top).max().unwrap_or(0)
    }
}

fn scanned(log: &[u8], key: Option<&EncryptionKey>) -> Scan {
    let mut file = Cursor::new(log);
    let header = format::read_log_header_with(&mut file, key, None).unwrap();
    let mut frames = 0;
    let mut offset = header.start;
    while let Some(frame) = format::next_frame(&mut file, &header, offset).unwrap() {
        offset += frame.disk.len() as u64;
        frames += 1;
    }
    let mut steps = Vec::with_capacity(frames);
    let mut check = ImageCheck::new(
        header.version,
        header.flags,
        header.dimensions,
        header.collection_floor,
    )
    .unwrap();
    let ((), whole) = measured(|| {
        let start = LIVE.load(Relaxed);
        let mut offset = header.start;
        loop {
            let before = LIVE.load(Relaxed) - start;
            let (read, used) = measured(|| {
                let frame =
                    format::next_checked_frame(&mut file, &header, offset, Some(&mut check))
                        .unwrap()?;
                check.frame(&frame, offset).unwrap();
                Some((frame.operation, frame.disk.len()))
            });
            let Some((operation, length)) = read else {
                break;
            };
            offset += length as u64;
            steps.push(Step {
                operation,
                length,
                before,
                used,
                graph: check.graph.is_some(),
            });
        }
        check.end(offset).unwrap();
    });
    assert_eq!(steps.len(), frames);
    let graph = held(check.graph.take());
    let indexes = check
        .described
        .values_mut()
        .map(|live| {
            let records = live.records.len();
            (records, held(std::mem::take(&mut live.records)))
        })
        .collect();
    Scan {
        steps,
        graph,
        indexes,
        rest: held(check),
        blocks: whole.blocks,
    }
}

/// What decoding each graph snapshot body of the plaintext `image` alone
/// takes, and the bytes all the decoded bodies hold together.
fn decoded(image: &[u8]) -> (Vec<Use>, isize) {
    let (_, frames) = split(image);
    let snapshots: Vec<_> = frames
        .iter()
        .filter(|frame| frame[4] == format::OP_GRAPH_SNAPSHOT)
        .collect();
    let mut uses = Vec::with_capacity(snapshots.len());
    let mut bodies = Vec::with_capacity(snapshots.len());
    let ((), all) = measured(|| {
        for frame in &snapshots {
            let (body, used) = measured(|| {
                crate::property_graph::frame::decode_frame(
                    Some(u64_at(frame, 8)),
                    u64_at(frame, 16),
                    u32_at(frame, 24),
                    super::limits::body(frame),
                    0,
                )
                .unwrap()
            });
            uses.push(used);
            bodies.push(body);
        }
    });
    (uses, all.retained)
}

/// What the feed's graph projection (`FeedGraphs`) holds over `log`: the
/// most it held, with the transient of decoding and projecting one graph
/// frame into its events, beside the frame read.
fn projected(log: &[u8], key: Option<&EncryptionKey>) -> isize {
    let mut file = Cursor::new(log);
    let header = format::read_log_header_with(&mut file, key, None).unwrap();
    let collections = BTreeMap::from([(0, ("default".to_owned(), 2)), (1, ("c".to_owned(), 2))]);
    let mut graphs = crate::property_graph::feed::FeedGraphs::default();
    let mut most = 0;
    let ((), _) = measured(|| {
        let start = LIVE.load(Relaxed);
        let mut offset = header.start;
        while let Some(frame) = format::next_frame(&mut file, &header, offset).unwrap() {
            offset += frame.disk.len() as u64;
            if matches!(frame.operation, 12 | 15) {
                let kept = LIVE.load(Relaxed) - start - held_by(&frame);
                let (events, used) =
                    measured(|| graphs.frame(&frame, offset, &collections).unwrap());
                drop(events);
                most = most.max(kept + used.peak);
            }
        }
    });
    most
}

/// The bytes the buffers of `frame` hold.
fn held_by(frame: &format::RawFrame) -> isize {
    (frame.disk.capacity() + frame.plain.capacity()) as isize
}

/// A complete precision image of a store and a log of it followed by the
/// frames that `after` commits.
struct Scenario {
    name: &'static str,
    image: Vec<u8>,
    floor: u64,
    log: Vec<u8>,
}

fn scenario(
    name: &'static str,
    db: Database,
    after: impl FnOnce(&mut Database) -> Result<()>,
) -> Scenario {
    let (image, floor) = imaged(&db);
    let log = with(&image, &[&committed(&image, after)]);
    Scenario {
        name,
        image,
        floor,
        log,
    }
}

fn written(db: &mut Database, ops: Vec<GraphOp>) -> Result<()> {
    for batch in ops.chunks(1_000) {
        db.graph_mut("g")?.write_batch(batch)?;
    }
    Ok(())
}

fn mutated(db: &mut Database, mutations: impl Iterator<Item = Mutation>) -> Result<()> {
    let mutations: Vec<_> = mutations.collect();
    for batch in mutations.chunks(MAX_BATCH_OPERATIONS) {
        db.collection_mut("c")?.write_batch(batch)?;
    }
    Ok(())
}

fn node(key: String, labels: Vec<String>, properties: Metadata) -> GraphOp {
    GraphOp::CreateNode {
        key,
        labels,
        properties,
    }
}

fn plain(key: String) -> GraphOp {
    node(key, Vec::new(), Metadata::new())
}

fn deleted(key: String) -> GraphOp {
    GraphOp::DeleteNode {
        key,
        expected: Expected::Any,
    }
}

fn text(fields: usize, tag: usize) -> Metadata {
    (0..fields)
        .map(|field| {
            let value = format!("{tag:08}-{field:02}-{}", "v".repeat(52));
            (format!("p{field}"), MetadataValue::String(value))
        })
        .collect()
}

/// A store with `records` sparse records in `c` and an empty graph `g`.
fn graphed(records: u64) -> Database {
    let mut db = store(0);
    mutated(&mut db, (0..records).map(|i| record(sparse(i)))).unwrap();
    db.create_graph("g").unwrap();
    db
}

fn scenarios() -> [Scenario; 3] {
    // Sparse record and node IDs, churned in the image and after it.
    let mut db = graphed(3_000);
    written(
        &mut db,
        (0..3_000).map(|i| plain(format!("n{i}"))).collect(),
    )
    .unwrap();
    written(
        &mut db,
        (0..2_000).map(|i| deleted(format!("n{i}"))).collect(),
    )
    .unwrap();
    let churned = scenario("sparse, churned", db, |db| {
        mutated(db, (3_000..7_000).map(|i| record(sparse(i))))?;
        mutated(
            db,
            (3_000..6_500).map(|i| Mutation::Delete { id: sparse(i) }),
        )?;
        written(db, (0..1_500).map(|i| plain(format!("m{i}"))).collect())?;
        written(db, (0..1_000).map(|i| deleted(format!("m{i}"))).collect())
    });
    // Labelled nodes and edges with eight 64-byte properties each.
    let mut db = graphed(0);
    let labels = |i: usize| vec!["person".to_owned(), format!("l{}", i % 7)];
    let nodes = (0..2_000).map(|i| node(format!("n{i}"), labels(i), text(8, i)));
    written(&mut db, nodes.collect()).unwrap();
    let edge = |i: usize, relationship: &str| GraphOp::CreateEdge {
        key: Some(format!("e{i}-{relationship}")),
        source: format!("n{i}"),
        relationship: relationship.to_owned(),
        destination: format!("n{}", i + 1),
        properties: text(2, i),
    };
    written(&mut db, (0..1_999).map(|i| edge(i, "next")).collect()).unwrap();
    let properties = scenario("properties", db, |db| {
        let patch = |i: usize| GraphOp::PatchNode {
            key: format!("n{i}"),
            patch: NodePatch {
                set: text(1, i + 9_000),
                ..NodePatch::default()
            },
            expected: Expected::Any,
        };
        written(db, (0..500).map(patch).collect())?;
        written(db, (0..500).map(|i| edge(i, "near")).collect())
    });
    // Every node attached to a record; after the image, detached and its
    // record deleted, then attached to a record put in the same frame.
    let mut db = graphed(5_000);
    written(
        &mut db,
        (0..5_000).map(|i| plain(format!("n{i}"))).collect(),
    )
    .unwrap();
    let attach = |i: u64, record: u64| GraphOp::Attach {
        node: format!("n{i}"),
        collection: "c".into(),
        record,
    };
    written(&mut db, (0..5_000).map(|i| attach(i, sparse(i))).collect()).unwrap();
    let vector = |mutation| GraphOp::Vector {
        collection: "c".into(),
        mutation,
    };
    let attachments = scenario("attachments", db, |db| {
        let moved = |i: u64| {
            [
                GraphOp::Detach {
                    node: format!("n{i}"),
                    collection: "c".into(),
                },
                vector(Mutation::Delete { id: sparse(i) }),
                vector(record(sparse(i + 5_000))),
                attach(i, sparse(i + 5_000)),
            ]
        };
        written(db, (0..500).flat_map(moved).collect())
    });
    [churned, properties, attachments]
}

#[test]
fn readers_hold_replay_graph_state_record_indexes_and_one_frame() {
    counting(|| {
        let key = key();
        for scenario in scenarios() {
            let (bodies, all_bodies) = decoded(&scenario.image);
            let cursor = FeedCursor {
                lineage: LINEAGE,
                sequence: scenario.floor,
            };
            for key in [None, Some(&key)] {
                let sealed = |log: &[u8]| match key {
                    Some(_) => encrypted(log),
                    None => log.to_vec(),
                };
                let (image, log) = (sealed(&scenario.image), sealed(&scenario.log));
                let name = format!(
                    "{} {}",
                    scenario.name,
                    if key.is_some() { "encrypted" } else { "plain" }
                );

                // Full replay, the reopen of the store, reported apart.
                let (mut loaded, reopen) = measured(|| load(&log, key).unwrap());
                let replay_graph = held(loaded.graph.take());
                drop(loaded);

                // The image alone. While its snapshot bodies are read, the
                // check holds the state built from the bodies before and,
                // beside it, only the frame read and its decoded body; once
                // the snapshot is loaded, the state and nothing more.
                let image_scan = scanned(&image, key);
                let image_graph = image_scan.graph;
                let steps = &image_scan.steps;
                let snapshots: Vec<&Step> = steps
                    .iter()
                    .filter(|step| step.operation == format::OP_GRAPH_SNAPSHOT)
                    .collect();
                assert_eq!(snapshots.len(), bodies.len(), "{name}");
                let start = snapshots[0].before;
                let loading = snapshots.iter().map(|step| step.top()).max().unwrap() - start;
                let one_body = snapshots
                    .iter()
                    .zip(&bodies)
                    .map(|(step, body)| 2 * step.length as isize + body.peak)
                    .max()
                    .unwrap();
                let loaded_at = steps.iter().position(|step| step.graph).unwrap();
                let kept = steps[loaded_at].before + steps[loaded_at].used.retained - start;
                assert!(
                    loading <= image_graph + one_body,
                    "{name}: loading peak {loading}, state {image_graph}, one body {one_body}"
                );
                assert!(
                    kept <= image_graph + 1024,
                    "{name}: kept {kept} for state {image_graph}"
                );

                // The whole log: replay's graph state, record indexes
                // within their bound, and bookkeeping that grows with
                // neither.
                let scan = scanned(&log, key);
                for (records, bytes) in &scan.indexes {
                    assert!(
                        *bytes <= index_bound(*records),
                        "{name}: index of {records} records holds {bytes}"
                    );
                }
                assert!(
                    scan.graph <= replay_graph,
                    "{name}: graph {} of replay's {replay_graph}",
                    scan.graph
                );
                assert!(scan.rest <= 4096, "{name}: rest {}", scan.rest);

                // The readers hold the check and, beside it, their output
                // and, in the feed, its graph projection.
                let projection = projected(&log, key);
                let (feed, feed_use) =
                    measured(|| read_changes(&mut Cursor::new(&log), key, None, &cursor, 16));
                let feed_out = held(feed.unwrap());
                let (export, export_use) = measured(|| {
                    logops::read_segment(&mut Cursor::new(&log), key, None, scenario.floor, 1 << 20)
                });
                let export = export.unwrap();
                let exported = export.0.frames.len();
                let export_out = held(export);
                let check_peak = scan.peak();
                assert!(
                    export_use.peak <= check_peak + 2 * export_out + 4096,
                    "{name}: export peak {}, check {check_peak}, out {export_out}",
                    export_use.peak
                );
                assert!(
                    feed_use.peak <= check_peak + projection + 2 * feed_out + 4096,
                    "{name}: feed peak {}, check {check_peak}, projection {projection}, \
                     out {feed_out}",
                    feed_use.peak
                );

                // Per operation: the most transient and the most transient
                // per byte of the frame.
                let mut most: BTreeMap<u8, (isize, f64)> = BTreeMap::new();
                for step in &scan.steps {
                    let entry = most.entry(step.operation).or_default();
                    let ratio = step.transient() as f64 / step.length as f64;
                    *entry = (entry.0.max(step.transient()), entry.1.max(ratio));
                }
                eprintln!(
                    "{name}: log {} bytes, {} frames, {} snapshot bodies; reopen peak {} \
                     kept {}; check peak {check_peak} blocks {}, graph {} (replay's \
                     {replay_graph}), indexes {:?}, rest {}; snapshot loading peak \
                     {loading} kept {kept}, state {image_graph}, one body {one_body}, \
                     all bodies {all_bodies}; feed peak {} out {feed_out} projection \
                     {projection}; export peak {} out {export_out} ({exported} frame \
                     bytes); per operation (most transient, most per frame byte) {most:.1?}",
                    log.len(),
                    scan.steps.len(),
                    snapshots.len(),
                    reopen.peak,
                    reopen.retained,
                    scan.blocks,
                    scan.graph,
                    scan.indexes,
                    scan.rest,
                    feed_use.peak,
                    export_use.peak,
                );
            }
        }
    });
}
