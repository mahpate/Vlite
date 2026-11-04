//! Ordinary key-value operations on explicitly authorized collections.
use super::*;
use crate::kv::{
    KvEntry, KvPut, MAX_KV_KEY_BYTES, MAX_KV_LIST_LIMIT, MAX_KV_NAMESPACE_BYTES, MAX_KV_VALUE_BYTES,
};
use axum::extract::RawQuery;

const PAGE_BYTES: usize = 16_777_216;

struct AuditDetails<'a> {
    operation: &'a str,
    expires_at_ms: Option<u64>,
}

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
pub(super) struct Address {
    namespace: String,
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    revision: String,
}

enum Expectation {
    Name(String),
    Revision(Revision),
    Other,
}

impl<'de> Deserialize<'de> for Expectation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ExpectationVisitor;

        impl<'de> serde::de::Visitor<'de> for ExpectationVisitor {
            type Value = Expectation;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("any, absent, or an object containing only revision")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(Expectation::Name(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(Expectation::Name(value))
            }

            fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
                Ok(Expectation::Other)
            }

            fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
                Ok(Expectation::Other)
            }

            fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
                Ok(Expectation::Other)
            }

            fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
                Ok(Expectation::Other)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(Expectation::Other)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                while sequence.next_element::<Value>()?.is_some() {}
                Ok(Expectation::Other)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let Some(field) = map.next_key::<String>()? else {
                    return Ok(Expectation::Other);
                };
                if field != "revision" {
                    return Err(serde::de::Error::unknown_field(&field, &["revision"]));
                }
                let value = map.next_value::<Value>()?;
                if map.next_key::<String>()?.is_some() {
                    return Err(serde::de::Error::duplicate_field("revision"));
                }
                match value {
                    Value::String(revision) => Ok(Expectation::Revision(Revision { revision })),
                    _ => Err(serde::de::Error::custom(
                        "revision must be a canonical decimal string",
                    )),
                }
            }
        }

        deserializer.deserialize_any(ExpectationVisitor)
    }
}

impl Default for Expectation {
    fn default() -> Self {
        Self::Name("any".into())
    }
}

impl Expectation {
    fn parse(&self) -> Result<Expected, ApiError> {
        match self {
            Self::Name(name) if name == "any" || name == "absent" => expected(name),
            Self::Name(_) => Err(unprocessable(
                "expected must be any, absent, or a revision object",
            )),
            Self::Revision(revision) => Ok(Expected::Revision(decimal(&revision.revision)?)),
            Self::Other => Err(unprocessable(
                "expected must be any, absent, or a revision object",
            )),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PutInput {
    namespace: String,
    key: String,
    value_base64: String,
    #[serde(default)]
    expected: Expectation,
    expires_at_ms: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteQuery {
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
pub(super) struct ListQuery {
    namespace: String,
    #[serde(default)]
    prefix: String,
    after: Option<String>,
    limit: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NoQuery {}

fn query<T>(input: Result<Query<T>, QueryRejection>) -> Result<T, ApiError> {
    input.map(|Query(input)| input).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "invalid query parameters",
        )
    })
}

fn strict_query_encoding(query: Option<&str>) -> Result<(), ApiError> {
    let Some(query) = query else {
        return Ok(());
    };
    for component in query.split('&') {
        let (key, value) = component.split_once('=').unwrap_or((component, ""));
        for field in [key, value] {
            let bytes = field.as_bytes();
            let mut decoded = Vec::with_capacity(bytes.len());
            let mut index = 0;
            while index < bytes.len() {
                match bytes[index] {
                    b'+' => {
                        decoded.push(b' ');
                        index += 1;
                    }
                    b'%' => {
                        let hex = bytes.get(index + 1..index + 3).ok_or_else(invalid_query)?;
                        let digit = |byte: u8| match byte {
                            b'0'..=b'9' => Some(byte - b'0'),
                            b'a'..=b'f' => Some(byte - b'a' + 10),
                            b'A'..=b'F' => Some(byte - b'A' + 10),
                            _ => None,
                        };
                        let high = digit(hex[0]).ok_or_else(invalid_query)?;
                        let low = digit(hex[1]).ok_or_else(invalid_query)?;
                        decoded.push(high << 4 | low);
                        index += 3;
                    }
                    byte => {
                        decoded.push(byte);
                        index += 1;
                    }
                }
            }
            if std::str::from_utf8(&decoded).is_err() {
                return Err(invalid_query());
            }
        }
    }
    Ok(())
}

fn invalid_query() -> ApiError {
    ApiError::new(
        StatusCode::BAD_REQUEST,
        "invalid_query",
        "query parameters must use valid UTF-8 percent encoding",
    )
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

pub(super) async fn fetch(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    RawQuery(raw_query): RawQuery,
    input: Result<Query<Address>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    strict_query_encoding(raw_query.as_deref())?;
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

pub(super) async fn save(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request_id): Extension<RequestId>,
    target: Result<Path<String>, PathRejection>,
    RawQuery(raw_query): RawQuery,
    no_query: Result<Query<NoQuery>, QueryRejection>,
    input: Result<Json<PutInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    strict_query_encoding(raw_query.as_deref())?;
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
        AuditDetails {
            operation: "kv_put",
            expires_at_ms: options.expires_at_ms,
        },
        &result,
        started,
    );
    result
}

pub(super) async fn remove(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request_id): Extension<RequestId>,
    target: Result<Path<String>, PathRejection>,
    RawQuery(raw_query): RawQuery,
    input: Result<Query<DeleteQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    strict_query_encoding(raw_query.as_deref())?;
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
        AuditDetails {
            operation: "kv_delete",
            expires_at_ms: None,
        },
        &result,
        started,
    );
    result
}

pub(super) async fn list(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    RawQuery(raw_query): RawQuery,
    input: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    strict_query_encoding(raw_query.as_deref())?;
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
            let page = build_page(input, limit, now, |namespace, prefix, after, count, now| {
                view.list_at(namespace, prefix, after, count, now)
                    .map_err(ApiError::from)
            })?;
            Ok(Json(page))
        })
        .await
}

