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
    pub fn known(&self, id: &str) -> Result<bool, ApiError> {
        Ok(self
            .tenants
            .lock()
            .map_err(internal)?
            .iter()
            .any(|t| t.id == id))
    }
    pub fn get(&self, id: &str) -> Result<Tenant, ApiError> {
        self.tenants
            .lock()
            .map_err(internal)?
            .iter()
            .find(|t| t.id == id && !t.deleted)
            .cloned()
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::FORBIDDEN,
                    "tenant_unavailable",
                    "tenant is unavailable",
                )
            })
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
}
async fn list(State(service): State<Service>) -> Result<Json<Value>, ApiError> {
    let store = store(&service)?;
    let tenants = store.tenants.lock().map_err(internal)?;
    Ok(Json(
        json!({"items": tenants.iter().filter(|t| !t.deleted).collect::<Vec<_>>()}),
    ))
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
    if input.name.is_empty() || input.name.len() > 64 || input.name.chars().any(char::is_control) {
        return Err(unprocessable("name must be 1..64 printable characters"));
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(internal)?;
    let tenant = Tenant {
        id: hex(&random),
        name: input.name,
        max_vectors: input.max_vectors,
        deleted: false,
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
    Ok((StatusCode::CREATED, Json(json!({"tenant":result}))))
}
async fn delete(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let store = store(&service)?;
    let service_clone = service.clone();
    service
        .run(move |db| {
            let mut next = store.tenants.lock().map_err(internal)?.clone();
            let tenant = next
                .iter_mut()
                .find(|t| t.id == id && !t.deleted)
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::NOT_FOUND,
                        "tenant_not_found",
                        "tenant not found",
                    )
                })?;
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
            tenant.deleted = true;
            if let Some(log) = &service_clone.admin.audit_log {
                log.append(json!({"event":"tenant_deleted", "tenant_id":id}), true)
                    .map_err(internal)?;
            }
            store.commit(&next)
        })
        .await?;
    Ok(Json(json!({"deleted":true})))
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
