---
goal: VLite MemoryAI live grants — restart-free updates to the grants of memory bindings the server already serves
version: 1.2
date_created: 2026-11-08
last_updated: 2026-11-08
owner: VLite maintainers (mahpate/Vlite)
tags: [feature, memory, security, authorization, operations]
---

# Introduction

VLite agent memory (MemoryAI) reads a private bindings file once, at startup (`src/server/admin.rs:255-256`). Each binding declares its graph, an optional evidence collection, a receipt window and its grants. A grant maps a credential ID (`principal`) to exact `scopes`, optional `read_scopes` and an optional `raw_access` flag.

Today every grant change needs a restart:
- adding or removing a credential;
- adding, removing or moving a scope or a read scope;
- turning `raw_access` on or off.

The operator raises `generation` and restarts (`docs/MEMORY.md:93`, `docs/API.md:380-382`). A restart drops every connection and every in-flight request, reruns startup over every binding, and turns a revocation into a deployment step.

This PRD plans one capability. An admin-only request rereads the same configured bindings file and applies a **grants-only** change to the bindings that the server already serves, without a restart. The authorization semantics are explicit at request level. Everything else about bindings stays restart-only.

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in RFC 2119.

**Cross-reference conventions**: This document uses standardized prefixes for traceability — `FR-` (functional requirements), `NFR-` (non-functional requirements), `FM-` (failure modes), `AC-` (acceptance criteria), and `RD-` (resolved decisions). When a `.req.md` exists, identifiers from that document are referenced directly.

No `.req.md` exists, so this PRD has no `FR-` or `NFR-` items. Its requirements use the Section 4 prefixes `REQ-`, `SEC-`, `CON-`, `GUD-` and `PAT-`.

**Evidence base**
- **Code base.** Analysis used the local worktree at `HEAD` `4cdb2ce311ab94acad35df393888291df3ee7af3`, with crate `vlite` 0.7.2, edition 2024 and MSRV 1.89. Every `file:line` reference below is at that commit.
  - This head contains the completed `read_scopes` grants and the bounded reclamation of expired receipts (`docs/projects/feature-memory-hardening/`).
  - It was not yet pushed when this PRD was written, so the remote code-search index and `main` were deliberately not used.
- **Owner direction.**
  - The owner said "please go ahead with restart free grant updates" and chose a focused PRD, limited to existing bindings, before implementation.
  - The owner later delegated routine choices ("Take your best judgement…") and named 0.7.3 as a *candidate* release once this capability is done. This PRD grants no release or publication authority.
- **Baseline test run at that commit.** `cargo nextest run --locked --features server --lib -E 'test(memory::) | test(reserved::) | test(guard::) | test(published::)'` ran 518 tests: 518 passed, 1,069 skipped.
- **Baseline mutation-ledger anchors at that commit.** `python tests/mutant_replay.py plan --root . --ledger <ledger> --shards 1` reports `"stale":0` for every replayable ledger:
  - `tests/memory_followup_mutants.json` (257);
  - `tests/k71_mutants.json` (206);
  - `tests/f3_mutants.json` (66);
  - `tests/f2_mutants.json` (72).

  The files changed here hold anchors from `memory_followup`, `f3`, `k71` and `f2`, listed in CON-004.

  `tests/reserved_resource_mutants.json` is a campaign record, and the replay tool refuses it by design (`tests/mutant_replay_selftest.py:2378`). A direct count at that commit shows that each of its 59 anchored entries (`G71-*` `old` texts) occurs exactly once. 23 of them are in `src/server/guard.rs`.
- **Parked contract.** The parked F4 contract (`docs/projects/roadmap/f4-self-serve-memory-contract.md` on branch `mahpate-f4-memory-self-serve`) was read for context only. It is not approved scope, and this PRD does not ratify any part of it.

## 1. Goals and Non-Goals

- **G1:** An admin changes the grants of a binding the server serves without restarting. That covers a grant's principal (credential ID), its exact `scopes`, its `read_scopes` and its `raw_access` flag. The workflow is to edit the configured file (with a higher `generation`, as today) and send one admin request.
- **G2:** Authorization never runs ahead of durability, and no request mixes versions.
  - Every request authorizes against exactly one grants version.
  - Once the change is durable, a memory write admitted under superseded grants fails closed.
  - The response states whether any request under older grants may still be running.
- **G3:** A reload is accepted only when a restart with the same file would pass the same checks and the change is grants-only.
  - After a successful reload, the file, the stored records and the served grants agree, so the next restart adds no commit.
  - Startup, the file format, the generation rules, every existing route's wire format and every gate stay unchanged.

### In Scope
- **EPIC-001: live grants cell and request pin.** A live grants cell (`reserved::Served`) replaces the fixed `Arc<reserved::Bindings>`, with a request-lifetime pin and a bounded drain. These are the smallest changes that make an in-process grants change safe.
- **EPIC-002: reload route and apply path.** `POST /v1/admin/memory/bindings/reload` and its apply path reuse the startup checks (`plan`, `check_binding`, `write_binding_record`, `provision_scope`).
- **EPIC-003: contract surfaces that CI requires for a new admin route.** None of these gates is lowered:
  - the OpenAPI entry, plus a reviewed REG-008 approved addition;
  - the `site/api.html` row;
  - the capabilities flag;
  - the admin-route inventory, with one minimal dashboard button.
- **EPIC-004: tests.** Unit tests, in-crate REST pins, an integration test file and one dashboard end-to-end step.
- **EPIC-005: documentation.** `docs/API.md`, `docs/MEMORY.md`, `docs/SECURITY.md` and `docs/CONSOLE-ADMIN.md`.

### Out of Scope (deferred)
- **Binding lifecycle changes** stay restart-only (RD-003). These include:
  - adding, removing or renaming a binding;
  - changing its tenant, graph, evidence collection, receipt window or state (retiring included);
  - first-time memory enable, meaning a server started with no configured binding.
- **Self-service and new models.** A self-service token or credential model, any memory-management UI beyond the one button the admin-route gate requires (RD-012), and the F4 contract.
- **Other work.** Git integration, cleanup or erasure controls, E1 reclaim, and erasing data that a revoked scope already holds (`forget` and erasure stay separate).
- **gRPC live reload.** With bindings configured, the shipped server refuses gRPC; library embedders that built a gRPC router get 409 (RD-009).
- **Other triggers.** A file watcher, signals, or grants carried in a request body (RD-001).
- **Keeping several REST routers of one `Service` in sync.** This affects library embedders only; each router reloads its own grants (RD-010).
- **Release, deployment and gates.** Release notes, a version bump to 0.7.3, any release or deployment, edits under `deploy/`, and lowering any gate.

## 2. Terminology

| Term | Definition |
|---|---|
| Bindings file | The private `memory-bindings-v1` JSON file named by `--memory-bindings-file` or `VLITE_MEMORY_BINDINGS_FILE` (`ServerOptions.memory_bindings_file`, `src/server/admin.rs:120`). |
| Served view | The `reserved::Bindings` value (binding set plus reservations) that authorizes requests now. |
| Live cell | The new `reserved::Served`. It holds the served view, the source path, the retired epochs and the reload lock. One live cell is shared by every clone of the `Service` a REST router was built with. |
| Epoch | A private marker allocated with each served view. A request that consulted grants holds its view's epoch until the request and every worker it started end. |
| Pin | A served view and its epoch, captured lazily by a request the first time it consults grants. Every later lookup in that request reuses it. |
| Drain | Bounded waiting until no epoch older than the current one is held. |
| Grants-only delta | A new file whose bindings have the same (tenant, binding) keys as the served view, with every field except `grants` and `generation` unchanged. Order may differ. |
| Record | The binding record node (`binding:v1`) that startup writes into each bound graph (`src/memory/startup.rs:219-249`). |
| Superseded grants | The grants of a served view that a reload has replaced. |
| In-flight request | A request whose handler, or a blocking or group-commit worker it started, is still running. |

## 3. Solution Architecture

### 3.1 Current state (evidence)

