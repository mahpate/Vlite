# VLite for WebAssembly

An in-memory VLite database for `wasm32-unknown-unknown`. The host persists the
database by saving `toBytes()` and reloading it with `fromBytes()`; there are no
files, OS locks or threads. Encryption at rest (32-byte raw key) is supported.

```text
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked
cd bindings/wasm
cargo build --release --target wasm32-unknown-unknown
wasm-bindgen target/wasm32-unknown-unknown/release/vlite_wasm.wasm --target nodejs --out-dir pkg
node test/smoke.mjs pkg/vlite_wasm.js
```

Use `--target web` or `bundler` for browsers. Call `setClock(Date.now())` before
operations that depend on time (TTL, commit timestamps): WebAssembly has no
system clock.

API: `VLiteDatabase.create(dimensions, key?)`, `VLiteDatabase.fromBytes(bytes, key?)`,
`put(id, Float32Array, Uint8Array)`, `get(id)`, `delete(id)`, `search(query, k)`
(flat `[id, distance, ...]`), `len()`, `isEmpty()`, `toBytes()`. IDs are JS safe
integers. Limits: file/path APIs, background HNSW rebuilds (no threads), sidecars
and GPU search are unavailable; exact search and synchronous `build_hnsw` work.
