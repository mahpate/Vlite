//! Explicit administration; secrets and filesystem paths never enter responses.
use super::*;
use axum::Extension;
use std::{
    collections::VecDeque,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path as FilePath, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Admin,
    ReadOnly,
}
impl Role {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::ReadOnly => "read_only",
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct Auth {
    pub admin: Option<BearerToken>,
    pub read_only: Option<BearerToken>,
    admin_file: Option<PathBuf>,
    read_only_file: Option<PathBuf>,
    generation: u64,
}

#[derive(Default)]
pub(super) struct AdminState {
    pub auth: Mutex<Auth>,
    audit: Mutex<VecDeque<Value>>,
    maintenance: Mutex<Maintenance>,
    directory: Option<PathBuf>,
}
impl AdminState {
    pub fn record(&self, event: Value) {
        let mut audit = match self.audit.lock() {
            Ok(audit) => audit,
            Err(error) => {
                let _ = internal(error);
                return;
            }
        };
        if audit.len() == 256 {
            audit.pop_front();
        }
        audit.push_back(event);
    }
}

/// Token files are explicit reload sources. The admin directory must be private
/// to the operator; it contains database snapshots and startup selection.
#[derive(Default)]
pub struct ServerOptions {
    pub admin_token: Option<BearerToken>,
    pub read_only_token: Option<BearerToken>,
    pub admin_token_file: Option<PathBuf>,
    pub read_only_token_file: Option<PathBuf>,
    pub admin_directory: Option<PathBuf>,
}

impl ServerOptions {
    /// Validate roles before creating/opening a database.
    pub fn validate(&self) -> std::io::Result<()> {
        if self.admin_token.is_none()
            && (self.read_only_token.is_some()
                || self.read_only_token_file.is_some()
                || self.admin_token_file.is_some())
        {
            return Err(std::io::Error::other(
                "token files and read-only authentication require an admin token",
            ));
        }
        if let (Some(a), Some(r)) = (&self.admin_token, &self.read_only_token)
            && bool::from(a.0.ct_eq(&r.0))
        {
            return Err(std::io::Error::other(
                "admin and read-only tokens must differ",
            ));
        }
        Ok(())
    }
}

pub fn configured_admin_router(
    mut service: Service,
    options: ServerOptions,
) -> std::io::Result<Router> {
    options.validate()?;
    let directory = options
        .admin_directory
        .map(|p| {
            std::fs::create_dir_all(&p)?;
            std::fs::canonicalize(p)
        })
        .transpose()?;
    service.admin = Arc::new(AdminState {
        auth: Mutex::new(Auth {
            admin: options.admin_token,
            read_only: options.read_only_token,
            admin_file: options.admin_token_file,
            read_only_file: options.read_only_token_file,
            generation: 1,
        }),
        directory,
        ..AdminState::default()
    });
    Ok(super::configured_routes(service))
}

pub(super) fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn invalid(message: &'static str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_input", message)
}

pub fn read_token_file(path: &FilePath, optional: bool) -> std::io::Result<Option<BearerToken>> {
    let mut text = String::new();
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("token source must be a regular file"));
    }
    file.take(259).read_to_string(&mut text)?;
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    if optional && text.is_empty() {
        return Ok(None);
    }
    BearerToken::new(text)
        .map(Some)
        .map_err(std::io::Error::other)
}

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/admin/tokens", get(tokens))
        .route("/v1/admin/tokens/reload", post(reload))
        .route("/v1/admin/tokens/revoke-read-only", post(revoke))
        .route("/v1/admin/audit", get(audit))
        .route(
            "/v1/index/hnsw/rebuild",
            get(rebuild_status).post(rebuild_start),
        )
        .route("/v1/index/hnsw/rebuild/poll", post(rebuild_poll))
        .route("/v1/index/hnsw/rebuild/cancel", post(rebuild_cancel))
        .route(
            "/v1/index/hnsw/maintenance",
            get(maintenance_get).put(maintenance_put),
        )
        .route("/v1/index/hnsw/maintenance/tick", post(maintenance_tick))
        .route("/v1/admin/backups", get(backups).post(backup))
        .route("/v1/admin/restores", post(restore))
        .route("/v1/admin/restores/select", post(select_restore))
}

