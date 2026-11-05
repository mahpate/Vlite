//! The library accepts an already due expiry (the key-value store relies on
//! it), so the server refuses one on the record routes itself; see
//! tests/server_record_expiry.rs. These pins keep the library contract, and the
//! absence of an expiry argument on the MCP upsert tool, from changing.

use vlite::kv::{KvPut, first_slot};
use vlite::{CreateOptions, Database, Expected, Metadata, SearchHit};

fn ids(hits: Vec<SearchHit>) -> Vec<u64> {
    hits.into_iter().map(|hit| hit.id).collect()
}

#[test]
fn put_with_expiry_accepts_a_due_expiry_and_reads_it_as_absent() {
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    db.put_with_expiry(1, &[1.0, 0.0], b"due", &Metadata::new(), Some(0))
        .unwrap();
    db.put_with_expiry(2, &[0.0, 1.0], b"timed", &Metadata::new(), Some(1_000))
        .unwrap();
    db.put_with_expiry(3, &[1.0, 1.0], b"open", &Metadata::new(), None)
        .unwrap();

    // The record was written with its deadline, and no clock can read it.
    assert_eq!(db.expires_at(1).unwrap(), Some(0));
    assert!(db.get(1).unwrap().is_none());
    assert!(db.get_at(1, 0).unwrap().is_none());
    assert_eq!(db.get_at(2, 999).unwrap().unwrap().payload, b"timed");
    assert!(db.get_at(2, 1_000).unwrap().is_none());
    assert_eq!(db.get(3).unwrap().unwrap().payload, b"open");

    let query = [1.0, 0.0];
    assert_eq!(ids(db.search(&query, 10).unwrap()), [3]);
    assert_eq!(ids(db.search_at(&query, 10, 999).unwrap()), [3, 2]);
    assert_eq!(ids(db.search_at(&query, 10, 1_000).unwrap()), [3]);
}

#[test]
fn kv_put_at_accepts_a_due_deadline_and_get_at_reads_it_as_absent() {
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    let mut kv = db.kv_store("kv").unwrap();
    let due = KvPut {
        expected: Expected::Any,
        expires_at_ms: Some(0),
    };
    let version = kv.put_at("n", "k", b"v", due, 5_000).unwrap();
    assert!(version > 0);
    for now in [0, 5_000, u64::MAX] {
        assert_eq!(kv.view().get_at("n", "k", now).unwrap(), None, "now={now}");
    }
    // It was stored, in the first slot of its window, at that version.
    let stored = db.collection("kv").unwrap().revision(first_slot("n", "k"));
    assert_eq!(stored.unwrap(), Some(version));
}

#[cfg(feature = "mcp")]
mod mcp {
    use serde_json::{Value, json};
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    use vlite::Database;

    fn run(path: &std::path::Path, requests: &[Value]) -> Vec<Value> {
        let mut child = Command::new(env!("CARGO_BIN_EXE_vlite-mcp"))
            .args(["--db", path.to_str().unwrap(), "--writable"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            let mut stdin = child.stdin.take().unwrap();
            for request in requests {
                writeln!(stdin, "{request}").unwrap();
            }
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn call(id: usize, tool: &str, arguments: Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":tool,"arguments":arguments}})
    }

    #[test]
    fn vlite_upsert_has_no_expiry_argument() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("upsert.vlite");
        Database::create(&path, 2).unwrap().close().unwrap();
        let mut requests = vec![json!({"jsonrpc":"2.0","id":0,"method":"tools/list","params":{}})];
        let refused = [
            json!({"expires_at_ms":"0"}),
            json!({"expires_at_ms":0}),
            json!({"ttl_ms":0}),
        ];
        for (index, extra) in refused.iter().enumerate() {
            let mut arguments = json!({"id":"9","vector":[1,2]});
            arguments
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            requests.push(call(index + 1, "vlite_upsert", arguments));
        }
        let responses = run(&path, &requests);

        let tools = responses[0]["result"]["tools"].as_array().unwrap();
        let upsert = tools
            .iter()
            .find(|tool| tool["name"] == "vlite_upsert")
            .unwrap();
        let schema = &upsert["inputSchema"];
        assert_eq!(schema["additionalProperties"], false);
        let mut properties: Vec<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        properties.sort_unstable();
        assert_eq!(
            properties,
            [
                "collection",
                "id",
                "metadata",
                "payload",
                "payload_base64",
                "vector"
            ]
        );

        for (index, extra) in refused.iter().enumerate() {
            let result = &responses
                .iter()
                .find(|response| response["id"] == index + 1)
                .unwrap()["result"];
            let field = extra.as_object().unwrap().keys().next().unwrap();
            let message = format!("unexpected argument: {field}");
            assert_eq!(
                *result,
                json!({"isError":true,"content":[{"type":"text","text":message}],
                    "structuredContent":{"error":message}}),
                "{extra}"
            );
        }
        // Nothing was written.
        assert!(Database::open(&path).unwrap().get(9).unwrap().is_none());
    }
}
