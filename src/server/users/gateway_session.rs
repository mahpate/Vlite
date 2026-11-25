//! OD-004: dashboard sign-in through an HA gateway (`vlite-server gateway`
//! with a `dashboard` block).
//!
//! A gateway opens no admin directory, so it cannot use the standalone
//! in-memory sessions, and its replicas share no state. Instead it loads,
//! at startup, a read-only export of the admin server's `users.json` (tenant
//! editor and viewer accounts only; Argon2id hashes, mounted as a secret and
//! never placed in the control state or Blob) and a session key every
//! replica shares. A session is a cookie carrying `{user_id, version,
//! created, tenant_id, issued, seen, expires, generation}` sealed with
//! HMAC-SHA256 under that key and naming the key's id, so any replica with
//! the same key and users file accepts it without shared state. Replicas
//! may also accept (never sign with) up to three other keys, so the key
//! can be rotated one replica at a time.
//!
//! Every cookie request is re-checked here: same-origin and `X-VLite-Request`
//! (as standalone), the seal, lifetime and idle limits, the account's
//! version, disabled flag and tenant in this replica's users file, the
//! tenant's admission and route in the current control-state snapshot, and
//! that the snapshot is not older than one the session has already seen. It
//! is then forwarded as the principal `user:<id>` with the scoped grant a
//! standalone sign-in gives (`user_grant`), which the backend accepts only as
//! a session grant (`credentials::Principal::grant`).
//!
//! Sessions are stateless: signing out clears this browser's cookie, and a
//! copied cookie stays valid until it idles out or reaches its lifetime.
//! Ending sessions early is an account change (new version, re-export,
//! restart) or a session key rotation. Passwords are changed on the admin
//! server; accounts that must change theirs cannot sign in here.
use super::*;
use crate::server::control_state::Snapshot;
use crate::server::gateway::RouteRefusal;
use crate::server::gateway_trust::read_bounded;
use crate::server::webhooks::hmac_sha256_bytes;

/// Bounds on a session's lifetime and idle timeout. The idle floor keeps
/// it well above the `seen` refresh interval.
const MIN_SESSION_MS: u64 = 5 * 60 * 1000;
const MAX_LIFETIME_MS: u64 = 24 * 3600 * 1000;
/// How stale a cookie's `seen` may get before a request re-issues it.
const REFRESH_MS: u64 = 60_000;
/// Clock skew tolerated between replicas for `issued` and `seen`.
const SKEW_MS: u64 = 60_000;
const MAX_COOKIE: usize = 512;
const MAX_KEY_FILE_BYTES: u64 = 1024;
const MIN_KEY_BYTES: usize = 32;
/// Verify-only keys a replica may hold while a key is rotated.
const MAX_ACCEPTED_KEYS: usize = 3;
/// Bound on a gateway login request body.
pub(in crate::server) const MAX_LOGIN_BYTES: usize = 16 * 1024;
const PREFIX: &str = "v1.";
/// Domain separation for the session MAC.
const CONTEXT: &[u8] = b"vlite gateway dashboard session v1.";
/// Domain separation for a key's public id, which reveals nothing of it.
const KID_CONTEXT: &[u8] = b"vlite gateway dashboard session kid v1.";

