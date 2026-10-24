# Embedded, cross-language deployment

VLite can run inside your application's process instead of as a REST service.
The Rust engine is compiled to a native shared library with a C-compatible
interface. Python uses the included ctypes binding; Node.js/TypeScript uses a
worker-backed N-API addon; C/C++ can use the header directly. Other languages
can bind the same three exported functions using P/Invoke, cgo, JNI/FFM or
their native FFI tooling. Dedicated C#, Go and Java bindings are not included.
Use [the embedded Node package](node/README.md) for server-free JavaScript;
the separate package under `clients/javascript` remains a REST client.

## Build once, deploy without Rust

From the repository root, build for the machine that will run the application:

```text
cargo build --locked --release -p vlite-ffi
```

Deploy the resulting library next to your application:

| Platform | Build output |
|---|---|
| Windows | `target\release\vlite_ffi.dll` |
| Linux | `target/release/libvlite_ffi.so` |
| macOS | `target/release/libvlite_ffi.dylib` |

The consumer needs the native library and a compatible OS/architecture/runtime,
but no Rust, Cargo, Docker, HTTP service, database credentials or network port.
Build on your deployment baseline: native OS runtime requirements still apply.
The release workflow packages the library/header and binding sources with native
archives; workflow configuration does not imply that a release is published.
ARM and other targets require their own builds and validation.

## Python embedded usage

