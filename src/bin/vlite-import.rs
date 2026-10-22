mod tool_support;
use serde::Deserialize;
use serde_json::json;
#[cfg(feature = "import-formats")]
use std::io::Read;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    process::ExitCode,
};
use tool_support::{Result, main_result};
use vlite::{Database, FormatVersion, ImportChunkOutcome, InvalidRowPolicy, Metadata, Mutation};
const HELP: &str = r#"vlite-import --db PATH --input FILE --format csv|jsonl|pgvector|copy|faiss|parquet|arrow
 [--create DIMENSIONS] [--collection NAME] [--job ID] [--chunk-size N]
CSV: header id,vector[,payload,metadata]; vector is a quoted JSON array.
JSONL: {"id":1,"vector":[1,2],"payload":"text","metadata":{}} per line.
pgvector: CSV above. copy: PostgreSQL tab-separated COPY text (id, vector, optional payload).
Parquet/Arrow IPC file or stream: id uint64/int64, vector list<float32/float64> or UTF8 JSON,
optional payload UTF8, metadata UTF8 typed JSON (requires --features import-formats).
FAISS: little-endian 64-bit IndexFlatL2/IP only (IxF2/IxFI), sequential IDs from 0.
Default: validate entire input, then ONE atomic resumable v4 chunk (max 4096 rows).
--chunk-size explicitly permits partial progress across chunks; --job resumes that same input.
No rows are published on parse/validation failure. Existing files are never upgraded.
"#;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    id: u64,
    vector: Vec<f32>,
    #[serde(default)]
    payload: String,
    #[serde(default)]
    metadata: Metadata,
}
impl Row {
    fn mutation(self) -> Mutation {
        Mutation::PutWithMetadata {
            id: self.id,
            vector: self.vector,
            payload: self.payload.into_bytes(),
            metadata: self.metadata,
        }
    }
}
fn columns(id: &str, vector: &str, payload: &str, metadata: &str) -> Result<Row> {
    Ok(Row {
        id: id.parse()?,
        vector: serde_json::from_str(vector)?,
        payload: payload.into(),
        metadata: if metadata.is_empty() {
            Metadata::new()
        } else {
            serde_json::from_str(metadata)?
        },
    })
}
fn text_rows(path: &str, format: &str) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    let mut rejected = 0;
    let mut accept = |row: Result<Row>, number: usize| match row {
        Ok(r) => rows.push(r),
        Err(_) => {
            rejected += 1;
            if rejected <= 20 {
                eprintln!("rejected row {number}: invalid {format} record");
            }
        }
    };
    if matches!(format, "csv" | "pgvector") {
        let mut reader = csv::Reader::from_path(path)?;
        let headers = reader.headers()?.clone();
        let id = headers
            .iter()
            .position(|h| h == "id")
            .ok_or("CSV missing id column")?;
        let vector = headers
            .iter()
            .position(|h| h == "vector")
            .ok_or("CSV missing vector column")?;
        let payload = headers.iter().position(|h| h == "payload");
        let metadata = headers.iter().position(|h| h == "metadata");
        for (i, record) in reader.records().enumerate() {
            accept(
                (|| {
                    let r = record?;
                    columns(
                        &r[id],
                        &r[vector],
                        payload.and_then(|j| r.get(j)).unwrap_or(""),
                        metadata.and_then(|j| r.get(j)).unwrap_or(""),
                    )
                })(),
                i + 2,
            );
        }
    } else {
        for (i, line) in BufReader::new(File::open(path)?).lines().enumerate() {
            let line = line?;
            if format == "copy" && (line.starts_with("COPY ") || line == "\\." || line.is_empty()) {
                continue;
            }
            accept(
                if format == "jsonl" {
                    serde_json::from_str(&line).map_err(Into::into)
                } else {
                    (|| {
                        let fields: Vec<_> = line.split('\t').collect();
                        if !(2..=3).contains(&fields.len()) {
                            return Err("COPY expects 2 or 3 fields".into());
                        }
                        let payload = fields.get(2).copied().unwrap_or("");
                        columns(fields[0], fields[1], &copy_unescape(payload)?, "")
                    })()
                },
                i + 1,
            );
        }
    }
    if rejected > 0 {
        return Err(format!("rejected_rows={rejected}; no rows published").into());
    }
    Ok(rows)
}
fn copy_unescape(text: &str) -> Result<String> {
    if text == "\\N" {
        return Ok(String::new());
    }
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next() {
            Some('t') => '\t',
            Some('n') => '\n',
            Some('r') => '\r',
            Some('\\') => '\\',
            _ => return Err("unsupported COPY escape".into()),
        });
    }
    Ok(out)
}
fn faiss(path: &str) -> Result<Vec<Row>> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 45 {
        return Err("truncated FAISS header".into());
    }
    let metric = match &bytes[..4] {
        b"IxF2" => 1,
        b"IxFI" => 0,
        _ => return Err("only FAISS IndexFlatL2/IP IxF2/IxFI supported".into()),
    };
    let d = i32::from_le_bytes(bytes[4..8].try_into()?);
    let n = i64::from_le_bytes(bytes[8..16].try_into()?);
    if d <= 0
        || d as usize > vlite::MAX_DIMENSIONS
        || n < 0
        || bytes[32] != 1
        || i32::from_le_bytes(bytes[33..37].try_into()?) != metric
    {
        return Err("invalid FAISS header".into());
    }
    let count = u64::from_le_bytes(bytes[37..45].try_into()?);
    let expected = (d as u64)
        .checked_mul(n as u64)
        .ok_or("FAISS size overflow")?;
    if count != expected
        || count.checked_mul(4).and_then(|x| x.checked_add(45)) != Some(bytes.len() as u64)
    {
        return Err("FAISS size mismatch/trailing data".into());
    }
    let mut rows = Vec::new();
    for (id, vector) in bytes[45..].chunks_exact(d as usize * 4).enumerate() {
        rows.push(Row {
            id: id as u64,
            vector: vector
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect(),
            payload: String::new(),
            metadata: Metadata::new(),
        });
    }
    Ok(rows)
}
#[cfg(feature = "import-formats")]
fn columnar(path: &str, format: &str) -> Result<Vec<Row>> {
    use arrow_array::{
        Array, FixedSizeListArray, Float32Array, Float64Array, Int64Array, LargeListArray,
        ListArray, RecordBatch, StringArray, UInt64Array,
    };
    fn batch_rows(batch: RecordBatch, out: &mut Vec<Row>) -> Result<()> {
        let column = |name: &str| -> Result<&dyn Array> {
            Ok(batch
                .column_by_name(name)
                .ok_or_else(|| format!("missing column {name}"))?
                .as_ref())
        };
        let ids = column("id")?;
        let vectors = column("vector")?;
        for i in 0..batch.num_rows() {
            let text = |name: &str| -> Result<String> {
                match batch.column_by_name(name) {
                    None => Ok(String::new()),
                    Some(a) if a.is_null(i) => Ok(String::new()),
                    Some(a) => Ok(a
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or("optional columns must be UTF8")?
                        .value(i)
                        .into()),
                }
            };
            if ids.is_null(i) || vectors.is_null(i) {
                return Err(format!("rejected row {}: null id/vector", out.len() + 1).into());
            }
            let id = if let Some(a) = ids.as_any().downcast_ref::<UInt64Array>() {
                a.value(i)
            } else if let Some(a) = ids.as_any().downcast_ref::<Int64Array>() {
                u64::try_from(a.value(i))?
            } else {
                return Err("id must be uint64/int64".into());
            };
            let vector = if let Some(a) = vectors.as_any().downcast_ref::<StringArray>() {
                serde_json::from_str(a.value(i))?
            } else {
                let values = if let Some(a) = vectors.as_any().downcast_ref::<ListArray>() {
                    a.value(i)
                } else if let Some(a) = vectors.as_any().downcast_ref::<LargeListArray>() {
                    a.value(i)
                } else if let Some(a) = vectors.as_any().downcast_ref::<FixedSizeListArray>() {
                    a.value(i)
                } else {
                    return Err("vector must be float list or UTF8 JSON".into());
                };
                if values.null_count() != 0 {
                    return Err("rejected vector with null element".into());
                }
                if let Some(a) = values.as_any().downcast_ref::<Float32Array>() {
                    a.values().to_vec()
                } else if let Some(a) = values.as_any().downcast_ref::<Float64Array>() {
                    a.values().iter().map(|v| *v as f32).collect()
                } else {
                    return Err("vector list must contain float32/float64".into());
                }
            };
            let metadata = text("metadata")?;
            out.push(Row {
                id,
                vector,
                payload: text("payload")?,
                metadata: if metadata.is_empty() {
                    Metadata::new()
                } else {
                    serde_json::from_str(&metadata)?
                },
            });
        }
        Ok(())
    }
    let mut out = Vec::new();
    let mut file = File::open(path)?;
    if format == "parquet" {
        for batch in
            parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?.build()?
        {
            batch_rows(batch?, &mut out)?;
        }
    } else {
        let mut magic = [0; 6];
        file.read_exact(&mut magic)?;
        drop(file);
        let file = File::open(path)?;
        if &magic == b"ARROW1" {
            for batch in arrow_ipc::reader::FileReader::try_new(file, None)? {
                batch_rows(batch?, &mut out)?;
            }
        } else {
            for batch in arrow_ipc::reader::StreamReader::try_new(file, None)? {
                batch_rows(batch?, &mut out)?;
            }
        }
    }
    Ok(out)
}