/// The gateway config's `dashboard` block.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::server) struct DashboardFile {
    /// An export of the admin server's `users.json` holding only tenant
    /// editor and viewer accounts.
    users_file: PathBuf,
    /// Base64 of at least 32 random bytes, identical on every replica.
    /// It signs new sessions and verifies existing ones.
    session_key_file: PathBuf,
    /// Keys that verify sessions but never sign them: the incoming or
    /// outgoing key while the session key is rotated.
    #[serde(default)]
    accepted_session_key_files: Vec<PathBuf>,
    #[serde(default)]
    idle_timeout_ms: Option<u64>,
    #[serde(default)]
    lifetime_ms: Option<u64>,
    /// An HTTPS ingress sets `X-Forwarded-Proto` and `X-Forwarded-For`.
    #[serde(default)]
    trusted_proxy: bool,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl DashboardFile {
    /// Validates the block and loads the users export and session key.
    /// Errors name the setting, never secret content.
    pub(in crate::server) fn load(&self, loopback_only: bool) -> Result<GatewaySessions, String> {
        let lifetime_ms = self.lifetime_ms.unwrap_or(millis(DEFAULT_LIFETIME));
        if !(MIN_SESSION_MS..=MAX_LIFETIME_MS).contains(&lifetime_ms) {
            return Err(format!(
                "dashboard.lifetime_ms must be {MIN_SESSION_MS}..={MAX_LIFETIME_MS}"
            ));
        }
        let idle_ms = self
            .idle_timeout_ms
            .unwrap_or_else(|| millis(DEFAULT_IDLE).min(lifetime_ms));
        if !(MIN_SESSION_MS..=lifetime_ms).contains(&idle_ms) {
            return Err(format!(
                "dashboard.idle_timeout_ms must be {MIN_SESSION_MS}..=lifetime_ms"
            ));
        }
        let text = read_bounded(&self.users_file, MAX_FILE_BYTES, "dashboard users file")?;
        let users = parse_store(&text)
            .map_err(|_| "dashboard users file is not a valid users.json".to_owned())?;
        if users.is_empty() {
            return Err("dashboard users file has no accounts".to_owned());
        }
        if users
            .iter()
            .any(|account| account.role == AccountRole::SuperAdmin || account.tenant_id.is_none())
        {
            return Err(
                "dashboard users file may hold only tenant editor and viewer accounts; \
                 super admins and accounts without a tenant cannot sign in through a gateway"
                    .to_owned(),
            );
        }
        if self.accepted_session_key_files.len() > MAX_ACCEPTED_KEYS {
            return Err(format!(
                "dashboard.accepted_session_key_files may list at most {MAX_ACCEPTED_KEYS} keys"
            ));
        }
        let mut keys = vec![SessionKey::read(
            &self.session_key_file,
            "dashboard session key",
        )?];
        for path in &self.accepted_session_key_files {
            let key = SessionKey::read(path, "dashboard accepted session key")?;
            if keys.iter().any(|known| known.kid == key.kid) {
                return Err(
                    "dashboard.accepted_session_key_files repeats a key or the session key"
                        .to_owned(),
                );
            }
            keys.push(key);
        }
        let newest = users
            .iter()
            .map(|account| account.created_unix_ms.max(account.updated_unix_ms))
            .max()
            .unwrap_or(0);
        Ok(GatewaySessions {
            accounts: Accounts {
                directory: PathBuf::new(),
                users: Mutex::new(users),
                sessions: Mutex::new(HashMap::new()),
                failures: Mutex::new(HashMap::new()),
                last_login: Mutex::new(HashMap::new()),
                hashing: Arc::new(Semaphore::new(HASHING_PERMITS)),
                idle_ms,
                lifetime_ms,
                https: false,
                loopback_only,
                trusted_proxy: self.trusted_proxy,
            },
            keys,
            newest,
        })
    }
}

fn read_key(path: &FilePath, what: &str) -> Result<Vec<u8>, String> {
    let mut text = read_bounded(path, MAX_KEY_FILE_BYTES, what)?;
    let key = std::str::from_utf8(&text)
        .ok()
        .map(|text| text.trim().trim_end_matches('='))
        .and_then(|text| {
            BASE64_STANDARD_NO_PAD
                .decode(text)
                .or_else(|_| BASE64_URL_SAFE_NO_PAD.decode(text))
                .ok()
        });
    text.fill(0);
    match key {
        Some(key) if key.len() >= MIN_KEY_BYTES => Ok(key),
        other => {
            if let Some(mut short) = other {
                short.fill(0);
            }
            Err(format!(
                "{what} must be base64 of at least {MIN_KEY_BYTES} random bytes"
            ))
        }
    }
}

/// A session key and its public id, which cookies name.
struct SessionKey {
    kid: String,
    key: Vec<u8>,
}

impl SessionKey {
    fn read(path: &FilePath, what: &str) -> Result<Self, String> {
        let key = read_key(path, what)?;
        let kid = hmac_sha256_bytes(&key, KID_CONTEXT)[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self { kid, key })
    }