fn build_page(
    input: ListQuery,
    limit: u64,
    now: u64,
    mut list_at: impl FnMut(&str, &str, Option<&str>, usize, u64) -> Result<Vec<KvEntry>, ApiError>,
) -> Result<Value, ApiError> {
    let mut cursor = input.after;
    let mut items = Vec::new();
    let mut item_bytes = 0;
    let mut chunk_size = 16;
    loop {
        let entries = list_at(
            &input.namespace,
            &input.prefix,
            cursor.as_deref(),
            chunk_size,
            now,
        )?;
        if entries.is_empty() {
            return Ok(json!({"items":items,"next_after":null}));
        }
        let last = &entries.last().expect("nonempty chunk").key;
        let more = !list_at(&input.namespace, &input.prefix, Some(last), 1, now)?.is_empty();
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
                return Ok(json!({"items":items,"next_after":cursor}));
            }
            item_bytes += size;
            cursor = Some(key);
            items.push(item);
        }
        if !more {
            return Ok(json!({"items":items,"next_after":null}));
        }
        chunk_size = chunk_size.saturating_mul(2).min(MAX_KV_LIST_LIMIT);
    }
}

fn audit<T>(
    service: &Service,
    role: &admin::Role,
    target: &Target,
    request_id: &RequestId,
    details: AuditDetails<'_>,
    result: &Result<T, ApiError>,
    started: Instant,
) {
    let principal = match role {
        admin::Role::Scoped(grant) => grant.id.as_str(),
        role => role.name(),
    };
    let outcome = result.as_ref().map_or_else(|error| error.code, |_| "ok");
    // Success proves `open` accepted the collection and its binding.
    let collection = if result.is_ok() && crate::format::valid_collection_name(target.name()) {
        tenants::qualify(role, target.name())
    } else {
        "(invalid)".to_owned()
    };
    if let Some(log) = &service.admin.audit_log
        && log
            .append(
                json!({
                    "event":details.operation,"request_id":request_id.0,"principal":principal,
                    "collection":collection,"outcome":outcome,
                    "expires_at_ms":details.expires_at_ms.map(|value|value.to_string()),
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
    use std::cell::Cell;

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
    fn large_page_uses_bounded_number_of_list_scans() {
        let entries = (0..2_000)
            .map(|index| KvEntry {
                key: format!("{index:05}"),
                value: b"x".to_vec(),
                version: 1,
                expires_at_ms: None,
            })
            .collect::<Vec<_>>();
        let scans = Cell::new(0);
        let page = build_page(
            ListQuery {
                namespace: "bulk".into(),
                prefix: String::new(),
                after: None,
                limit: Some("10000".into()),
            },
            10_000,
            0,
            |_, _, after, count, _| {
                scans.set(scans.get() + 1);
                Ok(entries
                    .iter()
                    .filter(|entry| after.is_none_or(|after| entry.key.as_str() > after))
                    .take(count)
                    .cloned()
                    .collect())
            },
        )
        .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 2_000);
        assert!(
            scans.get() <= 20,
            "page required {} full-list scans",
            scans.get()
        );
    }

    #[tokio::test]
    async fn audit_never_stores_unvalidated_collection_names() {
        let directory = tempfile::tempdir().unwrap();
        let mut service = Service::new(Database::create_in_memory(1, Default::default()).unwrap());
        let mut admin_state = admin::AdminState::default();
        admin_state.audit_log = Some(Arc::new(audit::Log::open(directory.path()).unwrap()));
        service.admin = Arc::new(admin_state);
        let target = Target::Named("a".repeat(1024 * 1024));
        let request_id = RequestId("test".into());
        let failure: Result<(), ApiError> = Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "collection_not_found",
            "missing",
        ));
        audit(
            &service,
            &admin::Role::Admin,
            &target,
            &request_id,
            AuditDetails {
                operation: "kv_put",
                expires_at_ms: None,
            },
            &failure,
            Instant::now(),
        );
        let contents = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
        assert!(
            contents.len() < 4_096,
            "audit log was {} bytes",
            contents.len()
        );
        let event: Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(event["collection"], "(invalid)");
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
