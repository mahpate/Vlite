//! Borrowed float32 inputs with the same owner lock, execution and response ABI.
use super::*;

const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bulk {
    dimensions: usize,
    operations: Vec<Put>,
    collection: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Put {
    id: String,
    #[serde(default)]
    payload_base64: String,
    #[serde(default)]
    metadata: Metadata,
}

unsafe fn inputs<'a>(
    input: *const u8,
    length: usize,
    vectors: *const f32,
    count: usize,
) -> Result<(&'a [u8], &'a [f32])> {
    if input.is_null() || length == 0 || length > MAX_BYTES {
        return Err(invalid("request must contain 1..8388608 UTF-8 JSON bytes"));
    }
    if count > MAX_BYTES / size_of::<f32>()
        || (count != 0
            && (vectors.is_null() || !(vectors as usize).is_multiple_of(align_of::<f32>())))
    {
        return Err(invalid(
            "vector buffer must be aligned and contain at most 2097152 float32 values",
        ));
    }
    let bytes = unsafe { std::slice::from_raw_parts(input, length) };
    let values = if count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(vectors, count) }
    };
    if values.iter().any(|value| !value.is_finite()) {
        return Err(invalid("vector values must be finite"));
    }
    Ok((bytes, values))
}

fn caught(action: impl FnOnce() -> Result<Value>) -> *mut c_char {
    response(catch_unwind(AssertUnwindSafe(action)).unwrap_or_else(|_| {
        Err((
            "panic",
            "Native panic; owner may be poisoned; restart the process".into(),
        ))
    }))
}

/// Search with a borrowed contiguous native-endian float32 vector.
///
/// # Safety
/// Input references `length` readable bytes; vectors references `count` aligned
/// readable floats. Both are immutable for this call. Free the result with
/// `vlite_response_free`. Invalid non-null pointers are caller undefined behavior.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlite_search_f32(
    handle: u64,
    input: *const u8,
    length: usize,
    vectors: *const f32,
    count: usize,
) -> *mut c_char {
    caught(|| {
        let (bytes, values) = unsafe { inputs(input, length, vectors, count)? };
        let mut request: Request =
            serde_json::from_slice(bytes).map_err(|error| ("invalid_input", error.to_string()))?;
        match &mut request {
            Request::Search { vector, .. } | Request::SearchApproximateReady { vector, .. }
                if vector.is_none() =>
            {
                *vector = Some(values.to_vec())
            }
            _ => {
                return Err(invalid(
                    "typed search requires op search or search_approximate_ready with vector omitted",
                ));
            }
        }
        execute(handle, request)
    })
}

