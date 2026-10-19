//! C ABI: owned UTF-8 JSON responses, opaque numeric handles, no network runtime.

use base64::{Engine, prelude::BASE64_STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::{CString, c_char},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Mutex, OnceLock},
};
use vlite::{Database, HnswConfig, Metadata, Mutation};

#[derive(Default)]
struct State {
    next: u64,
    databases: BTreeMap<u64, Database>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Create {
        path: String,
        dimensions: usize,
    },
    Open {
        path: String,
    },
    Close {},
    Info {},
    Get {
        id: String,
    },
    GetMany {
        ids: Vec<String>,
    },
    Put {
        id: String,
        vector: Vec<f32>,
        #[serde(default)]
        payload_base64: String,
        #[serde(default)]
        metadata: Metadata,
    },
    Delete {
        id: String,
    },
    Batch {
        operations: Vec<Operation>,
    },
    Search {
        vector: Vec<f32>,
        k: usize,
        #[serde(default)]
        filter: Vec<vlite::Condition>,
        #[serde(default)]
        metric: vlite::DistanceMetric,
    },
    BuildIndex {},
    SearchApproximateReady {
        vector: Vec<f32>,
        k: usize,
        ef_search: usize,
    },
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Put {
        id: String,
        vector: Vec<f32>,
        #[serde(default)]
        payload_base64: String,
        #[serde(default)]
        metadata: Metadata,
    },
    Delete {
        id: String,
    },
}