    /// Covers the key id, so a cookie cannot be re-labelled to another key.
    fn mac(&self, payload: &str) -> [u8; 32] {
        let mut message = Vec::with_capacity(CONTEXT.len() + self.kid.len() + 1 + payload.len());
        message.extend_from_slice(CONTEXT);
        message.extend_from_slice(self.kid.as_bytes());
        message.push(b'.');
        message.extend_from_slice(payload.as_bytes());
        hmac_sha256_bytes(&self.key, &message)
    }
}

impl Drop for SessionKey {
    fn drop(&mut self) {
        self.key.fill(0);
    }
}

/// A gateway's dashboard accounts and session keys.
pub(in crate::server) struct GatewaySessions {
    accounts: Accounts,
    /// The active key first; it alone signs.
    keys: Vec<SessionKey>,
    /// The newest account stamp in the users file. An unknown account
    /// created after it is one this replica has not loaded yet.
    newest: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    user_id: String,
    /// The account version at sign-in; any account change ends the session.
    version: u64,
    /// The account's creation stamp: tells an account this replica has not
    /// loaded yet from one that was removed.
    created: u64,
    tenant_id: String,
    issued: u64,
    /// Last re-issue; idle time counts from here.
    seen: u64,
    expires: u64,
    /// The highest control-state generation the session has been served at.
    generation: u64,
}

/// A checked session.
struct Session {
    account: Account,
    claims: Claims,
    tenant_name: String,
    lineage: u64,
    /// `claims.expires`, capped by this replica's lifetime.
    expires: u64,
}

/// A cookie request authorized as its signed-in user.
pub(in crate::server) struct Resolved {
    pub(in crate::server) grant: Arc<Grant>,
    /// A re-issued cookie to set on the response.
    pub(in crate::server) refresh: Option<HeaderValue>,
}

enum Refused {
    /// The session stays; the request is refused.
    Keep(ApiError),
    /// The session is over: 401 and both cookies cleared.
    End,
}

impl From<ApiError> for Refused {
    fn from(error: ApiError) -> Self {
        Self::Keep(error)
    }
}

impl Refused {
    fn into_response(self) -> Response {
        match self {
            Self::Keep(error) => no_store(error.into_response()),
            Self::End => cleared(session_expired().into_response()),
        }
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn cleared(mut response: Response) -> Response {
    for cookie in clear_cookies() {
        response
            .headers_mut()
            .append(header::SET_COOKIE, HeaderValue::from_static(cookie));
    }
    no_store(response)
}

fn behind(code: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, code, message)
}

fn users_behind() -> Refused {
    Refused::Keep(behind(
        "dashboard_users_behind",
        "this gateway replica has an older dashboard users file than your session; retry shortly",
    ))
}

/// Shaped like a sealed session: never a standalone session token.
fn sealed(value: &str) -> bool {
    value.len() <= MAX_COOKIE
        && value.starts_with(PREFIX)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

impl GatewaySessions {
    /// The presented session cookie, if any. A request that arrived over
    /// HTTPS (as `trusted_proxy` reports it) may only use the `__Host-`
    /// cookie: a plain `vlite_session` can be planted by a sibling host or
    /// over HTTP, which would sign the browser in as someone else.
    pub(in crate::server) fn presented(&self, headers: &HeaderMap) -> Option<String> {
        cookie_value(headers, sealed, self.accounts.secure_request(headers))
    }

    /// The sign-in checks that need no body: same origin and a transport
    /// that may carry a password. Run before the body is read.
    pub(in crate::server) fn admit_login(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        check_origin(&Method::POST, headers)?;
        if !self.accounts.transport_ok() {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "insecure_transport",
                "password sign-in through a gateway needs a loopback bind, or dashboard.trusted_proxy behind an HTTPS ingress",
            ));
        }
        Ok(())
    }

