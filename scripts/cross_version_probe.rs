//! HP-3 cross-version probe. `scripts/cross_version_check.py` copies it into
//! a `git archive` extraction of each compared revision as
//! `src/bin/cross_version_probe.rs`, so it uses only the vlite v0.6.1 public
//! API and one source builds against every revision.
//!
//! Output is tab-separated lines: `STEP op args result` (matrix A),
//! `DUMP name section value` and `APPEND name op result` (matrix B) and
//! `RESULT name entry ok|err detail` (matrix C). Paths, lineage values and
//! wall-clock values are never printed, so equal behaviour gives equal text.
#![allow(unreachable_patterns)]

use std::path::{Path, PathBuf};

use vlite::changes::{ChangeKind, FeedCursor};
use vlite::hybrid::HybridQuery;
use vlite::maintenance::{self, Inspection};
use vlite::{
    CollectionSettings, CollectionSpec, Condition, CreateOptions, Database, DistanceMetric, EncryptionKey, Error,
    Expected, FormatVersion, Metadata, MetadataValue, Mutation, Record, SearchHit,
};

const DIMS: usize = 4;
const T0: u64 = 1_700_000_000_000;
const KEY: [u8; 32] = [0x42; 32];
const WRONG_KEY: [u8; 32] = [0x17; 32];
const METRICS: [DistanceMetric; 3] = [DistanceMetric::SquaredL2, DistanceMetric::Cosine, DistanceMetric::InnerProduct];
const BASE: [[f32; 4]; 8] = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.5, 0.5, 0.0, 0.0],
    [2.0, 0.0, 0.0, 0.0],
    [-1.0, 0.0, 0.0, 0.0],
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 0.0, 0.0, 0.0],
];
const COLORS: [&str; 4] = ["red", "blue", "red", "green"];
const TEXTS: [&str; 5] = ["alpha beta", "beta gamma gamma", "alpha delta", "gamma epsilon alpha", "zeta"];
const Q1: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const Q2: [f32; 4] = [1.0, 1.0, 0.0, 0.0];

type Res<T> = std::result::Result<T, Error>;

