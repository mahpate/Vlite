//! Persisted tenant namespace and aggregate vector limits.
use super::*;
use serde::Serialize;
use std::{
    io::{Read, Write},
    path::PathBuf,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Tenant {
    pub id: String,
    pub name: String,
    pub max_vectors: Option<u64>,
    #[serde(default)]
    deleted: bool,
    /// Set while suspended; omitted otherwise so an active-only store stays
    /// readable by servers that predate suspension. Older servers refuse to
    /// start on a store that contains a suspended tenant (fail closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    suspended_unix_ms: Option<u64>,
}
impl Tenant {
    fn state(&self) -> &'static str {
        if self.deleted {
            "deleted"
        } else if self.suspended_unix_ms.is_some() {
            "suspended"
        } else {
            "active"
        }
    }
    /// Stable public view: the persisted fields plus the derived `state`.
    fn view(&self) -> Value {
        json!({"id":self.id,"name":self.name,"max_vectors":self.max_vectors,
            "deleted":self.deleted,"state":self.state(),"suspended_unix_ms":self.suspended_unix_ms})
    }
}

fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "tenant_not_found",
        "tenant not found",
    )
}
fn valid_name(name: &str) -> Result<(), ApiError> {
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return Err(unprocessable("name must be 1..64 printable characters"));
    }
    Ok(())
}
/// Live vectors per collection owned by `id` (`t<id>__*`), unqualified.
fn usage(db: &Database, id: &str) -> Vec<(String, String, u64)> {
    let prefix = format!("t{id}__");
    let mut items: Vec<_> = db
        .collections
        .values()
        .filter_map(|state| {
            state.name.strip_prefix(&prefix).map(|name| {
                (
                    name.to_owned(),
                    state.name.clone(),
                    state.records.len() as u64,
                )
            })
        })
        .collect();
    items.sort();
    items
}