fn metadata_bytes(metadata: &Metadata) -> Result<usize> {
    use vlite::MetadataValue;
    if metadata.len() > vlite::MAX_METADATA_FIELDS {
        return Err("metadata has too many fields".into());
    }
    let mut length = 4;
    for (key, value) in metadata {
        if key.is_empty() || key.len() > 256 {
            return Err("invalid metadata field name".into());
        }
        length += 3
            + key.len()
            + match value {
                MetadataValue::String(v) => {
                    if v.len() > 4096 {
                        return Err("metadata string too long".into());
                    }
                    4 + v.len()
                }
                MetadataValue::Integer(_) => 8,
                MetadataValue::Float(v) => {
                    if !v.is_finite() {
                        return Err("non-finite metadata".into());
                    }
                    8
                }
                MetadataValue::Boolean(_) => 1,
                MetadataValue::GeoPoint {
                    latitude,
                    longitude,
                } => {
                    if !latitude.is_finite()
                        || !longitude.is_finite()
                        || !(-90.0..=90.0).contains(latitude)
                        || !(-180.0..=180.0).contains(longitude)
                    {
                        return Err("invalid geo metadata".into());
                    }
                    16
                }
            };
    }
    if length > vlite::MAX_METADATA_BYTES {
        return Err("metadata too large".into());
    }
    Ok(length)
}

