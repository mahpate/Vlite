//! Managed inference on existing REST routes; legacy inference stays separate.
use super::*;
use providers::{BindingKey, BindingSnapshot, Candidate, ProviderError};
use std::time::Duration;

impl From<ProviderError> for ApiError {
    fn from(error: ProviderError) -> Self {
        let (status, code, _) = error.mapping();
        Self::new(
            StatusCode::from_u16(status).expect("fixed provider status"),
            code,
            code,
        )
    }
}

// Ownership ratified 2026-11-03: only the collection's owning tenant binds.
fn binding_tenant(
    service: &Service,
    _role: &admin::Role,
    name: &str,
) -> Result<Option<String>, ApiError> {
    let tenant = name
        .strip_prefix('t')
        .and_then(|name| name.split_once("__"))
        .map(|(id, _)| id)
        .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()));
    let Some(tenant) = tenant else {
        return Ok(None);
    };
    service
        .admin
        .tenants
        .as_ref()
        .ok_or(ProviderError::NotConfigured)?
        .get(tenant)?;
    Ok(Some(tenant.to_owned()))
}

// REST mapping ratified 2026-11-03; the caller retries the whole request.
fn snapshot_conflict() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "provider_snapshot_conflict",
        "provider request snapshot changed",
    )
}

// Payload rule ratified 2026-11-03. Serving always selects reject_non_json=true.
fn approved_payload(
    bytes: &[u8],
    fields: &[String],
    reject_non_json: bool,
) -> Result<BTreeMap<String, Value>, ProviderError> {
    if fields.is_empty() {
        return Ok(BTreeMap::new());
    }
    let object = match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(object)) => object,
        _ if reject_non_json => return Err(ProviderError::InvalidInput),
        _ => return Ok(BTreeMap::new()),
    };
    Ok(fields
        .iter()
        .filter_map(|field| {
            object
                .get(field)
                .map(|value| (field.clone(), value.clone()))
        })
        .collect())
}

fn fresh_role(
    service: &Service,
    headers: &HeaderMap,
    target: &Target,
) -> Result<admin::Role, ApiError> {
    let path = if target.legacy() {
        "/v1/search".to_owned()
    } else {
        format!("/v1/collections/{}/search", target.name())
    };
    let role = match users::resolve(service, &Method::POST, &path, headers) {
        Some(result) => result?.role,
        None => authenticate(service, headers)?.ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "authentication required",
            )
        })?,
    };
    authorize_need(
        service,
        &role,
        Need::Op(credentials::Op::Search, Reach::Collection),
        true,
        target.legacy(),
    )?;
    Ok(role)
}

struct Snapshot {
    key: BindingKey,
    binding: BindingSnapshot,
    dimensions: usize,
    revisions: Vec<(u64, Option<u64>, Option<u64>)>,
}

fn capture(
    service: &Service,
    db: &Database,
    headers: &HeaderMap,
    target: &Target,
    alias: &str,
    embedding: bool,
) -> Result<(Snapshot, admin::Role), ApiError> {
    // Authorization and exact collection access precede even alias lookup.
    let role = fresh_role(service, headers, target)?;
    let id = open_guarded(db, &role, target, &service.memory_bindings)?;
    let collection = CollectionRef { db, id };
    let tenant =
        binding_tenant(service, &role, collection.name())?.ok_or(ProviderError::NotConfigured)?;
    let key = (
        db.lineage().ok_or(ProviderError::NotConfigured)?,
        id,
        tenant.to_owned(),
        alias.to_owned(),
    );
    let binding = service.providers().snapshot(&key, embedding)?;
    if embedding && binding.dimensions != Some(collection.dimensions()) {
        return Err(ProviderError::InvalidInput.into());
    }
    Ok((
        Snapshot {
            key,
            binding,
            dimensions: collection.dimensions(),
            revisions: Vec::new(),
        },
        role,
    ))
}

