# VLite embedded Python

In-process binding for Python 3.11+, using ctypes and a bundled VLite shared
library. Platform wheels require no server, HTTP, Docker or Rust installation
at runtime. Install the wheel matching your operating system and architecture,
then use `NativeLibrary()` to load the included library automatically.
An explicit native library path is still supported.

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
settings. Maintenance, backups, metrics and credentials are server-only.

See the [embedded deployment guide](https://github.com/mahpate/Vlite/blob/main/bindings/README.md)
for builds, installation, examples, ABI ownership and limits.
Licensed under MIT.
