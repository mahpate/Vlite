//! Ordinary key-value operations on explicitly authorized collections.
use super::*;
use crate::kv::{
    Gated, KvEntry, KvOp, KvOutcome, KvPut, KvRef, MAX_KV_BATCH_OPERATIONS,
    MAX_KV_BATCH_VALUE_BYTES, MAX_KV_GET_MANY_KEYS, MAX_KV_KEY_BYTES, MAX_KV_LIST_LIMIT,
    MAX_KV_NAMESPACE_BYTES, MAX_KV_VALUE_BYTES,
};
use axum::extract::RawQuery;
use std::{collections::HashMap, ops::ControlFlow};

const PAGE_BYTES: usize = 16_777_216;

// An entry always fits a page together with the cursor that names it, so a page
// is never empty for want of room: the largest value as base64, the key twice
// (item and cursor) at six bytes per key byte when escaped, and the field names.
// The key is bounded by the 16-bit length a stored record holds, not by
// `MAX_KV_KEY_BYTES`: a record planted through another route need not keep to it.
const _: () =
    assert!(MAX_KV_VALUE_BYTES.div_ceil(3) * 4 + 2 * 6 * (u16::MAX as usize) + 512 < PAGE_BYTES);

#[cfg(test)]
thread_local! {
    /// Value bytes this thread has base64-encoded for a response.
    static ENCODED_VALUE_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct AuditDetails<'a> {
    operation: &'a str,
    expires_at_ms: Option<u64>,
    /// The puts and deletes of a batch. A batch event carries these counts and
    /// never a namespace, key or value.
    counts: Option<(usize, usize)>,
    /// The dashboard session behind the request, when there is one.
    principal: Option<&'a users::Principal>,
}

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route(
            "/v1/collections/{collection}/kv/entry",
            get(fetch).put(save).delete(remove),
        )
        .route("/v1/collections/{collection}/kv/entries", get(list))
        .route("/v1/collections/{collection}/kv/batch", post(batch))
        .route(
            "/v1/collections/{collection}/kv/entries/get",
            post(fetch_many),
        )
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
}

impl<'de> Deserialize<'de> for Expectation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ExpectationVisitor;

        // Any other JSON type (number, boolean, null, array) falls through to the
        // visitor's default `invalid_type` error, which the extractor reports as
        // `invalid_json`: a wrong type is a shape failure, not invalid input.
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

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                // Read the whole array first so a syntax error inside it keeps its 400.
                while sequence.next_element::<Value>()?.is_some() {}
                Err(serde::de::Error::invalid_type(
                    serde::de::Unexpected::Seq,
                    &self,
                ))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let Some(field) = map.next_key::<String>()? else {
                    return Err(serde::de::Error::missing_field("revision"));
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

/// `POST …/kv/batch`: puts and deletes of one collection, applied together.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BatchInput {
    operations: Vec<BatchOperation>,
}

/// A put has no `expires_at_ms`: a batch commits as one engine batch frame,
/// which has no deadline, so the field is unknown and refused.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum BatchOperation {
    Put {
        namespace: String,
        key: String,
        value_base64: String,
        #[serde(default)]
        expected: Expectation,
    },
    Delete {
        namespace: String,
        key: String,
        #[serde(default)]
        expected: Expectation,
    },
}

/// `POST …/kv/entries/get`: the entries of up to 1,000 named keys.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GetManyInput {
    keys: Vec<Address>,
}

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
    item_json(&entry.key, &entry.value, entry.version, entry.expires_at_ms)
}

/// One entry of a response: the key, the base64 value, and the decimal
/// version and deadline.
fn item_json(key: &str, value: &[u8], version: u64, expires_at_ms: Option<u64>) -> Value {
    #[cfg(test)]
    ENCODED_VALUE_BYTES.with(|bytes| bytes.set(bytes.get() + value.len()));
    json!({
        "key":key, "value_base64":BASE64_STANDARD.encode(value),
        "version":version.to_string(),
        "expires_at_ms":expires_at_ms.map(|value|value.to_string())
    })
}

