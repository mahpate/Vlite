//! Dashboard accounts: username/password sign-in backed by server sessions.
//!
//! Accounts live in `<admin dir>/users.json` and store only Argon2id PHC
//! hashes. Every change follows the credential store's discipline: synced
//! audit, then atomic file replacement, then publish. Sessions are random
//! tokens kept only in memory (keyed by SHA-256) and carried in an
//! `HttpOnly; SameSite=Strict` cookie, so a restart signs everyone out. A
//! bearer token always wins over a cookie. Cookie requests must be
//! same-origin, and state-changing ones must also send `X-VLite-Request`.
use super::credentials::{Grant, Op};
use super::*;
use argon2::{Algorithm, Argon2, Params, Version};
use axum::http::HeaderValue;
use base64::prelude::{BASE64_STANDARD_NO_PAD, BASE64_URL_SAFE_NO_PAD};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::{Path as FilePath, PathBuf},
    time::Duration,
};

const FILE: &str = "users.json";
const SCHEMA: u32 = 1;
const CAPACITY: usize = 1024;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SEED_BYTES: u64 = 4096;
const COOKIE: &str = "vlite_session";
const HOST_COOKIE: &str = "__Host-vlite_session";
const REQUEST_HEADER: &str = "x-vlite-request";
const SESSIONS_PER_USER: usize = 16;
const MAX_SESSIONS: usize = 4096;
pub(super) const DEFAULT_IDLE: Duration = Duration::from_secs(30 * 60);
pub(super) const DEFAULT_LIFETIME: Duration = Duration::from_secs(8 * 3600);
const MAX_THROTTLE_KEYS: usize = 10_000;
const FREE_USER_FAILURES: u32 = 5;
const FREE_IP_FAILURES: u32 = 20;
const MAX_LOCK_MS: u64 = 15 * 60 * 1000;
const FAILURE_DECAY_MS: u64 = 3600 * 1000;
const HASHING_PERMITS: usize = 4;
const ARGON2_M: u32 = 19_456;
const ARGON2_T: u32 = 2;
const ARGON2_P: u32 = 1;
const MIN_PASSWORD: usize = 15;
const MAX_PASSWORD: usize = 128;
const MAX_PASSWORD_BYTES: usize = 1024;
const DUMMY_SALT: [u8; 16] = [0x5a; 16];
const COMMON_WORDS: [&str; 9] = [
    "vlite", "password", "passw0rd", "admin", "qwerty", "letmein", "welcome", "changeme", "secret",
];

/// Super admins hold full admin rights. Editors and viewers get the scoped
/// operations below, optionally confined to one tenant.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AccountRole {
    SuperAdmin,
    Editor,
    Viewer,
}

impl AccountRole {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::SuperAdmin => "super_admin",
            Self::Editor => "editor",
            Self::Viewer => "viewer",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::SuperAdmin => "Super admin",
            Self::Editor => "Editor",
            Self::Viewer => "Viewer",
        }
    }

    /// Role defaults; the account's graph knobs may narrow the graph
    /// operations (`Account::ops`).
    fn ops(self) -> &'static [Op] {
        match self {
            Self::SuperAdmin => &[],
            Self::Editor => EDITOR_OPS,
            Self::Viewer => VIEWER_OPS,
        }
    }
}

const EDITOR_OPS: &[Op] = &[
    Op::Read,
    Op::Search,
    Op::SearchBuild,
    Op::Write,
    Op::Import,
    Op::Index,
    Op::Collections,
    Op::GraphRead,
    Op::GraphWrite,
];
const VIEWER_OPS: &[Op] = &[Op::Read, Op::Search, Op::GraphRead];

/// Per-account graph operation cap. The effective level is the lower of this
/// and the role's (editor: write, viewer: read); a viewer can never hold
/// `write`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum GraphAccess {
    None,
    Read,
    Write,
}

/// Validates an account's graph knobs against its final role and tenant.
/// Super admins always have full graph access, so they take neither knob.
/// `graphs` is `[]` (no graph), `["*"]` or 1..64 graph names, interpreted
/// inside the tenant like a tenant credential's binding.
fn check_graph(
    role: AccountRole,
    tenant: Option<&str>,
    graphs: Option<&[String]>,
    access: Option<GraphAccess>,
) -> Result<(), ApiError> {
    match (role, access) {
        (AccountRole::SuperAdmin, _) if graphs.is_some() || access.is_some() => Err(unprocessable(
            "super admins always have full graph access; set graphs and graph_access to null",
        )),
        (AccountRole::Viewer, Some(GraphAccess::Write)) => Err(unprocessable(
            "viewers cannot hold graph write access; use graph_access read, none or null",
        )),
        _ => match graphs {
            Some(graphs) if !graphs.is_empty() => {
                super::credentials::graph_binding(graphs, tenant.is_some())
            }
            _ => Ok(()),
        },
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Account {
    id: String,
    username: String,
    role: AccountRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tenant_id: Option<String>,
    disabled: bool,
    must_change_password: bool,
    /// Argon2id PHC string; never returned or logged.
    password: String,
    created_unix_ms: u64,
    updated_unix_ms: u64,
    /// Bumped on every change; sessions bound to an older version end.
    version: u64,
    /// Graph names this account may use; absent is the role default
    /// (`["*"]` within the tenant), `[]` is no graph access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    graphs: Option<Vec<String>>,
    /// Graph operation cap below the role; absent is the role default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    graph_access: Option<GraphAccess>,
}

impl Account {
    fn valid(&self) -> bool {
        hex_id(&self.id)
            && normalize_username(&self.username).is_ok_and(|name| name == self.username)
            && Phc::parse(&self.password).is_some()
            && self.version >= 1
            && match self.role {
                AccountRole::SuperAdmin => self.tenant_id.is_none(),
                _ => self.tenant_id.as_deref().is_none_or(hex_id),
            }
            && self.graph_valid()
    }

    fn graph_valid(&self) -> bool {
        check_graph(
            self.role,
            self.tenant_id.as_deref(),
            self.graphs.as_deref(),
            self.graph_access,
        )
        .is_ok()
    }

    /// Effective graph level: the role's, capped by `graph_access`; an empty
    /// `graphs` binding means none.
    fn graph_level(&self) -> GraphAccess {
        let role = match self.role {
            AccountRole::Viewer => GraphAccess::Read,
            _ => GraphAccess::Write,
        };
        if self.graphs.as_ref().is_some_and(Vec::is_empty) {
            GraphAccess::None
        } else {
            self.graph_access.map_or(role, |access| access.min(role))
        }
    }

    /// Scoped operations for a signed-in editor or viewer.
    fn ops(&self) -> Vec<Op> {
        let level = self.graph_level();
        self.role
            .ops()
            .iter()
            .copied()
            .filter(|op| match op {
                Op::GraphRead => level >= GraphAccess::Read,
                Op::GraphWrite => level >= GraphAccess::Write,
                _ => true,
            })
            .collect()
    }
}

fn hex_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    schema: u32,
    users: Vec<Account>,
}

#[derive(Serialize)]
struct StoreRef<'a> {
    schema: u32,
    users: &'a [Account],
}

struct Session {
    user_id: String,
    version: u64,
    seen: u64,
    expires: u64,
}

#[derive(Default)]
struct Failures {
    count: u32,
    last: u64,
    locked_until: u64,
}

