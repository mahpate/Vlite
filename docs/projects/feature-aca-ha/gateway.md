# Shared gateway: scale-out is not writer HA

Design reference for the shared gateway in the staged Azure Container Apps (ACA) HA plan ([PRD](feature-aca-ha.prd.md), EPIC-004 ITEM-018). It states what the gateway is, what state it may hold, where its responsibility ends and what is built today. The PRD remains the authority; section references below point into it.

> **Status (2026-11-19).** The gateway MVP is wired as an opt-in mode of the same binary: `vlite-server gateway --config <json>` opens no database, and `vlite-server <db> --gateway-trust-file <json>` runs a gateway-trusted backend. Commands, configuration schema and the control-state bootstrap are in [API.md](../../API.md#gateway-mode-and-gateway-trusted-backends-experimental-opt-in-off-by-default). Opt-in local writer authority, epoch fencing, the gateway's response-epoch check for `dedicated` records and manually prepared failover ([HA-ACA.md](../../HA-ACA.md)) are implemented. It has run only as local processes against a loopback Azurite (test plan tier (b)). It is not deployed, has no ACA rehearsal, measured timing or automatic failover, and no zero-window fencing guarantee is claimed. The default deployment is still one standalone server, and production is unchanged.

## Gateway scale-out is not writer HA

The gateway is a stateless public edge: TLS, authentication, operation classification, edge rate limiting and routing to a tenant's backend. It opens no database and never holds writer authority. Adding gateway replicas therefore raises request-handling capacity and nothing else.

| More of... | Adds | Does NOT add |
|---|---|---|
| Gateway replicas | Concurrent request handling (TLS, authentication, routing, edge limits); survival of one gateway replica loss | Write capacity, tenant failover, read-your-writes, data durability |
| Reader replicas | Read capacity for staleness-tolerant reads of opted-in tenants | Write capacity, failover, read-your-writes |
| Writer replicas | Nothing: each tenant has exactly one active writer | - |

- **One writer per tenant.** Write throughput is bounded by that writer, as today (C-08, AC-011).
- **A writer outage lasts until a fenced failover completes.** In v1 an operator performs it and acknowledges a loss report (C-05, PRD section 3.7). Gateway replicas cannot mask it.
- **Readers are eventually consistent.** Freshness-required reads go to the writer (C-02, OD-007).
- **The loss window is whatever archival actually achieved**, not a configured interval (C-03).

## State classification

"Stateless" means any replica can serve any request and can be added, removed or killed without losing authority. A replica may keep disposable caches rebuilt from shared control state; it must never hold the only copy of a tenant, credential, session, policy, routing or authority decision (PRD 3.4.1-3.4.2).

| State | Today (standalone) | Gateway class | Built so far |
|---|---|---|---|
| Credential and policy snapshot, generation `A` | `credentials.json`, loaded at startup | Authoritative in shared control state; replicas cache a snapshot for at most S_ctl | Wired (opt-in): `vlite.control-state/1` object with CAS writes, published from an admin directory by `vlite-server control-state`; gateways and trusted backends poll it and fail closed after S_ctl (`src/server/control_state.rs`) |
| Tenant status (active, suspended, deleted) | `tenants.json` | Authoritative shared; cached like the policy snapshot | Wired: carried in the same snapshot; suspension reaches gateways and backends by polling |
| Tenant routing record (deployment kind, endpoints, epoch `E`) | Not present | Authoritative shared; cached snapshot, generation `A` | Wired: directory records in the snapshot; every request goes to the record's writer. For a `dedicated` record the gateway requires one canonical `X-VLite-Epoch` stamp at least both the dispatched and the current record's `E`; otherwise a read gets `503 backend_epoch_stale` and a write `502 write_outcome_unknown` (OD-002, EPIC-006) |
| Memory bindings | Startup-only file | Authoritative shared; enforced by backends | Not in the snapshot yet (FR-012); a trusted backend applies its own startup bindings |
| Dashboard users and sessions | `users.json`; sessions in process memory | Removed from the gateway MVP | Deferred (OD-004): the gateway refuses sign-in and dashboard routes |
| Rate-limit buckets | One token bucket per process | Disposable per replica; static share of the tenant limit | Wired: one bounded bucket per credential per gateway replica, capacity and refill `replica_share(limit, max_replicas)`; backends keep their own bucket |
| Writer authority, epoch `E` | Not present | Authoritative shared; CAS-only | Wired (opt-in, EPIC-006): `vlite-server authority` and `--authority-target-file`; the writer CAS-claims its epoch and stamps responses with it. Failover is operator-prepared; not ACA-qualified |
| Route-classification table | Compiled code | Code, changed by release | Wired: the gateway classifies every request before forwarding (`src/server/gateway.rs`) |
| Idempotency receipts | Memory API only | Backend-owned; gateway keeps none | Unchanged; the gateway retries nothing |
| Envelope signing key | Not present | Platform secret store, never in control state or logs (OD-009) | Wired: `gateway-keygen` writes a signing-key file and a public bundle, both read at startup; ACA app-secret volume delivery (OD-009) not run |

## Boundary: gateway versus tenant backends

| Concern | Gateway (scale-out, public) | Writer app (1 replica, private) | Reader app (1..N, private) |
|---|---|---|---|
| Authentication | Checks the bearer credential against the cached snapshot; mints a signed principal envelope | Accepts only a valid envelope (audience, expiry, `A`); network position grants nothing | Same as writer |
| Authorization | Classifies the operation and applies scoped-credential policy before routing | Re-runs `permission`, `authorize` and the guard independently | Same, and refuses every mutation |
| Routing | Owns tenant lookup; picks writer, reader or refusal | Does not route | Does not route |
| Rate limits | Edge share of the tenant limit | Own per-credential bucket; authoritative vector and storage quotas | Own per-credential bucket |
| Data | None | Authoritative database; archive publisher; epoch holder | Local copy applied from the archive; never promotable by itself |
| Scaling | ACA HTTP rule; minimum 2 warm replicas proposed | Fixed at 1; revision overlap handled by fencing | ACA rules |
| Failure | Loses only its in-flight requests | Fenced failover | Other readers or the writer serve |

Backends never trust the gateway alone: a request without a valid envelope gets `401`, even from inside the ACA environment, because internal ingress is reachability, not authentication (PRD 3.6).

## What is built

| Piece | Status | Reachable in a running server? |
|---|---|---|
| Serving modes `standalone`, `writer`, `reader` (EPIC-002) | Implemented. `--serve-mode` or `VLITE_SERVE_MODE`, default `standalone`. A reader refuses writes with `403 reader_mode`; writer and reader report `draining` on `/ready` after SIGTERM; `GET /v1/admin/serve-status` reports mode and archive progress. | Yes, opt-in |
| Control-state snapshot and poller (ITEM-008, ITEM-011) | Generation `A`, CAS create and update, stale cache refused with `503 control_state_stale`, regression refused and logged. `vlite-server control-state publish` and `update` import an admin directory and a directory file with that CAS, so nobody edits the object by hand; `update` makes exactly one change. Revoke, suspend and resume change the local admin directory and then only that id in the published state, so a stale or concurrent admin directory cannot undo another change, and a retry after a failed publish is safe. New credentials or tenants need `--reimport-if-generation <A>`, which publishes only while the published generation is still `A` (else `409 control_state_generation_mismatch`) and keeps published revocations, earlier expiries, suspensions and deletions. A bare `update` is refused, and a second command on the same admin directory gets `admin_dir_busy` (`control-state.lock`). | Yes, opt-in |
| Principal envelope (ITEM-009) | Ed25519 `vle1` tokens in `x-vlite-principal`; audience, expiry, skew and minimum `A` checks; two-key rotation overlap. `vlite-server gateway-keygen` writes the signing-key file and public bundle. | Yes, opt-in |
| Gateway-trusted backend authentication (ITEM-010) | `--gateway-trust-file`. Envelope-only authentication feeding the existing `permission`/`authorize`/guard path. The backend checks the tenant's directory record (audience, lineage) and admission in its own polled copy, and its poll thread raises the minimum accepted `A` without waiting for requests. | Yes, opt-in |
| Operation classifier (ITEM-013) | Classifies all 179 registered REST routes by method and path; unknown paths and methods are refused, never forwarded. | Yes, in gateway mode |
| Tenant directory and routing (ITEM-014) | `vlite-server gateway`. One validated record per tenant (`shared-pool` or `dedicated`). Each request is authenticated against the fresh snapshot, classified and authorized, then forwarded to that tenant's writer with an envelope minted at the current `A`. Caller credentials, cookies, principal, proxy, hop-by-hop and `Connection`-named headers are dropped; no redirect is followed. `upstream_timeout_ms` bounds connecting, waiting for the response head and each wait for the next streamed piece, so a long response is not cut off while data keeps arriving; a stalled stream is cut short after the head was sent and logged as `response_cut_short`. Nothing is retried: a write that fails after it may have been sent gets `502 write_outcome_unknown`. With `read_routing: readers` (ITEM-021), a read goes to the reader and falls back to the writer when the reader is unreachable, answers 503, or has a missing or mismatched lineage, binding generation or `X-VLite-Epoch` stamp; the writer's answer is still epoch-checked. No real-process test qualifies reader routing yet, so keep `writer_only`. | Yes, opt-in |
| Gateway readiness and drain (ITEM-015) | `/health` and `/ready` without a database or `/metrics`. Startup reads the control state before the listener binds. `/ready` fails when the snapshot is older than S_ctl. On a signal, `/ready` answers `draining` for `readiness_propagation_ms` before the listener closes, in-flight requests get `drain_timeout_ms`, and the poller stops. A trusted backend drains the same way. | Yes, opt-in |
| Per-replica rate-limit share (ITEM-016) | One bucket per credential per gateway replica, holding and refilling `replica_share(limit, max_replicas)`, never rounded up; at most `max_buckets`. A share under one request a second is refused with `503 rate_limit_share_below_one`. Backends keep their own buckets and vector quotas. | Yes, opt-in |

The timing and replica values are configuration fields with no defaults (S_ctl, poll interval, `max_replicas`, envelope lifetime and skew, readiness propagation, drain). Nothing here measures or recommends them; the values in tests are local inputs.

## Behavior the gateway must keep

- **Fail closed.** With no snapshot, or one older than S_ctl, authentication answers `503`. There is no empty-policy or allow-by-default fallback (C-07, FM-004).
- **Bounded revocation.** Revocation, suspension and binding changes reach every replica within S_ctl. Backends also refuse an envelope whose `A` is below the highest generation they have seen (C-06).
- **No retry of ambiguous writes.** A write is retried only if it provably never reached a backend. Anything else is reported to the client as ambiguous (`502 write_outcome_unknown`), because most writes have no idempotency key (RD-010). The MVP retries nothing: a connection that was never established gets `502 backend_unavailable`.
- **No sticky sessions.** Affinity would hide state bugs.
- **Readiness without `/metrics`.** The gateway's `/ready` reports the control-state generation and fails once the snapshot is older than S_ctl; `/metrics` is refused when memory bindings exist. It does not report the snapshot age itself.
- **Rate limits are shares.** Each replica enforces `limit / gateway_max_replicas`, never rounded up. Below the maximum replica count the tenant is under-served; temporary ACA over-replication during platform upgrades or a rollout that overlaps revisions can overshoot the tenant limit by the replica ratio (PRD 3.4.7). That overshoot is not qualified for production.

## Open owner decisions

These are undecided. No value below is measured or approved.

| ID | Question | Recommendation | Blocks |
|---|---|---|---|
| OD-002 | Writer fencing: per-batch epoch verification versus periodic recheck plus infrastructure fence plus gateway epoch stamp | Start with per-batch verification and measure (MEAS-006); implemented locally under the 2026-11-18 delegation, not an owner confirmation | Qualification of the gateway epoch check and fenced failover; MEAS-006 has only a local in-memory probe and no ACA run |
| OD-004 | Dashboard users and sessions through the gateway | Defer; later stateless signed session tokens | Dashboard sign-in through the gateway |
| OD-005 | S_ctl and the poll interval | Decide from MEAS-005; no default | The deployed values; the MVP takes them as required configuration |
| OD-006 | Gateway minimum and maximum replicas | Minimum 2 warm; maximum from MEAS-004 and quota check | The deployed `max_replicas` and ACA sizing; the MVP takes the maximum as required configuration. The minimum is an ACA setting with no runtime effect |

OD-012 (mode of `vlite-server` or a separate binary) was settled for the MVP as a mode of the same binary. That was an implementation decision by the coordinator under the 2026-11-17 continuation request, not an owner approval.

## Verification

- **TEST-012 / AC-011.** This document and API.md state that gateway and reader replicas add no write capacity. The gateway's `gateway_started` status event says the same; `tests/gateway_azurite.rs` asserts it.
- **TEST-002.** `server::gateway::` tests and `tests/api_route_inventory.py` check that the classifier matches the registered routes and the existing authorization rules.
- **TEST-004, TEST-010, TEST-014.** Library and in-process router tests in `src/server/principal_envelope.rs`, `src/server/control_state.rs`, `src/server/gateway_mode.rs` and `src/server/gateway_trust_tests.rs`.
- **TEST-003, TEST-004, TEST-005 (local).** `tests/gateway_azurite.rs` (ignored by default; needs a loopback Azurite) runs two gateway processes and two trusted backends against Azurite: equivalence with a standalone server, shared-pool and dedicated tenant isolation, header stripping, restart, revocation and suspension within S_ctl, outage and regression fail-closed, drain, the edge rate limit and a write cut after sending. These are local tier (b) runs of the [test plan](gateway-test-plan.md). The ACA tier (c) is NOT RUN, so they are not GATE-003 or MEAS evidence.