/// The serialized size of `item_json`, found from the length of the value alone
/// so that a value is never encoded just to be measured: the item with an empty
/// value plus four base64 characters for every started group of three value
/// bytes.
fn item_size(
    key: &str,
    value_len: usize,
    version: u64,
    expires_at_ms: Option<u64>,
) -> Result<usize, ApiError> {
    let shell = serde_json::to_vec(&json!({
        "key":key, "value_base64":"",
        "version":version.to_string(),
        "expires_at_ms":expires_at_ms.map(|value|value.to_string())
    }))
    .map_err(internal)?;
    Ok(shell.len() + value_len.div_ceil(3) * 4)
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

/// Engine errors of the write routes. A failed precondition carries no detail:
/// the shared mapping names the live revision, or that the key is absent, and a
/// caller that may write need not be allowed to read. Only the KV routes use
/// this; every other route keeps the shared mapping.
fn kv_failure(error: Error) -> ApiError {
    match error {
        Error::RevisionMismatch { .. } => ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            "precondition_failed",
            "revision precondition failed",
        ),
        error => error.into(),
    }
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
    let scope = service.scope(role);
    service
        .run_read(move |db| {
            let entry = scope
                .collection(db, &target)?
                .kv_view(db)?
                .get_at(&input.namespace, &input.key, crate::current_time_ms())?
                .ok_or_else(|| {
                    ApiError::new(StatusCode::NOT_FOUND, "kv_not_found", "KV entry not found")
                })?;
            Ok(Json(entry_json(entry)))
        })
        .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn save(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request_id): Extension<RequestId>,
    principal: Option<Extension<users::Principal>>,
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
    let scope = service.scope(role);
    let result = service
        .run_mutation(retained, move |db| {
            let collection = scope.collection(db, &target)?;
            writable(db)?;
            let now = crate::current_time_ms();
            let slot = collection
                .kv_view(db)?
                .put_slot_at(&input.namespace, &input.key, options.expected, now)
                .map_err(kv_failure)?;
            enforce_quota(
                &quota_service,
                db,
                scope.role(),
                collection.id(),
                &[(slot, true)],
            )?;
            let version = collection
                .kv_store(db)?
                .put_at(&input.namespace, &input.key, &bytes, options, now)
                .map_err(kv_failure)?;
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
            counts: None,
            principal: principal.as_ref().map(|Extension(principal)| principal),
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
    principal: Option<Extension<users::Principal>>,
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
    let scope = service.scope(role);
    let retained =
        4096 + input.namespace.capacity() + input.key.capacity() + target.retained_bytes();
    let result = service
        .run_mutation(retained, move |db| {
            let collection = scope.collection(db, &target)?;
            writable(db)?;
            let deleted = collection
                .kv_store(db)?
                .delete_at(
                    &input.namespace,
                    &input.key,
                    expected,
                    crate::current_time_ms(),
                )
                .map_err(kv_failure)?;
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
            counts: None,
            principal: principal.as_ref().map(|Extension(principal)| principal),
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
    let scope = service.scope(role);
    service
        .run_read(move |db| {
            let view = scope.collection(db, &target)?.kv_view(db)?;
            let now = crate::current_time_ms();
            let page = build_page(
                input,
                limit,
                now,
                |namespace, prefix, after, count, now, visit| {
                    view.visit_at(namespace, prefix, after, count, now, visit)
                        .map_err(ApiError::from)
                },
            )?;
            Ok(Json(page))
        })
        .await
}

/// Prefixes the message of a request error with the position of the part of
/// the request it is about.
pub(super) fn at(list: &str, index: usize, error: ApiError) -> ApiError {
    ApiError {
        message: format!("{list}[{index}]: {}", error.message),
        ..error
    }
}

/// A batch request after every check that needs no store: counts and bounds,
/// values decoded, conditions parsed, no key named twice.
struct Prepared {
    names: Vec<(String, String)>,
    /// The decoded value of a put; `None` for a delete.
    values: Vec<Option<Vec<u8>>>,
    conditions: Vec<Expected>,
}

impl Prepared {
    fn puts(&self) -> usize {
        self.values.iter().flatten().count()
    }

    fn retained_bytes(&self) -> usize {
        self.names
            .iter()
            .zip(&self.values)
            .fold(0usize, |total, ((namespace, key), value)| {
                total
                    .saturating_add(namespace.capacity())
                    .saturating_add(key.capacity())
                    .saturating_add(value.as_ref().map_or(0, Vec::capacity))
                    .saturating_add(64)
            })
    }

    fn operations(&self) -> Vec<KvOp<'_>> {
        self.names
            .iter()
            .zip(&self.values)
            .zip(&self.conditions)
            .map(|(((namespace, key), value), &expected)| match value {
                Some(value) => KvOp::Put {
                    namespace,
                    key,
                    value,
                    options: KvPut {
                        expected,
                        expires_at_ms: None,
                    },
                },
                None => KvOp::Delete {
                    namespace,
                    key,
                    expected,
                },
            })
            .collect()
    }
}

/// The count check of a batch. The gRPC adapter runs it before it converts a
/// single operation, so a message of many tiny operations is refused without
/// building a JSON value for each.
pub(super) fn check_operation_count(count: usize) -> Result<(), ApiError> {
    if count > MAX_KV_BATCH_OPERATIONS {
        return Err(unprocessable(format!(
            "a batch takes at most {MAX_KV_BATCH_OPERATIONS} operations"
        )));
    }
    Ok(())
}

/// The count check of a get-many, shared with the gRPC adapter like
/// `check_operation_count`.
pub(super) fn check_key_count(count: usize) -> Result<(), ApiError> {
    if count > MAX_KV_GET_MANY_KEYS {
        return Err(unprocessable(format!(
            "a get-many takes at most {MAX_KV_GET_MANY_KEYS} keys"
        )));
    }
    Ok(())
}

/// Checks a whole batch request in operation order, so the first operation
/// that cannot be valid is the one reported, whatever the state of the store.
/// Messages name the position of the operation.
fn prepare(input: BatchInput) -> Result<Prepared, ApiError> {
    let operations = input.operations;
    check_operation_count(operations.len())?;
    let mut values = Vec::with_capacity(operations.len());
    let mut conditions = Vec::with_capacity(operations.len());
    let mut value_bytes = 0usize;
    {
        let mut named: HashMap<(&str, &str), usize> = HashMap::with_capacity(operations.len());
        for (index, operation) in operations.iter().enumerate() {
            let (namespace, key, text, condition) = match operation {
                BatchOperation::Put {
                    namespace,
                    key,
                    value_base64,
                    expected,
                } => (namespace, key, Some(value_base64), expected),
                BatchOperation::Delete {
                    namespace,
                    key,
                    expected,
                } => (namespace, key, None, expected),
            };
            address(namespace, key).map_err(|error| at("operations", index, error))?;
            let bytes = text
                .map(|text| value(text))
                .transpose()
                .map_err(|error| at("operations", index, error))?;
            value_bytes += bytes.as_ref().map_or(0, Vec::len);
            if value_bytes > MAX_KV_BATCH_VALUE_BYTES {
                return Err(at(
                    "operations",
                    index,
                    unprocessable(format!(
                        "the values of a batch total at most {MAX_KV_BATCH_VALUE_BYTES} decoded bytes"
                    )),
                ));
            }
            if let Some(first) = named.insert((namespace.as_str(), key.as_str()), index) {
                return Err(at(
                    "operations",
                    index,
                    unprocessable(format!(
                        "names the same namespace and key as operations[{first}]; a batch names each key once"
                    )),
                ));
            }
            conditions.push(
                condition
                    .parse()
                    .map_err(|error| at("operations", index, error))?,
            );
            values.push(bytes);
        }
    }
    // The base64 text and the conditions are done with: keep only the names.
    let names = operations
        .into_iter()
        .map(|operation| match operation {
            BatchOperation::Put { namespace, key, .. }
            | BatchOperation::Delete { namespace, key, .. } => (namespace, key),
        })
        .collect();
    Ok(Prepared {
        names,
        values,
        conditions,
    })
}

fn outcome_json(outcome: &KvOutcome) -> Value {
    match outcome {
        KvOutcome::Put { version } => json!({"version":version.to_string()}),
        KvOutcome::Deleted(deleted) => json!({"deleted":deleted}),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn batch(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    Extension(request_id): Extension<RequestId>,
    principal: Option<Extension<users::Principal>>,
    target: Result<Path<String>, PathRejection>,
    RawQuery(raw_query): RawQuery,
    no_query: Result<Query<NoQuery>, QueryRejection>,
    input: Result<Json<BatchInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    strict_query_encoding(raw_query.as_deref())?;
    let started = Instant::now();
    let target = collection_path(target)?;
    query(no_query)?;
    let prepared = prepare(body(input)?)?;
    let puts = prepared.puts();
    let counts = (puts, prepared.names.len() - puts);
    let retained = prepared
        .retained_bytes()
        .saturating_add(target.retained_bytes())
        .saturating_add(4096);
    let quota_service = service.clone();
    let audit_role = role.clone();
    let audit_target = target.clone();
    let scope = service.scope(role);
    let result = service
        .run_mutation(retained, move |db| {
            let collection = scope.collection(db, &target)?;
            writable(db)?;
            let now = crate::current_time_ms();
            let operations = prepared.operations();
            let outcomes = collection
                .kv_store(db)?
                .write_batch_gated_at(&operations, now, |database, changes| {
                    // A batch that writes no record takes no slot: as for a
                    // single delete, no quota applies, whatever the scope
                    // already holds.
                    if changes.iter().any(|&(_, present)| present) {
                        enforce_quota(
                            &quota_service,
                            database,
                            scope.role(),
                            collection.id(),
                            changes,
                        )?;
                    }
                    Ok(())
                })
                .map_err(|error| match error {
                    Gated::Batch(error) => kv_failure(error),
                    Gated::Refused(error) => error,
                })?;
            Ok(Json(
                json!({"results":outcomes.iter().map(outcome_json).collect::<Vec<_>>()}),
            ))
        })
        .await;
    audit(
        &service,
        &audit_role,
        &audit_target,
        &request_id,
        AuditDetails {
            operation: "kv_batch",
            expires_at_ms: None,
            counts: Some(counts),
            principal: principal.as_ref().map(|Extension(principal)| principal),
        },
        &result,
        started,
    );
    result
}

pub(super) async fn fetch_many(
    State(service): State<Service>,
    Extension(role): Extension<admin::Role>,
    target: Result<Path<String>, PathRejection>,
    RawQuery(raw_query): RawQuery,
    no_query: Result<Query<NoQuery>, QueryRejection>,
    input: Result<Json<GetManyInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    strict_query_encoding(raw_query.as_deref())?;
    let target = collection_path(target)?;
    query(no_query)?;
    let input = body(input)?;
    check_key_count(input.keys.len())?;
    for (index, key) in input.keys.iter().enumerate() {
        address(&key.namespace, &key.key).map_err(|error| at("keys", index, error))?;
    }
    let scope = service.scope(role);
    service
        .run_read(move |db| {
            let view = scope.collection(db, &target)?.kv_view(db)?;
            let keys: Vec<(&str, &str)> = input
                .keys
                .iter()
                .map(|key| (key.namespace.as_str(), key.key.as_str()))
                .collect();
            let total = keys.len();
            let mut page = ManyPage::default();
            let mut failure = None;
            view.visit_many_at(&keys, crate::current_time_ms(), |index, entry| {
                match page.push(index, total, entry.as_ref()) {
                    Ok(true) => ControlFlow::Continue(()),
                    Ok(false) => {
                        page.full = true;
                        ControlFlow::Break(())
                    }
                    Err(error) => {
                        failure = Some(error);
                        ControlFlow::Break(())
                    }
                }
            })?;
            if let Some(error) = failure {
                return Err(error);
            }
            if page.full && page.items.is_empty() {
                return Err(internal("one KV entry exceeds the page boundary"));
            }
            // The first key not read: this response ends where the next must
            // begin.
            let next_index = page.full.then_some(page.items.len());
            let mut body = serde_json::Map::with_capacity(2);
            body.insert("items".to_owned(), Value::Array(page.items));
            body.insert(
                "next_index".to_owned(),
                next_index.map_or(Value::Null, |index| json!(index)),
            );
            Ok(Json(Value::Object(body)))
        })
        .await
}

/// One get-many response: the items of the longest prefix of the keys whose
/// response, closed after it, fits `PAGE_BYTES`.
#[derive(Default)]
struct ManyPage {
    items: Vec<Value>,
    item_bytes: usize,
    /// A key was refused because the response would pass `PAGE_BYTES`.
    full: bool,
}

impl ManyPage {
    /// Adds the item for the key at `index` of `total` unless the response,
    /// closed after it, would pass `PAGE_BYTES`. A value is measured from the
    /// borrow and only encoded when its item is added.
    fn push(
        &mut self,
        index: usize,
        total: usize,
        entry: Option<&KvRef<'_>>,
    ) -> Result<bool, ApiError> {
        let size = match entry {
            Some(entry) => item_size(
                entry.key,
                entry.value.len(),
                entry.version,
                entry.expires_at_ms,
            )?,
            None => "null".len(),
        };
        // As the last item of the response it carries the index of the key
        // that follows, when one does.
        let next = (index + 1 < total).then_some(index + 1);
        let envelope = serde_json::to_vec(&json!({"items":[],"next_index":next}))
            .map_err(internal)?
            .len();
        if envelope + self.item_bytes + size + self.items.len() > PAGE_BYTES {
            return Ok(false);
        }
        self.item_bytes += size;
        self.items.push(match entry {
            Some(entry) => item_json(entry.key, entry.value, entry.version, entry.expires_at_ms),
            None => Value::Null,
        });
        Ok(true)
    }
}

/// One list page: the entries of a single selection scan, in key order, that
/// fit `limit` and `PAGE_BYTES` together with the response envelope.
#[derive(Default)]
struct PageBuilder {
    items: Vec<Value>,
    item_bytes: usize,
    /// The last emitted key, and whether another live entry follows it.
    last: Option<(String, bool)>,
    /// An entry was refused because the page would pass `PAGE_BYTES`.
    full: bool,
}

impl PageBuilder {
    /// Adds `entry` unless the page, closed after it, would pass `PAGE_BYTES`.
    /// The entry's value is measured from the borrow and only encoded when it
    /// is added.
    fn push(&mut self, entry: &KvRef<'_>) -> Result<bool, ApiError> {
        let size = item_size(
            entry.key,
            entry.value.len(),
            entry.version,
            entry.expires_at_ms,
        )?;
        // As the last entry of the page it carries the cursor, when more follow.
        let next = entry.has_next.then_some(entry.key);
        let envelope = serde_json::to_vec(&json!({"items":[],"next_after":next}))
            .map_err(internal)?
            .len();
        if envelope + self.item_bytes + size + self.items.len() > PAGE_BYTES {
            return Ok(false);
        }
        self.item_bytes += size;
        self.items.push(item_json(
            entry.key,
            entry.value,
            entry.version,
            entry.expires_at_ms,
        ));
        self.last = Some((entry.key.to_owned(), entry.has_next));
        Ok(true)
    }
}

/// Builds one page from a single selection scan. `scan` calls its visitor, in
/// key order, for at most `limit` entries of the query; the visitor stops it
/// at the first entry that does not fit. What is read is therefore the emitted
/// entries and, at most, the one that did not fit, which is only measured.
fn build_page(
    input: ListQuery,
    limit: u64,
    now: u64,
    scan: impl FnOnce(
        &str,
        &str,
        Option<&str>,
        usize,
        u64,
        &mut dyn FnMut(KvRef<'_>) -> ControlFlow<()>,
    ) -> Result<(), ApiError>,
) -> Result<Value, ApiError> {
    let mut page = PageBuilder::default();
    let mut failure = None;
    scan(
        &input.namespace,
        &input.prefix,
        input.after.as_deref(),
        limit as usize,
        now,
        &mut |entry| match page.push(&entry) {
            Ok(true) => ControlFlow::Continue(()),
            Ok(false) => {
                page.full = true;
                ControlFlow::Break(())
            }
            Err(error) => {
                failure = Some(error);
                ControlFlow::Break(())
            }
        },
    )?;
    if let Some(error) = failure {
        return Err(error);
    }
    if page.full && page.items.is_empty() {
        return Err(internal("one KV entry exceeds the page boundary"));
    }
    // The last emitted entry knows whether another live entry follows it: the
    // one that did not fit, or one beyond `limit`.
    let next_after = page
        .last
        .as_ref()
        .filter(|(_, more)| *more)
        .map(|(key, _)| key.as_str());
    // The items move into the response instead of being copied by `json!`.
    let mut body = serde_json::Map::with_capacity(2);
    body.insert("items".to_owned(), Value::Array(page.items));
    body.insert(
        "next_after".to_owned(),
        next_after.map_or(Value::Null, |key| Value::String(key.to_owned())),
    );
    Ok(Value::Object(body))
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
    let actor = audit::actor(role, details.principal);
    let outcome = result.as_ref().map_or_else(|error| error.code, |_| "ok");
    // Success proves `open` accepted the collection and its binding.
    let collection = if result.is_ok() && crate::format::valid_collection_name(target.name()) {
        tenants::qualify(role, target.name())
    } else {
        "(invalid)".to_owned()
    };
    let mut event = json!({
        "event":details.operation,"request_id":request_id.0,"principal":principal,
        "actor":actor,
        "collection":collection,"outcome":outcome,
        "expires_at_ms":details.expires_at_ms.map(|value|value.to_string()),
        "duration_ns":started.elapsed().as_nanos().min(u64::MAX as u128) as u64
    });
    if let (Some((puts, deletes)), Some(fields)) = (details.counts, event.as_object_mut()) {
        fields.remove("expires_at_ms");
        fields.insert("puts".to_owned(), json!(puts));
        fields.insert("deletes".to_owned(), json!(deletes));
    }
    if let Some(log) = &service.admin.audit_log
        && log.append(event, false).is_err()
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
    fn kv_write_failures_hide_the_revision_and_other_routes_keep_it() {
        for current in [None, Some(7)] {
            let mapped = kv_failure(Error::RevisionMismatch { current });
            assert_eq!(mapped.status, StatusCode::PRECONDITION_FAILED);
            assert_eq!(mapped.code, "precondition_failed");
            assert_eq!(mapped.message, "revision precondition failed");
        }
        let shared = ApiError::from(Error::KvSlotsExhausted);
        let mapped = kv_failure(Error::KvSlotsExhausted);
        assert_eq!(
            (mapped.status, mapped.code, mapped.message),
            (shared.status, shared.code, shared.message)
        );
        // The shared mapping, used by every other route, is unchanged since 0.6.1.
        assert_eq!(
            ApiError::from(Error::RevisionMismatch { current: Some(7) }).message,
            "revision precondition failed; current revision is 7"
        );
        assert_eq!(
            ApiError::from(Error::RevisionMismatch { current: None }).message,
            "revision precondition failed; record does not exist"
        );
    }

    /// A scan shaped like the `scan` argument of `build_page`.
    type Scan<'a> = Box<
        dyn FnOnce(
                &str,
                &str,
                Option<&str>,
                usize,
                u64,
                &mut dyn FnMut(KvRef<'_>) -> ControlFlow<()>,
            ) -> Result<(), ApiError>
            + 'a,
    >;

    /// `KvView::visit_at` as documented, over `entries` sorted by key: at most
    /// `count` entries after `after`, each with `has_next` set when another
    /// entry follows within `count + 1`. `scans` counts scans and `lent` the
    /// entries handed to the visitor.
    fn scan_of<'a>(
        entries: &'a [KvEntry],
        scans: &'a Cell<usize>,
        lent: &'a Cell<usize>,
    ) -> Scan<'a> {
        Box::new(move |_, _, after, count, _, visit| {
            scans.set(scans.get() + 1);
            let matching: Vec<&KvEntry> = entries
                .iter()
                .filter(|entry| after.is_none_or(|after| entry.key.as_str() > after))
                .collect();
            let total = matching.len().min(count + 1);
            for (index, entry) in matching.iter().take(count).enumerate() {
                lent.set(lent.get() + 1);
                let borrowed = KvRef {
                    key: &entry.key,
                    value: &entry.value,
                    version: entry.version,
                    expires_at_ms: entry.expires_at_ms,
                    has_next: index + 1 < total,
                };
                if visit(borrowed).is_break() {
                    break;
                }
            }
            Ok(())
        })
    }

    fn query(after: Option<&str>) -> ListQuery {
        ListQuery {
            namespace: "bulk".into(),
            prefix: String::new(),
            after: after.map(str::to_owned),
            limit: None,
        }
    }

    fn numbered(count: usize, value: usize) -> Vec<KvEntry> {
        (0..count)
            .map(|index| KvEntry {
                key: format!("{index:05}"),
                value: vec![0; value],
                version: 1,
                expires_at_ms: None,
            })
            .collect()
    }

    #[test]
    fn large_page_uses_bounded_number_of_list_scans() {
        let entries = numbered(2_000, 1);
        let (scans, lent) = (Cell::new(0), Cell::new(0));
        let page = build_page(query(None), 10_000, 0, scan_of(&entries, &scans, &lent)).unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 2_000);
        // One selection scan builds the page: no chunks, no lookahead scan.
        assert_eq!(scans.get(), 1, "scans of the collection");
        assert_eq!(lent.get(), 2_000);
        assert_eq!(page["next_after"], Value::Null);
    }

    #[test]
    fn a_page_asks_the_scan_for_exactly_the_query_and_limit() {
        let asked = Cell::new(false);
        let page = build_page(
            ListQuery {
                namespace: "ns".into(),
                prefix: "pre".into(),
                after: Some("a".into()),
                limit: Some("7".into()),
            },
            7,
            42,
            |namespace, prefix, after, count, now, _| {
                assert_eq!(
                    (namespace, prefix, after, count, now),
                    ("ns", "pre", Some("a"), 7, 42)
                );
                asked.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(asked.get());
        assert_eq!(page, json!({"items":[],"next_after":null}));
    }

    // The cursor is the last emitted key, and only when another live entry follows it.
    #[test]
    fn the_cursor_is_the_last_emitted_key_only_when_another_entry_follows() {
        let entries = numbered(5, 1);
        for (limit, emitted, cursor) in [
            (1, 1, Some("00000")),
            (2, 2, Some("00001")),
            (4, 4, Some("00003")),
            (5, 5, None),
            (6, 5, None),
            (10_000, 5, None),
        ] {
            let (scans, lent) = (Cell::new(0), Cell::new(0));
            let page = build_page(query(None), limit, 0, scan_of(&entries, &scans, &lent)).unwrap();
            let keys: Vec<&str> = page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["key"].as_str().unwrap())
                .collect();
            let expected: Vec<String> = (0..emitted).map(|index| format!("{index:05}")).collect();
            assert_eq!(keys, expected, "limit {limit}");
            assert_eq!(page["next_after"], json!(cursor), "limit {limit}");
        }
        let (scans, lent) = (Cell::new(0), Cell::new(0));
        let page = build_page(
            query(Some("00002")),
            10,
            0,
            scan_of(&entries, &scans, &lent),
        )
        .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
        assert_eq!(page["next_after"], Value::Null);
        let page = build_page(
            query(Some("00004")),
            10,
            0,
            scan_of(&entries, &scans, &lent),
        )
        .unwrap();
        assert_eq!(page, json!({"items":[],"next_after":null}));
    }

    // 16 MiB of response holds 12 values of the largest size. Reading is
    // bounded by the page: the entry that does not fit is measured, never
    // encoded, and nothing beyond it is touched.
    #[test]
    fn a_page_reads_only_what_it_emits() {
        let entries = numbered(20, 1_047_290);
        let (scans, lent) = (Cell::new(0), Cell::new(0));
        ENCODED_VALUE_BYTES.with(|bytes| bytes.set(0));
        let page = build_page(query(None), 10_000, 0, scan_of(&entries, &scans, &lent)).unwrap();
        let emitted = page["items"].as_array().unwrap().len();
        assert_eq!(emitted, 12);
        assert_eq!(page["next_after"], "00011");
        assert_eq!(
            ENCODED_VALUE_BYTES.with(Cell::get),
            emitted * 1_047_290,
            "only emitted values are encoded"
        );
        assert_eq!(
            lent.get(),
            emitted + 1,
            "emitted entries plus the one refused"
        );
        assert_eq!(scans.get(), 1);
        assert!(serde_json::to_vec(&page).unwrap().len() <= PAGE_BYTES);
    }

    #[test]
    fn item_size_is_the_serialized_size_of_the_item() {
        let keys = ["k", "a/b", "\"quoted\\\u{1}\n", "\u{e9}\u{4e2d}\u{1f600}"];
        let versions = [0, 9, 10, u64::MAX];
        let expiries = [None, Some(0), Some(u64::MAX)];
        for (round, key) in keys.iter().enumerate() {
            for length in (0..=40usize).chain([1_000, 1_047_289, 1_047_290]) {
                let value: Vec<u8> = (0..length).map(|index| (index * 7 + round) as u8).collect();
                for (&version, &expires_at_ms) in versions.iter().zip(expiries.iter().cycle()) {
                    let item = item_json(key, &value, version, expires_at_ms);
                    assert_eq!(
                        item_size(key, length, version, expires_at_ms).unwrap(),
                        serde_json::to_vec(&item).unwrap().len(),
                        "key {key:?}, {length} value bytes, version {version}, expiry {expires_at_ms:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_entry_larger_than_a_page_is_an_internal_error() {
        let entries = numbered(1, 13_000_000);
        let (scans, lent) = (Cell::new(0), Cell::new(0));
        let error = build_page(query(None), 10, 0, scan_of(&entries, &scans, &lent)).unwrap_err();
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.code, "internal_error");
    }

    #[test]
    fn a_failed_scan_returns_its_error_and_no_page() {
        let error = build_page(query(None), 10, 0, |_, _, _, _, _, _| {
            Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "collection_not_found",
                "gone",
            ))
        })
        .unwrap_err();
        assert_eq!(
            (error.status, error.code),
            (StatusCode::NOT_FOUND, "collection_not_found")
        );
    }

    fn boundary_fixture(
        count: usize,
        last_key: &str,
        bytes: usize,
        more: bool,
    ) -> (Vec<KvEntry>, Value) {
        let mut entries = (0..count)
            .map(|index| KvEntry {
                key: if index + 1 == count {
                    last_key.to_owned()
                } else {
                    format!("{index:03}")
                },
                value: Vec::new(),
                version: 1,
                expires_at_ms: None,
            })
            .collect::<Vec<_>>();
        let oracle = |entries: &[KvEntry]| {
            let items = entries
                .iter()
                .map(|entry| {
                    json!({"key":entry.key,"value_base64":BASE64_STANDARD.encode(&entry.value),
                        "version":"1","expires_at_ms":null})
                })
                .collect::<Vec<_>>();
            json!({"items":items,"next_after":if more { Some(last_key) } else { None }})
        };
        let overhead = serde_json::to_vec(&oracle(&entries)).unwrap().len();
        let padding = (bytes - overhead) % 4;
        entries[0].key.push_str(&"x".repeat(padding));
        let mut remaining = bytes - overhead - padding;
        for entry in &mut entries {
            let encoded = remaining.min(1_396_384);
            entry.value = vec![1; encoded / 4 * 3];
            remaining -= encoded;
        }
        assert_eq!(remaining, 0);
        let expected = oracle(&entries);
        assert_eq!(serde_json::to_vec(&expected).unwrap().len(), bytes);
        (entries, expected)
    }

    fn synthetic_page(entries: &[KvEntry]) -> Value {
        let (scans, lent) = (Cell::new(0), Cell::new(0));
        build_page(
            ListQuery {
                namespace: "boundary".into(),
                prefix: String::new(),
                after: None,
                limit: Some("10000".into()),
            },
            10_000,
            0,
            scan_of(entries, &scans, &lent),
        )
        .unwrap()
    }

    #[test]
    fn exact_byte_boundary_page_decides_the_cursor_from_has_next() {
        for more in [false, true] {
            for bytes in [16_777_216, 16_777_217] {
                let (mut entries, expected) = boundary_fixture(16, "zzz", bytes, more);
                if more {
                    entries.push(KvEntry {
                        key: "zzzz".into(),
                        value: Vec::new(),
                        version: 1,
                        expires_at_ms: None,
                    });
                }
                let page = synthetic_page(&entries);
                if bytes == 16_777_216 {
                    assert_eq!(page["items"].as_array().unwrap().len(), 16, "more={more}");
                    assert_eq!(page["next_after"], expected["next_after"]);
                    assert_eq!(page, expected);
                } else {
                    assert_eq!(page["items"].as_array().unwrap().len(), 15, "more={more}");
                    assert_eq!(page["next_after"], "014");
                }
            }
        }
    }

    #[test]
    fn final_page_budgets_null_not_a_cursor_for_long_and_short_keys() {
        for key in ["zzz", "z"] {
            let (entries, expected) = boundary_fixture(16, key, 16_777_216, false);
            let page = synthetic_page(&entries);
            assert_eq!(page["items"].as_array().unwrap().len(), 16, "key={key}");
            assert_eq!(page["next_after"], Value::Null, "key={key}");
            assert_eq!(page, expected);
        }
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
                counts: None,
                principal: None,
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

    #[test]
    fn a_batch_at_every_limit_is_charged_less_than_eight_mebibytes() {
        // docs/KEY-VALUE.md tells operators that a group-commit budget of 8 MiB
        // always admits a batch: 1,000 puts with the longest namespaces and
        // keys and 4,194,304 value bytes in all.
        let names: Vec<(String, String)> = (0..MAX_KV_BATCH_OPERATIONS)
            .map(|index| ("n".repeat(255), format!("{index:04}{}", "k".repeat(1_020))))
            .collect();
        let mut values: Vec<Option<Vec<u8>>> = vec![Some(vec![0; 4_194]); MAX_KV_BATCH_OPERATIONS];
        values[0] = Some(vec![0; 4_194 + 304]);
        let value_bytes: usize = values.iter().flatten().map(Vec::len).sum();
        assert_eq!(value_bytes, MAX_KV_BATCH_VALUE_BYTES);
        let prepared = Prepared {
            names,
            values,
            conditions: vec![Expected::Any; MAX_KV_BATCH_OPERATIONS],
        };
        let charged = prepared.retained_bytes();
        assert!(charged > value_bytes, "{charged}");
        assert!(charged < 8 * 1024 * 1024, "{charged}");
    }
}
