//! WebAssembly bindings: an in-memory VLite database persisted by the host as
//! a byte image (`toBytes` / `fromBytes`). No files, locks or threads.

use js_sys::{Function, JsString, Reflect, Symbol, Uint8Array};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};
use vlite::changes::{Change, ChangeKind, FeedCursor};
use vlite::kv::{KvPut, KvView, MAX_KV_VALUE_BYTES};
use vlite::{CreateOptions, Database, EncryptionKey, Expected, Metadata, MetadataValue};
use wasm_bindgen::prelude::*;

fn js_error(error: impl std::fmt::Display) -> JsError {
    JsError::new(&error.to_string())
}

/// The clock key-value expiry reads: the host's last `setClock` value.
static CLOCK_MS: AtomicU64 = AtomicU64::new(0);

/// Sets the Unix-millisecond clock (pass `Date.now()`); needed for TTLs and
/// commit timestamps because WebAssembly has no system clock.
#[wasm_bindgen(js_name = setClock)]
pub fn set_clock(now_ms: f64) {
    CLOCK_MS.store(now_ms as u64, Ordering::Relaxed);
    #[cfg(target_arch = "wasm32")]
    vlite::set_wasm_clock_ms(now_ms as u64);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = now_ms;
}

// JS built-ins that build key-value results and errors. They are not exported.
// The ones that read the caller's `expected` object can run the caller's code (a
// getter, a proxy), so they are `catch`: a JavaScript exception that unwound
// through this frame would leave the database borrowed for good. A value is
// read only through the intrinsics of `value_of`, which run none of it.
#[wasm_bindgen]
extern "C" {
    type Object;
    #[wasm_bindgen(constructor)]
    fn new() -> Object;
    type Array;
    #[wasm_bindgen(constructor)]
    fn new() -> Array;
    #[wasm_bindgen(method)]
    fn push(this: &Array, value: &JsValue) -> u32;
    #[wasm_bindgen(method, getter)]
    fn length(this: &Array) -> u32;
    #[wasm_bindgen(js_namespace = Reflect, js_name = set)]
    fn reflect_set(target: &JsValue, key: &str, value: &JsValue) -> bool;
    #[wasm_bindgen(catch, js_namespace = Reflect, js_name = get)]
    fn reflect_get(target: &JsValue, key: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch, static_method_of = Object)]
    fn keys(target: &JsValue) -> Result<Array, JsValue>;
}

#[wasm_bindgen(typescript_custom_section)]
const KV_TYPES: &str = r#"
/** Compare-and-set condition: "any" (the default), "absent" (create only) or the exact version you read, as a bigint. */
export type KvExpected = "any" | "absent" | { revision: bigint };
export interface KvPutResult { version: bigint; expires_at_ms: bigint | null }
export interface KvEntry extends KvPutResult { key: string; value: Uint8Array }
export interface KvPage { items: KvEntry[]; next_after: string | null }
"#;

#[wasm_bindgen]
pub struct VLiteDatabase {
    inner: Database,
}

#[wasm_bindgen]
impl VLiteDatabase {
    /// New empty graph-enabled V5 database. A 32-byte `key` enables
    /// authenticated encryption.
    #[wasm_bindgen(js_name = create)]
    pub fn create(dimensions: usize, key: Option<Vec<u8>>) -> Result<VLiteDatabase, JsError> {
        let encryption = match key {
            Some(bytes) => Some(EncryptionKey::from_bytes(key_bytes(&bytes)?)),
            None => None,
        };
        let inner = Database::create_in_memory(
            dimensions,
            CreateOptions {
                encryption,
                ..CreateOptions::default()
            },
        )
        .map_err(js_error)?;
        Ok(Self { inner })
    }