fn run() -> Result<()> {
    let args = tool_support::args()?;
    tool_support::check_args(
        &args,
        &[
            "--help",
            "--db",
            "--input",
            "--format",
            "--create",
            "--collection",
            "--job",
            "--chunk-size",
        ],
    )?;
    if args.contains_key("--help") {
        print!("{HELP}");
        return Ok(());
    }
    let path = tool_support::required(&args, "--db")?;
    let input = tool_support::required(&args, "--input")?;
    let format = tool_support::required(&args, "--format")?;
    let rows = match format {
        "csv" | "jsonl" | "pgvector" | "copy" => text_rows(input, format)?,
        "faiss" => faiss(input)?,
        "parquet" | "arrow" => {
            #[cfg(feature = "import-formats")]
            {
                columnar(input, format)?
            }
            #[cfg(not(feature = "import-formats"))]
            {
                return Err("format requires --features import-formats".into());
            }
        }
        _ => return Err("unknown format; use --help".into()),
    };
    let collection = args
        .get("--collection")
        .map(String::as_str)
        .unwrap_or("default");
    if args.contains_key("--create") && collection != "default" {
        return Err(
            "--create imports into default; pre-create named collections through the engine".into(),
        );
    }
    let chunk_size: usize = args
        .get("--chunk-size")
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(vlite::MAX_BATCH_OPERATIONS - 1);
    if !(1..vlite::MAX_BATCH_OPERATIONS).contains(&chunk_size) {
        return Err("chunk-size must be 1..4095 (one operation reserved for checkpoint)".into());
    }
    if !args.contains_key("--chunk-size") && rows.len() > chunk_size {
        return Err(
            "atomic import exceeds 4095 rows; --chunk-size permits resumable partial progress"
                .into(),
        );
    }
    // Complete syntactic/dimension/value validation before opening or creating a destination.
    let dims = if let Some(d) = args.get("--create") {
        d.parse()?
    } else {
        let db = Database::open(path)?;
        db.collection(collection)?.dimensions()
    };
    let mut rejected = 0;
    for (i, row) in rows.iter().enumerate() {
        if row.vector.len() != dims
            || row.vector.iter().any(|v| !v.is_finite())
            || row.payload.len() > vlite::MAX_PAYLOAD_BYTES
            || metadata_bytes(&row.metadata).is_err()
        {
            rejected += 1;
            eprintln!(
                "rejected row {}: dimensions/non-finite/payload/metadata limit",
                i + 1
            );
        }
    }
    if rejected > 0 {
        return Err(format!("rejected_rows={rejected}; no rows published").into());
    }
    for chunk in rows.chunks(chunk_size) {
        let bytes = chunk
            .iter()
            .try_fold(1024usize, |total, row| -> Result<usize> {
                Ok(total
                    + 64
                    + row.vector.len() * 4
                    + row.payload.len()
                    + metadata_bytes(&row.metadata)?)
            })?;
        if bytes > vlite::MAX_BATCH_BYTES {
            return Err("chunk exceeds atomic frame byte budget; use a smaller --chunk-size; no rows published".into());
        }
    }
    let mut db = if args.contains_key("--create") {
        Database::create_with_format(path, dims, FormatVersion::V4)?
    } else {
        Database::open(path)?
    };
    if db.format_version() < 4 {
        return Err("resumable imports require v4+; upgrade with vlite-maintain first".into());
    }
    let job = args
        .get("--job")
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(db.next_import_job_id()?);
    let resume = if args.contains_key("--job") {
        match db.import_status(job) {
            Ok(status) => {
                if status.state == vlite::ImportState::Aborted {
                    return Err("cannot resume aborted job".into());
                }
                Some(status)
            }
            Err(vlite::Error::ImportNotFound) => None,
            Err(e) => return Err(e.into()),
        }
    } else {
        None
    };
    let operations: Vec<_> = rows.into_iter().map(Row::mutation).collect();

    if let Some(status) = &resume
        && (status.last_chunk > operations.len().div_ceil(chunk_size) as u64
            || status.rows_committed
                != (status.last_chunk as usize)
                    .saturating_mul(chunk_size)
                    .min(operations.len()) as u64)
    {
        return Err("resume input length or chunk-size differs from checkpoint".into());
    }
    let mut committed = 0;
    for (i, chunk) in operations.chunks(chunk_size).enumerate() {
        if let Some(status) = &resume
            && i as u64 + 1 < status.last_chunk
        {
            committed += chunk.len();
            continue;
        }
        let report = db.collection_mut(collection)?.import_chunk(
            job,
            i as u64 + 1,
            chunk,
            InvalidRowPolicy::RejectChunk,
        )?;
        if report.outcome == ImportChunkOutcome::Rejected {
            for invalid in &report.invalid {
                eprintln!(
                    "rejected row {}: {}",
                    i * chunk_size + invalid.row as usize + 1,
                    invalid.code
                );
            }
            return Err(
                format!("rejected_rows={}; chunk not published", report.invalid_rows).into(),
            );
        }
        committed += chunk.len();
    }
    if !operations.is_empty() {
        db.collection_mut(collection)?.finish_import(job)?;
    }
    println!(
        "{}",
        json!({"job":job,"rows_committed":committed,"rejected_rows":0,"chunks":operations.len().div_ceil(chunk_size)})
    );
    db.close()?;
    Ok(())
}
fn main() -> ExitCode {
    main_result(run())
}
