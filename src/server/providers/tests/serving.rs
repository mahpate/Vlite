use super::*;
use crate::{CollectionSpec, CreateOptions, Database, FormatVersion, server};
use axum::{Router, body::Body, http::Request};
use sha2::Digest;
use tower::ServiceExt;

const TENANT: &str = "0123456789abcdef0123456789abcdef";
const OTHER: &str = "1123456789abcdef0123456789abcdef";
const ADMIN: &str = "fake-admin-token-for-provider-tests";

struct RestTransport {
    calls: AtomicUsize,
    status: u16,
    response: Vec<u8>,
    action: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    bodies: Mutex<Vec<Value>>,
    gate: Mutex<Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>>,
}

impl Transport for RestTransport {
    fn send<'a>(&'a self, request: TransportRequest<'a>) -> TransportFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.bodies
            .lock()
            .unwrap()
            .push(serde_json::from_slice(request.body).unwrap());
        if let Some(action) = &*self.action.lock().unwrap() {
            action();
        }
        let gate = self.gate.lock().unwrap().clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.0.notify_one();
                gate.1.notified().await;
            }
            Ok(TransportResponse {
                status: self.status,
                body: self.response.clone(),
                retry_after: None,
            })
        })
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    service: server::Service,
    app: Router,
    transport: Arc<RestTransport>,
    name: String,
}

fn fixture(kind: &str, status: u16, response: &str, fields: &[&str]) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("tenants.json"),
        serde_json::to_vec(&json!([
            {"id":TENANT,"name":"one","max_vectors":null},
            {"id":OTHER,"name":"two","max_vectors":null}
        ]))
        .unwrap(),
    )
    .unwrap();
    let mut db = Database::create_in_memory(
        2,
        CreateOptions {
            format: Some(FormatVersion::V5),
            ..Default::default()
        },
    )
    .unwrap();
    let name = format!("t{TENANT}__items");
    let spec = CollectionSpec {
        dimensions: 2,
        settings: Default::default(),
    };
    let info = db.create_collection(&name, spec.clone()).unwrap();
    let other = format!("t{OTHER}__items");
    db.create_collection(&other, spec).unwrap();
    for collection in [&name, &other] {
        let mut collection = db.collection_mut(collection).unwrap();
        collection
            .put(
                1,
                &[1., 0.],
                br#"{"public":"allowed","private":"sentinel-secret"}"#,
            )
            .unwrap();
        collection
            .put(
                2,
                &[0., 1.],
                br#"{"public":"second","private":"sentinel-secret"}"#,
            )
            .unwrap();
    }
    let mut value = config("http://127.0.0.1:9", kind);
    value["bindings"][0]["lineage"] = json!(db.lineage().unwrap());
    value["bindings"][0]["collection_id"] = json!(info.id);
    value["bindings"][0]["tenant"] = json!(TENANT);
    value["bindings"][0]["payload_fields"] = json!(fields);
    let transport = Arc::new(RestTransport {
        calls: AtomicUsize::new(0),
        status,
        response: response.as_bytes().to_vec(),
        action: Mutex::default(),
        bodies: Mutex::default(),
        gate: Mutex::default(),
    });
    let mut registry = registry(&value);
    registry.transport = transport.clone();
    let mut service = server::Service::new(db).with_providers(registry);
    let admin = Arc::get_mut(&mut service.admin).unwrap();
    admin.auth.lock().unwrap().admin = Some(server::BearerToken::new(ADMIN).unwrap());
    admin.tenants = Some(Arc::new(
        server::tenants::Store::load(directory.path()).unwrap(),
    ));
    admin.credentials = Some(Arc::new(
        server::credentials::Store::load(directory.path()).unwrap(),
    ));
    admin.audit_log = Some(Arc::new(
        server::audit::Log::open(directory.path()).unwrap(),
    ));
    admin.directory = Some(directory.path().to_owned());
    let app = server::configured_routes(service.clone());
    Fixture {
        _directory: directory,
        service,
        app,
        transport,
        name,
    }
}

async fn request(
    app: &Router,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (u16, Value, axum::http::HeaderMap) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1_048_576)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap(), headers)
}

fn search() -> Value {
    json!({"vector":[1.,0.],"k":1,"rerank":{"mode":"custom","provider":"test","candidates":2}})
}

