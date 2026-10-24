# VLite embedded Python

In-process binding for Python 3.11+, using ctypes and a bundled VLite shared
library. Platform wheels require no server, HTTP, Docker or Rust installation
at runtime. Install the wheel matching your operating system and architecture,
then use `NativeLibrary()` to load the included library automatically.
An explicit native library path is still supported.

For application use, install the published
[vlite-embedded package from PyPI](https://pypi.org/project/vlite-embedded/):

```text
python -m pip install vlite-embedded
```

The current published version is 0.3.0, with wheels for Windows x64, macOS
arm64 and Linux x86_64/glibc 2.28+. The API below also documents current
repository-source additions; those may not be present in the published wheel.

`native.create(path, dims)` creates format 3; `native.create(path, dims,
format=4)` enables named collections (`create_collection`, `list_collections`,
`set_collection_settings`, `drop_collection`) and resumable imports
(`next_import_job_id`, `import_chunk`, `import_status`, `import_finish`,
`import_abort`). Record methods accept `collection=` and
`expected_revision=` (`"absent"` or a revision string); conditional `put`
returns the new revision. `search(..., explain=True)`, `search_auto` and
`search_hybrid` expose plans, work statistics and BM25 fusion; `ann_quality` and
`ann_memory` report ANN recall and memory, including `quantization="scalar_u8"`.
Omitted metric, quantization and rerank multiplier use the collection's
settings. Server-scheduled maintenance, backups, metrics and authentication
remain server-only.
Offline point-in-time restore and encryption are available below.

See the [embedded deployment guide](https://github.com/mahpate/Vlite/blob/main/bindings/README.md)
for builds, installation, examples, ABI ownership and limits.
Licensed under MIT.


## Since 0.4.1: read-only multi-process access

This feature is included in 0.4.1 builds, not the PyPI 0.3.0 package.
Check the release and PyPI publication status before installing.
`native.open(path, read_only=True)`
opens an existing database with a shared OS lock. Multiple read-only handles may
coexist, including in different processes. `get` and `search` work normally;
mutations raise `EmbeddedError` with `code == "read_only"` without invalidating
the handle. Read-only opening also accepts `key=` or `passphrase=` for encrypted
files.

A writer (`native.open(path)`, the default) holds an exclusive lock: opening a
writer while any reader exists raises code `"locked"`, and opening a reader while
a writer exists does too. Close every reader before opening a writer. Each reader
uses a snapshot at open; close and reopen it to see new commits (no live refresh).

Within one Python process, C ABI calls on all handles are serialized. For
parallel reads, use multiple spawned processes, each loading its own
`NativeLibrary` and opening `read_only=True`; do not reuse inherited handles after
fork. Explicitly close handles, for example with `with native.open(...) as db:`.

## V5 embedded feature API

These bindings use ctypes, not PyO3/maturin. The wheel bundles the same ABI-1
Rust library as the Node package; new operations need a matching new library.
Default creation remains format 3. Pass `format=5` to enable TTL/metadata-only
writes and named/sparse/model-versioned vectors. Encryption and commit timestamps
imply format 5 if no explicit incompatible format is supplied.

```python
from vlite_embedded import NativeLibrary

native = NativeLibrary()  # or NativeLibrary("absolute/path/to/native-library")
with native.create_in_memory(2, format=5) as db:
    cursor = db.feed_cursor()
    db.put_with_expiry("1", [1, 0], expires_at_ms=2_000_000_000_000,
                       metadata={"tag": {"type": "string", "value": "example"}})
    db.update_metadata("1", {"tag": {"type": "string", "value": "updated"}})
    print(db.search_grouped([1, 0], 5, 2, "tag"))
    db.configure_vector("text", 2, active_version="model-v1", history=1)
    db.put_vectors("1", dense={"text": [{"version": "model-v1", "vector": [1, 0]}]},
                   sparse={"terms": [(7, 0.8)]})
    print(db.search_named("text", [1, 0], 5))
    print(db.search_dense_sparse("text", [1, 0], "terms", [(7, 1.0)], 5))
    print(db.query("SEARCH default NEAR $q K 5", {"q": [1, 0]}))
    print(db.changes_after(cursor))
    image = db.to_bytes()  # persist these bytes in host-owned storage
with native.open_from_bytes(image) as reopened:
    print(reopened.get("1"))
```

Additional methods (all accept `collection=` where meaningful):

- `put(..., expires_at_ms=...)` is an unconditional TTL write; the explicit
  `put_with_expiry` also accepts None for no expiry and returns a revision string.
  `update_metadata` replaces metadata only and returns its revision. TTL is an
  absolute Unix-ms timestamp, not a duration; revision preconditions are rejected
  for expiry writes. Base writes clear additional vectors; metadata-only writes
  preserve vectors, payload and expiry.
- `recommend(k, positive_ids=..., negative_ids=..., positive_vectors=...,
  negative_vectors=...)`, `facets(field)`, `numeric_stats(field)`,
  `search_geo(vector, k, field, latitude, longitude, radius_meters,
  sort="geo_distance")`; retrieval accepts optional AND `filter=` and `metric=`.
  Geo metadata is `{"type":"geopoint","value":{"latitude":0,"longitude":0}}`.
- `vector_specs`, `record_vectors(id)`, `put_vector_version(id,name,version,vector)`,
  `prune_vector_history(name,history)`, `search_sparse(name,pairs,k)`;
  `search_named(...,version="old-model")` selects retained history. `put_vectors`
  replaces all additional dense/sparse vectors, not a partial merge.
- `rerank(vector, candidates, limit, method="exact", metric=...)` or
  `method="mmr", lambda_=0.5` (always cosine; omit metric). Candidates are hit
  dictionaries with IDs and optional distances. The engine fetches live records.
- `feed_cursor()` starts at head; `changes_after(cursor=None,max_events=1000)`
  starts at the beginning when omitted and returns `{changes,next,has_more,head}`.
  Persist `next`; changes never split a committed batch. Expiry itself is not
  an event. Query syntax is documented in `docs/VECTORS.md`.
- `native.create/open(...,key=bytes_of_length_32)` or `passphrase="..."` enables
  encrypted creation/opening. `is_encrypted()`, `encryption_key_id()` and
  `encryption_info()` expose non-secret status. Credentials also work with
  in-memory create and byte-image open; never supply both credential kinds.
- `native.restore_to(closed_log,new_path,sequence="42")` or
  `timestamp_ms=...`, optionally `base_backup=...`, `key=`/`passphrase=`.
  Timestamp restore needs creation with `commit_timestamps=True`. Sources must
  be closed/copied; restore publishes a new file and never overwrites a source.
- `build_disk_ann(path,metric=...)`, `search_disk_ann(path,vector,k,
  ef_search=100,cache_bytes=...)`. Each search opens/closes its own bounded cache;
  every database mutation makes the sidecar stale. Disk ANN does not filter
  expiry/metadata, and encrypted databases refuse plaintext sidecars.

IDs/revisions are decimal strings. Absolute input timestamps also accept Python
integers (serialized losslessly as decimal strings). Feed timestamps/head,
lineage, key IDs and restore revisions are returned as strings. Metadata and
filter values retain the existing explicit typed JSON shape. Responses are
ordinary dictionaries/lists; byte images are `bytes`. New methods carry inline
Python type annotations; no separate stub file is required.

JSON requests/responses are capped at 8 MiB including base64 overhead, so large
byte images need a different persistence surface. In-memory images are durable
only when the host saves them. Encryption protects contents, not access patterns
or process memory; Python/JSON key copies cannot be reliably wiped. No provider
callbacks, GPU, read replica, re-embedding worker or named-ANN wrapper is exposed.
See `bindings/c/FEATURES.md` for the complete result/error contract and limits.

Build and test from the repository root (Rust 1.89+, Python 3.11+):

```text
cargo build --locked -p vlite-ffi
python -m unittest discover -s bindings/python -p test_features.py -v
python bindings/build_wheel.py --output embedded-wheels
python tests/embedded_wheel.py embedded-wheels
```

The direct feature suite defaults to `target/debug` and accepts
`VLITE_TEST_LIBRARY` to validate an exact alternate/bundled library. It creates
and cleans its test files below the binding directory.

## Optional contiguous float32 search and bulk puts

Source builds provide opt-in `search_f32` and `batch_put_f32` methods. Existing
list-based methods and the JSON ABI are unchanged. These methods avoid converting
each vector component to JSON; results and metadata still use JSON.

```python
from array import array
from vlite_embedded import NativeLibrary

native = NativeLibrary("target/debug/vlite_ffi.dll")
with native.create_in_memory(2) as db:
    db.batch_put_f32(
        [{"id": "18446744073709551615"}, {"id": "2", "payload_base64": "AP8="}],
        array("f", [1, 0, 0, 1]), 2,
    )
    hits = db.search_f32(array("f", [1, 0]), 2)
    planned = db.search_f32(array("f", [1, 0]), 2, mode="auto", explain=True)
```

Use a native-endian, C-contiguous float32 buffer, such as `array("f")`, a ctypes
float array, or an appropriately typed NumPy array (NumPy is not required).
Read-only buffers are accepted; strided, non-native-endian, float64, byte and
nonfinite buffers are rejected. Bulk buffers may be flat or row-major; they must
contain exactly `len(records) * dimensions` floats. Dimensions must be positive,
with at most 4096 rows and 2097152 floats per call. Record dictionaries contain
`id` and optional `payload_base64`/`metadata`; IDs retain canonical decimal u64
validation. Bulk puts retain `batch`'s atomic validation, commit and durability
semantics; they are not individually acknowledged writes.

`search_f32` exact mode has the same options/return shape as `search`; auto mode
returns the `search_auto` dictionary and accepts `ef_search`, `quantization` and
`rerank_multiplier`. Filters, metrics, named collections, explain output and
errors use the existing engine path. Unsupported option combinations are still
errors. The Python wrapper owns a validated snapshot during the call because
ctypes releases the GIL. This is a copy-reducing path, **not zero-copy**. The
existing per-handle Python lock and global native owner lock remain in place.

Published 0.3.0 wheels lack these native symbols (and their bundled Python API
may lack these methods). With this source wrapper and an older ABI-1 library,
`native.supports_float32` is false and the new methods fall back to JSON using
the buffer's exact float32 values. Set `require_native=True` on either method to
reject that fallback with `EmbeddedError(code="unsupported")`. Loading an old
library never requires a new symbol. Non-ABI-1 libraries remain unsupported.
Fallback retains the existing 8 MiB JSON request cap; a large typed buffer may
need smaller batches to fit the older library's JSON representation.

### Reproducing the bounded comparison

From the repository root, build a dedicated release library and run:

```text
cargo build -p vlite-ffi
python -m unittest discover -s bindings/python -p test_typed_vectors.py -v
cargo build -p vlite-ffi --release --target-dir target/perf-bindings
python bindings/python/benchmark_typed_vectors.py --library target/perf-bindings/release/vlite_ffi.dll --output target/perf-bindings/typed-results.json
```

Use `libvlite_ffi.so` on Linux or `libvlite_ffi.dylib` on macOS. The test defaults
to `target/debug`, like the existing binding suites; override
`VLITE_TEST_LIBRARY` to select another build, including the release benchmark library.
The benchmark uses only the standard library, alternates baseline/candidate
order, checks identical f32 values/results (including durable reopen), and
reports separate wrapper-conversion, search, in-memory bulk and durable bulk
timings. Both bulk modes execute the same one-batch commit, with creation,
verification and close outside the timing. It records raw samples, environment
and binary/source hashes; its bounds and workload are configurable.

One Windows shared-host run (5 rounds, 128D, 256 records, 80 queries,
128-row batches) measured these median JSON → float32 timings: query conversion
107.9 → 43.8 µs, bulk conversion 12.76 → 3.44 ms, search 185.5 → 125.5 µs,
in-memory bulk 19.70 → 6.03 ms, durable bulk 19.91 → 7.18 ms. A bounded 768D
run (128 records, 40 queries, 64-row batches) measured query conversion
549.9 → 201.1 µs and search 691.2 → 311.6 µs. Baseline lists were already
materialized; typed timings include snapshot copying and finite validation.
Concurrent build/benchmark agents can distort end-to-end results: this is not
isolated speedup evidence, nor a claim about cold cache, peak RSS, concurrent
throughput or all dimensions. Tiny vectors and JSON fallback can see no gain;
the fast path remains opt-in.
