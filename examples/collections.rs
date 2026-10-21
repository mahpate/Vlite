//! Create a new opt-in v4 file and exercise collection-scoped retrieval.
use std::{error::Error, path::PathBuf};
use vlite::{
    AnnOptions, CollectionSettings, CollectionSpec, Condition, Database, Expected, FormatVersion,
    HnswConfig, InvalidRowPolicy, Metadata, MetadataIndexKind, MetadataIndexSpec, MetadataValue,
    Mutation, Quantization, hybrid::HybridQuery,
};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(args.next().ok_or("usage: collections <new-file.vlite>")?);
    if args.next().is_some() {
        return Err("usage: collections <new-file.vlite>".into());
    }
    let mut db = Database::create_with_format(&path, 3, FormatVersion::V4)?;
    let config = HnswConfig::default();
    let mut settings = CollectionSettings::default();
    settings.text_fields = vec!["title".into()];
    settings.metadata_indexes = vec![MetadataIndexSpec {
        field: "category".into(),
        kind: MetadataIndexKind::Eq,
    }];
    settings.quantization = Quantization::ScalarU8;
    db.create_collection(
        "documents",
        CollectionSpec {
            dimensions: 3,
            settings,
        },
    )?;
    let rows = [
        (
            1,
            vec![1.0, 0.0, 0.0],
            "Vector database durability",
            "storage",
        ),
        (
            2,
            vec![0.9, 0.1, 0.0],
            "Database backup verification",
            "storage",
        ),
        (3, vec![0.0, 0.0, 1.0], "Garden plants", "outdoors"),
    ]
    .into_iter()
    .map(|(id, vector, title, category)| Mutation::PutWithMetadata {
        id,
        vector,
        payload: vec![],
        metadata: Metadata::from([
            ("title".into(), MetadataValue::String(title.into())),
            ("category".into(), MetadataValue::String(category.into())),
        ]),
    })
    .collect::<Vec<_>>();
    let job = db.next_import_job_id()?;
    {
        let mut collection = db.collection_mut("documents")?;
        collection.import_chunk(job, 1, &rows, InvalidRowPolicy::RejectChunk)?;
        collection.import_chunk(job, 1, &rows, InvalidRowPolicy::RejectChunk)?;
        collection.finish_import(job)?;
    }
    let revision = db
        .collection("documents")?
        .revision(1)?
        .ok_or("missing imported record revision")?;
    let record = db
        .collection("documents")?
        .get(1)?
        .ok_or("missing imported record")?
        .clone();
    db.collection_mut("documents")?.put_with_metadata_if(
        1,
        &record.vector,
        b"updated payload",
        &record.metadata,
        Expected::Revision(revision),
    )?;
    let filter = [Condition::Eq {
        field: "category".into(),
        value: MetadataValue::String("storage".into()),
    }];
    let query = [1.0, 0.0, 0.0];
    let planned = db
        .collection("documents")?
        .search_auto_with_settings(&query, 2, &filter, 100)?;
    if planned.hits.len() != 2 {
        return Err("indexed query did not return both storage records".into());
    }
    let mut hybrid = HybridQuery::new(Some(query.to_vec()), "database durability", 2);
    hybrid.conditions = filter.to_vec();
    let (hits, _) = db.collection("documents")?.search_hybrid(&hybrid)?;
    if hits.len() != 2 {
        return Err("hybrid query did not return both storage records".into());
    }
    let options = AnnOptions {
        quantization: Quantization::ScalarU8,
        rerank_multiplier: 4,
    };
    db.collection_mut("documents")?
        .build_hnsw_with_options(config, hybrid.metric, options)?;
    let memory = db
        .collection("documents")?
        .ann_memory(config, hybrid.metric, options)?;
    println!(
        "Imported and replayed one chunk; planner={}; hybrid_hits={}; quantized_code_bytes={} (not process RAM)",
        planned.plan.strategy.name(),
        hits.len(),
        memory.code_bytes,
    );
    db.close()?;
    let db = Database::open(&path)?;
    if db.collection("documents")?.len() != 3 || db.import_status(job)?.last_chunk != 1 {
        return Err("collection data or checkpoint did not survive reopen".into());
    }
    db.close()?;
    Ok(())
}
