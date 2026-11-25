//! Gateway-trusted backend mode through the real router (feature-aca-ha
//! ITEM-010/012/014: TEST-010, TEST-004 backend side, TEST-014 skew), with
//! the tenant directory and freshness read from a polled control state.
use super::cloud_backup::fake::Memory;
use super::control_state::{Feed, Objects, Route, Snapshot};
use super::gateway_trust::Trust;
use super::principal_envelope::{Claims, Policy, Signer, Verifier, tests as env};
use super::*;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

const AUDIENCE: &str = "backend-a";
const LIFETIME: u64 = 60_000;
const T1: &str = "0123456789abcdef0123456789abcdef";
const T2: &str = "fedcba9876543210fedcba9876543210";
const T3: &str = "00112233445566778899aabbccddeeff";
const NAME: &str = "control/state.json";

fn now() -> u64 {
    admin::now_ms() as u64
}

fn service() -> Service {
    Service::new(Database::create_in_memory(1, crate::CreateOptions::default()).unwrap())
}

fn verifier(keys: &[&Signer], minimum: u64) -> Verifier {
    Verifier::new(
        keys.iter()
            .map(|k| ("k1".to_owned(), k.public_key()))
            .take(1)
            .chain(
                keys.iter()
                    .skip(1)
                    .map(|k| ("k2".to_owned(), k.public_key())),
            )
            .collect(),
        Policy {
            audience: AUDIENCE.into(),
            tenant: None,
            skew_ms: 1_000,
            max_lifetime_ms: LIFETIME,
            minimum_generation: minimum,
        },
    )
    .unwrap()
}

fn claims(tenant: Option<&str>, lineage: u64, ops: &[credentials::Op], generation: u64) -> Claims {
    claims_for("abcdefghijkl", tenant, lineage, ops, generation)
}

fn claims_for(
    id: &str,
    tenant: Option<&str>,
    lineage: u64,
    ops: &[credentials::Op],
    generation: u64,
) -> Claims {
    let now = now();
    Claims {
        aud: AUDIENCE.into(),
        iat: now,
        exp: now + 30_000,
        generation,
        principal: credentials::Principal::of(&credentials::Grant::for_user(
            id.into(),
            "gateway principal".into(),
            tenant.map(str::to_owned),
            ops,
            lineage,
            now + 3_600_000,
        )),
    }
}

