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
    let now = now();
    Claims {
        aud: AUDIENCE.into(),
        iat: now,
        exp: now + 30_000,
        generation,
        principal: credentials::Principal::of(&credentials::Grant::for_user(
            "abcdefghijkl".into(),
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
        audience: AUDIENCE.into(),
        lineage,
        ..Route::shared_for_test(tenant)
    };
    let directory = vec![
        here(T1),
        here(T2),
        Route {
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
