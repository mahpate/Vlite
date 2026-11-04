# K1 (lane form, v2): Gap 2 key-value store over REST (server adapter, OpenAPI, tests, docs)

You implement the first serving slice of the VLite key-value (KV) feature: the four ordinary KV routes over HTTP. The
embedded Rust KV API already exists (`src/kv.rs`, `tests/kv.rs`). The exact contract is
`docs/projects/roadmap/serving-kv-ttl-contract.md`; read sections 1, 3, 4, 8, 10, 11 and 12 completely, and
`docs/projects/roadmap/serving-wire-conventions.md` for the shared rules they cite. The owner approved every recorded
choice in section 12 (a)-(p) provisionally on 2026-11-04 (blanket approval while the owner is away). An earlier worker
stopped on two scope problems; both are settled below.

## Rulings that settle the earlier worker's two open items (PROVISIONAL, authoritative)

- OPEN-K1-01 (authorization allowlist): `permission()` in `src/server.rs` is an explicit route allowlist whose default is
  `Need::Admin`. You MAY and MUST edit it, with the smallest hunk, to map `GET .../kv/entry` and `GET .../kv/entries` to
  the collection-read need and `PUT` and `DELETE .../kv/entry` to the collection-write need (contract section 10).
  Treat HEAD on the GET routes exactly as the existing record GET routes are treated (mirror the neighboring arms). More
  generally: any other minimal edit that the feature needs to function (the route allowlist, a router merge, an error arm,
  a test helper) is allowed; make it, and list it in your final report. Stop only for a decision the contract does not
  cover and no safe conservative default exists.
- OPEN-K1-02 (list page boundary): the KV contract (section 3) says a page holds the largest prefix of results that fits
  BOTH `limit` and the response cap, and that gRPC applies the same boundary computed from the canonical JSON form. The
  shared wire conventions say REST has no response cap, but a KV page that is shorter than `limit` because of its size
  is pagination, not an error, and without it one page can reach gigabytes (values are up to 1,047,290 bytes and `limit`
  is up to 10,000). Rule: REST and gRPC use the SAME boundary: the canonical JSON of the page, that is
  `{"items":[...],"next_after":...}` as the response body, never exceeds 16,777,216 bytes; the page holds the largest
  prefix that fits `limit` and that bound; `next_after` is the last returned key when another live matching entry existed
  at the page's clock, otherwise null; an empty page never carries a continuation; one entry always fits (the largest
  entry is about 1.4 MB). Document it in the OpenAPI text for `KvList` and in the REST docs.

## Lane rules (you are a local session in your own checkout)

- Create the branch `mahpate-gap2-kv-rest` from `origin/mahpate-pool-briefs-w7` (`git fetch origin
  mahpate-pool-briefs-w7`): it is the current integration head plus this brief. Never commit anything under
  `pool-briefs/`, and never mention or link `pool-briefs/` or any worker pool from any other file.
- Commit locally in logical commits with the trailer `Co-authored-by: Copilot App
  <223556219+Copilot@users.noreply.github.com>`: (1) wire goldens + failing tests, (2) adapter + error arms + permission
  arms, (3) OpenAPI + reference rows + docs, (4) mutant-driven test fixes if any. No push, no pull request, no tag.
- Cargo: your own `CARGO_TARGET_DIR`, `CARGO_BUILD_JOBS=3`, `CARGO_INCREMENTAL=0`, `--locked --offline`, one cargo command
  at a time, below-normal priority; the host CPU ceiling is 95%.
- Never derive an expectation from the code under test. Expected values come from the contract text, by hand.
- If the contract is silent or contradictory beyond the two rulings above, take the most conservative reading, label it
  PROVISIONAL in your report, and carry on; do not wait for the owner (the owner is away).

## Scope

In: the routes `GET /v1/collections/{collection}/kv/entry`, `PUT` and `DELETE` on the same path, and
`GET /v1/collections/{collection}/kv/entries` (contract section 4, operation ids `KvGet`, `KvPut`, `KvDelete`,
`KvList`), their bounds, validation, errors, authorization, tenancy, OpenAPI and documentation.

Out (other work owns it; do not touch): the three lease routes (no ledger exists), gRPC and `proto/**`, MCP, CLI, HTTP
client libraries, graph TTL and feeds, `src/server/admin.rs` (the `features.kv` flag and the `routes` array are a
follow-up the coordinator makes: the file belongs to another active session), `src/dashboard/**`, `bindings/**`,
`clients/**`, `src/bin/**`. The embedded `src/kv.rs` semantics do not change; if a visibility change is unavoidable,
make the smallest one and list it.