type Failure = (&'static str, String);
type Result<T> = std::result::Result<T, Failure>;

fn failure(error: vlite::Error) -> Failure {
    let code = match &error {
        vlite::Error::CommitIndeterminate(_) => "commit_indeterminate",
        vlite::Error::Poisoned => "poisoned",
        vlite::Error::Locked => "locked",
        vlite::Error::Corrupt { .. } => "corrupt",
        vlite::Error::IncompleteTail { .. } => "incomplete_tail",
        vlite::Error::IndexNotReady => "index_not_ready",
        vlite::Error::Io(_) => "io_error",
        vlite::Error::UnsupportedVersion(_)
        | vlite::Error::MetadataRequiresUpgrade
        | vlite::Error::BatchRequiresUpgrade => "format_error",
        vlite::Error::SequenceExhausted => "sequence_exhausted",
        _ => "invalid_input",
    };
    (code, error.to_string())
}

fn invalid(message: &str) -> Failure {
    ("invalid_input", message.to_owned())
}

fn id(text: &str) -> Result<u64> {
    let value: u64 = text.parse().map_err(|_| invalid("invalid u64 ID"))?;
    if value.to_string() != text {
        return Err(invalid("ID must be a canonical decimal u64 string"));
    }
    Ok(value)
}

fn payload(text: &str) -> Result<Vec<u8>> {
    if text.len() > vlite::MAX_PAYLOAD_BYTES.div_ceil(3) * 4 {
        return Err(invalid("payload too large"));
    }
    let bytes = BASE64_STANDARD
        .decode(text)
        .map_err(|_| invalid("payload must be canonical padded base64"))?;
    if bytes.len() > vlite::MAX_PAYLOAD_BYTES {
        return Err(invalid("payload too large"));
    }
    Ok(bytes)
}

fn execute(handle: u64, request: Request) -> Result<Value> {
    let mut state = STATE
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| ("panic", "Native owner poisoned; restart the process".into()))?;
    match request {
        Request::Create { .. } | Request::Open { .. } => {
            if handle != 0 {
                return Err(invalid("create/open requires handle 0"));
            }
            let next = state
                .next
                .checked_add(1)
                .ok_or_else(|| invalid("handle space exhausted"))?;
            let database = match request {
                Request::Create { path, dimensions } => Database::create(path, dimensions),
                Request::Open { path } => Database::open(path),
                _ => unreachable!(),
            }
            .map_err(failure)?;
            state.next = next;
            state.databases.insert(next, database);
            return Ok(json!({"handle":next.to_string()}));
        }
        Request::Close {} => {
            let database = state
                .databases
                .remove(&handle)
                .ok_or_else(|| ("invalid_handle", "Unknown or closed handle".into()))?;
            database.close().map_err(failure)?;
            return Ok(Value::Null);
        }
        _ => {}
    }
    let db = state
        .databases
        .get_mut(&handle)
        .ok_or_else(|| ("invalid_handle", "Unknown or closed handle".into()))?;
    match request {
        Request::Info {} => {
            db.get(0).map_err(failure)?;
            Ok(
                json!({"dimensions":db.dimensions(),"records":db.len(),"format_version":db.format_version()}),
            )
        }
        Request::Get { id: text } => {
            let record = db
                .get(id(&text)?)
                .map_err(failure)?
                .ok_or_else(|| ("not_found", "Vector not found".into()))?;
            Ok(
                json!({"id":text,"vector":record.vector,"payload_base64":BASE64_STANDARD.encode(&record.payload),"metadata":record.metadata}),
            )
        }
        Request::GetMany { ids } => {
            db.get(0).map_err(failure)?;
            if ids.len() > 100 {
                return Err(invalid("get_many accepts at most 100 IDs"));
            }
            let ids = ids
                .into_iter()
                .map(|text| id(&text).map(|value| (text, value)))
                .collect::<Result<Vec<_>>>()?;
            let mut records = Vec::with_capacity(ids.len());
            let mut bytes = 1024;
            // Bound the complete JSON response, including escaping and the envelope.
            for (text, id) in ids {
                let item = match db.get(id).map_err(failure)? {
                    Some(record) => {
                        json!({"id":text,"vector":record.vector,"payload_base64":BASE64_STANDARD.encode(&record.payload),"metadata":record.metadata})
                    }
                    None => Value::Null,
                };
                bytes += item.to_string().len() + 1;
                if bytes > 8 * 1024 * 1024 {
                    return Err((
                        "response_too_large",
                        "get_many exceeds 8 MiB; request fewer IDs".into(),
                    ));
                }
                records.push(item);
            }
            Ok(json!({"records":records}))
        }
        Request::Put {
            id: text,
            vector,
            payload_base64,
            metadata,
        } => {
            db.put_with_metadata(id(&text)?, &vector, &payload(&payload_base64)?, &metadata)
                .map_err(failure)?;
            Ok(Value::Null)
        }
        Request::Delete { id: text } => {
            Ok(json!({"deleted":db.delete(id(&text)?).map_err(failure)?}))
        }
        Request::Batch { operations } => {
            if operations.len() > vlite::MAX_BATCH_OPERATIONS {
                return Err(invalid("batch exceeds 4096 operations"));
            }
            let operations = operations
                .into_iter()
                .map(|operation| {
                    Ok(match operation {
                        Operation::Put {
                            id: text,
                            vector,
                            payload_base64,
                            metadata,
                        } => Mutation::PutWithMetadata {
                            id: id(&text)?,
                            vector,
                            payload: payload(&payload_base64)?,
                            metadata,
                        },
                        Operation::Delete { id: text } => Mutation::Delete { id: id(&text)? },
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            db.write_batch(&operations).map_err(failure)?;
            Ok(Value::Null)
        }
        Request::Search {
            vector,
            k,
            filter,
            metric,
        } => {
            let hits = db
                .search_filtered_with_metric(&vector, k, &filter, metric)
                .map_err(failure)?;
            Ok(
                json!({"hits":hits.into_iter().map(|h| json!({"id":h.id.to_string(),"distance":h.distance})).collect::<Vec<_>>()}),
            )
        }
        Request::BuildIndex {} => {
            db.build_hnsw(HnswConfig::default()).map_err(failure)?;
            Ok(Value::Null)
        }
        Request::SearchApproximateReady {
            vector,
            k,
            ef_search,
        } => {
            let hits = db
                .search_approximate_ready(&vector, k, ef_search, HnswConfig::default())
                .map_err(failure)?;
            Ok(
                json!({"hits":hits.into_iter().map(|h| json!({"id":h.id.to_string(),"distance":h.distance})).collect::<Vec<_>>()}),
            )
        }
        Request::Create { .. } | Request::Open { .. } | Request::Close {} => unreachable!(),
    }
}

/// Returns the major ABI version supported by this library.
#[unsafe(no_mangle)]
pub extern "C" fn vlite_abi_version() -> u32 {
    1
}

/// Execute one UTF-8 JSON request. Returns an owned NUL-terminated JSON envelope.
///
/// # Safety
/// `input` must reference `length` readable bytes for the duration of this call.
/// A null pointer is rejected. Free the returned pointer exactly once with
/// `vlite_response_free`; never use a foreign allocator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlite_execute(
    handle: u64,
    input: *const u8,
    length: usize,
) -> *mut c_char {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if input.is_null() || length == 0 || length > 8 * 1024 * 1024 {
            return Err(invalid("request must contain 1..8388608 UTF-8 JSON bytes"));
        }
        // The caller owns the input; decoding produces fully owned request fields.
        let bytes = unsafe { std::slice::from_raw_parts(input, length) };
        let request =
            serde_json::from_slice(bytes).map_err(|e| ("invalid_input", e.to_string()))?;
        execute(handle, request)
    }))
    .unwrap_or_else(|_| {
        Err((
            "panic",
            "Native panic; owner may be poisoned; restart the process".into(),
        ))
    });
    let envelope = match result {
        Ok(value) => json!({"ok":true,"result":value}),
        Err((code, message)) => json!({"ok":false,"error":{"code":code,"message":message}}),
    };
    CString::new(envelope.to_string())
        .expect("JSON escapes NUL")
        .into_raw()
}