- **Where grants live.** They live in `Service.memory_bindings: Arc<reserved::Bindings>` (`src/server.rs:219`). `configured_admin_router` sets the field (`src/server/admin.rs:255-256`, then `285-286` with orphans) and it never changes after that. The field is published to gRPC by `publish_with` (`src/server.rs:404-449`) and adopted by `adopt_published` (`src/server.rs:459-480`).
- **Exactly two code paths read grants to authorize a request.**
  1. `guard::RequestScope::new` (`src/server/guard.rs:84-90`, clone at line 87). Every raw collection, graph, KV and feed route reaches resources only through `service.scope(role)` (`src/server.rs:483-485`). Its raw-access checks call `Reserved::raw_allowed` (`src/server/reserved.rs:80-90`). No production code calls `memory_bindings.reserved()` directly.
  2. Memory `resolve` (`src/server/memory/routes.rs:225-288`, line 239). It reads `service.memory_bindings.resolve(..)` before any database work and copies the caller's grant into an owned `Bound`.
- **Every other reader is grant-independent.** The remaining uses read `is_empty()` or `is_configured()`, at:
  - `graph.rs:260`;
  - `collections.rs` ×7;
  - `features.rs:801`;
  - `webhooks.rs:269/295`;
  - `observability.rs:190/226`;
  - `ops.rs:906/930/1026`;
  - `memory.rs:99/174`;
  - `published.rs`, in tests only.

  Two uses iterate identities only: `tenants.rs:289` and `memory.rs:85` (tests). A grants-only delta cannot change any of these results.
- **Admin-role decisions depend on reservation keys only.** `raw_allowed(None, ..)` is `false` for every reserved resource (`src/server/reserved.rs:86-89`). `attachment_allowed` compares owners (`:136-140`). So the webhook dispatcher's long-lived admin scope (`src/server/webhooks.rs:400`) makes the same decisions under any grants-only delta.
- **Memory writes already fail closed on a stale generation.** `Bound::recheck` (`src/server/memory/routes.rs:301-305`) runs under the database write lock and compares the persisted record generation through `check_binding` (`src/memory/startup.rs:189-204`). On a mismatch it answers 409 with `binding_generation` and writes nothing. Reads have no recheck.
- **A per-request context already exists.** `observe` runs every REST handler inside `guard::track_hidden` (`src/server.rs:1414`), after authentication, rate limiting and authorization. The task-local (`src/server/guard.rs:40-68`) is carried into blocking and group-commit workers by:
  - `run_mutation` (`src/server.rs:515-536`);
  - `run` (`:538-567`);
  - `run_read_then` (`:584-608`).

  A worker keeps the context until its closure ends, even after the client disconnects (`src/server.rs:553-554`). REST responses are not streamed. All middleware layers sit at `src/server.rs:974-976`, and none of them builds a `RequestScope`.
- **Startup order.** `prepare` (`src/server/memory.rs:108-162`) runs these steps:
  1. resource checks;
  2. `plan` (read-only, `src/memory/startup.rs:79-114`);
  3. return on a read-only database;
  4. write records, one commit per changed binding;
  5. provision every active binding's scopes, one commit per new scope (`src/memory.rs:336-365`).

  Every error becomes a fixed string, which erases the difference between `CommitIndeterminate` and other failures.
- **Generation rules.** `validate_against` (`src/memory/binding.rs:246-311`) applies them:
  - a change to grants or state needs a higher generation;
  - an unchanged entry keeps its generation;
  - identity, graph, evidence and window never change.
- **Reload precedents.** `POST /v1/admin/tokens/reload` (`src/server/admin.rs:652-692`) rereads only configured files, validates them before swapping, and answers 409 or 422. Its OpenAPI text says "In-flight requests may finish." `POST /v1/admin/tls/reload` takes no body.
- **Unmapped routes are admin-only.** The permission fallback is `_ => Need::Admin` (`src/server.rs:1185`). Read-only callers are barred from `/v1/admin/` (`authorize`, `:1206`), and scoped callers receive `forbidden_scope` (`authorize_need`, `:1244`).

### 3.2 Target design

Three components change:

1. **Live cell (`src/server/reserved.rs`).**
   - `Served { source: Option<PathBuf>, current: RwLock<(Arc<Bindings>, Arc<Epoch>)>, retired: Mutex<Vec<Weak<Epoch>>>, reloading: Arc<tokio::sync::Mutex<()>> }`.
   - `Service.memory_bindings` becomes `Arc<Served>`, and `published_bindings` becomes `Option<Arc<Served>>`, so every clone and the gRPC adoption share one cell.
   - Methods that do not depend on grants delegate to the current view: `is_empty`, `is_configured` and `covers`.
   - `current()` returns an unpinned snapshot. `pin()`, `install(..)` and `drain(timeout)` are added.
2. **Request pin (`src/server/guard.rs`).**
   - The task-local value changes from `Arc<AtomicBool>` to `Arc<RequestContext { hidden: AtomicBool, pinned: OnceLock<Pin> }>`. The names `track_hidden`, `current` and `within` stay.
   - A new function `guard::bindings(&Served) -> Arc<Bindings>` pins on first use inside a request and otherwise returns `current()`.
   - `RequestScope::new` and memory `resolve` are the only two call sites changed. No other handler changes.
3. **Reload apply path.**
   - A new file, `src/server/memory/reload.rs`, holds the handler, the grants-only delta check, and its own provisioning and record loops. It reuses `plan`, `resources`, `refusal_message` and the fixed messages; `prepare` is unchanged (PAT-001).
   - The route is registered in `src/server/admin.rs` `routes()`, next to `tokens/reload`.

```mermaid
sequenceDiagram
    participant Op as Admin
    participant H as reload handler
    participant DB as Database (write lock)
    participant C as Served cell
    participant R as In-flight requests
    Op->>H: POST /v1/admin/memory/bindings/reload
    H->>C: try_lock_owned(reloading) else 409 memory_reload_in_progress
    H->>H: read + parse configured file (spawn_blocking) else 422
    H->>DB: service.run(closure) else 503 busy/unavailable
    Note over DB: exclusive write lock held from here
    DB->>DB: lock published_bindings; grpc_built? -> 409
    DB->>C: current() ; grants-only delta? else 409 memory_restart_required
    DB->>DB: resource checks + plan(db, set, now) else 409 memory_bindings_conflict
    DB->>DB: new view = Bindings::new(set).with_orphans(plan.orphans); keys equal both ways? else 409
    DB->>DB: if writable: provision every active binding's scopes (idempotent, as startup)
    DB->>DB: if writable: write records of plan.writes (old-view writes now get 409)
    DB->>C: if changed: install(new view): swap, retire old epoch
    Note over DB: write lock released
    R-->>C: requests that pinned the old epoch finish under old grants
    H->>C: drain(10 s): wait until no retired epoch is held
    H-->>Op: 200 {"bindings":N,"changed":K,"drained":true|false}
```

### 3.3 Interface contract

**Route.** `POST /v1/admin/memory/bindings/reload`. It is admin only through the existing `Need::Admin` fallback.

**Request.** The handler has no body extractor, so the body is ignored, as for `tls/reload`. The dashboard sends `{}`, as `tokens/reload` does. No path, grant or binding name is accepted from the request. The server rereads exactly `ServerOptions.memory_bindings_file` as configured.

**Success (200).**

```json
{"bindings": 3, "changed": 1, "drained": true}
```

- `bindings` is the number of bindings in the file.
- `changed` is the number of bindings whose entry differs from the previously served view. It is 0 for an unchanged file, which is rechecked but not installed (RD-011).
- `drained` is `true` when, at return time, no request that consulted older grants is still running. It is `false` when `DRAIN_TIMEOUT` (10 s) elapsed first. The change is then applied and durable; send the request again to wait again.
- The response never contains a binding name, tenant, principal or scope.

**Errors.** Every message is fixed and never names a binding.

| Condition | Status / `error.code` | Message |
|---|---|---|
| Not configured: no source file, or no binding served | 409 `memory_reload_unavailable` | "memory bindings reload is unavailable; it needs a configured bindings file that names a binding" |
| A gRPC router was built from this service (`grpc_built`) | 409 `memory_reload_unavailable` | "memory bindings reload is unavailable after a gRPC router was built; restart instead" |
| Another reload holds the reload lock | 409 `memory_reload_in_progress` | "a memory bindings reload is already running" |
| File unavailable, not a regular file, or invalid (strict parse, caps) | 422 `invalid_input` | the startup messages of `read_memory_bindings` (`src/server/admin.rs:390-410`) |
| The delta is not grants-only, or reservations would differ | 409 `memory_restart_required` | "a memory bindings reload may change only grants and generations; restart to apply other changes" |
| Missing graph or evidence, plan refusal (`Rotation`, `Records`, `Unreadable`, `InUse`), provisioning or record refusal | 409 `memory_bindings_conflict` | the existing fixed startup message (`src/server/memory.rs:18-58`, via `refusal_message` / `rotation_message`) |
| Storage error with a 5xx mapping | that mapping, e.g. 503 `commit_indeterminate`, 503 `poisoned` | `ApiError::from(Error)` (`src/server.rs:1755+`) |
| Database queue full / closed | 503 `busy` / 503 `unavailable` | from `Service::run` (`src/server.rs:538-567`) |
| Read-only token / scoped credential | 403 | existing `authorize` / `authorize_need` |

