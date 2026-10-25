//! REG-008 surface parity (feature-agent-graph EPIC-001 ITEM-002/ITEM-003).
//!
//! Compares the current build's served `/openapi.json` bytes (modulo the
//! exact, reviewed additions in `APPROVED_OPENAPI_ADDITIONS`) and `vlite-mcp`
//! responses (read-only and writable) on a vector-only file against the golden
//! captures produced from baseline commit `347b01d` by
//! `python scripts\graph_baseline.py fixtures`, except for the package version
//! string, which release preparation bumps (see `MASKED_VERSION`). Requires
//! `server` and `mcp`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use vlite::Database;
use vlite::server::{Service, router};

fn fixture(name: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("graph_baseline")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .replace("\r\n", "\n")
}

/// Reviewed post-baseline OpenAPI additions, each matched byte-for-byte (LF)
/// at a line boundary; any other difference from the `347b01d` capture
/// fails REG-008. Empty because that capture already contains every change
/// published so far (the `x-durable-group-commit` block, the
/// `AutoMaintenancePolicy` retry fields and `MaintenanceStatus.rebuild.phase`).
/// Extend this list only for reviewed, intentional additions, or re-pin the
/// baseline with `python scripts\graph_baseline.py fixtures --rev <sha>`.
const APPROVED_OPENAPI_ADDITIONS: &[&str] = &[];

/// `/openapi.json` (`info.version`) and the MCP `initialize` result
/// (`serverInfo.version`) report the package version, which release
/// preparation bumps. That one value is masked on both sides and the served
/// one must equal `CARGO_PKG_VERSION`; every other byte must still match the
/// capture, so a version bump never needs a re-pin.
const MASKED_VERSION: &str = "<package-version>";
const OPENAPI_VERSION_ANCHOR: &str = "\"title\": \"VLite REST API\",\n    \"version\": \"";