/// Atomic put batch with row-major borrowed native-endian float32 vectors.
///
/// # Safety
/// Same pointer and response ownership contract as `vlite_search_f32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vlite_batch_put_f32(
    handle: u64,
    input: *const u8,
    length: usize,
    vectors: *const f32,
    count: usize,
) -> *mut c_char {
    caught(|| {
        let (bytes, values) = unsafe { inputs(input, length, vectors, count)? };
        let bulk: Bulk =
            serde_json::from_slice(bytes).map_err(|error| ("invalid_input", error.to_string()))?;
        if bulk.dimensions == 0
            || bulk.operations.len() > vlite::MAX_BATCH_OPERATIONS
            || bulk.operations.len().checked_mul(bulk.dimensions) != Some(values.len())
        {
            return Err(invalid(
                "bulk requires positive dimensions, at most 4096 rows and rows * dimensions floats",
            ));
        }
        let operations = bulk
            .operations
            .into_iter()
            .zip(values.chunks(bulk.dimensions))
            .map(|(put, vector)| Operation::Put {
                id: put.id,
                vector: vector.to_vec(),
                payload_base64: put.payload_base64,
                metadata: put.metadata,
            })
            .collect();
        execute(
            handle,
            Request::Batch {
                operations,
                collection: bulk.collection,
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn envelope(pointer: *mut c_char) -> Value {
        assert!(!pointer.is_null());
        let value = serde_json::from_slice(unsafe { std::ffi::CStr::from_ptr(pointer) }.to_bytes())
            .unwrap();
        unsafe { vlite_response_free(pointer) };
        value
    }

    #[test]
    fn typed_buffer_contract_and_atomic_errors() {
        let create = br#"{"op":"features","request":{"op":"create","dimensions":2}}"#;
        let created = unsafe { envelope(vlite_execute(0, create.as_ptr(), create.len())) };
        let handle = created["result"]["handle"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let search = br#"{"op":"search","k":2}"#;
        let bulk = br#"{"dimensions":2,"operations":[{"id":"18446744073709551615"},{"id":"2"}]}"#;
        let values = [1.0, 0.0, 0.0, 1.0];
        unsafe {
            assert_eq!(
                envelope(vlite_batch_put_f32(
                    handle,
                    bulk.as_ptr(),
                    bulk.len(),
                    values.as_ptr(),
                    4
                ))["ok"],
                true
            );
            let hits = envelope(vlite_search_f32(
                handle,
                search.as_ptr(),
                search.len(),
                values.as_ptr(),
                2,
            ));
            assert_eq!(hits["result"]["hits"][0]["id"], "18446744073709551615");
            for (pointer, count) in [
                (std::ptr::null(), 2),
                (values.as_ptr().byte_add(1), 2),
                (values.as_ptr(), usize::MAX),
                (values.as_ptr(), 1),
            ] {
                assert_eq!(
                    envelope(vlite_search_f32(
                        handle,
                        search.as_ptr(),
                        search.len(),
                        pointer,
                        count
                    ))["error"]["code"],
                    "invalid_input"
                );
            }
            assert_eq!(
                envelope(vlite_batch_put_f32(
                    handle,
                    bulk.as_ptr(),
                    bulk.len(),
                    values.as_ptr(),
                    3
                ))["error"]["code"],
                "invalid_input"
            );
            assert_eq!(
                envelope(vlite_search_f32(
                    handle,
                    std::ptr::null(),
                    0,
                    values.as_ptr(),
                    2
                ))["error"]["code"],
                "invalid_input"
            );
            assert_eq!(
                envelope(vlite_search_f32(
                    handle,
                    search.as_ptr(),
                    search.len(),
                    [f32::NAN, 0.0].as_ptr(),
                    2
                ))["error"]["code"],
                "invalid_input"
            );
            let bad = br#"{"dimensions":2,"operations":[{"id":"3"},{"id":"02"}]}"#;
            assert_eq!(
                envelope(vlite_batch_put_f32(
                    handle,
                    bad.as_ptr(),
                    bad.len(),
                    values.as_ptr(),
                    4
                ))["error"]["code"],
                "invalid_input"
            );
            let get = br#"{"op":"get","id":"3"}"#;
            assert_eq!(
                envelope(vlite_execute(handle, get.as_ptr(), get.len()))["error"]["code"],
                "not_found"
            );
            let empty = br#"{"dimensions":2,"operations":[]}"#;
            assert_eq!(
                envelope(vlite_batch_put_f32(
                    handle,
                    empty.as_ptr(),
                    empty.len(),
                    std::ptr::null(),
                    0
                ))["ok"],
                true
            );
            let close = br#"{"op":"close"}"#;
            assert_eq!(
                envelope(vlite_execute(handle, close.as_ptr(), close.len()))["ok"],
                true
            );
            assert_eq!(
                envelope(vlite_search_f32(
                    handle,
                    search.as_ptr(),
                    search.len(),
                    values.as_ptr(),
                    2
                ))["error"]["code"],
                "invalid_handle"
            );
        }
    }

    #[test]
    fn typed_approximate_ready_search_matches_json() {
        let create = br#"{"op":"features","request":{"op":"create","dimensions":2}}"#;
        let created = unsafe { envelope(vlite_execute(0, create.as_ptr(), create.len())) };
        let handle: u64 = created["result"]["handle"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let bulk = br#"{"dimensions":2,"operations":[{"id":"1"},{"id":"2"},{"id":"3"}]}"#;
        let values = [1.0, 0.0, 0.0, 1.0, 0.5, 0.5];
        let typed = br#"{"op":"search_approximate_ready","k":2,"ef_search":10}"#;
        let json = br#"{"op":"search_approximate_ready","vector":[1,0],"k":2,"ef_search":10}"#;
        let both = br#"{"op":"search_approximate_ready","vector":[1,0],"k":2,"ef_search":10}"#;
        let missing = br#"{"op":"search_approximate_ready","k":2,"ef_search":10}"#;
        let build = br#"{"op":"build_index"}"#;
        unsafe {
            assert_eq!(
                envelope(vlite_batch_put_f32(
                    handle,
                    bulk.as_ptr(),
                    bulk.len(),
                    values.as_ptr(),
                    6
                ))["ok"],
                true
            );
            assert_eq!(
                envelope(vlite_execute(handle, build.as_ptr(), build.len()))["ok"],
                true
            );
            let expected = envelope(vlite_execute(handle, json.as_ptr(), json.len()));
            assert_eq!(expected["result"]["hits"][0]["id"], "1");
            let actual = envelope(vlite_search_f32(
                handle,
                typed.as_ptr(),
                typed.len(),
                values.as_ptr(),
                2,
            ));
            assert_eq!(actual, expected);
            for (input, pointer, count) in [
                (&both[..], values.as_ptr(), 2),
                (&typed[..], values.as_ptr(), 1),
                (&missing[..], std::ptr::null(), 0),
            ] {
                assert_eq!(
                    envelope(vlite_search_f32(
                        handle,
                        input.as_ptr(),
                        input.len(),
                        pointer,
                        count
                    ))["error"]["code"],
                    "invalid_input"
                );
            }
            assert_eq!(
                envelope(vlite_execute(handle, missing.as_ptr(), missing.len()))["error"]["code"],
                "invalid_input"
            );
            let close = br#"{"op":"close"}"#;
            vlite_execute(handle, close.as_ptr(), close.len());
        }
    }
}