## Milestones (tests first)

1. Wire goldens. Create `tests/fixtures/kv/wire/` with hand-written JSON cases (request, expected status, expected
   body or error code) derived only from the contract: every success shape (all successes are HTTP 200), `D` decimal
   strings (no sign, no leading zero except "0", at most 18446744073709551615), `Expected` (`"any"` default,
   `"absent"`, `{"revision":"42"}`), `expires_at_ms` Deadline (string or null), already-due put, empty value, base64
   canonical padded encoding only, the 1,047,290-byte decoded value limit and its one-byte-over case, namespace
   1..255 and key 1..1024 UTF-8 byte bounds at each edge, list `prefix`, `after`, `limit` defaults and bounds
   (1..10000), the page rule of ruling OPEN-K1-02, DELETE query parameters (`expected=any|absent|<D>`), unknown or
   duplicate JSON fields (400 or 422 `invalid_json`), JSON integers where a `D` string is required, values that parse
   but break a bound (422 `invalid_input`), 404 `kv_not_found` (absent or due key), 412 `precondition_failed` (CAS
   mismatch), 409 `not_kv_collection`, 409 `kv_slots_exhausted`, 409 `upgrade_required` (TTL put on a pre-v5 store), 405
   for unlisted methods, 404 for unknown routes, 413 for an oversize body, and scoped-credential cases (read scope may
   get and list but not put or delete; write scope may put and delete; a credential without a binding for the
   collection is refused; tenant-local collection names; authorize before any lookup).
2. Failing tests. Write `tests/server_kv.rs` (HTTP black-box tests using the in-process test server pattern of the other
   `tests/server_*.rs` files) that load those goldens, plus unit tests for validation helpers. Run them on the unmodified
   code and record that they FAIL there.
3. Adapter. New module `src/server/kv.rs` (registered in `src/server.rs`); the `permission()` arms of ruling
   OPEN-K1-01; error arms for `InvalidKvKey` (422 `invalid_input`), `KvSlotsExhausted` (409 `kv_slots_exhausted`) and
   `NotKvCollection` (409 `not_kv_collection`) in the `From<Error> for ApiError` mapping (an unmatched engine error
   would become a 500 today) with a regression test in `tests/server_ops.rs`; credential scopes exactly as section 10;
   audit and quota hooks the same way the existing record routes use them; ordinary KV follows the configured durability
   (KV-002); a read-only or replica server refuses mutations with the existing error.
4. OpenAPI and reference surface. KV paths, components and error rows in `src/openapi.json` per section 8, with the
   qualifier on the "Successful writes are synced." sentence (ledger row X2). `site/api.html` rows and `data-api` keys
   (ledger row X6) for the REST operations only. `python tests/api_reference.py` and `python tests/api_route_inventory.py`
   must pass; extend the extractor only if a REST family needs it, minimally.
5. Documentation. `docs/API.md` (a KV section), `docs/STORAGE-FEATURES.md` (replace "REST, gRPC and bindings come after
   the OpenAPI review" with what now ships: REST yes; gRPC, MCP, CLI, clients and leases not yet), `docs/SECURITY.md`
   (KV scopes, tenancy, dedicated-collection advice from section 10). Plain language, no performance claims.
6. Hand mutants (at least 12). Each must fail at least one named test, be restored, and be hash-checked: an off-by-one on
   each bound, `read` and `write` swapped in `permission()`, `Expected` absent/revision semantics reversed, the page
   boundary and `next_after` rule, accepting non-canonical base64, accepting a duplicate field, a missing key mapped to
   200, skipping the authorize step, losing the 412 mapping, 500 for `NotKvCollection`. Report any survivor.

## Gates (all must pass; report exact commands and results)

```powershell
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --all-features --test server_kv --test server_ops
cargo test --locked --all-features --lib server::
python tests/api_reference.py
python tests/api_route_inventory.py
python tests/release_docs.py
python tests/content_budget.py
python tests/agent_docs.py
python tests/ci_workflows.py
```

Also `cargo test --locked --all-features --lib` and the embedded `kv` integration test before the final report.

## Final report (to the coordinator, at most 400 words)

The commit SHAs in order, what changed where, the base-failure evidence, gate results, the mutants by name with the
killing test, every PROVISIONAL choice, every extra edit beyond the listed scope, and the follow-ups (admin.rs
`features.kv`, gRPC, MCP/CLI proxy forwarding, clients, leases).
