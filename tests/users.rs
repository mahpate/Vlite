#![cfg(feature = "server")]
//! Dashboard accounts: password sign-in, cookie sessions, and the super-admin
//! account lifecycle.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::Path;
use tower::ServiceExt;
use vlite::{
    Database, FormatVersion,
    server::{BearerToken, ServerOptions, Service, configured_admin_router},
};

const ADMIN: &str = "admin_secret_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const PASSWORD: &str = "correct-horse-battery-staple";
const NEW_PASSWORD: &str = "maple-orbit-violet-canyon-42";

struct Reply {
    status: StatusCode,
    body: Value,
    cookies: Vec<String>,
    retry_after: Option<String>,
}

impl Reply {
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }

    /// The token from the `Set-Cookie` header that starts a session.
    fn session(&self) -> String {
        self.cookies
            .iter()
            .find_map(|cookie| {
                let (pair, _) = cookie.split_once(';')?;
                let (_, value) = pair.split_once('=')?;
                (!value.is_empty()).then(|| value.to_owned())
            })
            .unwrap_or_else(|| panic!("no session cookie: {} {}", self.status, self.body))
    }
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    input: Value,
) -> Reply {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = if input.is_null() {
        Body::empty()
    } else {
        request = request.header("content-type", "application/json");
        Body::from(input.to_string())
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let cookies = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect();
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        cookies,
        retry_after,
    }
}

/// A request made with the bootstrap admin token.
async fn admin(app: &Router, method: &str, path: &str, input: Value) -> Reply {
    let bearer = format!("Bearer {ADMIN}");
    send(app, method, path, &[("authorization", &bearer)], input).await
}

/// A same-origin dashboard request carrying a session cookie.
async fn browser(app: &Router, session: &str, method: &str, path: &str, input: Value) -> Reply {
    let cookie = format!("vlite_session={session}");
    let headers = [
        ("cookie", cookie.as_str()),
        ("sec-fetch-site", "same-origin"),
        ("x-vlite-request", "1"),
    ];
    send(app, method, path, &headers, input).await
}

async fn login(app: &Router, username: &str, password: &str) -> Reply {
    let headers = [("sec-fetch-site", "same-origin"), ("x-vlite-request", "1")];
    let input = json!({"username": username, "password": password});
    send(app, "POST", "/v1/auth/login", &headers, input).await
}

async fn create_user(app: &Router, input: Value) -> String {
    let reply = admin(app, "POST", "/v1/admin/users", input).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    reply.body["user"]["id"].as_str().unwrap().to_owned()
}

async fn create_tenant(app: &Router, name: &str) -> String {
    let reply = admin(app, "POST", "/v1/admin/tenants", json!({ "name": name })).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    reply.body["tenant"]["id"].as_str().unwrap().to_owned()
}

fn options(dir: &Path) -> ServerOptions {
    ServerOptions {
        admin_token: Some(BearerToken::new(ADMIN).unwrap()),
        admin_directory: Some(dir.join("admin")),
        loopback_only: true,
        ..ServerOptions::default()
    }
}

fn v4(dir: &Path) -> Service {
    Service::new(
        Database::create_with_format(dir.join("data.vlite"), 2, FormatVersion::V4).unwrap(),
    )
}

