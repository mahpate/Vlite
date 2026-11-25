//! OD-004: dashboard sign-in and sessions through gateway replicas
//! (`users::gateway_session`), end to end through the gateway router.
use super::tests::{TENANT, Upstream, error_code, gateway_on, gateway_with_dashboard, upstream};
use super::*;
use base64::prelude::{BASE64_STANDARD_NO_PAD, BASE64_URL_SAFE_NO_PAD};
use std::sync::OnceLock;
use tower::ServiceExt;

const ALICE: &str = "11111111111111111111111111111111";
const BOB: &str = "22222222222222222222222222222222";
const PASSWORD: &str = "correct horse battery staple";
const KEY: [u8; 32] = [42; 32];

fn hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| gateway_session::hash_for_test(PASSWORD))
}

/// Alice edits and Bob views the test tenant.
fn accounts() -> Vec<Value> {
    vec![
        gateway_session::account_for_test(ALICE, "alice", "editor", Some(TENANT), hash()),
        gateway_session::account_for_test(BOB, "bob", "viewer", Some(TENANT), hash()),
    ]
}

/// Two gateway replicas on one control state. Each loads the users export
/// and session key itself; they share no session state.
struct Replicas {
    a: Arc<Gateway>,
    b: Arc<Gateway>,
    token: String,
    objects: control_state::Objects,
    backend: Upstream,
}

async fn replicas(users: &[Value], extra: Value, loopback: bool) -> Replicas {
    let backend = upstream(StatusCode::OK, HeaderMap::new(), Duration::ZERO).await;
    let mut record = control_state::Route::shared_for_test(TENANT);
    record.writer = backend.endpoint.clone();
    let directory = tempfile::tempdir().unwrap();
    let load = || {
        gateway_session::load_for_test(directory.path(), users, &KEY, extra.clone(), loopback)
            .unwrap()
    };
    let (a, token, objects) = gateway_with_dashboard(record, Some(load()));
    let b = gateway_on(a.feed.clone(), Some(load()));
    Replicas {
        a,
        b,
        token,
        objects,
        backend,
    }
}

impl Replicas {
    /// Another replica with its own users export and session key.
    fn other(&self, users: &[Value], key: &[u8]) -> Arc<Gateway> {
        self.rotating(users, key, &[])
    }

    /// Another replica that signs with `key` and also accepts `accepted`.
    fn rotating(&self, users: &[Value], key: &[u8], accepted: &[&[u8]]) -> Arc<Gateway> {
        let directory = tempfile::tempdir().unwrap();
        let files: Vec<_> = accepted
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let path = directory.path().join(format!("accepted-{index}.key"));
                std::fs::write(&path, BASE64_STANDARD_NO_PAD.encode(key)).unwrap();
                path
            })
            .collect();
        let sessions = gateway_session::load_for_test(
            directory.path(),
            users,
            key,
            json!({"accepted_session_key_files": files}),
            true,
        )
        .unwrap();
        gateway_on(self.a.feed.clone(), Some(sessions))
    }

    fn forwarded(&self) -> usize {
        self.backend.requests.lock().unwrap().len()
    }

    /// A sealed cookie, as a browser sends it, with arbitrary claims for an
    /// account created at 1 (as `account_for_test` creates them).
    fn forged(&self, user: &str, version: u64, issued: u64, seen: u64, generation: u64) -> String {
        self.forged_created(user, version, 1, issued, seen, generation)
    }

    fn forged_created(
        &self,
        user: &str,
        version: u64,
        created: u64,
        issued: u64,
        seen: u64,
        generation: u64,
    ) -> String {
        let sessions = self.b.dashboard.as_ref().unwrap();
        format!(
            "vlite_session={}",
            sessions.cookie_for_test(user, version, created, TENANT, issued, seen, generation)
        )
    }

    fn generation(&self) -> u64 {
        self.a.feed.current().unwrap().generation()
    }
}

/// Same-origin dashboard request headers (`X-VLite-Request` as `app.js`
/// sends it) with an optional session cookie.
fn browser(cookie: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("sec-fetch-site", "same-origin".to_owned()),
        ("x-vlite-request", "1".to_owned()),
    ];
    headers.extend(cookie.map(|cookie| ("cookie", cookie.to_owned())));
    headers
}

