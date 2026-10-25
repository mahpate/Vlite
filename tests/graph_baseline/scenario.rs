//! Shared deterministic graph-off baseline scenario (feature-agent-graph
//! EPIC-001 ITEM-003/ITEM-004, REG-003..REG-007).
//!
//! Compiled by `tests\graph_regression.rs` (current tree) and, verbatim, by
//! the capture program `tests\graph_baseline\capture.rs` that
//! `scripts\graph_baseline.py fixtures` builds inside an isolated
//! `git archive` of the baseline commit. Uses only public APIs present at the
//! baseline commit `347b01d` and default features. Every operation's result
//! (success value or error) is logged with `{:?}`, so result *and* error
//! parity are compared. Nothing here may depend on wall-clock time, random
//! lineage values or file paths.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::Path;

use vlite::changes::FeedCursor;
use vlite::hybrid::HybridQuery;
use vlite::{
    AnnOptions, CollectionSettings, CollectionSpec, Condition, CreateOptions, Database,
    DistanceMetric, EncryptionKey, Expected, FormatVersion, GeoSearchQuery, GeoSort, HnswConfig,
    InvalidRowPolicy, Metadata, MetadataValue, Mutation, Quantization, ReplicaFollower,
};

pub const DIMS: usize = 4;
pub const KEY: [u8; 32] = [0x5a; 32];
/// Commit clock start for timestamped files (2023-11-14T22:13:20Z).
pub const CLOCK_START_MS: u64 = 1_700_000_000_000;
/// Fixed "now" for `get_at`/`compact_at`, far after every scenario timestamp.
pub const NOW_MS: u64 = 1_800_000_000_000;

#[derive(Clone, Copy, Debug)]
pub struct Variant {
    pub name: &'static str,
    pub format: FormatVersion,
    pub encrypted: bool,
    pub timestamps: bool,
}

pub const VARIANTS: [Variant; 5] = [
    Variant {
        name: "v3",
        format: FormatVersion::V3,
        encrypted: false,
        timestamps: false,
    },
    Variant {
        name: "v4",
        format: FormatVersion::V4,
        encrypted: false,
        timestamps: false,
    },
    Variant {
        name: "v5_plain",
        format: FormatVersion::V5,
        encrypted: false,
        timestamps: false,
    },
    Variant {
        name: "v5_encrypted",
        format: FormatVersion::V5,
        encrypted: true,
        timestamps: false,
    },
    Variant {
        name: "v5_timestamps",
        format: FormatVersion::V5,
        encrypted: false,
        timestamps: true,
    },
];

pub fn key() -> EncryptionKey {
    EncryptionKey::from_bytes(KEY)
}

impl Variant {
    pub fn key(&self) -> Option<EncryptionKey> {
        self.encrypted.then(key)
    }

    pub fn create(&self, path: &Path) -> Database {
        let options = CreateOptions {
            format: Some(self.format),
            encryption: self.key(),
            commit_timestamps: self.timestamps,
        };
        let mut db = Database::create_with_options(path, DIMS, options).unwrap();
        self.install_clock(&mut db, 0);
        db
    }

    pub fn open(&self, path: &Path) -> Database {
        let mut db = match self.key() {
            Some(key) => Database::open_encrypted(path, &key).unwrap(),
            None => Database::open(path).unwrap(),
        };
        self.install_clock(&mut db, 1);
        db
    }

    /// Deterministic commit timestamps; `epoch` separates sessions so that a
    /// reopened file keeps advancing past the populate session's clock.
    pub fn install_clock(&self, db: &mut Database, epoch: u64) {
        if self.timestamps {
            let mut now = CLOCK_START_MS + epoch * 1_000_000_000;
            db.set_commit_clock(Box::new(move || {
                now += 1_000;
                now
            }));
        }
    }
}

const WORDS: [&str; 8] = [
    "alpha", "bravo", "charlie", "delta", "echo", "rust", "vector", "graph",
];