fn esc(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn key() -> EncryptionKey {
    EncryptionKey::from_bytes(KEY)
}

fn err(error: &Error) -> String {
    match error {
        Error::EncryptionKeyMismatch { .. } => "EncryptionKeyMismatch".into(),
        other => {
            let text = format!("{other:?}");
            // CursorCompacted carries both (random) lineage values.
            if text.starts_with("CursorCompacted") { "CursorCompacted".into() } else { text }
        }
    }
}

fn res<T>(result: Res<T>, render: impl FnOnce(T) -> String) -> String {
    match result {
        Ok(value) => format!("ok:{}", render(value)),
        Err(error) => format!("err:{}", err(&error)),
    }
}

fn done<T>(result: Res<T>) -> String {
    res(result, |_| String::new())
}

fn flag(result: Res<bool>) -> String {
    res(result, |value| value.to_string())
}

fn num(result: Res<u64>) -> String {
    res(result, |value| value.to_string())
}

fn optnum(result: Res<Option<u64>>) -> String {
    res(result, |value| format!("{value:?}"))
}

fn same(result: Res<String>) -> String {
    res(result, |value| value)
}

fn hits_txt(result: Res<Vec<SearchHit>>) -> String {
    res(result, |found| hits(&found))
}

fn insp(result: Res<Inspection>) -> String {
    res(result, |value| inspection(&value))
}

fn compaction(result: Res<maintenance::CompactionReport>) -> String {
    res(result, |r| format!("src[{}] out[{}]", inspection(&r.source), inspection(&r.output)))
}

fn page_txt(result: Res<vlite::BrowsePage>) -> String {
    res(result, |page| page_line(&page))
}

fn mv(value: &MetadataValue) -> String {
    match value {
        MetadataValue::String(text) => format!("s:{}", esc(text)),
        MetadataValue::Integer(number) => format!("i:{number}"),
        MetadataValue::Float(number) => format!("f:{number:?}"),
        MetadataValue::Boolean(flag) => format!("b:{flag}"),
        MetadataValue::GeoPoint { latitude, longitude, .. } => format!("g:{latitude:?},{longitude:?}"),
        other => format!("?:{other:?}"),
    }
}

fn md(metadata: &Metadata) -> String {
    let parts: Vec<String> = metadata.iter().map(|(field, value)| format!("{}={}", esc(field), mv(value))).collect();
    format!("{{{}}}", parts.join(","))
}

fn rec(record: &Record) -> String {
    format!("v={:?} p={} m={}", record.vector, hex(&record.payload), md(&record.metadata))
}

fn opt_rec(result: Res<Option<&Record>>) -> String {
    res(result, |record| match record {
        Some(record) => rec(record),
        None => "none".into(),
    })
}

fn hits(found: &[SearchHit]) -> String {
    let parts: Vec<String> = found.iter().map(|hit| format!("{}@{:?}", hit.id, hit.distance)).collect();
    parts.join(",")
}

fn settings(value: &CollectionSettings) -> String {
    format!(
        "metric={:?} text={:?} rerank={} indexes={}",
        value.metric,
        value.text_fields,
        value.rerank_multiplier,
        value.metadata_indexes.len()
    )
}

fn inspection(value: &Inspection) -> String {
    format!(
        "v={} dims={} live={} commits={} file={} valid={} tail={}",
        value.format_version,
        value.dimensions,
        value.live_records,
        value.commits,
        value.file_bytes,
        value.valid_bytes,
        value.incomplete_tail_bytes
    )
}

fn change_kind(kind: &ChangeKind) -> String {
    match kind {
        ChangeKind::Put { id, record, expires_at_ms, .. } => format!("put {id} {} exp={expires_at_ms:?}", rec(record)),
        ChangeKind::Delete { id, .. } => format!("del {id}"),
        ChangeKind::MetadataUpdate { id, metadata, .. } => format!("meta {id} {}", md(metadata)),
        ChangeKind::CollectionCreated { dimensions, settings: value, .. } => {
            format!("coll+ {dimensions} {}", settings(value))
        }
        ChangeKind::CollectionDropped => "coll-".into(),
        ChangeKind::SettingsUpdated { settings: value, .. } => {
            format!("settings {}", settings(value))
        }
        ChangeKind::VectorsUpdated => "vectors".into(),
        other => format!("other {other:?}"),
    }
}

fn feed(db: &Database, cursor: &FeedCursor, max: usize) -> String {
    res(db.changes_after(cursor, max), |batch| {
        let items: Vec<String> = batch
            .changes
            .iter()
            .map(|change| {
                format!(
                    "{}.{}[{}]@{:?} {}",
                    change.sequence,
                    change.ordinal,
                    esc(&change.collection),
                    change.committed_at_ms,
                    change_kind(&change.kind)
                )
            })
            .collect();
        format!(
            "n={} next={} same_lineage={} more={} head={} [{}]",
            items.len(),
            batch.next.sequence,
            db.lineage() == Some(batch.next.lineage),
            batch.has_more,
            batch.head,
            items.join(";")
        )
    })
}

/// File bytes with the lineage (16..24) and header CRC (28..32) zeroed for
/// v4/v5 headers, which are the only random header fields.
fn masked(bytes: &[u8]) -> String {
    let mut copy = bytes.to_vec();
    if copy.len() >= 32 && copy[..8] == *b"VLITEDB\0" {
        let version = u32::from_le_bytes([copy[8], copy[9], copy[10], copy[11]]);
        if version >= 4 {
            copy[16..24].fill(0);
            copy[28..32].fill(0);
        }
    }
    format!("len={} hex={}", copy.len(), hex(&copy))
}

fn file_image(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => masked(&bytes),
        Err(error) => format!("io:{:?}", error.kind()),
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn vector(id: u64) -> Vec<f32> {
    match id {
        1..=8 => BASE[(id - 1) as usize].to_vec(),
        _ => vec![0.1 * (id % 7) as f32, 0.0, 0.25, 1.0],
    }
}

fn payload(id: u64) -> Vec<u8> {
    format!("p{id}").into_bytes()
}

fn meta(id: u64) -> Metadata {
    let slot = (id % 4) as usize;
    let mut metadata = Metadata::new();
    metadata.insert("color".into(), MetadataValue::String(COLORS[slot].into()));
    metadata.insert("n".into(), MetadataValue::Integer(id as i64 * 10));
    metadata.insert("w".into(), MetadataValue::Float(id as f64 / 4.0));
    metadata.insert("flag".into(), MetadataValue::Boolean(id % 2 == 0));
    metadata.insert("text".into(), MetadataValue::String(TEXTS[(id % 5) as usize].into()));
    metadata.insert("geo".into(), MetadataValue::GeoPoint { latitude: 47.6 + slot as f64 * 0.01, longitude: -122.3 });
    metadata
}

fn eq(field: &str, value: MetadataValue) -> Condition {
    Condition::Eq { field: field.into(), value }
}

fn range(field: &str, min: Option<MetadataValue>, max: Option<MetadataValue>) -> Condition {
    Condition::Range { field: field.into(), min, max }
}

fn step(op: &str, args: &str, result: String) {
    println!("STEP\t{}\t{}\t{}", esc(op), esc(args), esc(&result));
}

fn db_line(db: &Database) -> String {
    format!("v={} dims={} lineage={} len={}", db.format_version(), db.dimensions(), db.lineage().is_some(), db.len())
}

fn page_line(page: &vlite::BrowsePage) -> String {
    let ids: Vec<String> = page.hits.iter().map(|hit| format!("{}:{}", hit.id, rec(&hit.record))).collect();
    format!("next={:?} [{}]", page.next_after, ids.join(";"))
}

fn core_steps(db: &mut Database) {
    for id in 1..=8u64 {
        step("put", &format!("id={id}"), done(db.put(id, &vector(id), &payload(id))));
    }
    for id in 9..=12u64 {
        let result = db.put_with_metadata(id, &vector(id), &payload(id), &meta(id));
        step("put_with_metadata", &format!("id={id}"), done(result));
    }
    step("put.dim_mismatch", "id=13 dims=2", done(db.put(13, &[1.0, 2.0], b"x")));
    step("put.nan", "id=13", done(db.put(13, &[f32::NAN, 0.0, 0.0, 0.0], b"x")));
    step("put.inf", "id=13", done(db.put(13, &[f32::INFINITY, 0.0, 0.0, 0.0], b"x")));
    for id in [1u64, 8, 9, 99] {
        step("get", &format!("id={id}"), opt_rec(db.get(id)));
        step("revision", &format!("id={id}"), optnum(db.revision(id)));
    }
    step("upsert", "id=2", done(db.put(2, &[0.0, 2.0, 0.0, 0.0], b"p2b")));
    step("get", "id=2", opt_rec(db.get(2)));
    step("revision", "id=2", optnum(db.revision(2)));
    step("delete", "id=3", flag(db.delete(3)));
    step("delete.again", "id=3", flag(db.delete(3)));
    step("get", "id=3", opt_rec(db.get(3)));
    step("len", "", db.len().to_string());
    let current = db.revision(4).ok().flatten().unwrap_or(0);
    let cas = [
        ("absent.existing", 4u64, Expected::Absent),
        ("revision.stale", 4, Expected::Revision(current + 100)),
        ("revision.current", 4, Expected::Revision(current)),
        ("any", 4, Expected::Any),
        ("absent.new", 14, Expected::Absent),
        ("revision.missing", 15, Expected::Revision(1)),
    ];
    for (label, id, expected) in cas {
        let result = db.put_with_metadata_if(id, &vector(id), &payload(id), &meta(id), expected);
        step(&format!("cas.put.{label}"), &format!("id={id}"), num(result));
    }
    let now = db.revision(14).ok().flatten().unwrap_or(0);
    step("cas.delete.stale", "id=14", flag(db.delete_if(14, Expected::Revision(now + 1))));
    step("cas.delete.absent", "id=14", flag(db.delete_if(14, Expected::Absent)));
    step("cas.delete.current", "id=14", flag(db.delete_if(14, Expected::Revision(now))));
    let mut update = meta(9);
    update.insert("color".into(), MetadataValue::String("violet".into()));
    update.remove("geo");
    step("update_metadata", "id=9", num(db.update_metadata(9, &update)));
    step("update_metadata.missing", "id=99", num(db.update_metadata(99, &update)));
    step("get", "id=9", opt_rec(db.get(9)));
}

fn search_steps(db: &Database) {
    for (qname, query) in [("q1", Q1), ("q2", Q2)] {
        step("search", &format!("{qname} k=3"), hits_txt(db.search(&query, 3)));
        for metric in METRICS {
            for k in [0usize, 3, 50] {
                let result = db.search_with_metric(&query, k, metric);
                step("search_with_metric", &format!("{qname} {metric:?} k={k}"), hits_txt(result));
            }
        }
    }
    let zero = [0.0f32; 4];
    for metric in METRICS {
        let result = db.search_with_metric(&zero, 3, metric);
        step("search.zero_query", &format!("{metric:?}"), hits_txt(result));
    }
    step("search.nan_query", "", hits_txt(db.search(&[f32::NAN, 0.0, 0.0, 0.0], 3)));
    step("search.dim_mismatch", "", hits_txt(db.search(&[1.0, 0.0], 3)));
    let filters: Vec<(&str, Vec<Condition>)> = vec![
        ("eq.color", vec![eq("color", MetadataValue::String("red".into()))]),
        ("eq.flag", vec![eq("flag", MetadataValue::Boolean(true))]),
        ("range.n", vec![range("n", Some(MetadataValue::Integer(100)), Some(MetadataValue::Integer(110)))]),
        ("range.w.open", vec![range("w", Some(MetadataValue::Float(2.6)), None)]),
        (
            "combined",
            vec![eq("color", MetadataValue::String("red".into())), range("n", None, Some(MetadataValue::Integer(105)))],
        ),
        ("missing.field", vec![eq("nope", MetadataValue::Integer(1))]),
        ("type.mismatch", vec![eq("n", MetadataValue::String("90".into()))]),
        (
            "geo.radius",
            vec![Condition::GeoRadius {
                field: "geo".into(),
                latitude: 47.6,
                longitude: -122.3,
                radius_meters: 1500.0,
            }],
        ),
        ("range.empty", vec![range("n", None, None)]),
    ];
    for (label, conditions) in &filters {
        step("search_filtered", label, hits_txt(db.search_filtered(&Q2, 50, conditions)));
        let result = db.search_filtered_with_metric(&Q2, 50, conditions, DistanceMetric::Cosine);
        step("search_filtered.cosine", label, hits_txt(result));
    }
    for (field, conditions) in
        [("color", vec![]), ("flag", vec![range("n", Some(MetadataValue::Integer(100)), None)]), ("n", vec![])]
    {
        let result = db.facets(field, &conditions);
        let rendered = res(result, |counts| {
            let parts: Vec<String> = counts.iter().map(|c| format!("{}={}", mv(&c.value), c.count)).collect();
            parts.join(",")
        });
        step("facets", field, rendered);
    }
    for (field, conditions) in [
        ("n", vec![]),
        ("w", vec![eq("color", MetadataValue::String("red".into()))]),
        ("missing", vec![]),
        ("color", vec![]),
    ] {
        let result = db.numeric_stats(field, &conditions);
        let rendered = res(result, |s| {
            format!("count={} min={:?} max={:?} sum={:?} avg={:?}", s.count, s.min, s.max, s.sum, s.average)
        });
        step("numeric_stats", field, rendered);
    }
    let mut after = None;
    for page in 0..6 {
        match db.browse_at(after, 4, T0) {
            Ok(result) => {
                step("browse_at", &format!("page={page}"), format!("ok:{}", page_line(&result)));
                after = result.next_after;
                if after.is_none() {
                    break;
                }
            }
            Err(error) => {
                step("browse_at", &format!("page={page}"), format!("err:{}", err(&error)));
                break;
            }
        }
    }
    step("browse.limit0", "", page_txt(db.browse(None, 0)));
    step("browse.limit1001", "", page_txt(db.browse(None, 1001)));
}

fn batch_ttl_steps(db: &mut Database) {
    let good = [
        Mutation::Put { id: 20, vector: vector(20), payload: payload(20) },
        Mutation::PutWithMetadata { id: 21, vector: vector(21), payload: payload(21), metadata: meta(21) },
        Mutation::Delete { id: 1 },
        Mutation::Delete { id: 77 },
    ];
    step("write_batch", "20,21,-1,-77", done(db.write_batch(&good)));
    let bad = [
        Mutation::Put { id: 22, vector: vector(22), payload: payload(22) },
        Mutation::Put { id: 23, vector: vec![1.0, 2.0], payload: payload(23) },
    ];
    step("write_batch.invalid", "22,23(dims=2)", done(db.write_batch(&bad)));
    step("write_batch.empty", "", done(db.write_batch(&[])));
    for id in [1u64, 20, 21, 22] {
        step("get", &format!("id={id}"), opt_rec(db.get(id)));
    }
    let ttl = db.put_with_expiry(30, &vector(30), &payload(30), &meta(30), Some(T0 + 1000));
    step("put_with_expiry", "id=30 exp=T0+1000", num(ttl));
    let keep = db.put_with_expiry(31, &vector(31), &payload(31), &Metadata::new(), None);
    step("put_with_expiry", "id=31 exp=none", num(keep));
    for id in [30u64, 31, 99] {
        step("expires_at", &format!("id={id}"), optnum(db.expires_at(id)));
    }
    for (label, now) in [("T0", T0), ("T0+999", T0 + 999), ("T0+1000", T0 + 1000)] {
        step("get_at", &format!("id=30 now={label}"), opt_rec(db.get_at(30, now)));
        let result = db.search_filtered_with_metric_at(&vector(30), 3, &[], DistanceMetric::SquaredL2, now);
        step("search_at", &format!("now={label}"), hits_txt(result));
        step("browse_at", &format!("after=29 now={label}"), page_txt(db.browse_at(Some(29), 10, now)));
    }
    step("get.wallclock", "id=30", opt_rec(db.get(30)));
    step("len", "", db.len().to_string());
}

fn hybrid_line(result: Res<(Vec<vlite::hybrid::HybridHit>, vlite::SearchStats)>) -> String {
    res(result, |(found, _stats)| {
        let parts: Vec<String> = found
            .iter()
            .map(|hit| {
                format!(
                    "{}:{:?}/{:?}/{:?}/{:?}/{:?}",
                    hit.id, hit.score, hit.distance, hit.bm25, hit.vector_rank, hit.text_rank
                )
            })
            .collect();
        parts.join(",")
    })
}

fn docs_spec() -> CollectionSpec {
    let mut settings = CollectionSettings::default();
    settings.text_fields = vec!["text".into()];
    CollectionSpec { dimensions: DIMS, settings }
}

fn collection_lines(db: &Database) -> String {
    res(db.collections(), |list| {
        let parts: Vec<String> = list
            .iter()
            .map(|info| {
                format!(
                    "{}#{} dims={} live={} persisted={} {}",
                    esc(&info.name),
                    info.id,
                    info.dimensions,
                    info.live_records,
                    info.settings_persisted,
                    settings(&info.settings)
                )
            })
            .collect();
        parts.join(";")
    })
}

fn collection_steps(db: &mut Database) {
    let created = db.create_collection("docs", docs_spec());
    step("create_collection", "docs", res(created, |info| format!("{}#{}", info.name, info.id)));
    step("create_collection.exists", "docs", res(db.create_collection("docs", docs_spec()), |_| String::new()));
    step("create_collection.default", "", res(db.create_collection("default", docs_spec()), |_| String::new()));
    step("create_collection.bad_name", "a b", res(db.create_collection("a b", docs_spec()), |_| String::new()));
    match db.collection_mut("docs") {
        Ok(mut docs) => {
            for id in 1..=5u64 {
                let mut metadata = meta(id);
                metadata.insert("text".into(), MetadataValue::String(TEXTS[(id - 1) as usize].into()));
                let result = docs.put_with_metadata(id, &vector(id), &payload(id), &metadata);
                step("docs.put_with_metadata", &format!("id={id}"), num(result));
            }
            step("docs.delete", "id=5", flag(docs.delete(5)));
        }
        Err(error) => step("collection_mut", "docs", format!("err:{}", err(&error))),
    }
    step("collection_mut.missing", "nope", done(db.collection_mut("nope")));
    step("collections", "", collection_lines(db));
    let docs = match db.collection("docs") {
        Ok(docs) => docs,
        Err(error) => return step("collection", "docs", format!("err:{}", err(&error))),
    };
    step("docs.get", "id=2", opt_rec(docs.get(2)));
    step("docs.get", "id=5", opt_rec(docs.get(5)));
    let status = docs.text_index_status();
    step(
        "docs.text_index_status",
        "",
        format!("fields={} available={} documents={}", status.declared_fields, status.available, status.documents),
    );
    let mut queries: Vec<(&str, HybridQuery)> = vec![
        ("vector+text", HybridQuery::new(Some(Q1.to_vec()), "alpha", 5)),
        ("text.only", HybridQuery::new(None, "beta gamma", 5)),
        ("vector.only", HybridQuery::new(Some(Q2.to_vec()), "", 5)),
        ("unknown.token", HybridQuery::new(None, "omega", 5)),
        ("nothing", HybridQuery::new(None, "", 5)),
        ("k0", HybridQuery::new(Some(Q1.to_vec()), "alpha", 0)),
    ];
    let mut filtered = HybridQuery::new(Some(Q1.to_vec()), "alpha gamma", 5);
    filtered.conditions = vec![eq("color", MetadataValue::String("red".into()))];
    queries.push(("filtered", filtered));
    let mut cosine = HybridQuery::new(Some(Q2.to_vec()), "gamma", 5);
    cosine.metric = DistanceMetric::Cosine;
    queries.push(("cosine", cosine));
    let mut weighted = HybridQuery::new(Some(Q1.to_vec()), "gamma", 5);
    weighted.fusion.rrf_k = 10;
    weighted.fusion.vector_weight = 0.5;
    weighted.fusion.text_weight = 2.0;
    weighted.fusion.candidates = Some(2);
    queries.push(("weighted", weighted));
    let mut invalid = HybridQuery::new(Some(Q1.to_vec()), "alpha", 5);
    invalid.fusion.text_weight = f64::NAN;
    queries.push(("invalid.weight", invalid));
    for (label, query) in &queries {
        step("docs.search_hybrid", label, hybrid_line(docs.search_hybrid(query)));
    }
    step("default.search_hybrid", "text.only", hybrid_line(db.search_hybrid(&HybridQuery::new(None, "alpha", 5))));
    let result = docs.search_with_metric(&Q1, 10, DistanceMetric::InnerProduct);
    step("docs.search_with_metric", "q1 InnerProduct", hits_txt(result));
    step("docs.browse", "", page_txt(docs.browse(None, 10)));
}

fn feed_steps(db: &Database) {
    let start = FeedCursor::beginning();
    step("changes_after", "beginning max=1000", feed(db, &start, 1000));
    match db.changes_after(&start, 5) {
        Ok(batch) => {
            step("changes_after", "beginning max=5", feed(db, &start, 5));
            step("changes_after", "second page max=5", feed(db, &batch.next, 5));
            let token = batch.next.to_token();
            let round = FeedCursor::from_token(&token).map(|cursor| cursor == batch.next);
            step("feed.token_roundtrip", "", res(round, |same| same.to_string()));
        }
        Err(error) => step("changes_after", "beginning max=5", format!("err:{}", err(&error))),
    }
    match db.feed_cursor() {
        Ok(head) => {
            step(
                "feed_cursor",
                "",
                format!("ok:seq={} same_lineage={}", head.sequence, db.lineage() == Some(head.lineage)),
            );
            step("changes_after", "head", feed(db, &head, 10));
            let wrong = FeedCursor { lineage: head.lineage ^ 1, sequence: 1 };
            step("changes_after", "wrong lineage", feed(db, &wrong, 10));
            let ahead = FeedCursor { lineage: head.lineage, sequence: head.sequence + 50 };
            step("changes_after", "ahead of head", feed(db, &ahead, 10));
        }
        Err(error) => step("feed_cursor", "", format!("err:{}", err(&error))),
    }
    let bad = FeedCursor::from_token("vlc1.zz.00000000").map(|_| ());
    step("feed.bad_token", "", done(bad));
}

fn lifecycle_steps(dir: &Path, a: &Path, db: Database) {
    let lineage = db.lineage();
    step("close", "a", done(db.close()));
    let reopened = Database::open(a);
    match &reopened {
        Ok(db) => step("open", "a", format!("ok:{}", db_line(db))),
        Err(error) => step("open", "a", format!("err:{}", err(error))),
    }
    let Ok(mut db) = reopened else { return };
    step("open.same_lineage", "a", (db.lineage() == lineage).to_string());
    step("open.second_writer", "a", done(Database::open(a)));
    step("get", "id=2 after reopen", opt_rec(db.get(2)));
    step("put", "id=40 after reopen", done(db.put(40, &vector(40), &payload(40))));
    step("close", "a", done(db.close()));
    match Database::open_read_only(a) {
        Ok(mut ro) => {
            step("open_read_only", "a", format!("ok:{}", db_line(&ro)));
            step("read_only.put", "id=41", done(ro.put(41, &vector(41), b"x")));
            step("read_only.get", "id=40", opt_rec(ro.get(40)));
        }
        Err(error) => step("open_read_only", "a", format!("err:{}", err(&error))),
    }
    step("inspect", "a", insp(maintenance::inspect(a)));
    step("verify", "a", res(maintenance::verify(a), |v| format!("{} bytes={}", inspection(&v.inspection), v.bytes)));
    let detail = maintenance::inspect_detail(a);
    step(
        "inspect_detail",
        "a",
        res(detail, |d| {
            format!(
                "{} lineage={} collections={} imports={}",
                inspection(&d.inspection),
                d.lineage.is_some(),
                d.collections,
                d.open_imports
            )
        }),
    );
    step("image", "a", file_image(a));
    let compacted = dir.join("a_compact.vlite");
    let report = maintenance::compact_at(a, &compacted, T0 + 2000);
    step("compact_at", "a T0+2000", compaction(report));
    step("image", "a_compact", file_image(&compacted));
    let report = maintenance::compact(a, dir.join("a_compact_now.vlite"));
    step("compact", "a wallclock", compaction(report));
    step("compact.exists", "a", res(maintenance::compact_at(a, &compacted, T0), |_| String::new()));
    match Database::open_read_only(&compacted) {
        Ok(db) => {
            step("open_read_only", "a_compact", format!("ok:{}", db_line(&db)));
            step("a_compact.browse_at", "T0", page_txt(db.browse_at(None, 100, T0)));
            step("a_compact.collections", "", collection_lines(&db));
            step("a_compact.feed", "", feed(&db, &FeedCursor::beginning(), 1000));
        }
        Err(error) => step("open_read_only", "a_compact", format!("err:{}", err(&error))),
    }
    let backup = dir.join("a_backup.vlite");
    let restored = dir.join("a_restore.vlite");
    step("backup", "a", insp(maintenance::backup(a, &backup)));
    step("image", "a_backup", file_image(&backup));
    step("restore", "a_backup", insp(maintenance::restore(&backup, &restored)));
    step("restore.exists", "a_backup", insp(maintenance::restore(&backup, &restored)));
    step("image", "a_restore", file_image(&restored));
    let lineages = (
        Database::open_read_only(&backup).ok().and_then(|db| db.lineage()),
        Database::open_read_only(&restored).ok().and_then(|db| db.lineage()),
    );
    step(
        "restore.lineage",
        "",
        format!(
            "backup_same={} restore_fresh={}",
            lineages.0 == lineage,
            lineages.1.is_some() && lineages.1 != lineage
        ),
    );
}

fn encrypted_steps(dir: &Path, plain: &Path) {
    let path = dir.join("e.vlite");
    let mut options = CreateOptions::default();
    options.encryption = Some(key());
    match Database::create_with_options(&path, DIMS, options) {
        Ok(mut db) => {
            step("enc.create", "", format!("ok:{}", db_line(&db)));
            for id in 9..=11u64 {
                let result = db.put_with_metadata(id, &vector(id), &payload(id), &meta(id));
                step("enc.put_with_metadata", &format!("id={id}"), done(result));
            }
            step("enc.delete", "id=10", flag(db.delete(10)));
            step("enc.feed", "", feed(&db, &FeedCursor::beginning(), 100));
            step("enc.close", "", done(db.close()));
        }
        Err(error) => return step("enc.create", "", format!("err:{}", err(&error))),
    }
    let wrong = EncryptionKey::from_bytes(WRONG_KEY);
    step("enc.open.no_key", "", done(Database::open(&path)));
    step("enc.open.wrong_key", "", done(Database::open_encrypted(&path, &wrong)));
    step("enc.open_read_only.no_key", "", done(Database::open_read_only(&path)));
    step("plain.open_encrypted", "a", done(Database::open_encrypted(plain, &key())));
    match Database::open_encrypted(&path, &key()) {
        Ok(db) => {
            step("enc.open", "", format!("ok:{}", db_line(&db)));
            step("enc.get", "id=9", opt_rec(db.get(9)));
            step("enc.get", "id=10", opt_rec(db.get(10)));
            step(
                "enc.search",
                "q2 k=5",
                res(db.search_filtered_with_metric_at(&Q2, 5, &[], DistanceMetric::SquaredL2, T0), |found| {
                    hits(&found)
                }),
            );
        }
        Err(error) => step("enc.open", "", format!("err:{}", err(&error))),
    }
    let read_only = Database::open_read_only_encrypted(&path, &key());
    step("enc.open_read_only", "", same(read_only.map(|db| db_line(&db))));
    step("enc.inspect.no_key", "", insp(maintenance::inspect(&path)));
    step("enc.verify.no_key", "", res(maintenance::verify(&path), |v| inspection(&v.inspection)));
    step("enc.inspect_with_key", "", insp(maintenance::inspect_with_key(&path, &key())));
    step("enc.inspect_with_key.wrong", "", insp(maintenance::inspect_with_key(&path, &wrong)));
    let backup = dir.join("e_backup.vlite");
    let restored = dir.join("e_restore.vlite");
    let compacted = dir.join("e_compact.vlite");
    step("enc.backup.no_key", "", insp(maintenance::backup(&path, dir.join("e_nokey.vlite"))));
    step("enc.backup_with_key", "", insp(maintenance::backup_with_key(&path, &backup, &key())));
    step("enc.backup.identical", "", (std::fs::read(&path).ok() == std::fs::read(&backup).ok()).to_string());
    step("enc.restore_with_key", "", insp(maintenance::restore_with_key(&backup, &restored, &key())));
    let report = maintenance::compact_with_key(&path, &compacted, &key());
    step("enc.compact_with_key", "", compaction(report));
    for (label, file) in [("restore", &restored), ("compact", &compacted)] {
        let opened = Database::open_read_only_encrypted(file, &key());
        let rendered = res(opened, |db| format!("{} {}", db_line(&db), page_txt(db.browse_at(None, 100, T0))));
        step("enc.reopen", label, rendered);
    }
}

fn timestamp_steps(dir: &Path) {
    let path = dir.join("t.vlite");
    let mut options = CreateOptions::default();
    options.commit_timestamps = true;
    match Database::create_with_options(&path, DIMS, options) {
        Ok(mut db) => {
            let mut tick = T0;
            db.set_commit_clock(Box::new(move || {
                tick += 1000;
                tick
            }));
            step("ts.create", "", format!("ok:{} ts={}", db_line(&db), db.has_commit_timestamps()));
            for id in 1..=3u64 {
                step("ts.put", &format!("id={id}"), done(db.put(id, &vector(id), &payload(id))));
            }
            step("ts.delete", "id=2", flag(db.delete(2)));
            let batch = [Mutation::Put { id: 4, vector: vector(4), payload: payload(4) }, Mutation::Delete { id: 1 }];
            step("ts.write_batch", "4,-1", done(db.write_batch(&batch)));
            step("ts.feed", "", feed(&db, &FeedCursor::beginning(), 100));
            step("ts.close", "", done(db.close()));
        }
        Err(error) => return step("ts.create", "", format!("err:{}", err(&error))),
    }
    match Database::open_read_only(&path) {
        Ok(db) => {
            step("ts.reopen", "", format!("ok:{} ts={}", db_line(&db), db.has_commit_timestamps()));
            step("ts.reopen.feed", "", feed(&db, &FeedCursor::beginning(), 100));
        }
        Err(error) => step("ts.reopen", "", format!("err:{}", err(&error))),
    }
    step("image", "t", file_image(&path));
}

fn legacy_steps(dir: &Path) {
    for (label, format) in [("v3", FormatVersion::V3), ("v4", FormatVersion::V4)] {
        let path = dir.join(format!("{label}.vlite"));
        let op = |name: &str| format!("{label}.{name}");
        match Database::create_with_format(&path, DIMS, format) {
            Ok(mut db) => {
                step(&op("create"), "", format!("ok:{}", db_line(&db)));
                for id in 1..=4u64 {
                    step(&op("put"), &format!("id={id}"), done(db.put(id, &vector(id), &payload(id))));
                }
                let result = db.put_with_metadata(9, &vector(9), &payload(9), &meta(9));
                step(&op("put_with_metadata"), "id=9", done(result));
                step(&op("delete"), "id=2", flag(db.delete(2)));
                step(&op("update_metadata"), "id=9", num(db.update_metadata(9, &meta(10))));
                let ttl = db.put_with_expiry(30, &vector(30), &payload(30), &Metadata::new(), Some(T0));
                step(&op("put_with_expiry"), "id=30", num(ttl));
                step(
                    &op("create_collection"),
                    "docs",
                    res(db.create_collection("docs", docs_spec()), |info| info.name),
                );
                step(&op("feed"), "", feed(&db, &FeedCursor::beginning(), 100));
                step(
                    &op("search"),
                    "q1 k=10",
                    res(db.search_filtered_with_metric_at(&Q1, 10, &[], DistanceMetric::SquaredL2, T0), |found| {
                        hits(&found)
                    }),
                );
                step(&op("close"), "", done(db.close()));
            }
            Err(error) => {
                step(&op("create"), "", format!("err:{}", err(&error)));
                continue;
            }
        }
        step(&op("inspect"), "", insp(maintenance::inspect(&path)));
        step(&op("image"), "", file_image(&path));
        let compacted = dir.join(format!("{label}_compact.vlite"));
        let report = maintenance::compact_at(&path, &compacted, T0);
        step(&op("compact_at"), "", compaction(report));
        step(&op("compact.image"), "", file_image(&compacted));
    }
}

fn memory_steps() {
    let mut db = match Database::create_in_memory(DIMS, CreateOptions::default()) {
        Ok(db) => db,
        Err(error) => return step("mem.create", "", format!("err:{}", err(&error))),
    };
    step("mem.create", "", format!("ok:{}", db_line(&db)));
    for id in 1..=3u64 {
        step("mem.put", &format!("id={id}"), done(db.put(id, &vector(id), &payload(id))));
    }
    step("mem.delete", "id=1", flag(db.delete(1)));
    match db.to_bytes() {
        Some(bytes) => {
            step("mem.to_bytes", "", masked(&bytes));
            let copy = Database::open_from_bytes(bytes, None);
            step("mem.open_from_bytes", "", res(copy, |copy| format!("{} {}", db_line(&copy), opt_rec(copy.get(2)))));
        }
        None => step("mem.to_bytes", "", "none".into()),
    }
    step("mem.open_from_bytes.garbage", "", done(Database::open_from_bytes(b"not a database".to_vec(), None)));
    step("mem.close", "", done(db.close()));
}

fn manifest(names: &[&str]) -> Vec<u8> {
    let mut text = String::from("vlite-manifest 1\n");
    for name in names {
        text.push_str(&format!("file {name}\n"));
    }
    let crc = crc32(text.as_bytes());
    text.push_str(&format!("crc {crc:08x}\n"));
    text.into_bytes()
}

fn listing(store: &Path) -> String {
    let mut names: Vec<String> = match std::fs::read_dir(store) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(error) => return format!("io:{:?}", error.kind()),
    };
    names.sort();
    names.join(",")
}

/// A candidate directory store built from bytes (the v0.6.1 API has no
/// directory constructor): a closed v5 file becomes `wal-1.log`.
fn directory_steps(dir: &Path, source: &Path) {
    let store = dir.join("store.d");
    let built = std::fs::create_dir_all(&store)
        .and_then(|()| std::fs::copy(source, store.join("wal-1.log")).map(|_| ()))
        .and_then(|()| std::fs::write(store.join("LOCK"), b""))
        .and_then(|()| std::fs::write(store.join("MANIFEST"), manifest(&["wal-1.log"])));
    step("dir.build", "", done(built.map_err(Error::from)));
    match Database::open(&store) {
        Ok(mut db) => {
            step("dir.open", "", format!("ok:{}", db_line(&db)));
            step("dir.browse_at", "T0", page_txt(db.browse_at(None, 100, T0)));
            step("dir.collections", "", collection_lines(&db));
            step("dir.feed", "", feed(&db, &FeedCursor::beginning(), 1000));
            step("dir.put", "id=50", done(db.put(50, &vector(50), &payload(50))));
            step("dir.delete", "id=2", flag(db.delete(2)));
            step("dir.close", "", done(db.close()));
        }
        Err(error) => return step("dir.open", "", format!("err:{}", err(&error))),
    }
    step("dir.listing", "after close", listing(&store));
    match Database::open(&store) {
        Ok(db) => {
            step("dir.reopen", "", format!("ok:{}", db_line(&db)));
            step("dir.get", "id=50", opt_rec(db.get(50)));
            step("dir.get", "id=2", opt_rec(db.get(2)));
            step("dir.second_writer", "", done(Database::open(&store)));
            step("dir.close", "reopen", done(db.close()));
        }
        Err(error) => step("dir.reopen", "", format!("err:{}", err(&error))),
    }
    match Database::open_read_only(&store) {
        Ok(mut db) => {
            step("dir.open_read_only", "", format!("ok:{}", db_line(&db)));
            step("dir.read_only.put", "id=51", done(db.put(51, &vector(51), b"x")));
        }
        Err(error) => step("dir.open_read_only", "", format!("err:{}", err(&error))),
    }
    step("dir.inspect", "", insp(maintenance::inspect(&store)));
}

fn workload(dir: &Path) {
    let a = dir.join("a.vlite");
    step("create.bad_dims", "dims=0", done(Database::create(dir.join("bad0.vlite"), 0)));
    let missing = dir.join("missing").join("x.vlite");
    step("create.missing_parent", "", done(Database::create(missing, DIMS)));
    let mut db = match Database::create(&a, DIMS) {
        Ok(db) => db,
        Err(error) => return step("create", "a", format!("err:{}", err(&error))),
    };
    step("create", "a", format!("ok:{}", db_line(&db)));
    step("create.exists", "a", done(Database::create(&a, DIMS)));
    step("to_bytes.file", "", db.to_bytes().is_some().to_string());
    core_steps(&mut db);
    search_steps(&db);
    batch_ttl_steps(&mut db);
    collection_steps(&mut db);
    feed_steps(&db);
    lifecycle_steps(dir, &a, db);
    encrypted_steps(dir, &a);
    timestamp_steps(dir);
    legacy_steps(dir);
    memory_steps();
    directory_steps(dir, &a);
}

fn populate(db: &mut Database, collections: bool, v5: bool) -> Vec<String> {
    let mut failures = Vec::new();
    let mut note = |label: &str, result: Res<()>| {
        if let Err(error) = result {
            failures.push(format!("{label}:{}", err(&error)));
        }
    };
    for id in 1..=8u64 {
        note("put", db.put(id, &vector(id), &payload(id)));
    }
    // V3/V4 files refuse metadata writes (ExpiryRequiresUpgrade); they get plain puts.
    for id in 9..=12u64 {
        let put = if v5 {
            db.put_with_metadata(id, &vector(id), &payload(id), &meta(id))
        } else {
            db.put(id, &vector(id), &payload(id))
        };
        note("put", put);
    }
    note("delete", db.delete(3).map(|_| ()));
    note("upsert", db.put(2, &[0.0, 2.0, 0.0, 0.0], b"p2b"));
    if v5 {
        note("update_metadata", db.update_metadata(9, &meta(13)).map(|_| ()));
        note("ttl", db.put_with_expiry(30, &vector(30), &payload(30), &meta(30), Some(T0 + 1000)).map(|_| ()));
        note(
            "ttl",
            db.put_with_expiry(31, &vector(31), &payload(31), &Metadata::new(), Some(T0 + 1_000_000_000)).map(|_| ()),
        );
    }
    let batch = [Mutation::Put { id: 20, vector: vector(20), payload: payload(20) }, Mutation::Delete { id: 1 }];
    note("write_batch", db.write_batch(&batch));
    if collections {
        note("create_collection", db.create_collection("docs", docs_spec()).map(|_| ()));
        match db.collection_mut("docs") {
            Ok(mut docs) => {
                for id in 1..=4u64 {
                    let mut metadata = meta(id);
                    metadata.insert("text".into(), MetadataValue::String(TEXTS[(id - 1) as usize].into()));
                    let put = if v5 {
                        docs.put_with_metadata(id, &vector(id), &payload(id), &metadata)
                    } else {
                        docs.put(id, &vector(id), &payload(id))
                    };
                    note("docs.put", put.map(|_| ()));
                }
                note("docs.delete", docs.delete(4).map(|_| ()));
            }
            Err(error) => note("collection_mut", Err(error)),
        }
    }
    failures
}

fn produce_line(name: &str, outcome: Res<Vec<String>>) {
    let text = match outcome {
        Ok(failures) if failures.is_empty() => "ok".to_string(),
        Ok(failures) => format!("partial:{}", failures.join(";")),
        Err(error) => format!("err:{}", err(&error)),
    };
    println!("PRODUCE\t{}\t{}", esc(name), esc(&text));
}

fn produce_with(open: Res<Database>, collections: bool, v5: bool) -> Res<Vec<String>> {
    let mut db = open?;
    let failures = populate(&mut db, collections, v5);
    db.close()?;
    Ok(failures)
}

fn produce(dir: &Path) {
    let v5 = dir.join("b_v5.vlite");
    produce_line("b_v5", produce_with(Database::create(&v5, DIMS), true, true));
    for (name, format, collections) in [("b_v3", FormatVersion::V3, false), ("b_v4", FormatVersion::V4, true)] {
        let path = dir.join(format!("{name}.vlite"));
        produce_line(name, produce_with(Database::create_with_format(&path, DIMS, format), collections, false));
    }
    let ts = dir.join("b_ts.vlite");
    let mut options = CreateOptions::default();
    options.commit_timestamps = true;
    let opened = Database::create_with_options(&ts, DIMS, options).map(|mut db| {
        let mut tick = T0;
        db.set_commit_clock(Box::new(move || {
            tick += 1000;
            tick
        }));
        db
    });
    produce_line("b_ts", produce_with(opened, true, true));
    let enc = dir.join("enc_b.vlite");
    let mut options = CreateOptions::default();
    options.encryption = Some(key());
    produce_line("enc_b", produce_with(Database::create_with_options(&enc, DIMS, options), true, true));
    let report = maintenance::compact_at(&v5, dir.join("b_compact.vlite"), T0 + 2000);
    produce_line("b_compact", report.map(|_| Vec::new()));
    let backup = dir.join("b_v5_backup.vlite");
    let restored =
        maintenance::backup(&v5, &backup).and_then(|_| maintenance::restore(&backup, dir.join("b_restore.vlite")));
    produce_line("b_restore", restored.map(|_| Vec::new()));
    let memory = Database::create_in_memory(DIMS, CreateOptions::default()).and_then(|mut db| {
        let failures = populate(&mut db, true, true);
        let bytes = db.to_bytes().unwrap_or_default();
        std::fs::write(dir.join("b_mem.vlite"), bytes)?;
        Ok(failures)
    });
    produce_line("b_mem", memory);
    let torn = std::fs::read(&v5).and_then(|mut bytes| {
        bytes.extend_from_slice(b"VREC\x01\x00\x00\x00\x00\x00\x00");
        std::fs::write(dir.join("b_torn.vlite"), bytes)
    });
    produce_line("b_torn", torn.map(|()| Vec::new()).map_err(Error::from));
}

fn open_named(name: &str, path: &Path, read_only: bool) -> Res<Database> {
    match (name.contains("enc"), read_only) {
        (true, true) => Database::open_read_only_encrypted(path, &key()),
        (true, false) => Database::open_encrypted(path, &key()),
        (false, true) => Database::open_read_only(path),
        (false, false) => Database::open(path),
    }
}

fn inspect_named(name: &str, path: &Path) -> Res<Inspection> {
    if name.contains("enc") { maintenance::inspect_with_key(path, &key()) } else { maintenance::inspect(path) }
}

fn dump(name: &str, path: &Path) {
    let line = |section: &str, value: String| {
        println!("DUMP\t{}\t{}\t{}", esc(name), esc(section), esc(&value));
    };
    let db = match open_named(name, path, true) {
        Ok(db) => db,
        Err(error) => return line("open", format!("err:{}", err(&error))),
    };
    line("open", format!("ok:{} ts={}", db_line(&db), db.has_commit_timestamps()));
    let mut after = None;
    loop {
        match db.browse_at(after, 1000, T0) {
            Ok(page) => {
                for hit in &page.hits {
                    let rev = db.revision(hit.id).ok().flatten();
                    let exp = db.expires_at(hit.id).ok().flatten();
                    line("record", format!("{} {} rev={rev:?} exp={exp:?}", hit.id, rec(&hit.record)));
                }
                after = page.next_after;
                if after.is_none() {
                    break;
                }
            }
            Err(error) => {
                line("browse", format!("err:{}", err(&error)));
                break;
            }
        }
    }
    line("collections", collection_lines(&db));
    for info in db.collections().unwrap_or_default() {
        if info.name == "default" {
            continue;
        }
        if let Ok(collection) = db.collection(&info.name) {
            let section = format!("collection.{}", info.name);
            line(&section, page_txt(collection.browse_at(None, 1000, T0)));
            let query = HybridQuery::new(Some(Q1.to_vec()), "alpha gamma", 5);
            line(&format!("{section}.hybrid"), hybrid_line(collection.search_hybrid(&query)));
        }
    }
    for metric in METRICS {
        let result = db.search_filtered_with_metric_at(&Q2, 50, &[], metric, T0);
        line(&format!("search.{metric:?}"), hits_txt(result));
    }
    line("feed", feed(&db, &FeedCursor::beginning(), 10_000));
    drop(db);
    line("inspect", insp(inspect_named(name, path)));
}

fn append(name: &str, path: &Path) {
    let line = |op: &str, value: String| {
        println!("APPEND\t{}\t{}\t{}", esc(name), esc(op), esc(&value));
    };
    let mut db = match open_named(name, path, false) {
        Ok(db) => db,
        Err(error) => return line("open", format!("err:{}", err(&error))),
    };
    line("open", format!("ok:{}", db_line(&db)));
    let mut tick = T0 + 50_000;
    db.set_commit_clock(Box::new(move || {
        tick += 7;
        tick
    }));
    let version = db.format_version();
    line("put", done(db.put(60, &vector(60), &payload(60))));
    line("upsert", done(db.put(2, &[0.0, 3.0, 0.0, 0.0], b"p2c")));
    line("delete", flag(db.delete(6)));
    line("put_with_metadata", done(db.put_with_metadata(61, &vector(61), &payload(61), &meta(61))));
    let batch = [Mutation::Put { id: 62, vector: vector(62), payload: payload(62) }, Mutation::Delete { id: 7 }];
    line("write_batch", done(db.write_batch(&batch)));
    if version >= 5 {
        line("update_metadata", num(db.update_metadata(10, &meta(14))));
        let ttl = db.put_with_expiry(63, &vector(63), &payload(63), &Metadata::new(), Some(T0 + 500_000));
        line("put_with_expiry", num(ttl));
    }
    if version >= 4 {
        line("create_collection", res(db.create_collection("more", docs_spec()), |info| info.name));
        if let Ok(mut more) = db.collection_mut("more") {
            let result = more.put_with_metadata(1, &vector(1), &payload(1), &meta(1));
            line("more.put_with_metadata", num(result));
        }
        if let Ok(mut docs) = db.collection_mut("docs") {
            line("docs.delete", flag(docs.delete(1)));
        }
    }
    line("close", done(db.close()));
}

fn check(name: &str, path: &Path) {
    let line = |entry: &str, outcome: String| {
        let (status, detail) = outcome.split_once(':').unwrap_or(("err", outcome.as_str()));
        println!("RESULT\t{}\t{}\t{}\t{}", esc(name), esc(entry), status, esc(detail));
    };
    let summary = |db: Database| {
        let ids = match db.browse_at(None, 1000, T0) {
            Ok(page) => format!("{:?}", page.hits.iter().map(|hit| hit.id).collect::<Vec<_>>()),
            Err(error) => format!("browse-err {}", err(&error)),
        };
        format!("{} ids={ids}", db_line(&db))
    };
    let enc = name.contains("enc");
    line("open_read_only", res(open_named(name, path, true), summary));
    if path.is_file() {
        let bytes = std::fs::read(path).unwrap_or_default();
        let key = key();
        let opened = Database::open_from_bytes(bytes, if enc { Some(&key) } else { None });
        line("open_from_bytes", res(opened, summary));
    }
    line("inspect", insp(inspect_named(name, path)));
    let verified = if enc { maintenance::verify_with_key(path, &key()) } else { maintenance::verify(path) };
    line("verify", res(verified, |v| inspection(&v.inspection)));
    line("open", res(open_named(name, path, false), summary));
}

fn make_encrypted(path: &Path) {
    let mut options = CreateOptions::default();
    options.encryption = Some(key());
    let made = Database::create_with_options(path, 2, options).and_then(|mut db| {
        db.put(7, &[1.0, 2.0], b"probe")?;
        db.close()
    });
    println!("MADE\t{}", done(made));
}

fn named(args: &[String]) -> Vec<(String, PathBuf)> {
    args.iter()
        .filter_map(|arg| arg.split_once('='))
        .map(|(name, path)| (name.to_string(), PathBuf::from(path)))
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("workload") if rest.len() == 1 => workload(Path::new(&rest[0])),
        Some("produce") if rest.len() == 1 => produce(Path::new(&rest[0])),
        Some("make-encrypted") if rest.len() == 1 => make_encrypted(Path::new(&rest[0])),
        Some("dump") => named(rest).iter().for_each(|(name, path)| dump(name, path)),
        Some("append") => named(rest).iter().for_each(|(name, path)| append(name, path)),
        Some("check") => named(rest).iter().for_each(|(name, path)| check(name, path)),
        _ => {
            eprintln!(
                "usage: cross_version_probe workload|produce|make-encrypted <path> | dump|append|check name=path..."
            );
            std::process::exit(2);
        }
    }
}