    /// Opens a byte image produced by `toBytes`.
    #[wasm_bindgen(js_name = fromBytes)]
    pub fn from_bytes(bytes: Vec<u8>, key: Option<Vec<u8>>) -> Result<VLiteDatabase, JsError> {
        let key = match key {
            Some(bytes) => Some(EncryptionKey::from_bytes(key_bytes(&bytes)?)),
            None => None,
        };
        let inner = Database::open_from_bytes(bytes, key.as_ref()).map_err(js_error)?;
        Ok(Self { inner })
    }

    /// The durable image; store it to persist the database.
    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.inner.to_bytes().unwrap_or_default()
    }

    pub fn put(&mut self, id: f64, vector: Vec<f32>, payload: Vec<u8>) -> Result<(), JsError> {
        self.inner
            .put(id_of(id)?, &vector, &payload)
            .map_err(js_error)
    }

    /// Puts a record with typed metadata and an optional absolute Unix-ms expiry. Requires V5.
    #[wasm_bindgen(js_name = putWithExpiry)]
    pub fn put_with_expiry(
        &mut self,
        id: f64,
        vector: Vec<f32>,
        payload: Vec<u8>,
        expires_at_ms: Option<f64>,
        metadata_json: Option<String>,
    ) -> Result<(), JsError> {
        let metadata = parse_metadata(metadata_json.as_deref().unwrap_or("{}"))?;
        let expiry = expires_at_ms
            .map(|value| {
                if value.is_finite()
                    && value >= 0.0
                    && value.fract() == 0.0
                    && value <= 9_007_199_254_740_991.0
                {
                    Ok(value as u64)
                } else {
                    Err(JsError::new("expiry must be a non-negative safe integer"))
                }
            })
            .transpose()?;
        self.inner
            .put_with_expiry(id_of(id)?, &vector, &payload, &metadata, expiry)
            .map_err(js_error)?;
        Ok(())
    }

    /// Replaces only metadata; requires a V5 database.
    #[wasm_bindgen(js_name = updateMetadata)]
    pub fn update_metadata(&mut self, id: f64, metadata_json: String) -> Result<(), JsError> {
        let metadata = parse_metadata(&metadata_json)?;
        self.inner
            .update_metadata(id_of(id)?, &metadata)
            .map_err(js_error)?;
        Ok(())
    }

    /// Returns exact results grouped by typed metadata values, encoded as JSON.
    #[wasm_bindgen(js_name = groupedSearch)]
    pub fn grouped_search(
        &self,
        query: Vec<f32>,
        groups: usize,
        hits_per_group: usize,
        field: String,
    ) -> Result<String, JsError> {
        let result = self
            .inner
            .collection("default")
            .map_err(js_error)?
            .search_grouped(
                &query,
                groups,
                hits_per_group,
                &field,
                &[],
                vlite::DistanceMetric::SquaredL2,
            )
            .map_err(js_error)?;
        let groups: Vec<_> = result.into_iter().map(|group| serde_json::json!({
            "value": metadata_value_json(&group.value),
            "hits": group.hits.into_iter().map(|hit| serde_json::json!({"id": hit.id.to_string(), "distance": hit.distance})).collect::<Vec<_>>(),
        })).collect();
        serde_json::to_string(&groups).map_err(js_error)
    }

    /// Cursor positioned at the current database head.
    #[wasm_bindgen(js_name = feedCursor)]
    pub fn feed_cursor(&self) -> Result<String, JsError> {
        self.inner
            .feed_cursor()
            .map(|cursor| cursor.to_token())
            .map_err(js_error)
    }

    /// Reads committed changes after an opaque feed cursor; result is JSON text.
    #[wasm_bindgen(js_name = changesAfter)]
    pub fn changes_after(
        &self,
        cursor_token: String,
        max_events: usize,
    ) -> Result<String, JsError> {
        let cursor = FeedCursor::from_token(&cursor_token).map_err(js_error)?;
        let batch = self
            .inner
            .changes_after(&cursor, max_events)
            .map_err(js_error)?;
        let changes: Vec<_> = batch.changes.iter().map(change_json).collect();
        serde_json::to_string(&serde_json::json!({
            "changes": changes, "next": batch.next.to_token(), "has_more": batch.has_more, "head": batch.head.to_string()
        })).map_err(js_error)
    }

    pub fn delete(&mut self, id: f64) -> Result<(), JsError> {
        self.inner.delete(id_of(id)?).map_err(js_error)?;
        Ok(())
    }

    /// Payload bytes, or `undefined` when the id does not exist.
    pub fn get(&self, id: f64) -> Result<Option<Vec<u8>>, JsError> {
        Ok(self
            .inner
            .get(id_of(id)?)
            .map_err(js_error)?
            .map(|record| record.payload.clone()))
    }

    /// Exact nearest neighbours as a flat `[id, distance, id, distance, ...]`.
    pub fn search(&self, query: Vec<f32>, k: usize) -> Result<Vec<f64>, JsError> {
        let hits = self.inner.search(&query, k).map_err(js_error)?;
        Ok(hits
            .into_iter()
            .flat_map(|hit| [hit.id as f64, hit.distance])
            .collect())
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    #[wasm_bindgen(js_name = isEmpty)]
    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    // Key-value: namespaced byte values in an existing one-dimension collection;
    // a missing collection is never created. Values are `Uint8Array`, versions and
    // deadlines `bigint`. Failures throw an `Error` whose `code` is the stable
    // code of the MCP/shell/C ABI key-value dialect (`kv_not_found`,
    // `collection_not_found`, `precondition_failed`, `invalid_input`, ...).
    // Expiry reads the clock set with `setClock`. Every argument is checked as
    // received: a string must be a well-formed string and a value a `Uint8Array`,
    // because a typed parameter would copy anything array-like or lossy.

    /// The live entry, or `undefined` when the key is absent or expired.
    #[wasm_bindgen(js_name = kvGet, unchecked_return_type = "KvEntry | undefined")]
    pub fn kv_get(
        &self,
        #[wasm_bindgen(unchecked_param_type = "string")] collection: JsValue,
        #[wasm_bindgen(unchecked_param_type = "string")] namespace: JsValue,
        #[wasm_bindgen(unchecked_param_type = "string")] key: JsValue,
    ) -> Result<JsValue, JsValue> {
        let (collection, namespace) = address(&collection, &namespace)?;
        let key = key_of(&key)?;
        let entry = self
            .inner
            .kv_view(&collection)
            .map_err(kv_error)?
            .get_at(&namespace, &key, now())
            .map_err(kv_error)?;
        Ok(entry.map_or(JsValue::UNDEFINED, |entry| {
            entry_object(&entry.key, &entry.value, entry.version, entry.expires_at_ms)
        }))
    }

    /// Writes `value`; `expected` is `"any"` (default), `"absent"` or
    /// `{ revision: bigint }`, `expires_at_ms` an absolute Unix-ms deadline (V5)
    /// as a `bigint`, or `null`/`undefined` for none.
    #[wasm_bindgen(js_name = kvPut, unchecked_return_type = "KvPutResult")]
    pub fn kv_put(
        &mut self,
        #[wasm_bindgen(unchecked_param_type = "string")] collection: JsValue,
        #[wasm_bindgen(unchecked_param_type = "string")] namespace: JsValue,
        #[wasm_bindgen(unchecked_param_type = "string")] key: JsValue,
        #[wasm_bindgen(unchecked_param_type = "Uint8Array")] value: JsValue,
        #[wasm_bindgen(unchecked_optional_param_type = "KvExpected")] expected: JsValue,
        #[wasm_bindgen(unchecked_optional_param_type = "bigint | null")] expires_at_ms: JsValue,
    ) -> Result<JsValue, JsValue> {
        let (collection, namespace) = address(&collection, &namespace)?;
        let key = key_of(&key)?;
        let expected = expected_of(&expected)?;
        let expires_at_ms = deadline_of(&expires_at_ms)?;
        let value = value_of(&value)?;
        // `kv_store` creates a missing collection; this surface must not.
        self.inner.kv_view(&collection).map_err(kv_error)?;
        let version = self
            .inner
            .kv_store(&collection)
            .map_err(kv_error)?
            .put_at(
                &namespace,
                &key,
                &value,
                KvPut {
                    expected,
                    expires_at_ms,
                },
                now(),
            )
            .map_err(kv_error)?;
        let result = Object::new().into();
        reflect_set(&result, "version", &JsValue::from(version));
        reflect_set(&result, "expires_at_ms", &optional_u64(expires_at_ms));
        Ok(result)
    }

    /// Whether an entry was deleted (`false` for an absent or expired key).
    #[wasm_bindgen(js_name = kvDelete)]
    pub fn kv_delete(
        &mut self,
        #[wasm_bindgen(unchecked_param_type = "string")] collection: JsValue,
        #[wasm_bindgen(unchecked_param_type = "string")] namespace: JsValue,
        #[wasm_bindgen(unchecked_param_type = "string")] key: JsValue,
        #[wasm_bindgen(unchecked_optional_param_type = "KvExpected")] expected: JsValue,
    ) -> Result<bool, JsValue> {
        let (collection, namespace) = address(&collection, &namespace)?;
        let key = key_of(&key)?;
        let expected = expected_of(&expected)?;
        self.inner.kv_view(&collection).map_err(kv_error)?;
        self.inner
            .kv_store(&collection)
            .map_err(kv_error)?
            .delete_at(&namespace, &key, expected, now())
            .map_err(kv_error)
    }

    /// One page in UTF-8 key order: at most `limit` (1..10000, default 100)
    /// entries and 16 MiB of JSON, so it can be shorter than `limit` while
    /// `next_after` is set. Pass `next_after` as `after` for the next page.
    #[wasm_bindgen(js_name = kvList, unchecked_return_type = "KvPage")]
    pub fn kv_list(
        &self,
        #[wasm_bindgen(unchecked_param_type = "string")] collection: JsValue,
        #[wasm_bindgen(unchecked_param_type = "string")] namespace: JsValue,
        #[wasm_bindgen(unchecked_optional_param_type = "string | null")] prefix: JsValue,
        #[wasm_bindgen(unchecked_optional_param_type = "string | null")] after: JsValue,
        #[wasm_bindgen(unchecked_optional_param_type = "number | null")] limit: JsValue,
    ) -> Result<JsValue, JsValue> {
        let (collection, namespace) = address(&collection, &namespace)?;
        let prefix = optional_text(&prefix, "prefix", 0, 1024)?.unwrap_or_default();
        let after = optional_text(&after, "after", 1, 1024)?;
        let limit = limit_of(&limit)?;
        if !(1..=10_000).contains(&limit) {
            return Err(invalid("limit must be 1..10000"));
        }
        let view = self.inner.kv_view(&collection).map_err(kv_error)?;
        let (entries, next_after) =
            page(view, &namespace, &prefix, after.as_deref(), limit as usize)?;
        let items = Array::new();
        for entry in &entries {
            items.push(&entry_object(
                &entry.key,
                &entry.value,
                entry.version,
                entry.expires_at_ms,
            ));
        }
        let result = Object::new().into();
        reflect_set(&result, "items", &items);
        reflect_set(
            &result,
            "next_after",
            &next_after.map_or(JsValue::NULL, |key| JsValue::from_str(&key)),
        );
        Ok(result)
    }
}

