# Shared gateway: scale-out is not writer HA

Design reference for the shared gateway in the staged Azure Container Apps (ACA) HA plan ([PRD](feature-aca-ha.prd.md), EPIC-004 ITEM-018). It states what the gateway is, what state it may hold, where its responsibility ends and what is built today. The PRD remains the authority; section references below point into it.

> **Status (2026-11-17).** No gateway runs anywhere. There is no gateway mode, flag or environment variable, and production is unchanged. The default deployment is still one standalone server. Only the backend serving modes are configurable; the control state, principal envelope, trusted backend authentication and operation classifier are library code with tests.

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
| Credential and policy snapshot, generation `A` | `credentials.json`, loaded at startup | Authoritative in shared control state; replicas cache a snapshot for at most S_ctl | Library: `vlite.control-state/1` object with CAS writes and a poller that fails closed after S_ctl (`src/server/control_state.rs`) |
| Tenant status (active, suspended, deleted) | `tenants.json` | Authoritative shared; cached like the policy snapshot | Library: carried in the same snapshot |
| Tenant routing record (deployment kind, endpoints, epoch `E`) | Not present | Authoritative shared; cached snapshot, generation `A` | Not built |
| Memory bindings | Startup-only file | Authoritative shared; enforced by backends | Not in the snapshot yet (FR-012) |
| Dashboard users and sessions | `users.json`; sessions in process memory | Removed from the gateway MVP | Deferred (OD-004) |
| Rate-limit buckets | One token bucket per process | Disposable per replica; static share of the tenant limit | Not built (ITEM-016) |
| Writer authority, epoch `E` | Not present | Authoritative shared; CAS-only | Not built (EPIC-006) |
| Route-classification table | Compiled code | Code, changed by release | Library: `src/server/gateway.rs` |
| Idempotency receipts | Memory API only | Backend-owned; gateway keeps none | Unchanged |
| Envelope signing key | Not present | Platform secret store, never in control state or logs (OD-009) | Library: startup bundle loader; platform delivery not run |

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
| Control-state snapshot and poller (ITEM-008, ITEM-011) | Library. Generation `A`, CAS create and update, stale cache refused with `503 control_state_stale`, regression refused. | No |
| Principal envelope (ITEM-009) | Library. Ed25519 `vle1` tokens in `x-vlite-principal`; audience, expiry, skew and minimum `A` checks; two-key rotation overlap. | No |
| Gateway-trusted backend authentication (ITEM-010) | Crate-private. Envelope-only authentication feeding the existing `permission`/`authorize`/guard path. | No |
| Operation classifier (ITEM-013) | Library. Classifies all 179 registered REST routes by method and path; unknown paths and methods are refused, never forwarded. | No |
| Routing, envelope minting, readiness by snapshot age, drain, rate-limit share (ITEM-014 to ITEM-016) | Not started | - |

## Behavior the gateway must keep

- **Fail closed.** With no snapshot, or one older than S_ctl, authentication answers `503`. There is no empty-policy or allow-by-default fallback (C-07, FM-004).
- **Bounded revocation.** Revocation, suspension and binding changes reach every replica within S_ctl. Backends also refuse an envelope whose `A` is below the highest generation they have seen (C-06).
- **No retry of ambiguous writes.** A write is retried only if it provably never reached a backend. Anything else is reported to the client as ambiguous, because most writes have no idempotency key (RD-010).
- **No sticky sessions.** Affinity would hide state bugs.
- **Readiness without `/metrics`.** The gateway needs its own readiness route reporting snapshot age; `/metrics` is refused when memory bindings exist.
- **Rate limits are shares.** Each replica enforces `limit / gateway_max_replicas`, never rounded up. Below the maximum replica count the tenant is under-served; temporary ACA over-replication during platform upgrades can overshoot the tenant limit (PRD 3.4.7).

## Open owner decisions

These are undecided. No value below is measured or approved.

| ID | Question | Recommendation | Blocks |
|---|---|---|---|
| OD-004 | Dashboard users and sessions through the gateway | Defer; later stateless signed session tokens | Dashboard sign-in through the gateway |
| OD-005 | S_ctl and the poll interval | Decide from MEAS-005; no default | ITEM-015 readiness, revocation bound |
| OD-006 | Gateway minimum and maximum replicas | Minimum 2 warm; maximum from MEAS-004 and quota check | ITEM-016 share, ACA sizing |
| OD-012 | Gateway as a `vlite-server` mode or a separate binary | Mode of the same binary | ITEM-014 wiring |

## Verification

- **TEST-012 / AC-011.** This document states that gateway and reader replicas add no write capacity. Status output must say the same once a gateway mode exists (ITEM-015).
- **TEST-002.** `server::gateway::` tests and `tests/api_route_inventory.py` check that the classifier matches the registered routes and the existing authorization rules.
- **TEST-004, TEST-010, TEST-014.** Library and in-process router tests in `src/server/principal_envelope.rs`, `src/server/control_state.rs` and `src/server/gateway_trust_tests.rs`. They are not end-to-end gateway or ACA tests.
- **TEST-003, TEST-005.** Multi-replica kill/add and drain tests need a running gateway (ITEM-017).
