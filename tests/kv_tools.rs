#![cfg(all(feature = "mcp", feature = "tools"))]

use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};
use vlite::{CollectionSettings, CollectionSpec, Database};

fn goldens() -> Value {
    serde_json::from_str(include_str!("fixtures/kv/tools/cases.json")).unwrap()
}

fn fixture(path: &Path) {
    let mut db = Database::create(path, 1).unwrap();
    db.create_collection(
        "vectors",
        CollectionSpec {
            dimensions: 2,
            settings: CollectionSettings::default(),
        },
    )
    .unwrap();
    db.close().unwrap();
}

fn process(path: &Path, mcp: bool, readonly: bool, lines: &[String]) -> Output {
    let binary = if mcp {
        env!("CARGO_BIN_EXE_vlite-mcp")
    } else {
        env!("CARGO_BIN_EXE_vlite-shell")
    };
    let mut command = Command::new(binary);
    command.args(["--db", path.to_str().unwrap()]);
    if readonly {
        command.arg("--read-only");
    } else if mcp {
        command.arg("--writable");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for line in lines {
            writeln!(stdin, "{line}").unwrap();
        }
    }
    child.wait_with_output().unwrap()
}

fn line(case: &Value, mcp: bool, id: usize) -> String {
    if mcp {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
            "name":format!("vlite_{}",case["command"].as_str().unwrap().replace('-', "_")),
            "arguments":case["request"]}})
        .to_string()
    } else {
        format!("{} {}", case["command"].as_str().unwrap(), case["request"])
    }
}