#[tokio::test]
async fn password_sign_in_uses_same_origin_cookie_sessions() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let service = v4(dir.path());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();

    let config = send(&app, "GET", "/v1/auth/config", &[], Value::Null).await;
    assert_eq!(config.status, StatusCode::OK);
    assert_eq!(
        config.body,
        json!({"mode":"protected","password_login":true,"reason":null,"setup_required":true})
    );
    let created = admin(
        &app,
        "POST",
        "/v1/admin/users",
        json!({"username":" Root.Admin ","password":PASSWORD,"role":"super_admin",
            "must_change_password":false}),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let user = &created.body["user"];
    assert_eq!(user["username"], "root.admin");
    assert_eq!(user["role"], "super_admin");
    assert_eq!(user["role_label"], "Super admin");
    assert!(user.get("password").is_none());
    assert!(!created.body.to_string().contains("argon2"));
    let config = send(&app, "GET", "/v1/auth/config", &[], Value::Null).await;
    assert_eq!(config.body["setup_required"], false);

    // Unknown usernames and wrong passwords are indistinguishable.
    let wrong = login(&app, "root.admin", NEW_PASSWORD).await;
    let unknown = login(&app, "nobody", PASSWORD).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.code(), "invalid_credentials");
    assert_eq!(wrong.body, unknown.body);
    assert!(wrong.cookies.is_empty());

    // Sign-in must come from the dashboard page itself.
    let input = json!({"username":"root.admin","password":PASSWORD});
    let missing = send(
        &app,
        "POST",
        "/v1/auth/login",
        &[("sec-fetch-site", "same-origin")],
        input.clone(),
    )
    .await;
    assert_eq!(
        (missing.status, missing.code()),
        (StatusCode::FORBIDDEN, "csrf_rejected")
    );
    let cross = send(
        &app,
        "POST",
        "/v1/auth/login",
        &[("sec-fetch-site", "cross-site"), ("x-vlite-request", "1")],
        input,
    )
    .await;
    assert_eq!(
        (cross.status, cross.code()),
        (StatusCode::FORBIDDEN, "csrf_rejected")
    );

    let signed_in = login(&app, "ROOT.ADMIN", PASSWORD).await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
    assert_eq!(signed_in.cookies.len(), 1);
    let cookie = &signed_in.cookies[0];
    assert!(cookie.starts_with("vlite_session="), "{cookie}");
    assert!(
        cookie.ends_with("; Path=/; HttpOnly; SameSite=Strict"),
        "{cookie}"
    );
    assert_eq!(signed_in.body["user"]["username"], "root.admin");
    assert_eq!(signed_in.body["must_change_password"], false);
    assert_eq!(signed_in.body["idle_timeout_seconds"], 1800);
    let session = signed_in.session();

    let who = browser(&app, &session, "GET", "/v1/auth/session", Value::Null).await;
    assert_eq!(who.status, StatusCode::OK, "{}", who.body);
    assert_eq!(who.body["method"], "password");
    assert_eq!(who.body["role"], "admin");
    assert_eq!(who.body["user"]["username"], "root.admin");
    let caps = browser(&app, &session, "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(caps.status, StatusCode::OK, "{}", caps.body);
    let identity = &caps.body["identity"];
    assert_eq!(identity["kind"], "user");
    assert_eq!(identity["username"], "root.admin");
    assert_eq!(identity["account_role"], "super_admin");
    assert_eq!(identity["label"], "root.admin · Super admin");
    assert_eq!(caps.body["features"]["password_login"], true);

    // State changes need the dashboard header; other sites are refused.
    let cookie_header = format!("vlite_session={session}");
    let collection = json!({"name":"docs","dimensions":2});
    let bare = send(
        &app,
        "POST",
        "/v1/collections",
        &[("cookie", &cookie_header)],
        collection.clone(),
    )
    .await;
    assert_eq!(
        (bare.status, bare.code()),
        (StatusCode::FORBIDDEN, "csrf_rejected")
    );
    let foreign = send(
        &app,
        "GET",
        "/v1/collections",
        &[("cookie", &cookie_header), ("sec-fetch-site", "cross-site")],
        Value::Null,
    )
    .await;
    assert_eq!(
        (foreign.status, foreign.code()),
        (StatusCode::FORBIDDEN, "csrf_rejected")
    );
    let made = browser(&app, &session, "POST", "/v1/collections", collection).await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);

    // An Authorization header always wins over a cookie.
    let bearer = send(
        &app,
        "GET",
        "/v1/collections",
        &[
            ("cookie", &cookie_header),
            (
                "authorization",
                "Bearer not_a_valid_token_0123456789_abcdefghijkl",
            ),
        ],
        Value::Null,
    )
    .await;
    assert_eq!(bearer.status, StatusCode::UNAUTHORIZED);
    let token = admin(&app, "GET", "/v1/auth/session", Value::Null).await;
    assert_eq!(token.body["method"], "token");
    assert_eq!(token.body["role"], "admin");
    assert_eq!(token.body["user"], Value::Null);
    assert_eq!(token.body["accounts"], true);

    // Unknown sessions end with both cookie variants cleared.
    let forged = browser(&app, &"A".repeat(43), "GET", "/v1/collections", Value::Null).await;
    assert_eq!(
        (forged.status, forged.code()),
        (StatusCode::UNAUTHORIZED, "session_expired")
    );
    assert_eq!(forged.cookies.len(), 2);
    assert!(forged.cookies.iter().all(|c| c.contains("Max-Age=0")));

    let out = browser(&app, &session, "POST", "/v1/auth/logout", Value::Null).await;
    assert_eq!(out.status, StatusCode::OK);
    assert_eq!(out.body, json!({"signed_out":true}));
    assert_eq!(out.cookies.len(), 2);
    let after = browser(&app, &session, "GET", "/v1/auth/session", Value::Null).await;
    assert_eq!(
        (after.status, after.code()),
        (StatusCode::UNAUTHORIZED, "session_expired")
    );
    drop(app);
    service.close().await.unwrap();
}