fn is_release_version(text: &str) -> bool {
    let core = text.split_once('-').map_or(text, |(core, _)| core);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Masks `info.version`; returns the masked text and the version it held.
fn mask_openapi_version(text: &str) -> Result<(String, String), String> {
    if text.matches(OPENAPI_VERSION_ANCHOR).count() != 1 {
        return Err("OpenAPI info.version must appear exactly once after the title".to_string());
    }
    let start = text.find(OPENAPI_VERSION_ANCHOR).unwrap() + OPENAPI_VERSION_ANCHOR.len();
    let length = text[start..]
        .find('"')
        .ok_or_else(|| "unterminated OpenAPI info.version".to_string())?;
    let version = &text[start..start + length];
    if !is_release_version(version) {
        return Err(format!(
            "OpenAPI info.version {version:?} is not a release version"
        ));
    }
    Ok((
        format!(
            "{}{MASKED_VERSION}{}",
            &text[..start],
            &text[start + length..]
        ),
        version.to_string(),
    ))
}

/// Masks `serverInfo.version` of an `initialize` result and returns it; other
/// responses and non-release values are left untouched (`None`).
fn mask_mcp_version(response: &mut Value) -> Option<String> {
    let slot = response.pointer_mut("/result/serverInfo/version")?;
    let version = slot.as_str()?.to_string();
    if !is_release_version(&version) {
        return None;
    }
    *slot = Value::String(MASKED_VERSION.to_string());
    Some(version)
}

/// Served OpenAPI must equal the baseline once each approved addition (if
/// present, at most once, on whole lines) is removed and the package version
/// is masked. Returns the served package version, or the first differing line
/// on failure.
fn openapi_matches_baseline(
    served: &str,
    golden: &str,
    additions: &[&str],
) -> Result<String, String> {
    let mut remainder = served.to_string();
    for addition in additions {
        match remainder.matches(addition).count() {
            0 => {}
            1 => {
                let start = remainder.find(addition).unwrap();
                if start == 0 || !remainder[..start].ends_with('\n') {
                    return Err("approved OpenAPI addition is not on whole lines".to_string());
                }
                remainder.replace_range(start..start + addition.len(), "");
            }
            count => return Err(format!("approved OpenAPI addition appears {count} times")),
        }
    }
    let (remainder, version) = mask_openapi_version(&remainder)?;
    let (golden, _) = mask_openapi_version(golden)?;
    if remainder == golden {
        return Ok(version);
    }
    let line = remainder
        .lines()
        .zip(golden.lines())
        .position(|(a, b)| a != b)
        .map_or_else(|| "length".to_string(), |index| (index + 1).to_string());
    Err(format!(
        "served /openapi.json differs from the baseline capture beyond the approved additions \
         (first difference at line {line} after removing them)"
    ))
}

/// A test-local stand-in for a reviewed addition; it exercises the allowlist
/// mechanism and is never part of `APPROVED_OPENAPI_ADDITIONS`.
const SYNTHETIC_ADDITION: &str = concat!(
    "  \"x-synthetic-addition\": {\n",
    "    \"enabled_by_default\": false\n",
    "  },\n",
);

fn golden_with_synthetic_addition() -> String {
    let golden = fixture("openapi.json");
    let anchor = "  \"servers\": [{\"url\": \"http://127.0.0.1:8080\"}],\n";
    assert_eq!(golden.matches(anchor).count(), 1);
    golden.replacen(anchor, &format!("{anchor}{SYNTHETIC_ADDITION}"), 1)
}

#[test]
fn reg008_openapi_allowlist_accepts_baseline_and_approved_additions() {
    let golden = fixture("openapi.json");
    openapi_matches_baseline(&golden, &golden, &[]).unwrap();
    openapi_matches_baseline(&golden, &golden, &[SYNTHETIC_ADDITION]).unwrap();
    let extended = golden_with_synthetic_addition();
    serde_json::from_str::<Value>(&extended).expect("synthetic addition keeps valid JSON");
    openapi_matches_baseline(&extended, &golden, &[SYNTHETIC_ADDITION]).unwrap();
    assert!(
        openapi_matches_baseline(&extended, &golden, APPROVED_OPENAPI_ADDITIONS).is_err(),
        "an addition that is not approved must fail REG-008"
    );
}

#[test]
fn reg008_openapi_allowlist_rejects_unapproved_drift() {
    let golden = fixture("openapi.json");
    let extended = golden_with_synthetic_addition();
    let addition = SYNTHETIC_ADDITION;
    // A schema change elsewhere, alongside the approved addition.
    let first_integer = extended.find("\"type\":\"integer\"").unwrap();
    let mut schema_drift = extended.clone();
    schema_drift.replace_range(
        first_integer..first_integer + "\"type\":\"integer\"".len(),
        "\"type\":\"string\"",
    );
    // An edit inside the approved block itself.
    let edited_block = extended.replacen(
        "\"enabled_by_default\": false",
        "\"enabled_by_default\": true",
        1,
    );
    // A different, unapproved additive top-level extension.
    let other_addition = extended.replacen(
        addition,
        &format!("{addition}  \"x-unapproved\": {{}},\n"),
        1,
    );
    // The approved block duplicated.
    let duplicated = extended.replacen(addition, &format!("{addition}{addition}"), 1);
    // A schema change without the approved addition.
    let bare_drift = golden.replacen("\"type\":\"integer\"", "\"type\":\"number\"", 1);
    for (name, served) in [
        ("schema drift", schema_drift),
        ("edited approved block", edited_block),
        ("unapproved addition", other_addition),
        ("duplicated approved block", duplicated),
        ("drift without addition", bare_drift),
    ] {
        assert_ne!(served, extended, "{name}: mutation must apply");
        assert!(
            openapi_matches_baseline(&served, &golden, &[addition]).is_err(),
            "{name}: must fail REG-008"
        );
    }
}

#[test]
fn reg008_package_version_is_masked_but_nothing_else() {
    let golden = fixture("openapi.json");
    let (_, baseline_version) = mask_openapi_version(&golden).unwrap();
    let anchor = format!("{OPENAPI_VERSION_ANCHOR}{baseline_version}\"");
    let with_version = |version: &str| {
        golden.replacen(&anchor, &format!("{OPENAPI_VERSION_ANCHOR}{version}\""), 1)
    };
    let bumped = with_version("9.8.7");
    assert_ne!(bumped, golden, "version bump must apply");
    assert_eq!(
        openapi_matches_baseline(&bumped, &golden, &[]),
        Ok("9.8.7".to_string())
    );
    assert_eq!(
        openapi_matches_baseline(&with_version("0.5.0-rc.1"), &golden, &[]),
        Ok("0.5.0-rc.1".to_string())
    );
    for (name, served) in [
        (
            "description drift",
            bumped.replacen(
                "\"description\": \"Experimental API",
                "\"description\": \"Stable API",
                1,
            ),
        ),
        (
            "schema drift",
            bumped.replacen("\"type\":\"integer\"", "\"type\":\"number\"", 1),
        ),
        ("version anchor duplicated", format!("{bumped}{bumped}")),
        (
            "version anchor missing",
            bumped.replacen("\"title\": \"VLite REST API\",\n", "", 1),
        ),
        ("non-release version", with_version("latest")),
        ("literal mask text", with_version(MASKED_VERSION)),
    ] {
        assert_ne!(served, bumped, "{name}: mutation must apply");
        assert!(
            openapi_matches_baseline(&served, &golden, &[]).is_err(),
            "{name}: must fail REG-008"
        );
    }
}

#[test]
fn reg008_mcp_version_is_masked_but_nothing_else() {
    let transcript: Vec<Value> = serde_json::from_str(&fixture("mcp_read_only.json")).unwrap();
    assert_eq!(
        transcript[0]["id"], 1,
        "response 1 is the initialize result"
    );
    let mut golden = transcript[0].clone();
    let mut bumped = golden.clone();
    bumped["result"]["serverInfo"]["version"] = Value::String("9.8.7".to_string());
    assert_eq!(mask_mcp_version(&mut bumped).as_deref(), Some("9.8.7"));
    assert!(mask_mcp_version(&mut golden).is_some());
    assert_eq!(bumped, golden, "only the version differed");
    let mut not_a_version = transcript[0].clone();
    not_a_version["result"]["serverInfo"]["version"] = Value::String("latest".to_string());
    assert_eq!(mask_mcp_version(&mut not_a_version), None);
    assert_eq!(not_a_version["result"]["serverInfo"]["version"], "latest");
    let mut tools_list = transcript[1].clone();
    assert_eq!(mask_mcp_version(&mut tools_list), None);
    assert_eq!(tools_list, transcript[1]);
}

#[tokio::test]
async fn reg008_served_openapi_matches_baseline_except_approved_additions() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::create(directory.path().join("openapi.vlite"), 2).unwrap();
    let response = router(Service::new(db))
        .oneshot(
            Request::builder()
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let served = String::from_utf8(bytes.to_vec())
        .unwrap()
        .replace("\r\n", "\n");
    let version = openapi_matches_baseline(
        &served,
        &fixture("openapi.json"),
        APPROVED_OPENAPI_ADDITIONS,
    )
    .unwrap_or_else(|message| panic!("{message}"));
    assert_eq!(version, env!("CARGO_PKG_VERSION"), "served OpenAPI version");
}

fn mcp_transcript(mode: &str) -> Vec<Value> {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp.vlite");
    let mut db = Database::create(&path, 2).unwrap();
    db.put(1, &[0.0, 0.0], b"origin").unwrap();
    db.put(2, &[1.0, 1.0], b"near").unwrap();
    db.put(3, &[2.0, 2.0], b"far").unwrap();
    db.close().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_vlite-mcp"))
        .args(["--db", path.to_str().unwrap(), &format!("--{mode}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(fixture("mcp_requests.jsonl").as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn reg008_mcp_tools_list_and_outputs_match_baseline() {
    for (mode, golden) in [
        ("read-only", "mcp_read_only.json"),
        ("writable", "mcp_writable.json"),
    ] {
        let expected: Vec<Value> = serde_json::from_str(&fixture(golden)).unwrap();
        let actual = mcp_transcript(mode);
        assert_eq!(actual.len(), expected.len(), "{mode}: response count");
        for (actual, expected) in actual.iter().zip(&expected) {
            let (mut actual, mut expected) = (actual.clone(), expected.clone());
            let served_version = mask_mcp_version(&mut actual);
            let golden_version = mask_mcp_version(&mut expected);
            assert_eq!(
                served_version.is_some(),
                golden_version.is_some(),
                "{mode}: MCP response id {} serverInfo.version presence",
                expected["id"]
            );
            if let Some(version) = served_version {
                assert_eq!(
                    version,
                    env!("CARGO_PKG_VERSION"),
                    "{mode}: MCP server version"
                );
            }
            assert_eq!(
                actual, expected,
                "{mode}: MCP response id {} differs from baseline",
                expected["id"]
            );
        }
    }
}