**Capabilities.**
- `features.memory_bindings_reload` is `true` only when all three hold:
  - the live cell has a source;
  - the current view `is_configured()`;
  - no gRPC router was built.
- `routes` gains `/v1/admin/memory/bindings/reload`.

### 3.4 Apply algorithm

The handler (`src/server/memory/reload.rs`) runs these steps in order:

1. **Availability.** Return 409 `memory_reload_unavailable` when the cell has no source or the current view is not configured.
2. **Serialization.** `served.reloading.clone().try_lock_owned()`, otherwise 409 `memory_reload_in_progress`. The owned guard moves into the database closure and comes back with its result, so it is held while the apply runs even if the client disconnects. After that it is held through the drain and the report, and released last, so audit entries follow reload order.
3. **Read.** `read_memory_bindings(source)` runs inside `spawn_blocking`, before any database lock. An error gives 422 with the existing fixed message.
4. **Apply.** After the file is read, an owned task runs steps 4 to 6. The handler awaits that task, but dropping the handler does not cancel it. The apply phase runs in one `service.run` closure, so the exclusive database write lock is held for all of it:
   1. Lock `published_bindings`, and keep it until step 4.8. Lock order is always database write lock → `published_bindings` → cell lock. If `grpc_built` is set, return 409 `memory_reload_unavailable`. `adopt_published` takes the same lock (`src/server.rs:462-468`), so a gRPC router cannot adopt the cell between this check and the install.
   2. Read `old = served.current()`. Check the grants-only delta of the new set against `old.set()`; otherwise return 409 `memory_restart_required`. This check runs under the lock, so it always compares against the view in force. Set `changed` to the number of bindings whose entry differs from `old`.
   3. Run the resource checks of `prepare` (graph-enabled database, graph exists, evidence exists). These two existence lookups take names from the operator's file, never the request, so `tests/server_name_lookup_scan.rs` allows them in `reload.rs::apply` with a live-server justification. Then run `plan(db, set, now_ms)`. Map refusals to 409 `memory_bindings_conflict` with the startup message.
   4. Build `new = Bindings::new(set).with_orphans(&plan.orphans)`. If `!(new.covers(&old) && old.covers(&new))`, return 409 `memory_restart_required`. This check is defensive: with equal keys and unchanged graphs it holds unless an orphan record appeared or vanished.
   5. If the database is writable, provision every scope and read scope of every **active** binding in the new set, exactly as startup does. Provisioning is idempotent: an existing scope costs one lookup and no commit. On error, return the storage error's 5xx mapping (the `core_error` rule, `src/server/memory/routes.rs:355-366`), or otherwise 409 `memory_bindings_conflict` with `PROVISION_FAILED`.
   6. If the database is writable, write the record of each binding in `plan.writes` (`write_binding_record`, one commit each). From the first committed record on, a memory write admitted under the old view and reaching the lock gets 409 `binding_generation`. On error, use the step 4.5 mapping with `RECORD_FAILED`.
   7. If `changed > 0`, install `new`: swap the current view and push a `Weak` of the old epoch onto `retired`. If the set is identical, install nothing. The file was still checked against the records, so a reverted or diverged file is refused exactly as at a restart.
   8. Release the `published_bindings` lock.
5. **Drain.** Release the database lock (the closure returns). Then await `served.drain(DRAIN_TIMEOUT)`, which polls every 10 ms with `tokio::time::sleep` (precedent: `src/server/providers.rs:1032`). It prunes dead `Weak`s and returns `true` once none is alive.
6. **Report.** On success, write a structured stderr line, `{"event":"memory_bindings_reloaded","bindings":N,"changed":K,"drained":b}`, with no names. When the persistent audit log is configured, append the same event plus `outcome:"ok"` (best effort, `sync=false`; on failure, increment `audit_failures`, as at `src/server/memory/routes.rs:180-184`). Reporting still runs if the client disconnected during apply or drain. Refused or failed reloads do not emit this success event. Return 200 when the caller is still waiting.

On a **read-only database or replica**, steps 4.5 and 4.6 are skipped and step 4.7 still installs. Startup behaves the same way: it runs every check, writes nothing, and serves the file's grants, while memory writes keep answering 403 `read_only` (`docs/API.md:407-409`).

### 3.5 Request-lifetime barrier: guarantees and limits

- **One version per request.** The first `guard::bindings` call in a request stores the pin in the request context's `OnceLock`. Every `RequestScope` and every memory `resolve` in that request reuses it, including those in blocking and group-commit workers reached through `within`. The memory route and the raw guard therefore always agree inside one request. One example is a `remember` that names an evidence record under `raw_access`.
- **Lazy pinning.**
  - A request that never consults grants holds no epoch, so drains do not wait for it.
  - The reload request never pins, so it cannot block its own drain (REQ-016).
  - A request that first consults grants after the install uses the new grants.
- **What `drained: true` means.** It is checked at return time. Every request that consulted grants before the install has finished, including its workers. From then on, no request runs under superseded grants.
- **Before the drain completes, in-flight requests may finish under superseded grants.** This matches the existing token-reload contract ("In-flight requests may finish").
  - Reads and raw writes admitted under the old view may complete.
  - A memory write admitted under the old view fails closed with 409 `binding_generation` if it reaches the write lock after its binding's record was written.
- **Emergency revocation.** It stays `POST /v1/admin/credentials/{id}/revoke`. It is immediate for new requests and independent of this feature.
- **Outside a request context** (the webhook dispatcher, startup, direct test calls), `current()` is used without a pin.
  - The dispatcher uses the Admin role, whose decisions do not depend on grants (§3.1, TEST-002).
  - gRPC also has a guard context, but live-reload behavior over gRPC is outside this feature's supported and qualified scope; step 4.1 excludes it.
- **Why a plain swap is not enough.** An `Arc` swap alone would let one request authorize with two versions (for example, a list filtered under old `raw_access` followed by a read under new grants). It would let served grants run ahead of the durable records, and it could not say when superseded grants stop being used. The pin, the record-first install under the lock and the drain each close one of these gaps.

### 3.6 Persistence and restart contract

- **Source of truth.** The bindings file stays the only source of truth. The reload never writes the file.
- **State after a successful reload.**
  - On a writable database, file == records == served view. The next restart with the same file writes no record (`RecordWrite::Unchanged`) and provisions no new scope, so it adds no commit.
  - On a read-only database, file == served view, and the records advance only through the primary.
- **An edit that is never reloaded** applies at the next restart, as today.
- **Restarting with an older file after a reload** is refused by the generation rules, as it is today after a restart. The remedy is unchanged: reissue the intended grants with a generation above the record.
- **A failed reload is not a revocation** (FM-009). The operator retries, restarts, or revokes the credential.
- **Parity.**
  - A reload rejects every file that startup's resource checks, `plan` or provisioning would reject, including an identical file whose records diverged.
  - It provisions every active binding, as startup does (RD-006).
  - An accepted reload leaves the same records, scopes and served grants that a restart with the same file would.

### 3.7 Failure modes