fn key_bytes(bytes: &[u8]) -> Result<[u8; 32], JsError> {
    bytes
        .try_into()
        .map_err(|_| JsError::new("encryption key must be exactly 32 bytes"))
}

/// JS numbers are exact only up to 2^53.
fn id_of(id: f64) -> Result<u64, JsError> {
    if id.is_finite() && id >= 0.0 && id.fract() == 0.0 && id <= 9_007_199_254_740_991.0 {
        Ok(id as u64)
    } else {
        Err(JsError::new("id must be a non-negative safe integer"))
    }
}

fn parse_metadata(text: &str) -> Result<Metadata, JsError> {
    serde_json::from_str(text).map_err(js_error)
}

const PAGE_BYTES: usize = 16 * 1024 * 1024;

fn now() -> u64 {
    CLOCK_MS.load(Ordering::Relaxed)
}

/// A thrown key-value failure: an `Error` whose `code` is the stable code of
/// the key-value dialect and whose message reads `code: message`.
fn reject(code: &str, message: impl std::fmt::Display) -> JsValue {
    let error = JsValue::from(JsError::new(&format!("{code}: {message}")));
    reflect_set(&error, "code", &JsValue::from_str(code));
    error
}

fn invalid(message: impl std::fmt::Display) -> JsValue {
    reject("invalid_input", message)
}