fn values(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn version(value: &mut Value) -> u64 {
    let text = value.as_str().expect("version must be a decimal string");
    assert!(!text.starts_with('0'));
    let version = text.parse::<u64>().unwrap();
    *value = Value::Null;
    version
}

#[test]
fn contract_goldens_mcp_and_shell() {
    let cases = goldens();
    for mcp in [true, false] {
        let temp = tempfile::tempdir_in(".").unwrap();
        let path = temp.path().join("kv.vlite");
        fixture(&path);
        let lines: Vec<_> = cases["success"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(id, case)| line(case, mcp, id))
            .collect();
        let output = process(&path, mcp, false, &lines);
        assert!(output.status.success(), "{output:?}");
        let results = values(&output);
        assert_eq!(results.len(), lines.len());
        let mut versions = Vec::new();
        for (result, case) in results
            .into_iter()
            .zip(cases["success"].as_array().unwrap())
        {
            let mut actual = if mcp {
                assert_eq!(result["result"]["isError"], false, "{result}");
                let result = &result["result"];
                assert_eq!(result["content"].as_array().unwrap().len(), 1);
                assert_eq!(
                    serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap())
                        .unwrap(),
                    result["structuredContent"]
                );
                result["structuredContent"].clone()
            } else {
                result
            };
            if case["version"] == true {
                versions.push(version(&mut actual["version"]));
                actual.as_object_mut().unwrap().remove("version");
            }
            if case["item_versions"] == true {
                for item in actual["items"].as_array_mut().unwrap() {
                    version(&mut item["version"]);
                    item.as_object_mut().unwrap().remove("version");
                }
            }
            assert_eq!(actual, case["result"], "{case}");
        }
        assert_eq!(versions[0], versions[1]);
        assert!(versions[2] > versions[0]);
        assert_eq!(versions[2], versions[3]);
        assert!(versions[4] > versions[3]);
        for case in cases["errors"].as_array().unwrap() {
            check_error(&path, mcp, false, case);
        }
        assert!(
            Database::open(&path)
                .unwrap()
                .collection("missing")
                .is_err()
        );
        let before = std::fs::read(&path).unwrap();
        for case in cases["readonly"].as_array().unwrap() {
            check_error(&path, mcp, true, case);
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

fn check_error(path: &Path, mcp: bool, readonly: bool, case: &Value) {
    let output = process(path, mcp, readonly, &[line(case, mcp, 1)]);
    let code = case["code"].as_str().unwrap();
    if mcp {
        assert!(output.status.success(), "{output:?}");
        let responses = values(&output);
        let result = &responses[0]["result"];
        assert_eq!(result["isError"], true, "{case}: {result}");
        let error = result["structuredContent"]["error"].as_str().unwrap();
        assert!(error.starts_with(&format!("{code}: ")), "{case}: {error}");
        assert_eq!(result["content"][0]["text"], error);
    } else {
        assert_eq!(output.status.code(), Some(1), "{case}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).starts_with(&format!("error: {code}: ")),
            "{case}: {output:?}"
        );
    }
}

#[test]
fn bounds_and_tool_listing() {
    let cases = goldens();
    let temp = tempfile::tempdir_in(".").unwrap();
    let path = temp.path().join("bounds.vlite");
    fixture(&path);
    for bound in cases["bounds"].as_array().unwrap() {
        let field = bound["field"].as_str().unwrap();
        let mut request = json!({"collection":"default","namespace":"n"});
        request[field] = json!("x".repeat(bound["length"].as_u64().unwrap() as usize));
        if matches!(field, "namespace" | "key") {
            if field == "namespace" {
                request["key"] = json!("k");
            }
            request["value_base64"] = json!("");
        }
        let case = json!({"command":if matches!(field,"prefix"|"after") {"kv-list"} else {"kv-put"},
            "request":request,"code":"invalid_input"});
        if bound["valid"] == false {
            check_error(&path, true, false, &case);
            check_error(&path, false, false, &case);
        } else {
            let output = process(&path, true, false, &[line(&case, true, 1)]);
            assert_eq!(
                values(&output)[0]["result"]["isError"],
                false,
                "{bound}: {output:?}"
            );
        }
    }
    for readonly in [true, false] {
        let output = process(
            &path,
            true,
            readonly,
            &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string()],
        );
        let responses = values(&output);
        let tools = responses[0]["result"]["tools"].as_array().unwrap();
        for name in [
            "vlite_kv_get",
            "vlite_kv_list",
            "vlite_kv_put",
            "vlite_kv_delete",
        ] {
            let tool = tools.iter().find(|tool| tool["name"] == name);
            let write = matches!(name, "vlite_kv_put" | "vlite_kv_delete");
            assert_eq!(tool.is_some(), !readonly || !write, "{name}");
            if let Some(tool) = tool {
                assert_eq!(tool["inputSchema"]["additionalProperties"], false);
                assert!(
                    tool["inputSchema"]["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("collection"))
                );
            }
        }
    }
}

#[test]
fn shell_stops_at_first_error() {
    let temp = tempfile::tempdir_in(".").unwrap();
    let path = temp.path().join("stop.vlite");
    fixture(&path);
    let cases = goldens();
    let output = process(
        &path,
        false,
        false,
        &[
            line(&cases["errors"][0], false, 1),
            line(&cases["success"][0], false, 2),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("error: kv_not_found: "));
    assert!(
        Database::open(&path)
            .unwrap()
            .kv_view("default")
            .unwrap()
            .get("n", "a")
            .unwrap()
            .is_none()
    );
}

#[test]
fn legacy_ttl_and_slot_exhaustion_errors() {
    let temp = tempfile::tempdir_in(".").unwrap();
    let path = temp.path().join("v4.vlite");
    Database::create_with_format(&path, 1, vlite::FormatVersion::V4)
        .unwrap()
        .close()
        .unwrap();
    for mcp in [true, false] {
        check_error(&path, mcp, false, &goldens()["legacy"][0]);
    }
    let path = temp.path().join("slots.vlite");
    let mut db = Database::create(&path, 1).unwrap();
    let first = vlite::kv::first_slot("n", "k");
    for offset in 0..16 {
        db.put(first.wrapping_add(offset), &[0.0], b"occupied")
            .unwrap();
    }
    db.close().unwrap();
    for mcp in [true, false] {
        check_error(
            &path,
            mcp,
            false,
            &json!({"command":"kv-put",
            "request":{"collection":"default","namespace":"n","key":"k","value_base64":""},
            "code":"kv_slots_exhausted"}),
        );
    }
}

#[test]
fn duplicate_fields_and_malformed_mcp_params() {
    let temp = tempfile::tempdir_in(".").unwrap();
    let path = temp.path().join("duplicate.vlite");
    fixture(&path);
    for mcp in [true, false] {
        for request in [
            r#"{"collection":"default","namespace":"n","key":"a","key":"b"}"#,
            r#"{"collection":"default","namespace":"n","key":"a","expected":{"revision":"0","revision":"1"}}"#,
        ] {
            let line = if mcp {
                format!(
                    r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"vlite_kv_delete","arguments":{request}}}}}"#
                )
            } else {
                format!("kv-delete {request}")
            };
            let output = process(&path, mcp, false, &[line]);
            if mcp {
                let result = &values(&output)[0]["result"];
                assert_eq!(result["isError"], true);
                assert!(
                    result["structuredContent"]["error"]
                        .as_str()
                        .unwrap()
                        .starts_with("invalid_request: ")
                );
            } else {
                assert_eq!(output.status.code(), Some(1));
                assert!(
                    String::from_utf8_lossy(&output.stderr).starts_with("error: invalid_request: ")
                );
            }
        }
    }
    for params in [
        json!([]),
        json!({"arguments":{}}),
        json!({"name":"vlite_kv_get","arguments":[]}),
    ] {
        let output = process(
            &path,
            true,
            false,
            &[json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":params}).to_string()],
        );
        assert_eq!(values(&output)[0]["error"]["code"], -32602);
    }
    let output = process(&path, true, false, &[
        r#"{"jsonrpc":"2.0","id":null,"method":"tools/call","params":{"name":"vlite_kv_get","arguments":{"collection":"default","namespace":"n","key":"a","key":"b"}}}"#.into(),
    ]);
    assert_eq!(values(&output)[0]["error"]["code"], -32600);
}

#[test]
fn pagination_order_prefix_exclusive_after_and_default_limit() {
    let temp = tempfile::tempdir_in(".").unwrap();
    let path = temp.path().join("pages.vlite");
    let mut db = Database::create(&path, 1).unwrap();
    for key in ["p/a", "p/b", "p/\u{e9}", "q/a"] {
        db.kv_store("default")
            .unwrap()
            .put("n", key, b"", vlite::kv::KvPut::default())
            .unwrap();
    }
    db.kv_store("default")
        .unwrap()
        .put(
            "n",
            "p/due",
            b"",
            vlite::kv::KvPut {
                expires_at_ms: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
    for index in 0..101 {
        db.kv_store("default")
            .unwrap()
            .put("many", &format!("{index:03}"), b"", Default::default())
            .unwrap();
    }
    db.close().unwrap();
    for mcp in [true, false] {
        let cases = [
            json!({"command":"kv-list","request":{"collection":"default","namespace":"n","prefix":"p/","limit":1}}),
            json!({"command":"kv-list","request":{"collection":"default","namespace":"n","prefix":"p/","after":"p/a","limit":1}}),
            json!({"command":"kv-list","request":{"collection":"default","namespace":"n","prefix":"p/","after":"p/b","limit":10000}}),
            json!({"command":"kv-list","request":{"collection":"default","namespace":"many"}}),
        ];
        let output = process(
            &path,
            mcp,
            false,
            &cases
                .iter()
                .enumerate()
                .map(|(i, c)| line(c, mcp, i))
                .collect::<Vec<_>>(),
        );
        assert!(output.status.success(), "{output:?}");
        let pages: Vec<_> = values(&output)
            .into_iter()
            .map(|v| {
                if mcp {
                    v["result"]["structuredContent"].clone()
                } else {
                    v
                }
            })
            .collect();
        assert_eq!(pages[0]["items"][0]["key"], "p/a");
        assert_eq!(pages[0]["next_after"], "p/a");
        assert_eq!(pages[1]["items"][0]["key"], "p/b");
        assert_eq!(pages[1]["next_after"], "p/b");
        assert_eq!(pages[2]["items"].as_array().unwrap().len(), 1);
        assert_eq!(pages[2]["items"][0]["key"], "p/\u{e9}");
        assert_eq!(pages[2]["next_after"], Value::Null);
        assert_eq!(pages[3]["items"].as_array().unwrap().len(), 100);
        assert_eq!(pages[3]["next_after"], "099");
    }
}

#[test]
fn cas_revision_success_stale_and_due_absent() {
    let temp = tempfile::tempdir_in(".").unwrap();
    let path = temp.path().join("cas.vlite");
    fixture(&path);
    for mcp in [true, false] {
        let mut db = Database::open(&path).unwrap();
        let initial = db
            .kv_store("default")
            .unwrap()
            .put("n", "cas", b"old", Default::default())
            .unwrap();
        db.close().unwrap();
        let case = json!({"command":"kv-put","request":{"collection":"default","namespace":"n","key":"cas","value_base64":"bmV3","expected":{"revision":initial.to_string()}}});
        let output = process(&path, mcp, false, &[line(&case, mcp, 1)]);
        assert!(output.status.success(), "{output:?}");
        let responses = values(&output);
        let result = if mcp {
            &responses[0]["result"]["structuredContent"]
        } else {
            &responses[0]
        };
        assert!(result["version"].as_str().unwrap().parse::<u64>().unwrap() > initial);
        let mut stale = case.clone();
        stale["code"] = json!("precondition_failed");
        check_error(&path, mcp, false, &stale);
        let mut db = Database::open(&path).unwrap();
        let due_version = db
            .kv_store("default")
            .unwrap()
            .put(
                "n",
                "due",
                b"",
                vlite::kv::KvPut {
                    expires_at_ms: Some(0),
                    ..Default::default()
                },
            )
            .unwrap();
        db.close().unwrap();
        check_error(
            &path,
            mcp,
            false,
            &json!({"command":"kv-delete","request":{"collection":"default","namespace":"n","key":"due","expected":{"revision":due_version.to_string()}},"code":"precondition_failed"}),
        );
        let output = process(
            &path,
            mcp,
            false,
            &[line(
                &json!({"command":"kv-put","request":{"collection":"default","namespace":"n","key":"due","value_base64":"","expected":"absent"}}),
                mcp,
                1,
            )],
        );
        assert!(output.status.success(), "{output:?}");
        assert!(
            Database::open(&path)
                .unwrap()
                .kv_view("default")
                .unwrap()
                .get("n", "due")
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn value_bound_exact_and_over() {
    let temp = tempfile::tempdir_in(".").unwrap();
    let path = temp.path().join("value.vlite");
    fixture(&path);
    // 1,047,288 zero bytes are 349,096 AAAA groups; append 2 or 3 bytes.
    for mcp in [true, false] {
        let mut case = json!({"command":"kv-put","request":{"collection":"default","namespace":"n","key":"large","value_base64":format!("{}AAA=","AAAA".repeat(349_096))}});
        let output = process(&path, mcp, false, &[line(&case, mcp, 1)]);
        assert!(output.status.success(), "{output:?}");
        if mcp {
            assert_eq!(values(&output)[0]["result"]["isError"], false);
        }

        assert_eq!(
            Database::open(&path)
                .unwrap()
                .kv_view("default")
                .unwrap()
                .get("n", "large")
                .unwrap()
                .unwrap()
                .value
                .len(),
            1_047_290
        );
        case["request"]["value_base64"] = json!("AAAA".repeat(349_097));
        case["code"] = json!("invalid_input");
        check_error(&path, mcp, false, &case);
    }
}

#[test]
fn list_limit_10000_peeks_for_continuation() {
    let temp = tempfile::tempdir_in(".").unwrap();
    let path = temp.path().join("limit.vlite");
    let mut db = Database::create(&path, 1).unwrap();
    for index in 0..10_001 {
        db.kv_store("default")
            .unwrap()
            .put("n", &format!("{index:05}"), b"", Default::default())
            .unwrap();
    }
    db.close().unwrap();
    for mcp in [true, false] {
        let cases = [
            json!({"command":"kv-list","request":{"collection":"default","namespace":"n","limit":10000}}),
            json!({"command":"kv-list","request":{"collection":"default","namespace":"n","limit":10000,"after":"09999"}}),
        ];
        let output = process(
            &path,
            mcp,
            false,
            &cases
                .iter()
                .enumerate()
                .map(|(i, c)| line(c, mcp, i))
                .collect::<Vec<_>>(),
        );
        assert!(output.status.success(), "{output:?}");
        let pages: Vec<_> = values(&output)
            .into_iter()
            .map(|v| {
                if mcp {
                    v["result"]["structuredContent"].clone()
                } else {
                    v
                }
            })
            .collect();
        assert_eq!(pages[0]["items"].as_array().unwrap().len(), 10_000);
        assert_eq!(pages[0]["items"][0]["key"], "00000");
        assert_eq!(pages[0]["items"][9999]["key"], "09999");
        assert_eq!(pages[0]["next_after"], "09999");
        assert_eq!(pages[1]["items"].as_array().unwrap().len(), 1);
        assert_eq!(pages[1]["items"][0]["key"], "10000");
        assert_eq!(pages[1]["next_after"], Value::Null);
    }
}

#[test]
fn shell_script_uses_explicit_collection_and_stops() {
    let temp = tempfile::tempdir_in(".").unwrap();
    let path = temp.path().join("script.vlite");
    fixture(&path);
    let script = temp.path().join("commands.txt");
    let cases = goldens();
    std::fs::write(
        &script,
        format!(
            "use vectors\n{}\n{}\n{}\n",
            line(&cases["success"][0], false, 1),
            line(&cases["errors"][0], false, 2),
            line(&cases["success"][3], false, 3)
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_vlite-shell"))
        .args([
            "--db",
            path.to_str().unwrap(),
            "--script",
            script.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(values(&output).len(), 1);
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("error: kv_not_found: "));
    assert_eq!(
        Database::open(&path)
            .unwrap()
            .kv_view("default")
            .unwrap()
            .get("n", "a")
            .unwrap()
            .unwrap()
            .value,
        [0, 1, 2]
    );
}