/// Session lifetime and transport policy for [`Accounts::load`].
pub(super) struct Settings {
    pub idle: Duration,
    pub lifetime: Duration,
    /// Native TLS terminates in this server.
    pub https: bool,
    /// The listener is bound to a loopback address.
    pub loopback_only: bool,
    /// A reverse proxy sets `X-Forwarded-Proto` and `X-Forwarded-For`.
    pub trusted_proxy: bool,
}

pub(super) struct Accounts {
    directory: PathBuf,
    users: Mutex<Vec<Account>>,
    sessions: Mutex<HashMap<[u8; 32], Session>>,
    failures: Mutex<HashMap<String, Failures>>,
    last_login: Mutex<HashMap<String, u64>>,
    hashing: Arc<Semaphore>,
    idle_ms: u64,
    lifetime_ms: u64,
    https: bool,
    loopback_only: bool,
    trusted_proxy: bool,
}

/// The signed-in dashboard user on a cookie-authenticated request.
#[derive(Clone)]
pub(super) struct Principal {
    pub user_id: String,
    pub username: String,
    pub role: AccountRole,
    pub tenant_id: Option<String>,
    pub must_change_password: bool,
    pub expires: u64,
    key: [u8; 32],
}

pub(super) struct SignedIn {
    pub role: admin::Role,
    pub principal: Principal,
}

/// A parsed `$argon2id$v=19$m=..,t=..,p=..$salt$hash` string.
struct Phc {
    m: u32,
    t: u32,
    p: u32,
    salt: Vec<u8>,
    hash: [u8; 32],
}

impl Phc {
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('$');
        if !parts.next()?.is_empty() || parts.next()? != "argon2id" || parts.next()? != "v=19" {
            return None;
        }
        let params = parts.next()?;
        let salt = BASE64_STANDARD_NO_PAD.decode(parts.next()?).ok()?;
        let hash: [u8; 32] = BASE64_STANDARD_NO_PAD
            .decode(parts.next()?)
            .ok()?
            .try_into()
            .ok()?;
        if parts.next().is_some() {
            return None;
        }
        let mut values = [0u32; 3];
        let mut seen = 0;
        for (index, item) in params.split(',').enumerate() {
            let (key, value) = item.split_once('=')?;
            if Some(&key) != ["m", "t", "p"].get(index)
                || value.is_empty()
                || value.len() > 6
                || (value.len() > 1 && value.starts_with('0'))
                || !value.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            values[index] = value.parse().ok()?;
            seen += 1;
        }
        let [m, t, p] = values;
        (seen == 3
            && (1..=4).contains(&p)
            && (8 * p..=262_144).contains(&m)
            && (1..=10).contains(&t)
            && (16..=64).contains(&salt.len()))
        .then_some(Self {
            m,
            t,
            p,
            salt,
            hash,
        })
    }

    fn encode(&self) -> String {
        format!(
            "$argon2id$v=19$m={},t={},p={}${}${}",
            self.m,
            self.t,
            self.p,
            BASE64_STANDARD_NO_PAD.encode(&self.salt),
            BASE64_STANDARD_NO_PAD.encode(self.hash)
        )
    }
}

fn derive(password: &str, m: u32, t: u32, p: u32, salt: &[u8]) -> Option<[u8; 32]> {
    let params = Params::new(m, t, p, Some(32)).ok()?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut out)
        .ok()?;
    Some(out)
}

/// Constant-work verification: unknown users still derive against a dummy
/// salt with the default cost, so timing does not reveal which names exist.
fn verify_password(stored: Option<&str>, password: &str) -> bool {
    let phc = stored.and_then(Phc::parse);
    let derived = match &phc {
        Some(phc) => derive(password, phc.m, phc.t, phc.p, &phc.salt),
        None => derive(password, ARGON2_M, ARGON2_T, ARGON2_P, &DUMMY_SALT),
    };
    let expected = phc.as_ref().map_or([0u8; 32], |phc| phc.hash);
    let matched = derived.is_some_and(|d| bool::from(d.as_slice().ct_eq(&expected)));
    matched && phc.is_some()
}

fn hash_password(password: &str) -> Result<String, ApiError> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(internal)?;
    let hash =
        derive(password, ARGON2_M, ARGON2_T, ARGON2_P, &salt).ok_or_else(|| internal("argon2"))?;
    Ok(Phc {
        m: ARGON2_M,
        t: ARGON2_T,
        p: ARGON2_P,
        salt: salt.to_vec(),
        hash,
    }
    .encode())
}

fn weak_password(message: &str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "weak_password", message)
}

/// Length-first policy (15..128 characters) with a few cheap checks against
/// obviously guessable choices. Passphrases are welcome.
fn check_password(password: &str, username: &str) -> Result<(), ApiError> {
    let chars: Vec<char> = password.chars().collect();
    let n = chars.len();
    if !(MIN_PASSWORD..=MAX_PASSWORD).contains(&n) || password.len() > MAX_PASSWORD_BYTES {
        return Err(weak_password("use a password of 15 to 128 characters"));
    }
    if chars.iter().any(|c| c.is_control()) {
        return Err(weak_password("passwords cannot contain control characters"));
    }
    let distinct = chars.iter().collect::<HashSet<_>>().len();
    let steps = chars
        .windows(2)
        .filter(|pair| (pair[0] as u32).abs_diff(pair[1] as u32) <= 1)
        .count();
    if distinct < 5 || steps * 5 >= (n - 1) * 4 {
        return Err(weak_password(
            "this password is too predictable; mix in more varied characters",
        ));
    }
    let mut rest = password.to_lowercase();
    let name = username.trim().to_lowercase();
    if name.chars().count() >= 3 {
        rest = rest.replace(&name, "");
    }
    for word in COMMON_WORDS {
        rest = rest.replace(word, "");
    }
    if rest.chars().count() < 8 {
        return Err(weak_password(
            "this password is too close to the username or a common word",
        ));
    }
    Ok(())
}

fn invalid_username() -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_username",
        "usernames are 3 to 64 characters (letters, digits, '.', '_', '-', '@') and start with a letter or digit",
    )
}

/// Usernames are case-insensitive: stored trimmed and lowercased.
fn normalize_username(name: &str) -> Result<String, ApiError> {
    let name = name.trim().to_ascii_lowercase();
    let valid = (3..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._@-".contains(&b))
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric());
    if valid {
        Ok(name)
    } else {
        Err(invalid_username())
    }
}

fn csrf_rejected() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "csrf_rejected",
        "cross-site request refused; reload the dashboard and try again",
    )
}

/// Browsers label cross-site requests with `Sec-Fetch-Site`; a custom header
/// on state-changing requests cannot be sent cross-origin without CORS.
fn check_origin(method: &Method, headers: &HeaderMap) -> Result<(), ApiError> {
    if let Some(site) = headers.get("sec-fetch-site")
        && !matches!(site.to_str(), Ok("same-origin" | "none"))
    {
        return Err(csrf_rejected());
    }
    let safe = *method == Method::GET || *method == Method::HEAD || *method == Method::OPTIONS;
    if !safe && !headers.contains_key(REQUEST_HEADER) {
        return Err(csrf_rejected());
    }
    Ok(())
}

fn valid_token(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The presented session token; the `__Host-` cookie wins over the plain one.
fn session_token(headers: &HeaderMap) -> Option<String> {
    let (mut host, mut plain) = (None, None);
    for value in headers.get_all(header::COOKIE) {
        let Ok(text) = value.to_str() else {
            continue;
        };
        for pair in text.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            let value = value.trim();
            if !valid_token(value) {
                continue;
            }
            match name.trim() {
                HOST_COOKIE if host.is_none() => host = Some(value.to_owned()),
                COOKIE if plain.is_none() => plain = Some(value.to_owned()),
                _ => {}
            }
        }
    }
    host.or(plain)
}