async fn capabilities(
    State(s): State<Service>,
    Extension(role): Extension<Role>,
) -> Result<Json<Value>, ApiError> {
    let auth = s.admin.auth.lock().map_err(internal)?;
    Ok(Json(
        json!({"version":env!("CARGO_PKG_VERSION"),"api_version":1,"role":role.name(),
        "features":{"read_only":true,"tokens_reload":auth.admin_file.is_some() || auth.read_only_file.is_some(),
        "audit":true,"background_rebuild":true,"maintenance":true,"backup_restore":s.admin.directory.is_some()},
        "routes":["/health","/ready","/metrics","/openapi.json","/dashboard","/dashboard/app.js","/dashboard/style.css","/dashboard/console-tools.js",
        "/v1/capabilities","/v1/database","/v1/vectors","/v1/vectors/{id}","/v1/batches","/v1/search",
        "/v1/index/hnsw","/v1/index/hnsw/rebuild","/v1/index/hnsw/rebuild/poll","/v1/index/hnsw/rebuild/cancel",
        "/v1/index/hnsw/maintenance","/v1/index/hnsw/maintenance/tick","/v1/admin/tokens","/v1/admin/tokens/reload",
        "/v1/admin/tokens/revoke-read-only","/v1/admin/audit","/v1/admin/backups","/v1/admin/restores","/v1/admin/restores/select"]}),
    ))
}
fn token_status(a: &Auth) -> Value {
    json!({"generation":a.generation,"admin_file_configured":a.admin_file.is_some(),
        "read_only_configured":a.read_only.is_some(),"read_only_file_configured":a.read_only_file.is_some()})
}
async fn tokens(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    let auth = s.admin.auth.lock().map_err(internal)?;
    Ok(Json(token_status(&auth)))
}
async fn reload(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    let state = s.admin.clone();
    tokio::task::spawn_blocking(move || {
        let snapshot = state.auth.lock().map_err(internal)?.clone();
        let a = &snapshot;
        if a.admin_file.is_none() && a.read_only_file.is_none() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "no_token_files",
                "no configured token files",
            ));
        }
        let admin = match &a.admin_file {
            Some(p) => read_token_file(p, false)
                .map_err(|_| invalid("admin token file unavailable or invalid"))?,
            None => a.admin.clone(),
        };
        let read_only = match &a.read_only_file {
            Some(p) => read_token_file(p, true)
                .map_err(|_| invalid("read-only token file unavailable or invalid"))?,
            None => a.read_only.clone(),
        };
        if let (Some(admin), Some(read_only)) = (&admin, &read_only)
            && bool::from(admin.0.ct_eq(&read_only.0))
        {
            return Err(invalid("admin and read-only tokens must differ"));
        }
        let mut a = state.auth.lock().map_err(internal)?;
        if a.generation != snapshot.generation {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "tokens_changed",
                "credentials changed during reload; inspect status before retrying",
            ));
        }
        a.admin = admin;
        a.read_only = read_only;
        a.generation += 1;
        Ok(Json(token_status(&a)))
    })
    .await
    .map_err(internal)?
}
async fn revoke(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    let mut a = s.admin.auth.lock().map_err(internal)?;
    a.read_only = None;
    a.generation += 1;
    Ok(Json(token_status(&a)))
}
async fn audit(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({"items":s.admin.audit.lock().map_err(internal)?.iter().cloned().collect::<Vec<_>>(),"capacity":256}),
    ))
}

