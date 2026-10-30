//! Local direction check, not a DO-005 campaign.
//! Run the same source with `cargo run --locked --release --example exact_search_bench`.
//! Compare result fingerprints as well as latency between revisions; set
//! RAYON_NUM_THREADS identically (also try 1 to isolate the serial scan).
use std::{error::Error, hint::black_box, time::Instant};
use vlite::{Database, Mutation};

const RECORDS: usize = 100_000;
const DIMS: usize = 384;
const K: usize = 10;
const QUERIES: usize = 100;

fn component(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*state >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
}

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::args().len() != 1 {
        return Err("this bounded benchmark takes no arguments".into());
    }
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("exact.vlite");
    let mut db = Database::create(&path, DIMS)?;
    let mut state = 42;
    let vectors: Vec<Vec<f32>> = (0..RECORDS)
        .map(|_| (0..DIMS).map(|_| component(&mut state)).collect())
        .collect();
    for (batch, chunk) in vectors.chunks(1000).enumerate() {
        let operations: Vec<_> = chunk
            .iter()
            .enumerate()
            .map(|(offset, vector)| Mutation::Put {
                id: (batch * 1000 + offset) as u64,
                vector: vector.clone(),
                payload: Vec::new(),
            })
            .collect();
        db.write_batch(&operations)?;
    }
    db.close()?;
    let db = Database::open(&path)?;
    let queries: Vec<Vec<f32>> = (0..QUERIES)
        .map(|_| (0..DIMS).map(|_| component(&mut state)).collect())
        .collect();
    // Independent sequential f64 sums and full sorting, outside timed searches.
    for query in &queries[..3] {
        let mut expected: Vec<_> = vectors
            .iter()
            .enumerate()
            .map(|(id, vector)| {
                let distance: f64 = vector
                    .iter()
                    .zip(query)
                    .map(|(&a, &b)| {
                        let difference = f64::from(a) - f64::from(b);
                        difference * difference
                    })
                    .sum();
                (id as u64, distance)
            })
            .collect();
        expected.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        let actual = db.search(query, K)?;
        if actual.len() != K
            || actual.iter().zip(&expected).any(|(hit, &(id, distance))| {
                hit.id != id || hit.distance.to_bits() != distance.to_bits()
            })
        {
            return Err("exact IDs or distance bits differ from full-sort oracle".into());
        }
    }
    for query in &queries[..10] {
        black_box(db.search(black_box(query), K)?);
    }
    let mut samples = Vec::with_capacity(QUERIES);
    let mut fingerprint = 0xcbf2_9ce4_8422_2325u64;
    for query in &queries {
        let start = Instant::now();
        let hits = black_box(db.search(black_box(query), K)?);
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        for hit in hits {
            for byte in hit
                .id
                .to_le_bytes()
                .into_iter()
                .chain(hit.distance.to_bits().to_le_bytes())
            {
                fingerprint = (fingerprint ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "local_indicative_only records={RECORDS} dims={DIMS} k={K} queries={QUERIES} threads={} p50_ms={:.6} p95_ms={:.6} result_bits={fingerprint:016x}",
        rayon::current_num_threads(),
        samples[QUERIES / 2],
        samples[QUERIES * 95 / 100],
    );
    db.close()?;
    Ok(())
}
