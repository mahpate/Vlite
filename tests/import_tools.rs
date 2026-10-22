#![cfg(feature = "tools")]
mod cli_support;
use cli_support::{Fixture, success};
use std::{
    path::Path,
    process::{Command, Output},
};
fn import(db: &Path, input: &Path, format: &str, extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vlite-import"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "--input",
            input.to_str().unwrap(),
            "--format",
            format,
        ])
        .args(extra)
        .output()
        .unwrap()
}
#[test]
fn csv_jsonl_copy_and_resume() {
    for (format, text) in [
        (
            "csv",
            "id,vector,payload\n1,\"[1,2]\",hello\n2,\"[3,4]\",world\n",
        ),
        (
            "jsonl",
            "{\"id\":1,\"vector\":[1,2]}\n{\"id\":2,\"vector\":[3,4]}\n",
        ),
        ("pgvector", "id,vector\n1,\"[1,2]\"\n2,\"[3,4]\"\n"),
        (
            "copy",
            "COPY public.items (id, vector, payload) FROM stdin;\n1\t[1,2]\thello\\tworld\n2\t[3,4]\t\\N\n\\.\n",
        ),
    ] {
        let f = Fixture::new();
        let input = f.path("input");
        let db = f.path("db");
        std::fs::write(&input, text).unwrap();
        let out = success(import(
            &db,
            &input,
            format,
            &["--create", "2", "--chunk-size", "1", "--job", "42"],
        ));
        assert!(out.contains("\"rows_committed\":2"));
        let before = std::fs::read(&db).unwrap();
        success(import(
            &db,
            &input,
            format,
            &["--chunk-size", "1", "--job", "42"],
        ));
        assert_eq!(before, std::fs::read(&db).unwrap());
        let db = vlite::Database::open(db).unwrap();
        assert_eq!(db.len(), 2);
        assert_eq!(db.get(2).unwrap().unwrap().vector, vec![3.0, 4.0]);
    }
}
#[test]
fn invalid_rows_publish_nothing() {
    let f = Fixture::new();
    let input = f.path("rows");
    let db = f.path("db");
    cli_support::database(&db);
    let before = std::fs::read(&db).unwrap();
    for rows in [
        "{\"id\":100,\"vector\":[1,2]}\n{\"id\":101,\"vector\":[1]}\n",
        "{\"id\":100,\"vector\":[1,2]}\nnot json\n",
        "{\"id\":100,\"vector\":[1,2]}\n{\"id\":101,\"vector\":[1,2],\"metadata\":{\"\":{\"type\":\"boolean\",\"value\":true}}}\n",
    ] {
        std::fs::write(&input, rows).unwrap();
        let out = import(&db, &input, "jsonl", &["--chunk-size", "1"]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("rejected"));
        assert_eq!(before, std::fs::read(&db).unwrap());
    }
}
#[test]
fn invalid_create_does_not_create_destination() {
    let f = Fixture::new();
    let input = f.path("rows");
    let db = f.path("db");
    std::fs::write(&input, "{\"id\":1,\"vector\":[1]}\n").unwrap();
    assert!(
        !import(&db, &input, "jsonl", &["--create", "2"])
            .status
            .success()
    );
    assert!(!db.exists());
}
#[test]
fn faiss_flat_l2_ip_and_invalid_size() {
    for (magic, metric) in [(b"IxF2", 1i32), (b"IxFI", 0i32)] {
        let f = Fixture::new();
        let input = f.path("flat.faiss");
        let db = f.path("db");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(magic);
        bytes.extend_from_slice(&2i32.to_le_bytes());
        bytes.extend_from_slice(&2i64.to_le_bytes());
        bytes.extend_from_slice(&(1i64 << 20).to_le_bytes());
        bytes.extend_from_slice(&(1i64 << 20).to_le_bytes());
        bytes.push(1);
        bytes.extend_from_slice(&metric.to_le_bytes());
        bytes.extend_from_slice(&4u64.to_le_bytes());
        for v in [1f32, 2., 3., 4.] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(&input, &bytes).unwrap();
        success(import(&db, &input, "faiss", &["--create", "2"]));
        let dbh = vlite::Database::open(&db).unwrap();
        assert_eq!(dbh.get(1).unwrap().unwrap().vector, vec![3., 4.]);
        drop(dbh);
        bytes.push(0);
        std::fs::write(&input, bytes).unwrap();
        assert!(!import(&db, &input, "faiss", &[]).status.success());
    }
}
#[cfg(feature = "import-formats")]
#[test]
fn generated_parquet_arrow_file_and_stream() {
    use arrow_array::{ListArray, RecordBatch, UInt64Array, types::Float32Type};
    use arrow_schema::{DataType, Field, Schema};
    use std::{fs::File, sync::Arc};
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt64, false),
        Field::new(
            "vector",
            DataType::List(Arc::new(Field::new("item", DataType::Float32, true))),
            false,
        ),
    ]));
    let vectors = ListArray::from_iter_primitive::<Float32Type, _, _>([
        Some(vec![Some(1.), Some(2.)]),
        Some(vec![Some(3.), Some(4.)]),
    ]);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(UInt64Array::from(vec![1, 2])), Arc::new(vectors)],
    )
    .unwrap();
    for format in ["parquet", "arrow", "stream"] {
        let f = Fixture::new();
        let input = f.path("columnar");
        let db = f.path("db");
        let file = File::create(&input).unwrap();
        match format {
            "parquet" => {
                let mut w =
                    parquet::arrow::ArrowWriter::try_new(file, schema.clone(), None).unwrap();
                w.write(&batch).unwrap();
                w.close().unwrap();
            }
            "arrow" => {
                let mut w = arrow_ipc::writer::FileWriter::try_new(file, &schema).unwrap();
                w.write(&batch).unwrap();
                w.finish().unwrap();
            }
            _ => {
                let mut w = arrow_ipc::writer::StreamWriter::try_new(file, &schema).unwrap();
                w.write(&batch).unwrap();
                w.finish().unwrap();
            }
        }
        success(import(
            &db,
            &input,
            if format == "parquet" {
                "parquet"
            } else {
                "arrow"
            },
            &["--create", "2"],
        ));
        let db = vlite::Database::open(db).unwrap();
        assert_eq!(db.len(), 2);
        assert_eq!(db.get(2).unwrap().unwrap().vector, vec![3., 4.]);
    }
}