fn kv_error(error: vlite::Error) -> JsValue {
    use vlite::Error as E;
    let code = match &error {
        E::CollectionNotFound => "collection_not_found",
        E::NotKvCollection => "not_kv_collection",
        E::KvSlotsExhausted => "kv_slots_exhausted",
        E::InvalidKvKey(_) | E::PayloadTooLarge(_) | E::InvalidCollectionName => "invalid_input",
        E::ExpiryRequiresUpgrade | E::BatchRequiresUpgrade | E::MetadataRequiresUpgrade => {
            "upgrade_required"
        }
        E::RevisionMismatch { .. } => "precondition_failed",
        E::ReadOnly => "read_only",
        E::Io(_) => "io_error",
        E::Locked => "locked",
        E::CommitIndeterminate(_) => "commit_indeterminate",
        E::Poisoned => "poisoned",
        E::Corrupt { .. } => "corrupt",
        E::IncompleteTail { .. } => "incomplete_tail",
        E::GraphAttachedRecordExpiry => "graph_attached_record_expiry",
        E::GraphAttachedRecord => "graph_attached_record",
        _ => "database_error",
    };
    reject(code, error)
}

fn bounded(text: &str, name: &str, min: usize, max: usize) -> Result<(), JsValue> {
    if !(min..=max).contains(&text.len()) {
        return Err(invalid(format!("{name} must be {min}..{max} UTF-8 bytes")));
    }
    Ok(())
}