| ID | Trigger | Response | Served view | Records | Operator remedy |
|---|---|---|---|---|---|
| FM-001 | File missing, not regular, over 1 MiB, invalid or half-written | 422 `invalid_input` | unchanged | unchanged | Fix the file (write it atomically), retry |
| FM-002 | Delta adds, removes or renames a binding, or changes tenant, graph, evidence, window or state | 409 `memory_restart_required` | unchanged | unchanged | Restart with the file |
| FM-003 | Grants changed at the same generation (stale file) | 409 `memory_bindings_conflict` ("…without a higher generation") | unchanged | unchanged | Raise `generation`, retry |
| FM-004 | Reverted or older file: generation below the record | 409 `memory_bindings_conflict` (rotation message) | unchanged | unchanged | Reissue intended grants above the record's generation |
| FM-005 | Generation raised with no grants change | 409 `memory_bindings_conflict` | unchanged | unchanged | Restore the generation |
| FM-006 | Graph or evidence missing, resource in use, records unreadable or corrupt | 409 `memory_bindings_conflict` (startup message) | unchanged | unchanged | As for the same startup refusal |
| FM-007 | Provisioning refused for any active granted scope (`DeletionPending` under an erasure barrier, an orphan erasure job, digest collision); restart would refuse the same file | 409 `memory_bindings_conflict` (`PROVISION_FAILED`) | unchanged | unchanged; scopes provisioned so far remain (empty, harmless, as at startup) | Wait for the erasure, or drop the scope from the file, then retry |
| FM-008 | Storage failure while provisioning or writing records | 503 `commit_indeterminate` or 503 `poisoned` | unchanged | earlier commits of this reload may be durable | Reopen or restart the database, then retry or restart with the same file |
| FM-009 | Record write fails after one or more records committed | 409 or 503 per FM-007/FM-008 | **unchanged (old grants)** | partially advanced | Retry with the same file (converges: written records are `Unchanged`) or restart with it. Until then, memory writes on advanced bindings get 409 `binding_generation`, and **reads and raw access on them keep the old grants**. Revoke the credential if revocation is urgent |
| FM-010 | Read-only database or replica | 200 | new | unchanged (primary-owned) | None |
| FM-011 | A second reload while one runs | 409 `memory_reload_in_progress` | — | — | Retry after the first returns |
| FM-012 | Database queue full or closed | 503 `busy` / `unavailable` | unchanged | unchanged | Retry |
| FM-013 | Requests under older grants outlive `DRAIN_TIMEOUT` | 200 `drained:false` | new | new | Send the request again (an unchanged file is rechecked, installs nothing and waits again), or revoke the credential |
| FM-014 | A gRPC router was built from the service | 409 `memory_reload_unavailable` | unchanged | unchanged | Restart |
| FM-015 | No configured file, or no binding served | 409 `memory_reload_unavailable` | unchanged | unchanged | First-time enable is restart-only |
| FM-016 | Request pinned before the install still running | its own result under the old grants; a memory write after the record write gets 409 `binding_generation` | — | — | Covered by the drain |
| FM-017 | Client disconnects after the reload's owned task starts | — | apply, bounded drain and success reporting continue; the reload lock stays held through the drain | installed on success | Send the request again after the running reload: an unchanged-file recheck that waits for the drain, or a convergent retry; a concurrent retry still answers 409 |
| FM-018 | Revoked scope still holds memory | — | — | data kept | Use `forget` or erasure (out of scope) |

## 4. Requirements

- **REQ-001**: The server MUST register `POST /v1/admin/memory/bindings/reload` in `src/server/admin.rs` `routes()` on every configured REST router. With no binding configured it answers 409 `memory_reload_unavailable`. The handler lives in `src/server/memory/reload.rs`.
- **REQ-002**: The handler MUST NOT read a path, grant or binding name from the request. It MUST reread exactly the configured `memory_bindings_file` through the same reader as startup (`read_memory_bindings`), with the same three fixed messages, under 422 `invalid_input`.
- **REQ-003**: The handler MUST answer 409 `memory_reload_unavailable` when the live cell has no source, when the current view is not configured, or when `grpc_built` is set. The `grpc_built` check MUST happen while the `published_bindings` lock is held, and the lock MUST be held until the install.
- **REQ-004**: At most one reload MUST run per live cell. A concurrent request MUST receive 409 `memory_reload_in_progress`. Once the owned task starts, it MUST keep the lock through apply and drain and finish success reporting even if the client disconnects.
- **REQ-005**: A reload MUST be refused with 409 `memory_restart_required` and change nothing unless both conditions hold:
  - the new set has exactly the served set's (tenant, binding) keys;
  - every binding's `state` (including `retired_at_ms`), `graph`, `evidence_collection` and `receipt_window_ms` are equal.
  
  Only `grants` and `generation` may differ, and binding order MAY differ. The check MUST compare against the view current under the database write lock.
- **REQ-006**: Before any write, a reload MUST run the `prepare` resource checks and `plan(db, set, now_ms)`. Refusals MUST map to 409 `memory_bindings_conflict` with the existing fixed startup messages. The generation rules MUST NOT be changed or bypassed.
- **REQ-007**: The new view MUST be `Bindings::new(set).with_orphans(&plan.orphans)`, and its reservation keys MUST equal the old view's in both directions (`covers` both ways). Otherwise the server MUST answer 409 `memory_restart_required`.
- **REQ-008**: On a writable database, a reload MUST idempotently provision the scopes and read scopes of every active binding in the new set (startup parity) **before** writing any record. It MUST write the records of `plan.writes` before installing. It MUST install while still holding the exclusive database write lock. It MUST NOT reuse `prepare()` itself, because `prepare` writes records before provisioning and erases the storage error distinction.
- **REQ-009**: On a read-only database, a reload MUST run every check, write nothing, and install the new view.
- **REQ-010**: A file whose set equals the served set MUST still run the checks of REQ-006 and REQ-008. It MUST NOT install, MUST answer `changed: 0` when accepted, and MUST still await the drain.
- **REQ-011**: A successful reload MUST answer 200 `{"bindings":N,"changed":K,"drained":bool}` and nothing else.
- **REQ-012**: A storage error MUST surface as its existing `ApiError` mapping when that mapping is a 5xx (notably 503 `commit_indeterminate` and 503 `poisoned`), so that `Service::run` counts `commit_indeterminate` in `commit_failures` as for any other route. Every other provisioning or record failure MUST map to 409 `memory_bindings_conflict` with the fixed startup message.
- **REQ-013**: `Service.memory_bindings` MUST become `Arc<reserved::Served>`, and `published_bindings` MUST hold `Option<Arc<reserved::Served>>`.
  - `Served` MUST provide `is_empty`, `is_configured` and `covers` with today's meaning, applied to the current view.
  - `current()` and `pin()` MUST copy out under a leaf lock that is never held across another lock or call. `covers` MUST snapshot both views before comparing, because both sides may be the same cell (`publish_with` compares the published cell with its own).
  - `install` MUST be reachable only from the reload path.
- **REQ-014**: The request task-local MUST carry a lazily set pin. `guard::bindings(&Served)` MUST return the pinned view inside a request, setting the pin on first use, and `current()` outside one. `RequestScope::new` and memory `resolve` MUST use `guard::bindings`. No other production code may read grants from the cell.
- **REQ-015**: `drain(timeout)` MUST return `true` only when no retired epoch has a live strong reference, and `false` after `timeout` otherwise. `DRAIN_TIMEOUT` MUST be 10 seconds, and tests MUST be able to pass a shorter timeout.
- **REQ-016**: The reload handler MUST NOT build a `RequestScope` or resolve a binding, so its own request never pins.
- **REQ-017**: `configured_admin_router` MUST record `options.memory_bindings_file` as the cell's source for the final served view.
  - The source is set after the orphan block (`src/server/admin.rs:270-287`) and before the second `refuse_reserving_after_grpc` (f3 P16).
  - It is never set inside `with_bindings`, which builds a new cell with a `None` source for every caller and keeps anchor S2-B117 (`service = service.with_bindings(reserved);`) unchanged.
  - `Served` MAY provide a delegating `with_orphans`, so that `src/server/admin.rs:285` keeps its text.