#[derive(Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct IndexConfig {
    m: usize,
    ef_construction: usize,
    #[serde(deserialize_with = "seed")]
    seed: u64,
}
fn seed<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Seed {
        Number(u64),
        Text(String),
    }
    match Seed::deserialize(deserializer)? {
        Seed::Number(n) => Ok(n),
        Seed::Text(s) => super::id(&s)
            .map_err(|_| serde::de::Error::custom("seed must be a canonical decimal u64")),
    }
}
impl Default for IndexConfig {
    fn default() -> Self {
        let c = crate::HnswConfig::default();
        Self {
            m: c.m,
            ef_construction: c.ef_construction,
            seed: c.seed,
        }
    }
}
impl IndexConfig {
    pub(super) fn validated(self) -> Result<crate::HnswConfig, ApiError> {
        let c = crate::HnswConfig {
            m: self.m,
            ef_construction: self.ef_construction,
            seed: self.seed,
        };
        c.validate()?;
        Ok(c)
    }
    fn json(self) -> Value {
        json!({"m":self.m,"ef_construction":self.ef_construction,"seed":self.seed.to_string()})
    }
}
#[derive(Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Limits {
    max_records: usize,
    max_snapshot_vector_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        let l = crate::HnswRebuildLimits::default();
        Self {
            max_records: l.max_records,
            max_snapshot_vector_bytes: l.max_snapshot_vector_bytes,
        }
    }
}
impl Limits {
    fn core(self) -> crate::HnswRebuildLimits {
        crate::HnswRebuildLimits {
            max_records: self.max_records,
            max_snapshot_vector_bytes: self.max_snapshot_vector_bytes,
        }
    }
    fn json(self) -> Value {
        json!({"max_records":self.max_records,"max_snapshot_vector_bytes":self.max_snapshot_vector_bytes})
    }
}
#[derive(Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Maintenance {
    config: IndexConfig,
    metric: crate::DistanceMetric,
    limits: Limits,
    rebuild_at_buffered_ids: usize,
    rebuild_at_buffer_vector_bytes: usize,
}
impl Default for Maintenance {
    fn default() -> Self {
        let p = crate::HnswMaintenancePolicy::default();
        Self {
            config: IndexConfig::default(),
            metric: crate::DistanceMetric::default(),
            limits: Limits::default(),
            rebuild_at_buffered_ids: p.rebuild_at_buffered_ids,
            rebuild_at_buffer_vector_bytes: p.rebuild_at_buffer_vector_bytes,
        }
    }
}
impl Maintenance {
    fn policy(self) -> crate::HnswMaintenancePolicy {
        crate::HnswMaintenancePolicy {
            rebuild_at_buffered_ids: self.rebuild_at_buffered_ids,
            rebuild_at_buffer_vector_bytes: self.rebuild_at_buffer_vector_bytes,
            limits: self.limits.core(),
        }
    }
    fn json(self) -> Value {
        json!({"config":self.config.json(),"metric":self.metric,"limits":self.limits.json(),
        "rebuild_at_buffered_ids":self.rebuild_at_buffered_ids,"rebuild_at_buffer_vector_bytes":self.rebuild_at_buffer_vector_bytes})
    }
}
fn outcome(s: crate::HnswRebuildStatus) -> &'static str {
    use crate::HnswRebuildStatus::*;
    match s {
        Idle => "idle",
        Running => "running",
        Cancelling => "cancelling",
        Published => "published",
        Cancelled => "cancelled",
        Superseded => "superseded",
        CatchUpOverflow => "catch_up_overflow",
    }
}
fn capacity(c: crate::HnswBufferCapacity) -> Value {
    json!({"buffered_ids":c.buffered_ids,"buffer_vector_bytes":c.buffer_vector_bytes,"remaining_ids":c.remaining_ids,"remaining_vector_bytes":c.remaining_vector_bytes})
}
fn status(s: crate::HnswMaintenanceStatus) -> Value {
    use crate::HnswPressure::*;
    json!({"ready":s.ready,"pressure":match s.pressure {Healthy=>"healthy",RebuildRecommended=>"rebuild_recommended",CapacityExhausted=>"capacity_exhausted",Unavailable=>"unavailable",CatchUpOverflow=>"catch_up_overflow"},
        "active_buffer":s.active_buffer.map(capacity),"rebuild":s.rebuild.map(|p| json!({
            "config":{"m":p.config.m,"ef_construction":p.config.ef_construction,"seed":p.config.seed.to_string()},
            "metric":p.metric,"snapshot_records":p.snapshot_records,"processed_records":p.processed_records,
            "worker_finished":p.worker_finished,"terminal_reason":p.terminal_reason.map(outcome),
            "catchup_buffer":p.catchup_buffer.map(capacity)}))})
}
fn current(s: &Service) -> Result<Maintenance, ApiError> {
    s.admin.maintenance.lock().map(|p| *p).map_err(internal)
}
async fn maintenance_get(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    let p = current(&s)?;
    s.run(move |db| Ok(Json(json!({"policy":p.json(),"status":status(db.hnsw_maintenance_status(p.config.validated()?,p.metric,p.policy())?)})))).await
}
async fn maintenance_put(
    State(s): State<Service>,
    input: Result<Json<Maintenance>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let p = body(input)?;
    let admin = s.admin.clone();
    s.run(move |db| {
        let result = db.hnsw_maintenance_status(p.config.validated()?, p.metric, p.policy())?;
        *admin.maintenance.lock().map_err(internal)? = p;
        Ok(Json(json!({"policy":p.json(),"status":status(result)})))
    })
    .await
}
async fn maintenance_tick(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    let p = current(&s)?;
    s.run(move |db| {
        let r = db.maintain_hnsw(p.config.validated()?, p.metric, p.policy())?;
        Ok(Json(
            json!({"outcome":outcome(r.outcome),"started":r.started,"status":status(r.status)}),
        ))
    })
    .await
}
async fn rebuild_status(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    maintenance_get(State(s)).await
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Start {
    config: IndexConfig,
    metric: crate::DistanceMetric,
    limits: Limits,
}
async fn rebuild_start(
    State(s): State<Service>,
    input: Result<Json<Start>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let p = body(input)?;
    s.run(move |db| {
        db.start_hnsw_rebuild(p.config.validated()?, p.metric, p.limits.core())?;
        Ok((StatusCode::ACCEPTED, Json(json!({"outcome":"running"}))))
    })
    .await
}
async fn rebuild_poll(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    s.run(|db| Ok(Json(json!({"outcome":outcome(db.poll_hnsw_rebuild()?)}))))
        .await
}
async fn rebuild_cancel(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    s.run(|db| {
        db.ensure_healthy()?;
        Ok(Json(json!({"cancelled":db.cancel_hnsw_rebuild()})))
    })
    .await
}

fn generated(name: &str, prefix: &str) -> bool {
    name.starts_with(prefix)
        && name.ends_with(".vlite")
        && name.len() <= 96
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !name.contains("..")
}
fn directory(s: &Service) -> Result<PathBuf, ApiError> {
    s.admin.directory.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "admin_directory_disabled",
            "configure VLITE_ADMIN_DIR to enable backups",
        )
    })
}
fn safe_file(dir: &FilePath, name: &str, prefix: &str) -> Result<PathBuf, ApiError> {
    if !generated(name, prefix) {
        return Err(invalid("invalid generated snapshot name"));
    }
    let p = dir.join(name);
    let m = std::fs::symlink_metadata(&p).map_err(|_| invalid("snapshot not found"))?;
    if !m.is_file() || m.file_type().is_symlink() {
        return Err(invalid("snapshot must be a regular file"));
    }
    Ok(p)
}
fn inspection(i: crate::maintenance::Inspection) -> Value {
    json!({"format_version":i.format_version,"dimensions":i.dimensions,"live_records":i.live_records,
        "file_bytes":i.file_bytes,"valid_bytes":i.valid_bytes,"incomplete_tail_bytes":i.incomplete_tail_bytes})
}
static NAMES: AtomicU64 = AtomicU64::new(0);
fn name(prefix: &str) -> String {
    format!(
        "{prefix}{}-{}-{}.vlite",
        now_ms(),
        std::process::id(),
        NAMES.fetch_add(1, Ordering::Relaxed)
    )
}
async fn backups(State(s): State<Service>) -> Result<Json<Value>, ApiError> {
    let Some(dir) = s.admin.directory.clone() else {
        return Ok(Json(
            json!({"enabled":false,"items":[],"restart_required":true}),
        ));
    };
    s.run(move |_| {
        let mut items = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(internal)? {
            let entry = entry.map_err(internal)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = if generated(&name, "backup-") {
                "backup"
            } else if generated(&name, "restore-") {
                "restore"
            } else {
                continue;
            };
            if !entry.file_type().map_err(internal)?.is_file() {
                continue;
            }
            items.push(
                json!({"name":name,"kind":kind,"bytes":entry.metadata().map_err(internal)?.len()}),
            );
            if items.len() == 1000 {
                break;
            }
        }
        Ok(Json(
            json!({"enabled":true,"items":items,"restart_required":true}),
        ))
    })
    .await
}
async fn backup(State(s): State<Service>) -> Result<(StatusCode, Json<Value>), ApiError> {
    let dir = directory(&s)?;
    s.run(move |db| {
        db.ensure_healthy()?;
        let name = name("backup-");
        let mut staged = tempfile::NamedTempFile::new_in(&dir).map_err(internal)?;
        let position = db.file.0.stream_position().map_err(internal)?;
        let copied = (|| -> std::io::Result<()> {
            db.file.0.seek(SeekFrom::Start(0))?;
            std::io::copy(&mut db.file.0, staged.as_file_mut())?;
            staged.as_file().sync_all()
        })();
        db.file
            .0
            .seek(SeekFrom::Start(position))
            .map_err(internal)?;
        copied.map_err(internal)?;
        let report = crate::maintenance::inspect(staged.path())?;
        if report.incomplete_tail_bytes != 0 {
            return Err(invalid("database snapshot has an incomplete tail"));
        }
        staged
            .persist_noclobber(dir.join(&name))
            .map_err(internal)?;
        sync_directory(&dir)?;
        Ok((
            StatusCode::CREATED,
            Json(json!({"name":name,"inspection":inspection(report),"restart_required":true})),
        ))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Restore {
    backup: String,
}
async fn restore(
    State(s): State<Service>,
    input: Result<Json<Restore>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let input = body(input)?;
    let dir = directory(&s)?;
    s.run(move |_| {
        let source = safe_file(&dir, &input.backup, "backup-")?;
        let name = name("restore-");
        let report = crate::maintenance::restore(source, dir.join(&name))?;
        Ok((
            StatusCode::CREATED,
            Json(json!({"name":name,"inspection":inspection(report),"restart_required":true})),
        ))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Select {
    name: String,
}
fn sync_directory(dir: &FilePath) -> Result<(), ApiError> {
    #[cfg(unix)]
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(internal)?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
async fn select_restore(
    State(s): State<Service>,
    input: Result<Json<Select>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let input = body(input)?;
    let dir = directory(&s)?;
    s.run(move |_| {
        let path = safe_file(&dir, &input.name, "restore-")?;
        let report = crate::maintenance::inspect(path)?;
        if report.incomplete_tail_bytes != 0 {
            return Err(invalid("restore has incomplete tail"));
        }
        let mut file = tempfile::NamedTempFile::new_in(&dir).map_err(internal)?;
        file.write_all(input.name.as_bytes()).map_err(internal)?;
        file.as_file().sync_all().map_err(internal)?;
        file.persist(dir.join("next-startup")).map_err(internal)?;
        sync_directory(&dir)?;
        Ok(Json(
            json!({"name":input.name,"restart_required":true,"inspection":inspection(report)}),
        ))
    })
    .await
}
/// Resolves only a validated generated restored copy. A selection remains active
/// across restarts until the operator removes the private marker offline.
pub fn selected_database(directory: &FilePath) -> std::io::Result<Option<PathBuf>> {
    let marker = directory.join("next-startup");
    if !marker.try_exists()? {
        return Ok(None);
    }
    if std::fs::symlink_metadata(&marker)?.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "startup marker must not be a symlink",
        ));
    }
    let mut name = String::new();
    std::fs::File::open(marker)?
        .take(97)
        .read_to_string(&mut name)?;
    let path = safe_file(directory, &name, "restore-")
        .map_err(|_| std::io::Error::other("invalid startup selection"))?;
    let report = crate::maintenance::inspect(&path)
        .map_err(|_| std::io::Error::other("startup selection failed validation"))?;
    if report.incomplete_tail_bytes != 0 {
        return Err(std::io::Error::other(
            "startup selection has incomplete tail",
        ));
    }
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    fn poison<T>(mutex: &Mutex<T>) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("injected mutex poison");
        }));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn poisoned_admin_locks_return_internal_errors_without_panicking_or_bypassing_auth() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(Database::create(directory.path().join("db"), 1).unwrap());
        poison(&service.admin.audit);
        service.admin.record(json!({"event":"must_log_failure"}));
        assert_eq!(
            audit(State(service.clone())).await.unwrap_err().status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        poison(&service.admin.maintenance);
        assert_eq!(
            maintenance_get(State(service.clone()))
                .await
                .unwrap_err()
                .status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            maintenance_tick(State(service.clone()))
                .await
                .unwrap_err()
                .status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        poison(&service.admin.auth);
        assert_eq!(
            capabilities(State(service.clone()), Extension(Role::Admin))
                .await
                .unwrap_err()
                .status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            tokens(State(service.clone())).await.unwrap_err().status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            revoke(State(service.clone())).await.unwrap_err().status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let app = super::super::configured_routes(service);
        for (path, expected) in [
            ("/v1/database", StatusCode::INTERNAL_SERVER_ERROR),
            ("/v1/capabilities", StatusCode::INTERNAL_SERVER_ERROR),
            ("/health", StatusCode::OK),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert!(response.headers().contains_key("x-request-id"));
        }
    }
}
