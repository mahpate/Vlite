//! WebAssembly bindings: an in-memory VLite database persisted by the host as
//! a byte image (`toBytes` / `fromBytes`). No files, locks or threads.

use vlite::changes::{Change, ChangeKind, FeedCursor};
use vlite::{CreateOptions, Database, EncryptionKey, FormatVersion, Metadata, MetadataValue};
use wasm_bindgen::prelude::*;

fn js_error(error: impl std::fmt::Display) -> JsError {
    JsError::new(&error.to_string())
}

/// Sets the Unix-millisecond clock (pass `Date.now()`); needed for TTLs and
/// commit timestamps because WebAssembly has no system clock.
#[wasm_bindgen(js_name = setClock)]
pub fn set_clock(now_ms: f64) {
    #[cfg(target_arch = "wasm32")]
    vlite::set_wasm_clock_ms(now_ms as u64);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = now_ms;
}

#[wasm_bindgen]
pub struct VLiteDatabase {
    inner: Database,
}

#[wasm_bindgen]
impl VLiteDatabase {
    /// New empty database. A 32-byte `key` enables authenticated encryption.
    #[wasm_bindgen(js_name = create)]
    pub fn create(
        dimensions: usize,
        key: Option<Vec<u8>>,
        format_v5: Option<bool>,
    ) -> Result<VLiteDatabase, JsError> {
        let encryption = match key {
            Some(bytes) => Some(EncryptionKey::from_bytes(key_bytes(&bytes)?)),
            None => None,
        };
        let inner = Database::create_in_memory(
            dimensions,
            CreateOptions {
                encryption,
                format: format_v5.unwrap_or(false).then_some(FormatVersion::V5),
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