- **REQ-018**: Capabilities MUST add `features.memory_bindings_reload` (REQ-003 conditions) and list the route in `routes`.
- **REQ-019**: Startup behaviour MUST NOT change. That covers the `prepare` order and messages, `refusal_message`, `rotation_message`, the file format, the 1 MiB cap, and the gRPC and auto-maintenance refusals.
- **SEC-001**: The route MUST be admin-only. Read-only tokens and scoped credentials MUST receive 403, through the existing `Need::Admin` fallback with no new permission arm.
- **SEC-002**: No response, stderr line or audit event of a reload MUST contain a binding name, tenant, principal, scope or file path.
- **SEC-003**: Within one request, every grant-dependent decision MUST use one served view (§3.5).
- **SEC-004**: A memory write admitted under superseded grants MUST NOT commit after its binding's record advanced. The existing `Bound::recheck` under the write lock provides this and MUST be kept.
- **SEC-005**: Served grants MUST NOT run ahead of durable records on a writable database: install only after every record of the reload has committed.
- **SEC-006**: Documentation MUST state the in-flight contract (§3.5), the meaning of `drained:false`, FM-009, and that credential revoke remains the immediate emergency control.
- **CON-001**: The file format, generation rules and binding record format MUST stay unchanged, with no new field or version.
- **CON-002**: The change MUST add no crate dependency and no `pub` item. `scripts/api_surface.py check` MUST pass unchanged.
- **CON-003**: The change MUST lower no gate or threshold. Every new surface goes through the existing reviewed mechanisms: the REG-008 approved additions, the admin-route inventory and the API reference row.
- **CON-004**: Every mutation-ledger anchor (`old` text) in a touched file MUST still occur exactly once, and `mutant_replay.py plan` MUST report `"stale":0` for `memory_followup`, `k71`, `f3` and `f2`, the same as the baseline. The anchors at risk are:
  - `src/server/memory.rs`: S2-B90–B102, so `prepare`, `refusal_message` and `routes` stay byte-identical.
  - `src/server/reserved.rs`: S2-B110–B113 and f3 P27, so `Bindings::is_empty` and `is_configured` stay next to `set`.
  - `src/server/admin.rs`: S2-B114–B117 (including `        service = service.with_bindings(reserved);`), f3 P8, P15, P16 and P20, and f2 A01.
  - `src/server/memory/routes.rs`: S2-C01–C15, S6-A01–A22 and k71 MA1–MA6 (including the `Bound` field initializers in `resolve`).
  - `src/server.rs`: f3 P1–P31 (including `self.memory_bindings = bindings.clone();`, `*bindings = Some(self.memory_bindings.clone());` and both `covers` comparisons), plus the k71 and f2 anchors.
  - `tests/reserved_resource_mutants.json` (campaign record, refused by the replay tool, so checked by count): every `G71-*` `old` text MUST still occur exactly once. These include:
    - `src/server/guard.rs` ×23, notably G71-19 `(output, flag.load(Ordering::Relaxed))` in `track_hidden`: the local `flag` stays the request's `AtomicBool`;
    - `src/server/admin.rs` G71-21 `    use std::io::Read;` in `read_memory_bindings`;
    - `src/server/reserved.rs` G71-15, 16 and 51;
    - `src/server.rs` G71-17, 18, 28, 43 and 52;
    - `src/server/tenants.rs` G71-20.
- **CON-005**: The change MUST include no version bump, release notes, release, deployment or `deploy/` change.
- **GUD-001**: Mirror `tokens/reload`: configured file only, validate before swapping, fixed messages, 409 for configuration state, 422 for file content.
- **GUD-002**: Build a `RequestScope` in the handler task or inside the `run*` runners. Never build one in a detached `tokio::spawn` from a REST handler, because it would miss the pin.
- **PAT-001**: Reuse `plan`, `check_binding` (through the existing recheck), `write_binding_record`, `provision_scope`, `resources`, `refusal_message` and the fixed messages.
  - `prepare` SHOULD stay byte-identical.
  - The reload's own provisioning and record loops live in `reload.rs`, because they map errors differently (5xx storage passthrough) and run in the opposite order.
  - Only the pure scope-set computation (`grants → BTreeSet` of scope tuples) MAY be shared as a helper, and only if no anchor line moves.
- **PAT-002**: Reuse the existing task-local request context (`track_hidden`, `current`, `within`) as the request-lifetime carrier, rather than adding a middleware layer or a global barrier lock.

## 5. Risk Classification

| ID | Risk | Level | Mitigation |
|---|---|---|---|
| RISK-001 | A future handler reads grants outside the request context (a detached task) and escapes the pin | 🟡 | REQ-014 makes `guard::bindings` the only reader. GUD-002. TEST-003 covers runners. The two call sites are the full set today (§3.1) |
| RISK-002 | Partial record writes (FM-009) leave reads under old grants while writes fail closed | 🟡 | The same per-binding commits as startup; retry converges (TEST-010); docs state that a failed reload is not a revocation |
| RISK-003 | A long request keeps `drained:false` for longer than operators expect | 🟢 | An honest flag; repeat the request to wait again; credential revoke stays available; no request is cut off |
| RISK-004 | Library embedders with several REST routers on one `Service` reload only one router's cell (RD-010) | 🟡 | Documented in the embedding notes. The shipped binary builds one router (`src/bin/vlite-server.rs:723`). Other routers' memory writes fail closed through the recheck |
| RISK-005 | Refactoring `prepare` and the cell type breaks mutation-ledger anchors | 🟢 | CON-004 with a measured baseline of `stale:0`; keep anchor texts and names (`covers`, `is_configured`, `self.memory_bindings = bindings.clone();`) |
| RISK-006 | A non-recursive admin-route gate tempts registering the route in a subdirectory | 🟢 | REQ-001 registers it in `admin.rs`; the alternative is rejected as gate evasion (§11) |
| RISK-007 | The admin-route gate forces a dashboard control although UI is excluded | 🟢 | One button only, mirroring token reload (RD-012); flagged to the owner |
| RISK-008 | Lock-order deadlock between the database lock, `published_bindings` and the cell | 🟢 | Fixed order: database write lock → `published_bindings` → cell lock (leaf). `publish_with` and `adopt_published` never take the database lock |
| ASSUMPTION-001 | The admin role's guard decisions do not depend on grants | — | Verified at `src/server/reserved.rs:80-90,136-140`; pinned by TEST-002 |
| ASSUMPTION-002 | No REST middleware builds a `RequestScope` | — | Verified at `src/server.rs:974-976` and by grep; TEST-013 (`drained:true` when idle) would expose a violation |
| ASSUMPTION-003 | `tokio::time` is available in the `server` build | — | The direct `tokio` features in `Cargo.toml:69` do not list `time`. It is enabled through `tokio-stream`'s default `time` feature, which `server` pulls in (`cargo tree -e features -i tokio`), and production server code already depends on it (`src/server/providers.rs:1032`, `src/server/tls.rs:125`). No `Cargo.toml` change |
| ASSUMPTION-004 | A 10-second drain bound suits REST traffic | — | No REST streaming. The bound is a constant, adjustable later without wire changes |

## 6. Dependencies

- **DEP-001**: The base is local `HEAD` `4cdb2ce311ab94acad35df393888291df3ee7af3`, which the parent session is pushing to `main`. Implementation MUST start from that commit, or from `main` once it contains it.
- **DEP-002**: No new crate. The change uses existing `tokio` (`sync`, plus `time` as already used), `axum` and `std::sync`.
- **DEP-003**: Existing memory startup primitives: `plan`, `check_binding`, `write_binding_record` (`src/memory/startup.rs`), `provision_scope` (`src/memory.rs`) and `validate_against` (`src/memory/binding.rs`).
- **DEP-004**: CI gates that a new admin route triggers:
  - `tests/admin_route_coverage.py` (ci.yml:294, dashboard-ui.yml:79, release.yml:50);
  - `tests/dashboard_feature_coverage.py`;
  - `tests/api_reference.py`;
  - `tests/openapi_contract.py`;
  - `tests/graph_surface_parity.rs` REG-008;
  - `tests/dashboard_memory_mode.py` (dashboard-ui.yml:112).

## 7. Quality & Testing

**Unit tests (`src/server/reserved.rs`, `src/server/guard.rs`).**
- **TEST-001**: `Served` swaps and drains correctly.
  - `install` swaps `current()`.
  - A pin taken before the install still yields the old `Arc` (`ptr_eq`).
  - `drain(50 ms)` returns `false` while that pin lives and `true` after it is dropped.
  - Dead entries are pruned.
- **TEST-002**: For two views that differ only in grants (including `raw_access`), the Admin role's guard decisions are identical: `raw_allowed(None, ..)`, `attachment_allowed` and `retain_visible(None, ..)`.
- **TEST-003**: Inside one `track_hidden` context, two `guard::bindings` calls separated by an `install` return the same `Arc`, including inside `spawn_blocking` via `within`. A new context sees the new view. Outside any context, the call returns `current()`.
- **TEST-004**: A `RequestScope` created before an install that revokes `raw_access` keeps raw visibility. A scope from a new request hides the resource (`collection_not_found`).