fn session_key(token: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    key.copy_from_slice(&Sha256::digest(token.as_bytes()));
    key
}

/// Clears both cookie variants (sent on sign-out and on an ended session).
pub(super) fn clear_cookies() -> [&'static str; 2] {
    [
        "vlite_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict",
        "__Host-vlite_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict; Secure",
    ]
}

/// Last item of the last header: the value appended by the trusted proxy.
fn forwarded(headers: &HeaderMap, name: &str) -> Option<String> {
    let last = headers.get_all(name).iter().next_back()?.to_str().ok()?;
    let item = last.rsplit(',').next()?.trim();
    (!item.is_empty()).then(|| item.to_owned())
}

fn live(session: &Session, now: u64, idle_ms: u64) -> bool {
    now < session.expires && now.saturating_sub(session.seen) < idle_ms
}

fn now() -> u64 {
    admin::now_ms() as u64
}

fn random_id() -> Result<String, ApiError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(internal)?;
    Ok(hex(&bytes))
}

fn invalid_credentials() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_credentials",
        "Invalid username or password",
    )
}

fn session_expired() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "session_expired",
        "your session ended; sign in again",
    )
}

fn accounts_disabled() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "accounts_disabled",
        "password sign-in needs an admin token and VLITE_ADMIN_DIR",
    )
}

fn accounts_require_upgrade() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "accounts_require_upgrade",
        "editor and viewer accounts need a format v4 or newer database",
    )
}

fn insecure_transport() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "insecure_transport",
        "password sign-in needs HTTPS, a loopback-only listener, or VLITE_TRUSTED_PROXY=1 behind a reverse proxy",
    )
}

fn busy() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "busy",
        "sign-in is busy; retry shortly",
    )
}

fn user_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "user_not_found", "user not found")
}

fn username_taken() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "username_taken",
        "that username is already in use",
    )
}

fn too_many_attempts(wait_ms: u64) -> Response {
    let mut response = ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "too_many_attempts",
        "too many sign-in attempts; wait and try again",
    )
    .into_response();
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from(wait_ms.div_ceil(1000).max(1)),
    );
    response
}

enum Persist {
    NotReplaced,
    Replaced,
}