#[test]
fn resume_conflict_and_legacy_format_do_not_modify_files() {
    let f = Fixture::new();
    let input = f.path("rows");
    let db = f.path("db");
    std::fs::write(&input, "{\"id\":1,\"vector\":[1,2]}\n").unwrap();
    success(import(
        &db,
        &input,
        "jsonl",
        &["--create", "2", "--job", "7"],
    ));
    let before = std::fs::read(&db).unwrap();
    std::fs::write(&input, "{\"id\":1,\"vector\":[9,9]}\n").unwrap();
    assert!(
        !import(&db, &input, "jsonl", &["--job", "7"])
            .status
            .success()
    );
    assert_eq!(before, std::fs::read(&db).unwrap());
    let legacy = f.path("legacy");
    vlite::Database::create(&legacy, 2)
        .unwrap()
        .close()
        .unwrap();
    let before = std::fs::read(&legacy).unwrap();
    assert!(!import(&legacy, &input, "jsonl", &[]).status.success());
    assert_eq!(before, std::fs::read(&legacy).unwrap());
}
#[test]
fn imports_into_existing_named_collection() {
    let f = Fixture::new();
    let path = f.path("db");
    let input = f.path("rows");
    let mut db = vlite::Database::create_with_format(&path, 3, vlite::FormatVersion::V4).unwrap();
    db.create_collection(
        "named",
        vlite::CollectionSpec {
            dimensions: 2,
            settings: Default::default(),
        },
    )
    .unwrap();
    db.close().unwrap();
    std::fs::write(&input, "{\"id\":1,\"vector\":[1,2]}\n").unwrap();
    success(import(&path, &input, "jsonl", &["--collection", "named"]));
    let db = vlite::Database::open(path).unwrap();
    assert_eq!(db.len(), 0);
    assert_eq!(db.collection("named").unwrap().len(), 1);
}