**In-crate REST pins (new `src/server/memory/pins_reload.rs`, using the `pins_binding.rs` build and holder-thread pattern at :145-244 and :454-489).**
- **TEST-005**: A `remember` resolved under the old view and held at the write lock by a holder thread across a reload that advances the record answers 409 `binding_generation` and writes nothing.
- **TEST-006**:
  - A `recall` pinned before the install completes under the old grants.
  - While it is held, a reload through a test hook with `DRAIN_TIMEOUT` = 50 ms reports `drained:false`.
  - After release, a reload of the unchanged file reports `drained:true` and `changed:0`.
- **TEST-007**: Storage failure mapping.
  - Inject `CommitIndeterminate` by replacing `db.file` with a read-only handle, as in `src/server.rs:4220-4260`.
  - A reload that must provision answers 503 `commit_indeterminate`.
  - The served view is unchanged and `commit_failures` is incremented.
- **TEST-008**: Two concurrent reloads: one answers 409 `memory_reload_in_progress`.
- **TEST-009** (`#[cfg(feature = "grpc")]`, because `adopt_published` exists only with gRPC, `src/server.rs:459`): after `adopt_published` (`grpc_built`), the reload answers 409 `memory_reload_unavailable` and capabilities report `memory_bindings_reload:false`.

**Integration tests (new `tests/server_memory_live_grants.rs`, `#![cfg(feature = "server")]`, reusing the `Rig` helpers of `tests/server_memory_binding_record.rs:36-218`).**
- **TEST-010**: Partial-state convergence.
  - Pre-write binding A's new record, emulating FM-009.
  - Memory writes on A answer 409 and reads keep the old grants.
  - A reload with the file converges (`changed:2`).
  - A restart with the same file adds no commit (file size and record node unchanged).
- **TEST-011**: Grant changes apply live:
  - revoking a scope gives 403 `forbidden_scope` on the next request;
  - adding a scope or read scope provisions it, and recall answers an empty list;
  - adding or replacing a principal takes effect;
  - revoking `raw_access` hides the raw graph and collection using their existing missing-resource responses (409 `graph_not_found`, 404 `collection_not_found`), and granting it restores access.
- **TEST-012**: Refusals change nothing (record node and file size unchanged):
  - changed grants with the same generation → 409 `memory_bindings_conflict`; an unchanged file at the same generation is accepted under REQ-010;
  - lower or reverted generation → 409;
  - generation raised without a change → 409;
  - a window, state, graph or evidence change, or an added or removed binding → 409 `memory_restart_required`;
  - an invalid or half-written file → 422;
  - no file → 409 `memory_reload_unavailable`;
  - read-only token and scoped credential → 403.
- **TEST-013**: Response and persistence.
  - With no other request in flight, the reload answers `drained:true`.
  - The body has exactly `bindings`, `changed` and `drained` and contains no name, principal or scope string.
  - A restart with the same file after a reload adds no commit.
  - A restart with the pre-reload file is refused.
  - Capabilities list the flag and the route.
- **TEST-014**: On a read-only reopen, the reload installs the new grants (reads follow them), writes nothing, and memory writes stay 403 `read_only`.

**Dashboard and gates.**
- **TEST-015**: `tests/dashboard_memory_mode.py` (real server with a bindings file): the "Reload memory grants" button, after confirmation, shows a result with `"drained": true`.
- **TEST-016**: These gates pass with only additive, reviewed entries:
  - `tests/admin_route_coverage.py`;
  - `tests/dashboard_feature_coverage.py`;
  - `tests/api_reference.py`;
  - `tests/openapi_contract.py`;
  - `tests/api_route_inventory.py`;
  - `tests/graph_surface_parity.rs`;
  - `scripts/api_surface.py check`;
  - `tests/content_budget.py`, `tests/release_docs.py` and `tests/agent_docs.py`.

**Regression.**
- **TEST-017**: Existing suites pass unchanged, including `server::memory::tests::*`, `tests/server_memory_binding_record.rs` and `src/server/published.rs`. The `published.rs` edits are limited to `.reserved()` → `.current().reserved()`.
- **TEST-018**:
  - `python tests/mutant_replay.py plan` reports `"stale":0` for the four replayable ledgers.
  - A count script shows every `G71-*` `old` text of `tests/reserved_resource_mutants.json` occurring exactly once in its file (CON-004).
- **TEST-019**: Cancellation and success auditing (ITEM-006 and ITEM-011):
  - Drop the outer reload while its apply waits on the database lock and while its drain waits on an old pin; the owned task completes without the caller.
  - A concurrent reload is still refused until the bounded drain ends.
  - Successful reloads append exactly one configured audit event with only `event`, `bindings`, `changed`, `drained` and `outcome:"ok"`; no binding, tenant, principal, scope or path is disclosed.

**Validation commands.**
- `cargo nextest run --locked --workspace --all-targets --all-features`
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- The Python and node gates listed in TEST-016, as run by `.github/workflows/ci.yml` and `.github/workflows/dashboard-ui.yml`.

### Acceptance Criteria

| ID | Criterion | Verification | Traces To |
|---|---|---|---|
| AC-001 | A grants-only file change takes effect on the next request without a restart, for scopes, read scopes, principals and `raw_access` | TEST-011 | G1, REQ-005, REQ-008 |
| AC-002 | Every non-grants change, stale generation, invalid file and unavailable state is refused with its documented code, and nothing changes | TEST-012, TEST-009 | REQ-002, REQ-003, REQ-005, REQ-006, FM-001–FM-006, FM-014, FM-015 |
| AC-003 | One request never authorizes with two grants versions, for raw and memory routes | TEST-003, TEST-004, TEST-006 | SEC-003, REQ-014 |
| AC-004 | A memory write admitted under superseded grants never commits after its record advanced | TEST-005 | SEC-004, FM-016 |
| AC-005 | Served grants never run ahead of durable records; storage failures report 5xx and leave the served view unchanged | TEST-007, TEST-010 | SEC-005, REQ-012, FM-008, FM-009 |
| AC-006 | `drained` is truthful: `false` while an older-grants request lives, `true` when none does, never blocked by the reload itself | TEST-001, TEST-006, TEST-013 | REQ-015, REQ-016, FM-013 |
| AC-007 | After a reload, a restart with the same file adds no commit; an older file is refused | TEST-010, TEST-013 | G3, §3.6 |
| AC-008 | Admin-only; no name, principal, scope or path in any response, log or audit event | TEST-012, TEST-013, TEST-019 | SEC-001, SEC-002 |
| AC-009 | Startup, file format, generation rules and existing routes are unchanged; mutation anchors stay at `stale:0` | TEST-017, TEST-018 | REQ-019, CON-001, CON-004 |
| AC-010 | All CI gates pass with only additive, reviewed entries; no public API change | TEST-015, TEST-016 | CON-002, CON-003 |
| AC-011 | Read-only and replica reloads apply the grants and write nothing | TEST-014 | REQ-009, FM-010 |

## 8. Security Considerations

- **Data handling.**
  - The reload reads the private bindings file, which holds credential IDs (never tokens) and scope tuples.
  - Nothing from the file is echoed: responses carry counts only, errors use fixed messages, and the stderr line and audit event carry counts and outcome only (SEC-002).
  - The existing request audit ring records completed HTTP requests' route, status, role and caller (`src/server.rs:1479`); cancellation can omit that ring entry. The counts-only success event survives caller cancellation but does not attribute an actor.
  - Revoking a scope does not erase its memory (FM-018).
- **Input validation.**
  - The request carries no input. The file goes through the strict startup parser and the 1 MiB cap.
  - The grants-only delta check, `plan` (generation and rotation rules, adoption) and the reservation-equality check all run before any write.
  - A half-written file fails the strict parse (FM-001). Documentation recommends replacing the file atomically.
- **Access control.**
  - The route is admin-only through the existing deny-unmapped fallback (SEC-001).
  - Grant changes inherit every existing per-request check, because only the source of the grants changes, not how they are applied.
  - Superseded grants are bounded:
    - per request, by the pin (SEC-003);
    - for memory writes, by the record recheck (SEC-004);
    - in time, by the reported drain.
  - Credential revoke stays the immediate emergency control (SEC-006).
  - gRPC live-reload behavior is not supported or qualified by this feature and is excluded (REQ-003).
- **Secrets.** No token is read, written or logged. The file path comes from server configuration only and is never accepted from a request or returned in a response.