#[tokio::test]
async fn first_sign_in_requires_a_password_change() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let service = v4(dir.path());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    create_user(
        &app,
        json!({"username":"eddie","password":PASSWORD,"role":"editor"}),
    )
    .await;

    let first = login(&app, "eddie", PASSWORD).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.body["must_change_password"], true);
    let session = first.session();
    let gated = browser(&app, &session, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(
        (gated.status, gated.code()),
        (StatusCode::FORBIDDEN, "password_change_required")
    );
    let caps = browser(&app, &session, "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(caps.status, StatusCode::OK, "{}", caps.body);
    assert_eq!(caps.body["role"], "scoped");
    assert_eq!(caps.body["identity"]["must_change_password"], true);

    let change = |current: &str, new: &str| json!({"current_password":current,"new_password":new});
    let path = "/v1/auth/password";
    let wrong = browser(
        &app,
        &session,
        "POST",
        path,
        change(NEW_PASSWORD, "another-long-passphrase-77"),
    )
    .await;
    assert_eq!(
        (wrong.status, wrong.code()),
        (StatusCode::FORBIDDEN, "wrong_password")
    );
    let same = browser(&app, &session, "POST", path, change(PASSWORD, PASSWORD)).await;
    assert_eq!(
        (same.status, same.code()),
        (StatusCode::UNPROCESSABLE_ENTITY, "password_unchanged")
    );
    for weak in [
        "short",
        "aaaaaaaaaaaaaaaaaaaa",
        "abcdefghijklmnopqrst",
        "eddie-password-password-eddie",
    ] {
        let reply = browser(&app, &session, "POST", path, change(PASSWORD, weak)).await;
        assert_eq!(
            (reply.status, reply.code()),
            (StatusCode::UNPROCESSABLE_ENTITY, "weak_password"),
            "{weak}"
        );
    }
    let changed = browser(&app, &session, "POST", path, change(PASSWORD, NEW_PASSWORD)).await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
    assert_eq!(changed.body["must_change_password"], false);
    let fresh = changed.session();
    assert_ne!(fresh, session);
    let old = browser(&app, &session, "GET", "/v1/auth/session", Value::Null).await;
    assert_eq!(old.code(), "session_expired");
    let listed = browser(&app, &fresh, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(
        login(&app, "eddie", PASSWORD).await.code(),
        "invalid_credentials"
    );
    let again = login(&app, "eddie", NEW_PASSWORD).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(again.body["must_change_password"], false);

    // Password changes need a signed-in account, not a token.
    let token = admin(&app, "POST", path, change(NEW_PASSWORD, PASSWORD)).await;
    assert_eq!(
        (token.status, token.code()),
        (StatusCode::CONFLICT, "not_signed_in")
    );
    drop(app);
    service.close().await.unwrap();
}

#[tokio::test]
async fn roles_and_tenants_scope_what_accounts_can_do() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let service = v4(dir.path());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let acme = create_tenant(&app, "acme").await;
    let beta = create_tenant(&app, "beta").await;

    // Invalid accounts are refused before anything is stored.
    for (input, status, code) in [
        (
            json!({"username":"root","password":PASSWORD,"role":"super_admin","tenant_id":acme}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_input",
        ),
        (
            json!({"username":"ghost","password":PASSWORD,"role":"viewer","tenant_id":"0".repeat(32)}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "tenant_not_found",
        ),
        (
            json!({"username":"-x","password":PASSWORD,"role":"viewer"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_username",
        ),
        (
            json!({"username":"zed","password":"too-short","role":"viewer"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "weak_password",
        ),
        (
            json!({"username":"zed","password":PASSWORD,"role":"owner"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_json",
        ),
    ] {
        let reply = admin(&app, "POST", "/v1/admin/users", input.clone()).await;
        assert_eq!((reply.status, reply.code()), (status, code), "{input}");
    }

    let alice = create_user(
        &app,
        json!({"username":"alice","password":PASSWORD,"role":"editor","tenant_id":acme,
            "must_change_password":false}),
    )
    .await;
    create_user(
        &app,
        json!({"username":"victor","password":PASSWORD,"role":"viewer","tenant_id":acme,
            "must_change_password":false}),
    )
    .await;
    create_user(
        &app,
        json!({"username":"bob","password":PASSWORD,"role":"editor","tenant_id":beta,
            "must_change_password":false}),
    )
    .await;
    let taken = admin(
        &app,
        "POST",
        "/v1/admin/users",
        json!({"username":"ALICE","password":PASSWORD,"role":"viewer"}),
    )
    .await;
    assert_eq!(
        (taken.status, taken.code()),
        (StatusCode::CONFLICT, "username_taken")
    );

    let alice_session = login(&app, "alice", PASSWORD).await.session();
    let victor_session = login(&app, "victor", PASSWORD).await.session();
    let bob_session = login(&app, "bob", PASSWORD).await.session();

    let made = browser(
        &app,
        &alice_session,
        "POST",
        "/v1/collections",
        json!({"name":"docs","dimensions":2}),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.body);
    assert_eq!(made.body["name"], "docs");
    let put = browser(
        &app,
        &alice_session,
        "PUT",
        "/v1/collections/docs/vectors/1",
        json!({"vector":[1.0,0.0]}),
    )
    .await;
    assert_eq!(put.status, StatusCode::NO_CONTENT, "{}", put.body);
    let read = browser(
        &app,
        &victor_session,
        "GET",
        "/v1/collections/docs/vectors/1",
        Value::Null,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    let write = browser(
        &app,
        &victor_session,
        "PUT",
        "/v1/collections/docs/vectors/2",
        json!({"vector":[0.0,1.0]}),
    )
    .await;
    assert_eq!(
        (write.status, write.code()),
        (StatusCode::FORBIDDEN, "forbidden_scope")
    );
    let other = browser(
        &app,
        &bob_session,
        "GET",
        "/v1/collections/docs/vectors/1",
        Value::Null,
    )
    .await;
    assert_eq!(other.status, StatusCode::NOT_FOUND, "{}", other.body);
    for session in [&alice_session, &victor_session] {
        let users = browser(&app, session, "GET", "/v1/admin/users", Value::Null).await;
        assert_eq!(
            (users.status, users.code()),
            (StatusCode::FORBIDDEN, "forbidden_scope")
        );
        let tenant = browser(
            &app,
            session,
            "POST",
            "/v1/admin/tenants",
            json!({"name":"mine"}),
        )
        .await;
        assert_eq!(tenant.status, StatusCode::FORBIDDEN);
    }
    let caps = browser(&app, &alice_session, "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(caps.body["role"], "scoped");
    assert_eq!(caps.body["identity"]["label"], "alice · acme · Editor");
    assert_eq!(caps.body["identity"]["tenant_name"], "acme");

    // The admin list shows accounts and activity, never password hashes.
    let list = admin(&app, "GET", "/v1/admin/users", Value::Null).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert_eq!(list.body["capacity"], 1024);
    let items = list.body["items"].as_array().unwrap();
    let names: Vec<&str> = items
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["alice", "bob", "victor"]);
    assert_eq!(items[0]["tenant_name"], "acme");
    assert_eq!(items[0]["active_sessions"], 1);
    assert!(items[0]["last_login_unix_ms"].as_u64().is_some());
    assert!(!list.body.to_string().contains("argon2"));
    let tenants = admin(&app, "GET", "/v1/admin/tenants", Value::Null).await;
    let acme_view = tenants.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == acme.as_str())
        .unwrap();
    assert_eq!(acme_view["users"], 2);

    // Suspension blocks sign-in and live sessions; resuming restores them.
    let suspended = admin(
        &app,
        "POST",
        &format!("/v1/admin/tenants/{acme}/suspend"),
        Value::Null,
    )
    .await;
    assert_eq!(suspended.status, StatusCode::OK, "{}", suspended.body);
    let blocked = browser(&app, &alice_session, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(
        (blocked.status, blocked.code()),
        (StatusCode::FORBIDDEN, "tenant_suspended")
    );
    let refused = login(&app, "alice", PASSWORD).await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::FORBIDDEN, "tenant_suspended")
    );
    let resumed = admin(
        &app,
        "POST",
        &format!("/v1/admin/tenants/{acme}/resume"),
        Value::Null,
    )
    .await;
    assert_eq!(resumed.status, StatusCode::OK, "{}", resumed.body);
    let back = browser(&app, &alice_session, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.body);

    // Deleting a tenant removes its accounts and ends their sessions.
    let deleted = admin(
        &app,
        "DELETE",
        &format!("/v1/admin/tenants/{beta}"),
        Value::Null,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(deleted.body, json!({"deleted":true,"users_removed":1}));
    let gone = browser(&app, &bob_session, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(
        (gone.status, gone.code()),
        (StatusCode::UNAUTHORIZED, "session_expired")
    );
    assert_eq!(
        login(&app, "bob", PASSWORD).await.code(),
        "invalid_credentials"
    );

    // Any account change ends that account's sessions.
    let demoted = admin(
        &app,
        "PUT",
        &format!("/v1/admin/users/{alice}"),
        json!({"role":"viewer"}),
    )
    .await;
    assert_eq!(demoted.status, StatusCode::OK, "{}", demoted.body);
    assert_eq!(demoted.body["changed"], json!(["role"]));
    let ended = browser(&app, &alice_session, "GET", "/v1/collections", Value::Null).await;
    assert_eq!(ended.code(), "session_expired");
    drop(app);
    service.close().await.unwrap();
}

#[tokio::test]
async fn super_admins_manage_other_accounts_but_not_their_own() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let service = v4(dir.path());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let root = create_user(
        &app,
        json!({"username":"root","password":PASSWORD,"role":"super_admin","must_change_password":false}),
    )
    .await;
    let eddie = create_user(
        &app,
        json!({"username":"eddie","password":PASSWORD,"role":"editor","must_change_password":false}),
    )
    .await;
    let root_session = login(&app, "root", PASSWORD).await.session();
    let eddie_session = login(&app, "eddie", PASSWORD).await.session();

    let me = format!("/v1/admin/users/{root}");
    for (method, path, input) in [
        ("PUT", me.clone(), json!({"disabled":true})),
        ("DELETE", me.clone(), Value::Null),
        (
            "POST",
            format!("{me}/password"),
            json!({ "password": NEW_PASSWORD }),
        ),
    ] {
        let reply = browser(&app, &root_session, method, &path, input).await;
        assert_eq!(
            (reply.status, reply.code()),
            (StatusCode::CONFLICT, "cannot_modify_self"),
            "{method} {path}"
        );
    }

    let target = format!("/v1/admin/users/{eddie}");
    let empty = browser(&app, &root_session, "PUT", &target, json!({})).await;
    assert_eq!(
        (empty.status, empty.code()),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input")
    );
    let reset = browser(
        &app,
        &root_session,
        "POST",
        &format!("{target}/password"),
        json!({ "password": NEW_PASSWORD }),
    )
    .await;
    assert_eq!(reset.status, StatusCode::OK, "{}", reset.body);
    assert_eq!(reset.body["user"]["must_change_password"], true);
    let ended = browser(&app, &eddie_session, "GET", "/v1/auth/session", Value::Null).await;
    assert_eq!(ended.code(), "session_expired");
    assert_eq!(
        login(&app, "eddie", PASSWORD).await.code(),
        "invalid_credentials"
    );
    let temporary = login(&app, "eddie", NEW_PASSWORD).await;
    assert_eq!(temporary.status, StatusCode::OK, "{}", temporary.body);
    assert_eq!(temporary.body["must_change_password"], true);

    let disabled = browser(
        &app,
        &root_session,
        "PUT",
        &target,
        json!({"disabled":true}),
    )
    .await;
    assert_eq!(disabled.status, StatusCode::OK, "{}", disabled.body);
    assert_eq!(disabled.body["changed"], json!(["disabled"]));
    let ended = browser(
        &app,
        &temporary.session(),
        "GET",
        "/v1/auth/session",
        Value::Null,
    )
    .await;
    assert_eq!(ended.code(), "session_expired");
    let refused = login(&app, "eddie", NEW_PASSWORD).await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::FORBIDDEN, "account_disabled")
    );
    let unchanged = browser(
        &app,
        &root_session,
        "PUT",
        &target,
        json!({"disabled":true}),
    )
    .await;
    assert_eq!(unchanged.body["changed"], json!([]));
    let renamed = browser(
        &app,
        &root_session,
        "PUT",
        &target,
        json!({"username":"Edward","disabled":false}),
    )
    .await;
    assert_eq!(renamed.body["changed"], json!(["username", "disabled"]));
    assert_eq!(renamed.body["user"]["username"], "edward");
    assert_eq!(
        login(&app, "edward", NEW_PASSWORD).await.status,
        StatusCode::OK
    );

    let removed = browser(&app, &root_session, "DELETE", &target, Value::Null).await;
    assert_eq!(removed.body, json!({"deleted":true}));
    let again = browser(&app, &root_session, "DELETE", &target, Value::Null).await;
    assert_eq!(
        (again.status, again.code()),
        (StatusCode::NOT_FOUND, "user_not_found")
    );

    // The bootstrap admin token stays the break-glass path for any account.
    let locked = admin(&app, "PUT", &me, json!({"disabled":true})).await;
    assert_eq!(locked.status, StatusCode::OK, "{}", locked.body);
    let ended = browser(&app, &root_session, "GET", "/v1/auth/session", Value::Null).await;
    assert_eq!(ended.code(), "session_expired");
    drop(app);
    service.close().await.unwrap();
}

#[tokio::test]
async fn repeated_failures_lock_sign_in_with_retry_after() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let service = v4(dir.path());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    for name in ["carol", "dave"] {
        create_user(
            &app,
            json!({"username":name,"password":PASSWORD,"role":"super_admin","must_change_password":false}),
        )
        .await;
    }
    for attempt in 0..6 {
        let reply = login(&app, "carol", NEW_PASSWORD).await;
        assert_eq!(reply.code(), "invalid_credentials", "attempt {attempt}");
    }
    // Locked even with the right password.
    let locked = login(&app, "carol", PASSWORD).await;
    assert_eq!(
        (locked.status, locked.code()),
        (StatusCode::TOO_MANY_REQUESTS, "too_many_attempts")
    );
    assert_eq!(locked.retry_after.as_deref(), Some("1"));
    assert_eq!(login(&app, "dave", PASSWORD).await.status, StatusCode::OK);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let signed_in = login(&app, "carol", PASSWORD).await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
    drop(app);
    service.close().await.unwrap();
}

#[tokio::test]
async fn accounts_survive_restarts_but_sessions_do_not() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let path = dir.path().join("data.vlite");
    let service = v4(dir.path());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    create_user(
        &app,
        json!({"username":"root","password":PASSWORD,"role":"super_admin","must_change_password":false}),
    )
    .await;
    assert_eq!(
        login(&app, "root", NEW_PASSWORD).await.code(),
        "invalid_credentials"
    );
    let session = login(&app, "root", PASSWORD).await.session();
    drop(app);
    service.close().await.unwrap();

    // Only Argon2id hashes are stored, and no file ever holds a password.
    let admin_dir = dir.path().join("admin");
    let stored = std::fs::read_to_string(admin_dir.join("users.json")).unwrap();
    assert!(stored.contains("$argon2id$"), "{stored}");
    let audit = std::fs::read_to_string(admin_dir.join("audit.jsonl")).unwrap();
    assert!(audit.contains("\"user_created\""), "{audit}");
    assert!(audit.contains("\"user_login\""), "{audit}");
    for entry in std::fs::read_dir(&admin_dir).unwrap() {
        let file = entry.unwrap().path();
        if file.is_file() {
            let text = String::from_utf8_lossy(&std::fs::read(&file).unwrap()).into_owned();
            assert!(
                !text.contains(PASSWORD) && !text.contains(NEW_PASSWORD),
                "{}",
                file.display()
            );
        }
    }

    let service = Service::new(Database::open(&path).unwrap());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let stale = browser(&app, &session, "GET", "/v1/auth/session", Value::Null).await;
    assert_eq!(stale.code(), "session_expired");
    assert_eq!(login(&app, "root", PASSWORD).await.status, StatusCode::OK);
    drop(app);
    service.close().await.unwrap();

    // A damaged account file stops startup instead of dropping accounts.
    std::fs::write(
        admin_dir.join("users.json"),
        br#"{"schema":1,"users":[{"id":"x"}]}"#,
    )
    .unwrap();
    let service = Service::new(Database::open(&path).unwrap());
    assert!(configured_admin_router(service.clone(), options(dir.path())).is_err());
    service.close().await.unwrap();
}

#[tokio::test]
async fn password_sign_in_needs_a_safe_transport_and_an_admin_token() {
    // Plain HTTP on a non-loopback listener would expose passwords.
    let dir = tempfile::tempdir_in(".").unwrap();
    let service = v4(dir.path());
    let exposed = ServerOptions {
        loopback_only: false,
        ..options(dir.path())
    };
    let app = configured_admin_router(service.clone(), exposed).unwrap();
    create_user(
        &app,
        json!({"username":"root","password":PASSWORD,"role":"super_admin","must_change_password":false}),
    )
    .await;
    let config = send(&app, "GET", "/v1/auth/config", &[], Value::Null).await;
    assert_eq!(config.body["password_login"], false);
    assert_eq!(config.body["reason"], "insecure_transport");
    let refused = login(&app, "root", PASSWORD).await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::FORBIDDEN, "insecure_transport")
    );
    drop(app);
    service.close().await.unwrap();

    // Behind a trusted TLS-terminating proxy the cookie is `__Host-` and Secure.
    let dir = tempfile::tempdir_in(".").unwrap();
    let service = v4(dir.path());
    let proxied = ServerOptions {
        loopback_only: false,
        trusted_proxy: true,
        ..options(dir.path())
    };
    let app = configured_admin_router(service.clone(), proxied).unwrap();
    create_user(
        &app,
        json!({"username":"root","password":PASSWORD,"role":"super_admin","must_change_password":false}),
    )
    .await;
    let signed_in = send(
        &app,
        "POST",
        "/v1/auth/login",
        &[
            ("sec-fetch-site", "same-origin"),
            ("x-vlite-request", "1"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-for", "203.0.113.7"),
        ],
        json!({"username":"root","password":PASSWORD}),
    )
    .await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
    let cookie = &signed_in.cookies[0];
    assert!(cookie.starts_with("__Host-vlite_session="), "{cookie}");
    assert!(cookie.ends_with("; Secure"), "{cookie}");
    let host_cookie = format!("__Host-vlite_session={}", signed_in.session());
    let who = send(
        &app,
        "GET",
        "/v1/auth/session",
        &[("cookie", &host_cookie), ("sec-fetch-site", "same-origin")],
        Value::Null,
    )
    .await;
    assert_eq!(who.body["user"]["username"], "root", "{}", who.body);
    drop(app);
    service.close().await.unwrap();

    // Without an admin token every caller is already admin: no accounts.
    let dir = tempfile::tempdir_in(".").unwrap();
    let service = v4(dir.path());
    let open = ServerOptions {
        admin_token: None,
        ..options(dir.path())
    };
    let app = configured_admin_router(service.clone(), open).unwrap();
    let config = send(&app, "GET", "/v1/auth/config", &[], Value::Null).await;
    assert_eq!(
        config.body,
        json!({"mode":"open","password_login":false,"reason":"accounts_disabled","setup_required":false})
    );
    let refused = login(&app, "root", PASSWORD).await;
    assert_eq!(
        (refused.status, refused.code()),
        (StatusCode::CONFLICT, "accounts_disabled")
    );
    let users = send(&app, "GET", "/v1/admin/users", &[], Value::Null).await;
    assert_eq!(users.code(), "accounts_disabled");
    let cookie = format!("vlite_session={}", "A".repeat(43));
    let session = send(
        &app,
        "GET",
        "/v1/auth/session",
        &[("cookie", &cookie)],
        Value::Null,
    )
    .await;
    assert_eq!(session.status, StatusCode::OK, "{}", session.body);
    assert_eq!(session.body["method"], "open");
    assert_eq!(session.body["accounts"], false);
    drop(app);
    service.close().await.unwrap();
}

#[tokio::test]
async fn operators_can_seed_the_first_super_admin_from_a_file() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let path = dir.path().join("data.vlite");
    let secret = dir.path().join("admin-password");
    let seeded = || ServerOptions {
        admin_username: Some("Owner".into()),
        admin_password_file: Some(secret.clone()),
        ..options(dir.path())
    };
    let service = v4(dir.path());

    // Both settings are required, and the file must hold a strong password.
    let half = ServerOptions {
        admin_username: Some("owner".into()),
        ..options(dir.path())
    };
    assert!(configured_admin_router(service.clone(), half).is_err());
    std::fs::write(&secret, "password\n").unwrap();
    let error = configured_admin_router(service.clone(), seeded())
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("VLITE_ADMIN_PASSWORD_FILE"), "{error}");

    std::fs::write(&secret, format!("{PASSWORD}\r\n")).unwrap();
    let app = configured_admin_router(service.clone(), seeded()).unwrap();
    let signed_in = login(&app, "owner", PASSWORD).await;
    assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
    assert_eq!(signed_in.body["user"]["role"], "super_admin");
    assert_eq!(signed_in.body["must_change_password"], false);
    drop(app);
    service.close().await.unwrap();

    // Once a super admin exists the file is ignored; it is not a reset path.
    std::fs::write(&secret, format!("{NEW_PASSWORD}\n")).unwrap();
    let service = Service::new(Database::open(&path).unwrap());
    let app = configured_admin_router(service.clone(), seeded()).unwrap();
    assert_eq!(login(&app, "owner", PASSWORD).await.status, StatusCode::OK);
    assert_eq!(
        login(&app, "owner", NEW_PASSWORD).await.code(),
        "invalid_credentials"
    );
    let list = admin(&app, "GET", "/v1/admin/users", Value::Null).await;
    assert_eq!(list.body["items"].as_array().unwrap().len(), 1);
    drop(app);
    service.close().await.unwrap();
}

#[tokio::test]
async fn editor_and_viewer_accounts_need_a_v4_database() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let service = Service::new(Database::create(dir.path().join("data.vlite"), 2).unwrap());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let editor = admin(
        &app,
        "POST",
        "/v1/admin/users",
        json!({"username":"eddie","password":PASSWORD,"role":"editor"}),
    )
    .await;
    assert_eq!(
        (editor.status, editor.code()),
        (StatusCode::CONFLICT, "accounts_require_upgrade")
    );
    create_user(
        &app,
        json!({"username":"root","password":PASSWORD,"role":"super_admin","must_change_password":false}),
    )
    .await;
    assert_eq!(login(&app, "root", PASSWORD).await.status, StatusCode::OK);
    drop(app);
    service.close().await.unwrap();
}

/// Graph knobs: `graphs` (absent = role default, `[]` = none, or names) and
/// `graph_access` (`none` | `read` | `write`, capped by role).
#[tokio::test]
async fn graph_knobs_are_validated_and_reported() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let service = v4(dir.path());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let acme = create_tenant(&app, "acme").await;
    let base = |username: &str, role: &str| {
        json!({"username": username, "password": PASSWORD, "role": role,
            "must_change_password": false})
    };
    let mut input = base("eddie", "editor");
    input["graphs"] = json!(["g"]);
    input["graph_access"] = json!("read");
    let reply = admin(&app, "POST", "/v1/admin/users", input).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert_eq!(reply.body["user"]["graphs"], json!(["g"]));
    assert_eq!(reply.body["user"]["graph_access"], "read");
    let eddie = reply.body["user"]["id"].as_str().unwrap().to_owned();

    let long = "a".repeat(29);
    let bad = [
        ("viewer", None, json!({"graph_access": "write"})),
        ("super_admin", None, json!({"graph_access": "read"})),
        ("super_admin", None, json!({"graphs": []})),
        ("editor", None, json!({"graphs": ["G"]})),
        ("editor", None, json!({"graphs": ["*", "g"]})),
        ("editor", None, json!({"graph_access": "admin"})),
        (
            "editor",
            None,
            json!({"graphs": (0..65).map(|i| format!("g{i}")).collect::<Vec<_>>()}),
        ),
        ("editor", Some(&acme), json!({ "graphs": [long.clone()] })),
    ];
    for (index, (role, tenant, knobs)) in bad.into_iter().enumerate() {
        let mut input = base(&format!("bad{index}"), role);
        input["tenant_id"] = json!(tenant);
        for (key, value) in knobs.as_object().unwrap() {
            input[key] = value.clone();
        }
        let reply = admin(&app, "POST", "/v1/admin/users", input.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{input}: {}",
            reply.body
        );
    }
    // Untenanted names have the whole 63-byte budget; `[]` means no access.
    let mut input = base("ulla", "viewer");
    input["graphs"] = json!([long]);
    create_user(&app, input).await;
    let mut input = base("vera", "viewer");
    input["graphs"] = json!([]);
    input["tenant_id"] = json!(acme);
    let vera = create_user(&app, input).await;

    let path = format!("/v1/admin/users/{vera}");
    let reply = admin(&app, "PUT", &path, json!({"graph_access": "write"})).await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.body
    );
    let reply = admin(&app, "PUT", &path, json!({})).await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.body
    );
    assert!(
        reply.body.to_string().contains("graph_access"),
        "{}",
        reply.body
    );

    // A signed-in editor's knob change ends its sessions.
    let session = login(&app, "eddie", PASSWORD).await.session();
    let me = browser(&app, &session, "GET", "/v1/auth/session", Value::Null).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert_eq!(me.body["user"]["graphs"], json!(["g"]), "{}", me.body);
    assert_eq!(me.body["user"]["graph_access"], "read", "{}", me.body);
    let caps = browser(&app, &session, "GET", "/v1/capabilities", Value::Null).await;
    assert_eq!(caps.body["scope"]["graphs"], json!(["g"]), "{}", caps.body);
    let operations = caps.body["scope"]["operations"].as_array().unwrap();
    assert!(operations.contains(&json!("graph_read")), "{}", caps.body);
    assert!(!operations.contains(&json!("graph_write")), "{}", caps.body);

    let path = format!("/v1/admin/users/{eddie}");
    let reply = admin(&app, "PUT", &path, json!({"graph_access": "write"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["changed"], json!(["graph_access"]));
    let stale = browser(&app, &session, "GET", "/v1/auth/session", Value::Null).await;
    assert_eq!(stale.status, StatusCode::UNAUTHORIZED, "{}", stale.body);
    // A writer cannot be demoted to viewer while keeping write...
    let reply = admin(&app, "PUT", &path, json!({"role": "viewer"})).await;
    assert_eq!(
        reply.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        reply.body
    );
    // ...unless the same request lowers the knob; `null` resets to default.
    let input = json!({"role": "viewer", "graph_access": "read", "graphs": null});
    let reply = admin(&app, "PUT", &path, input).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        reply.body["changed"],
        json!(["role", "graphs", "graph_access"])
    );
    assert_eq!(reply.body["user"]["graphs"], Value::Null);
    let list = admin(&app, "GET", "/v1/admin/users", Value::Null).await;
    let listed = list.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|user| user["id"] == eddie)
        .unwrap();
    assert_eq!(listed["graph_access"], "read", "{listed}");
    assert_eq!(listed["role"], "viewer", "{listed}");
    let audit = std::fs::read_to_string(dir.path().join("admin").join("audit.jsonl")).unwrap();
    assert!(audit.contains("\"graph_access\":\"read\""), "{audit}");
    drop(app);
    service.close().await.unwrap();
}