/// A string argument of `min..=max` UTF-8 bytes. A `&str` parameter would be
/// copied through `TextEncoder`, which turns an unpaired surrogate into U+FFFD,
/// so two different keys could name one entry; the other bindings refuse such a
/// string, and so does this one, as it does anything that is not a string.
fn text(value: &JsValue, name: &str, min: usize, max: usize) -> Result<String, JsValue> {
    let string = value
        .dyn_ref::<JsString>()
        .ok_or_else(|| reject("invalid_request", format!("{name} must be a string")))?;
    // Every UTF-16 unit takes at least one UTF-8 byte.
    if string.length() as usize > max {
        return Err(invalid(format!("{name} must be {min}..{max} UTF-8 bytes")));
    }
    let mut units = (0..string.length())
        .map(|index| string.char_code_at(index) as u16)
        .peekable();
    while let Some(unit) = units.next() {
        match unit {
            0xD800..=0xDBFF if matches!(units.peek(), Some(0xDC00..=0xDFFF)) => {
                units.next();
            }
            0xD800..=0xDFFF => {
                return Err(invalid(format!("{name} must be well-formed Unicode")));
            }
            _ => {}
        }
    }
    let text = value.as_string().unwrap_or_default();
    bounded(&text, name, min, max)?;
    Ok(text)
}

/// `undefined` and `null` are absent.
fn optional_text(
    value: &JsValue,
    name: &str,
    min: usize,
    max: usize,
) -> Result<Option<String>, JsValue> {
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    text(value, name, min, max).map(Some)
}