impl Accounts {
    /// Loads (bounded, validated) or starts empty. Invalid content fails
    /// startup rather than silently dropping accounts.
    pub(super) fn load(directory: &FilePath, settings: &Settings) -> std::io::Result<Self> {
        let path = directory.join(FILE);
        let users = match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
            Ok(meta) => {
                if !meta.is_file() || meta.file_type().is_symlink() {
                    return Err(std::io::Error::other("users.json must be a regular file"));
                }
                if meta.len() > MAX_FILE_BYTES {
                    return Err(std::io::Error::other("users.json exceeds 1 MiB"));
                }
                let mut text = Vec::new();
                std::fs::File::open(&path)?
                    .take(MAX_FILE_BYTES + 1)
                    .read_to_end(&mut text)?;
                let file: StoreFile = serde_json::from_slice(&text)
                    .map_err(|_| std::io::Error::other("users.json is invalid"))?;
                let (mut ids, mut names) = (HashSet::new(), HashSet::new());
                if file.schema != SCHEMA
                    || file.users.len() > CAPACITY
                    || file.users.iter().any(|u| {
                        !u.valid() || !ids.insert(u.id.clone()) || !names.insert(u.username.clone())
                    })
                {
                    return Err(std::io::Error::other("users.json is invalid"));
                }
                file.users
            }
        };
        let millis = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Ok(Self {
            directory: directory.to_owned(),
            users: Mutex::new(users),
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
            last_login: Mutex::new(HashMap::new()),
            hashing: Arc::new(Semaphore::new(HASHING_PERMITS)),
            idle_ms: millis(settings.idle),
            lifetime_ms: millis(settings.lifetime),
            https: settings.https,
            loopback_only: settings.loopback_only,
            trusted_proxy: settings.trusted_proxy,
        })
    }

    /// Creates the first super admin from a password file unless one already
    /// exists. The password is never echoed or logged.
    pub(super) fn seed(
        &self,
        audit: &audit::Log,
        username: &str,
        password_file: &FilePath,
    ) -> std::io::Result<()> {
        let error = |message: &str| std::io::Error::other(message.to_owned());
        let users = self
            .users
            .lock()
            .map_err(|_| error("the account store is unavailable"))?;
        if users.iter().any(|u| u.role == AccountRole::SuperAdmin) {
            return Ok(());
        }
        let username = normalize_username(username)
            .map_err(|_| error("VLITE_ADMIN_USERNAME is not a valid username"))?;
        if users.iter().any(|u| u.username == username) {
            return Err(error(
                "VLITE_ADMIN_USERNAME is already used by another account",
            ));
        }
        if users.len() >= CAPACITY {
            return Err(error("the account store is full"));
        }
        let meta = std::fs::metadata(password_file)?;
        if !meta.is_file() || meta.len() > MAX_SEED_BYTES {
            return Err(error(
                "VLITE_ADMIN_PASSWORD_FILE must be a regular file of at most 4 KiB",
            ));
        }
        let mut text = String::new();
        std::fs::File::open(password_file)?
            .take(MAX_SEED_BYTES + 1)
            .read_to_string(&mut text)?;
        let password = text
            .strip_suffix('\n')
            .map_or(text.as_str(), |t| t.strip_suffix('\r').unwrap_or(t));
        check_password(password, &username).map_err(|e| {
            std::io::Error::other(format!("VLITE_ADMIN_PASSWORD_FILE: {}", e.message))
        })?;
        let now = now();
        let account = Account {
            id: random_id().map_err(|_| error("randomness is unavailable"))?,
            username,
            role: AccountRole::SuperAdmin,
            tenant_id: None,
            disabled: false,
            must_change_password: false,
            password: hash_password(password)
                .map_err(|_| error("could not hash the admin password"))?,
            created_unix_ms: now,
            updated_unix_ms: now,
            version: 1,
            graphs: None,
            graph_access: None,
        };
        let event = json!({"event":"user_created","actor":"bootstrap","user_id":account.id,
            "username":account.username,"role":"super_admin"});
        let mut next = users.clone();
        next.push(account);
        self.commit(audit, users, next, event)
            .map_err(|e| std::io::Error::other(e.message))
    }

    /// Temp file, fsync, atomic replace, directory fsync. A failure after the
    /// replacement means the new content is visible but not confirmed durable.
    fn persist(&self, users: &[Account]) -> Result<(), Persist> {
        let text = serde_json::to_vec(&StoreRef {
            schema: SCHEMA,
            users,
        })
        .map_err(|_| Persist::NotReplaced)?;
        let mut file =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|_| Persist::NotReplaced)?;
        file.write_all(&text).map_err(|_| Persist::NotReplaced)?;
        file.as_file()
            .sync_all()
            .map_err(|_| Persist::NotReplaced)?;
        file.persist(self.directory.join(FILE))
            .map_err(|_| Persist::NotReplaced)?;
        admin::sync_directory(&self.directory).map_err(|_| Persist::Replaced)
    }

    /// Audit (synced), persist, publish: no success is reported for a change
    /// that is not durable and audited. After the file is replaced, memory
    /// follows it even if the directory sync fails (reported as
    /// `accounts_commit_indeterminate`).
    fn commit(
        &self,
        audit: &audit::Log,
        mut users: std::sync::MutexGuard<'_, Vec<Account>>,
        next: Vec<Account>,
        event: Value,
    ) -> Result<(), ApiError> {
        audit
            .append(event.clone(), true)
            .map_err(|_| audit_unavailable())?;
        let name = event["event"].as_str().unwrap_or("user").to_owned();
        match self.persist(&next) {
            Ok(()) => {
                *users = next;
                Ok(())
            }
            Err(Persist::NotReplaced) => {
                let mut failed = event;
                failed["event"] = json!(format!("{name}_failed"));
                let _ = audit.append(failed, true);
                Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "accounts_unavailable",
                    "the account store could not be written; no change was made",
                ))
            }
            Err(Persist::Replaced) => {
                *users = next;
                let mut unknown = event;
                unknown["event"] = json!(format!("{name}_durability_unknown"));
                let _ = audit.append(unknown, true);
                Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "accounts_commit_indeterminate",
                    "the account change is in effect but its durability could not be confirmed; review the user list",
                ))
            }
        }
    }

    /// New session for `account`, evicting the user's least recently used
    /// session past 16 and the globally least recently used past 4096.
    fn start(&self, account: &Account, now: u64) -> Result<(String, u64), ApiError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(internal)?;
        let token = BASE64_URL_SAFE_NO_PAD.encode(bytes);
        let expires = now.saturating_add(self.lifetime_ms);
        let idle_ms = self.idle_ms;
        let mut sessions = self.sessions.lock().map_err(internal)?;
        sessions.retain(|_, s| live(s, now, idle_ms));
        let mine: Vec<([u8; 32], u64)> = sessions
            .iter()
            .filter(|(_, s)| s.user_id == account.id)
            .map(|(k, s)| (*k, s.seen))
            .collect();
        if mine.len() >= SESSIONS_PER_USER {
            let oldest = mine.iter().min_by_key(|(_, seen)| *seen).map(|(k, _)| *k);
            if let Some(oldest) = oldest {
                sessions.remove(&oldest);
            }
        }
        if sessions.len() >= MAX_SESSIONS {
            let oldest = sessions.iter().min_by_key(|(_, s)| s.seen).map(|(k, _)| *k);
            if let Some(oldest) = oldest {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(
            session_key(&token),
            Session {
                user_id: account.id.clone(),
                version: account.version,
                seen: now,
                expires,
            },
        );
        Ok((token, expires))
    }

    fn end(&self, key: &[u8; 32]) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.remove(key);
        }
    }

    fn end_user(&self, id: &str) {
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.retain(|_, s| s.user_id != id);
        }
    }

    /// Ends a deleted account's sessions and forgets its last sign-in.
    fn forget(&self, id: &str) {
        self.end_user(id);
        if let Ok(mut last) = self.last_login.lock() {
            last.remove(id);
        }
    }

    fn session_counts(&self, now: u64) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        if let Ok(sessions) = self.sessions.lock() {
            for session in sessions.values().filter(|s| live(s, now, self.idle_ms)) {
                *counts.entry(session.user_id.clone()).or_default() += 1;
            }
        }
        counts
    }

    /// Milliseconds until a locked key may try again, if any key is locked.
    fn locked(&self, keys: &[String], now: u64) -> Result<Option<u64>, ApiError> {
        let failures = self.failures.lock().map_err(internal)?;
        Ok(keys
            .iter()
            .filter_map(|key| failures.get(key))
            .map(|f| f.locked_until.saturating_sub(now))
            .filter(|wait| *wait > 0)
            .max())
    }

    /// Records a failure per key. Past `free` failures a key locks for 1s,
    /// 2s, 4s, ... up to 15 minutes; counts reset after an idle hour.
    fn fail(&self, keys: &[(String, u32)], now: u64) -> Result<(), ApiError> {
        let mut failures = self.failures.lock().map_err(internal)?;
        for (key, free) in keys {
            if !failures.contains_key(key) && failures.len() >= MAX_THROTTLE_KEYS {
                failures.retain(|_, f| {
                    f.locked_until > now || now.saturating_sub(f.last) < FAILURE_DECAY_MS
                });
                if failures.len() >= MAX_THROTTLE_KEYS {
                    let oldest = failures
                        .iter()
                        .filter(|(_, f)| f.locked_until <= now)
                        .min_by_key(|(_, f)| f.last)
                        .map(|(k, _)| k.clone());
                    let Some(oldest) = oldest else {
                        return Err(ApiError::new(
                            StatusCode::TOO_MANY_REQUESTS,
                            "too_many_attempts",
                            "too many sign-in attempts; wait and try again",
                        ));
                    };
                    failures.remove(&oldest);
                }
            }
            let entry = failures.entry(key.clone()).or_default();
            if now.saturating_sub(entry.last) >= FAILURE_DECAY_MS {
                *entry = Failures::default();
            }
            entry.count = entry.count.saturating_add(1);
            entry.last = now;
            if entry.count > *free {
                let shift = (entry.count - free - 1).min(20);
                entry.locked_until = now.saturating_add(MAX_LOCK_MS.min(1000u64 << shift));
            }
        }
        Ok(())
    }

    fn forgive(&self, key: &str) {
        if let Ok(mut failures) = self.failures.lock() {
            failures.remove(key);
        }
    }

    fn transport_ok(&self) -> bool {
        self.https || self.loopback_only || self.trusted_proxy
    }

    fn secure_request(&self, headers: &HeaderMap) -> bool {
        self.https
            || (self.trusted_proxy
                && forwarded(headers, "x-forwarded-proto")
                    .is_some_and(|proto| proto.eq_ignore_ascii_case("https")))
    }

    fn set_cookie(&self, headers: &HeaderMap, token: &str) -> Result<HeaderValue, ApiError> {
        let text = if self.secure_request(headers) {
            format!("{HOST_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Secure")
        } else {
            format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict")
        };
        HeaderValue::from_str(&text).map_err(internal)
    }

    /// The client address reported by a trusted proxy; `None` otherwise.
    fn client_ip(&self, headers: &HeaderMap) -> Option<String> {
        if !self.trusted_proxy {
            return None;
        }
        forwarded(headers, "x-forwarded-for")?
            .parse::<std::net::IpAddr>()
            .ok()
            .map(|ip| ip.to_string())
    }

    fn account(&self, id: &str) -> Result<Option<Account>, ApiError> {
        Ok(self
            .users
            .lock()
            .map_err(internal)?
            .iter()
            .find(|u| u.id == id)
            .cloned())
    }

    fn find_by_name(&self, username: &str) -> Result<Option<Account>, ApiError> {
        Ok(self
            .users
            .lock()
            .map_err(internal)?
            .iter()
            .find(|u| u.username == username)
            .cloned())
    }

    fn is_empty(&self) -> bool {
        self.users.lock().is_ok_and(|users| users.is_empty())
    }

    /// Accounts per tenant, for the tenant list.
    pub(super) fn count_by_tenant(&self) -> HashMap<String, u64> {
        let mut counts = HashMap::new();
        if let Ok(users) = self.users.lock() {
            for tenant in users.iter().filter_map(|u| u.tenant_id.clone()) {
                *counts.entry(tenant).or_default() += 1;
            }
        }
        counts
    }

    /// Removes a tenant's accounts while the tenant is being deleted. The
    /// caller holds the exclusive database lock, which serializes changes.
    pub(super) fn remove_tenant(
        &self,
        audit: &audit::Log,
        tenant: &str,
    ) -> Result<usize, ApiError> {
        let users = self.users.lock().map_err(internal)?;
        let removed: Vec<String> = users
            .iter()
            .filter(|u| u.tenant_id.as_deref() == Some(tenant))
            .map(|u| u.id.clone())
            .collect();
        if removed.is_empty() {
            return Ok(0);
        }
        let next = users
            .iter()
            .filter(|u| u.tenant_id.as_deref() != Some(tenant))
            .cloned()
            .collect();
        let event = json!({"event":"tenant_users_removed","tenant_id":tenant,"user_ids":removed});
        self.commit(audit, users, next, event)?;
        for id in &removed {
            self.forget(id);
        }
        Ok(removed.len())
    }

    /// Hashes off the async runtime, at most four at a time.
    async fn hash(&self, password: String) -> Result<String, ApiError> {
        let permit = self
            .hashing
            .clone()
            .acquire_owned()
            .await
            .map_err(internal)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            hash_password(&password)
        })
        .await
        .map_err(internal)?
    }
}