## 9. Deployment & Rollback

- **Additive and dormant.** The capability does nothing until an admin sends the request. A server that is never asked to reload behaves exactly as at the base commit.
- **Discovery.** Clients detect support through `features.memory_bindings_reload` and the capabilities `routes`.
  - An older server answers 404 to an admin, because the route is unregistered.
  - For a non-admin, `Need::Admin` deny-unmapped answers 403.
  - The button uses the existing `data-feature="memory_bindings_reload"` gating and `admin("memory_bindings_reload")` check, as the tenant buttons and token reload do, so it is unavailable when the flag is absent.
- **Rollback.**
  - Deploy the previous binary. Records written by a reload are exactly the records a restart with the same file writes, and the file format is unchanged, so 0.7.2 starts with the same file and database without a migration.
  - A file that uses `read_scopes` still needs a server with read-scope support, as documented today.
- **Operations.**
  - The reload holds the database write lock for the same checks startup runs: resource checks and `plan` over every binding, then idempotent provisioning of every active binding (one lookup per existing scope). Record writes and commits happen only for the changed bindings and any new scope. Memory deployments hold few bindings (the file is capped at 1 MiB), so this lasts about as long as the startup memory step.
  - Recommended procedure: replace the file atomically → send the request → confirm `drained:true`, or send it again.
- **Release.** No version bump or release notes. Whether to release this as 0.7.3 is a separate decision for the owner.

## 10. Resolved Decisions

| ID | Decision | Rationale |
|---|---|---|
| RD-001 | **Interface:** `POST /v1/admin/memory/bindings/reload`, no request input, rereading the configured file | Smallest operator-triggered interface; mirrors `tokens/reload` and `tls/reload`; the file stays the only source of truth, so there is no second store and no stale file |
| RD-002 | Admin-only through the existing `Need::Admin` fallback | Deny-unmapped already gives the right answer; no new permission arm |
| RD-003 | Grants-only delta. Every other binding change, including retiring, stays restart-only | Matches the confirmed scope; reservations and routes stay invariant, so only two grant readers need the barrier |
| RD-004 | Restart parity: reuse the startup resource checks and `plan` with unchanged generation rules | No second validation path; generations act as the operator's optimistic concurrency token |
| RD-005 | Apply order: provision → write records → install, all under one exclusive database write lock | Install after records keeps served state from running ahead of durable state. Provisioning before records means a provisioning failure leaves the records, and so the running server and a restart with the old file, untouched. Installing under the lock means no write can slip between the record write and the swap |
| RD-006 | Provision every active binding of the new set, as startup does | Exact restart parity. Covers records written without provisioning (a failed startup, the `write_records` test state). Provisioning is idempotent, so existing scopes cost one lookup each and no commit |
| RD-007 | A live cell (`Served`) with a lazy per-request pin carried by the existing task-local context, plus a bounded drain | Evidence: only two code paths read grants, and every REST request already has a propagated context. This changes two call sites, not every handler |
| RD-008 | `DRAIN_TIMEOUT` = 10 s, polled every 10 ms; `drained` reported, never enforced by cancelling requests | Bounded admin latency below common proxy timeouts; no request is cut off; an honest flag |
| RD-009 | Refuse reload once a gRPC router was built (checked under `published_bindings`) | Live-reload behavior over gRPC is outside this feature's supported and qualified scope; the shipped server already refuses gRPC with bindings |
| RD-010 | One live cell per configured REST router; reload applies to the router that answered | Matches per-router `AdminState` (tokens, credentials) at `src/server/admin.rs:370-384`; the shipped binary builds one router |
| RD-011 | An unchanged file is rechecked against the records and provisioned like any reload, installs nothing, and waits for the drain | Gives operators a way to wait again after `drained:false` without a second route. Keeps restart parity: a reverted file after FM-009 is refused rather than reported as success |
| RD-012 | One dashboard button ("Reload memory grants") in the existing admin token section, handled in `console-tools.js` | `tests/admin_route_coverage.py` requires a named control for every admin route, and lowering it is forbidden. This is not the excluded memory-management UI |
| RD-013 | New error codes `memory_reload_unavailable`, `memory_reload_in_progress`, `memory_restart_required`, `memory_bindings_conflict`; storage 5xx mappings pass through | Distinguishes configuration state, a race, an out-of-scope change and a rule refusal; keeps `commit_indeterminate` distinct, unlike startup's fixed strings |
| RD-014 | Best-effort persistent success audit event and a stderr line, counts only, independent of caller cancellation | Grant changes matter for security; the owned task completes reporting after its bounded drain. Completed HTTP requests also enter the existing request ring, but that ring is not guaranteed on disconnect. Actor attribution and failed-reload events are not added |

## 11. Alternatives Considered

| Alternative | Pros | Cons | Decision |
|---|---|---|---|
| Restart only (status quo) | No code | Drops connections; slow revocation; the owner asked for restart-free | Rejected |
| SIGHUP | Unix convention | Not portable (Windows), no response channel, no drain report | Rejected |
| File watcher | Automatic | Not operator-triggered; may read partial writes; no response channel | Rejected |
| PUT JSON grants, or the F4 `memory.json` store | Self-contained requests | Second source of truth, stale file at restart, larger scope (parked F4) | Rejected |
| Plain `Arc` swap | Tiny | One request can mix versions; no bound on superseded grants; served state can run ahead of records | Rejected |
| A global write-preferring `RwLock` barrier around every request | Strong ordering | Blocks all traffic during a reload; deadlock risk with the database lock | Rejected |
| A generation recheck on every read route | No in-flight reads | Touches every memory read handler; does not cover raw routes or list filtering | Rejected |
| Swap first, write records after | Fast visibility | Served grants run ahead of durable records; a failed write leaves a contradiction | Rejected |
| Partially install only the bindings whose records were written | Finer recovery | Complex; the reservation and cap checks would run on a mixed set | Rejected |
| Reuse `prepare()` | Less code | Writes records before provisioning; erases the storage error distinction | Rejected |
| Register the route under `src/server/memory/` or outside `/v1/admin/` to avoid the dashboard gate | No UI | Gate evasion | Rejected |
| Eager pin on every request in `observe` | Simpler | Pins the reload request itself; drains wait for unrelated requests | Rejected |

## 12. Files

- **FILE-001**: `src/server/reserved.rs` — `Served`, `Epoch` and `Pin`; delegating methods; `install` and `drain`; unit tests TEST-001 and TEST-002.
- **FILE-002**: `src/server/guard.rs` — request context with a lazy pin; `guard::bindings`; `RequestScope::new` uses it; TEST-003 and TEST-004.
- **FILE-003**: `src/server.rs` — field types (`memory_bindings`, `published_bindings`); `with_bindings` and the source; default construction. Anchor texts in `publish_with` and `adopt_published` stay unchanged.
- **FILE-004**: `src/server/memory/routes.rs` — `resolve` reads through `guard::bindings` (line 239 only).
- **FILE-005**: `src/server/tenants.rs` — line 289 reads `current().iter()`.
- **FILE-006**: `src/server/memory.rs` — `mod reload;` and `#[cfg(test)] mod pins_reload;`; `current()` at the startup call sites (`start`, the `write_records` test helper). `prepare` stays byte-identical (PAT-001, CON-004).
- **FILE-007**: `src/server/memory/reload.rs` (new) — handler, delta check, apply, drain and reporting.
- **FILE-008**: `src/server/admin.rs` — route registration as a literal `.route("/v1/admin/memory/bindings/reload", post(..))`; source recording in `configured_admin_router` (REQ-017); capabilities flag and route. `read_memory_bindings` is unchanged: its errors are always one of three fixed messages, which the reload maps to 422 `invalid_input` through `ApiError::new(.., error.to_string())`.
- **FILE-009**: `src/server/published.rs` — test updates (`.current().reserved()`).
- **FILE-010**: `src/server/memory/pins_reload.rs` (new, `#[cfg(test)]`) — TEST-005 to TEST-009.
- **FILE-011**: `tests/server_memory_live_grants.rs` (new) — TEST-010 to TEST-014.
- **FILE-012**: `src/openapi.json` — one path line for the route.
- **FILE-013**: `tests/graph_surface_parity.rs` — one reviewed `APPROVED_OPENAPI_ADDITIONS` entry with a comment.
- **FILE-014**: `site/api.html` — one `data-api="rest:POST /v1/admin/memory/bindings/reload"` row.
- **FILE-015**: `tests/admin_route_coverage.py` — one `INVENTORY` entry `("console-tools.js", "memory-bindings-reload")`.
- **FILE-016**: `src/dashboard/index.html` — one paragraph, button and `<pre>` in `#admin-token-management`.
- **FILE-017**: `src/dashboard/console-tools.js` — one `on("memory-bindings-reload", …)` handler.
- **FILE-018**: `tests/dashboard_memory_mode.py` — TEST-015.
- **FILE-019**: `docs/API.md` — a "Change grants without a restart" subsection and the error codes.
- **FILE-020**: `docs/MEMORY.md` — "raise `generation` and restart" becomes "… and reload or restart", plus the procedure.
- **FILE-021**: `docs/SECURITY.md` — the in-flight contract, `drained`, FM-009 and emergency revoke.
- **FILE-022**: `docs/CONSOLE-ADMIN.md` — one row for the route and button.

