# Task A — HA ITEM-021: gateway reader routing (code)

Repository: mahpate/Vlite (Rust). You work in a fresh clone at the plan base.
Read `docs/projects/feature-aca-ha/feature-aca-ha.prd.md` rows FR-010 (~line 309),
FR-011 (310), FM-006 (441), FM-009 (444), RD-007 (486), OD-007 (501), section
3.4.6/3.7 (~230-240) and ITEM-021 (~845) before you start.

## Goal

Make the DB-free gateway (`vlite-server gateway`, `src/server/gateway_mode.rs`)
send staleness-tolerant **reads** of a tenant to that tenant's reader backend
when, and only when, the tenant's control-state record opts in, and fall back to
the writer whenever the reader cannot be trusted. Writes always go to the writer.

## Exclusive write scope (touch nothing else)

- `src/server/gateway.rs`
- `src/server/gateway_mode.rs`
- `src/bin/vlite-server.rs`
- `tests/server_cli.rs`

Do NOT edit any docs, `src/server.rs`, `src/server/serve_mode.rs`,
`src/server/reserved.rs`, `src/server/reader_apply.rs`,
`src/server/control_state.rs`, or anything under `deploy/`. Another task edits
those files in parallel. Add no new crate dependencies.

## Existing code you build on

- `src/server/control_state.rs` 67-100: `ReadRouting { WriterOnly (default), Readers }`
  and `Route { writer, reader: Option<String>, audience, lineage: u64, epoch: Option<u64>,
  binding_generation: u64, read_routing, .. }`. `valid` already guarantees
  `Readers` ⇒ `reader.is_some()` and only `dedicated` records may have a reader.
  Test constructors `Route::dedicated_for_test(tenant, epoch)` (reader Some,
  lineage 0x1234, binding_generation 3, WriterOnly) and `shared_for_test`.
- `src/server/gateway.rs`: `Target { endpoint, audience, epoch, operation }` (~395),
  `route_target` (~435), `target_for` (~447, always the writer today), `mint` (~462),
  `Delivery`/`may_retry` (~485-505, currently unused), `EpochRefusal`/`check_epoch`.
  Module doc lines 19-32 say "The target is always the writer; reader routing is
  ITEM-021" and "it never retries" — update them.
  Test `every_request_targets_the_writer_even_with_reader_routing` (~908) must be
  replaced by tests of the new behaviour.
- `src/server/gateway_mode.rs` `forward` (~345-495): route_target → `upstream_url`
  → `mint` → body buffered (`axum::body::to_bytes`, so it can be re-sent) →
  `request_headers` + envelope header → `tokio::time::timeout(self.upstream_timeout,
  client.request(..).send())` → `response_headers` (passes `x-vlite-*`) → `pump`.
  `upstream_failure(connect, timeout, operation)` maps errors.
- `src/bin/vlite-server.rs` ~537-546: a gateway-trusted backend refuses
  `--serve-mode reader` ("--gateway-trust-file cannot be used with --serve-mode reader").
  A reader behind the gateway is gateway-trusted, so lift that one refusal (keep the
  others). `tests/server_cli.rs` ~1912 lists `(vec![os("--serve-mode"), os("reader")], false)`
  as an expected refusal: change it. A reader needs an existing database
  (reader mode opens read-only), so drop the case from the refusal list and add a
  separate assertion that a trust-file reader start on an existing database is no
  longer refused for that reason (it may still fail later for another reason; assert
  stderr does not contain "cannot be used with --serve-mode reader").
- `src/server/gateway_trust.rs` ~245 checks envelope audience + lineage; a reader
  that uses the writer's audience and has the same lineage accepts the writer's
  envelope. Mint the reader envelope with the record's `audience` (same as writer).

## Required behaviour (the contract — a parallel task implements the reader side)

1. `target_for`: a `Read` goes to `record.reader` only if
   `record.read_routing == ReadRouting::Readers` and `record.reader` is `Some`.
   Everything else, and every `Write`, goes to `record.writer`. Expose enough on
   `Target` (e.g. `fallback: Option<String>` = writer endpoint, plus the expected
   `lineage` and `binding_generation`) for the forwarder to fall back.
2. Reader response validation in `forward` (only when the target is a reader):
   - Reader-mode backends stamp these response headers (added in a parallel task):
     `X-VLite-Lineage` = `format!("{:016x}", lineage)`,
     `X-VLite-Binding-Generation` = decimal u64, `X-VLite-Applied-Age-Ms` = decimal,
     `X-VLite-Applied-Sequence` = decimal (may be absent when hidden).
   - Fall back to the writer (re-send the same buffered body with a freshly minted
     envelope for the writer target) when: connect error or timeout before a response
     head, response status 503, `X-VLite-Lineage` missing or ≠
     `format!("{:016x}", record.lineage)`, or `X-VLite-Binding-Generation` missing or
     ≠ `record.binding_generation`.
   - Otherwise return the reader's response unchanged (headers pass through).
   - Read fallback is always safe (`Delivery::may_retry` allows reads after any
     failure); use it so it is no longer dead code, or drop its `allow(dead_code)`.
   - At most one fallback per request; the writer's result is final.
   - Optional: after a mismatch/failure, skip that reader for ~10 s (a small
     `Mutex<HashMap<String, Instant>>` on the gateway state). Keep it small.
3. HA-off behaviour must stay identical: with `WriterOnly` (the default) or no
   reader, requests go to the writer exactly as today, no extra headers, no extra
   latency. Standalone `vlite-server` (no gateway) is untouched (AC-010).

## Tests (test-first)

Unit tests in `gateway.rs` for `target_for` (Readers+reader → reader; WriterOnly
→ writer; Write with Readers → writer; shared-pool → writer). Tests in
`gateway_mode.rs` (the existing test harness ~1048 spins tiny axum upstreams;
reuse it) for: reader answer accepted when headers match; fallback on 503; on
lineage mismatch; on missing/mismatched binding generation; on a reader that
refuses the connection; a write with `Readers` never touches the reader.
Update the CLI test.

## Validation you must run before committing

```
cargo fmt --check
cargo clippy -q --features server --lib --tests -- -D warnings
cargo clippy -q --all-features --lib --tests -- -D warnings
cargo clippy -q --features server --bins -- -D warnings
cargo test -q --features server --lib -- gateway
cargo test -q --features server --test server_cli
```

All must pass. Commit on your result branch with a message that says what
changed, the fallback rules and the new test names.