/// The longest name a collection can have: `^[a-z][a-z0-9_-]{0,62}$`. A longer
/// string is refused before it is scanned.
const MAX_COLLECTION_BYTES: usize = 63;

/// The collection and the namespace of a call.
fn address(collection: &JsValue, namespace: &JsValue) -> Result<(String, String), JsValue> {
    let collection = text(collection, "collection", 0, MAX_COLLECTION_BYTES)?;
    if collection.is_empty() {
        return Err(invalid("collection must not be empty"));
    }
    Ok((collection, text(namespace, "namespace", 1, 255)?))
}

fn key_of(key: &JsValue) -> Result<String, JsValue> {
    text(key, "key", 1, 1024)
}

/// The intrinsics of `%TypedArray%.prototype` that read the internal slots of a
/// typed array and nothing else: the `@@toStringTag` and `length` getters and
/// `set`. They are taken once, the first time a value is read.
struct Intrinsics {
    name: Function,
    length: Function,
    set: Function,
}

fn capture_intrinsics() -> Result<Intrinsics, JsValue> {
    let sample = Uint8Array::new_with_length(0);
    let uint8_array = Reflect::get_prototype_of(&sample)?;
    let typed_array = Reflect::get_prototype_of(&uint8_array)?;
    let function = |value: JsValue| value.dyn_into::<Function>();
    let getter = |key: &JsValue| -> Result<Function, JsValue> {
        let descriptor = Reflect::get_own_property_descriptor(&typed_array, key)?;
        function(Reflect::get(&descriptor, &JsValue::from_str("get"))?)
    };
    Ok(Intrinsics {
        name: getter(&Symbol::to_string_tag().into())?,
        length: getter(&JsValue::from_str("length"))?,
        set: function(Reflect::get(&typed_array, &JsValue::from_str("set"))?)?,
    })
}

thread_local! {
    static INTRINSICS: Option<Intrinsics> = capture_intrinsics().ok();
}

/// The bytes of a `Uint8Array`, checked before they are copied. A `Vec<u8>`
/// parameter would copy any array-like: a string as zero bytes, an `ArrayBuffer`
/// as nothing, numbers modulo 256.
///
/// The check and the copy call the intrinsics of `%TypedArray%.prototype`
/// themselves. Nothing of the caller's value is looked up (`slice`, `length`,
/// `constructor`, `Symbol.species`), so no code of the value's own runs: a proxy
/// or an object that merely inherits from `Uint8Array.prototype` is refused, a
/// subclass that overrides those members is copied as the array it is, a
/// detached or out-of-bounds array is refused, and nothing larger than a value
/// may be is allocated. The intrinsics come from the page's realm: code that
/// replaces a built-in runs as the caller and is outside this guard, and the
/// allocation of the copy and `copy_to` are not `catch`, so such code can trap
/// while the database is borrowed.
fn value_of(value: &JsValue) -> Result<Vec<u8>, JsValue> {
    let malformed = || reject("invalid_request", "value must be a Uint8Array");
    INTRINSICS.with(|intrinsics| {
        let intrinsics = intrinsics.as_ref().ok_or_else(malformed)?;
        // `Float64Array`, `DataView`, a proxy and a plain object have another
        // tag or none.
        let name = intrinsics.name.call0(value).map_err(|_| malformed())?;
        if name.as_string().as_deref() != Some("Uint8Array") {
            return Err(malformed());
        }
        let length = intrinsics
            .length
            .call0(value)
            .ok()
            .and_then(|length| length.as_f64())
            .ok_or_else(malformed)?;
        if length > MAX_KV_VALUE_BYTES as f64 {
            return Err(invalid(format!("value exceeds {MAX_KV_VALUE_BYTES} bytes")));
        }
        let copy = Uint8Array::new_with_length(length as u32);
        intrinsics
            .set
            .call1(&copy, value)
            .map_err(|_| malformed())?;
        let mut bytes = vec![0; length as usize];
        copy.copy_to(&mut bytes);
        Ok(bytes)
    })
}