fn audit_unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "audit_unavailable",
        "persistent audit could not be written; nothing was changed",
    )
}

fn store(service: &Service) -> Result<Arc<Accounts>, ApiError> {
    service.admin.users.clone().ok_or_else(accounts_disabled)
}

fn stores(service: &Service) -> Result<(Arc<Accounts>, Arc<audit::Log>), ApiError> {
    let accounts = store(service)?;
    let log = service
        .admin
        .audit_log
        .clone()
        .ok_or_else(accounts_disabled)?;
    Ok((accounts, log))
}

/// Authenticates a request by its session cookie. `None` means this is not a
/// session request (no account store, an `Authorization` header, or no
/// well-formed session cookie), so token authentication applies instead.
pub(super) fn resolve(
    service: &Service,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
) -> Option<Result<SignedIn, ApiError>> {
    let accounts = service.admin.users.as_ref()?;
    if headers.contains_key(header::AUTHORIZATION) {
        return None;
    }
    let token = session_token(headers)?;
    Some(sign_in(service, accounts, method, path, headers, &token))
}

fn sign_in(
    service: &Service,
    accounts: &Accounts,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    token: &str,
) -> Result<SignedIn, ApiError> {
    check_origin(method, headers)?;
    let key = session_key(token);
    let now = now();
    let (user_id, version, expires) = {
        let mut sessions = accounts.sessions.lock().map_err(internal)?;
        let Some(session) = sessions.get_mut(&key) else {
            return Err(session_expired());
        };
        if !live(session, now, accounts.idle_ms) {
            sessions.remove(&key);
            return Err(session_expired());
        }
        session.seen = now;
        (session.user_id.clone(), session.version, session.expires)
    };
    let account = accounts
        .account(&user_id)?
        .filter(|a| a.version == version && !a.disabled);
    let Some(account) = account else {
        accounts.end(&key);
        return Err(session_expired());
    };
    if let Some(tenant) = &account.tenant_id {
        let tenants = service.admin.tenants.as_ref().ok_or_else(session_expired)?;
        tenants.get(tenant)?;
    }
    let exempt = matches!(
        path,
        "/v1/auth/session" | "/v1/auth/logout" | "/v1/auth/password"
    ) || ((*method == Method::GET || *method == Method::HEAD)
        && path == "/v1/capabilities");
    if account.must_change_password && !exempt {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "password_change_required",
            "change your password to continue",
        ));
    }
    let role = match account.role {
        AccountRole::SuperAdmin => admin::Role::Admin,
        _ => {
            let lineage = service.lineage.ok_or_else(accounts_require_upgrade)?;
            #[allow(unused_mut)]
            let mut grant = Grant::for_user(
                format!("user:{}", account.id),
                account.username.clone(),
                account.tenant_id.clone(),
                &account.ops(),
                lineage,
                expires,
            );
            if let Some(graphs) = account.graphs.as_ref().filter(|_| !grant.graphs.is_empty()) {
                grant.graphs.clone_from(graphs);
            }
            admin::Role::Scoped(Arc::new(grant))
        }
    };
    Ok(SignedIn {
        role,
        principal: Principal {
            user_id: account.id,
            username: account.username,
            role: account.role,
            tenant_id: account.tenant_id,
            must_change_password: account.must_change_password,
            expires,
            key,
        },
    })
}

/// Validates a role/tenant pairing against live tenants. Takes the tenants
/// lock, so call it before locking the account list.
fn check_scope(service: &Service, role: AccountRole, tenant: Option<&str>) -> Result<(), ApiError> {
    match (role, tenant) {
        (AccountRole::SuperAdmin, Some(_)) => Err(unprocessable(
            "super admins cannot belong to a tenant; set tenant_id to null",
        )),
        (AccountRole::SuperAdmin, None) => Ok(()),
        _ if service.lineage.is_none() => Err(accounts_require_upgrade()),
        (_, Some(id))
            if service
                .admin
                .tenants
                .as_ref()
                .and_then(|tenants| tenants.name(id))
                .is_none() =>
        {
            Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "tenant_not_found",
                "tenant not found",
            ))
        }
        _ => Ok(()),
    }
}

/// Public account fields. Takes the tenants lock for the tenant name.
fn user_view(service: &Service, account: &Account) -> Value {
    let tenant_name = account
        .tenant_id
        .as_deref()
        .and_then(|id| service.admin.tenants.as_ref()?.name(id));
    #[allow(unused_mut)]
    let mut view = json!({
        "id": account.id,
        "username": account.username,
        "role": account.role.name(),
        "role_label": account.role.label(),
        "tenant_id": account.tenant_id,
        "tenant_name": tenant_name,
        "disabled": account.disabled,
        "must_change_password": account.must_change_password,
        "created_unix_ms": account.created_unix_ms,
        "updated_unix_ms": account.updated_unix_ms,
    });
    // Stored knobs (null = role default); the effective scope is in
    // `GET /v1/capabilities` `scope`.
    {
        view["graphs"] = json!(account.graphs);
        view["graph_access"] = json!(account.graph_access);
    }
    view
}

fn signed_in_response(
    service: &Service,
    accounts: &Accounts,
    headers: &HeaderMap,
    account: &Account,
    token: &str,
    expires: u64,
) -> Result<Response, ApiError> {
    let cookie = accounts.set_cookie(headers, token)?;
    let mut response = Json(json!({
        "user": user_view(service, account),
        "expires_unix_ms": expires,
        "idle_timeout_seconds": accounts.idle_ms / 1000,
        "must_change_password": account.must_change_password,
    }))
    .into_response();
    response.headers_mut().append(header::SET_COOKIE, cookie);
    Ok(response)
}

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/auth/config", get(config))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/session", get(session))
        .route("/v1/auth/logout", post(logout))
        .route("/v1/auth/password", post(change_password))
        .route("/v1/admin/users", get(list).post(create))
        .route("/v1/admin/users/{id}", put(update).delete(remove))
        .route("/v1/admin/users/{id}/password", post(reset_password))
}

