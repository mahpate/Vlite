#![cfg(feature = "server")]
//! Cloud backup admin routes through the fully configured router: the
//! documented operation set is exactly the nine cloud backup operations, the
//! router answers each one, and only the super-administrator reaches a
//! handler. Read-only, globally scoped and tenant keys are refused with 403
//! before any cloud work, on every operation.

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use tower::ServiceExt;
use vlite::{
    CollectionSpec, CreateOptions, Database,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "fake_cloud_routes_admin_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const READ_ONLY: &str = "fake_cloud_routes_readonly_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const TENANT: &str = "0123456789abcdef0123456789abcdef";
const SET: &str = "20250101T000000Z-0123456789abcdef";
const PIN: &str = "pin-0123456789abcdef";
/// A disabled policy: configuring it performs no cloud request.
const POLICY: &str = r#"{"target":{"account":"vlitebackup","container":"vlite-data","coordination_container":"vlite-meta","prefix":"vlite/routes/"},"enabled":false}"#;
const OPS: &[&str] = &[
    "read",
    "write",
    "search",
    "search_build",
    "collections",
    "index",
    "import",
    "diagnostics",
    "backup",
    "graph_read",
    "graph_write",
];

/// The nine documented operations with a request body that would pass the
/// handler's own parsing, so a refusal can only come from authorization.
const OPERATIONS: &[(&str, &str, &str)] = &[
    ("GET", "/v1/admin/cloud-backup", ""),
    ("PUT", "/v1/admin/cloud-backup", POLICY),
    ("POST", "/v1/admin/cloud-backup/run", ""),
    ("POST", "/v1/admin/cloud-backup/collect", ""),
    ("GET", "/v1/admin/cloud-backup/sets", ""),
    ("POST", "/v1/admin/cloud-backup/sets/{id}/drill", ""),
    ("POST", "/v1/admin/cloud-backup/sets/{id}/restore", ""),
    (
        "POST",
        "/v1/admin/cloud-backup/sets/{id}/pins",
        r#"{"reason":"hold"}"#,
    ),
    ("DELETE", "/v1/admin/cloud-backup/sets/{id}/pins/{pin}", ""),
];

fn concrete(path: &str) -> String {
    path.replace("{id}", SET).replace("{pin}", PIN)
}

struct Fixture {
    app: Router,
    directory: tempfile::TempDir,
    /// (actor, bearer secret) for every non-administrator principal.
    refused: Vec<(&'static str, String)>,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Database::create_in_memory(2, CreateOptions::default()).unwrap();
    for name in ["ordinary".to_owned(), format!("t{TENANT}__ordinary")] {
        db.create_collection(
            &name,
            CollectionSpec {
                dimensions: 2,
                settings: Default::default(),
            },
        )
        .unwrap();
    }
    let mut refused = vec![("read_only", READ_ONLY.to_owned())];
    let credentials: Vec<_> = [
        ("global_all", "abcdefghij33", false),
        ("tenant_all", "abcdefghij34", true),
    ]
    .into_iter()
    .map(|(actor, id, tenant)| {
        let secret = "B".repeat(43);
        refused.push((actor, format!("vlk_{id}_{secret}")));
        json!({"id":id,"name":actor,"all":true,"collections":[],"graphs":["*"],
            "operations":OPS,"lineage":format!("{:016x}",db.lineage().unwrap()),
            "secret_sha256":format!("{:x}",Sha256::digest(secret.as_bytes())),
            "created_unix_ms":0,"expires_unix_ms":9_999_999_999_999u64,
            "revoked_unix_ms":null,"rotated_from":null,
            "tenant_id":tenant.then_some(TENANT)})
    })
    .collect();
    std::fs::write(
        directory.path().join("credentials.json"),
        json!({"schema":1,"credentials":credentials}).to_string(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("tenants.json"),
        json!([{"id":TENANT,"name":"fake tenant","max_vectors":null}]).to_string(),
    )
    .unwrap();
    let app = configured_admin_router(
        Service::new(db),
        ServerOptions {
            loopback_only: true,
            admin_token: Some(BearerToken::new(ADMIN).unwrap()),
            read_only_token: Some(BearerToken::new(READ_ONLY).unwrap()),
            admin_directory: Some(directory.path().into()),
            ..Default::default()
        },
    )
    .unwrap();
    Fixture {
        app,
        directory,
        refused,
    }
}

async fn request(app: &Router, token: &str, method: &str, path: &str, body: &str) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("sec-fetch-site", "same-origin")
        .header("x-vlite-request", "1")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_owned()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn cloud_backup_routes_are_super_admin_only_through_the_configured_router() {
    let spec: Value = serde_json::from_str(include_str!("../src/openapi.json")).unwrap();
    let documented: BTreeSet<(String, String)> = spec["paths"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(path, _)| path.starts_with("/v1/admin/cloud-backup"))
        .flat_map(|(path, item)| {
            item.as_object()
                .unwrap()
                .keys()
                .filter(|key| *key != "parameters")
                .map(move |method| (method.to_ascii_uppercase(), path.clone()))
        })
        .collect();
    let expected: BTreeSet<(String, String)> = OPERATIONS
        .iter()
        .map(|(method, path, _)| ((*method).to_owned(), (*path).to_owned()))
        .collect();
    assert_eq!(documented, expected, "the documented cloud backup routes");

    let fixture = fixture();
    let policy = fixture.directory.path().join("cloud-backup");
    for (method, path, body) in OPERATIONS {
        let uri = concrete(path);
        for (actor, token) in &fixture.refused {
            let (status, reply) = request(&fixture.app, token, method, &uri, body).await;
            assert_eq!(status, 403, "{actor} {method} {path}: {reply}");
            let code = reply["error"]["code"].as_str().unwrap_or_default();
            assert!(
                matches!(code, "read_only" | "forbidden_scope"),
                "{actor} {method} {path}: {reply}"
            );
        }
        assert!(
            !policy.join("policy.json").exists(),
            "a refused {method} {path} must not change the cloud policy"
        );
    }

    // Every operation is routed, and the administrator reaches its handler:
    // with no cloud target configured nothing is written to any store.
    for (method, path, body) in OPERATIONS {
        if *method == "PUT" {
            continue;
        }
        let (status, reply) = request(&fixture.app, ADMIN, method, &concrete(path), body).await;
        assert!(
            !matches!(status, 401 | 403 | 404 | 405),
            "admin {method} {path}: {status} {reply}"
        );
        if *method == "GET" && !path.ends_with("/sets") {
            assert_eq!(status, 200, "{reply}");
            assert_eq!(reply["configured"], false, "{reply}");
        } else {
            assert_eq!(status, 409, "admin {method} {path}: {reply}");
            assert_eq!(
                reply["error"]["code"], "cloud_backup_not_configured",
                "admin {method} {path}: {reply}"
            );
        }
    }
    let (status, reply) =
        request(&fixture.app, ADMIN, "PUT", "/v1/admin/cloud-backup", POLICY).await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["configured"], true, "{reply}");
    assert!(policy.join("policy.json").exists());
}