pub(super) struct Store {
    path: PathBuf,
    tenants: Mutex<Vec<Tenant>>,
}
impl Store {
    pub fn load(directory: &std::path::Path) -> std::io::Result<Self> {
        let path = directory.join("tenants.json");
        let tenants: Vec<Tenant> = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
            Ok(meta) => {
                if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 1024 * 1024 {
                    return Err(std::io::Error::other("invalid tenant store"));
                }
                let mut bytes = Vec::new();
                std::fs::File::open(&path)?
                    .take(1024 * 1024 + 1)
                    .read_to_end(&mut bytes)?;
                serde_json::from_slice(&bytes).map_err(std::io::Error::other)?
            }
        };
        let mut ids = std::collections::HashSet::new();
        if tenants.len() > 1024
            || tenants.iter().any(|t| {
                t.id.len() != 32
                    || !t.id.bytes().all(|b| b.is_ascii_hexdigit())
                    || !ids.insert(t.id.clone())
                    || t.name.is_empty()
                    || t.name.len() > 64
                    || t.name.chars().any(char::is_control)
            })
        {
            return Err(std::io::Error::other("invalid tenant store entries"));
        }
        Ok(Self {
            path,
            tenants: Mutex::new(tenants),
        })
    }
    /// Quota lookup for a collection named `t<id>__*`: unknown IDs have no
    /// tenant policy, deleted IDs fail closed, and suspended tenants still
    /// apply their quota so administrators can inspect and clean them up.
    pub fn quota(&self, id: &str) -> Result<Option<Tenant>, ApiError> {
        let tenants = self.tenants.lock().map_err(internal)?;
        match tenants.iter().find(|t| t.id == id) {
            None => Ok(None),
            Some(t) if t.deleted => Err(unavailable_tenant()),
            Some(t) => Ok(Some(t.clone())),
        }
    }
    /// Live display name, if any (for identity labels).
    pub fn name(&self, id: &str) -> Option<String> {
        self.tenants
            .lock()
            .ok()?
            .iter()
            .find(|t| t.id == id && !t.deleted)
            .map(|t| t.name.clone())
    }
    /// Admission check for tenant-bound credentials: the tenant must exist,
    /// not be deleted and not be suspended.
    pub fn get(&self, id: &str) -> Result<Tenant, ApiError> {
        let tenant = self
            .tenants
            .lock()
            .map_err(internal)?
            .iter()
            .find(|t| t.id == id && !t.deleted)
            .cloned()
            .ok_or_else(unavailable_tenant)?;
        if tenant.suspended_unix_ms.is_some() {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "tenant_suspended",
                "tenant is suspended; ask an administrator to resume it",
            ));
        }
        Ok(tenant)
    }
    fn commit(&self, next: &[Tenant]) -> Result<(), ApiError> {
        let directory = self.path.parent().expect("tenant directory");
        let mut file = tempfile::NamedTempFile::new_in(directory).map_err(internal)?;
        file.write_all(&serde_json::to_vec(next).map_err(internal)?)
            .map_err(internal)?;
        file.as_file().sync_all().map_err(internal)?;
        file.persist(&self.path).map_err(internal)?;
        // Reconcile memory before reporting uncertain directory durability.
        *self.tenants.lock().map_err(internal)? = next.to_vec();
        admin::sync_directory(directory).map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "tenants_commit_indeterminate",
                "tenant change is in effect but durability is unconfirmed",
            )
        })
    }
}
fn unavailable_tenant() -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        "tenant_unavailable",
        "tenant is unavailable",
    )
}
fn store(service: &Service) -> Result<Arc<Store>, ApiError> {
    service.admin.tenants.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "tenants_disabled",
            "tenants require admin authentication and an admin directory",
        )
    })
}
pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/v1/admin/tenants", get(list).post(create))
        .route("/v1/admin/tenants/{id}", axum::routing::delete(delete))
        .route("/v1/admin/tenants/{id}/usage", get(usage_report))
        .route("/v1/admin/tenants/{id}/settings", put(settings))
        .route("/v1/admin/tenants/{id}/suspend", post(suspend))
        .route("/v1/admin/tenants/{id}/resume", post(resume))
}
async fn list(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let store = store(&service)?;
    let tenants: Vec<Tenant> = store
        .tenants
        .lock()
        .map_err(internal)?
        .iter()
        .filter(|t| !t.deleted)
        .cloned()
        .collect();
    let users = service
        .admin
        .users
        .as_ref()
        .map(|users| users.count_by_tenant())
        .unwrap_or_default();
    let items = service
        .run_read(move |db| {
            Ok(tenants
                .iter()
                .map(|tenant| {
                    let collections = usage(db, &tenant.id);
                    let mut view = tenant.view();
                    view["collections"] = json!(collections.len());
                    view["vectors"] = json!(collections.iter().map(|c| c.2).sum::<u64>());
                    view["users"] = json!(users.get(&tenant.id).copied().unwrap_or(0));
                    view
                })
                .collect::<Vec<_>>())
        })
        .await?;
    Ok(Json(json!({"items": items})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    name: String,
    max_vectors: Option<u64>,
}
async fn create(
    State(service): State<Service>,
    input: Result<Json<Create>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let store = store(&service)?;
    let input = body(input)?;
    valid_name(&input.name)?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(internal)?;
    let tenant = Tenant {
        id: hex(&random),
        name: input.name,
        max_vectors: input.max_vectors,
        deleted: false,
        suspended_unix_ms: None,
    };
    let result = tenant.clone();
    // Serialize tenant mutations independently of authentication reads.
    let service_clone = service.clone();
    service
        .run(move |_db| {
            let mut next = store.tenants.lock().map_err(internal)?.clone();
            if next.len() >= 1024 {
                return Err(unprocessable("tenant capacity reached"));
            }
            next.push(tenant.clone());
            if let Some(log) = &service_clone.admin.audit_log {
                log.append(
                    json!({"event":"tenant_created", "tenant_id":tenant.id}),
                    true,
                )
                .map_err(internal)?;
            }
            store.commit(&next)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(json!({"tenant":result.view()}))))
}
async fn delete(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let store = store(&service)?;
    let service_clone = service.clone();
    let users_removed = service
        .run(move |db| {
            let mut next = store.tenants.lock().map_err(internal)?.clone();
            let tenant = next
                .iter_mut()
                .find(|t| t.id == id && !t.deleted)
                .ok_or_else(not_found)?;
            if db
                .collections()?
                .iter()
                .any(|c| c.name.starts_with(&format!("t{id}__")))
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "tenant_not_empty",
                    "drop tenant collections before deleting the tenant",
                ));
            }
            // Tenant graphs block deletion like tenant collections.
            #[cfg(feature = "graph")]
            if service_clone.graph_enabled
                && db
                    .list_graphs()?
                    .iter()
                    .any(|g| g.name.starts_with(&format!("t{id}__")))
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "tenant_not_empty",
                    "drop tenant graphs before deleting the tenant",
                ));
            }
            // Accounts go first: if this fails the tenant stays and a retry finishes.
            let users_removed = match (&service_clone.admin.users, &service_clone.admin.audit_log)
            {
                (Some(users), Some(log)) => users.remove_tenant(log, &id)?,
                _ => 0,
            };
            tenant.deleted = true;
            tenant.suspended_unix_ms = None;
            if let Some(log) = &service_clone.admin.audit_log {
                log.append(
                    json!({"event":"tenant_deleted", "tenant_id":id, "users_removed":users_removed}),
                    true,
                )
                .map_err(internal)?;
            }
            store.commit(&next)?;
            Ok(users_removed)
        })
        .await?;
    Ok(Json(json!({"deleted":true, "users_removed":users_removed})))
}