    /// `v1.<kid>.<payload>.<mac>` under the active key.
    fn seal(&self, claims: &Claims) -> Result<String, ApiError> {
        let key = self
            .keys
            .first()
            .ok_or_else(|| internal("no gateway session key"))?;
        let payload = BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).map_err(internal)?);
        let mac = BASE64_URL_SAFE_NO_PAD.encode(key.mac(&payload));
        let sealed = format!("{PREFIX}{}.{payload}.{mac}", key.kid);
        if sealed.len() > MAX_COOKIE {
            return Err(internal("gateway session cookie too large"));
        }
        Ok(sealed)
    }

    /// The claims of a cookie sealed under any key this replica holds.
    fn open(&self, cookie: &str) -> Option<Claims> {
        let mut parts = cookie.strip_prefix(PREFIX)?.split('.');
        let (kid, payload, mac) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let key = self.keys.iter().find(|key| key.kid == kid)?;
        let mac = BASE64_URL_SAFE_NO_PAD.decode(mac).ok()?;
        if !bool::from(key.mac(payload).as_slice().ct_eq(&mac)) {
            return None;
        }
        serde_json::from_slice(&BASE64_URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
    }

    /// The key ids this replica accepts, the signing key first. Ids are
    /// derived one-way from the keys and safe to log.
    pub(in crate::server) fn kids(&self) -> Vec<&str> {
        self.keys.iter().map(|key| key.kid.as_str()).collect()
    }

    fn check(
        &self,
        method: &Method,
        headers: &HeaderMap,
        snapshot: &Snapshot,
        now: u64,
    ) -> Result<Session, Refused> {
        check_origin(method, headers)?;
        let claims = self
            .presented(headers)
            .and_then(|cookie| self.open(&cookie))
            .ok_or(Refused::End)?;
        let expires = claims
            .expires
            .min(claims.issued.saturating_add(self.accounts.lifetime_ms));
        if claims.issued > now.saturating_add(SKEW_MS)
            || claims.seen > now.saturating_add(SKEW_MS)
            || claims.seen < claims.issued
            || now >= expires
            || now.saturating_sub(claims.seen) >= self.accounts.idle_ms
        {
            return Err(Refused::End);
        }
        if claims.generation > snapshot.generation() {
            return Err(Refused::Keep(behind(
                "control_state_behind",
                "this gateway replica has not yet read the control state your session was served at; retry shortly",
            )));
        }
        // An unknown account created after everything in this replica's
        // users file was created after the file was exported; otherwise it
        // was removed (or filtered out of the export) and the session ends.
        let Some(account) = self.accounts.account(&claims.user_id)? else {
            return Err(if claims.created > self.newest {
                users_behind()
            } else {
                Refused::End
            });
        };
        if account.version < claims.version {
            return Err(users_behind());
        }
        if account.version != claims.version
            || account.disabled
            || account.must_change_password
            || account.tenant_id.as_deref() != Some(claims.tenant_id.as_str())
        {
            return Err(Refused::End);
        }
        let tenant = snapshot.admitted(&claims.tenant_id)?;
        let lineage = snapshot
            .record_for(&claims.tenant_id)
            .ok_or_else(|| RouteRefusal::NoRecord.error())?
            .lineage;
        Ok(Session {
            account,
            claims,
            tenant_name: tenant.name,
            lineage,
            expires,
        })
    }

    /// The re-issued cookie once `seen` is `REFRESH_MS` old.
    fn refreshed(
        &self,
        headers: &HeaderMap,
        session: &Session,
        snapshot: &Snapshot,
        now: u64,
    ) -> Result<Option<HeaderValue>, ApiError> {
        if now.saturating_sub(session.claims.seen) < REFRESH_MS {
            return Ok(None);
        }
        let claims = Claims {
            seen: now,
            generation: session.claims.generation.max(snapshot.generation()),
            ..session.claims.clone()
        };
        self.accounts
            .set_cookie(headers, &self.seal(&claims)?)
            .map(Some)
    }

    /// Authorizes a cookie request: the user's scoped grant for the routed
    /// tenant, or the response that refuses it.
    pub(in crate::server) fn resolve(
        &self,
        method: &Method,
        headers: &HeaderMap,
        snapshot: &Snapshot,
        now: u64,
    ) -> Result<Resolved, Box<Response>> {
        let session = self
            .check(method, headers, snapshot, now)
            .map_err(|refused| Box::new(refused.into_response()))?;
        let refresh = self
            .refreshed(headers, &session, snapshot, now)
            .map_err(|error| Box::new(error.into_response()))?;
        Ok(Resolved {
            grant: Arc::new(user_grant(
                &session.account,
                session.lineage,
                session.expires,
            )),
            refresh,
        })
    }

    /// `GET /v1/auth/config`.
    pub(in crate::server) fn config(&self) -> Response {
        let ok = self.accounts.transport_ok();
        no_store(
            Json(json!({
                "mode": "protected",
                "password_login": ok,
                "reason": if ok { None } else { Some("insecure_transport") },
                "setup_required": false,
            }))
            .into_response(),
        )
    }

    /// `POST /v1/auth/login`: the standalone checks and throttling, then a
    /// sealed session for an admitted, routed tenant. Keep it in step with
    /// the standalone `users::login`; unlike that one it writes no audit
    /// record, because a gateway has no admin directory or audit log.
    pub(in crate::server) async fn login(
        &self,
        headers: &HeaderMap,
        body: &[u8],
        snapshot: &Snapshot,
    ) -> Result<Response, ApiError> {
        let accounts = &self.accounts;
        self.admit_login(headers)?;
        let input: LoginInput = serde_json::from_slice(body).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "the body must be {\"username\": string, \"password\": string}",
            )
        })?;
        let username = input.username.trim().to_ascii_lowercase();
        if username.len() > 64
            || input.password.len() > MAX_PASSWORD_BYTES
            || input.password.chars().count() > MAX_PASSWORD
        {
            return Err(invalid_credentials());
        }
        let ip = accounts.client_ip(headers);
        let user_key = format!("u:{username}");
        let ip_key = ip.as_ref().map(|ip| format!("ip:{ip}"));
        let keys: Vec<String> = std::iter::once(user_key.clone())
            .chain(ip_key.clone())
            .collect();
        if let Some(wait) = accounts.locked(&keys, now())? {
            return Ok(no_store(too_many_attempts(wait)));
        }
        let account = accounts.find_by_name(&username)?;
        let permit = accounts
            .hashing
            .clone()
            .try_acquire_owned()
            .map_err(|_| busy())?;
        let stored = account.as_ref().map(|a| a.password.clone());
        let password = input.password;
        let verified = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            verify_password(stored.as_deref(), &password)
        })
        .await
        .map_err(internal)?;
        let now = now();
        let account = match account {
            Some(account) if verified => account,
            other => {
                let mut failed = vec![(user_key, FREE_USER_FAILURES)];
                failed.extend(ip_key.map(|key| (key, FREE_IP_FAILURES)));
                accounts.fail(&failed, now)?;
                // Unknown names are not logged: people sometimes type a password there.
                let mut event = json!({"event":"gateway_user_login_failed","ip":ip});
                if let Some(account) = &other {
                    event["user_id"] = json!(account.id);
                    event["username"] = json!(account.username);
                }
                log_stderr("warn", "gateway", event.to_string());
                return Err(invalid_credentials());
            }
        };
        accounts.forgive(&user_key);
        if account.disabled {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "account_disabled",
                "this account is disabled; ask an administrator to enable it",
            ));
        }
        let tenant_id = account.tenant_id.clone().ok_or_else(invalid_credentials)?;
        let tenant = snapshot.admitted(&tenant_id)?;
        snapshot
            .record_for(&tenant_id)
            .ok_or_else(|| RouteRefusal::NoRecord.error())?;
        if account.must_change_password {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "password_change_required",
                "change your password on the admin server, then sign in again",
            ));
        }
        let claims = Claims {
            user_id: account.id.clone(),
            version: account.version,
            created: account.created_unix_ms,
            tenant_id,
            issued: now,
            seen: now,
            expires: now.saturating_add(accounts.lifetime_ms),
            generation: snapshot.generation(),
        };
        let cookie = accounts.set_cookie(headers, &self.seal(&claims)?)?;
        log_stderr(
            "info",
            "gateway",
            json!({"event":"gateway_user_login","user_id":account.id,
                "username":account.username,"tenant_id":claims.tenant_id,"ip":ip})
            .to_string(),
        );
        let mut response = Json(json!({
            "user": account_view(&account, Some(tenant.name)),
            "expires_unix_ms": claims.expires,
            "idle_timeout_seconds": accounts.idle_ms / 1000,
            "must_change_password": false,
        }))
        .into_response();
        response.headers_mut().append(header::SET_COOKIE, cookie);
        Ok(no_store(response))
    }

    /// `GET /v1/auth/session` for a cookie request.
    pub(in crate::server) fn session(
        &self,
        headers: &HeaderMap,
        snapshot: &Snapshot,
        now: u64,
    ) -> Response {
        let session = match self.check(&Method::GET, headers, snapshot, now) {
            Ok(session) => session,
            Err(refused) => return refused.into_response(),
        };
        let refresh = match self.refreshed(headers, &session, snapshot, now) {
            Ok(refresh) => refresh,
            Err(error) => return error.into_response(),
        };
        let mut response = Json(json!({
            "method": "password",
            "role": crate::server::admin::SCOPED_ROLE,
            "user": account_view(&session.account, Some(session.tenant_name)),
            "expires_unix_ms": session.expires,
            "idle_timeout_seconds": self.accounts.idle_ms / 1000,
            "must_change_password": false,
            "accounts": true,
            "setup_required": false,
        }))
        .into_response();
        if let Some(cookie) = refresh {
            response.headers_mut().append(header::SET_COOKIE, cookie);
        }
        no_store(response)
    }

    /// `POST /v1/auth/password` for a cookie request: accounts are a
    /// read-only export here.
    pub(in crate::server) fn password(
        &self,
        headers: &HeaderMap,
        snapshot: &Snapshot,
        now: u64,
    ) -> Response {
        if let Err(refused) = self.check(&Method::POST, headers, snapshot, now) {
            return refused.into_response();
        }
        no_store(
            ApiError::new(
                StatusCode::CONFLICT,
                "password_change_unavailable",
                "change passwords on the admin server; a gateway's accounts are read-only",
            )
            .into_response(),
        )
    }

    /// `POST /v1/auth/logout` for a cookie request: clears both cookie
    /// variants. The session is stateless, so a copy of the cookie stays
    /// valid until it idles out or expires.
    pub(in crate::server) fn logout(&self, headers: &HeaderMap) -> Result<Response, ApiError> {
        check_origin(&Method::POST, headers)?;
        let claims = self
            .presented(headers)
            .and_then(|cookie| self.open(&cookie));
        if let Some(claims) = &claims {
            log_stderr(
                "info",
                "gateway",
                json!({"event":"gateway_user_logout","user_id":claims.user_id}).to_string(),
            );
        }
        Ok(cleared(
            Json(json!({ "signed_out": claims.is_some() })).into_response(),
        ))
    }
}