async fn call(
    gateway: &Arc<Gateway>,
    method: Method,
    uri: &str,
    headers: &[(&'static str, String)],
    body: Option<Value>,
) -> Response {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    router(gateway.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

async fn login(gateway: &Arc<Gateway>, headers: &[(&'static str, String)], user: &str) -> Response {
    call(
        gateway,
        Method::POST,
        "/v1/auth/login",
        headers,
        Some(json!({"username": user, "password": PASSWORD})),
    )
    .await
}

fn set_cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect()
}

/// The `name=value` a browser returns for the response's one Set-Cookie.
fn cookie_of(response: &Response) -> String {
    let set = set_cookies(response);
    assert_eq!(set.len(), 1, "{set:?}");
    set[0].split(';').next().unwrap().to_owned()
}

async fn signed_in(gateway: &Arc<Gateway>, user: &str) -> String {
    let response = login(gateway, &browser(None), user).await;
    assert_eq!(response.status(), StatusCode::OK);
    cookie_of(&response)
}

async fn body_of(response: Response) -> Value {
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn no_store(response: &Response) -> bool {
    response
        .headers()
        .get(header::CACHE_CONTROL)
        .map(|v| v.as_bytes())
        == Some(b"no-store")
}

/// A 401 `session_expired` that clears both cookie variants.
async fn assert_ended(response: Response) {
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let set = set_cookies(&response);
    assert_eq!(set.len(), 2, "{set:?}");
    assert!(set.iter().all(|cookie| cookie.contains("Max-Age=0")));
    assert!(no_store(&response));
    assert_eq!(error_code(response).await, "session_expired");
}

/// A refusal that keeps the session: no Set-Cookie.
async fn assert_kept(response: Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    assert!(set_cookies(&response).is_empty());
    assert_eq!(error_code(response).await, code);
}

/// The principal claims of the envelope the backend received.
fn principal_of(headers: &HeaderMap) -> Value {
    let envelope = headers
        .get(principal_envelope::HEADER)
        .unwrap()
        .to_str()
        .unwrap();
    let payload = envelope.split('.').nth(2).unwrap();
    let claims: Value =
        serde_json::from_slice(&BASE64_URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    claims["principal"].clone()
}

#[tokio::test]
async fn dashboard_sign_in_on_one_replica_reads_tenant_data_on_another() {
    let replicas = replicas(&accounts(), json!({}), true).await;
    let page = call(&replicas.a, Method::GET, "/dashboard", &[], None).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(
        page.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(
        page.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    let config = call(&replicas.a, Method::GET, "/v1/auth/config", &[], None).await;
    assert_eq!(config.status(), StatusCode::OK);
    assert!(no_store(&config));
    let config = body_of(config).await;
    assert_eq!(
        (
            config["password_login"].as_bool(),
            config["setup_required"].as_bool()
        ),
        (Some(true), Some(false))
    );

    let response = login(&replicas.a, &browser(None), "alice").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(no_store(&response));
    let set = set_cookies(&response);
    assert_eq!(set.len(), 1, "{set:?}");
    assert!(set[0].starts_with("vlite_session=v1."), "{set:?}");
    for flag in ["Path=/", "HttpOnly", "SameSite=Strict"] {
        assert!(set[0].contains(flag), "{set:?}");
    }
    assert!(
        !set[0].contains("Secure"),
        "a loopback cookie has no Secure: {set:?}"
    );
    let cookie = cookie_of(&response);
    let signed = body_of(response).await;
    assert_eq!(signed["user"]["id"], ALICE);
    assert_eq!(signed["user"]["role"], "editor");
    assert_eq!(signed["user"]["tenant_name"], "test tenant");
    assert!(signed["user"].get("password").is_none());
    assert_eq!(signed["must_change_password"], false);

    // Replica B never saw the sign-in.
    let session = call(
        &replicas.b,
        Method::GET,
        "/v1/auth/session",
        &browser(Some(&cookie)),
        None,
    )
    .await;
    assert_eq!(session.status(), StatusCode::OK);
    assert!(no_store(&session));
    let session = body_of(session).await;
    assert_eq!(
        (session["method"].as_str(), session["role"].as_str()),
        (Some("password"), Some("scoped"))
    );
    assert_eq!(session["user"]["username"], "alice");
    assert_eq!(
        replicas.forwarded(),
        0,
        "sign-in and session stay on the gateway"
    );

    for (method, uri, body) in [
        (Method::GET, "/v1/capabilities", None),
        (Method::GET, "/v1/collections", None),
        (
            Method::POST,
            "/v1/collections/docs/search",
            Some(json!({"vector": [1.0], "k": 1})),
        ),
    ] {
        let response = call(&replicas.b, method, uri, &browser(Some(&cookie)), body).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert!(set_cookies(&response).is_empty(), "{uri}");
    }
    let requests = replicas.backend.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for (headers, _) in requests.iter() {
        assert!(!headers.contains_key(header::COOKIE));
        assert!(!headers.contains_key(header::AUTHORIZATION));
        let principal = principal_of(headers);
        assert_eq!(principal["id"], format!("user:{ALICE}"));
        assert_eq!(principal["tenant_id"], TENANT);
        assert_eq!(principal["all"], true);
        assert_eq!(principal["lineage"], "0000000000001234");
        assert!(principal["requests_per_second"].is_null());
        let ops = principal["operations"].as_array().unwrap();
        assert!(ops.contains(&json!("write")) && !ops.contains(&json!("diagnostics")));
    }
}

#[tokio::test]
async fn dashboard_sign_in_refuses_wrong_passwords_and_cross_site_requests() {
    let replicas = replicas(&accounts(), json!({}), true).await;
    for (user, password) in [("alice", "wrong password here"), ("nobody", PASSWORD)] {
        let response = call(
            &replicas.a,
            Method::POST,
            "/v1/auth/login",
            &browser(None),
            Some(json!({"username": user, "password": password})),
        )
        .await;
        assert!(no_store(&response));
        assert_kept(response, StatusCode::UNAUTHORIZED, "invalid_credentials").await;
    }
    let missing_header = vec![("sec-fetch-site", "same-origin".to_owned())];
    let cross_site = vec![
        ("sec-fetch-site", "cross-site".to_owned()),
        ("x-vlite-request", "1".to_owned()),
    ];
    for headers in [&missing_header, &cross_site] {
        let response = login(&replicas.a, headers, "alice").await;
        assert_kept(response, StatusCode::FORBIDDEN, "csrf_rejected").await;
    }

    let cookie = signed_in(&replicas.a, "alice").await;
    let search = Some(json!({"vector": [1.0], "k": 1}));
    let mut no_header = missing_header.clone();
    no_header.push(("cookie", cookie.clone()));
    let response = call(
        &replicas.b,
        Method::POST,
        "/v1/collections/docs/search",
        &no_header,
        search.clone(),
    )
    .await;
    assert_kept(response, StatusCode::FORBIDDEN, "csrf_rejected").await;
    let mut from_elsewhere = cross_site.clone();
    from_elsewhere.push(("cookie", cookie.clone()));
    let response = call(
        &replicas.b,
        Method::GET,
        "/v1/collections",
        &from_elsewhere,
        None,
    )
    .await;
    assert_kept(response, StatusCode::FORBIDDEN, "csrf_rejected").await;
    assert_eq!(replicas.forwarded(), 0);
}

#[tokio::test]
async fn dashboard_sessions_end_on_key_or_account_changes_idle_and_lifetime() {
    let replicas = replicas(&accounts(), json!({}), true).await;
    let cookie = signed_in(&replicas.a, "alice").await;
    let foreign = replicas.other(&accounts(), &[7; 32]);
    let mut changed = accounts();
    changed[0]["version"] = json!(2);
    let newer = replicas.other(&changed, &KEY);
    for gateway in [&foreign, &newer] {
        let session = call(
            gateway,
            Method::GET,
            "/v1/auth/session",
            &browser(Some(&cookie)),
            None,
        )
        .await;
        assert_ended(session).await;
        let data = call(
            gateway,
            Method::GET,
            "/v1/collections",
            &browser(Some(&cookie)),
            None,
        )
        .await;
        assert_ended(data).await;
    }

    // A replica that has not yet loaded the newer export, or read the
    // control state the session was served at, refuses without ending it.
    let now = admin::now_ms() as u64;
    let generation = replicas.generation();
    let ahead = replicas.forged(ALICE, 2, now, now, generation);
    let response = call(
        &replicas.b,
        Method::GET,
        "/v1/collections",
        &browser(Some(&ahead)),
        None,
    )
    .await;
    assert_kept(
        response,
        StatusCode::SERVICE_UNAVAILABLE,
        "dashboard_users_behind",
    )
    .await;
    let ahead = replicas.forged(ALICE, 1, now, now, generation + 1);
    let response = call(
        &replicas.b,
        Method::GET,
        "/v1/auth/session",
        &browser(Some(&ahead)),
        None,
    )
    .await;
    assert_kept(
        response,
        StatusCode::SERVICE_UNAVAILABLE,
        "control_state_behind",
    )
    .await;
    // An account created after this replica's users file is one it has
    // not loaded yet: refused, not ended.
    let created_later = replicas.forged_created(
        "33333333333333333333333333333333",
        1,
        now,
        now,
        now,
        generation,
    );
    let response = call(
        &replicas.b,
        Method::GET,
        "/v1/auth/session",
        &browser(Some(&created_later)),
        None,
    )
    .await;
    assert_kept(
        response,
        StatusCode::SERVICE_UNAVAILABLE,
        "dashboard_users_behind",
    )
    .await;

    let idle = replicas.b.dashboard.as_ref().unwrap().idle_ms_for_test();
    let idled = replicas.forged(ALICE, 1, now - idle - 1, now - idle - 1, generation);
    // Past the longest configurable lifetime, however recently seen.
    let lifetime = 25 * 3600 * 1000;
    let expired = replicas.forged(ALICE, 1, now - lifetime - 1, now - 1_000, generation);
    // Removed: unknown, and created no later than the newest account here.
    let unknown = replicas.forged("33333333333333333333333333333333", 1, now, now, generation);
    // Under a key id no replica holds.
    let unknown_key = {
        let (_, rest) = ahead.split_once("v1.").unwrap();
        let (_, sealed) = rest.split_once('.').unwrap();
        format!("vlite_session=v1.0000000000000000.{sealed}")
    };
    for stale in [
        idled,
        expired,
        unknown,
        unknown_key,
        "vlite_session=v1.e30.AAAA".to_owned(),
    ] {
        let response = call(
            &replicas.b,
            Method::GET,
            "/v1/collections",
            &browser(Some(&stale)),
            None,
        )
        .await;
        assert_ended(response).await;
    }
    assert_eq!(replicas.forwarded(), 0);

    // Activity re-issues the cookie once a minute; the new one still works.
    let quiet = replicas.forged(ALICE, 1, now - 120_000, now - 120_000, generation);
    let response = call(
        &replicas.b,
        Method::GET,
        "/v1/collections",
        &browser(Some(&quiet)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let refreshed = cookie_of(&response);
    assert_ne!(refreshed, quiet);
    let response = call(
        &replicas.a,
        Method::GET,
        "/v1/auth/session",
        &browser(Some(&refreshed)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(set_cookies(&response).is_empty());
    assert_eq!(replicas.forwarded(), 1);
}

/// Rotating the session key one replica at a time: (1) every replica
/// accepts the new key, (2) each signs with it and accepts the old one,
/// (3) the old key is dropped. Sessions survive every step; only a cookie
/// under a key a replica no longer holds ends.
#[tokio::test]
async fn dashboard_sessions_survive_a_rolling_session_key_rotation() {
    const NEW: [u8; 32] = [43; 32];
    let replicas = replicas(&accounts(), json!({}), true).await;
    let old_cookie = signed_in(&replicas.a, "alice").await;
    let step1 = replicas.rotating(&accounts(), &KEY, &[&NEW]);
    let step2 = replicas.rotating(&accounts(), &NEW, &[&KEY]);
    let step3 = replicas.other(&accounts(), &NEW);
    let new_cookie = signed_in(&step2, "alice").await;
    let session = |gateway: &Arc<Gateway>, cookie: &str| {
        let gateway = gateway.clone();
        let headers = browser(Some(cookie));
        async move { call(&gateway, Method::GET, "/v1/auth/session", &headers, None).await }
    };
    for (gateway, cookie) in [
        (&step1, &old_cookie),
        (&step1, &new_cookie),
        (&step2, &old_cookie),
        (&step2, &new_cookie),
        (&step3, &new_cookie),
    ] {
        let response = session(gateway, cookie).await;
        assert_eq!(response.status(), StatusCode::OK, "{cookie}");
    }
    // A replica not yet at step 1 cannot read a step-2 cookie, which is why
    // every replica must accept the new key before any signs with it.
    assert_ended(session(&replicas.a, &new_cookie).await).await;
    assert_ended(session(&step3, &old_cookie).await).await;
    assert_eq!(replicas.forwarded(), 0);
}

#[tokio::test]
async fn dashboard_users_keep_their_role_and_never_reach_admin_routes() {
    let replicas = replicas(&accounts(), json!({}), true).await;
    let viewer = signed_in(&replicas.a, "bob").await;
    let response = call(
        &replicas.b,
        Method::POST,
        "/v1/collections",
        &browser(Some(&viewer)),
        Some(json!({"name": "new", "dimension": 3})),
    )
    .await;
    assert_kept(response, StatusCode::FORBIDDEN, "forbidden_scope").await;
    let editor = signed_in(&replicas.a, "alice").await;
    for uri in ["/v1/admin/users", "/metrics", "/v1/database"] {
        let response = call(&replicas.b, Method::GET, uri, &browser(Some(&editor)), None).await;
        assert_kept(response, StatusCode::FORBIDDEN, "forbidden_scope").await;
    }
    let response = call(
        &replicas.b,
        Method::POST,
        "/v1/auth/password",
        &browser(Some(&editor)),
        Some(json!({"current_password": PASSWORD, "new_password": "another long passphrase"})),
    )
    .await;
    assert_kept(
        response,
        StatusCode::CONFLICT,
        "password_change_unavailable",
    )
    .await;
    assert_eq!(replicas.forwarded(), 0);
    let response = call(
        &replicas.b,
        Method::POST,
        "/v1/collections/docs/search",
        &browser(Some(&viewer)),
        Some(json!({"vector": [1.0], "k": 1})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let requests = replicas.backend.requests.lock().unwrap();
    let ops = principal_of(&requests[0].0)["operations"].clone();
    assert_eq!(ops, json!(["read", "search", "graph_read"]));
}

#[tokio::test]
async fn dashboard_logout_suspension_and_unusable_accounts() {
    let mut users = accounts();
    users.push(gateway_session::account_for_test(
        "33333333333333333333333333333333",
        "carol",
        "viewer",
        Some(TENANT),
        hash(),
    ));
    users[2]["must_change_password"] = json!(true);
    users.push(gateway_session::account_for_test(
        "44444444444444444444444444444444",
        "dave",
        "viewer",
        Some(TENANT),
        hash(),
    ));
    users[3]["disabled"] = json!(true);
    let replicas = replicas(&users, json!({}), true).await;
    for (user, code) in [
        ("carol", "password_change_required"),
        ("dave", "account_disabled"),
    ] {
        let response = login(&replicas.a, &browser(None), user).await;
        assert_kept(response, StatusCode::FORBIDDEN, code).await;
    }

    let cookie = signed_in(&replicas.a, "alice").await;
    let only_cookie = vec![("cookie", cookie.clone())];
    let response = call(
        &replicas.b,
        Method::POST,
        "/v1/auth/logout",
        &only_cookie,
        None,
    )
    .await;
    assert_kept(response, StatusCode::FORBIDDEN, "csrf_rejected").await;
    let response = call(
        &replicas.b,
        Method::POST,
        "/v1/auth/logout",
        &browser(Some(&cookie)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let set = set_cookies(&response);
    assert_eq!(set.len(), 2, "{set:?}");
    assert!(set.iter().all(|cookie| cookie.contains("Max-Age=0")));
    assert_eq!(body_of(response).await["signed_out"], true);

    let cookie = signed_in(&replicas.a, "alice").await;
    let now = admin::now_ms() as u64;
    replicas
        .objects
        .update(now, |snapshot| {
            snapshot.clone().with_suspended(TENANT, true, now)
        })
        .unwrap();
    replicas.a.feed.poll_once().unwrap();
    let response = call(
        &replicas.b,
        Method::GET,
        "/v1/collections",
        &browser(Some(&cookie)),
        None,
    )
    .await;
    assert_kept(response, StatusCode::FORBIDDEN, "tenant_suspended").await;
    let response = login(&replicas.a, &browser(None), "alice").await;
    assert_kept(response, StatusCode::FORBIDDEN, "tenant_suspended").await;
    assert_eq!(replicas.forwarded(), 0);
}

#[tokio::test]
async fn dashboard_cookies_need_a_loopback_bind_or_a_trusted_https_proxy() {
    let insecure = replicas(&accounts(), json!({}), false).await;
    let config = body_of(call(&insecure.a, Method::GET, "/v1/auth/config", &[], None).await).await;
    assert_eq!(
        (
            config["password_login"].as_bool(),
            config["reason"].as_str()
        ),
        (Some(false), Some("insecure_transport"))
    );
    let response = login(&insecure.a, &browser(None), "alice").await;
    assert_kept(response, StatusCode::FORBIDDEN, "insecure_transport").await;

    let proxied = replicas(&accounts(), json!({"trusted_proxy": true}), false).await;
    let mut headers = browser(None);
    headers.push(("x-forwarded-proto", "https".to_owned()));
    let response = login(&proxied.a, &headers, "alice").await;
    assert_eq!(response.status(), StatusCode::OK);
    let set = set_cookies(&response);
    assert!(set[0].starts_with("__Host-vlite_session=v1."), "{set:?}");
    assert!(set[0].contains("; Secure"), "{set:?}");
}

#[tokio::test]
async fn dashboard_bearer_requests_and_gateways_without_a_dashboard_are_unchanged() {
    let replicas = replicas(&accounts(), json!({}), true).await;
    let cookie = signed_in(&replicas.a, "alice").await;
    let mut both = browser(Some(&cookie));
    both.push(("authorization", format!("Bearer {}", replicas.token)));
    for uri in ["/v1/collections", "/v1/auth/session"] {
        let response = call(&replicas.b, Method::GET, uri, &both, None).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }
    {
        let requests = replicas.backend.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "bearer requests reach the backend");
        for (headers, _) in requests.iter() {
            assert!(
                !principal_of(headers)["id"]
                    .as_str()
                    .unwrap()
                    .starts_with("user:")
            );
        }
    }

    let mut record = control_state::Route::shared_for_test(TENANT);
    record.writer = replicas.backend.endpoint.clone();
    let (plain, _, _) = gateway_with_dashboard(record, None);
    for (method, uri) in [
        (Method::GET, "/dashboard"),
        (Method::GET, "/dashboard/app.js"),
        (Method::GET, "/v1/auth/config"),
        (Method::POST, "/v1/auth/login"),
    ] {
        let response = call(&plain, method, uri, &browser(None), Some(json!({}))).await;
        assert_eq!(no_store(&response), uri.starts_with("/v1/auth/"), "{uri}");
        assert_kept(response, StatusCode::NOT_FOUND, "not_found").await;
    }
    for uri in ["/v1/collections", "/v1/auth/session"] {
        let response = call(&plain, Method::GET, uri, &browser(Some(&cookie)), None).await;
        assert_eq!(no_store(&response), uri.starts_with("/v1/auth/"), "{uri}");
        assert_kept(response, StatusCode::UNAUTHORIZED, "unauthorized").await;
    }
    let response = call(
        &replicas.a,
        Method::GET,
        "/v1/collections",
        &browser(None),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let message = body_of(response).await["error"]["message"].clone();
    assert!(
        message
            .as_str()
            .unwrap()
            .contains("dashboard session cookie"),
        "{message}"
    );
    assert_eq!(replicas.forwarded(), 2);
}

/// The whole data path in one process: a browser session on one replica,
/// the gateway's signed envelope over HTTP, and a real gateway-trusted
/// backend (`server::router`) that serves it as the standalone session scope
/// on the shared control state.
#[tokio::test]
async fn dashboard_sessions_reach_tenant_data_on_a_real_trusted_backend() {
    use super::super::principal_envelope::{Policy, Verifier};
    let mut service = super::super::Service::new(
        crate::Database::create_in_memory(1, crate::CreateOptions::default()).unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut record = control_state::Route::shared_for_test(TENANT);
    record.writer = format!("http://{}", listener.local_addr().unwrap());
    record.audience = "backend-a".into();
    record.lineage = service.lineage.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let load = || {
        gateway_session::load_for_test(directory.path(), &accounts(), &KEY, json!({}), true)
            .unwrap()
    };
    // A debug-build backend may take longer than the helpers' 100 ms.
    let patient = |gateway: Arc<Gateway>| {
        let mut gateway = Arc::into_inner(gateway).unwrap();
        gateway.upstream_timeout = Duration::from_secs(30);
        Arc::new(gateway)
    };
    let (a, _, _) = gateway_with_dashboard(record, Some(load()));
    let a = patient(a);
    let b = patient(gateway_on(a.feed.clone(), Some(load())));
    let verifier = Verifier::new(
        vec![("gw-1".to_owned(), a.signer.public_key())],
        Policy {
            audience: "backend-a".into(),
            tenant: None,
            skew_ms: 1_000,
            max_lifetime_ms: 60_000,
            minimum_generation: 1,
        },
    )
    .unwrap();
    service.trust = Some(Arc::new(super::super::gateway_trust::Trust::for_test(
        verifier,
        a.feed.clone(),
        "backend-a",
    )));
    let backend = super::super::router(service);
    let task = tokio::spawn(async move { axum::serve(listener, backend).await.unwrap() });

    let alice = signed_in(&a, "alice").await;
    let made = call(
        &b,
        Method::POST,
        "/v1/collections",
        &browser(Some(&alice)),
        Some(json!({"name": "made", "dimensions": 2})),
    )
    .await;
    assert_eq!(made.status(), StatusCode::CREATED);
    let bob = signed_in(&b, "bob").await;
    let listed = call(
        &a,
        Method::GET,
        "/v1/collections",
        &browser(Some(&bob)),
        None,
    )
    .await;
    assert_eq!(listed.status(), StatusCode::OK);
    assert_eq!(body_of(listed).await["items"][0]["name"], "made");
    let refused = call(
        &a,
        Method::POST,
        "/v1/collections",
        &browser(Some(&bob)),
        Some(json!({"name": "other", "dimensions": 2})),
    )
    .await;
    assert_kept(refused, StatusCode::FORBIDDEN, "forbidden_scope").await;
    let capabilities = call(
        &b,
        Method::GET,
        "/v1/capabilities",
        &browser(Some(&bob)),
        None,
    )
    .await;
    assert_eq!(capabilities.status(), StatusCode::OK);
    let capabilities = body_of(capabilities).await;
    assert_eq!(capabilities["role"], "scoped");
    let identity = &capabilities["identity"];
    assert_eq!(
        (identity["kind"].as_str(), identity["username"].as_str()),
        (Some("user"), Some("bob"))
    );
    assert_eq!(identity["tenant_id"], TENANT);
    assert_eq!(identity["account_role"], "viewer");
    let capabilities = call(
        &a,
        Method::GET,
        "/v1/capabilities",
        &browser(Some(&alice)),
        None,
    )
    .await;
    assert_eq!(capabilities.status(), StatusCode::OK);
    assert_eq!(
        body_of(capabilities).await["identity"]["account_role"],
        "editor"
    );
    for uri in ["/v1/admin/users", "/metrics"] {
        let response = call(&a, Method::GET, uri, &browser(Some(&alice)), None).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
    }
    task.abort();
}

/// A replica that answers only Alice and Bob, built fresh so its limits can
/// still be changed (`Arc::get_mut` before the first clone).
fn limited(
    replicas: &Replicas,
    in_flight: usize,
    login_slots: usize,
    timeout: Duration,
) -> Arc<Gateway> {
    let directory = tempfile::tempdir().unwrap();
    let sessions =
        gateway_session::load_for_test(directory.path(), &accounts(), &KEY, json!({}), true)
            .unwrap();
    let mut gateway = gateway_on(replicas.a.feed.clone(), Some(sessions));
    let limits = Arc::get_mut(&mut gateway).unwrap();
    limits.in_flight = Arc::new(Semaphore::new(in_flight));
    limits.login_slots = Arc::new(Semaphore::new(login_slots));
    limits.upstream_timeout = timeout;
    gateway
}

/// A same-origin sign-in whose body arrives only as the test sends it.
fn streamed_login(
    gateway: &Arc<Gateway>,
    body: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
) -> tokio::task::JoinHandle<Response> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri("/v1/auth/login")
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in browser(None) {
        request = request.header(name, value);
    }
    let request = request
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(body),
        ))
        .unwrap();
    let router = router(gateway.clone());
    tokio::spawn(async move { router.oneshot(request).await.unwrap() })
}

/// Sign-in has its own slots: a stalled body never holds an API slot, and
/// when every sign-in slot is busy only sign-in is refused.
#[tokio::test]
async fn a_stalled_sign_in_holds_no_api_slot_and_busy_sign_in_slots_refuse_only_sign_in() {
    let replicas = replicas(&accounts(), json!({}), true).await;
    let gateway = limited(&replicas, 1, 1, Duration::from_secs(5));
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    sender
        .send(Ok(Bytes::from_static(b"{\"username\":\"al")))
        .await
        .unwrap();
    let stalled = streamed_login(&gateway, receiver);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let bearer = vec![("authorization", format!("Bearer {}", replicas.token))];
    let response = call(&gateway, Method::GET, "/v1/collections", &bearer, None).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the only API slot is free"
    );
    assert_eq!(replicas.forwarded(), 1);
    let response = login(&gateway, &browser(None), "alice").await;
    assert!(no_store(&response));
    assert_kept(response, StatusCode::SERVICE_UNAVAILABLE, "overloaded").await;

    drop(sender);
    let response = tokio::time::timeout(Duration::from_secs(2), stalled)
        .await
        .expect("an ended body finishes the sign-in")
        .unwrap();
    assert!(set_cookies(&response).is_empty());
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // The sign-in slot is free again.
    signed_in(&gateway, "alice").await;
}

/// The sign-in body deadline covers the whole body, so a byte at a time
/// (each well inside the per-piece wait) cannot stretch it.
#[tokio::test]
async fn a_trickled_sign_in_body_times_out_as_a_whole() {
    let replicas = replicas(&accounts(), json!({}), true).await;
    let gateway = limited(&replicas, 8, 1, Duration::from_millis(100));
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let trickle = tokio::spawn(async move {
        for byte in br#"{"username":"alice","password":"correct horse battery staple"}"# {
            tokio::time::sleep(Duration::from_millis(40)).await;
            if sender.send(Ok(Bytes::from(vec![*byte]))).await.is_err() {
                return;
            }
        }
    });
    let response = tokio::time::timeout(Duration::from_secs(2), streamed_login(&gateway, receiver))
        .await
        .expect("a trickled body is refused promptly")
        .unwrap();
    assert!(set_cookies(&response).is_empty());
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    assert_eq!(error_code(response).await, "request_body_timeout");
    trickle.abort();
    signed_in(&gateway, "alice").await;
}

/// Over HTTPS (as a trusted proxy reports it) only the `__Host-` cookie
/// counts: a plain `vlite_session` with the same sealed value, which a
/// sibling host or plain HTTP could plant, is no session at all.
#[tokio::test]
async fn a_plain_session_cookie_is_ignored_over_trusted_proxy_https() {
    let replicas = replicas(&accounts(), json!({"trusted_proxy": true}), false).await;
    let https = |cookie: Option<&str>| {
        let mut headers = browser(cookie);
        headers.push(("x-forwarded-proto", "https".to_owned()));
        headers
    };
    let response = login(&replicas.a, &https(None), "alice").await;
    assert_eq!(response.status(), StatusCode::OK);
    let host = cookie_of(&response);
    let sealed = host.strip_prefix("__Host-vlite_session=").unwrap();
    let plain = format!("vlite_session={sealed}");
    let response = call(
        &replicas.b,
        Method::GET,
        "/v1/auth/session",
        &https(Some(&plain)),
        None,
    )
    .await;
    assert_ended(response).await;
    let response = call(
        &replicas.b,
        Method::GET,
        "/v1/collections",
        &https(Some(&plain)),
        None,
    )
    .await;
    assert_kept(response, StatusCode::UNAUTHORIZED, "unauthorized").await;
    assert_eq!(replicas.forwarded(), 0);
    let response = call(
        &replicas.b,
        Method::GET,
        "/v1/auth/session",
        &https(Some(&host)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_of(response).await["user"]["username"], "alice");
}
