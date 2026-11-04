//! Ordinary key-value operations on explicitly authorized collections.
use super::*;
use crate::kv::{
    KvEntry, KvPut, MAX_KV_KEY_BYTES, MAX_KV_LIST_LIMIT, MAX_KV_NAMESPACE_BYTES, MAX_KV_VALUE_BYTES,
};

const PAGE_BYTES: usize = 16_777_216;

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route(
            "/v1/collections/{collection}/kv/entry",
            get(fetch).put(save).delete(remove),
        )
        .route("/v1/collections/{collection}/kv/entries", get(list))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Address {
    namespace: String,
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    revision: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Expectation {
    Name(String),
    Revision(Revision),
}

impl Default for Expectation {
    fn default() -> Self {
        Self::Name("any".into())
    }
}

impl Expectation {
    fn parse(&self) -> Result<Expected, ApiError> {
        match self {
            Self::Name(name) => expected(name),
            Self::Revision(revision) => Ok(Expected::Revision(decimal(&revision.revision)?)),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutInput {
    namespace: String,
    key: String,
    value_base64: String,
    #[serde(default)]
    expected: Expectation,
    expires_at_ms: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    namespace: String,
    key: String,
    #[serde(default = "any")]
    expected: String,
}

fn any() -> String {
    "any".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    namespace: String,
    #[serde(default)]
    prefix: String,
    after: Option<String>,
    limit: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoQuery {}

fn query<T>(input: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    input.map(|Query(input)| input).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "invalid query parameters",
        )
    })
}

fn decimal(text: &str) -> Result<u64, ApiError> {
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(unprocessable("expected a canonical uint64 decimal string"));
    }
    text.parse()
        .map_err(|_| unprocessable("decimal string exceeds uint64"))
}

fn expected(text: &str) -> Result<Expected, ApiError> {
    match text {
        "any" => Ok(Expected::Any),
        "absent" => Ok(Expected::Absent),
        revision => Ok(Expected::Revision(decimal(revision)?)),
    }
}

fn bounded(text: &str, minimum: usize, maximum: usize) -> Result<(), ApiError> {
    if text.len() < minimum || text.len() > maximum {
        return Err(unprocessable("KV string exceeds its UTF-8 byte bounds"));
    }
    Ok(())
}

fn address(namespace: &str, key: &str) -> Result<(), ApiError> {
    bounded(namespace, 1, MAX_KV_NAMESPACE_BYTES)?;
    bounded(key, 1, MAX_KV_KEY_BYTES)
}

fn value(text: &str) -> Result<Vec<u8>, ApiError> {
    if text.len() > MAX_KV_VALUE_BYTES.div_ceil(3) * 4 {
        return Err(unprocessable("KV value exceeds 1047290 decoded bytes"));
    }
    let bytes = BASE64_STANDARD
        .decode(text)
        .map_err(|_| unprocessable("KV value must be canonical padded standard base64"))?;
    if bytes.len() > MAX_KV_VALUE_BYTES || BASE64_STANDARD.encode(&bytes) != text {
        return Err(unprocessable(
            "KV value exceeds its bound or is not canonical base64",
        ));
    }
    Ok(bytes)
}

fn entry_json(entry: KvEntry) -> Value {
    json!({
        "key":entry.key, "value_base64":BASE64_STANDARD.encode(entry.value),
        "version":entry.version.to_string(),
        "expires_at_ms":entry.expires_at_ms.map(|value|value.to_string())
    })
}

fn writable(db: &Database) -> Result<(), ApiError> {
    if db.is_read_only() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "read_only",
            "database is read-only",
        ));
    }
    Ok(())
}

async fn fetch(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Query<Address>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let input = query(input)?;
    address(&input.namespace, &input.key)?;
    service
        .run_read(move |db| {
            open(db, &role, &target)?;
            let name = tenants::qualify(&role, target.name());
            let entry = db
                .kv_view(&name)?
                .get_at(&input.namespace, &input.key, crate::current_time_ms())?
                .ok_or_else(|| {
                    ApiError::new(StatusCode::NOT_FOUND, "kv_not_found", "KV entry not found")
                })?;
            Ok(Json(entry_json(entry)))
        })
        .await
}

async fn save(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request_id): Extension<RequestId>,
    target: Result<Path<String>, PathRejection>,
    no_query: Result<Query<NoQuery>, QueryRejection>,
    input: Result<Json<PutInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let started = Instant::now();
    let target = collection_path(target)?;
    query(no_query)?;
    let input = body(input)?;
    address(&input.namespace, &input.key)?;
    let bytes = value(&input.value_base64)?;
    let options = KvPut {
        expected: input.expected.parse()?,
        expires_at_ms: input.expires_at_ms.as_deref().map(decimal).transpose()?,
    };
    let retained = bytes
        .capacity()
        .saturating_add(input.namespace.capacity())
        .saturating_add(input.key.capacity())
        .saturating_add(target.retained_bytes())
        .saturating_add(4096);
    let quota_service = service.clone();
    let audit_role = role.clone();
    let audit_target = target.clone();
    let result = service
        .run_mutation(retained, move |db| {
            let collection = open(db, &role, &target)?;
            writable(db)?;
            let name = tenants::qualify(&role, target.name());
            let now = crate::current_time_ms();
            let slot = db.kv_view(&name)?.put_slot_at(
                &input.namespace,
                &input.key,
                options.expected,
                now,
            )?;
            enforce_quota(&quota_service, db, &role, collection, &[(slot, true)])?;
            let version =
                db.kv_store(&name)?
                    .put_at(&input.namespace, &input.key, &bytes, options, now)?;
            Ok(Json(json!({
                "version":version.to_string(),
                "expires_at_ms":options.expires_at_ms.map(|value|value.to_string())
            })))
        })
        .await;
    audit(
        &service,
        &audit_role,
        &audit_target,
        &request_id,
        "kv_put",
        &result,
        started,
    );
    result
}