#[cfg(test)]
impl GatewaySessions {
    /// A sealed session cookie value with arbitrary claims.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::server) fn cookie_for_test(
        &self,
        user_id: &str,
        version: u64,
        created: u64,
        tenant_id: &str,
        issued: u64,
        seen: u64,
        generation: u64,
    ) -> String {
        self.seal(&Claims {
            user_id: user_id.to_owned(),
            version,
            created,
            tenant_id: tenant_id.to_owned(),
            issued,
            seen,
            expires: issued.saturating_add(self.accounts.lifetime_ms),
            generation,
        })
        .unwrap()
    }

    pub(in crate::server) fn idle_ms_for_test(&self) -> u64 {
        self.accounts.idle_ms
    }
}

/// An Argon2id hash as `users.json` stores it.
#[cfg(test)]
pub(in crate::server) fn hash_for_test(password: &str) -> String {
    hash_password(password).unwrap()
}

/// One `users.json` account.
#[cfg(test)]
pub(in crate::server) fn account_for_test(
    id: &str,
    username: &str,
    role: &str,
    tenant_id: Option<&str>,
    hash: &str,
) -> Value {
    json!({"id":id,"username":username,"role":role,"tenant_id":tenant_id,
        "disabled":false,"must_change_password":false,"password":hash,
        "created_unix_ms":1,"updated_unix_ms":1,"version":1})
}