pub fn vector(id: u64) -> Vec<f32> {
    vec![
        (id % 5) as f32,
        (id % 7) as f32 * 0.5,
        id as f32 * 0.125,
        ((id * 3) % 11) as f32 - 5.0,
    ]
}

pub fn metadata(id: u64) -> Metadata {
    let word = |offset: u64| WORDS[((id + offset) % WORDS.len() as u64) as usize];
    Metadata::from([
        (
            "title".into(),
            MetadataValue::String(format!("{} {}", word(0), word(3))),
        ),
        (
            "body".into(),
            MetadataValue::String(format!("{} {} {}", word(1), word(5), word(0))),
        ),
        ("group".into(), MetadataValue::Integer((id % 3) as i64)),
        ("score".into(), MetadataValue::Float(id as f64 * 0.25)),
        ("even".into(), MetadataValue::Boolean(id.is_multiple_of(2))),
        (
            "place".into(),
            MetadataValue::GeoPoint {
                latitude: 47.0 + id as f64 * 0.01,
                longitude: -122.0 - id as f64 * 0.01,
            },
        ),
    ])
}

fn text_settings() -> CollectionSettings {
    let mut settings = CollectionSettings::default();
    settings.text_fields = vec!["title".into(), "body".into()];
    settings
}

macro_rules! log {
    ($out:expr, $label:expr, $value:expr) => {
        writeln!($out, "{}: {:?}", $label, $value).unwrap()
    };
}

