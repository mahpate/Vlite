//! WebAssembly bindings: an in-memory VLite database persisted by the host as
//! a byte image (`toBytes` / `fromBytes`). No files, locks or threads.

use vlite::{CreateOptions, Database, EncryptionKey};
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