fn safe_integer(value: f64) -> bool {
    value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= 9_007_199_254_740_991.0
}

/// `undefined` and `null` are the default of 100; anything else must be a
/// number that is a safe integer, as in the Node binding, never a coerced value.
fn limit_of(value: &JsValue) -> Result<u64, JsValue> {
    if value.is_undefined() || value.is_null() {
        return Ok(100);
    }
    match value.as_f64() {
        Some(number) if safe_integer(number) => Ok(number as u64),
        _ => Err(reject(
            "invalid_request",
            "limit must be a non-negative safe integer",
        )),
    }
}

/// `undefined`/`"any"`, `"absent"` or `{ revision: bigint }`; nothing else.
fn expected_of(value: &JsValue) -> Result<Expected, JsValue> {
    let malformed = || {
        reject(
            "invalid_request",
            r#"expected must be "any", "absent" or { revision: bigint }"#,
        )
    };
    if value.is_undefined() {
        return Ok(Expected::Any);
    }
    if let Some(name) = value.as_string() {
        return match name.as_str() {
            "any" => Ok(Expected::Any),
            "absent" => Ok(Expected::Absent),
            _ => Err(malformed()),
        };
    }
    // A getter or a proxy of the caller's may throw: that is a malformed value.
    if value.is_object()
        && let Ok(names) = Object::keys(value)
        && names.length() == 1
        // `u64::try_from` accepts only a bigint in range, never a wrapped one.
        && let Ok(Ok(revision)) = reflect_get(value, "revision").map(u64::try_from)
    {
        return Ok(Expected::Revision(revision));
    }
    Err(malformed())
}

fn deadline_of(value: &JsValue) -> Result<Option<u64>, JsValue> {
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    u64::try_from(value.clone()).map(Some).map_err(|_| {
        reject(
            "invalid_request",
            "expires_at_ms must be a bigint in 0..18446744073709551615",
        )
    })
}

fn optional_u64(value: Option<u64>) -> JsValue {
    value.map_or(JsValue::NULL, JsValue::from)
}

fn entry_object(key: &str, value: &[u8], version: u64, expires_at_ms: Option<u64>) -> JsValue {
    let entry = Object::new().into();
    reflect_set(&entry, "key", &JsValue::from_str(key));
    reflect_set(&entry, "value", &JsValue::from(value.to_vec()));
    reflect_set(&entry, "version", &JsValue::from(version));
    reflect_set(&entry, "expires_at_ms", &optional_u64(expires_at_ms));
    entry
}

struct Emitted {
    key: String,
    value: Vec<u8>,
    version: u64,
    expires_at_ms: Option<u64>,
}

/// The compact JSON size of one list item, found from the length of the value
/// alone: the item with an empty value plus four base64 characters for every
/// started group of three value bytes.
fn item_size(key: &str, value_len: usize, version: u64, expires_at_ms: Option<u64>) -> usize {
    serde_json::json!({"key":key,"value_base64":"","version":version.to_string(),
        "expires_at_ms":expires_at_ms.map(|deadline|deadline.to_string())})
    .to_string()
    .len()
        + value_len.div_ceil(3) * 4
}