/// Public: tells the sign-in page which methods are available.
async fn config(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let open = service.admin.auth.lock().map_err(internal)?.admin.is_none();
    let (password_login, reason, setup_required) = match &service.admin.users {
        None => (false, Some("accounts_disabled"), false),
        Some(accounts) if !accounts.transport_ok() => (false, Some("insecure_transport"), false),
        Some(accounts) => (true, None, accounts.is_empty()),
    };
    Ok(Json(json!({
        "mode": if open { "open" } else { "protected" },
        "password_login": password_login,
        "reason": reason,
        "setup_required": setup_required,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginInput {
    username: String,
    password: String,
}

/// Public: verifies a username and password and starts a cookie session.
/// Failures look identical whether or not the username exists.
async fn login(
    State(service): State<Service>,
    headers: HeaderMap,
    input: Result<Json<LoginInput>, JsonRejection>,
) -> Result<Response, ApiError> {
    let (accounts, log) = stores(&service)?;
    check_origin(&Method::POST, &headers)?;
    if !accounts.transport_ok() {
        return Err(insecure_transport());
    }
    let input = body(input)?;
    let username = input.username.trim().to_ascii_lowercase();
    if username.len() > 64
        || input.password.len() > MAX_PASSWORD_BYTES
        || input.password.chars().count() > MAX_PASSWORD
    {
        return Err(invalid_credentials());
    }
    let ip = accounts.client_ip(&headers);
    let user_key = format!("u:{username}");
    let ip_key = ip.as_ref().map(|ip| format!("ip:{ip}"));
    let keys: Vec<String> = std::iter::once(user_key.clone())
        .chain(ip_key.clone())
        .collect();
    if let Some(wait) = accounts.locked(&keys, now())? {
        return Ok(too_many_attempts(wait));
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
            let limiter = other
                .as_ref()
                .map_or_else(|| "login:unknown".to_owned(), |a| format!("user:{}", a.id));
            if log.should_record_denial(&limiter, "/v1/auth/login") {
                let mut event = json!({"event":"user_login_failed","ip":ip});
                if let Some(account) = &other {
                    event["user_id"] = json!(account.id);
                    event["username"] = json!(account.username);
                }
                let _ = log.append(event, false);
            }
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
    if let Some(tenant) = &account.tenant_id {
        let tenants = service
            .admin
            .tenants
            .as_ref()
            .ok_or_else(accounts_disabled)?;
        tenants.get(tenant)?;
    }
    if account.role != AccountRole::SuperAdmin && service.lineage.is_none() {
        return Err(accounts_require_upgrade());
    }
    log.append(
        json!({"event":"user_login","user_id":account.id,"username":account.username,"ip":ip}),
        true,
    )
    .map_err(|_| audit_unavailable())?;
    if let Some(old) = session_token(&headers) {
        accounts.end(&session_key(&old));
    }
    let (token, expires) = accounts.start(&account, now)?;
    if let Ok(mut last) = accounts.last_login.lock() {
        last.insert(account.id.clone(), now);
    }
    signed_in_response(&service, &accounts, &headers, &account, &token, expires)
}

/// Who is signed in and how. Works for cookies, tokens, and open mode.
async fn session(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    principal: Option<Extension<Principal>>,
) -> Result<Json<Value>, ApiError> {
    let open = service.admin.auth.lock().map_err(internal)?.admin.is_none();
    let accounts = service.admin.users.clone();
    let setup_required = accounts.as_ref().is_some_and(|a| a.is_empty());
    let Some(Extension(principal)) = principal else {
        return Ok(Json(json!({
            "method": if open { "open" } else { "token" },
            "role": role.name(),
            "user": null,
            "expires_unix_ms": null,
            "idle_timeout_seconds": null,
            "must_change_password": false,
            "accounts": accounts.is_some(),
            "setup_required": setup_required,
        })));
    };
    let accounts = store(&service)?;
    let account = accounts
        .account(&principal.user_id)?
        .ok_or_else(session_expired)?;
    Ok(Json(json!({
        "method": "password",
        "role": role.name(),
        "user": user_view(&service, &account),
        "expires_unix_ms": principal.expires,
        "idle_timeout_seconds": accounts.idle_ms / 1000,
        "must_change_password": account.must_change_password,
        "accounts": true,
        "setup_required": setup_required,
    })))
}

/// Ends the current session (if any) and clears both cookie variants.
async fn logout(
    State(service): State<Service>,
    headers: HeaderMap,
    principal: Option<Extension<Principal>>,
) -> Result<Response, ApiError> {
    let mut signed_out = false;
    if let Some(accounts) = &service.admin.users {
        match &principal {
            Some(Extension(principal)) => {
                accounts.end(&principal.key);
                signed_out = true;
                if let Some(log) = &service.admin.audit_log {
                    let _ = log.append(
                        json!({"event":"user_logout","user_id":principal.user_id,
                            "username":principal.username}),
                        false,
                    );
                }
            }
            None => {
                if let Some(token) = session_token(&headers) {
                    accounts.end(&session_key(&token));
                }
            }
        }
    }
    let mut response = Json(json!({ "signed_out": signed_out })).into_response();
    for cookie in clear_cookies() {
        response
            .headers_mut()
            .append(header::SET_COOKIE, HeaderValue::from_static(cookie));
    }
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordChange {
    current_password: String,
    new_password: String,
}

/// Changes the signed-in user's own password, ends all of their sessions,
/// and starts a fresh one for this browser.
async fn change_password(
    State(service): State<Service>,
    headers: HeaderMap,
    principal: Option<Extension<Principal>>,
    input: Result<Json<PasswordChange>, JsonRejection>,
) -> Result<Response, ApiError> {
    let (accounts, log) = stores(&service)?;
    let Some(Extension(principal)) = principal else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_signed_in",
            "sign in with a username and password to change it",
        ));
    };
    let input = body(input)?;
    let user_key = format!("u:{}", principal.username);
    if let Some(wait) = accounts.locked(std::slice::from_ref(&user_key), now())? {
        return Ok(too_many_attempts(wait));
    }
    check_password(&input.new_password, &principal.username)?;
    if input.new_password == input.current_password {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "password_unchanged",
            "choose a password different from the current one",
        ));
    }
    let account = accounts
        .account(&principal.user_id)?
        .ok_or_else(session_expired)?;
    let version = account.version;
    let permit = accounts
        .hashing
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy())?;
    let (current, new) = (input.current_password, input.new_password);
    let hashed = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if !verify_password(Some(&account.password), &current) {
            return Ok(None);
        }
        hash_password(&new).map(Some)
    })
    .await
    .map_err(internal)??;
    let Some(hashed) = hashed else {
        accounts.fail(&[(user_key, FREE_USER_FAILURES)], now())?;
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "wrong_password",
            "the current password is incorrect",
        ));
    };
    let store = accounts.clone();
    let user_id = principal.user_id.clone();
    let updated = service
        .run(move |_db| {
            let users = store.users.lock().map_err(internal)?;
            let index = users
                .iter()
                .position(|u| u.id == user_id)
                .ok_or_else(session_expired)?;
            if users[index].version != version {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "account_changed",
                    "the account changed meanwhile; sign in again",
                ));
            }
            let mut next = users.clone();
            let account = &mut next[index];
            account.password = hashed;
            account.must_change_password = false;
            account.version += 1;
            account.updated_unix_ms = now();
            let updated = account.clone();
            let event = json!({"event":"user_password_changed","actor":format!("user:{user_id}"),
                "user_id":user_id,"username":updated.username});
            store.commit(&log, users, next, event)?;
            Ok(updated)
        })
        .await?;
    accounts.forgive(&user_key);
    accounts.end_user(&updated.id);
    let (token, expires) = accounts.start(&updated, now())?;
    signed_in_response(&service, &accounts, &headers, &updated, &token, expires)
}

fn actor(principal: &Option<Extension<Principal>>) -> String {
    principal.as_ref().map_or_else(
        || "admin_token".to_owned(),
        |Extension(p)| format!("user:{}", p.user_id),
    )
}