/// Release a response allocated by `vlite_execute`. NULL is a no-op.
///
/// # Safety
/// A non-null pointer must be an unfreed response returned by this same library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlite_response_free(response: *mut c_char) {
    if !response.is_null() {
        drop(unsafe { CString::from_raw(response) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(handle: u64, request: &[u8]) -> Value {
        unsafe {
            let response = vlite_execute(handle, request.as_ptr(), request.len());
            let value =
                serde_json::from_slice(std::ffi::CStr::from_ptr(response).to_bytes()).unwrap();
            vlite_response_free(response);
            value
        }
    }

    #[test]
    fn abi_rejects_malformed_requests_and_stale_handles() {
        assert_eq!(vlite_abi_version(), 1);
        for request in [
            b"[".as_slice(),
            b"\xff",
            b"{}",
            b"{\"op\":\"info\",\"extra\":true}",
        ] {
            assert_eq!(call(0, request)["error"]["code"], "invalid_input");
        }
        assert_eq!(
            call(0, b"{\"op\":\"info\"}")["error"]["code"],
            "invalid_handle"
        );
        unsafe {
            let response = vlite_execute(0, std::ptr::null(), 0);
            assert!(!response.is_null());
            vlite_response_free(response);
            vlite_response_free(std::ptr::null_mut());
        }
    }

    #[test]
    fn abi_handles_close_and_reopen_without_reusing_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("abi.vlite");
        let create = json!({"op":"create","path":path,"dimensions":2}).to_string();
        let first = call(0, create.as_bytes());
        assert_eq!(first["ok"], true);
        let first: u64 = first["result"]["handle"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            call(first, b"{\"op\":\"put\",\"id\":\"1\",\"vector\":[1,0]}")["ok"],
            true
        );
        assert_eq!(call(first, b"{\"op\":\"close\"}")["ok"], true);
        assert_eq!(
            call(first, b"{\"op\":\"close\"}")["error"]["code"],
            "invalid_handle"
        );
        let open = json!({"op":"open","path":path}).to_string();
        let second = call(0, open.as_bytes());
        let second: u64 = second["result"]["handle"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(
            call(second, b"{\"op\":\"get\",\"id\":\"1\"}")["result"]["vector"],
            json!([1.0, 0.0])
        );
        assert_eq!(call(second, b"{\"op\":\"close\"}")["ok"], true);
    }
}