/// One page of at most `limit` entries and `PAGE_BYTES` of compact JSON, the
/// rule of the MCP/shell tools and the C ABI: the scan stops at the first entry
/// that does not fit, and the last emitted key is the cursor when another live
/// entry follows it.
fn page(
    view: KvView<'_>,
    namespace: &str,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<(Vec<Emitted>, Option<String>), JsValue> {
    let mut entries: Vec<Emitted> = Vec::new();
    let mut item_bytes = 0;
    let mut more = false;
    view.visit_at(namespace, prefix, after, limit, now(), |entry| {
        let bytes = item_size(
            entry.key,
            entry.value.len(),
            entry.version,
            entry.expires_at_ms,
        );
        // As the last entry of the page it carries the cursor, when more follow.
        let next = entry.has_next.then_some(entry.key);
        let envelope = serde_json::json!({"items":[],"next_after":next})
            .to_string()
            .len();
        if envelope + item_bytes + bytes + entries.len() > PAGE_BYTES {
            return ControlFlow::Break(());
        }
        item_bytes += bytes;
        entries.push(Emitted {
            key: entry.key.to_owned(),
            value: entry.value.to_vec(),
            version: entry.version,
            expires_at_ms: entry.expires_at_ms,
        });
        more = entry.has_next;
        ControlFlow::Continue(())
    })
    .map_err(kv_error)?;
    let next_after = if more {
        entries.last().map(|entry| entry.key.clone())
    } else {
        None
    };
    Ok((entries, next_after))
}

fn metadata_value_json(value: &MetadataValue) -> serde_json::Value {
    match value {
        MetadataValue::String(value) => serde_json::json!({"type":"string", "value":value}),
        MetadataValue::Integer(value) => {
            serde_json::json!({"type":"integer", "value":value.to_string()})
        }
        MetadataValue::Float(value) => serde_json::json!({"type":"float", "value":value}),
        MetadataValue::Boolean(value) => serde_json::json!({"type":"boolean", "value":value}),
        MetadataValue::GeoPoint {
            latitude,
            longitude,
        } => {
            serde_json::json!({"type":"geopoint", "value":{"latitude":latitude,"longitude":longitude}})
        }
    }
}

fn change_json(change: &Change) -> serde_json::Value {
    let (kind, id, record, expiry, metadata) = match &change.kind {
        ChangeKind::Put {
            id,
            record,
            expires_at_ms,
        } => ("put", Some(*id), Some(record), *expires_at_ms, None),
        ChangeKind::Delete { id } => ("delete", Some(*id), None, None, None),
        ChangeKind::MetadataUpdate { id, metadata } => {
            ("metadata_update", Some(*id), None, None, Some(metadata))
        }
        ChangeKind::CollectionCreated { .. } => ("collection_created", None, None, None, None),
        ChangeKind::CollectionDropped => ("collection_dropped", None, None, None, None),
        ChangeKind::SettingsUpdated { .. } => ("settings_updated", None, None, None, None),
        ChangeKind::VectorsUpdated => ("vectors_updated", None, None, None, None),
        graph @ (ChangeKind::GraphCreated { .. }
        | ChangeKind::GraphDropped { .. }
        | ChangeKind::GraphBatch { .. }) => {
            let mut output = serde_json::json!({"sequence":change.sequence.to_string(),"ordinal":change.ordinal,"collection":change.collection,"committed_at_ms":change.committed_at_ms,"id":null});
            let Some(serde_json::Value::Object(fields)) = graph.graph_json() else {
                unreachable!("graph feed event")
            };
            for (name, value) in fields {
                output[if name == "type" { "kind".to_owned() } else { name }] = value;
            }
            return output;
        }
    };
    let metadata_json = |values: &Metadata| {
        values
            .iter()
            .map(|(key, value)| (key.clone(), metadata_value_json(value)))
            .collect::<serde_json::Map<_, _>>()
    };
    let mut output = serde_json::json!({"sequence":change.sequence.to_string(),"ordinal":change.ordinal,"collection":change.collection,"committed_at_ms":change.committed_at_ms,"kind":kind,"id":id.map(|value| value.to_string())});
    if let Some(record) = record {
        output["vector"] = serde_json::json!(record.vector);
        output["payload"] = serde_json::json!(record.payload);
        output["metadata"] = serde_json::Value::Object(metadata_json(&record.metadata));
        output["expires_at_ms"] = serde_json::json!(expiry.map(|value| value.to_string()));
    }
    if let Some(metadata) = metadata {
        output["metadata"] = serde_json::Value::Object(metadata_json(metadata));
    }
    output
}