/// Super admins cannot lock themselves out; the admin token stays the
/// break-glass path for recovering the last super admin.
fn not_self(principal: &Option<Extension<Principal>>, id: &str) -> Result<(), ApiError> {
    if principal
        .as_ref()
        .is_some_and(|Extension(p)| p.user_id == id)
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "cannot_modify_self",
            "you cannot change, reset, or delete your own account here; use Change password or ask another super admin",
        ));
    }
    Ok(())
}

async fn list(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let accounts = store(&service)?;
    let mut users = accounts.users.lock().map_err(internal)?.clone();
    users.sort_by(|a, b| a.username.cmp(&b.username));
    let sessions = accounts.session_counts(now());
    let last_login = accounts.last_login.lock().map_err(internal)?.clone();
    let items: Vec<Value> = users
        .iter()
        .map(|account| {
            let mut view = user_view(&service, account);
            view["active_sessions"] = json!(sessions.get(&account.id).copied().unwrap_or(0));
            view["last_login_unix_ms"] = json!(last_login.get(&account.id));
            view
        })
        .collect();
    Ok(Json(json!({ "items": items, "capacity": CAPACITY })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUser {
    username: String,
    password: String,
    role: AccountRole,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    must_change_password: Option<bool>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    graphs: Option<Vec<String>>,
    #[serde(default)]
    graph_access: Option<GraphAccess>,
}

async fn create(
    State(service): State<Service>,
    principal: Option<Extension<Principal>>,
    input: Result<Json<CreateUser>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (accounts, log) = stores(&service)?;
    let input = body(input)?;
    let username = normalize_username(&input.username)?;
    check_scope(&service, input.role, input.tenant_id.as_deref())?;
    check_graph(
        input.role,
        input.tenant_id.as_deref(),
        input.graphs.as_deref(),
        input.graph_access,
    )?;
    check_password(&input.password, &username)?;
    if accounts.find_by_name(&username)?.is_some() {
        return Err(username_taken());
    }
    let password = accounts.hash(input.password).await?;
    let now = now();
    let account = Account {
        id: random_id()?,
        username,
        role: input.role,
        tenant_id: input.tenant_id,
        disabled: input.disabled.unwrap_or(false),
        must_change_password: input.must_change_password.unwrap_or(true),
        password,
        created_unix_ms: now,
        updated_unix_ms: now,
        version: 1,
        graphs: input.graphs,
        graph_access: input.graph_access,
    };
    let actor = actor(&principal);
    let scope = service.clone();
    let created = service
        .run(move |_db| {
            check_scope(&scope, account.role, account.tenant_id.as_deref())?;
            let users = accounts.users.lock().map_err(internal)?;
            if users.iter().any(|u| u.username == account.username) {
                return Err(username_taken());
            }
            if users.len() >= CAPACITY {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "accounts_full",
                    "the account limit (1024) is reached; delete unused accounts first",
                ));
            }
            let mut next = users.clone();
            next.push(account.clone());
            #[allow(unused_mut)]
            let mut event = json!({"event":"user_created","actor":actor,"user_id":account.id,
                "username":account.username,"role":account.role.name(),"tenant_id":account.tenant_id});
            {
                event["graphs"] = json!(account.graphs);
                event["graph_access"] = json!(account.graph_access);
            }
            accounts.commit(&log, users, next, event)?;
            Ok(account)
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "user": user_view(&service, &created) })),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateUser {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    role: Option<AccountRole>,
    #[serde(default, deserialize_with = "tenants::present")]
    tenant_id: Option<Option<String>>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    must_change_password: Option<bool>,
    #[serde(default, deserialize_with = "tenants::present")]
    graphs: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "tenants::present")]
    graph_access: Option<Option<GraphAccess>>,
}

impl UpdateUser {
    fn graph_empty(&self) -> bool {
        self.graphs.is_none() && self.graph_access.is_none()
    }
}

const UPDATE_FIELDS: &str = "provide at least one of username, role, tenant_id, disabled, \
     must_change_password, graphs, graph_access";

/// Any effective change bumps the account version, which ends its sessions.
async fn update(
    State(service): State<Service>,
    Path(id): Path<String>,
    principal: Option<Extension<Principal>>,
    input: Result<Json<UpdateUser>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let (accounts, log) = stores(&service)?;
    let input = body(input)?;
    if input.username.is_none()
        && input.role.is_none()
        && input.tenant_id.is_none()
        && input.disabled.is_none()
        && input.must_change_password.is_none()
        && input.graph_empty()
    {
        return Err(unprocessable(UPDATE_FIELDS));
    }
    not_self(&principal, &id)?;
    let username = input
        .username
        .as_deref()
        .map(normalize_username)
        .transpose()?;
    let actor = actor(&principal);
    let scope = service.clone();
    let store = accounts.clone();
    let (updated, changed) = service
        .run(move |_db| {
            let current = store.account(&id)?.ok_or_else(user_not_found)?;
            let mut account = current.clone();
            let mut changed: Vec<&'static str> = Vec::new();
            if let Some(name) = username.filter(|name| *name != account.username) {
                account.username = name;
                changed.push("username");
            }
            if let Some(role) = input.role.filter(|role| *role != account.role) {
                account.role = role;
                changed.push("role");
            }
            if let Some(tenant) = input.tenant_id.filter(|t| *t != account.tenant_id) {
                account.tenant_id = tenant;
                changed.push("tenant_id");
            }
            if let Some(disabled) = input.disabled.filter(|d| *d != account.disabled) {
                account.disabled = disabled;
                changed.push("disabled");
            }
            if let Some(must) = input
                .must_change_password
                .filter(|m| *m != account.must_change_password)
            {
                account.must_change_password = must;
                changed.push("must_change_password");
            }
            {
                if let Some(graphs) = input.graphs.filter(|g| *g != account.graphs) {
                    account.graphs = graphs;
                    changed.push("graphs");
                }
                if let Some(access) = input.graph_access.filter(|a| *a != account.graph_access) {
                    account.graph_access = access;
                    changed.push("graph_access");
                }
                // Checked on the final state even when nothing changed, so a
                // request that would leave an invalid pairing still fails.
                check_graph(
                    account.role,
                    account.tenant_id.as_deref(),
                    account.graphs.as_deref(),
                    account.graph_access,
                )?;
            }
            if changed.is_empty() {
                return Ok((current, changed));
            }
            check_scope(&scope, account.role, account.tenant_id.as_deref())?;
            let users = store.users.lock().map_err(internal)?;
            let index = users
                .iter()
                .position(|u| u.id == account.id)
                .ok_or_else(user_not_found)?;
            if users
                .iter()
                .any(|u| u.id != account.id && u.username == account.username)
            {
                return Err(username_taken());
            }
            account.version += 1;
            account.updated_unix_ms = now();
            let mut next = users.clone();
            next[index] = account.clone();
            #[allow(unused_mut)]
            let mut event = json!({"event":"user_updated","actor":actor,"user_id":account.id,
                "username":account.username,"changed":changed,"role":account.role.name(),
                "tenant_id":account.tenant_id,"disabled":account.disabled});
            {
                event["graphs"] = json!(account.graphs);
                event["graph_access"] = json!(account.graph_access);
            }
            store.commit(&log, users, next, event)?;
            Ok((account, changed))
        })
        .await?;
    if !changed.is_empty() {
        accounts.end_user(&updated.id);
    }
    Ok(Json(
        json!({ "user": user_view(&service, &updated), "changed": changed }),
    ))
}