fn recheck(
    service: &Service,
    db: &Database,
    headers: &HeaderMap,
    target: &Target,
    snapshot: &Snapshot,
    started: Instant,
) -> Result<(), ApiError> {
    let role = fresh_role(service, headers, target)?;
    let id = open_guarded(db, &role, target, &service.memory_bindings).map_err(|error| {
        if error.status == StatusCode::NOT_FOUND {
            snapshot_conflict()
        } else {
            error
        }
    })?;
    let collection = CollectionRef { db, id };
    let tenant =
        binding_tenant(service, &role, collection.name())?.ok_or_else(snapshot_conflict)?;
    if db.lineage() != Some(snapshot.key.0)
        || id != snapshot.key.1
        || tenant != snapshot.key.2
        || collection.dimensions() != snapshot.dimensions
        || !service
            .providers()
            .unchanged(&snapshot.key, &snapshot.binding)
    {
        return Err(snapshot_conflict());
    }
    let now = crate::current_time_ms();
    for &(id, revision, expiry) in &snapshot.revisions {
        if collection.revision(id)? != revision
            || collection.expires_at(id)? != expiry
            || collection.get_at(id, now)?.is_none()
        {
            return Err(snapshot_conflict());
        }
    }
    if started.elapsed() >= snapshot.binding.timeout {
        return Err(ProviderError::Timeout.into());
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub(super) struct EmbedResponse {
    pub(super) dimensions: usize,
    pub(super) provider: String,
    pub(super) vector: Vec<f32>,
}

pub(super) async fn embed(
    service: &Service,
    headers: HeaderMap,
    target: Target,
    alias: String,
    text: String,
    started: Instant,
) -> Result<EmbedResponse, ApiError> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let worker = service.clone();
        let initial_headers = headers.clone();
        let initial_target = target.clone();
        let initial_alias = alias.clone();
        let snapshot = service
            .run_read(move |db| {
                let (snapshot, _) = capture(
                    &worker,
                    db,
                    &initial_headers,
                    &initial_target,
                    &initial_alias,
                    true,
                )?;
                if text.len() > 16_384 {
                    return Err(ProviderError::InvalidInput.into());
                }
                Ok((snapshot, text))
            })
            .await?;
        let (snapshot, text) = snapshot;
        // The original start includes database scheduling and snapshot time.
        let vector = service
            .providers()
            .embed_bound(&snapshot.key, &text, started)
            .await?;
        let dimensions = snapshot.dimensions;
        let worker = service.clone();
        service
            .run_read(move |db| recheck(&worker, db, &headers, &target, &snapshot, started))
            .await?;
        Ok(EmbedResponse {
            vector,
            dimensions,
            provider: alias,
        })
    })
    .await
    .map_err(|_| ApiError::from(ProviderError::Timeout))?
}