/// Applies the fixed mutation sequence (REG-006 surface) and returns the
/// per-operation result log.
pub fn populate(db: &mut Database) -> String {
    let mut out = String::new();
    log!(
        out,
        "set_collection_settings(default,text)",
        db.set_collection_settings("default", text_settings())
    );
    for id in 1..=20u64 {
        log!(
            out,
            format!("put_with_metadata({id})"),
            db.put_with_metadata(id, &vector(id), format!("p{id}").as_bytes(), &metadata(id))
        );
    }
    log!(out, "put(21)", db.put(21, &vector(21), b"plain"));
    for id in 101..=110u64 {
        log!(
            out,
            format!("put({id})"),
            db.put(id, &vector(id), format!("v{id}").as_bytes())
        );
    }
    log!(out, "delete(3)", db.delete(3));
    log!(out, "delete(999)", db.delete(999));
    log!(out, "revision(4)", db.revision(4));
    log!(
        out,
        "delete_if(4,Revision(1))",
        db.delete_if(4, Expected::Revision(1))
    );
    let revision = db.revision(5).unwrap();
    log!(
        out,
        "put_with_metadata_if(5,Absent)",
        db.put_with_metadata_if(5, &vector(50), b"x", &metadata(50), Expected::Absent)
    );
    log!(
        out,
        "put_with_metadata_if(5,Revision(current))",
        db.put_with_metadata_if(
            5,
            &vector(55),
            b"p5b",
            &metadata(55),
            Expected::Revision(revision.unwrap_or(0))
        )
    );
    log!(
        out,
        "write_batch",
        db.write_batch(&[
            Mutation::Put {
                id: 22,
                vector: vector(22),
                payload: b"batch".to_vec(),
            },
            Mutation::PutWithMetadata {
                id: 23,
                vector: vector(23),
                payload: b"batch-meta".to_vec(),
                metadata: metadata(23),
            },
            Mutation::Delete { id: 2 },
        ])
    );
    log!(
        out,
        "write_batch(invalid dims)",
        db.write_batch(&[Mutation::Put {
            id: 30,
            vector: vec![1.0],
            payload: Vec::new(),
        }])
    );
    log!(
        out,
        "update_metadata(6)",
        db.update_metadata(
            6,
            &Metadata::from([("group".into(), MetadataValue::Integer(9))])
        )
    );
    log!(
        out,
        "put_with_expiry(24,expired)",
        db.put_with_expiry(24, &vector(24), b"ttl", &metadata(24), Some(CLOCK_START_MS))
    );
    log!(
        out,
        "put_with_expiry(25,never)",
        db.put_with_expiry(25, &vector(25), b"ttl-max", &metadata(25), Some(u64::MAX))
    );
    log!(out, "expires_at(24)", db.expires_at(24));
    log!(
        out,
        "get_at(24,before)",
        db.get_at(24, CLOCK_START_MS - 1).map(|r| r.is_some())
    );
    log!(
        out,
        "get_at(24,now)",
        db.get_at(24, NOW_MS).map(|r| r.is_some())
    );
    log!(
        out,
        "create_collection(aux)",
        db.create_collection(
            "aux",
            CollectionSpec {
                dimensions: 2,
                settings: CollectionSettings::default(),
            }
        )
    );
    if let Ok(mut aux) = db.collection_mut("aux") {
        log!(out, "aux.put(1)", aux.put(1, &[0.5, 0.5], b"aux"));
    }
    log!(
        out,
        "drop_collection(aux,non-empty)",
        db.drop_collection("aux")
    );
    if let Ok(mut aux) = db.collection_mut("aux") {
        log!(out, "aux.delete(1)", aux.delete(1));
    }
    log!(out, "drop_collection(aux)", db.drop_collection("aux"));
    log!(out, "drop_collection(aux,again)", db.drop_collection("aux"));
    log!(
        out,
        "create_collection(kept)",
        db.create_collection(
            "kept",
            CollectionSpec {
                dimensions: 2,
                settings: CollectionSettings::default(),
            }
        )
    );
    if let Ok(mut kept) = db.collection_mut("kept") {
        log!(out, "kept.put(1)", kept.put(1, &[1.0, 0.0], b"k1"));
        log!(out, "kept.put(2)", kept.put(2, &[0.0, 1.0], b"k2"));
    }
    let job = db.next_import_job_id();
    log!(out, "next_import_job_id", job);
    if let Ok(job) = job {
        let rows = [
            Mutation::Put {
                id: 40,
                vector: vector(40),
                payload: b"import".to_vec(),
            },
            Mutation::Put {
                id: 41,
                vector: vec![0.0],
                payload: Vec::new(),
            },
        ];
        log!(
            out,
            "import_chunk(1,skip)",
            db.import_chunk(job, 1, &rows, InvalidRowPolicy::SkipInvalid)
        );
        log!(
            out,
            "import_chunk(1,replay)",
            db.import_chunk(job, 1, &rows, InvalidRowPolicy::SkipInvalid)
        );
        log!(out, "finish_import", db.finish_import(job));
        log!(out, "finish_import(again)", db.finish_import(job));
        let aborted = job + 1;
        log!(
            out,
            "import_chunk(1,reject)",
            db.import_chunk(aborted, 1, &rows, InvalidRowPolicy::RejectChunk)
        );
        log!(
            out,
            "import_chunk(1,ok)",
            db.import_chunk(aborted, 1, &rows[..1], InvalidRowPolicy::RejectChunk)
        );
        log!(
            out,
            "import_chunk(3,gap)",
            db.import_chunk(aborted, 3, &rows[..1], InvalidRowPolicy::RejectChunk)
        );
        log!(out, "abort_import", db.abort_import(aborted));
    } else {
        log!(
            out,
            "import_chunk(no job ids)",
            db.import_chunk(1, 0, &[], InvalidRowPolicy::RejectChunk)
        );
    }
    out
}