/// Account files written before the graph knobs load unchanged. A stored
/// knob needs a graph build; other builds refuse the file rather than
/// silently dropping a restriction.
#[tokio::test]
async fn account_files_without_graph_knobs_still_load() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let path = dir.path().join("data.vlite");
    let service = v4(dir.path());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    create_user(
        &app,
        json!({"username":"eddie","password":PASSWORD,"role":"editor","must_change_password":false}),
    )
    .await;
    drop(app);
    service.close().await.unwrap();
    let file = dir.path().join("admin").join("users.json");
    let stored = std::fs::read_to_string(&file).unwrap();
    assert!(!stored.contains("graph"), "{stored}");

    let service = Service::new(Database::open(&path).unwrap());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let session = login(&app, "eddie", PASSWORD).await;
    assert_eq!(session.status, StatusCode::OK, "{}", session.body);
    drop(app);
    service.close().await.unwrap();

    let mut users: Value = serde_json::from_str(&stored).unwrap();
    users["users"][0]["graph_access"] = json!("read");
    std::fs::write(&file, users.to_string()).unwrap();
    let service = Service::new(Database::open(&path).unwrap());
    let app = configured_admin_router(service.clone(), options(dir.path())).unwrap();
    let list = admin(&app, "GET", "/v1/admin/users", Value::Null).await;
    assert_eq!(
        list.body["items"][0]["graph_access"], "read",
        "{}",
        list.body
    );
    assert_eq!(
        list.body["items"][0]["graphs"],
        Value::Null,
        "{}",
        list.body
    );
    service.close().await.unwrap();
}