async fn call(
    router: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = if method == "POST" {
        Body::from(r#"{"name":"made","dimensions":2}"#)
    } else {
        Body::empty()
    };
    let request = request
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A trusted backend over a published control state: T1 and T2 share this
/// backend's pool (audience `backend-a`, this database's lineage), T3 is
/// routed to another backend and T2 is suspended when `suspend_t2`.
struct Backend {
    router: Router,
    lineage: u64,
}

fn trusted(keys: &[&Signer], minimum: u64, s_ctl_ms: u64, suspend_t2: bool) -> Backend {
    let mut service = service();
    let lineage = service.lineage.unwrap();
    let here = |tenant: &str| Route {
        authority_mode: None,
        audience: AUDIENCE.into(),
        lineage,
        ..Route::shared_for_test(tenant)
    };
    let directory = vec![
        here(T1),
        here(T2),
        Route {
            authority_mode: None,
            audience: "backend-b".into(),
            lineage,
            ..Route::shared_for_test(T3)
        },
    ];
    let tenants = tenants::Store::for_test(vec![
        tenants::Tenant::for_test(T1, None),
        tenants::Tenant::for_test(T2, suspend_t2.then_some(1)),
        tenants::Tenant::for_test(T3, None),
    ]);
    let (credentials, _) = credentials::tests::issued(&[], lineage);
    let snapshot = Snapshot::import(&credentials, Some(&tenants))
        .unwrap()
        .with_directory(directory);
    let memory = Arc::new(Memory::default());
    Objects::new(memory.clone(), NAME)
        .create(snapshot, now())
        .unwrap();
    let feed = Arc::new(Feed::new(Objects::new(memory, NAME), s_ctl_ms, 1).unwrap());
    feed.poll_once().unwrap();
    service.trust = Some(Arc::new(Trust::for_test(
        verifier(keys, minimum),
        feed,
        AUDIENCE,
    )));
    Backend {
        router: router(service),
        lineage,
    }
}

const READ: &[credentials::Op] = &[credentials::Op::Read, credentials::Op::Collections];
const HOUR: u64 = 3_600_000;

#[tokio::test]
async fn standalone_default_is_unchanged() {
    let router = router(service());
    let (status, _) = call(&router, "GET", "/v1/collections", &[]).await;
    assert_eq!(status, StatusCode::OK);
    let key = env::signer("k1", 1);
    let token = key.sign(&claims(Some(T1), 1, READ, 1)).unwrap();
    // The header means nothing to a standalone server.
    let (status, _) = call(
        &router,
        "GET",
        "/v1/collections",
        &[("x-vlite-principal", &token)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// TEST-010: network position, admin tokens and dev-open mode are not
/// authentication in trusted mode; only a valid envelope for a tenant the
/// directory routes to this backend is.
#[tokio::test]
async fn trusted_mode_requires_a_valid_envelope_for_a_tenant_routed_here() {
    let key = env::signer("k1", 1);
    let impostor = env::signer("k1", 2);
    let Backend { router, lineage } = trusted(&[&key], 2, HOUR, false);
    let header = principal_envelope::HEADER;
    let as_t1 = |generation| claims(Some(T1), lineage, READ, generation);

    let (status, body) = call(&router, "GET", "/v1/collections", &[]).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("unauthorized"))
    );
    for (name, value) in [
        ("authorization", "Bearer anything"),
        ("cookie", "vlite_session=x"),
    ] {
        let (status, _) = call(&router, "GET", "/v1/collections", &[(name, value)]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    let valid = key.sign(&as_t1(2)).unwrap();
    let mut expired = as_t1(2);
    expired.iat -= 40_000;
    expired.exp -= 40_000;
    let mut audience = as_t1(2);
    audience.aud = "backend-b".into();
    let cases = [
        ("garbage".to_owned(), "envelope_invalid"),
        (impostor.sign(&as_t1(2)).unwrap(), "envelope_invalid"),
        (
            env::signer("k9", 1).sign(&as_t1(2)).unwrap(),
            "envelope_invalid",
        ),
        (key.sign(&audience).unwrap(), "envelope_invalid"),
        // Routed to another backend, unknown, or untenanted: not served here.
        (
            key.sign(&claims(Some(T3), lineage, READ, 2)).unwrap(),
            "envelope_invalid",
        ),
        (
            key.sign(&claims(Some(&"9".repeat(32)), lineage, READ, 2))
                .unwrap(),
            "envelope_invalid",
        ),
        (
            key.sign(&claims(None, lineage, READ, 2)).unwrap(),
            "envelope_invalid",
        ),
        (key.sign(&expired).unwrap(), "envelope_expired"),
        (key.sign(&as_t1(1)).unwrap(), "envelope_stale"),
    ];
    for (token, code) in &cases {
        let (status, body) = call(&router, "GET", "/v1/collections", &[(header, token)]).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some(*code)),
            "{code}"
        );
    }
    let (status, _) = call(
        &router,
        "GET",
        "/v1/collections",
        &[(header, &valid), (header, &valid)],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = call(&router, "GET", "/v1/collections", &[(header, &valid)]).await;
    assert_eq!(status, StatusCode::OK);
    // A bearer token next to a valid envelope is ignored, not elevated.
    let (status, _) = call(
        &router,
        "GET",
        "/v1/admin/credentials",
        &[(header, &valid), ("authorization", "Bearer x")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// The envelope principal goes through the existing authorization and
/// guards: operation scope, admin-only routes, the lineage binding and
/// tenant isolation on a shared pool.
#[tokio::test]
async fn envelope_principals_are_authorized_like_scoped_credentials() {
    let key = env::signer("k1", 1);
    let Backend { router, lineage } = trusted(&[&key], 1, HOUR, false);
    let header = principal_envelope::HEADER;
    let reader = key
        .sign(&claims(Some(T1), lineage, &[credentials::Op::Read], 1))
        .unwrap();
    let (status, body) = call(&router, "POST", "/v1/collections", &[(header, &reader)]).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden_scope"))
    );
    let (status, _) = call(
        &router,
        "GET",
        "/v1/admin/credentials",
        &[(header, &reader)],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let writer = key.sign(&claims(Some(T1), lineage, READ, 1)).unwrap();
    let (status, body) = call(&router, "POST", "/v1/collections", &[(header, &writer)]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let other_lineage = key.sign(&claims(Some(T1), lineage ^ 1, READ, 1)).unwrap();
    let (status, _) = call(
        &router,
        "GET",
        "/v1/collections/made",
        &[(header, &other_lineage)],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&router, "GET", "/v1/collections/made", &[(header, &writer)]).await;
    assert_eq!(status, StatusCode::OK);

    // Same pool, other tenant: T1's collection does not exist for T2.
    let neighbour = key.sign(&claims(Some(T2), lineage, READ, 1)).unwrap();
    let (status, _) = call(
        &router,
        "GET",
        "/v1/collections/made",
        &[(header, &neighbour)],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Tenant admission and freshness come from the polled control state:
/// a suspended tenant is refused and, once the snapshot is older than
/// S_ctl, both `/ready` and every authenticated request fail closed.
#[tokio::test]
async fn suspension_and_a_stale_control_state_fail_closed() {
    let key = env::signer("k1", 1);
    let Backend { router, lineage } = trusted(&[&key], 1, 200, true);
    let header = principal_envelope::HEADER;
    let suspended = key.sign(&claims(Some(T2), lineage, READ, 1)).unwrap();
    let (status, body) = call(&router, "GET", "/v1/collections", &[(header, &suspended)]).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("tenant_suspended"))
    );
    let (status, body) = call(&router, "GET", "/ready", &[]).await;
    assert_eq!(
        (status, body["generation"].as_u64()),
        (StatusCode::OK, Some(1))
    );

    // Nothing polls this feed: past S_ctl the confirmed read is too old.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let valid = key.sign(&claims(Some(T1), lineage, READ, 1)).unwrap();
    for (path, headers) in [
        ("/ready", vec![]),
        ("/v1/collections", vec![(header, valid.as_str())]),
    ] {
        let (status, body) = call(&router, "GET", path, &headers).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, Some("control_state_stale")),
            "{path}"
        );
    }
}

/// RD-004 through the router: once a backend has accepted `A = 3` it
/// refuses an envelope minted from an older snapshot, and old and new keys
/// both verify during a rotation overlap.
#[tokio::test]
async fn backend_holds_the_highest_generation_and_accepts_rotation_overlap() {
    let old = env::signer("k1", 1);
    let new = env::signer("k2", 2);
    let Backend { router, lineage } = trusted(&[&old, &new], 1, HOUR, false);
    let header = principal_envelope::HEADER;
    let at = |key: &Signer, a| key.sign(&claims(Some(T1), lineage, READ, a)).unwrap();
    assert_eq!(
        call(&router, "GET", "/v1/collections", &[(header, &at(&old, 1))])
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&router, "GET", "/v1/collections", &[(header, &at(&new, 3))])
            .await
            .0,
        StatusCode::OK
    );
    let (status, body) = call(&router, "GET", "/v1/collections", &[(header, &at(&old, 2))]).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("envelope_stale"))
    );
}

/// OD-004: a gateway forwards a signed-in dashboard user as a `user:`
/// principal. The backend serves it as the same session grant a standalone
/// sign-in gets: tenant data within the role's operations, never admin
/// routes, shown as a user (not an API key) in `/v1/capabilities`.
#[tokio::test]
async fn gateway_user_principals_get_the_standalone_session_scope() {
    let key = env::signer("k1", 1);
    let Backend { router, lineage } = trusted(&[&key], 1, HOUR, false);
    let header = principal_envelope::HEADER;
    let user = |tenant: &str, ops: &[credentials::Op]| {
        let mut claims = claims(Some(tenant), lineage, ops, 1);
        claims.principal = credentials::Principal::of(&credentials::Grant::for_user(
            format!("user:{}", "5a".repeat(16)),
            "alice".into(),
            Some(tenant.to_owned()),
            ops,
            lineage,
            now() + HOUR,
        ));
        key.sign(&claims).unwrap()
    };
    use credentials::Op;
    let editor = user(
        T1,
        &[
            Op::Read,
            Op::Search,
            Op::SearchBuild,
            Op::Write,
            Op::Import,
            Op::Index,
            Op::Collections,
        ],
    );
    let viewer = user(T1, &[Op::Read, Op::Search]);
    let (status, body) = call(&router, "POST", "/v1/collections", &[(header, &editor)]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = call(&router, "GET", "/v1/collections", &[(header, &viewer)]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["name"], "made");
    let (status, body) = call(&router, "POST", "/v1/collections", &[(header, &viewer)]).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden_scope"))
    );
    for token in [&editor, &viewer] {
        for path in ["/v1/admin/credentials", "/v1/admin/users", "/metrics"] {
            let (status, _) = call(&router, "GET", path, &[(header, token)]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        }
    }
    // Another tenant's signed-in user does not see T1's collection.
    let (status, body) = call(
        &router,
        "GET",
        "/v1/collections",
        &[(header, &user(T2, &[Op::Read]))],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"], json!([]));

    let (status, body) = call(&router, "GET", "/v1/capabilities", &[(header, &viewer)]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role"], "scoped");
    let identity = &body["identity"];
    assert_eq!(identity["kind"], "user", "{identity}");
    assert_eq!(identity["username"], "alice");
    assert_eq!(identity["user_id"], "5a".repeat(16));
    assert_eq!(identity["account_role"], "viewer");
    assert_eq!(identity["tenant_id"], T1);
    assert_eq!(identity["tenant_name"], "test tenant");
    assert_eq!(identity["must_change_password"], false);
    assert_eq!(identity["label"], "alice · test tenant · Viewer");
    let (_, body) = call(&router, "GET", "/v1/capabilities", &[(header, &editor)]).await;
    assert_eq!(body["identity"]["account_role"], "editor");
    // An API credential envelope is still shown as an API key.
    let api = key.sign(&claims(Some(T1), lineage, READ, 1)).unwrap();
    let (_, body) = call(&router, "GET", "/v1/capabilities", &[(header, &api)]).await;
    assert_eq!(body["identity"]["kind"], "scoped");
}

/// A `--gateway-trust-file` in `dir` with `view` (its `control_state` or
/// `control_projection` members), through the real parser. The bundle
/// holds `env::signer("k1", 1)`.
fn trust_config(dir: &std::path::Path, view: Value) -> Result<GatewayTrustConfig, String> {
    let bundle = dir.join("bundle.json");
    std::fs::write(&bundle, env::public_bundle(&[&env::signer("k1", 1)], 1)).unwrap();
    let mut file = json!({"audience": AUDIENCE, "public_key_bundle_file": bundle,
        "s_ctl_ms": HOUR, "poll_interval_ms": 10, "skew_ms": 1_000,
        "max_lifetime_ms": LIFETIME, "readiness_propagation_ms": 0});
    for (name, value) in view.as_object().unwrap() {
        file[name] = value.clone();
    }
    let path = dir.join("trust.json");
    std::fs::write(&path, file.to_string()).unwrap();
    GatewayTrustConfig::read(&path)
}

/// The `control_projection` member: `tenant`'s projection `object`.
fn projection_view(dir: &std::path::Path, object: &str, tenant: &str) -> Value {
    json!({"control_projection": {"store": {"target_file": dir.join("target.json"),
        "object": object}, "tenant": tenant}})
}

/// design-tenant-provisioning ITEM-006: the projection view is selected only
/// by the trust file's `control_projection`, exclusive with `control_state`
/// (a legacy file is unchanged), and validated before any store is
/// contacted. No field (a number, a flag) stands in for clock evidence.
#[test]
fn a_trust_file_selects_exactly_one_strictly_validated_control_view() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    let object = "control/projection-t.json";
    let state = json!({"target_file": dir.join("target.json"), "object": NAME});
    assert!(trust_config(dir, json!({ "control_state": state })).is_ok());
    assert!(trust_config(dir, projection_view(dir, object, T1)).is_ok());
    let with = |change: &dyn Fn(&mut Value)| {
        let mut view = projection_view(dir, object, T1);
        change(&mut view);
        view
    };
    for (view, expected) in [
        (
            with(&|v| v["control_state"] = state.clone()),
            "exactly one of control_state or control_projection",
        ),
        (
            json!({}),
            "exactly one of control_state or control_projection",
        ),
        (
            projection_view(dir, object, "0123"),
            "control_projection.tenant",
        ),
        (
            projection_view(dir, object, &T1.to_uppercase()),
            "control_projection.tenant",
        ),
        (
            projection_view(dir, "Control/Projection.json", T1),
            "control_projection.store.object",
        ),
        (
            with(&|v| v["control_projection"]["clock_uncertainty_ms"] = json!(500)),
            "unknown field",
        ),
        (with(&|v| v["clock_proof"] = json!(true)), "unknown field"),
        (
            json!({"control_projection": {"tenant": T1}}),
            "missing field",
        ),
    ] {
        let error = trust_config(dir, view).err().unwrap();
        assert!(
            error.starts_with("gateway_trust_invalid: ") && error.contains(expected),
            "{error}"
        );
    }
}

/// The production start of a projection backend: no approved clock-proof
/// source exists (OD-005), so it refuses before reading its projection and
/// never opens a database or serves. Its loopback store is never contacted.
#[test]
fn a_projection_backend_does_not_start_without_a_clock_proof() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    let target = json!({"account":"devstoreaccount1","container":"data",
        "coordination_container":"coord","prefix":"p/",
        "endpoint":"http://127.0.0.1:9/devstoreaccount1","auth":"emulator_shared_key"});
    std::fs::write(dir.join("target.json"), target.to_string()).unwrap();
    std::fs::write(dir.join("shared.key"), "a2V5").unwrap();
    let mut view = projection_view(dir, "control/projection-t.json", T1);
    view["control_projection"]["store"]["shared_key_file"] = json!(dir.join("shared.key"));
    let config = trust_config(dir, view).ok().unwrap();
    match GatewayTrust::start(config) {
        Ok(_) => panic!("a projection backend started without a clock proof"),
        Err(error) => assert!(
            error.starts_with("projection_freshness_unproven: "),
            "{error}"
        ),
    }
}

/// Polls `router`'s `/ready` until `done` holds (at most 5 s).
async fn ready_until(router: &Router, done: impl Fn(StatusCode, &Value) -> bool) -> Value {
    for _ in 0..500 {
        let (status, body) = call(router, "GET", "/ready", &[]).await;
        if done(status, &body) {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("/ready never reached the expected state");
}

/// design-tenant-provisioning ITEM-006 through the real trust file, start
/// and router: a backend that decides from its tenant's control projection
/// serves a pending `create` only to the operation's validator and only
/// read-only, while the control state still routes nothing to the tenant
/// (so the public gateway cannot reach it). Its poll thread follows new
/// witnesses and raises the minimum `A`, and never extends a witness by
/// rereading it. It starts only with a test-only clock proof: without one
/// (production, OD-005) even a valid published projection is refused.
#[tokio::test]
async fn a_projection_backend_serves_only_the_pending_validator_read_only() {
    use credentials::Op;
    use tenant_provisioning::projection::{ClockProof, tests as fixture};
    let key = env::signer("k1", 1);
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    let start = |clock: Option<ClockProof>| {
        let backend = service();
        let lineage = backend.lineage.unwrap();
        let memory = fixture::world(false, lineage);
        let pending = fixture::pending(AUDIENCE, lineage);
        fixture::publisher(&memory)
            .publish(fixture::T, Some(&pending), now)
            .unwrap();
        let config = trust_config(dir, projection_view(dir, fixture::NAME, fixture::T))
            .ok()
            .unwrap();
        GatewayTrust::start_projection_on(config, memory.clone(), clock).map(|trust| {
            let router = router(backend.with_gateway_trust(&trust));
            (router, trust, lineage, memory, pending)
        })
    };
    let header = principal_envelope::HEADER;
    let code = |body: &Value| body["error"]["code"].as_str().map(str::to_owned);

    match start(None) {
        Ok(_) => panic!("started without a clock proof"),
        Err(error) => assert!(
            error.starts_with("projection_freshness_unproven: "),
            "{error}"
        ),
    }

    let Ok((router, trust, lineage, memory, pending)) = start(Some(ClockProof::for_test(1_000)))
    else {
        panic!("a proven projection backend starts");
    };
    let source = Objects::new(memory.clone(), fixture::SOURCE);
    assert!(source.read().unwrap().0.record_for(fixture::T).is_none());
    let token = |id: &str, tenant: &str, ops: &[Op], a: u64| {
        key.sign(&claims_for(id, Some(tenant), lineage, ops, a))
            .unwrap()
    };
    // Generation 2 (the fixture's `create` reservation) is now the minimum.
    let validator = token(fixture::VALIDATOR, fixture::T, &[Op::Read], 2);
    let (status, _) = call(&router, "GET", "/v1/collections", &[(header, &validator)]).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(&router, "POST", "/v1/collections", &[(header, &validator)]).await;
    assert_eq!(
        (status, code(&body).as_deref()),
        (StatusCode::FORBIDDEN, Some("forbidden_scope"))
    );
    for (token, expected) in [
        (
            token(
                fixture::VALIDATOR,
                fixture::T,
                &[Op::Read, Op::Collections],
                2,
            ),
            (StatusCode::FORBIDDEN, "pending_validation_read_only"),
        ),
        (
            token(fixture::VALIDATOR, fixture::T, &[Op::Read, Op::Write], 2),
            (StatusCode::FORBIDDEN, "pending_validation_read_only"),
        ),
        (
            token("abcdefghijkl", fixture::T, &[Op::Read], 2),
            (StatusCode::UNAUTHORIZED, "envelope_invalid"),
        ),
        (
            token(fixture::VALIDATOR, T1, &[Op::Read], 2),
            (StatusCode::UNAUTHORIZED, "envelope_invalid"),
        ),
        (
            token(fixture::VALIDATOR, fixture::T, &[Op::Read], 1),
            (StatusCode::UNAUTHORIZED, "envelope_stale"),
        ),
    ] {
        let (status, body) = call(&router, "GET", "/v1/collections", &[(header, &token)]).await;
        assert_eq!(
            (status, code(&body).as_deref()),
            (expected.0, Some(expected.1))
        );
    }
    let (status, body) = call(&router, "GET", "/ready", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (&body["generation"], &body["minimum_generation"]),
        (&json!(2), &json!(2))
    );
    assert!(body.get("admission").is_none() && body.get("pending").is_none());

    // A new source generation in a new witness: the poll thread accepts it
    // and raises the minimum, so an envelope from generation 2 is stale.
    source.update(now(), |c| Ok(c.clone())).unwrap();
    fixture::publisher(&memory)
        .publish(fixture::T, Some(&pending), now)
        .unwrap();
    let body = ready_until(&router, |_, body| body["generation"] == 3).await;
    assert_eq!(body["minimum_generation"], 3);
    let (status, body) = call(&router, "GET", "/v1/collections", &[(header, &validator)]).await;
    assert_eq!(
        (status, code(&body).as_deref()),
        (StatusCode::UNAUTHORIZED, Some("envelope_stale"))
    );
    trust.stop();

    // An uncertainty leaving about 0.5 s of S_ctl: the polling backend
    // rereads the same witness, which never extends it, and fails closed.
    let Ok((router, trust, lineage, _, _)) =
        start(Some(ClockProof::for_test(fixture::S_CTL - 500)))
    else {
        panic!("a proven projection backend starts");
    };
    ready_until(&router, |status, body| {
        status == StatusCode::SERVICE_UNAVAILABLE
            && code(body).as_deref() == Some("projection_stale")
    })
    .await;
    let validator = key
        .sign(&claims_for(
            fixture::VALIDATOR,
            Some(fixture::T),
            lineage,
            &[Op::Read],
            2,
        ))
        .unwrap();
    let (status, body) = call(&router, "GET", "/v1/collections", &[(header, &validator)]).await;
    assert_eq!(
        (status, code(&body).as_deref()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("projection_stale"))
    );
    trust.stop();
}
