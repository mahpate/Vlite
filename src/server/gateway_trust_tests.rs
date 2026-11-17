//! Gateway-trusted backend mode through the real router (feature-aca-ha
//! ITEM-010/012: TEST-010, TEST-004 backend side, TEST-014 skew).
use super::principal_envelope::{Claims, Policy, Signer, Verifier, tests as env};
use super::*;
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

const AUDIENCE: &str = "backend-a";
const LIFETIME: u64 = 60_000;

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

fn claims(lineage: u64, ops: &[credentials::Op], generation: u64) -> Claims {
    let now = now();
    Claims {
        aud: AUDIENCE.into(),
        iat: now,
        exp: now + 30_000,
        generation,
        principal: credentials::Principal::of(&credentials::Grant::for_user(
            "abcdefghijkl".into(),
            "gateway principal".into(),
            None,
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

fn trusted(keys: &[&Signer], minimum: u64) -> (Router, u64) {
    let service = service();
    let lineage = service.lineage.unwrap();
    (
        router(service.with_gateway_trust(verifier(keys, minimum))),
        lineage,
    )
}

const READ: &[credentials::Op] = &[credentials::Op::Read, credentials::Op::Collections];

#[tokio::test]
async fn standalone_default_is_unchanged() {
    let router = router(service());
    let (status, _) = call(&router, "GET", "/v1/collections", &[]).await;
    assert_eq!(status, StatusCode::OK);
    let key = env::signer("k1", 1);
    let token = key.sign(&claims(1, READ, 1)).unwrap();
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
/// authentication in trusted mode; only a valid envelope is.
#[tokio::test]
async fn trusted_mode_requires_a_valid_envelope() {
    let key = env::signer("k1", 1);
    let impostor = env::signer("k1", 2);
    let (router, lineage) = trusted(&[&key], 2);
    let header = principal_envelope::HEADER;

    let (status, body) = call(&router, "GET", "/v1/collections", &[]).await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("unauthorized"))
    );
    let (status, _) = call(
        &router,
        "GET",
        "/v1/collections",
        &[("authorization", "Bearer anything")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &router,
        "GET",
        "/v1/collections",
        &[("cookie", "vlite_session=x")],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let valid = key.sign(&claims(lineage, READ, 2)).unwrap();
    let mut expired = claims(lineage, READ, 2);
    expired.iat -= 40_000;
    expired.exp -= 40_000;
    let mut audience = claims(lineage, READ, 2);
    audience.aud = "backend-b".into();
    let cases = [
        ("garbage".to_owned(), "envelope_invalid"),
        (
            impostor.sign(&claims(lineage, READ, 2)).unwrap(),
            "envelope_invalid",
        ),
        (
            env::signer("k9", 1)
                .sign(&claims(lineage, READ, 2))
                .unwrap(),
            "envelope_invalid",
        ),
        (key.sign(&audience).unwrap(), "envelope_invalid"),
        (key.sign(&expired).unwrap(), "envelope_expired"),
        (
            key.sign(&claims(lineage, READ, 1)).unwrap(),
            "envelope_stale",
        ),
    ];
    for (token, code) in &cases {
        let (status, body) = call(&router, "GET", "/v1/collections", &[(header, token)]).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some(*code))
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
/// guards: operation scope, admin-only routes and the lineage binding.
#[tokio::test]
async fn envelope_principals_are_authorized_like_scoped_credentials() {
    let key = env::signer("k1", 1);
    let (router, lineage) = trusted(&[&key], 1);
    let header = principal_envelope::HEADER;
    let reader = key
        .sign(&claims(lineage, &[credentials::Op::Read], 1))
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

    let writer = key.sign(&claims(lineage, READ, 1)).unwrap();
    let (status, _) = call(&router, "POST", "/v1/collections", &[(header, &writer)]).await;
    assert_eq!(status, StatusCode::CREATED);

    let other_lineage = key.sign(&claims(lineage ^ 1, READ, 1)).unwrap();
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
}

/// RD-004 through the router: once a backend has accepted `A = 3` it
/// refuses an envelope minted from an older snapshot, and old and new keys
/// both verify during a rotation overlap.
#[tokio::test]
async fn backend_holds_the_highest_generation_and_accepts_rotation_overlap() {
    let old = env::signer("k1", 1);
    let new = env::signer("k2", 2);
    let (router, lineage) = trusted(&[&old, &new], 1);
    let header = principal_envelope::HEADER;
    let at = |key: &Signer, a| key.sign(&claims(lineage, READ, a)).unwrap();
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