pub(super) async fn search(
    service: &Service,
    headers: HeaderMap,
    target: Target,
    input: Search,
    started: Instant,
) -> Result<Value, ApiError> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let worker = service.clone();
        let initial_headers = headers.clone();
        let initial_target = target.clone();
        let (snapshot, input, executed, candidates, alias) = service
            .run(move |db| {
                let role = fresh_role(&worker, &initial_headers, &initial_target)?;
                // Validate the custom shape without changing legacy validation/errors.
                input.validate(&role).map_err(|error| {
                    if error.status == StatusCode::UNPROCESSABLE_ENTITY {
                        ProviderError::InvalidInput.into()
                    } else {
                        error
                    }
                })?;
                let alias = input
                    .rerank
                    .as_ref()
                    .and_then(|r| r.provider.clone())
                    .ok_or(ProviderError::InvalidInput)?;
                let (mut snapshot, _) = capture(
                    &worker,
                    db,
                    &initial_headers,
                    &initial_target,
                    &alias,
                    false,
                )?;
                let id = snapshot.key.1;
                if input.vector.as_ref().is_none_or(|vector| {
                    vector.len() != snapshot.dimensions
                        || vector.iter().any(|value| !value.is_finite())
                }) {
                    return Err(ProviderError::InvalidInput.into());
                }
                let executed = match execute(db, id, &initial_target, &input, true)? {
                    Some(executed) => executed,
                    None => {
                        rebuild_stale(db, id, &initial_target, &input, &worker.metrics)?;
                        execute(db, id, &initial_target, &input, false)?
                            .ok_or_else(|| internal("index stale after rebuild"))?
                    }
                };
                let Hits::Vector(hits) = &executed.hits else {
                    return Err(ProviderError::InvalidInput.into());
                };
                if hits.len() < input.k {
                    return Err(ProviderError::InvalidInput.into());
                }
                let collection = CollectionRef { db, id };
                let mut candidates = Vec::with_capacity(hits.len());
                for hit in hits {
                    let record = collection.get(hit.id)?.ok_or_else(snapshot_conflict)?;
                    let payload =
                        approved_payload(&record.payload, &snapshot.binding.payload_fields, true)?;
                    candidates.push(Candidate {
                        id: hit.id,
                        vector: record.vector.clone(),
                        payload,
                    });
                    snapshot.revisions.push((
                        hit.id,
                        collection.revision(hit.id)?,
                        collection.expires_at(hit.id)?,
                    ));
                }
                Ok((snapshot, input, executed, candidates, alias))
            })
            .await?;
        let ranked = service
            .providers()
            .rerank_bound(
                &snapshot.key,
                input.vector.as_deref().ok_or(ProviderError::InvalidInput)?,
                &candidates,
                input.k,
                started,
            )
            .await?;
        let Hits::Vector(hits) = &executed.hits else {
            return Err(ProviderError::InvalidInput.into());
        };
        let distances: BTreeMap<_, _> = hits.iter().map(|hit| (hit.id, hit.distance)).collect();
        let ranked_hits: Vec<_> = ranked.into_iter().map(|hit| {
            json!({"id":hit.id.to_string(),"distance":distances[&hit.id],"rerank_score":0.0 - hit.cost})
        }).collect();
        let mut response =
            json!({"hits":ranked_hits,"ranking":{"provider":alias,"order":"score_desc_id_asc"}});
        if input.explain {
            let mut stats = executed.stats;
            stats.reranked_candidates = candidates.len() as u64;
            response["stats"] = stats_json(&stats);
            response["plan"] = executed.plan.as_ref().map_or(Value::Null, plan_json);
        }
        let worker = service.clone();
        service
            .run_read(move |db| recheck(&worker, db, &headers, &target, &snapshot, started))
            .await?;
        Ok(response)
    })
    .await
    .map_err(|_| ApiError::from(ProviderError::Timeout))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_precedes_binding_lookup_even_without_middleware() {
        let db = Database::create_in_memory(2, crate::CreateOptions::default()).unwrap();
        let service =
            Service::new(Database::create_in_memory(2, crate::CreateOptions::default()).unwrap());
        service.admin.auth.lock().unwrap().admin =
            Some(BearerToken::new("fake-admin-token-for-provider-tests").unwrap());
        let result = capture(
            &service,
            &db,
            &HeaderMap::new(),
            &Target::Legacy,
            "missing-alias",
            true,
        );
        assert_eq!(result.err().unwrap().status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn owning_tenant_seam_never_uses_caller_identity() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("tenants.json"),
            r#"[{"id":"0123456789abcdef0123456789abcdef","name":"one","max_vectors":null}]"#,
        )
        .unwrap();
        let mut service =
            Service::new(Database::create_in_memory(2, crate::CreateOptions::default()).unwrap());
        Arc::get_mut(&mut service.admin).unwrap().tenants =
            Some(Arc::new(tenants::Store::load(directory.path()).unwrap()));
        let role = admin::Role::Admin;
        assert_eq!(binding_tenant(&service, &role, "default").unwrap(), None);
        assert_eq!(
            binding_tenant(&service, &role, "t0123456789abcdef0123456789abcdef__items").unwrap(),
            Some("0123456789abcdef0123456789abcdef".to_owned())
        );
        assert_eq!(
            binding_tenant(&service, &role, "tshort__items").unwrap(),
            None
        );
        assert_eq!(
            binding_tenant(&service, &role, "tzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz__items").unwrap(),
            None
        );
    }

    #[test]
    fn empty_allowlist_never_inspects_payload_bytes() {
        assert!(
            approved_payload(&[0xff, 0x00], &[], true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn payload_fields_are_explicit_top_level_json_only() {
        let fields = vec!["public".into()];
        assert_eq!(
            approved_payload(
                br#"{"public":1,"private":"secret","metadata":{"hidden":2}}"#,
                &fields,
                true
            )
            .unwrap(),
            BTreeMap::from([("public".into(), json!(1))])
        );
    }

    #[test]
    fn reject_non_json_payload_branch_is_explicit() {
        for bytes in [b"not-json".as_slice(), b"[]".as_slice()] {
            assert_eq!(
                approved_payload(bytes, &["public".into()], true),
                Err(ProviderError::InvalidInput)
            );
        }
    }

    #[test]
    fn json_arrays_and_scalars_are_rejected() {
        for bytes in [
            b"[]".as_slice(),
            b"42".as_slice(),
            b"true".as_slice(),
            b"null".as_slice(),
            br#""scalar""#.as_slice(),
        ] {
            assert_eq!(
                approved_payload(bytes, &["public".into()], true),
                Err(ProviderError::InvalidInput)
            );
        }
    }

    #[test]
    fn missing_named_payload_field_is_omitted() {
        assert_eq!(
            approved_payload(
                br#"{"other":1,"metadata":{"private":2}}"#,
                &["missing".into()],
                true
            ),
            Ok(BTreeMap::new())
        );
    }

    #[test]
    fn omit_non_json_payload_branch_is_explicit() {
        for bytes in [b"not-json".as_slice(), b"[]".as_slice()] {
            assert!(
                approved_payload(bytes, &["public".into()], false)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn typed_legacy_embedding_preserves_json_bytes() {
        let vector = vec![0.25, 0.75];
        let legacy = json!({"vector":vector,"dimensions":2,"provider":"hashing"});
        let typed = EmbedResponse {
            dimensions: 2,
            provider: "hashing".into(),
            vector,
        };
        assert_eq!(
            serde_json::to_vec(&typed).unwrap(),
            serde_json::to_vec(&legacy).unwrap()
        );
    }
}