/// Writes `users` and `key` to `directory` and loads a `dashboard` block
/// with `extra` settings.
#[cfg(test)]
pub(in crate::server) fn load_for_test(
    directory: &FilePath,
    users: &[Value],
    key: &[u8],
    extra: Value,
    loopback_only: bool,
) -> Result<GatewaySessions, String> {
    let users_file = directory.join("dashboard-users.json");
    let key_file = directory.join("dashboard-session.key");
    std::fs::write(
        &users_file,
        serde_json::to_vec(&json!({"schema":SCHEMA,"users":users})).unwrap(),
    )
    .unwrap();
    std::fs::write(&key_file, BASE64_STANDARD_NO_PAD.encode(key)).unwrap();
    let mut block = json!({"users_file":users_file,"session_key_file":key_file});
    if let (Some(block), Some(extra)) = (block.as_object_mut(), extra.as_object()) {
        block.extend(extra.clone());
    }
    serde_json::from_value::<DashboardFile>(block)
        .map_err(|error| error.to_string())?
        .load(loopback_only)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TENANT: &str = "0123456789abcdef0123456789abcdef";
    const USER: &str = "11111111111111111111111111111111";

    fn loaded(users: &[Value], key: &[u8], extra: Value) -> Result<GatewaySessions, String> {
        let directory = tempfile::tempdir().unwrap();
        load_for_test(directory.path(), users, key, extra, true)
    }

    #[test]
    fn load_refuses_unsafe_accounts_weak_keys_and_bad_timeouts() {
        let hash = hash_for_test("a long enough passphrase");
        let editor = account_for_test(USER, "alice", "editor", Some(TENANT), &hash);
        let ok = loaded(std::slice::from_ref(&editor), &[9; 32], json!({})).unwrap();
        assert_eq!(ok.accounts.idle_ms, millis(DEFAULT_IDLE));
        assert_eq!(ok.accounts.lifetime_ms, millis(DEFAULT_LIFETIME));
        let admin = account_for_test(
            "22222222222222222222222222222222",
            "root",
            "super_admin",
            None,
            &hash,
        );
        let untenanted = account_for_test(
            "33333333333333333333333333333333",
            "bob",
            "viewer",
            None,
            &hash,
        );
        for (users, key, extra, expected) in [
            (vec![], vec![9; 32], json!({}), "no accounts"),
            (
                vec![editor.clone(), admin],
                vec![9; 32],
                json!({}),
                "only tenant editor and viewer",
            ),
            (
                vec![editor.clone(), untenanted],
                vec![9; 32],
                json!({}),
                "only tenant editor and viewer",
            ),
            (vec![editor.clone()], vec![9; 31], json!({}), "at least 32"),
            (
                vec![editor.clone()],
                vec![9; 32],
                json!({"lifetime_ms": 1000}),
                "lifetime_ms",
            ),
            (
                vec![editor.clone()],
                vec![9; 32],
                json!({"lifetime_ms": 600_000, "idle_timeout_ms": 700_000}),
                "idle_timeout_ms",
            ),
            (
                vec![editor.clone()],
                vec![9; 32],
                json!({"idle_timeout_ms": 60_000}),
                "idle_timeout_ms",
            ),
            (
                vec![editor.clone()],
                vec![9; 32],
                json!({"allow_super_admins": true}),
                "unknown field",
            ),
        ] {
            let error = loaded(&users, &key, extra).err().unwrap();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains(&hash), "{error}");
        }
        let mut bad = editor.clone();
        bad["password"] = json!("plaintext");
        assert!(
            loaded(&[bad], &[9; 32], json!({}))
                .err()
                .unwrap()
                .contains("not a valid users.json")
        );
        let keys = tempfile::tempdir().unwrap();
        let same = key_file(keys.path(), "same.key", &[9; 32]);
        let short = key_file(keys.path(), "short.key", &[5; 31]);
        let others: Vec<PathBuf> = (1..=4u8)
            .map(|n| key_file(keys.path(), &format!("{n}.key"), &[n; 32]))
            .collect();
        for (accepted, expected) in [
            (vec![same], "repeats a key"),
            (vec![others[0].clone(), others[0].clone()], "repeats a key"),
            (vec![short], "accepted session key must be base64"),
            (others.clone(), "at most 3"),
        ] {
            let error = loaded(
                std::slice::from_ref(&editor),
                &[9; 32],
                json!({ "accepted_session_key_files": accepted }),
            )
            .err()
            .unwrap();
            assert!(error.contains(expected), "{error}");
        }
    }

    fn key_file(directory: &FilePath, name: &str, key: &[u8]) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, BASE64_STANDARD_NO_PAD.encode(key)).unwrap();
        path
    }

    #[test]
    fn sealed_sessions_reject_tampering_foreign_keys_and_standalone_tokens() {
        let hash = hash_for_test("a long enough passphrase");
        let users = [account_for_test(
            USER,
            "alice",
            "editor",
            Some(TENANT),
            &hash,
        )];
        let sessions = loaded(&users, &[9; 32], json!({})).unwrap();
        let other = loaded(&users, &[8; 32], json!({})).unwrap();
        let cookie = sessions.cookie_for_test(USER, 1, 1, TENANT, 10, 20, 3);
        assert!(sealed(&cookie) && cookie.len() <= MAX_COOKIE, "{cookie}");
        let kid = sessions.kids()[0];
        assert!(cookie.starts_with(&format!("{PREFIX}{kid}.")), "{cookie}");
        assert_eq!(kid.len(), 16);
        assert_ne!(kid, other.kids()[0]);
        let claims = sessions.open(&cookie).unwrap();
        assert_eq!(
            (claims.user_id.as_str(), claims.version, claims.generation),
            (USER, 1, 3)
        );
        assert!(other.open(&cookie).is_none());
        let (payload, mac) = cookie.rsplit_once('.').unwrap();
        let mut forged: Vec<u8> = payload.bytes().collect();
        let last = forged.len() - 1;
        forged[last] = if forged[last] == b'A' { b'B' } else { b'A' };
        let forged = format!("{}.{mac}", String::from_utf8(forged).unwrap());
        assert!(sessions.open(&forged).is_none());
        // Re-labelling a cookie with another key's id breaks its MAC.
        let relabelled = cookie.replacen(kid, other.kids()[0], 1);
        assert!(other.open(&relabelled).is_none());
        let extra = format!("{cookie}.{mac}");
        for malformed in ["v1.e30.", "v1.e30.AAAA", &extra, &cookie[PREFIX.len()..]] {
            assert!(sessions.open(malformed).is_none(), "{malformed}");
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("vlite_session=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        );
        assert_eq!(sessions.presented(&headers), None);
    }

    /// Rotation: a replica that accepts the incoming (or outgoing) key
    /// opens sessions sealed under it, but signs only with its own key.
    #[test]
    fn accepted_keys_verify_but_never_sign() {
        let hash = hash_for_test("a long enough passphrase");
        let users = [account_for_test(
            USER,
            "alice",
            "editor",
            Some(TENANT),
            &hash,
        )];
        let keys = tempfile::tempdir().unwrap();
        let old = loaded(&users, &[1; 32], json!({})).unwrap();
        let new_only = loaded(&users, &[2; 32], json!({})).unwrap();
        let both = loaded(
            &users,
            &[2; 32],
            json!({"accepted_session_key_files": [key_file(keys.path(), "old.key", &[1; 32])]}),
        )
        .unwrap();
        assert_eq!(both.kids(), [new_only.kids()[0], old.kids()[0]]);
        let from_old = old.cookie_for_test(USER, 1, 1, TENANT, 10, 20, 3);
        let from_both = both.cookie_for_test(USER, 1, 1, TENANT, 10, 20, 3);
        assert!(both.open(&from_old).is_some());
        assert!(new_only.open(&from_both).is_some());
        assert!(old.open(&from_both).is_none());
        assert!(new_only.open(&from_old).is_none());
    }
}