/// REG-004 maintenance round trip on a closed file at `path`: backup,
/// restore and compact (at `NOW_MS`; encrypted files compact under the key at
/// the wall clock, which is also after every scenario expiry), then reports
/// each output's header version and state. Outputs are written into `work`
/// as `backup.vlite`, `restored.vlite`, `compacted.vlite`.
pub fn maintenance_report(variant: &Variant, path: &Path, work: &Path) -> String {
    use vlite::maintenance;
    let mut out = String::new();
    let backup = work.join("backup.vlite");
    let restored = work.join("restored.vlite");
    let compacted = work.join("compacted.vlite");
    match variant.key() {
        Some(key) => {
            log!(
                out,
                "backup",
                maintenance::backup_with_key(path, &backup, &key).map(|_| ())
            );
            log!(
                out,
                "restore",
                maintenance::restore_with_key(&backup, &restored, &key).map(|_| ())
            );
            log!(
                out,
                "compact",
                maintenance::compact_with_key(path, &compacted, &key).map(|_| ())
            );
            log!(
                out,
                "verify",
                maintenance::verify_with_key(path, &key).map(|v| v.inspection.format_version)
            );
        }
        None => {
            log!(
                out,
                "backup",
                maintenance::backup(path, &backup).map(|_| ())
            );
            log!(
                out,
                "restore",
                maintenance::restore(&backup, &restored).map(|_| ())
            );
            log!(
                out,
                "compact_at(NOW_MS)",
                maintenance::compact_at(path, &compacted, NOW_MS).map(|_| ())
            );
            log!(
                out,
                "verify",
                maintenance::verify(path).map(|v| v.inspection.format_version)
            );
        }
    }
    for (label, output) in [
        ("backup", &backup),
        ("restored", &restored),
        ("compacted", &compacted),
    ] {
        let bytes = std::fs::read(output).unwrap();
        log!(out, format!("{label}.header_version"), &bytes[8..12]);
        let scratch = work.join(format!("scratch-{label}"));
        std::fs::create_dir_all(&scratch).unwrap();
        let mut db = variant.open(output);
        let key = variant.key();
        writeln!(out, "--- {label} state").unwrap();
        out.push_str(&report(&mut db, &scratch, false, key.as_ref()));
        db.close().unwrap();
    }
    out
}