async fn remove(
    State(service): State<Service>,
    Path(id): Path<String>,
    principal: Option<Extension<Principal>>,
) -> Result<Json<Value>, ApiError> {
    let (accounts, log) = stores(&service)?;
    not_self(&principal, &id)?;
    let actor = actor(&principal);
    let store = accounts.clone();
    let target = id.clone();
    service
        .run(move |_db| {
            let users = store.users.lock().map_err(internal)?;
            let account = users
                .iter()
                .find(|u| u.id == target)
                .cloned()
                .ok_or_else(user_not_found)?;
            let next = users.iter().filter(|u| u.id != target).cloned().collect();
            let event = json!({"event":"user_deleted","actor":actor,"user_id":account.id,
                "username":account.username});
            store.commit(&log, users, next, event)
        })
        .await?;
    accounts.forget(&id);
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetPassword {
    password: String,
    #[serde(default)]
    must_change_password: Option<bool>,
}

/// Sets a new password (by default one the user must change at next sign-in)
/// and ends the user's sessions.
async fn reset_password(
    State(service): State<Service>,
    Path(id): Path<String>,
    principal: Option<Extension<Principal>>,
    input: Result<Json<ResetPassword>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let (accounts, log) = stores(&service)?;
    let input = body(input)?;
    not_self(&principal, &id)?;
    let target = accounts.account(&id)?.ok_or_else(user_not_found)?;
    check_password(&input.password, &target.username)?;
    let password = accounts.hash(input.password).await?;
    let must_change = input.must_change_password.unwrap_or(true);
    let actor = actor(&principal);
    let store = accounts.clone();
    let updated = service
        .run(move |_db| {
            let users = store.users.lock().map_err(internal)?;
            let index = users
                .iter()
                .position(|u| u.id == id)
                .ok_or_else(user_not_found)?;
            let mut next = users.clone();
            let account = &mut next[index];
            account.password = password;
            account.must_change_password = must_change;
            account.version += 1;
            account.updated_unix_ms = now();
            let updated = account.clone();
            let event = json!({"event":"user_password_reset","actor":actor,"user_id":updated.id,
                "username":updated.username,"must_change_password":must_change});
            store.commit(&log, users, next, event)?;
            Ok(updated)
        })
        .await?;
    accounts.end_user(&updated.id);
    accounts.forgive(&format!("u:{}", updated.username));
    Ok(Json(json!({ "user": user_view(&service, &updated) })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            idle: DEFAULT_IDLE,
            lifetime: DEFAULT_LIFETIME,
            https: false,
            loopback_only: true,
            trusted_proxy: false,
        }
    }

    #[test]
    fn phc_round_trips_and_verifies() {
        let stored = hash_password("correct horse battery staple").unwrap();
        assert!(stored.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert_eq!(Phc::parse(&stored).unwrap().encode(), stored);
        assert!(verify_password(
            Some(&stored),
            "correct horse battery staple"
        ));
        assert!(!verify_password(
            Some(&stored),
            "correct horse battery stapl"
        ));
        assert!(!verify_password(None, "correct horse battery staple"));
    }

    #[test]
    fn phc_rejects_malformed_or_extreme_parameters() {
        let good = hash_password("another long passphrase").unwrap();
        for bad in [
            good.replace("argon2id", "argon2i"),
            good.replace("m=19456", "m=019456"),
            good.replace("m=19456", "m=999999"),
            good.replace("t=2", "t=0"),
            good.replace("p=1", "p=9"),
            format!("{good}$extra"),
            String::new(),
        ] {
            assert!(Phc::parse(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn password_policy_prefers_length_and_rejects_guessable_choices() {
        for weak in [
            "short",
            "abcdefghijklmnop",
            "aaaaabbbbbcccccdd",
            "alicealicealice1",
            "passwordpassword1",
            "tab\tinside password!",
        ] {
            assert!(check_password(weak, "alice").is_err(), "{weak}");
        }
        assert!(check_password(&"x1".repeat(65), "alice").is_err());
        assert!(check_password("correct horse battery staple", "alice").is_ok());
        assert!(check_password("Blue-Kettle-Morning-42", "alice").is_ok());
    }

    #[test]
    fn usernames_are_normalized() {
        assert_eq!(
            normalize_username("  Alice.Smith@Example ").unwrap(),
            "alice.smith@example"
        );
        let long = "a".repeat(65);
        for bad in ["ab", "-alice", "al ice", "alice!", long.as_str()] {
            assert!(normalize_username(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_cookie_wins_and_malformed_tokens_are_ignored() {
        let (host, plain) = ("A".repeat(43), "b".repeat(43));
        let mut headers = HeaderMap::new();
        let first = format!("theme=dark; vlite_session={plain}");
        headers.append(header::COOKIE, HeaderValue::from_str(&first).unwrap());
        let second = format!("__Host-vlite_session={host}");
        headers.append(header::COOKIE, HeaderValue::from_str(&second).unwrap());
        assert_eq!(session_token(&headers).as_deref(), Some(host.as_str()));
        let mut bad = HeaderMap::new();
        bad.insert(
            header::COOKIE,
            HeaderValue::from_static("vlite_session=short"),
        );
        assert_eq!(session_token(&bad), None);
    }

    #[test]
    fn cookie_requests_must_be_same_origin() {
        let mut headers = HeaderMap::new();
        assert!(check_origin(&Method::GET, &headers).is_ok());
        assert!(check_origin(&Method::POST, &headers).is_err());
        headers.insert(REQUEST_HEADER, HeaderValue::from_static("1"));
        assert!(check_origin(&Method::POST, &headers).is_ok());
        headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(check_origin(&Method::GET, &headers).is_err());
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(check_origin(&Method::POST, &headers).is_ok());
    }

    #[test]
    fn failures_back_off_exponentially_and_decay() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::load(dir.path(), &settings()).unwrap();
        let keys = ["u:alice".to_owned()];
        let fail = |at: u64| {
            accounts
                .fail(&[("u:alice".to_owned(), FREE_USER_FAILURES)], at)
                .unwrap()
        };
        for _ in 0..FREE_USER_FAILURES {
            fail(1_000);
        }
        assert_eq!(accounts.locked(&keys, 1_000).unwrap(), None);
        fail(1_000);
        assert_eq!(accounts.locked(&keys, 1_000).unwrap(), Some(1_000));
        fail(2_000);
        assert_eq!(accounts.locked(&keys, 2_000).unwrap(), Some(2_000));
        fail(2_000 + FAILURE_DECAY_MS);
        assert_eq!(
            accounts.locked(&keys, 2_000 + FAILURE_DECAY_MS).unwrap(),
            None
        );
    }

    #[test]
    fn sessions_expire_when_idle_and_are_capped_per_user() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = Accounts::load(dir.path(), &settings()).unwrap();
        let account = Account {
            id: "0".repeat(32),
            username: "alice".to_owned(),
            role: AccountRole::Viewer,
            tenant_id: None,
            disabled: false,
            must_change_password: false,
            password: String::new(),
            created_unix_ms: 0,
            updated_unix_ms: 0,
            version: 1,
            graphs: None,
            graph_access: None,
        };
        for at in 0..20 {
            accounts.start(&account, 1_000 + at).unwrap();
        }
        assert_eq!(accounts.session_counts(1_100)["0".repeat(32).as_str()], 16);
        let idle = accounts.idle_ms;
        assert!(accounts.session_counts(1_020 + idle).is_empty());
    }
}
