#![cfg(feature = "tools")]
mod cli_support;
use cli_support::{
    Fixture, LONG_FLOAT, METADATA_FIELDS, STRING_VALUE_BYTES, database, largest_record_json,
};
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
    sync::{Mutex, MutexGuard, PoisonError},
};
use vlite::{Database, MAX_DIMENSIONS, MAX_PAYLOAD_BYTES, MetadataValue};

// The documented limit, written out so that a change of the constant fails here.
const CAP: usize = 16 * 1024 * 1024;
// A line over the cap that the reader has to drain: much longer than its read chunks.
const LONGER: usize = CAP + 100_000;
const TOO_LONG: &str = "line exceeds 16777216 bytes";
const COPY_HEADER: &str = "COPY public.items (id, vector, payload) FROM stdin;";

// The inputs here are tens of MiB. Holding this lock from before a file is built until the
// import has ended runs one at a time, which bounds the memory a loaded runner needs.
static HEAVY: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    HEAVY.lock().unwrap_or_else(PoisonError::into_inner)
}

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

fn describe(output: &Output) -> String {
    format!(
        "{:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// Runs an import that must succeed quietly and returns the summary it prints.
fn imported(db: &Path, input: &Path, format: &str, extra: &[&str]) -> Value {
    let output = import(db, input, format, extra);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(output.stderr.is_empty(), "{}", describe(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

// Runs an import that must fail with exit code 1 and exactly `stderr`, publish nothing and
// leave the existing database untouched.
fn refused(db: &Path, input: &Path, format: &str, stderr: &str) {
    let before = std::fs::read(db).unwrap();
    let output = import(db, input, format, &[]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(output.stdout.is_empty(), "{}", describe(&output));
    assert_eq!(String::from_utf8_lossy(&output.stderr), stderr);
    assert_eq!(std::fs::read(db).unwrap(), before, "nothing is published");
}

fn file(lines: &[String], terminator: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in lines {
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(terminator.as_bytes());
    }
    bytes
}

fn jsonl_row(id: u64) -> String {
    format!(r#"{{"id":{id},"vector":[{id},1],"payload":"p{id}"}}"#)
}

// A jsonl row of exactly `size` bytes: the spaces JSON allows after a value pad it out.
fn jsonl_padded(id: u64, size: usize) -> String {
    let row = jsonl_row(id);
    format!("{row}{}", " ".repeat(size - row.len()))
}

fn copy_row(id: u64) -> String {
    format!("{id}\t[{id},1]\tp{id}")
}

// A COPY row of exactly `size` bytes, padded with JSON spaces inside the vector field. A
// payload of that length would exceed the engine's payload limit instead.
fn copy_padded(id: u64, size: usize) -> String {
    let head = format!("{id}\t[{id},1]");
    let tail = format!("\tp{id}");
    format!("{head}{}{tail}", " ".repeat(size - head.len() - tail.len()))
}

fn payload(db: &Path, id: u64) -> Option<Vec<u8>> {
    Database::open(db)
        .unwrap()
        .get(id)
        .unwrap()
        .map(|record| record.payload.clone())
}

#[test]
fn jsonl_line_of_exactly_the_cap_is_imported_with_lf_and_crlf() {
    let _serial = serial();
    for terminator in ["\n", "\r\n"] {
        let f = Fixture::new();
        let (db, input) = (f.path("db"), f.path("rows"));
        database(&db);
        std::fs::write(
            &input,
            file(&[jsonl_padded(100, CAP), jsonl_row(101)], terminator),
        )
        .unwrap();
        let summary = imported(&db, &input, "jsonl", &[]);
        assert_eq!(summary["rows_committed"], 2, "{terminator:?}");
        assert_eq!(summary["rejected_rows"], 0, "{terminator:?}");
        assert_eq!(payload(&db, 100).unwrap(), b"p100");
        assert_eq!(payload(&db, 101).unwrap(), b"p101");
    }
}

#[test]
fn jsonl_line_over_the_cap_is_rejected_with_its_number_and_nothing_is_published() {
    let _serial = serial();
    for terminator in ["\n", "\r\n"] {
        for size in [CAP + 1, LONGER] {
            let f = Fixture::new();
            let (db, input) = (f.path("db"), f.path("rows"));
            database(&db);
            std::fs::write(
                &input,
                file(&[jsonl_row(100), jsonl_padded(101, size)], terminator),
            )
            .unwrap();
            refused(
                &db,
                &input,
                "jsonl",
                &format!("rejected row 2: {TOO_LONG}\nerror: rejected_rows=1; no rows published\n"),
            );
            assert_eq!(payload(&db, 100), None, "{terminator:?}, {size} bytes");
        }
    }
}

#[test]
fn jsonl_overlong_lines_are_numbered_like_every_line_and_reading_goes_on() {
    let _serial = serial();
    let f = Fixture::new();
    let (db, input) = (f.path("db"), f.path("rows"));
    database(&db);
    // Line 2 is blank, which a jsonl file has always rejected as a record. Lines 3 and 5 are
    // overlong, and the valid row on line 4 is not published either. Line 6 is still read.
    let lines = [
        jsonl_row(100),
        String::new(),
        jsonl_padded(1, CAP + 1),
        jsonl_row(101),
        jsonl_padded(1, LONGER),
        "not json".to_string(),
    ];
    std::fs::write(&input, file(&lines, "\n")).unwrap();
    refused(
        &db,
        &input,
        "jsonl",
        &format!(
            "rejected row 2: invalid jsonl record\n\
             rejected row 3: {TOO_LONG}\n\
             rejected row 5: {TOO_LONG}\n\
             rejected row 6: invalid jsonl record\n\
             error: rejected_rows=4; no rows published\n"
        ),
    );
}

#[test]
fn overlong_lines_count_toward_the_twenty_reported_rejections() {
    let _serial = serial();
    let f = Fixture::new();
    let (db, input) = (f.path("db"), f.path("rows"));
    database(&db);
    // Nineteen bad rows, then overlong lines 20 and 21 and one more bad row.
    let mut lines = vec!["not json".to_string(); 19];
    lines.extend([
        jsonl_padded(1, CAP + 1),
        jsonl_padded(1, LONGER),
        "not json".to_string(),
    ]);
    std::fs::write(&input, file(&lines, "\n")).unwrap();
    let mut expected = String::new();
    for number in 1..=19 {
        expected += &format!("rejected row {number}: invalid jsonl record\n");
    }
    expected += &format!("rejected row 20: {TOO_LONG}\n");
    expected += "error: rejected_rows=22; no rows published\n";
    refused(&db, &input, "jsonl", &expected);
}

#[test]
fn copy_line_of_exactly_the_cap_is_imported_with_lf_and_crlf() {
    let _serial = serial();
    for terminator in ["\n", "\r\n"] {
        let f = Fixture::new();
        let (db, input) = (f.path("db"), f.path("rows"));
        database(&db);
        let lines = [
            COPY_HEADER.to_string(),
            copy_padded(100, CAP),
            copy_row(101),
            "\\.".to_string(),
        ];
        std::fs::write(&input, file(&lines, terminator)).unwrap();
        let summary = imported(&db, &input, "copy", &[]);
        assert_eq!(summary["rows_committed"], 2, "{terminator:?}");
        assert_eq!(payload(&db, 100).unwrap(), b"p100");
        assert_eq!(payload(&db, 101).unwrap(), b"p101");
    }
}

#[test]
fn copy_line_over_the_cap_is_rejected_with_its_number_and_nothing_is_published() {
    let _serial = serial();
    for terminator in ["\n", "\r\n"] {
        for size in [CAP + 1, LONGER] {
            let f = Fixture::new();
            let (db, input) = (f.path("db"), f.path("rows"));
            database(&db);
            let lines = [
                COPY_HEADER.to_string(),
                copy_row(100),
                copy_padded(101, size),
                "\\.".to_string(),
            ];
            std::fs::write(&input, file(&lines, terminator)).unwrap();
            refused(
                &db,
                &input,
                "copy",
                &format!("rejected row 3: {TOO_LONG}\nerror: rejected_rows=1; no rows published\n"),
            );
            assert_eq!(payload(&db, 100), None, "{terminator:?}, {size} bytes");
        }
    }
}

#[test]
fn copy_overlong_lines_are_numbered_like_every_line_and_reading_goes_on() {
    let _serial = serial();
    let f = Fixture::new();
    let (db, input) = (f.path("db"), f.path("rows"));
    database(&db);
    // The header is line 1 and the blank line 3 is skipped; both still count. Lines 4 and 6
    // are overlong, the valid row on line 5 is not published either, line 7 is still read and
    // the `\.` line is unaffected.
    let lines = [
        COPY_HEADER.to_string(),
        copy_row(100),
        String::new(),
        copy_padded(1, CAP + 1),
        copy_row(101),
        copy_padded(1, LONGER),
        "not a row".to_string(),
        "\\.".to_string(),
    ];
    std::fs::write(&input, file(&lines, "\n")).unwrap();
    refused(
        &db,
        &input,
        "copy",
        &format!(
            "rejected row 4: {TOO_LONG}\n\
             rejected row 6: {TOO_LONG}\n\
             rejected row 7: invalid copy record\n\
             error: rejected_rows=3; no rows published\n"
        ),
    );
}

#[test]
fn a_payload_over_the_engine_limit_is_still_an_engine_rejection() {
    // The line limit does not replace the engine's: this payload is 2 MiB, the limit 1 MiB.
    let f = Fixture::new();
    let (db, input) = (f.path("db"), f.path("rows"));
    database(&db);
    let engine = "rejected row 1: dimensions/non-finite/payload/metadata limit\n\
                  error: rejected_rows=1; no rows published\n";
    let rows = format!(
        r#"{{"id":100,"vector":[1,2],"payload":"{}"}}"#,
        "x".repeat(2 << 20)
    );
    std::fs::write(&input, rows + "\n").unwrap();
    refused(&db, &input, "jsonl", engine);
    let rows = format!("100\t[1,2]\t{}\n", "x".repeat(2 << 20));
    std::fs::write(&input, rows).unwrap();
    refused(&db, &input, "copy", engine);
}

#[test]
fn invalid_utf8_in_a_line_that_fits_is_still_the_io_error() {
    let f = Fixture::new();
    let (db, input) = (f.path("db"), f.path("rows"));
    database(&db);
    for (format, rows) in [
        (
            "jsonl",
            &b"{\"id\":100,\"vector\":[1,2],\"payload\":\"\xff\"}\n"[..],
        ),
        ("copy", &b"100\t[1,2]\t\xff\n"[..]),
    ] {
        std::fs::write(&input, rows).unwrap();
        refused(
            &db,
            &input,
            format,
            "error: stream did not contain valid UTF-8\n",
        );
    }
}

#[test]
fn invalid_utf8_beyond_the_cap_is_never_checked() {
    let _serial = serial();
    let f = Fixture::new();
    let (db, input) = (f.path("db"), f.path("rows"));
    database(&db);
    let mut bytes = file(&[jsonl_row(100)], "\n");
    bytes.extend_from_slice(jsonl_padded(101, CAP + 1).as_bytes());
    bytes.extend_from_slice(b"\xff\xfe\n");
    std::fs::write(&input, bytes).unwrap();
    refused(
        &db,
        &input,
        "jsonl",
        &format!("rejected row 2: {TOO_LONG}\nerror: rejected_rows=1; no rows published\n"),
    );
}

#[test]
fn the_largest_jsonl_record_fits_the_line_cap_and_is_imported() {
    let _serial = serial();
    let f = Fixture::new();
    let (db, input) = (f.path("db"), f.path("rows"));
    let line = largest_record_json(1);
    assert!(
        (MAX_PAYLOAD_BYTES * 6 + MAX_DIMENSIONS * 30..CAP).contains(&line.len()),
        "{} bytes",
        line.len()
    );
    std::fs::write(&input, line + "\n").unwrap();
    let dimensions = MAX_DIMENSIONS.to_string();
    let summary = imported(&db, &input, "jsonl", &["--create", &dimensions]);
    assert_eq!(summary["rows_committed"], 1);
    let opened = Database::open(&db).unwrap();
    let record = opened.get(1).unwrap().unwrap();
    assert_eq!(record.vector.len(), MAX_DIMENSIONS);
    assert!(
        record
            .vector
            .iter()
            .all(|v| (v - 0.123_456_79).abs() < 1e-7)
    );
    assert_eq!(record.payload, vec![1u8; MAX_PAYLOAD_BYTES]);
    assert_eq!(record.metadata.len(), METADATA_FIELDS);
    for value in record.metadata.values() {
        assert_eq!(
            value,
            &MetadataValue::String("\u{1}".repeat(STRING_VALUE_BYTES))
        );
    }
}

#[test]
fn the_largest_copy_record_is_imported() {
    let _serial = serial();
    let f = Fixture::new();
    let (db, input) = (f.path("db"), f.path("rows"));
    // The payload is tabs, two characters each as COPY text.
    let line = format!(
        "1\t[{}]\t{}",
        vec![LONG_FLOAT; MAX_DIMENSIONS].join(","),
        "\\t".repeat(MAX_PAYLOAD_BYTES)
    );
    assert!(line.len() < CAP);
    std::fs::write(&input, line + "\n").unwrap();
    let dimensions = MAX_DIMENSIONS.to_string();
    let summary = imported(&db, &input, "copy", &["--create", &dimensions]);
    assert_eq!(summary["rows_committed"], 1);
    let opened = Database::open(&db).unwrap();
    let record = opened.get(1).unwrap().unwrap();
    assert_eq!(record.vector.len(), MAX_DIMENSIONS);
    assert_eq!(record.payload, vec![b'\t'; MAX_PAYLOAD_BYTES]);
}

// Records that are not lines have no line limit. A quoted csv cell may hold newlines, so one
// record can be a single physical line of any length or span any number of short ones.
#[test]
fn csv_and_pgvector_records_longer_than_the_cap_are_imported() {
    let _serial = serial();
    for format in ["csv", "pgvector"] {
        let f = Fixture::new();
        let (db, input) = (f.path("db"), f.path("rows"));
        database(&db);
        let long_line = format!("100,\"[1,2]{}\"\n", " ".repeat(CAP + 1));
        let short_lines = format!(
            "101,\"[3,{}4]\"\n",
            format!("{}\n", " ".repeat(1023)).repeat(17_500)
        );
        assert!(long_line.len() > CAP && short_lines.len() > CAP);
        std::fs::write(&input, format!("id,vector\n{long_line}{short_lines}")).unwrap();
        let summary = imported(&db, &input, format, &[]);
        assert_eq!(summary["rows_committed"], 2, "{format}");
        let db = Database::open(&db).unwrap();
        assert_eq!(db.get(100).unwrap().unwrap().vector, vec![1.0, 2.0]);
        assert_eq!(db.get(101).unwrap().unwrap().vector, vec![3.0, 4.0]);
    }
}

#[test]
fn a_csv_payload_over_the_engine_limit_is_an_engine_rejection_not_a_line_rejection() {
    let _serial = serial();
    for format in ["csv", "pgvector"] {
        let f = Fixture::new();
        let (db, input) = (f.path("db"), f.path("rows"));
        database(&db);
        // One quoted payload cell of 17 MB that spans 17,500 short lines.
        let cell = format!("{}\n", "x".repeat(1023)).repeat(17_500);
        assert!(cell.len() > CAP);
        std::fs::write(
            &input,
            format!("id,vector,payload\n100,\"[1,2]\",\"{cell}\"\n"),
        )
        .unwrap();
        refused(
            &db,
            &input,
            format,
            "rejected row 1: dimensions/non-finite/payload/metadata limit\n\
             error: rejected_rows=1; no rows published\n",
        );
    }
}

// 65 flat vectors of the most dimensions, all 1.0: a 17 MB file without a newline byte.
fn faiss_file() -> Vec<u8> {
    let (dimensions, count) = (MAX_DIMENSIONS as i32, 65i64);
    let mut bytes = b"IxF2".to_vec();
    bytes.extend_from_slice(&dimensions.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&(1i64 << 20).to_le_bytes());
    bytes.extend_from_slice(&(1i64 << 20).to_le_bytes());
    bytes.push(1);
    bytes.extend_from_slice(&1i32.to_le_bytes());
    bytes.extend_from_slice(&(dimensions as u64 * count as u64).to_le_bytes());
    for _ in 0..dimensions as usize * count as usize {
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
    }
    bytes
}

#[test]
fn a_faiss_file_longer_than_the_cap_is_imported() {
    let _serial = serial();
    let f = Fixture::new();
    let (db, input) = (f.path("db"), f.path("flat.faiss"));
    let bytes = faiss_file();
    assert!(bytes.len() > CAP && !bytes.contains(&b'\n'));
    std::fs::write(&input, bytes).unwrap();
    let dimensions = MAX_DIMENSIONS.to_string();
    let summary = imported(
        &db,
        &input,
        "faiss",
        &["--create", &dimensions, "--chunk-size", "8"],
    );
    assert_eq!(summary["rows_committed"], 65);
    assert_eq!(Database::open(&db).unwrap().len(), 65);
}