async fn credential(f: &Fixture, tenant: &str, operations: &[&str]) -> String {
    let (status, body, _) = request(
        &f.app,
        "/v1/admin/credentials",
        Some(ADMIN),
        json!({
            "name":"provider-test","collections":["items"],"operations":operations,
            "expires_unix_ms":server::admin::now_ms() as u64 + 600_000,"tenant_id":tenant
        }),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    body["token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn rest_custom_ranking_is_bound_sorted_and_payload_allowlisted() {
    let f = fixture(
        "rerank",
        200,
        r#"{"results":[{"id":2,"cost":-3.0}]}"#,
        &["public"],
    );
    let token = credential(&f, TENANT, &["search"]).await;
    let (status, body, _) = request(
        &f.app,
        "/v1/collections/items/search",
        Some(&token),
        search(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["hits"],
        json!([{"id":"2","distance":2.0,"rerank_score":3.0}])
    );
    assert_eq!(
        body["ranking"],
        json!({"provider":"test","order":"score_desc_id_asc"})
    );
    let bodies = f.transport.bodies.lock().unwrap();
    let wire = bodies[0].to_string();
    assert!(!wire.contains("sentinel-secret"));
    assert!(wire.contains("allowed"));
}

#[tokio::test]
async fn rest_zero_cost_score_is_positive_zero() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let path = format!("/v1/collections/{}/search", f.name);
    let (status, body, _) = request(&f.app, &path, Some(ADMIN), search()).await;
    assert_eq!(status, 200, "{body}");
    let score = body["hits"][0]["rerank_score"].as_f64().unwrap();
    assert_eq!(score.to_bits(), 0.0_f64.to_bits(), "{body}");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rest_auth_and_tenant_checks_precede_provider_dispatch() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let path = format!("/v1/collections/{}/search", f.name);
    let (status, _, _) = request(&f.app, &path, None, search()).await;
    assert_eq!(status, 401);
    let token = credential(&f, TENANT, &["read"]).await;
    let (status, _, _) = request(
        &f.app,
        "/v1/collections/items/search",
        Some(&token),
        search(),
    )
    .await;
    assert_eq!(status, 403);
    let token = credential(&f, OTHER, &["search"]).await;
    let (status, body, _) = request(
        &f.app,
        "/v1/collections/items/search",
        Some(&token),
        search(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "provider_not_configured");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_no_binding_and_defaults_do_not_dispatch() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    for body in [
        json!({"text":"hello"}),
        json!({"text":"hello","provider":"hashing"}),
    ] {
        let (status, body, _) =
            request(&f.app, "/v1/collections/default/embed", Some(ADMIN), body).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["provider"], "hashing");
    }
    let (status, body, _) = request(
        &f.app,
        "/v1/collections/default/embed",
        Some(ADMIN),
        json!({"text":"secret","provider":"test"}),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_managed_embedding_checks_dimensions_and_returns_typed_shape() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let path = format!("/v1/collections/{}/embed", f.name);
    let (status, body, _) = request(
        &f.app,
        &path,
        Some(ADMIN),
        json!({"text":"hello","provider":"test"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"vector":[1.0,0.0],"dimensions":2,"provider":"test"})
    );
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
    let f = fixture("embedding", 200, r#"{"data":[{"embedding":[1.0]}]}"#, &[]);
    let path = format!("/v1/collections/{}/embed", f.name);
    let (status, body, _) = request(
        &f.app,
        &path,
        Some(ADMIN),
        json!({"text":"hello","provider":"test"}),
    )
    .await;
    assert_eq!(status, 502, "{body}");
    assert_eq!(body["error"]["code"], "provider_invalid_response");
}

#[tokio::test]
async fn rest_custom_limits_fail_closed_and_legacy_fields_stay_absent() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let path = format!("/v1/collections/{}/search", f.name);
    for (k, candidates) in [(0, 2), (1, 0), (2, 1), (1, 1001), (1001, 1001)] {
        let mut body = search();
        body["k"] = json!(k);
        body["rerank"]["candidates"] = json!(candidates);
        let (status, body, _) = request(&f.app, &path, Some(ADMIN), body).await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["error"]["code"], "invalid_provider_input");
    }

    for mode in ["exact", "mmr"] {
        let (status, body, _) = request(
            &f.app,
            &path,
            Some(ADMIN),
            json!({"vector":[1.,0.],"k":1,"rerank":{"mode":mode}}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.get("ranking").is_none());
        assert!(body["hits"][0].get("rerank_score").is_none());
    }
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_snapshot_rechecks_all_candidates_after_releasing_database_guard() {
    for changed in [1, 2] {
        let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
        let database = f.service.database.clone();
        let name = f.name.clone();
        *f.transport.action.lock().unwrap() = Some(Box::new(move || {
            let mut owner = database
                .try_write()
                .expect("provider must not hold database guard");
            owner
                .as_mut()
                .unwrap()
                .collection_mut(&name)
                .unwrap()
                .put(changed, &[0.5, 0.5], b"sentinel-updated-record")
                .unwrap();
        }));
        let path = format!("/v1/collections/{}/search", f.name);
        let (status, body, headers) = request(&f.app, &path, Some(ADMIN), search()).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "provider_snapshot_conflict");
        assert!(headers.get("retry-after").is_none());
        assert!(!body.to_string().contains(TENANT));
        assert!(!body.to_string().contains("sentinel-updated-record"));
    }
}

#[tokio::test]
async fn rest_auth_is_revalidated_after_provider_work() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let admin = f.service.admin.clone();
    *f.transport.action.lock().unwrap() = Some(Box::new(move || {
        admin.auth.lock().unwrap().admin =
            Some(server::BearerToken::new("fake-rotated-token-for-provider-tests").unwrap());
    }));
    let path = format!("/v1/collections/{}/embed", f.name);
    let (status, _, _) = request(
        &f.app,
        &path,
        Some(ADMIN),
        json!({"text":"secret","provider":"test"}),
    )
    .await;
    assert_eq!(status, 401);
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rest_provider_errors_are_sanitized_and_auth_never_becomes_caller_401() {
    for (upstream, expected, code) in [
        (401, 503, "provider_unavailable"),
        (403, 503, "provider_unavailable"),
        (500, 503, "provider_unavailable"),
        (200, 502, "provider_invalid_response"),
    ] {
        let f = fixture(
            "rerank",
            upstream,
            r#"{"secret":"sentinel-secret","text":"private-text"}"#,
            &[],
        );
        let path = format!("/v1/collections/{}/search", f.name);
        let (status, body, _) = request(&f.app, &path, Some(ADMIN), search()).await;
        assert_eq!(status, expected, "{body}");
        assert_eq!(body["error"]["code"], code);
        assert!(!body.to_string().contains("sentinel-secret"));
        assert!(!body.to_string().contains("private-text"));
        assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
    }
    for response in [
        r#"{"results":[{"id":999,"cost":0.0}]}"#,
        r#"{"results":[{"id":1,"cost":0.0},{"id":1,"cost":0.0}]}"#,
        r#"{"results":[{"id":1,"cost":1e999}]}"#,
    ] {
        let f = fixture("rerank", 200, response, &[]);
        let path = format!("/v1/collections/{}/search", f.name);
        let (status, body, _) = request(&f.app, &path, Some(ADMIN), search()).await;
        assert_eq!(status, 502, "{body}");
        assert_eq!(body["error"]["code"], "provider_invalid_response");
        assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn rest_provider_error_mapping_and_busy_retry_after() {
    use axum::response::IntoResponse;
    for (error, status, code) in [
        (ProviderError::NotConfigured, 409, "provider_not_configured"),
        (ProviderError::InvalidInput, 422, "invalid_provider_input"),
        (
            ProviderError::InvalidResponse,
            502,
            "provider_invalid_response",
        ),
        (ProviderError::Unavailable, 503, "provider_unavailable"),
        (ProviderError::Timeout, 504, "provider_timeout"),
        (ProviderError::Busy, 429, "provider_busy"),
    ] {
        let api = server::ApiError::from(error);
        assert_eq!(api.status.as_u16(), status);
        assert_eq!(api.code, code);
        let response = api.into_response();
        assert_eq!(
            response.headers().get("retry-after").is_some(),
            error == ProviderError::Busy
        );
    }
}

#[tokio::test]
async fn rest_default_and_ceiling_candidate_counts_are_enforced() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    {
        let mut owner = f.service.database.write().unwrap();
        let mut collection = owner.as_mut().unwrap().collection_mut(&f.name).unwrap();
        for id in 3..=1001 {
            collection.put(id, &[id as f32, 0.], b"").unwrap();
        }
    }
    let path = format!("/v1/collections/{}/search", f.name);
    for (count, expected) in [(None, 50), (Some(1000), 1000)] {
        let mut body = search();
        body["rerank"].as_object_mut().unwrap().remove("candidates");
        if let Some(count) = count {
            body["rerank"]["candidates"] = json!(count);
        }
        let (status, body, _) = request(&f.app, &path, Some(ADMIN), body).await;
        assert_eq!(status, 200, "{body}");
        let bodies = f.transport.bodies.lock().unwrap();
        assert_eq!(
            bodies.last().unwrap()["candidates"]
                .as_array()
                .unwrap()
                .len(),
            expected
        );
    }
    let mut body = search();
    body["rerank"]["candidates"] = json!(1001);
    assert_eq!(request(&f.app, &path, Some(ADMIN), body).await.0, 422);
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn rest_busy_admission_rejects_without_dispatching() {
    for tenant_slots in [false, true] {
        let f = fixture("embedding", 200, r#"{"data":[{"embedding":[1.,0.]}]}"#, &[]);
        let slots = if tenant_slots {
            f.service
                .providers
                .bindings
                .values()
                .next()
                .unwrap()
                .slots
                .clone()
        } else {
            f.service.providers.providers["test"].slots.clone()
        };
        let _held = slots
            .try_acquire_many(if tenant_slots { 1 } else { 2 })
            .unwrap();
        let path = format!("/v1/collections/{}/embed", f.name);
        let (status, body, headers) = request(
            &f.app,
            &path,
            Some(ADMIN),
            json!({"text":"hello","provider":"test"}),
        )
        .await;
        assert_eq!(status, 429, "{body}");
        assert_eq!(body["error"]["code"], "provider_busy");
        assert_eq!(headers["retry-after"], "1");
        assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn rest_empty_registry_means_zero_egress() {
    let mut f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let registry = ProviderRegistry {
        transport: f.transport.clone(),
        ..ProviderRegistry::default()
    };
    f.service.providers = Arc::new(registry);
    f.app = server::configured_routes(f.service.clone());
    let path = format!("/v1/collections/{}/embed", f.name);
    let (status, body, _) = request(
        &f.app,
        &path,
        Some(ADMIN),
        json!({"text":"hello","provider":"test"}),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "provider_not_configured");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dispatch_uses_original_start_budget_and_binding_generation() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let key = f.service.providers.bindings.keys().next().unwrap().clone();
    assert_eq!(
        f.service
            .providers
            .embed_bound(&key, "text", Instant::now() - Duration::from_secs(11))
            .await
            .unwrap_err(),
        ProviderError::Timeout
    );
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
    let mut value = config("http://127.0.0.1:9", "embedding");
    value["bindings"][0]["tenant"] = json!(TENANT);
    let mut registry = registry(&value);
    let key = (42, 0, TENANT.to_owned(), "test".to_owned());
    let snapshot = registry.snapshot(&key, true).unwrap();
    assert!(registry.unchanged(&key, &snapshot));
    registry.bindings.get_mut(&key).unwrap().generation = Arc::new(());
    assert!(!registry.unchanged(&key, &snapshot));
}

#[tokio::test]
async fn rest_suspended_tenant_is_checked_before_and_after_inference_even_for_admin() {
    for scoped in [false, true] {
        let f = fixture(
            "embedding",
            200,
            r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
            &[],
        );
        let token = if scoped {
            credential(&f, TENANT, &["search"]).await
        } else {
            ADMIN.to_owned()
        };
        let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        *f.transport.gate.lock().unwrap() = Some(gate.clone());
        let app = f.app.clone();
        let path = if scoped {
            "/v1/collections/items/embed".to_owned()
        } else {
            format!("/v1/collections/{}/embed", f.name)
        };
        let initial_path = path.clone();
        let initial_token = token.clone();
        let running = tokio::spawn(async move {
            request(
                &app,
                &initial_path,
                Some(&initial_token),
                json!({"text":"hello","provider":"test"}),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(10), gate.0.notified())
            .await
            .unwrap();
        let (status, body, _) = request(
            &f.app,
            &format!("/v1/admin/tenants/{TENANT}/suspend"),
            Some(ADMIN),
            json!({}),
        )
        .await;
        gate.1.notify_one();
        assert_eq!(status, 200, "{body}");
        let (status, body, _) = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["error"]["code"], "tenant_suspended");
        *f.transport.gate.lock().unwrap() = None;
        let (status, _, _) = request(
            &f.app,
            &path,
            Some(&token),
            json!({"text":"hello","provider":"test"}),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
        let (status, body, _) = request(
            &f.app,
            &format!("/v1/admin/tenants/{TENANT}/resume"),
            Some(ADMIN),
            json!({}),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            request(
                &f.app,
                &path,
                Some(&token),
                json!({"text":"hello","provider":"test"})
            )
            .await
            .0,
            200
        );
    }
}

#[tokio::test]
async fn owning_tenant_binding_is_used_for_every_authorized_role() {
    for caller in [
        "admin",
        "tenant-scoped",
        "tenantless-scoped",
        "read-only",
        "no-auth",
    ] {
        let f = fixture(
            "embedding",
            200,
            r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
            &[],
        );
        let (path, token) = match caller {
            "tenant-scoped" => (
                "/v1/collections/items/embed".to_owned(),
                Some(credential(&f, TENANT, &["search"]).await),
            ),
            "tenantless-scoped" => {
                let (status, body, _) = request(&f.app,"/v1/admin/credentials",Some(ADMIN),json!({
                    "name":"tenantless-provider-test","collections":[f.name],"operations":["search"],
                    "expires_unix_ms":server::admin::now_ms() as u64 + 600_000
                })).await;
                assert_eq!(status, 201, "{body}");
                (
                    format!("/v1/collections/{}/embed", f.name),
                    Some(body["token"].as_str().unwrap().to_owned()),
                )
            }
            "read-only" => {
                let token = "fake-read-only-token-for-provider-tests";
                f.service.admin.auth.lock().unwrap().read_only =
                    Some(server::BearerToken::new(token).unwrap());
                (
                    format!("/v1/collections/{}/embed", f.name),
                    Some(token.to_owned()),
                )
            }
            "no-auth" => {
                f.service.admin.auth.lock().unwrap().admin = None;
                (format!("/v1/collections/{}/embed", f.name), None)
            }
            _ => (
                format!("/v1/collections/{}/embed", f.name),
                Some(ADMIN.to_owned()),
            ),
        };
        let (status, body, _) = request(
            &f.app,
            &path,
            token.as_deref(),
            json!({"text":"hello","provider":"test"}),
        )
        .await;
        assert_eq!(status, 200, "caller={caller}: {body}");
        assert_eq!(body["provider"], "test");
        assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn a_different_tenants_binding_is_never_used() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let token = credential(&f, OTHER, &["search"]).await;
    let (status, body, _) = request(
        &f.app,
        "/v1/collections/items/embed",
        Some(&token),
        json!({"text":"hello","provider":"test"}),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "provider_not_configured");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn non_tenant_binding_is_rejected_even_when_config_has_an_exact_key() {
    for kind in ["embedding", "rerank"] {
        let response = if kind == "embedding" {
            r#"{"data":[{"embedding":[1.0,0.0]}]}"#
        } else {
            r#"{"results":[{"id":1,"cost":0.0}]}"#
        };
        let mut f = fixture(kind, 200, response, &[]);
        {
            let mut owner = f.service.database.write().unwrap();
            let mut collection = owner.as_mut().unwrap().collection_mut("default").unwrap();
            collection.put(1, &[1., 0.], b"").unwrap();
            collection.put(2, &[0., 1.], b"").unwrap();
        }
        let key = f.service.providers.bindings.keys().next().unwrap().clone();
        let mut value = config("http://127.0.0.1:9", kind);
        value["bindings"][0]["lineage"] = json!(key.0);
        value["bindings"][0]["collection_id"] = json!(0);
        value["bindings"][0]["tenant"] = json!(TENANT);
        let mut registry = registry(&value);
        registry.transport = f.transport.clone();
        f.service.providers = Arc::new(registry);
        f.app = server::configured_routes(f.service.clone());
        let (path, input) = if kind == "embedding" {
            (
                "/v1/collections/default/embed",
                json!({"text":"hello","provider":"test"}),
            )
        } else {
            ("/v1/search", search())
        };
        let (status, body, _) = request(&f.app, path, Some(ADMIN), input).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "provider_not_configured");
        assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn rest_deadline_includes_database_queue_before_dispatch() {
    let mut f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let key = f.service.providers.bindings.keys().next().unwrap().clone();
    let mut value = config("http://127.0.0.1:9", "embedding");
    value["aliases"][0]["connect_timeout_ms"] = json!(1000);
    value["aliases"][0]["total_timeout_ms"] = json!(1000);
    value["bindings"][0]["lineage"] = json!(key.0);
    value["bindings"][0]["collection_id"] = json!(key.1);
    value["bindings"][0]["tenant"] = json!(key.2);
    let mut registry = registry(&value);
    registry.transport = f.transport.clone();
    f.service.providers = Arc::new(registry);
    let worker = f.service.clone();
    let target = server::Target::Named(f.name.clone());
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {ADMIN}").parse().unwrap());
    let database = f.service.database.clone();
    let (entered, observed) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _guard = database.write().unwrap();
        entered.send(()).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(20));
    });
    observed.recv_timeout(Duration::from_secs(20)).unwrap();
    let started = Instant::now();
    let pending = tokio::spawn(async move {
        server::provider_serving::embed(
            &worker,
            headers,
            target,
            "test".into(),
            "hello".into(),
            started,
        )
        .await
    });
    // Deliberately cross the absolute deadline while the database is unavailable.
    // There is no upper elapsed-time assertion or connect/header timing assumption.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(started.elapsed() >= Duration::from_secs(1));
    let _ = release.send(()); // The leak guard may already have released it.
    holder.join().unwrap();
    let error = tokio::time::timeout(Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap()
        .err()
        .unwrap();
    assert_eq!(error.status.as_u16(), 504);
    assert_eq!(error.code, "provider_timeout");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_non_json_payload_with_allowlist_is_rejected_before_dispatch() {
    let f = fixture(
        "rerank",
        200,
        r#"{"results":[{"id":1,"cost":0.0}]}"#,
        &["public"],
    );
    {
        let mut owner = f.service.database.write().unwrap();
        owner
            .as_mut()
            .unwrap()
            .collection_mut(&f.name)
            .unwrap()
            .put(1, &[1., 0.], b"raw-private-payload")
            .unwrap();
    }
    let path = format!("/v1/collections/{}/search", f.name);
    let (status, body, _) = request(&f.app, &path, Some(ADMIN), search()).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "invalid_provider_input");
    assert!(!body.to_string().contains("raw-private-payload"));
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_search_scope_is_required_without_middleware() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let token = credential(&f, TENANT, &["read"]).await;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    let result = server::provider_serving::embed(
        &f.service,
        headers,
        server::Target::Named("items".into()),
        "test".into(),
        "hello".into(),
        Instant::now(),
    )
    .await;
    assert_eq!(result.err().unwrap().status.as_u16(), 403);
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_text_ceiling_and_custom_vector_dimensions_precede_egress() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let path = format!("/v1/collections/{}/embed", f.name);
    for (bytes, status) in [(16384, 200), (16385, 422)] {
        let (actual, body, _) = request(
            &f.app,
            &path,
            Some(ADMIN),
            json!({"text":"a".repeat(bytes),"provider":"test"}),
        )
        .await;
        assert_eq!(actual, status, "{body}");
    }
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let path = format!("/v1/collections/{}/search", f.name);
    let mut body = search();
    body["vector"] = json!([1.0]);
    let (status, body, _) = request(&f.app, &path, Some(ADMIN), body).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "invalid_provider_input");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_expiry_and_collection_identity_changes_abort_the_snapshot() {
    for expiry_only in [true, false] {
        let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
        let database = f.service.database.clone();
        let name = f.name.clone();
        *f.transport.action.lock().unwrap() = Some(Box::new(move || {
            let mut owner = database.try_write().expect("database guard released");
            let db = owner.as_mut().unwrap();
            if expiry_only {
                let id = db.collection(&name).unwrap().id();
                let revision = db.collection(&name).unwrap().revision(2).unwrap();
                Arc::make_mut(&mut db.state_mut(id).expiries)
                    .insert(2, crate::current_time_ms() + 600_000);
                assert_eq!(db.collection(&name).unwrap().revision(2).unwrap(), revision);
                assert!(db.collection(&name).unwrap().get(2).unwrap().is_some());
            } else {
                let mut collection = db.collection_mut(&name).unwrap();
                collection.delete(1).unwrap();
                collection.delete(2).unwrap();
                db.drop_collection(&name).unwrap();
                db.create_collection(
                    &name,
                    CollectionSpec {
                        dimensions: 2,
                        settings: Default::default(),
                    },
                )
                .unwrap();
            }
        }));
        let path = format!("/v1/collections/{}/search", f.name);
        let (status, body, _) = request(&f.app, &path, Some(ADMIN), search()).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "provider_snapshot_conflict");
    }
}

fn replace_registry(f: &mut Fixture, kind: &str, alias: &str, connect_ms: u64, total_ms: u64) {
    let key = f.service.providers.bindings.keys().next().unwrap().clone();
    let mut value = config("http://127.0.0.1:9", kind);
    value["aliases"][0]["alias"] = json!(alias);
    value["aliases"][0]["connect_timeout_ms"] = json!(connect_ms);
    value["aliases"][0]["total_timeout_ms"] = json!(total_ms);
    value["bindings"][0]["lineage"] = json!(key.0);
    value["bindings"][0]["collection_id"] = json!(key.1);
    value["bindings"][0]["tenant"] = json!(key.2);
    value["bindings"][0]["alias"] = json!(alias);
    let mut registry = registry(&value);
    registry.transport = f.transport.clone();
    f.service.providers = Arc::new(registry);
    f.app = server::configured_routes(f.service.clone());
}

#[tokio::test]
async fn rest_embedding_recreated_collection_identity_aborts_without_candidates() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let database = f.service.database.clone();
    let name = f.name.clone();
    *f.transport.action.lock().unwrap() = Some(Box::new(move || {
        let mut owner = database.try_write().expect("database guard released");
        let db = owner.as_mut().unwrap();
        let old_id = db.collection(&name).unwrap().id();
        let mut collection = db.collection_mut(&name).unwrap();
        collection.delete(1).unwrap();
        collection.delete(2).unwrap();
        db.drop_collection(&name).unwrap();
        let new = db
            .create_collection(
                &name,
                CollectionSpec {
                    dimensions: 2,
                    settings: Default::default(),
                },
            )
            .unwrap();
        assert_ne!(old_id, new.id);
        assert_eq!(db.collection(&name).unwrap().dimensions(), 2);
    }));
    let path = format!("/v1/collections/{}/embed", f.name);
    let (status, body, _) = request(
        &f.app,
        &path,
        Some(ADMIN),
        json!({"text":"hello","provider":"test"}),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "provider_snapshot_conflict");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rest_candidate_expiration_without_revision_change_aborts_snapshot() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let expires = crate::current_time_ms() + 1_500;
    let (revision, id) = {
        let mut owner = f.service.database.write().unwrap();
        let db = owner.as_mut().unwrap();
        let id = db.collection(&f.name).unwrap().id();
        Arc::make_mut(&mut db.state_mut(id).expiries).insert(2, expires);
        (db.collection(&f.name).unwrap().revision(2).unwrap(), id)
    };
    let database = f.service.database.clone();
    let name = f.name.clone();
    *f.transport.action.lock().unwrap() = Some(Box::new(move || {
        while crate::current_time_ms() <= expires {
            std::thread::sleep(Duration::from_millis(10));
        }
        let owner = database.try_read().expect("database guard released");
        let db = owner.as_ref().unwrap();
        assert_eq!(db.collection(&name).unwrap().id(), id);
        assert_eq!(db.collection(&name).unwrap().revision(2).unwrap(), revision);
        assert_eq!(
            db.collection(&name).unwrap().expires_at(2).unwrap(),
            Some(expires)
        );
    }));
    let path = format!("/v1/collections/{}/search", f.name);
    let (status, body, _) = request(&f.app, &path, Some(ADMIN), search()).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "provider_snapshot_conflict");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rest_provider_receives_stored_candidate_vectors_and_explain_count() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let mut input = search();
    input["explain"] = json!(true);
    input["mode"] = json!("auto");
    let path = format!("/v1/collections/{}/search", f.name);
    let (status, body, _) = request(&f.app, &path, Some(ADMIN), input).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["stats"]["reranked_candidates"], 2);
    assert!(body["plan"].is_object(), "{body}");
    let bodies = f.transport.bodies.lock().unwrap();
    let candidates = bodies[0]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    for candidate in candidates {
        let expected = match candidate["id"].as_u64().unwrap() {
            1 => json!([1.0, 0.0]),
            2 => json!([0.0, 1.0]),
            id => panic!("foreign candidate {id}"),
        };
        assert_eq!(candidate["vector"], expected);
    }
}

#[tokio::test]
async fn rest_custom_validation_maps_422_but_preserves_role_403() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let path = format!("/v1/collections/{}/search", f.name);
    for field in ["ef_search", "text", "fusion"] {
        let mut input = search();
        input[field] = match field {
            "text" => json!("not a hybrid request"),
            "fusion" => json!({}),
            _ => json!(10),
        };
        let (status, body, _) = request(&f.app, &path, Some(ADMIN), input).await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["error"]["code"], "invalid_provider_input");
    }
    let token = "fake-read-only-validation-token-for-tests";
    f.service.admin.auth.lock().unwrap().read_only = Some(server::BearerToken::new(token).unwrap());
    let mut input = search();
    input["mode"] = json!("approximate");
    let (status, body, _) = request(&f.app, &path, Some(token), input).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["code"], "read_only");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_malformed_tenant_names_have_no_managed_binding() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let uppercase = "tABCDEF0123456789ABCDEF0123456789__x";
    assert!(matches!(
        f.service
            .database
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .create_collection(
                uppercase,
                CollectionSpec {
                    dimensions: 2,
                    settings: Default::default()
                }
            ),
        Err(crate::Error::InvalidCollectionName)
    ));
    for (name, expected, code) in [("tabcd__x", 409, "provider_not_configured")] {
        f.service
            .database
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .create_collection(
                name,
                CollectionSpec {
                    dimensions: 2,
                    settings: Default::default(),
                },
            )
            .unwrap();
        let path = format!("/v1/collections/{name}/embed");
        let (status, body, _) = request(
            &f.app,
            &path,
            Some(ADMIN),
            json!({"text":"hello","provider":"test"}),
        )
        .await;
        assert_eq!(status, expected, "{body}");
        assert_eq!(body["error"]["code"], code);
    }
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_invalid_managed_aliases_fail_before_egress() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let path = format!("/v1/collections/{}/embed", f.name);
    for alias in ["".to_owned(), "bad alias".to_owned(), "a".repeat(257)] {
        let (status, body, _) = request(
            &f.app,
            &path,
            Some(ADMIN),
            json!({"text":"hello","provider":alias}),
        )
        .await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["error"]["code"], "invalid_provider_input");
    }
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let path = format!("/v1/collections/{}/search", f.name);
    let mut input = search();
    input["rerank"]["provider"] = json!("bad alias");
    let (status, body, _) = request(&f.app, &path, Some(ADMIN), input).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "invalid_provider_input");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_total_budget_allows_work_longer_than_connect_budget() {
    let mut f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    replace_registry(&mut f, "embedding", "test", 10, 10_000);
    *f.transport.action.lock().unwrap() = Some(Box::new(|| {
        std::thread::sleep(Duration::from_millis(100));
    }));
    let path = format!("/v1/collections/{}/embed", f.name);
    let (status, body, _) = request(
        &f.app,
        &path,
        Some(ADMIN),
        json!({"text":"hello","provider":"test"}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn rest_custom_rerank_records_provider_outcome() {
    let mut f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let alias = "rest-rank-log-proof-21f93";
    replace_registry(&mut f, "rerank", alias, 2_000, 10_000);
    let mut input = search();
    input["rerank"]["provider"] = json!(alias);
    let path = format!("/v1/collections/{}/search", f.name);
    let (status, body, _) = request(&f.app, &path, Some(ADMIN), input).await;
    assert_eq!(status, 200, "{body}");
    let expected = format!("{:x}", sha2::Sha256::digest(TENANT.as_bytes()));
    let entries: Vec<_> = server::logs::buffer()
        .snapshot()
        .0
        .into_iter()
        .filter(|entry| {
            entry.target == "providers"
                && entry.fields["alias"] == alias
                && entry.fields["tenant_fingerprint"] == expected
        })
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].fields["outcome"], "ok");
    assert_eq!(entries[0].fields["batch_count"], 2);
}

#[tokio::test]
async fn rest_builtin_rerank_rejects_provider_field_without_egress() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let path = format!("/v1/collections/{}/search", f.name);
    for mode in ["exact", "mmr"] {
        let mut input = search();
        input["rerank"]["mode"] = json!(mode);
        let (status, body, _) = request(&f.app, &path, Some(ADMIN), input).await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["error"]["code"], "invalid_provider_input");
    }
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rest_http_alias_preserves_legacy_dispatch() {
    #[cfg(feature = "http-embedder")]
    if std::env::var_os("VLITE_TEST_HTTP_PRECEDENCE_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "server::providers::tests::serving::rest_http_alias_preserves_legacy_dispatch",
                "--nocapture",
            ])
            .env("VLITE_TEST_HTTP_PRECEDENCE_CHILD", "1")
            .env_remove("VLITE_EMBEDDING_URL")
            .env_remove("VLITE_EMBEDDING_MODEL")
            .env_remove("VLITE_EMBEDDING_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let mut f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    replace_registry(&mut f, "embedding", "http", 2_000, 10_000);
    let path = format!("/v1/collections/{}/embed", f.name);
    let (status, body, _) = request(
        &f.app,
        &path,
        Some(ADMIN),
        json!({"text":"hello","provider":"http"}),
    )
    .await;
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
    #[cfg(not(feature = "http-embedder"))]
    {
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "http_embedder_disabled");
    }
    #[cfg(feature = "http-embedder")]
    {
        assert_ne!(status, 200, "{body}");
        assert_ne!(body["error"]["code"], "provider_not_configured");
    }
}

#[test]
fn registry_snapshot_generation_requires_the_same_instance_and_key() {
    let value = config("http://127.0.0.1:9", "embedding");
    let first = registry(&value);
    let second = registry(&value);
    let snapshot = first.snapshot(&key(), true).unwrap();
    assert!(first.unchanged(&key(), &snapshot));
    assert!(!second.unchanged(&key(), &snapshot));
    let mut missing = key();
    missing.3 = "missing".into();
    assert!(!first.unchanged(&missing, &snapshot));
}

#[test]
fn collection_settings_preserve_identity_name_and_dimensions() {
    let f = fixture(
        "embedding",
        200,
        r#"{"data":[{"embedding":[1.0,0.0]}]}"#,
        &[],
    );
    let mut owner = f.service.database.write().unwrap();
    let db = owner.as_mut().unwrap();
    let before = db.collection(&f.name).unwrap();
    let identity = (before.id(), before.name().to_owned(), before.dimensions());
    db.set_collection_settings(&f.name, Default::default())
        .unwrap();
    let after = db.collection(&f.name).unwrap();
    assert_eq!(
        (after.id(), after.name().to_owned(), after.dimensions()),
        identity
    );
}

#[tokio::test]
async fn rest_float_overflow_query_is_provider_input_error_without_egress() {
    let f = fixture("rerank", 200, r#"{"results":[{"id":1,"cost":0.0}]}"#, &[]);
    let path = format!("/v1/collections/{}/search", f.name);
    let response = f.app.clone().oneshot(
        Request::builder().method("POST").uri(&path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {ADMIN}"))
            .body(Body::from(r#"{"vector":[1e39,0],"k":1,"rerank":{"mode":"custom","provider":"test","candidates":2}}"#)).unwrap()
    ).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1_048_576)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "invalid_provider_input");
    assert_eq!(f.transport.calls.load(Ordering::SeqCst), 0);
}