/// Applies `change` to one live tenant under the exclusive database lock,
/// audits `event` with `detail`, commits, and returns the updated view.
async fn mutate(
    service: Service,
    id: String,
    event: &'static str,
    change: impl FnOnce(&mut Tenant) -> Result<Value, ApiError> + Send + 'static,
) -> Result<Value, ApiError> {
    let store = store(&service)?;
    let service_clone = service.clone();
    service
        .run(move |_db| {
            let mut next = store.tenants.lock().map_err(internal)?.clone();
            let tenant = next
                .iter_mut()
                .find(|t| t.id == id && !t.deleted)
                .ok_or_else(not_found)?;
            let mut detail = change(tenant)?;
            let view = tenant.view();
            detail["event"] = json!(event);
            detail["tenant_id"] = json!(id);
            if let Some(log) = &service_clone.admin.audit_log {
                log.append(detail, true).map_err(internal)?;
            }
            store.commit(&next)?;
            Ok(view)
        })
        .await
}

async fn usage_report(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let store = store(&service)?;
    let tenant = store
        .tenants
        .lock()
        .map_err(internal)?
        .iter()
        .find(|t| t.id == id && !t.deleted)
        .cloned()
        .ok_or_else(not_found)?;
    let [active, expired, revoked] = match &service.admin.credentials {
        Some(credentials) => credentials.tenant_states(&id, admin::now_ms() as u64)?,
        None => [0; 3],
    };
    let collections = service.run_read(move |db| Ok(usage(db, &id))).await?;
    let vectors: u64 = collections.iter().map(|c| c.2).sum();
    Ok(Json(json!({
        "tenant": tenant.view(),
        "collections": collections.iter().map(|(name, physical, vectors)|
            json!({"name":name,"physical_name":physical,"vectors":vectors})).collect::<Vec<_>>(),
        "vectors": vectors,
        "max_vectors": tenant.max_vectors,
        "remaining_vectors": tenant.max_vectors.map(|max| max.saturating_sub(vectors)),
        "over_quota": tenant.max_vectors.is_some_and(|max| vectors > max),
        "credentials": {"active":active,"expired":expired,"revoked":revoked},
    })))
}

/// Explicit `null` deserializes as `Some(None)`; an omitted field stays `None`.
pub(super) fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, deserialize_with = "present")]
    max_vectors: Option<Option<u64>>,
}
async fn settings(
    State(service): State<Service>,
    Path(id): Path<String>,
    input: Result<Json<Settings>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    store(&service)?;
    let input = body(input)?;
    if input.name.is_none() && input.max_vectors.is_none() {
        return Err(unprocessable("provide name and/or max_vectors"));
    }
    if let Some(name) = &input.name {
        valid_name(name)?;
    }
    let tenant = mutate(service, id, "tenant_updated", move |tenant| {
        let mut changed = Vec::new();
        if let Some(name) = input.name {
            tenant.name = name;
            changed.push("name");
        }
        if let Some(limit) = input.max_vectors {
            tenant.max_vectors = limit;
            changed.push("max_vectors");
        }
        Ok(json!({"changed": changed, "max_vectors": tenant.max_vectors}))
    })
    .await?;
    Ok(Json(json!({ "tenant": tenant })))
}
async fn suspend(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let tenant = mutate(service, id, "tenant_suspended", |tenant| {
        let already = tenant.suspended_unix_ms.is_some();
        if !already {
            tenant.suspended_unix_ms = Some(admin::now_ms() as u64);
        }
        Ok(json!({ "already_suspended": already }))
    })
    .await?;
    Ok(Json(json!({ "tenant": tenant })))
}
async fn resume(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let tenant = mutate(service, id, "tenant_resumed", |tenant| {
        let was = tenant.suspended_unix_ms.take().is_some();
        Ok(json!({ "was_suspended": was }))
    })
    .await?;
    Ok(Json(json!({ "tenant": tenant })))
}

pub(super) fn qualify(role: &admin::Role, name: &str) -> String {
    match role {
        admin::Role::Scoped(grant) => grant.qualify(name),
        _ => name.to_owned(),
    }
}
pub(super) fn unqualify(role: &admin::Role, name: &str) -> String {
    if let admin::Role::Scoped(grant) = role
        && let Some(id) = &grant.tenant_id
    {
        return name
            .strip_prefix(&format!("t{id}__"))
            .unwrap_or(name)
            .to_owned();
    }
    name.to_owned()
}