async fn remove(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request_id): Extension<RequestId>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Query<DeleteQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let started = Instant::now();
    let target = collection_path(target)?;
    let input = query(input)?;
    address(&input.namespace, &input.key)?;
    let expected = expected(&input.expected)?;
    let audit_role = role.clone();
    let audit_target = target.clone();
    let retained =
        4096 + input.namespace.capacity() + input.key.capacity() + target.retained_bytes();
    let result = service
        .run_mutation(retained, move |db| {
            open(db, &role, &target)?;
            writable(db)?;
            let name = tenants::qualify(&role, target.name());
            let deleted = db.kv_store(&name)?.delete_at(
                &input.namespace,
                &input.key,
                expected,
                crate::current_time_ms(),
            )?;
            Ok(Json(json!({"deleted":deleted})))
        })
        .await;
    audit(
        &service,
        &audit_role,
        &audit_target,
        &request_id,
        "kv_delete",
        &result,
        started,
    );
    result
}

async fn list(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    input: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let target = collection_path(target)?;
    let input = query(input)?;
    bounded(&input.namespace, 1, MAX_KV_NAMESPACE_BYTES)?;
    bounded(&input.prefix, 0, MAX_KV_KEY_BYTES)?;
    if let Some(after) = &input.after {
        bounded(after, 1, MAX_KV_KEY_BYTES)?;
    }
    let limit = input
        .limit
        .as_deref()
        .map(decimal)
        .transpose()?
        .unwrap_or(100);
    if limit == 0 || limit > MAX_KV_LIST_LIMIT as u64 {
        return Err(unprocessable("KV list limit must be 1..10000"));
    }
    service
        .run_read(move |db| {
            open(db, &role, &target)?;
            let name = tenants::qualify(&role, target.name());
            let view = db.kv_view(&name)?;
            let now = crate::current_time_ms();
            let mut cursor = input.after;
            let mut items = Vec::new();
            let mut item_bytes = 0;
            loop {
                let entries =
                    view.list_at(&input.namespace, &input.prefix, cursor.as_deref(), 16, now)?;
                if entries.is_empty() {
                    return Ok(Json(json!({"items":items,"next_after":null})));
                }
                let last = &entries.last().expect("nonempty chunk").key;
                let more = !view
                    .list_at(&input.namespace, &input.prefix, Some(last), 1, now)?
                    .is_empty();
                let length = entries.len();
                for (index, entry) in entries.into_iter().enumerate() {
                    let has_more = index + 1 < length || more;
                    let key = entry.key.clone();
                    let item = entry_json(entry);
                    let size = serde_json::to_vec(&item).map_err(internal)?.len();
                    let next = has_more.then_some(&key);
                    let overhead = serde_json::to_vec(&json!({"items":[],"next_after":next}))
                        .map_err(internal)?
                        .len();
                    let page_bytes = overhead + item_bytes + size + items.len();
                    if items.len() == limit as usize || page_bytes > PAGE_BYTES {
                        if items.is_empty() {
                            return Err(internal("one KV entry exceeds the page boundary"));
                        }
                        return Ok(Json(json!({"items":items,"next_after":cursor})));
                    }
                    item_bytes += size;
                    cursor = Some(key);
                    items.push(item);
                }
                if !more {
                    return Ok(Json(json!({"items":items,"next_after":null})));
                }
            }
        })
        .await
}

fn audit<T>(
    service: &Service,
    role: &admin::Role,
    target: &Target,
    request_id: &RequestId,
    operation: &str,
    result: &Result<T, ApiError>,
    started: Instant,
) {
    let principal = match role {
        admin::Role::Scoped(grant) => grant.id.as_str(),
        role => role.name(),
    };
    let outcome = result.as_ref().map_or_else(|error| error.code, |_| "ok");
    if let Some(log) = &service.admin.audit_log
        && log
            .append(
                json!({
                    "event":operation,"request_id":request_id.0,"principal":principal,
                    "collection":tenants::qualify(role,target.name()),"outcome":outcome,
                    "duration_ns":started.elapsed().as_nanos().min(u64::MAX as u128) as u64
                }),
                false,
            )
            .is_err()
    {
        service
            .metrics
            .audit_failures
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_errors_have_explicit_client_mapping() {
        for (error, status, code) in [
            (
                Error::InvalidKvKey("invalid namespace"),
                422,
                "invalid_input",
            ),
            (Error::KvSlotsExhausted, 409, "kv_slots_exhausted"),
            (Error::NotKvCollection, 409, "not_kv_collection"),
        ] {
            let mapped = ApiError::from(error);
            assert_eq!(mapped.status.as_u16(), status);
            assert_eq!(mapped.code, code);
        }
    }

    #[test]
    fn decimal_expected_and_base64_are_canonical() {
        assert_eq!(decimal("18446744073709551615").unwrap(), u64::MAX);
        assert_eq!(decimal("9007199254740993").unwrap(), 9_007_199_254_740_993);
        for invalid in ["", "01", "+1", "-1", " 1", "18446744073709551616"] {
            assert_eq!(decimal(invalid).unwrap_err().code, "invalid_input");
        }
        assert_eq!(expected("0").unwrap(), Expected::Revision(0));
        assert_eq!(expected("absent").unwrap(), Expected::Absent);
        assert_eq!(value("").unwrap(), b"");
        assert_eq!(value("AAH/").unwrap(), [0, 1, 255]);
        for invalid in ["YQ", "YR==", "__8=", "YQ==\n"] {
            assert_eq!(value(invalid).unwrap_err().code, "invalid_input");
        }
    }
}