For application use, install the published
[vlite-embedded package from PyPI](https://pypi.org/project/vlite-embedded/)
with Python 3.11+:

```text
python -m pip install vlite-embedded
```

PyPI 0.3.0 includes bundled wheels for Windows x64, macOS arm64 and Linux
x86_64/glibc 2.28+. `NativeLibrary()` loads the bundled native library.

For contributors or source-only APIs, build a platform-specific wheel from the
checkout (the build host needs Rust, a native linker, Python and access to
Python build dependencies):

```text
python bindings/build_wheel.py --output embedded-wheels
python tests/embedded_wheel.py embedded-wheels
```

Install the generated `.whl` into the consuming application with
`python -m pip install <wheel-path>`. The consuming machine needs only Python
and its normal native OS runtime, not Rust or a server. CI retains these wheels
as artifacts and the release workflow includes them with checksums. Tagged
releases are uploaded to PyPI as `vlite-embedded` by
[`.github/workflows/pypi.yml`](../.github/workflows/pypi.yml); see the
[published package](https://pypi.org/project/vlite-embedded/) and
[maintainer publication details](../docs/INSTALL.md#pypi-publication-details-for-maintainers).

Version 0.4.1 builds add APIs absent from the older 0.3.0 wheel, including
`NativeLibrary.open(..., read_only=True)` and typed float32 inputs.
Check [release assets](https://github.com/mahpate/Vlite/releases/tag/v0.4.1)
and the actual PyPI version; registry uploads follow release verification.
Use source-build instructions when the required package is not yet available.

For installation without a checkout or Rust toolchain, use the
[versioned release packages and installation guide](../docs/INSTALL.md).

```python
from vlite_embedded import NativeLibrary, EmbeddedError

native = NativeLibrary()  # Loads the native library bundled in the wheel.
with native.create("documents.vlite", dimensions=3) as db:
    db.put("18446744073709551615", [1, 0, 0], payload_base64="AP8=",
           metadata={"category": {"type": "string", "value": "example"}})
    print(db.search([0.9, 0.1, 0], 5))
    db.build_index()
    print(db.search_approximate_ready([0.9, 0.1, 0], 5))

with native.open("documents.vlite") as db:
    print(db.get("18446744073709551615"))
```

`create` refuses an existing file; `open` never creates or automatically repairs
one. Close with a context manager or `close()` explicitly: there is no automatic
finalizer that hides close failures. The native handle remains owned until
closed or the process exits. Use multiprocessing **spawn**, not fork with a
loaded library/open database. The binding rejects calls from a forked child.

Wheels contain the wrapper and host-native binary and use `py3-none-<platform>`
tags, never `py3-none-any`. ctypes does not require a separate build per CPython
ABI, but the OS, architecture and native runtime must match. Linux builds use
the host's `linux_*` platform tag, **not a manylinux compatibility claim**; build
on your deployment baseline. macOS builds align the Rust deployment target with
the Python build configuration. Cross-compilation is not supported by this
builder; build and run installation checks on each target host.

For **Linux x86_64 with glibc 2.28 or newer**, use the dedicated manylinux build
instead of a host-specific wheel:

```text
docker build --file bindings/manylinux.Dockerfile --target artifacts --output type=local,dest=manylinux-wheels .
```

This builds with Rust 1.89 in the PyPA manylinux_2_28 image, runs `auditwheel
repair` for that policy, inspects the result and tests isolated installation.
CI and release gates also test the repaired wheel in a separate Debian Python
3.11 container without network access. Only repaired wheels receive the
`manylinux_2_28_x86_64` tag; ordinary host builds remain `linux_*`.
This does not support musl/Alpine, glibc older than 2.28, or ARM64.
Images use maintained upstream tags; record resolved digests for a particular
release build rather than assuming byte-for-byte reproducibility.

For advanced deployment, `NativeLibrary(absolute_path)` loads a separately
supplied compatible library. Missing bundled libraries produce an explicit
installation error rather than searching arbitrary directories or downloading
code. Direct `pip install ./bindings/python` requires `VLITE_NATIVE_LIBRARY`
to identify an already built host-native artifact; prefer the wheel builder.

## Local release-candidate bundle

After building a wheel, create a new candidate archive:

```text
python bindings/release_candidate.py --wheel <path-to-wheel> --output vlite-candidate.zip
python bindings/release_candidate.py --verify vlite-candidate.zip
```

The assembler first installs the exact wheel into an isolated environment and
checks CRUD, search, index maintenance and reopen. It bundles that wheel, its
exact native library, C header, MIT license, operational reference, installation
instructions and a SHA-256 manifest. Existing output files are never replaced.
CI creates a candidate per host alongside the wheel artifacts without tagging,
pushing or publishing a release.

The manifest covers every archive payload and records packaging-checkout source
hashes. Verification rejects changed files, extra entries and unsafe paths.
Checksums detect damage, not malicious replacement: these archives are **not
signed**. Source hashes alone do not prove that an externally supplied wheel was
built from that source; use the wheel produced from the same trusted CI checkout.
Windows standalone consumers using static import linking still need an import
library; the candidate's header/library pair supports dynamic ABI loading.
This is a host-validated candidate, not a stable release or portable-Linux claim.

## C ABI v1

Include [`c/vlite.h`](c/vlite.h) and link to the shared library. The three symbols
use the C calling convention:

```c
uint32_t vlite_abi_version(void);
char *vlite_execute(uint64_t handle, const uint8_t *input, size_t length);
void vlite_response_free(char *response);
```

For a Linux C consumer, for example:

```text
cc -Wall -Wextra -Werror -Ibindings/c bindings/c/example.c -Ltarget/release -lvlite_ffi -o vlite-c-example
LD_LIBRARY_PATH=target/release ./vlite-c-example
```

The example checks the ABI and error/response ownership without opening a file.
Windows C/C++ consumers also link the `vlite_ffi.dll.lib` import library included
in Windows release archives.

Check that the ABI major version is 1. `input` is borrowed UTF-8 JSON for the
duration of the call; its length excludes any trailing NUL. Requests are bounded
at 8 MiB. A returned NUL-terminated UTF-8 string is owned by this library:
copy/read it, then free it **exactly once** with `vlite_response_free`, never
`free()` or another library instance. Freeing NULL is allowed. Input NULL,
empty/oversized requests and unknown/stale handles produce errors. Invalid
non-NULL pointers, double frees, unloading with live handles, and calls into an
unloaded library remain caller errors; FFI cannot make invalid memory safe.

Each response is one of:

```json
{"ok":true,"result":null}
{"ok":false,"error":{"code":"invalid_handle","message":"Unknown or closed handle"}}
```

Use handle 0 for `create` or `open`. Their result is
`{"handle":"1"}` (an example, not a fixed ID); convert that string to a u64 for
subsequent calls. Handles are process-local, never reused during that loaded
library's lifetime, and must not be shared across processes or library copies.

| Request `op` | Additional fields | Successful `result` |
|---|---|---|
| `create` | `path`: UTF-8 string, `dimensions`: integer, optional `format` (3 default, or 4) | `{"handle":"..."}` |
| `open` | `path`: UTF-8 string | `{"handle":"..."}` |
| `close` | none | null; handle consumed |
| `info` | none | dimensions, format_version, records |
| `put` | id, vector, optional payload_base64/metadata/collection/expected_revision | null; `{"revision":"..."}` when expected_revision is given |
| `get` | id, optional collection | id, vector, payload_base64, metadata, revision |
| `get_many` | ids: up to 100 canonical ID strings, optional collection | `{"records":[record,null,...]}` in request order |
| `delete` | id, optional collection/expected_revision | `{"deleted":true}` or false |
| `batch` | operations: put/delete objects with `op`, optional collection | null |
| `revision` | id, optional collection | `{"revision":"..."}` or `{"revision":null}` |
| `search` | vector, k, optional filter/metric/collection/mode/explain and mode-specific fields below | `{"hits":[{"id":"...","distance":0.0}]}` |
| `build_index` | optional metric (squared_l2/cosine), collection, quantization, rerank_multiplier | null |
| `search_approximate_ready` | vector, k, ef_search, optional metric (squared_l2/cosine), collection, quantization, rerank_multiplier | same hits envelope |
| `list_collections` | none | `{"collections":[collection,...]}` |
| `create_collection` | name, dimensions, optional settings | collection |
| `drop_collection` | name | null |
| `set_collection_settings` | name, settings (full replacement) | `{"persisted":true}` or false |
| `next_import_job_id` | none | `{"job":"..."}` |
| `import_chunk` | job, chunk, operations, optional collection, policy (`reject_chunk` default or `skip_invalid`) | chunk report |
| `import_status` | job | import status |
| `import_finish` / `import_abort` | job, optional collection | import status |
| `ann_quality` | optional collection, metric, quantization, rerank_multiplier, samples, k, ef_search, seed, max_distance_evaluations | recall_at_k, samples_used, distance_evaluations, skipped_reason |
| `ann_memory` | optional collection, metric, quantization, rerank_multiplier | graph_nodes and byte counts |

Example creation: `{"op":"create","path":"documents.vlite","dimensions":3}`.
IDs are canonical decimal-u64 strings, and integer metadata values are
canonical decimal-i64 strings. Metadata, filters, payloads and atomic batches
follow the [existing JSON shapes](../src/openapi.json). Extra fields are errors.
`get_many` preserves duplicates, uses null for missing IDs and fails the whole
read with `response_too_large` if the response would exceed 8 MiB. It never
truncates results or silently skips IDs. Python exposes it as `db.get_many(ids)`.
Search is exact with optional `metric` (`squared_l2`, `cosine`, `inner_product`);
the default remains squared L2. Python uses `db.search(vector, k, metric="cosine")`.
Cosine rejects zero queries and excludes zero-norm records; inner product
returns negative dot product. Lower distance is better for every metric.
See [metric contracts](../docs/REFERENCE.md#exact-distance-metrics-v011).
Approximate-ready search supports squared L2 (default) and cosine and requires
an explicit matching-metric build after reopen/overflow or metric change.
Python uses `db.build_index(metric="cosine")` and
`db.search_approximate_ready(query, k, metric="cosine")`.
Inner product is exact-only. Cosine ANN requires a v0.1.2+ native
library and wrapper; v0.1.1 packages have L2 ANN only.
Omitting the default metric keeps old native requests compatible.
Ready-only never silently builds or falls back. REST metrics and lazy ANN are
not exposed by the native binding.

### Additive v1 operations: collections, revisions, planning, imports

These fields and operations are additive to ABI v1 (`vlite_abi_version()` stays
1). Requests without them behave exactly as before; the only change to existing
results is the extra `revision` string on records from `get`/`get_many`.

- **Format.** `create` makes a format-3 file unless `"format":4` is given. Named
  collections, settings changes and imports require format 4 and fail on format 3
  with `requires_format_v4`. Existing files are never upgraded by the binding.
- **Collections.** Omitting `collection` addresses `default`. A collection
  result is `{id,name,dimensions,live_records,settings,settings_persisted}`.
  Settings are `{metric, hnsw:{m,ef_construction,seed}, quantization
  (none/scalar_u8), rerank_multiplier, planner:{exact_max_candidates,
  exact_max_selectivity_ppm}, metadata_indexes:[{field,kind:eq/range/eq_range}],
  text_fields}`. Input settings may omit any field (engine default) but unknown
  fields are errors; `set_collection_settings` replaces the whole settings
  object. Codes: `collection_not_found`, `collection_exists`, `collection_limit`,
  `collection_not_empty`, `default_collection_immutable`.
- **Defaults.** When a search, build or quality request omits `metric`,
  `quantization` or `rerank_multiplier`, the collection's declared setting is
  used, and the HNSW configuration always comes from the collection's settings.
  The `default` collection's settings are the historical defaults (squared L2,
  no quantization, HNSW defaults), so legacy requests are unchanged.
- **Revisions.** `expected_revision` is `"absent"` (the ID must not exist) or a
  canonical decimal revision string (the record must have exactly that revision).
  A mismatch fails with `precondition_failed` and writes nothing.
- **Search modes.** `mode` is `exact` (default), `auto` or `hybrid`; `explain`
  adds `stats` (and `plan` for `auto`); without it the result is `{"hits":[...]}`.
  `exact` rejects ANN/text fields. `auto` accepts `ef_search` (default 100),
  `quantization`, `rerank_multiplier` and returns a plan
  `{strategy: exact_scan|exact_indexed|filtered_ann|ann, estimated_candidates,
  live_records, index_field, reason}`; it never builds an index. `hybrid`
  requires `text` (BM25 over the collection's `text_fields`), an optional
  `vector`, `fusion:{candidates,rrf_k,vector_weight,text_weight}` and
  `vector_leg` `exact` (default) or `approximate_ready`; its hits are
  `{id,score,distance,bm25,vector_rank,text_rank}`.
- **Imports.** Get a job with `next_import_job_id`, then send chunks numbered
  consecutively from `"1"`. Results report `outcome`
  (`committed`, `already_committed`, `rejected`), row counts, up to 100
  `invalid` rows `{row,code,message}` and the commit `revision`. Resending the
  most recent committed chunk with the same content is `already_committed`;
  different content, a different collection or a skipped number fails with
  `import_conflict`. A rejected chunk writes nothing and may be resent. Status is
  `{job,collection,collection_dropped,state:open|finished|aborted,last_chunk,
  rows_committed,rows_skipped,last_digest}` and survives reopen. Abort stops a
  job but never rolls back committed chunks.
- **Quality and memory.** `ann_quality` samples recall against exact search
  within `max_distance_evaluations`; `skipped_reason` explains a skipped
  measurement. `ann_memory` reports index memory in bytes and requires a ready
  index for that metric and quantization.

IDs, revisions, job and chunk numbers, seeds and digests are strings; counts
are JSON numbers. Automatic index maintenance, scheduled backups and restore
drills, metrics and slow-query logs, and scoped credentials with audit logs are
**server-only** features: the embedded binding provides no operations for them.

## Ownership, errors and durability

All calls are synchronous. ABI v1 serializes all handles through one in-process
mutex, including calls from multiple language threads. A slow build blocks
other ABI operations. A database file still has one exclusive owner across
processes and across native/REST access; this is not concurrent multi-process
shared-file access.

Engine error codes include `locked`, `not_found`, `corrupt`, `incomplete_tail`,
`format_error`, `invalid_input`, `io_error`, `index_not_ready`, `poisoned`,
`sequence_exhausted` and `commit_indeterminate`; format-4 operations add
`requires_format_v4`, `precondition_failed`, `import_not_found`,
`import_conflict`, `import_limit` and the collection codes above. Python raises
`EmbeddedError`
with `code`. Failed opens do not mutate/repair data. Success retains the engine's
sync-before-acknowledgment contract and its filesystem/hardware limitations.
There are no retries. An indeterminate commit may have succeeded; close/reopen
and reconcile using the [recovery contract](../docs/REFERENCE.md).

Rust unwind panics are caught before the C boundary. `panic` errors may leave
the owner mutex poisoned: restart the process rather than retrying. Allocation
failure, aborting panics, invalid foreign pointers and OS process termination
cannot be recovered by the wrapper. JSON encoding/copying adds overhead; this
is a portability-oriented interface, not a zero-copy vector API.

## Verification

```text
cargo test --locked --workspace --all-features
cargo build --locked -p vlite-ffi
python tests/embedded_e2e.py target/debug/libvlite_ffi.so
```

Use the corresponding DLL/dylib path on Windows/macOS. Native integration covers
Unicode paths, full-width IDs, binary payloads, typed metadata, exact filters,
atomic rejection, index maintenance, thread serialization, lock exclusion,
close/reopen across processes and corruption rejection. CI runs on the native
Windows/Linux/macOS runners; only local results establish what was tested here.

## Reproducible binding performance

```text
python benchmarks/embedded.py --library target/release/libvlite_ffi.so --output embedded-results.json
```

Use the platform library path above. The runner compares per-record durable puts
with atomic batch puts and cached per-record gets with `get_many`. It verifies
contents and exact search against an independent oracle, alternates operation
order, records raw timings and hashes the binary/source. Batch writes acknowledge
once per batch, not per record; this changes the durability granularity and is
not a free per-operation speedup. These measurements include Python/JSON/FFI
overhead and are not measurements of Rust-only search or network service latency.