/// Deterministic description of a database's observable vector-only state
/// (REG-005), plus, when `history` is set, the change feed and a replica
/// bootstrap (REG-007). `scratch` must be an empty directory; `key` is the
/// database key for encrypted files.
pub fn report(
    db: &mut Database,
    scratch: &Path,
    history: bool,
    key: Option<&EncryptionKey>,
) -> String {
    let mut out = String::new();
    log!(out, "format_version", db.format_version());
    log!(out, "dimensions", db.dimensions());
    log!(out, "len", db.len());
    log!(out, "has_lineage", db.lineage().is_some());
    if db.format_version() < 4 {
        // v1-v3 settings are memory-only; re-apply for lexical/hybrid parity.
        log!(
            out,
            "set_collection_settings(memory)",
            db.set_collection_settings("default", text_settings())
        );
    }
    log!(out, "collections", db.collections());
    log!(out, "browse", db.browse(None, 100));
    for id in [1u64, 2, 5, 6, 21, 23, 24, 25, 40, 41, 101, 110, 999] {
        log!(out, format!("get({id})"), db.get(id));
        log!(out, format!("get_at({id},now)"), db.get_at(id, NOW_MS));
        log!(out, format!("expires_at({id})"), db.expires_at(id));
        if history {
            log!(out, format!("revision({id})"), db.revision(id));
        }
    }
    let query = [1.0f32, 0.5, 1.0, -2.0];
    log!(out, "search", db.search(&query, 5));
    log!(
        out,
        "search_with_metric(cosine)",
        db.search_with_metric(&query, 5, DistanceMetric::Cosine)
    );
    log!(
        out,
        "search_with_metric(inner_product)",
        db.search_with_metric(&query, 5, DistanceMetric::InnerProduct)
    );
    let filter = [Condition::Eq {
        field: "group".into(),
        value: MetadataValue::Integer(1),
    }];
    log!(
        out,
        "search_filtered(group=1)",
        db.search_filtered(&query, 5, &filter)
    );
    let range = [Condition::Range {
        field: "score".into(),
        min: Some(MetadataValue::Float(1.0)),
        max: Some(MetadataValue::Float(4.0)),
    }];
    log!(
        out,
        "search_filtered(score 1..4)",
        db.search_filtered(&query, 5, &range)
    );
    let config = HnswConfig {
        m: 8,
        ef_construction: 32,
        seed: 7,
    };
    log!(
        out,
        "search_approximate(hnsw seed 7)",
        db.search_approximate(&query, 5, 32, config)
    );
    let quantized = AnnOptions {
        quantization: Quantization::ScalarU8,
        rerank_multiplier: 4,
    };
    log!(
        out,
        "build_hnsw_with_options(scalar_u8)",
        db.build_hnsw_with_options(config, DistanceMetric::SquaredL2, quantized)
    );
    log!(
        out,
        "search_approximate_ready_with_options(scalar_u8)",
        db.search_approximate_ready_with_options(
            &query,
            5,
            32,
            config,
            quantized,
            &[],
            DistanceMetric::SquaredL2
        )
        .map(|(hits, _)| hits)
    );
    log!(
        out,
        "search_hybrid(lexical rust)",
        db.search_hybrid(&HybridQuery::new(None, "rust", 5))
            .map(|(hits, _)| hits)
    );
    log!(
        out,
        "search_hybrid(vector+graph)",
        db.search_hybrid(&HybridQuery::new(Some(query.to_vec()), "graph vector", 5))
            .map(|(hits, _)| hits)
    );
    log!(
        out,
        "search_grouped(group)",
        db.search_grouped(&query, 3, 2, "group", &[], DistanceMetric::SquaredL2)
    );
    log!(out, "facets(group)", db.facets("group", &[]));
    log!(out, "numeric_stats(score)", db.numeric_stats("score", &[]));
    log!(
        out,
        "search_geo",
        db.search_geo(&GeoSearchQuery {
            vector: query.to_vec(),
            k: 5,
            field: "place".into(),
            latitude: 47.05,
            longitude: -122.05,
            radius_meters: 20_000.0,
            conditions: Vec::new(),
            metric: DistanceMetric::SquaredL2,
            sort: GeoSort::GeoDistance,
        })
    );
    match db.collection("kept") {
        Ok(kept) => log!(out, "kept.search", kept.search(&[1.0, 0.1], 2)),
        Err(error) => log!(out, "collection(kept)", Err::<(), _>(error)),
    }
    for text in [
        r#"SEARCH text NEAR $q K 10 WHERE lang = "en" AND price < 20 AND doc_id IN (0,1) AND EXISTS place AND GEO(place,0,0,100) GROUP BY doc_id FACET lang ORDER BY price DESC LIMIT 2"#,
        "SEARCH NEAR $q K 3",
        "SEARCH text NEAR $q K 3 WHERE",
        "DROP everything; SEARCH",
    ] {
        log!(
            out,
            format!("query_language::parse({text})"),
            vlite::query_language::parse(text)
        );
    }
    if history {
        let cursor = FeedCursor {
            lineage: 0,
            sequence: 0,
        };
        match db.changes_after(&cursor, 10_000) {
            Ok(batch) => {
                log!(out, "changes.head", batch.head);
                log!(out, "changes.has_more", batch.has_more);
                log!(out, "changes.next.sequence", batch.next.sequence);
                for change in &batch.changes {
                    log!(out, "change", change);
                }
            }
            Err(error) => log!(out, "changes_after", Err::<(), _>(error)),
        }
        match db.export_log(0, 1 << 24) {
            Ok(segment) => {
                log!(
                    out,
                    "export_log(base,last,head,header_len,frames_len)",
                    (
                        segment.base_sequence,
                        segment.last_sequence,
                        segment.head_sequence,
                        segment.header.len(),
                        segment.frames.len()
                    )
                );
                let follower_path = scratch.join("follower.vlite");
                match ReplicaFollower::create(&follower_path, &segment, key) {
                    Ok(follower) => {
                        let status = follower.status();
                        log!(
                            out,
                            "follower(applied,lag,len)",
                            (status.applied_sequence, status.lag, follower.db().len())
                        );
                        log!(out, "follower.search", follower.db().search(&query, 5));
                    }
                    Err(error) => log!(out, "ReplicaFollower::create", Err::<(), _>(error)),
                }
            }
            Err(error) => log!(out, "export_log", Err::<(), _>(error)),
        }
    }
    out
}