## 13. Simplicity Rationale

- **Scope justification.**
  - G1 needs a trigger and an apply path (EPIC-002).
  - G2 needs the cell, pin and drain (EPIC-001). Without them a swap is unsafe (§3.5).
  - EPIC-003 is what CI requires for any new admin route.
  - EPIC-004 and EPIC-005 provide proof and the operator contract.
  - Every lifecycle change beyond grants is excluded, which keeps reservations, routes, orphans and the gRPC and maintenance refusals invariant.
- **Abstractions check.**
  - One new type with two private helpers (`Served`, `Epoch`, `Pin`), one new function (`guard::bindings`) and one new module (`reload.rs`).
  - No trait, no new layer and no new crate.
  - The barrier reuses the existing task-local context instead of adding middleware.
- **Configuration check.**
  - No new option, environment variable or file field.
  - `DRAIN_TIMEOUT` is a constant.
  - The file path is the existing startup option.
- **Could this be simpler?**
  - A plain swap would be simpler and is unsafe (§11).
  - Dropping the drain would leave operators unable to know when revocation is complete.
  - Dropping the button would require lowering a gate, which is forbidden.
  - Skipping the database for an unchanged file, or provisioning only the rewritten bindings, would be marginally faster but would break restart parity (RD-006, RD-011).

## 14. Implementation Plan

EPIC-001 and EPIC-002 are sequential. EPIC-003 and EPIC-005 can follow EPIC-002 in parallel. EPIC-004 completes after EPIC-002 and EPIC-003.

### EPIC-001: Live grants cell and request-lifetime pin

| Task | Description | Status | Relevant Files |
|---|---|---|---|
| ITEM-001 | Add `Served`, `Epoch` and `Pin` with `current`, `pin`, `install`, `drain(timeout)` and delegating `is_empty`, `is_configured` and `covers`; `DRAIN_TIMEOUT`; TEST-001 and TEST-002 | Implemented | `src/server/reserved.rs` |
| ITEM-002 | Change `Service.memory_bindings` to `Arc<Served>` and `published_bindings` to `Option<Arc<Served>>`; `with_bindings` keeps a `None` source; keep anchor texts | Implemented | `src/server.rs` |
| ITEM-003 | Request context with a lazy `OnceLock<Pin>`; `guard::bindings`; `RequestScope::new` uses it. `track_hidden` keeps `let flag = &context.hidden;` so `(output, flag.load(Ordering::Relaxed))` stays byte-identical (G71-19); the 23 `G71-*` guard anchors are unchanged. TEST-003 and TEST-004 | Implemented | `src/server/guard.rs` |
| ITEM-004 | Switch memory `resolve` to `guard::bindings`. Switch `tenants.rs:289`, `start`'s `prepare(db, &service.memory_bindings.current(), ..)` at `src/server/memory.rs:105` and the `write_records` test helper at `:85` to `current()`. Update the `published.rs` tests | Implemented | `src/server/memory/routes.rs`, `src/server/tenants.rs`, `src/server/memory.rs`, `src/server/published.rs` |

### EPIC-002: Reload route and apply path

| Task | Description | Status | Relevant Files |
|---|---|---|---|
| ITEM-005 | Declare `mod reload`; make `resources`, `refusal_message` and the fixed messages reachable from `reload.rs` (visibility only; `prepare` byte-identical) | Implemented | `src/server/memory.rs` |
| ITEM-006 | Implement the handler of §3.4: availability, owned reload lock, file read, the closure under the database and `published_bindings` locks, delta, resource checks, plan, reservation equality, provisioning (every active binding), records, conditional install; then drain, log, audit and response | Implemented (owned-task cancellation fix) | `src/server/memory/reload.rs`, `tests/server_name_lookup_scan.rs` |
| ITEM-007 | Register the literal route; record the source after the orphan block (REQ-017); capabilities flag and route; `read_memory_bindings` unchanged | Implemented | `src/server/admin.rs` |

### EPIC-003: Contract surfaces and CI gates

| Task | Description | Status | Relevant Files |
|---|---|---|---|
| ITEM-008 | OpenAPI path line (admin security; 200, 403, 409, 422, 503 responses) and the matching reviewed REG-008 addition | Implemented | `src/openapi.json`, `tests/graph_surface_parity.rs` |
| ITEM-009 | API reference row | Implemented | `site/api.html` |
| ITEM-010 | Dashboard button, result `<pre>` and handler; admin-route inventory entry | Implemented | `src/dashboard/index.html`, `src/dashboard/console-tools.js`, `tests/admin_route_coverage.py` |

### EPIC-004: Tests

| Task | Description | Status | Relevant Files |
|---|---|---|---|
| ITEM-011 | In-crate REST pins TEST-005 to TEST-009 and cancellation/audit TEST-019 | Implemented | `src/server/memory/pins_reload.rs`, `src/server/memory.rs` |
| ITEM-012 | Integration tests TEST-010 to TEST-014 | Implemented | `tests/server_memory_live_grants.rs` |
| ITEM-013 | Dashboard end-to-end step TEST-015; run the TEST-016 to TEST-018 gates and the validation commands | In progress: final validation | `tests/dashboard_memory_mode.py` |

### EPIC-005: Documentation

| Task | Description | Status | Relevant Files |
|---|---|---|---|
| ITEM-014 | Operator procedure, response and error codes, in-flight contract, FM-009, emergency revoke, library multi-router note; within content budgets | In progress: review clarifications | `docs/API.md`, `docs/MEMORY.md`, `docs/SECURITY.md`, `docs/CONSOLE-ADMIN.md` |

## 15. Change Log

| Version | Date | Change |
|---|---|---|
| 1.0 | 2026-11-08 | Initial PRD from local `HEAD` `4cdb2ce3`, with baseline tests (518 passed) and mutation anchors (`stale:0`) recorded. |
| 1.1 | 2026-11-08 | Author self-check against code: provisioning covers every active binding and an unchanged file is rechecked rather than skipped (restart parity, RD-006 and RD-011); `prepare` stays byte-identical (PAT-001); anchors listed per file (CON-004); `tokio::time` provenance recorded (ASSUMPTION-003); `covers` snapshots both views (REQ-013). |
| 1.2 | 2026-11-08 | One local read-only `code-review` pass against `4cdb2ce3`. It found no safety, authorization, lock-order, drain or CI-gate defect: the two grant readers, task-local coverage, recheck-under-write-lock and gate compliance were confirmed. Applied: (MAJOR) `tests/reserved_resource_mutants.json` `G71-*` anchors added to CON-004 and TEST-018, with G71-19 and G71-21 preserved; (MINOR) the `with_orphans` and `prepare` call sites named and the source placement fixed (REQ-017, ITEM-004); §9 lock-duration text matched to RD-006; TEST-009 gated on `grpc`. Simplification: `read_memory_bindings` stays unchanged (FILE-008). |
| 1.3 | 2026-11-09 | Integration clarifications: TEST-011 preserves the existing graph 409 and collection 404 missing-resource responses; TEST-012's same-generation refusal applies to changed grants, not an unchanged file. No production status or generation rule changes. |
| 1.4 | 2026-11-09 | Three-perspective implementation review identified lost success auditing on caller cancellation. Clarified owned-task completion through drain/report and added TEST-019; corrected the gRPC-context rationale and request-ring limitations without adding actor or failure-event schemas. Full regression found the source-guard scan rejecting reload's two operator-file existence lookups; added two justified allow entries. |
