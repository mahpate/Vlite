//! Bounded controlled reader/writer interference and first-write COW measurements.
//! cargo run --release --example engine_mixed_perf -- LABEL
//! One reader starts a full exact scan before one writer attempts each mutation.
//! This deliberately contended schedule is not an arrival-rate/throughput model.
use std::{
    hint::black_box,
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
        mpsc::sync_channel,
    },
    time::Instant,
};
use vlite::{CreateOptions, Database, Mutation, SearchHit};

const RECORDS: usize = 8192;
const DIMENSIONS: usize = 128;
const ROUNDS: usize = 24;
const TRIALS: usize = 3;

fn vector(id: u64) -> Vec<f32> {
    (0..DIMENSIONS)
        .map(|d| ((id * 31 + d as u64 * 7) % 997) as f32 / 997.0)
        .collect()
}

fn populate(db: &mut Database) -> vlite::Result<()> {
    for start in (0..RECORDS).step_by(1024) {
        let operations: Vec<_> = (start..start + 1024)
            .map(|id| Mutation::Put {
                id: id as u64,
                vector: vector(id as u64),
                payload: vec![],
            })
            .collect();
        db.write_batch(&operations)?;
    }
    Ok(())
}

fn bits(hits: &[SearchHit]) -> Vec<(u64, u64)> {
    hits.iter().map(|h| (h.id, h.distance.to_bits())).collect()
}

fn update(db: &mut Database, round: usize) -> vlite::Result<()> {
    let id = (round % 64) as u64;
    if round.is_multiple_of(4) {
        db.delete(id)?;
    } else {
        db.put(id, &vector(100_000 + round as u64), b"updated")?;
    }
    Ok(())
}

fn mixed(label: &str, trial: usize, snapshots: bool) -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::Builder::new()
        .prefix("mixed-engine-")
        .tempdir_in("target\\perf2-engine-measure")?;
    let mut db = Database::create(directory.path().join("data.vlite"), DIMENSIONS)?;
    populate(&mut db)?;
    let mut reference = Database::create_in_memory(DIMENSIONS, CreateOptions::default())?;
    populate(&mut reference)?;
    let owner = Arc::new(RwLock::new(db));
    let (start_tx, start_rx) = sync_channel::<()>(0);
    let (done_tx, done_rx) = sync_channel::<()>(0);
    let completed = Arc::new(AtomicUsize::new(0));
    let mode = if snapshots { "snapshot" } else { "lock-held" };
    let (reads, writes) = std::thread::scope(|scope| {
        let writer_owner = Arc::clone(&owner);
        let writer_completed = Arc::clone(&completed);
        let writer = scope.spawn(move || {
            let mut samples = Vec::new();
            for round in 0..ROUNDS {
                start_rx.recv().unwrap();
                let start = Instant::now();
                let mut guard = writer_owner.write().unwrap();
                let wait = start.elapsed().as_nanos();
                let mutation = Instant::now();
                update(&mut guard, round).unwrap();
                let mutate = mutation.elapsed().as_nanos();
                drop(guard);
                samples.push((wait, mutate, start.elapsed().as_nanos()));
                writer_completed.store(round + 1, Ordering::Release);
                done_tx.send(()).unwrap();
            }
            samples
        });
        let mut samples = Vec::new();
        for round in 0..ROUNDS {
            let query = vector(50_000 + round as u64);
            // Independent mirrored state, outside the measured interval.
            let expected = bits(&reference.search(&query, 10).unwrap());
            let start = Instant::now();
            let guard = owner.read().unwrap();
            let lock_wait = start.elapsed().as_nanos();
            let capture = Instant::now();
            let snapshot = snapshots.then(|| guard.read_snapshot().unwrap());
            let capture_ns = capture.elapsed().as_nanos();
            // The writer is released only after a stable read generation exists.
            // Handshake/scheduling overhead is recorded separately from search.
            let (hits, scan_ns, handshake_ns) = if let Some(snapshot) = snapshot {
                drop(guard);
                let signal = Instant::now();
                start_tx.send(()).unwrap();
                let handshake = signal.elapsed().as_nanos();
                let scan = Instant::now();
                let hits = snapshot.search(&query, 10).unwrap();
                let elapsed = scan.elapsed().as_nanos();
                black_box(&snapshot);
                (hits, elapsed, handshake)
            } else {
                let signal = Instant::now();
                start_tx.send(()).unwrap();
                let handshake = signal.elapsed().as_nanos();
                let scan = Instant::now();
                let hits = guard.search(&query, 10).unwrap();
                let elapsed = scan.elapsed().as_nanos();
                drop(guard);
                (hits, elapsed, handshake)
            };
            let total_ns = start.elapsed().as_nanos();
            let writer_finished = completed.load(Ordering::Acquire) == round + 1;
            assert_eq!(bits(&hits), expected, "old-generation distance bits differ");
            samples.push((
                lock_wait,
                capture_ns,
                handshake_ns,
                scan_ns,
                total_ns,
                writer_finished,
            ));
            done_rx.recv().unwrap();
            update(&mut reference, round).unwrap();
            let current = owner.read().unwrap();
            assert_eq!(
                bits(&current.search(&query, 10).unwrap()),
                bits(&reference.search(&query, 10).unwrap())
            );
            assert_eq!(current.len(), reference.len());
        }
        (samples, writer.join().unwrap())
    });
    println!(
        "label={label} trial={trial} mode={mode} rounds={ROUNDS} read_columns=lock_wait,capture,handshake,scan,total,writer_finished_before_read_end read_ns={reads:?} write_columns=lock_wait,mutation_including_sync,total write_ns={writes:?} old_generation_bits_match=true committed_state_match=true"
    );
    Arc::try_unwrap(owner)
        .ok()
        .unwrap()
        .into_inner()
        .unwrap()
        .close()?;
    Ok(())
}

fn cow(label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut db = Database::create_in_memory(DIMENSIONS, CreateOptions::default())?;
    populate(&mut db)?;
    let replacement = vector(999_999);
    // Reserve/grow the in-memory log before timed COW pairs.
    db.put(0, &replacement, b"")?;
    let warm = db.read_snapshot()?;
    db.put(0, &replacement, b"")?;
    drop(warm);
    let mut unique = Vec::new();
    let mut shared = Vec::new();
    for round in 0..21 {
        for retain in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let snapshot = retain.then(|| db.read_snapshot().unwrap());
            let start = Instant::now();
            db.put(0, &replacement, b"")?;
            let elapsed = start.elapsed().as_nanos();
            if retain {
                shared.push(elapsed);
            } else {
                unique.push(elapsed);
            }
            if let Some(snapshot) = snapshot {
                assert_eq!(snapshot.len(), RECORDS);
                black_box(snapshot.get(0)?);
            }
            assert_eq!(db.get(0)?.unwrap().vector, replacement);
        }
    }
    println!(
        "label={label} cow_records={RECORDS} dimensions={DIMENSIONS} trials=21 unique_first_write_ns={unique:?} retained_first_write_ns={shared:?} memory_storage=true fsync_excluded=true"
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let label = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    std::fs::create_dir_all("target\\perf2-engine-measure")?;
    println!(
        "label={label} records={RECORDS} dimensions={DIMENSIONS} readers=1 writers=1 k=10 rayon_threads={} deterministic_seed_formula=true durable_mixed_writes=true",
        rayon::current_num_threads()
    );
    for trial in 0..TRIALS {
        for snapshots in if trial % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            mixed(&label, trial, snapshots)?;
        }
    }
    cow(&label)
}
