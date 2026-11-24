---
goal: Design automatic dedicated-tenant writer provisioning and live reader lifecycle
version: null
date_created: 2026-11-22
last_updated: null
owner: VLite maintainers
tags: [design, architecture, aca, tenant-isolation, provisioning, migration]
---

# Introduction

**DESIGN ONLY — implementation and deployment are not authorized by this document.** This is the next focused design requested by the owner: “also, as discussed previously per tenant writers and auto wirter/reader provisioning design needs to pickup next.”

> **Superseding note (offline implementation):** the owner later authorized offline implementation of EPIC-001 (ITEM-001..003) — “go ahead with above fixes” and “Start above together in parallel” — then EPIC-002 (“please go ahead”). The owner's next direction, “what are you waiting for why you are not pickingup next work items?”, is the continuing authorization for the remaining non-migration **offline** implementation (code and local fixtures) as prerequisites permit. It is not a per-epic approval wait. It does not authorize Azure/ACA calls, live provisioning, deployment, qualification runs, gate acceptance, gate/policy changes, enabling auto-failover, push, merge or a new PR. Existing-tenant migration (EPIC-006 orchestration and existing-writer migration barriers) stays excluded by the owner's standing direction. See section 15 for the implementation notes.

> **Later authorization (2026-11-24, isolated Azure evaluation):** the owner asked, "you can deploy test version into azure to make it production qualification?" This authorizes a separate test deployment and evidence collection, not a production rollout or a qualification pass. Existing technical gates and acceptance criteria remain unchanged; migration, singleton changes, automatic failover enablement, source publication, merge and PR creation remain excluded. The initial read-only preflight was refused with `AADSTS530035` ("Access has been blocked by security defaults"); that attempt changed no Azure resources. The owner subsequently selected a different test subscription explicitly, and its access/provider checks succeeded. Use only that designated target, scoped on every Azure call; never silently substitute a subscription, rely on the CLI default or weaken access policy. The isolated full-profile gateway measurement below is now running; the original refusal remains historical, not the current blocker.

This companion extends [feature-aca-ha.prd.md](../feature-aca-ha.prd.md), specifically section 3.11 and EPIC-007/008. It does not reopen delivered primitives or rewrite their history. Its identifiers are local to this companion; `HA:ITEM-028`, for example, names an item in the parent. The parent’s introductory implementation snapshot is historical; the source inventory below distinguishes current code from missing orchestration.

The intended result is an administrator-managed onboarding operation that creates a tenant’s own database, one active writer, and independently scalable readers, then publishes gateway routing only after proof. Today ordinary tenants remain namespaces in a shared database; creating one does **not** provision infrastructure. Neither the current singleton nor its storage is adopted or changed implicitly.

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in RFC 2119.

**Cross-reference conventions**: `FR-`, `NFR-`, `FM-`, `AC-`, and `RD-` identify requirements, failures, acceptance criteria, and resolved directions. `ASM-` marks a planning assumption, `ALT-` an alternative, `OD-` an owner decision, `SRC-` repository evidence, and `AZ-` public documentation. No separate requirements document is necessary for this raw-purpose continuation.

## 1. Goals and Non-Goals

### In Scope

- **Goal 1:** Turn managed tenant creation into a durable, idempotent provisioning operation; route application writes automatically to that tenant’s writer through the existing gateway.
- **Goal 2:** Add/remove reader capacity while the writer remains unchanged. Bootstrap, catch-up, readiness, eligibility, draining, and abandoned-pin recovery are part of the lifecycle, not just a replica-count update.
- **Goal 3:** Design a bounded shared-file-to-dedicated migration, protecting other tenants, carrying supported data and policy, and enforcing the real rollback boundary.
- **Goal 4:** Reconcile cloud/control-state ambiguity and concurrent administrative operations without data loss, duplicate writers, premature routing, or automatic deletion of retained data.
- **Goal 5:** Make missing capabilities and evidence explicit blockers. Preserve the existing measurement policies, resource limits, qualification gates, and operator authorization boundaries.

### Out of Scope (deferred)

- Runtime implementation, Azure/infrastructure operations, production access/deployment, measurement execution, release, commit, push, and PR creation in this planning task.
- Changes to the singleton deploy scripts, existing singleton app/share, protected local edits, or `pool-briefs/`. No main-checkout access.
- Multi-writer tenants, sharding a tenant, synchronous replication, a new consensus/database/key-management service, or reimplementing the engine, archive, authority, and migration primitives.
- Qualified automatic failover: existing opt-in supervisor remains off by default and unqualified; GATE-005 is open. This design does not enable it.
- Arbitrary encrypted/format/profile migration, incremental tenant-log catch-up, zero-downtime migration, unlimited readers, or new numerical SLOs/resource defaults.
- Unrelated MemoryAI/tenant-memory-integration work. Existing VLite memory bindings and reservations still have to survive supported migration.

## 2. Terminology

| Term | Definition |
|------|------------|
| Managed tenant | Tenant whose provisioning lifecycle is owned by the proposed VLite control worker. Not every legacy tenant becomes managed automatically. |
| Shared tenant | Existing `t<tenant-id>__*` namespaces in one database. A `shared-pool` gateway route has no reader or authority epoch. |
| Dedicated tenant | Own database file, lineage, authority scope, writer app, and reader fleet. Stored tenant-qualified names remain qualified even when the file holds only one tenant. |
| One writer | At most one authorized active writer per tenant; during creation/failure there may be zero. ACA revision/replica settings are not the authority. |
| Reader fleet | Initially one internal ACA reader app per tenant with independent replicas and replica-private copies. The existing route has one optional reader endpoint, not a list of individually routed readers. |
| `A` | Monotonically increasing shared control-state generation. Authentication, tenant admission, and route policy use this generation. |
| `E` | Tenant writer-authority epoch. It is not `A` or an ACA revision identifier. |
| Operation | Persisted administrator intent, canonical request digest, immutable resource identity, lifecycle reservation, and recoverable step journal. |
| Activation proof | Private evidence binding exact resources/revisions, tenant, lineage, authority/head, policy/binding generations, and readiness to one operation. An HTTP 200 or ARM acceptance is insufficient. |
| Frozen watermark `W` | Sequence of the verified, closed source snapshot captured after selected-tenant mutation drain. It is not a live incremental replication cursor. |
| Release | Irreversible end of the automatic route-back window. It is distinct from deleting source data or garbage-collecting cloud resources. |

## 3. Solution Architecture

### 3.1 Source-grounded current state

Evidence is from the isolated worktree supplied as HEAD `303c3cf34de5dd47e2cdca6431a5c23ca2afaee2`; line ranges refer to this source, not older PRD line references. Runtime facts were inspected directly; the required bounded research agent inspected the separate HA scaffold and Microsoft documentation. No tests or deployments are inferred from historical “Done” labels.

| ID | Evidence and current capability | Boundary for this design |
|----|--------------------------------|--------------------------|
| SRC-001 | `src/server/tenants.rs:311-360`, `Create`/`create`: accepts name/vector limit, allocates ID, audits, commits tenant metadata, returns 201. | No deployment request, infrastructure call, route publication, or durable provisioning job. Preserve this legacy API by default. |
| SRC-002 | `src/server/control_state.rs:1-25,35-135,228-241,645-720`: `Snapshot`, `Route`, `Objects`; strict schema, bounded object, Blob CAS, indeterminate outcome. `gateway_mode.rs:1198-1215` documents publish/update/reimport CLI. | Route supports `shared-pool` and `dedicated`, one writer and optional reader endpoint. Dedicated normally requires `E`; `authority_mode: off` permits only writer-only, epochless dedicated routes. No provisioning journal or live lifecycle reservation. |
| SRC-003 | `src/server/gateway.rs:19-33`; `gateway_mode.rs:410-431,548-625,660-671`: authenticated routing, reader eligibility and writer fallback, latest usable same-lineage epoch check before streaming. | Reuse classification/trust/routing. Unknown write outcomes are not safe to replay. Adding gateway replicas never adds writer capacity. |
| SRC-004 | `src/server/gateway_trust.rs:1-20,225-278`; `src/server.rs:2533-2571`: backend independently checks envelope/audience/lineage and current tenant admission; readiness tests DB, stale reader/control state, drain, writer fence; writer-ready body can include mode/epoch. | No migration-phase admission barrier. `/ready` is not a complete identity/archive proof. Trusted backend has no admin role for `/v1/admin/serve-status` (`docs/HA-ACA.md:534-545`). |
| SRC-005 | `src/tenant_log.rs:155-297,310-341,351-409,416-558`, `project`, `check_profile`, `Ownership::select`, `inventory`; crate-private module at `src/lib.rs:26-27`. | Actual projector is here, **not** `logops.rs`. Closed ordinary V5 only; new lineage, same qualified names/IDs and retained revisions; omitted suffix commits become no-ops preserving sequence. Verifies logical inventory. No public migration CLI or delta/catch-up interface. |
| SRC-006 | `src/server/tenant_migration.rs:1-12,68-247,253-379,427-486`: selection/target validation/rekey/user import. | Carries selected users, credential inventory, bindings and orphans; rejects unknown/symlinked sidecars, open selected-tenant imports, cross-tenant bindings, corrupt orphans, stale grants. Sidecar/import observations come from an unwired offline coordinator, not an authoritative live scan. |
| SRC-007 | `src/server/control_state.rs:181-254,414-619`; `tenant_migration.rs:498-650`; `credentials.rs:473-524,652-707`. | Existing prepare/freeze/verify/cutover/rollback/release are library transitions. Cutover atomically switches route + rekeyed credential view; preserves restrictions. Migration validation currently requires target `authority_mode: off`. Freeze/validation flags do **not** freeze a live server. Rollback’s target-write input is caller-supplied, not a durable mutation guard. Exact-successor reconciliation cannot resolve every later-generation ambiguity. |
| SRC-008 | `src/server/authority.rs:257-395`, `create`/`claim`/`advance_head`/`prepare_transfer`; `writer_authority.rs:181-331,339-444`; `writer_fence.rs:203-299,333-415`. | Authority creation, CAS ownership/head, exact-base recovery and serving fence exist. Startup refuses an already claimed authority; restart is not automatic failover. No zero zombie-acknowledgement window is established. |
| SRC-009 | `src/server/reader_apply/follower.rs:23-95,186-355,450-623`; `reader_apply.rs:60-102,141-153`; `serve_mode.rs:109-130`; `cloud_backup.rs:1241-1355,1614-1706`. | Independent two-slot read-only follower; pins, verified HA head/base, checkpoint follow, staleness, generation swap and fail-closed reseed signals exist. Reader pin operations write set coordination metadata; “read-only reader” does not mean read-only Blob permission. |
| SRC-010 | `src/server/cloud_backup.rs:37-54,238-255`; `docs/HA-ACA.md:478-578`. | Existing caps include 16 whole-set pins and 1 GiB cloud DB envelope, not a tenant/reader sizing promise. Re-enrollment resets freshness on a frozen base; exclude it until transfer completes. Crashed-replica forced-unpin has no supported trusted-mode operational path. |
| SRC-011 | `src/server/writer_authority.rs:413-425`; `src/server/ops.rs:252-267`; parent `HA:ITEM-041`; `docs/HA-ACA.md:15-47`. | First archive-base attempt exists, but failure does not by itself prevent all writer serving. Provisioner must withhold the route until committed-head proof. Azure Files power-loss qualification and MEAS-005/006 real propagation/fencing evidence are missing. |
| SRC-012 | `src/server/auto_failover.rs:1-4,33-40,405-521`: policy, detector, fence/report/transfer, optional promotion action; CLI and admin policy surface later in same file. | Opt-in unqualified supervisor exists, not absent. It is not a tenant provisioner, durable lifecycle scheduler, gateway cutover transaction, or proof of end-to-end recovery. |
| SRC-013 | `deploy/azure-container-apps/ha/deploy.ps1:319-326,411-512,723-728`: unconditional qualification refusal; checks existing owned environment/subnet/ZRS account/share/binding/identities/containers. | Prerequisite checking is not prerequisite provisioning or singleton adoption. Effective RBAC, capacity, mount durability and authority lifecycle remain unproven. `EnableHa` is not a bypass. |
| SRC-014 | `deploy/azure-container-apps/ha/deploy.ps1:328-398,524-582,611-707`; `ha/templates.ps1:291-292,332-360,392-411,453-470,503`. | Archive/authority secret projections, writer min=max=1, independent EmptyDir readers, revision-stop/inventory skeleton, readiness checks exist behind the gate. Local lock/random revision suffixes are not distributed idempotent orchestration. Writer update has a separate qualification refusal. |
| SRC-015 | Parent PRD section 3.11 and `HA:EPIC-007/008`, lines 264-271,947-964; `docs/HA-ACA.md:1-24`. | Extend completed migration state/projector and guarded templates. Historical broad “rollback until release” prose is narrowed by SRC-007: no target writes without reconciliation. |

**Supported migration profile is deliberately narrow.** `check_profile` rejects non-V5, encryption, timestamps, precision, compact layout, unavailable temporal support, and unknown flags. The frame allowlist rejects unknown operations; incomplete tails and cross-tenant graph/collection references refuse. Inventory covers selected collections/settings/records/vectors, graphs and retained import markers, not a claim to every current/future database feature. Preflight MUST validate the actual whole source image, build features, sidecars and selected tenant. A feature not proven by this allowlist and parity tests is unsupported, never silently omitted. A selected tenant without an unsupported feature may still be blocked by that feature elsewhere in the shared log.

### 3.2 Architecture graph

All control-worker/lifecycle extensions below are **proposed**, not current runtime. Use the existing VLite binary/admin/control-state facilities and Blob object-store abstraction. A VLite control worker is an additional execution role with cloud privileges, **not a new external backing service**. It MUST NOT run privileged reconciliation inside public gateway replicas.

| Node ID | Component | Scope / state ownership |
|---------|-----------|-------------------------|
| NODE-001 | Application / administrator onboarding client | Existing tenant credentials for data; separate existing administrator authorization for lifecycle. |
| NODE-002 | Stateless gateway | Shared data-plane edge; disposable control cache, no DB or ARM permission. |
| NODE-003 | VLite admin/control worker | Proposed lifecycle API/CLI and reconciler; persisted state is in Blob, not process memory. |
| NODE-004 | Blob global control snapshot | Authoritative `A`, tenant restrictions/credentials/routes, proposed compact lifecycle reservation and activation receipt. |
| NODE-005 | Blob operation records | Proposed private per-operation journal, idempotency binding, resource/proof references, retained-data inventory. |
| NODE-006 | ACA/ARM resource provider | Apps/revisions/identities/storage bindings and asynchronous operations; observed state, not application authority. |
| NODE-007 | Tenant writer app + private durable DB | One active writer for this tenant; own lineage; never shares writer DB/share with another tenant or a reader. |
| NODE-008 | Tenant archive + authority containers | Existing HA immutable archive/head and epoch authority; separately scoped from global control/journal. |
| NODE-009 | Tenant reader app / replicas | Internal endpoint; each replica owns disposable two-slot local copies, not the writer mount. |
| NODE-010 | Existing shared backend + DB | Compatibility path for unmigrated tenants; not provisioner-owned/adopted storage. |
| NODE-011 | Tenant-scoped backend control projection | Proposed restricted view of NODE-004 for dedicated trusted backends; no other tenant credentials or routes. |

| Edge ID | From | To | Contract |
|---------|------|----|----------|
| EDGE-001 | NODE-001 | NODE-003 | Idempotent authenticated lifecycle intent; 202/status rather than deployment completion. |
| EDGE-002 | NODE-003 | NODE-004 | CAS reserve/cancel/activate; preserve other tenants and restrictions, monotonic `A`. |
| EDGE-003 | NODE-003 | NODE-005 | Create-only intent, conditional journal updates, exact resource/proof readback. |
| EDGE-004 | NODE-003 | NODE-006 | Preflight, deterministic desired resource operations, terminal-result/readiness reconciliation. |
| EDGE-005 | NODE-001 | NODE-002 | Existing authenticated application REST traffic; no app-selected writer URL. |
| EDGE-006 | NODE-004 | NODE-002 | Existing fresh-cache policy/directory feed. |
| EDGE-007 | NODE-002 | NODE-007 | All writes and default/freshness-required reads; signed envelope; checked `E`. |
| EDGE-008 | NODE-002 | NODE-009 | Only opted-in eligible reads; one writer fallback, never write replay. |
| EDGE-009 | NODE-007 | NODE-008 | Authority claim/check + immutable publish/head CAS. |
| EDGE-010 | NODE-008 | NODE-009 | Pin, verified baseline/checkpoints, read-only generation swap; pin updates return to coordination metadata. |
| EDGE-011 | NODE-002 | NODE-010 | Existing shared-pool routing during coexistence, no synthetic epoch/readers. |
| EDGE-012 | NODE-003 | NODE-011 | Project only this tenant from confirmed `A`; never include global credential hashes. |
| EDGE-013 | NODE-011 | NODE-007 / NODE-009 | Fresh admission/audience/lineage/binding policy; fail closed when stale. |
| EDGE-014 | NODE-010 | NODE-003 | Authorized, frozen-tenant consistent private snapshot + policy inventory; never expose other tenant data to the target. |

### 3.3 Durable intent and concurrency contract

The following is a proposed wire/schema contract, not example configuration to execute. Field sets MUST reject unknown fields; bounded sizes and retry budgets MUST reuse the repository’s validation patterns. Production bounds require approved deployment profiles, not invented defaults.

| Record | Required fields / semantics |
|--------|-----------------------------|
| Operation intent | `schema`, `operation_id`, administrator identity reference, `idempotency_key_digest`, `request_digest`, `kind` (`create`, `set_readers`, `migrate`, `retire`), immutable `tenant_id`, tagged desired payload defined below, immutable profile-content references + digests, source identity if any, target resource IDs, created time. No plaintext tokens/passwords. |
| Journal | `operation_generation`, phase, attempt identity, desired resource/configuration digest, observed ARM operation/revision IDs, uncertain-result flag, stage result/proof references, current blocker code, retained resources, credential-delivery acknowledgement reference, cleanup ownership. Times are observations, not leases that elect a writer. |
| Global lifecycle reservation | Compact `{tenant_id, operation_id, kind, expected_route_identity, lifecycle_generation, admission_state, cancellation_requested, activation_receipt}` in the same CAS object as route/tenant policy. Large journals MUST NOT grow the existing 1 MiB global snapshot. |
| Activation receipt | Operation ID/digest, published `A`, tenant, route digest, lineage, `E`, bound backend configuration/proof digest. Persisted atomically with route and credential view; journal success is derived by readback. |
| Backend projection | Tenant-only policy, audience/lineage, lifecycle admission and binding generation, source `A`, source snapshot digest, monotonic witness revision, source-read start time and absolute freshness deadline under section 3.4. No plaintext or hashed client credentials, no other tenant inventory. Publishing a projection cannot create a route or advance source `A`. |

**Recoverable desired payloads:** Each intent MUST contain the bounded tagged payload itself or a digest-verified immutable Blob reference to its complete canonical bytes. A digest alone is not a recoverable request. Referenced content MUST be persisted create-only and verified before accepting the intent; deletion of a live profile catalog entry cannot delete this operation's retained version.

| Kind | Complete desired payload (proposed) |
|------|-------------------------------------|
| `create` | Tenant `name` and nullable `max_vectors`; immutable writer deployment profile; initialization options accepted by the existing engine/profile; target resource identities and secret-generation references. Creation requests no readers; a later reader operation owns that change. |
| `set_readers` | Tagged fleet `absent` or `present`. `present` includes explicit desired min/max replicas, immutable reader profile and scaling-rule content, and desired read-routing policy; `absent` has no replica/rule fields and requires writer-only routing. No values are defaulted by this design. |
| `migrate` | Expected source tenant/route/lineage, immutable dedicated target profile and resource identities, supported-profile selection and verification policy, credential-onboarding policy reference. Frozen image/inventory/watermark digests are subsequent write-once journal outputs, not fabricated request inputs. |
| `retire` | Expected active route/activation receipt, reason, explicit data disposition `retain`. Data deletion is not an accepted retirement payload. |

Profiles retain the exact nonsecret resource/identity/storage/image/configuration inputs and immutable secret-version references needed to reproduce desired state. Secrets remain in their private store/projection, not the payload. A missing/corrupt payload, retained profile or secret version yields `operation_input_unavailable`; resume MUST NOT resolve the latest mutable profile or infer missing values. Tests must recover with no original client request and after the profile catalog changes or disappears.

1. Authenticate the administrator and validate the entire canonical intent. Atomically create the idempotency binding and intent as one object keyed by principal/key digest; allocate and persist the tenant/operation identity **before** tenant metadata or cloud calls. Identical retry returns the same operation. Different content at the same key is `409 operation_request_conflict`. The key is not a bearer credential.
2. CAS reserve the tenant lifecycle slot in NODE-004. For a new tenant, reservation and non-routable tenant policy are created together; no live directory route exists. An orphan intent with no reservation is safe to reconcile or cancel without infrastructure. Existing metadata APIs need an idempotent “create this persisted ID” adapter; do not call today’s random-ID `create` twice.
3. Only the reserved operation can prepare route activation. Concurrent create/migrate/reader-change/retire/failover must serialize for that tenant. Unrelated tenant changes may advance `A`; a new CAS attempt revalidates restrictions and the reservation against the latest snapshot, rather than replacing a stale whole directory.
4. Before a cloud side effect, persist its exact request identity/digest and deterministic target resource/revision identity. After a timeout, read the operation URL and exact resource/revision/configuration, not “some app exists.” Honor ARM terminal status and retry guidance. Never issue a new randomly named rollout to conceal an uncertain result.
5. Blob CAS fences journal/route writes, **not a paused process’s ARM calls**. A worker may only execute the immutable currently reserved step; no destructive compensation or superseding revision may begin while any previous call/worker can still act. Takeover requires completion/reconciliation of recorded cloud operations and proof the previous actuator can no longer dispatch. If ARM cannot enforce the necessary condition, stop `actuator_fence_unproven`; a wall-clock lease is not proof. This is an explicit feasibility gate before cloud execution.
6. Cancel and activate contend on the same lifecycle reservation CAS. If cancel wins, no route can activate. If activate wins, cancel returns `retirement_required`; the administrator must submit a separate guarded retirement intent, not deletion. After an indeterminate CAS, the activation receipt is authoritative even if the worker died before updating its journal. Later unrelated `A` changes cannot erase the receipt.
7. Legacy CLI directory replacement, admin deletion, authority transfer, and auto-failover MUST NOT bypass reservations for managed tenants. Until all mutation paths participate or are operationally excluded and proven, managed lifecycle operations are blocked. Keep auto-failover disabled and confirm no in-flight promotion; disabling policy alone does not undo a transfer past its cutoff (SRC-012).
8. Upgrade consumers before publishing new strict-schema fields. Old readers reject unknown fields; never assume mixed-version compatibility. Freeze lifecycle writes during rollback to a binary that cannot read their schema; do not downgrade `A` or erase operations to make an old binary accept them.

**Terminal states and reservation release:** `blocked` and `reconciling` are nonterminal; they retain the reservation and admit only safe observation/resume/cancel actions. Create/reader operations become `succeeded` only after their active/fleet receipt and final serving proof; the tenant may remain active after its operation terminates. Migration holds the reservation through `Validating`, then finishes `released` only after explicit release and target write-enable reconciliation, or `rolled_back` after source route/admission restoration. Retirement progresses `retiring → draining → stopped → retired`, retaining data throughout. A pre-activation cancellation finishes `cancelled` only after all side effects are settled; otherwise it remains `cleanup_pending`.

Every terminal transition MUST first seal the exact terminal/activation receipt into immutable operation history and verify it. Only then may a CAS release the compact global reservation, preserving a reference to that receipt. A successor operation may replace the slot only after this sealing proof; it cannot erase the prior operation's outcome. Unknown cloud calls or a possibly active previous actuator prevent terminal release. This bounds global snapshot growth without losing late-retry evidence. A retirement requested after activation is a new idempotent intent against the active receipt, not a reinterpretation of the old create/cancel request.

**Deterministic invariants (proposed implementation obligations):**

| ID | Invariant | Oracle |
|----|-----------|--------|
| INV-001 | For each tenant, authorized active writer count is at most one; a reader action never changes writer identity, authority or storage. | TEST-003/005 |
| INV-002 | A live dedicated route implies a matching reserved operation, verified seed/head/identity/policy proof and atomic activation receipt. A ready process without proof is insufficient. | TEST-004 |
| INV-003 | One administrator/idempotency key binds one immutable complete intent; a different digest cannot create side effects. | TEST-001 |
| INV-004 | Unknown cloud/CAS outcome cannot authorize a new conflicting action, activation, destructive cleanup or unproven actuator takeover. | TEST-002/003 |
| INV-005 | Shared-source tenant mutations stop before final `W`; target mutations remain disabled until irreversible release. Possible target mutation forbids automatic route-back. | TEST-006/008 |
| INV-006 | Successful local projection reads cannot extend its authoritative source freshness deadline; stale/unknown policy refuses admission. | TEST-004 |
| INV-007 | Terminal receipt is durably sealed before lifecycle-slot reuse; retirement/cancel does not delete retained DB/archive or adopt shared resources. | TEST-010 |
| INV-008 | No operation advances on absent qualification, budget, permission, profile or onboarding proof; public errors/status do not leak secrets. | TEST-004/007/009/010 |

### 3.4 Create → provision → activate

| Step | Proposed durable state | Required action and proof before advancing |
|------|------------------------|--------------------------------------------|
| 1 | `accepted` | Persist intent/idempotency identity; reserve lifecycle/non-routable tenant. Return operation handle only. Legacy namespace-only creation remains unchanged. |
| 2 | `preflight` | Validate approved image/profile, resource ownership and name conflicts, application/cores/subnet/storage/pin budgets including overlap, supported file format, network topology, available effective permissions and required qualification evidence. Missing proof yields `blocked`, not success. |
| 3 | `provisioning` | Create or verify only operation-owned prerequisites and apps through deterministic ARM steps. Record created vs pre-existing references. Never silently adopt the singleton or its share/environment identity. Prerequisite provisioning is missing from the existing HA scaffold. |
| 4 | `seeding` | Create a private new database with recorded nonzero lineage using existing engine primitives in an isolated initialization step; for migration use only the verified tenant projection. Persist seed digest/lineage before starting a writer. No running reader/writer shares the init mount. Reconcile an uncertain seed without overwrite or rekey. |
| 5 | `establishing_authority` | Create authority only if absent, bound to installation/scope/seed lineage; otherwise verify exact identity. Start the writer with existing trust/archive/authority configuration. Startup must claim the expected record. A claimed record after a crash requires the existing fenced recovery protocol, not a reset or re-run of “create.” |
| 6 | `verifying` | Require an authority-committed baseline/head matching lineage/installation/scope/`E`; independently verify archive references and writer configuration/readiness. Check internal ingress, all revisions, backend control projection, binding generation and policy. First-base failure keeps the route absent even if the process serves internally. |
| 7 | `onboarding` | Issue a candidate tenant credential once through existing credential primitives in the admin/control role, persist its private hash/view and delivery-generation reference, and complete the explicit private delivery acknowledgement. A lost delivery blocks activation until replacement/revocation is confirmed. Do not install an admin directory on the trusted writer. |
| 8 | `activating` | CAS route, acknowledged tenant credential view and activation receipt with the live reservation/current restrictions; initial route is `writer_only`. Never use `authority_mode: off` as a shortcut for this HA target. |
| 9 | `active` | Read back activation receipt; confirm serving backends and gateway observe the required policy generation; seal success and release the reservation. Client writes now follow the gateway directory automatically. Readers are a separate guarded operation. |

**Pre-route verification deadlock:** today a trusted backend requires a matching active directory record to accept a scoped envelope (SRC-004). Publishing the live route to test it would violate step 6. The proposed lifecycle admission protocol MUST support an operation-bound, read-only validation audience/credential on a **pending**, non-public route, usable only by the control worker, with existing permission checks. It MUST NOT grant admin access, admit application writes, or appear in gateway data routing. A private sanitized proof channel must report identity/policy/archive readiness without exposing memory positions publicly. This capability is missing and is a prerequisite, not an instruction to disable trust or temporarily expose a backend.

The control worker publishes tenant-only backend projections, including pending admission, from confirmed global generations. Projection publication and route CAS are not one Blob transaction: persist proof of the required projection generation, withhold activation until it is readable, and fail closed on stale/missing projections. After route CAS, lagging projections may cause temporary refusals. Existing bounded stale-policy behavior is not an instantaneous revocation guarantee; no removed route/grant may remain usable beyond the existing approved `S_ctl` contract. Migration additionally requires explicit barrier/drain proof before relying on revocation.

**Upstream freshness witness (required, proposed):** Existing `Poller::poll` semantics refresh local age even on a successful unchanged-ETag read (`src/server/control_state.rs:808-843`). Reusing that clock for a derived projection would keep an abandoned view fresh forever. The projection therefore needs an independent upstream deadline:

- The publisher records source-read start time **before** a successful authoritative Blob read, binds the confirmed `A`/digest and monotonic witness revision, and sets the deadline to that start time plus the existing configured `S_ctl`. Request latency and projection propagation consume that same budget; they do not start another `S_ctl`.
- Only a new successful source read may issue a new witness, even if `A` is unchanged. Conditional publication rejects source-generation, read-start-time or witness regression; resumed/old workers may not overwrite a newer witness. A publisher restart cannot refresh a witness from its old cache.
- Backend permission decisions require both a healthy local projection cache and an unexpired upstream witness. An unchanged projection ETag never renews upstream validity. On receipt, remaining validity is `deadline - local_wall_time - approved_pairwise_clock_uncertainty`; nonpositive means refuse. Anchor that remaining duration to a monotonic timer, never extending it on rereads of the same witness.
- The clock-uncertainty allowance must be covered by existing approved timing/skew policy and qualified evidence; this PRD supplies no new value or relaxed bound. Detect wall-clock regression and refuse until revalidated. Across restart, acceptance requires a clock whose error is proven within that allowance; absent trustworthy clock/bootstrap proof, refuse `projection_freshness_unproven`.
- If the publisher stops while global policy changes and projection Blob reads remain healthy, the backend must refuse by the original witness deadline. Test suspended/removed tenants, stalled/slow reads, restart, replayed witnesses and clock rollback. If the extra hop cannot meet existing `S_ctl`, activation is blocked; transport polling success is never substituted for authoritative freshness.

### 3.5 Live reader add/remove

| Sequence | Behavior |
|----------|----------|
| Add / increase | Persist `set_readers` intent with desired profile/capacity and reserve tenant; verify writer/authority unchanged and no transfer. Preflight total pins, two copies + download/apply/index headroom per replica, environment quota and peak overlap. Create/update only the reader app. Existing writer app/configuration/authority MUST remain byte-for-byte unchanged by this operation. |
| Bootstrap | Each new replica gets a unique lifetime identity, tenant-scoped targets, trust/bindings and private EmptyDir slots; it enrolls through `ReaderApply`, pins the authority-named set, verifies baseline and committed checkpoints, and opens read-only. Replica restart means reseed, not mounting the writer. |
| Catch up / eligibility | Stay out of ingress eligibility until role/lineage/binding generation and current authority/head epoch match, staleness is within the configured qualified bound, and no recovery-base transition is active. Response checks remain enabled after activation. Applied age measures archive-head confirmation age, not commit lag or read-your-writes. Withheld memory positions remain private. |
| Publish | For the first reader endpoint or policy change, CAS the existing route’s reader endpoint/read policy without changing writer identity or `E`. For same-app scaling, endpoint need not change; only eligible replicas may receive ACA traffic. Keep `writer_only` until reader qualification passes; gateway has one reader endpoint, not a new per-replica load balancer. |
| Decrease / remove | Record desired fleet first. Remove eligibility, then propagate draining and wait for in-flight reads and termination using approved, measured settings. For removing the entire fleet, CAS `writer_only` and remove the endpoint before stopping it. If ACA cannot target a subset for safe pre-drain, temporarily route the tenant’s reads to the writer, reconcile the reduced fleet, then re-enable qualified reader routing. |
| Clean up | On clean stop release that replica’s operation pin. After crash, prove the specific replica/revision is stopped before forced-unpin; then remove only its disposable copies. Unknown stop state retains pins/data and reports `cleanup_pending`. Unique replica identity must not release another live replica’s pin. |
| Failure | Missing head, full pins, quota, incompatible bindings, stale/frozen base or reseed-required yields blocked/degraded fleet, not a new writer. Failed/replaced baseline can trigger a durable replacement/re-enrollment operation; existing follower does not automatically reseed itself. |

Initially “add readers on the fly” means administrator-requested reader capacity through this lifecycle. Existing ACA HTTP scaling can operate within an approved reader envelope only after every replica startup/termination is proven safe. Autonomous demand/cost policy selection and a fleet of separate reader apps are deferred (OD-003). A configured replica maximum is not guaranteed capacity and platform maintenance can exceed it (AZ-001).

### 3.6 Shared-file → dedicated migration

**Compatibility:** unmigrated tenants continue using their original shared backend and policy. Shared-pool records remain epochless/writer-only. A gateway-trusted shared backend is an opt-in compatibility mode, not permission to turn the live singleton into one. If a production source still allows direct bearer/session/gRPC/background mutation paths that the migration barrier cannot control, migration is blocked. No automatic singleton redeploy, global writer shutdown, or storage conversion is permitted.

| Step | Existing primitive reused | Missing integration / required proof |
|------|--------------------------|--------------------------------------|
| Prepare | `tenant_migration::select`, `TenantState::validate`, existing `Migration::Prepared`. | Authoritative inventory of actual source format/features, sidecars/imports, tenant data, users, grants, bindings/orphans; source route/lineage and target identity reserved. Reject unsupported content before mutating either source or routing. Encrypted/timestamped etc. are blockers, not conversion tasks here. |
| Freeze + drain selected tenant | Existing `freeze` phase transition. | Implement durable per-tenant mutation admission at gateway **and** source backend before data capture; include in-flight/queued imports, memory/graph/batch writes, sessions, gRPC, admin writes and background mutators. Observe required generations or exclude unreachable/stale processes. Drain selected-tenant work and persist evidence. Phase metadata alone proves nothing. Other tenants are not suspended and continue normal progress, subject to existing shared DB locking. |
| Capture watermark | Existing server snapshot facility (`backup_policy.rs:1846` uses `ops::snapshot`) then closed-log `tenant_log::project`. | Capture a coherent private image after drain, with frozen `W` and consistent policy/binding inventory. Full shared image remains control-operator-only; only the selected projection reaches target storage. Do not run the offline exclusive-lock projector against a live mounted source file. |
| Project / verify | New lineage, unchanged qualified names/IDs and selected inventory at `W`; `TenantState::for_target` checks grants. | Compare resource digests/counts/revisions/settings, graphs/references/import markers, reservations/orphans, users and permission outcomes. Verify absence of other tenants. Do not strip `t<id>__` prefixes. No incremental projector/catch-up exists: first slice freezes before the final snapshot. An earlier copy is disposable preview only; rebuild final target from the frozen snapshot rather than append an unsupported tenant delta. |
| Rekey / provision | `rekey_for_migration`, paired source/target credential views, sanitized `import_users`, authority/archive startup primitives. | One-time credential onboarding and user-password/reset delivery through private authorized channels; persist hashes/references only. Verify target identity and supported source/target permissions before cutover. Extend migration validation deliberately for fenced targets: existing `Migration::valid` requires `authority_mode: off`; do not weaken that legacy rule silently or route epochless then “add HA.” |
| Target verification | Existing `target_verified` and independent projected-inventory check. | Target remains application-write-disabled. Validate new authority/head, pending-route proof, private data reads, policy/bindings and credentials. No direct test writes that change `W` or invalidate route-back evidence. |
| CAS cutover | Existing `cutover` switches route and rekeyed credential views atomically into `Validating`. | Include lifecycle reservation/proof/receipt and required fenced target identity in the transition. Keep a durable source tombstone/barrier so stale gateways/direct paths cannot resume source writes. Both source and target are read-only for the migrating tenant’s validation window; other tenants remain live. |
| Validate / release | Existing `rollback`, `release`, monotonic `A` and restriction-preserving credential views. | Confirm application credential delivery/read parity, then explicit irreversible release closes route-back window **before** enabling target mutations. Maintain source tenant write refusal after release. Retain source namespace/snapshot according to approved retention; release does not delete anything. |

**Rollback boundary (stronger orchestration guard, same conservative primitive):**

- Before activation, cancel/rollback discards only unactivated owned compute when safe; source unfreeze follows proof the target never admitted tenant writes.
- During `Validating`, CAS back to the shared route with the paired fresh **source-lineage** credential view only when durable admission/drain evidence proves no target mutation. Retain revocations, expiries, suspension and tombstones; never restore old secrets or a stale global snapshot. The fresh credential tokens can be the same across source/target lineage views (SRC-007).
- Any target mutation that committed, was acknowledged, or has an uncertain outcome blocks automatic route-back with `rollback_requires_reconciliation`. A caller-supplied `false` or absent log entry is not proof. Unknown is treated as possible divergence.
- After explicit `Released`, existing code returns `migration_source_released`. Any return to the shared deployment is a separately planned reverse migration/reconciliation, not “flip the route back.”
- Do not delete the shared DB/share to clean up one tenant. Removing retained namespace data is a separate administrator-approved, scoped operation; validate other tenants before/after it.

### 3.7 Status and administrator workflow

Proposed initial surface: VLite admin/control CLI and matching admin-only lifecycle endpoints, **not** gateway tenant-data routes. `POST /v1/admin/tenant-deployments` creates managed tenant intent; `POST /v1/admin/tenant-deployments/{tenant}/readers` changes desired reader capacity; `POST /v1/admin/tenant-deployments/{tenant}/migrations` starts a reserved shared-to-dedicated migration; `POST /v1/admin/tenant-deployments/{tenant}/retire` requests a separate retirement intent; `GET /v1/admin/tenant-deployment-operations/{operation}` returns status. Action endpoints are defined below. These routes do not exist today and must enter the repository route/auth/OpenAPI inventories when implemented.

Every action uses `POST /v1/admin/tenant-deployment-operations/{operation}/{action}` with administrator authorization, action idempotency key, expected operation/stage generation, expected tenant lifecycle generation and, after activation, expected receipt digest. Persist accepted action intent before applying it. The same key/content returns its original result even after state advances; conflicting content or stale unaccepted action returns 409. A CLI `tenant-deployment <action>` maps to the same contract, not a separate transition implementation.

| Action | Eligible state and required input | Effect / refusal |
|--------|-----------------------------------|------------------|
| `resume` | Nonterminal blocked/reconciling operation; exact expected stage | Reconcile stored desired input/proofs only. Never supplies implicit release, credential delivery, rollback or data deletion approval. |
| `cancel` | Nonterminal pre-activation operation; expected stage | Request guarded cancellation. After activation, refuse with `retirement_required` and require a new retirement intent; never reinterpret cancel as destructive rollback. |
| `acknowledge-delivery` | Create `onboarding` or migration pre-cutover/`Validating`; exact credential delivery generation and private delivery receipt | Persist delivery acknowledgement without secret bytes. Earlier/lost credential generations cannot satisfy activation/release. |
| `replace-credentials` | Create/migration blocked on delivery, before write-enable; explicit acknowledgement to revoke the previous candidate generation | Persist replacement intent, issue once, revoke/retire prior views, then require delivery acknowledgement for the replacement. In `Validating`, preserve paired source/target views and current restrictions; no concurrent release/rollback. Active-tenant credential rotation outside this window uses existing credential administration, not this action. |
| `rollback` | Migration Frozen/TargetVerified/Validating, source retained, expected source/target identity | Require durable no-target-mutation/drain proof, not an administrator Boolean. CAS source route and paired fresh view when necessary; unknown writes or Released refuses. |
| `release` | Migration `Validating`; acknowledged delivery, read-only validation proof and exact cutover receipt; `ack_irreversible: true` | Persist explicit irreversible decision, CAS Released, then reconcile target write-enable. Lost response is resolved by receipt; retry cannot repeat or undo release. No target writes enabled before the release decision. |

The application’s onboarding flow invokes this control surface with existing administrator authority; its data requests continue to use ordinary tenant credentials through gateway app #1. Namespace-only `POST /v1/admin/tenants` remains wire-compatible and performs no cloud work. Whether the UI defaults to the new managed workflow is OD-001.

| Outcome | Required status semantics |
|---------|---------------------------|
| Accepted/running | `202`, operation handle, durable phase, desired vs observed resources and next verification step. No “tenant ready” until activation receipt and serving proof. |
| Blocked | Stable code, missing capability/proof, retryability, next administrator action. Examples: `qualification_missing`, `profile_unsupported`, `quota_unproven`, `identity_unready`, `actuator_fence_unproven`, `migration_admission_unproven`. |
| Ambiguous | `reconciling` / `cloud_outcome_unknown` / `control_outcome_unknown`; same operation resumes readback. No fresh tenant/revision or blind delete. |
| Conflict | `409` for idempotency mismatch or concurrent lifecycle; no changes on refusal. |
| Partial failure | Record owned compute, retained data/pins, route/admission state and safe resume/cancel path. Failed provisioning is not absence of resources. |
| Active/degraded | Route receipt and observed generation; separate writer/reader status. Reader failure does not report writer failover or data loss without evidence. |
| Cancelled / cleanup pending | Cancellation is durable; resource reclamation is independently reported. Retained backups/data and unresolved ARM actions remain visible. |

Status/logs MUST exclude tokens, hashes, passwords, raw cloud stderr, envelope keys and payloads. Detailed resource/proof references are administrator-private; public tenant errors preserve existing isolation semantics.

## 4. Requirements

**Summary**: The owner approved the dedicated deployment direction, not its implementation details, resource sizes, automatic failover, or production rollout.

**Items**:

| ID | Requirement | Origin |
|----|-------------|--------|
| FR-001 | Every newly managed dedicated tenant MUST have its own DB/lineage/authority and at most one active writer; all application writes MUST route through the gateway to that writer. | Owner: “Every tenant gets individual VLite deployments”; “every Tenant has 1 writer … write gets automateally routed from the app #1”; “dedicated tenants on there individual files.” |
| FR-002 | Managed creation MUST persist an idempotent operation before side effects and activate routing only after storage/identity/archive/authority/readiness/policy proof and CAS. | Owner automatic new-tenant backend/routing request; safety elaboration. |
| FR-003 | Live reader add/remove MUST use independent copies, guarded bootstrap/catch-up/eligibility/drain and pin recovery; MUST NOT change writer count/identity. | Owner: “can I add readers on the fly?” |
| FR-004 | Migration MUST retain supported selected-tenant content/policy, preserve namespace identities, enforce freeze/drain and watermark validation, and protect other tenants and source storage. | Owner singleton-to-multi-tenant safety question; no blanket safety claim. |
| FR-005 | Cutover/rollback MUST atomically preserve route/credential restrictions; rollback MUST refuse after possible target mutations or source release. | Existing code contract, SRC-007. |
| FR-006 | Crash/retry/cancel/concurrent operations MUST reconcile exact outcomes; unresolved side effects MUST block activation, takeover and destructive cleanup. | Required operational safety. |
| FR-007 | Administrator status MUST expose durable phase, stable failure/recovery actions and retained resources without secrets; legacy tenant creation and shared routes MUST remain compatible. | Required usability/compatibility. |
| NFR-001 | The design MUST NOT change existing gates, limits, measurement policies, SLOs or production authorization. Missing qualified inputs MUST block execution. | Owner constraints. |
| NFR-002 | Capacity/cost admission MUST include quota headroom, overlap, private-reader staging, retained source/archive and cleanup debt; no new numerical defaults. | Operational constraint. |
| SEC-001 | Existing permission classification and backend enforcement MUST remain; lifecycle is administrator-only, with no new tenant-data privilege or trust bypass. | Parent SEC-001/002. |
| SEC-002 | Storage/identities/credentials/control views MUST be scoped, secret-safe and ownership-verifiable; no cross-tenant archive/mount access or automatic resource adoption. | Dedicated isolation. |
| CON-001 | This task MUST edit only local design docs; no runtime/cloud/main checkout/protected-file/pool/PR actions. Superseded for offline code/test/doc files by later owner authorization (the superseding note at the top: continuing, non-migration, offline only); every cloud/main/pool/PR exclusion still applies. | Explicit task boundary. |
| PAT-001 | Reuse current engine, migration, Blob CAS, archive, authority, reader and scaffold primitives; implement only missing composition and safety contracts in future epics. | Avoid duplicate runtime. |

| ID | Assumption / open interpretation | Consequence until resolved |
|----|----------------------------------|----------------------------|
| ASM-001 | “Every tenant” applies to new managed onboarding; existing tenants move individually through explicit migration, not a fleet-wide automatic conversion. | Preserve legacy create/shared mode; owner confirmation OD-001. |
| ASM-002 | Initially one ACA reader app per tenant with many private replicas satisfies “many readers.” | Reuse one reader endpoint; separate app-per-reader alternative deferred. |
| ASM-003 | A VLite-owned control-worker role, Blob journal, managed identities and ACA app secrets are acceptable; no new external state service. | Role placement/privileged host and recovery owner require OD-002. |
| ASM-004 | A selected-tenant write freeze + read-only validation window is acceptable for first migration. | No live tenant-delta replay; owner must accept downtime/profile limits under OD-004. |
| ASM-005 | Dedicated VLite backends are trusted platform processes, not tenant-supplied containers. | Hostile process/storage-key isolation is not asserted; effective Blob pin permissions and mount auth must still be qualified (OD-005). |

## 5. Risk Classification

**Risk**: 🔴 HIGH RISK

**Summary**: The plan joins several independently implemented primitives across nontransactional cloud operations and polled policy. Misordering can route writes before recovery is safe or lose acknowledged data on rollback.

**Items**:

| ID | Risk / failure mode | Required mitigation |
|----|---------------------|---------------------|
| RISK-001 / FM-001 | Paused worker or overlapping ACA writer revision acts after takeover. | Immutable pending action, actuator fencing proof, full revision/replica inventory, authority fencing. No lease/replica-count-only guarantee. |
| RISK-002 / FM-002 | ARM/Blob side effect succeeds but response/journal update is lost. | Exact readback/receipt, no blind retry; retain resources and route state. |
| RISK-003 / FM-003 | Freeze metadata published but old gateway/backend/import still writes. | Backend per-tenant durable barrier, mutation-path coverage and drain proof; block migration when source cannot enforce it. |
| RISK-004 / FM-004 | Target accepts writes during supposedly reversible cutover. | Durable read-only validation guard; close rollback window before write-enable; any uncertain mutation requires reconciliation. |
| RISK-005 / FM-005 | Reader appears ready on a recovery base, changed bindings or replaced lineage. | Verify authority is serving a matching committed head, staleness/identity; response checks and writer fallback; re-enroll only outside transfer. |
| RISK-006 / FM-006 | Capacity, full pins, revoked RBAC or missing prerequisite discovered late. | Preflight actual effective operations/peak budgets; retain blocked state; no permissive defaults. |
| RISK-007 / FM-007 | Stale control projection remains apparently fresh after publisher stalls. | Source-generation/digest plus upstream freshness witness, no credential hashes in backend view, unchanged `S_ctl` end-to-end budget; healthy projection rereads cannot renew it. |
| RISK-008 / FM-008 | Cleanup removes shared data or another operation’s resources. | Exact tenant/operation ownership, stop proof, explicit retained-data release; unknown means retain. |
| RISK-009 / FM-009 | Unsupported source feature is silently discarded. | Strict whole-image profile preflight/allowlist and selected logical inventory parity; no arbitrary migration claim. |
| RISK-010 / FM-010 | Azure Files acknowledged data lost on power loss; stale-writer acknowledgement window unbounded by evidence. | Retain experimental qualification status. Do not deploy this design on the strength of local tests or cloud durability marketing. |

## 6. Dependencies

**Summary**: Existing primitives are dependencies, not new work. Missing live guards, proof channels and cloud qualification are execution blockers.

**Items**:

| ID | Dependency | State / unblock condition |
|----|------------|--------------------------|
| DEP-001 | Control snapshot/CAS, projector, sanitized migration/rekey, authority, archive, reader follower and gateway. | Implemented in SRC-002..010; reuse. No acceptance gate is passed by this inventory. |
| DEP-002 | Durable lifecycle reservation/journal/activation receipt and compatible schema rollout. | Missing; EPIC-001. Required before any provisioning automation. |
| DEP-003 | Pre-route proof and tenant-only policy projection with unchanged revocation/staleness guarantees. | Missing; EPIC-002/003, including upstream freshness witness/clock qualification. A backend public `/ready` or unavailable admin route is not substitute evidence. |
| DEP-004 | Idempotent ARM adapter, prerequisite ownership provisioning, previous-actuator exclusion. | Missing; EPIC-004. Current HA scaffold remains blocked. |
| DEP-005 | Complete source mutation barrier/drain, frozen snapshot coordinator, durable target-write guard and fenced migration profile. | Missing; EPIC-006. Unsupported actual source profile blocks migration regardless of route capabilities. |
| DEP-006 | Trusted-mode pin recovery and qualified reader bootstrap/reseed/scale-down. | Missing integration/proof; EPIC-005. Existing caps remain. |
| DEP-007 | HA qualification/measurement and separate owner authorization. | Parent GATE-004..007 remain independent; GATE-005 open; MEAS-005/006 lack real timing/storage evidence. Azure Files power-loss qualification absent. No real rollout now. |
| DEP-008 | Approved resource, cost, retention, permissions and onboarding profiles. | OD-001..006 below; absence blocks the affected operation, not a placeholder profile. |

### Public-source research (read-only, 2026-11-22)

Official sources were verified by the bounded research agent. Platform claims below are constraints, not target-environment observations; no proprietary content was submitted to the web.

| ID | Source | Design implication |
|----|--------|--------------------|
| AZ-001 | [ACA revisions](https://learn.microsoft.com/en-us/azure/container-apps/revisions#zero-downtime-deployment), [scaling](https://learn.microsoft.com/en-us/azure/container-apps/scale-app#scale-definition) | Single-revision updates overlap old/new revisions; maintenance can exceed configured replica counts. One active writer needs application/storage authority, not `min=max`. |
| AZ-002 | [Blob concurrency](https://learn.microsoft.com/en-us/azure/storage/blobs/concurrency-manage#optimistic-concurrency), [conditional headers](https://learn.microsoft.com/en-us/rest/api/storageservices/specifying-conditional-headers-for-blob-service-operations#supported-conditional-headers) | `If-Match`/412 and create-only `If-None-Match:*` support persisted CAS/idempotency. They do not fence arbitrary ARM calls or mounted filesystem writes. |
| AZ-003 | [ACA storage](https://learn.microsoft.com/en-us/azure/container-apps/storage-mounts) | EmptyDir ends with replica lifetime. Current docs describe SMB managed-identity mounting in preview using an environment identity; do not claim MI mounts are universally impossible or silently adopt preview auth. Neither account-key nor MI auth proves VLite power-loss safety. |
| AZ-004 | [Managed identities](https://learn.microsoft.com/en-us/azure/container-apps/managed-identity), [Blob role scopes](https://learn.microsoft.com/en-us/azure/storage/blobs/assign-azure-role-data-access), [ACA secrets](https://learn.microsoft.com/en-us/azure/container-apps/manage-secrets) | Identity existence does not prove usable permissions; caches delay changes. Container-scoped grants are possible; prefixes are not permission scopes. Identity/secret updates do not automatically create revisions; explicit safe rollout/restart is required. |
| AZ-005 | [Quotas](https://learn.microsoft.com/en-us/azure/container-apps/quotas), [subnet sizing](https://learn.microsoft.com/en-us/azure/container-apps/custom-virtual-networks#subnet), [ACA reliability](https://learn.microsoft.com/en-us/azure/reliability/reliability-container-apps) | Budget all active replicas and overlap, not desired steady state. Zone redundancy is creation-time; it does not replicate the application DB. Do not infer deployable capacity or retrofit singleton properties. |
| AZ-006 | [ACA create/update REST](https://learn.microsoft.com/en-us/rest/api/resource-manager/containerapps/container-apps/create-or-update?view=rest-resource-manager-containerapps-2025-01-01), [ARM async operations](https://learn.microsoft.com/en-us/azure/azure-resource-manager/management/async-operations) | 201 starts an operation, not readiness. Persist operation URL/identity, honor retry guidance, verify terminal result and exact resource desired state. Polling permissions must be validated. |
| AZ-007 | [Revision deactivation](https://learn.microsoft.com/en-us/rest/api/resource-manager/containerapps/container-apps-revisions/deactivate-revision?view=rest-resource-manager-containerapps-2025-01-01), [revision inventory](https://learn.microsoft.com/en-us/rest/api/resource-manager/containerapps/container-apps-revisions/list-revisions?view=rest-resource-manager-containerapps-2025-01-01) | Deactivation response is not flush or zero-replica proof; enumerate all pages/revisions and check actual stopped state before destructive steps. |

## 7. Quality & Testing

**Summary**: Future tests start with deterministic fake stores/cloud adapters, then local multi-process/emulator integration. Cloud qualification is separately authorized and currently blocked. This planning task performs documentation verification, not runtime qualification.

**Items**:

| ID | Test design / deterministic oracle |
|----|------------------------------------|
| TEST-001 | Duplicate create before/after every persisted step, response loss and concurrent same-key requests: exactly one tenant/operation/resource identity; conflicting digest changes nothing. Crash intent-before-reservation and reservation-before-journal-update: resume without orphan activation. Recover with no original request and changed/deleted profile catalog; use retained payload or refuse missing/corrupt inputs. |
| TEST-002 | Inject cloud accepted-but-timeout, partial prerequisites, delayed RBAC, failed poll, resource collision, all-page revision drift, quota failure and paused old worker. Assert no route/cleanup/new writer while unresolved; exact readback resumes the same action. A previous actuator capable of dispatch blocks takeover. |
| TEST-003 | Race activation/cancel/revoke/suspend/delete/migrate/reader change/failover and unrelated tenant updates. At most one tenant lifecycle winner; route+credential+receipt atomic; restrictions persist; no stale whole-directory overwrite; later `A` still resolves operation identity. |
| TEST-004 | Fail each activation prerequisite independently: lineage, epoch, head hash, readiness, bindings, effective identity, projection freshness, unsupported image/storage/gate. Assert live route absent. Pending validation credentials cannot write, access another operation/tenant, or appear in public routing. Stop the projection publisher, advance source policy, keep projection reads healthy: assert admission expires within original source witness `S_ctl`, including slow reads, stale-worker publish, restart, replay and clock regression. |
| TEST-005 | Bootstrap/add/restart/reseed/decrease/remove readers, frozen base, lost authority, stale head, changed bindings, disk-full/two-slot budget, pin exhaustion and replica crash. No writer configuration/identity change; only eligible readers serve; clean drain/unpin or explicit retained cleanup debt. |
| TEST-006 | Two shared tenants with vector/graph/settings/import-marker/memory-binding/orphan and user/credential fixtures. Freeze one while the other continues; inject every supported mutation surface/background job. Capture `W`, project/verify selected parity and no cross-tenant leakage. Refuse each unsupported profile/sidecar/open import/stale grant before cutover. |
| TEST-007 | Validate create and migration credential delivery, stale admin reimport, expiry/revocation/suspension during migration, source/target credential views and audience/lineage checks. Old tokens/password hashes/sessions are not restored. Lost token response requires explicit replacement/revocation workflow, not secret recovery from logs/journal. Replayed/stale delivery acknowledgements cannot activate a different credential generation. |
| TEST-008 | Cutover before/after every CAS; stale source process/gateway; pending in-flight target mutation; read-only validation and rollback; release/rollback race; unknown target outcome. Rollback only with durable no-mutation proof; post-release refuses. Migration does not alter other tenants or overwrite/truncate the shared source; legitimate other-tenant writes may continue. Private captured source images remain unchanged. |
| TEST-009 | Effective permissions/mount and projection isolation: writer/reader cannot access another tenant’s data, global hashes/journal/signing seed or ARM. Reader cannot publish head/claim authority. Exercise required pin writes separately. Denial must not be “fixed” with account-wide data grants. |
| TEST-010 | Status redaction, operation/action ownership and idempotency, cleanup on crash/cancel/retire, retained-data inventory, sealed terminal receipt before reservation reuse, cost/quota failure, snapshot size limit and schema rollout. Crash after route removal, stop or terminal seal resumes retirement without deleting data. Generic resume/cancel never grants release. Legacy routes/create response and HA-off tests unchanged; old schema consumers fail closed until upgraded, not silently accept new admission state. |
| TEST-011 | Separately approved isolated qualification: complete create/readers/migrate/recover/cleanup sequences under ACA revision overlap/maintenance/storage/network/policy faults; collect existing MEAS-005/006 and applicable reader/storage evidence without modifying policies or thresholds. Not run by this plan. |

### Acceptance Criteria

| ID | Criterion | Verification | Traces To |
|----|-----------|--------------|-----------|
| AC-001 | All managed tenant writes route to the unique authority-bound writer; reader operations do not mutate writer configuration or authority. | TEST-003/005; exact diff of resource intents and authority records | FR-001, FR-003, PAT-001 |
| AC-002 | Duplicate/uncertain requests cannot create a second identity or activate without complete proof. | TEST-001/002/004 | FR-002, FR-006, FM-001, FM-002 |
| AC-003 | Add/remove reader never exposes an unready/frozen-base/wrong-policy copy; pin/resource cleanup is proven or reported pending. | TEST-005 | FR-003, NFR-002, FM-005, FM-006 |
| AC-004 | Supported selected-tenant data/policy at `W` are identical by inventory and behavior; other tenants remain unaffected by tenant admission/cutover. Unsupported sources refuse. | TEST-006/007 | FR-004, SEC-001, FM-003, FM-009 |
| AC-005 | Rollback is possible only before release with durable no-target-mutation evidence and restriction-preserving fresh credentials. | TEST-008 | FR-005, FM-004 |
| AC-006 | Races/crashes preserve one lifecycle owner, atomic route receipt and recoverable status; unresolved external actuation blocks takeover. | TEST-001/002/003/010 | FR-006, FM-001, FM-002 |
| AC-007 | Administrator-only status and controls are secret-safe; no backend/gateway gains cloud/global-secret privilege; effective isolation is tested. | TEST-007/009/010 | FR-007, SEC-001, SEC-002, FM-007, FM-008 |
| AC-008 | Existing shared/HA-off behavior and singleton resources are unchanged; gates/limits remain unchanged and absent inputs refuse. | TEST-010/011 plus docs/diff review | NFR-001, NFR-002, CON-001, PAT-001, FM-010 |

## 8. Security Considerations

- **Data handling:** The operator’s shared snapshot contains all tenants and stays private. Only verified tenant projection and sanitized policy enter a dedicated deployment. Existing source credentials contain hashes; backend control projections MUST exclude them. Tenant-qualified names, reserved prefixes, orphan reservations and permission maps are retained.
- **Input validation:** Only administrator-approved profiles may resolve image, subscription/resource scope, storage, identity and endpoints. Tenant-supplied URLs/paths/shell fragments never become ARM targets or commands. Validate exact ownership IDs/tags and digests; tags alone do not authorize adoption/deletion.
- **Access control:** The control worker alone gets scoped resource-management and journal/control writes; gateway gets global control reads and signing material, never ARM/admin credentials; backend gets verification keys and its tenant projection. Dedicated writer gets its own mount/archive/authority access. Reader gets only tenant archive fetch plus necessary pin-metadata operations and authority reads. Current shared snapshot consumption and pin storage layout need changes/proof before claiming these narrower scopes.
- **Pin/authority granularity:** `Sets::pin/release` writes set records, not an isolated lease service. A broad contributor role on a coordination container can exceed pin privileges. EPIC-003 must prove a viable scoped arrangement using existing Blob/managed identities; if current object layout cannot prevent reader authority/head writes, isolate the objects/roles or broker only pin operations through the same VLite control role. Do not add a new external service or mark permission proof successful by granting broad access.
- **Mount isolation:** One tenant writer mount/share is not by itself per-tenant authorization if all apps receive an account-wide key/environment identity. Decide and test the storage/account/identity boundary; preview MI mount support is not automatically approved. Reader templates MUST contain no writer share binding. Tenant-owned workloads are not assumed.
- **Secrets:** Reuse ACA app secrets with immutable generation-specific projections and existing managed identities. No Key Vault dependency. Protect originals, redact process output, and never put secret values in CLI arguments, journals, PRD examples or Git. Signing seeds stay at gateway/control trust boundary; backends get public verification keys only. App-scoped secret rotation must account for all active revisions.
- **Onboarding:** Create credentials once per operation stage, keep candidate hashes/views private, deliver tokens/password-reset information through an authenticated one-time channel, and record delivery acknowledgement without the secret. If the delivery is lost, block activation/release until an explicit replacement-credential/revocation action completes; do not mint fresh credentials on every retry. Existing user-import helpers do not install an admin directory in trusted backends.
- **Admission:** The provisioner does not repurpose suspension as the whole migration protocol. A durable per-tenant mutation barrier and pending read-only validation are separate from authentication, revocation, deletion and ordinary reads. All relevant entry points must enforce both; missing coverage blocks migration.

## 9. Deployment & Rollback

| Stage | Allowed future scope | Go/no-go and rollback |
|-------|----------------------|-----------------------|
| Design (this task) | Local PRD and index link only. | No commands that provision, deploy, release, or change runtime. All gates unchanged. |
| Offline implementation proposal | EPIC-001 first, then dependent items as their prerequisites permit, under the owner's continuing offline authorization (superseding note at the top; existing-tenant migration excluded). Fake cloud/store tests and schema fixtures. | No Azure credentials required. Preserve legacy formats/API defaults; no automatic cloud enablement. |
| Local integration proposal | Synthetic multi-tenant fixtures, local processes/emulator, fault injection and permission/profile negative tests. | Evidence is local only. Schema rollback retains reservations/receipts and blocks incompatible binary downgrade. |
| Isolated cloud qualification | Only after separate authorization, prerequisite proof, actuator fencing, source/reader guards, and applicable existing HA gates. | Existing HA scaffold’s unconditional and writer-update gates are not removed via a Boolean flag; replace refusal only with the separately reviewed/proven protocol. Production remains excluded. |
| Production consideration | Not authorized; requires owner decisions plus all applicable gate/storage/timing/cost/rollback evidence. | Azure Files power-loss risk and MEAS-005/006 cannot be waived by declaring provisioning complete. GATE-005 stays open/off for auto-failover. |

- Cancellation before route activation retains all data/archive/authority and cleans only operation-owned, confirmed-stopped compute. Do not destroy an uncertain/claimed writer to make create retry succeed.
- After activation, disable reader routing first for reader failures; never multiply/promote writers to fix read scale. Writer replacement must follow the existing explicit fencing/loss-report/authority protocol with lifecycle interlocks.
- Migration rollback follows section 3.6, not whole-system snapshot restore. Read-only validation ends only with explicit release. An uncertain mutation or released source requires reconciliation/reverse migration.
- Retirement first CAS-disables tenant routing/admission, waits for policy/drain proof, stops only its apps, then releases owned pins. Retained tenant DB/archive/policy inventory remains until an explicit, audited data deletion decision. Shared infrastructure and unmigrated namespaces are never cleanup targets.
- Cost status includes blocked/idle compute, readers/overlap, Blob polling/requests, archive retention, source retention and unresolved cleanup. Profiles require owner-supplied budgets/quotas; missing cost/retention inputs refuse provisioning or data deletion rather than defaulting.

## 10. Resolved Decisions

| ID | Decision | Rationale |
|----|----------|-----------|
| RD-001 | Owner direction: dedicated deployments/files, one writer per tenant, many possible readers, automatic gateway write routing. | Verbatim confirmations captured in FR-001. This resolves topology intent, not rollout approval. |
| RD-002 | Original boundary: design documentation only; no runtime implementation, production deployment or PR. Retain the owner's constraint against new external service dependencies. Later directions authorized continuing non-migration offline work, then isolated Azure test deployment/evidence collection on 2026-11-24 (top note). Production rollout, migration, gate waivers, source publication and PRs stay excluded. | Narrow scope and protect the singleton. Worker-pool nonuse was a planning-execution choice, not a new owner policy. |
| RD-003 | Retain current gates, policy/measurement thresholds and default-off unqualified auto-failover. | Local primitives are not ACA/storage qualification. |
| RD-004 | Extend parent HA EPIC-007/008 with missing composition; do not reimplement completed primitives. | Existing source, not historical task wording, controls the gap list. |

Proposed architecture choices (control worker, single reader app, freeze-first migration, tenant projections) are recommendations/assumptions, not newly invented owner approvals.

| ID | Owner decision needed | Recommendation / behavior until decided |
|----|------------------------|-----------------------------------------|
| OD-001 | Which onboarding surface selects dedicated, and which existing tenants migrate first? | New explicit managed workflow; leave namespace-only API and existing tenants unchanged. No fleet-wide conversion. |
| OD-002 | Where does the privileged VLite control worker run, who owns recovery, and how is a former actuator excluded? | Separate VLite execution role using existing Blob/admin state. Cloud execution blocked until a concrete fencing/credential/host recovery protocol is verified. |
| OD-003 | Initial/min/max reader capacity, approved demand policy and per-tenant cost budget? | Explicit operator-requested fleet changes first; no numeric profile defaults or independent reader-app scheduler. |
| OD-004 | Accepted tenant write-freeze/read-only validation duration, supported migration profile, user credential delivery and release approval? | Freeze-first, supported closed V5 profile only; live incremental migration deferred. No downtime number invented. |
| OD-005 | Storage/mount authentication and isolation boundary; acceptable reader pin permissions and preview use? | Prove tenant-scoped effective access; do not assume shared account keys or preview MI satisfy it. Missing permission/storage proof blocks rollout. |
| OD-006 | Retention, deletion approval, budget/quota ownership and qualification campaign authorization? | Retain data by default; report ongoing cost; production and measurements remain unauthorized by this document. |

## 11. Alternatives Considered

| Alternative | Pros | Cons | Decision |
|-------------|------|------|----------|
| ALT-001: Extend legacy tenant create to synchronously deploy ACA | One apparent API | Breaks existing 201 contract, loses intent on timeout, exposes cloud latency and retries | Reject; managed asynchronous workflow with explicit opt-in. |
| ALT-002: Run PowerShell scaffold once per tenant / resume by rerunning | Reuses templates | Current gate/prerequisites, local lock and random revisions cannot prove durable idempotency or uncertain outcomes | Reuse validated projections/guards, not script success as lifecycle authority. |
| ALT-003: New workflow/queue/database/Key Vault service | Off-the-shelf durable infrastructure | Additional dependencies, privileges and operating model outside request | Reject here; existing Blob plus VLite worker. |
| ALT-004: One reader app per replica with gateway endpoint list | Fine-grained app lifecycle | New fleet/router schema and greater app cost; not needed to start | Defer; one tenant reader app/private replicas. |
| ALT-005: Shared writer file mounted by reader replicas | Minimal storage copies | Unsafe ownership and not an independently caught-up reader | Reject. |
| ALT-006: Online filtered log catch-up / strip tenant prefixes | Shorter freeze / prettier target names | No implemented delta path; changes references/grants/IDs and verification problem | Defer; preserve qualified names and frozen watermark. |
| ALT-007: Epochless dedicated cutover then enable HA | Matches current migration validator | Route becomes live before required authority/archive proof; hides missing fenced migration profile | Reject for managed HA target; retain legacy primitive compatibility only. |
| ALT-008: Route-first readiness test or expose trusted admin routes | Easy probing | Violates activation invariant or widens trust | Reject; proposed operation-bound read-only proof channel. |

## 12. Files

### Files Affected

**This planning task changes only:**

- **FILE-001**: `docs/projects/feature-aca-ha/design-tenant-provisioning/design-tenant-provisioning.prd.md` — focused companion, source research and review evidence.
- **FILE-002**: `docs/projects/feature-aca-ha/feature-aca-ha.prd.md` — short navigation/status link; no rewriting completed history.

**Future implementation candidates only; not edited or authorized now:**

| ID | File(s) | Planned bounded change / reuse |
|----|---------|-------------------------------|
| FILE-003 | `src/server/tenant_provisioning.rs` (new), `src/server/tenant_provisioning/tests.rs` (new) | Durable lifecycle model, Blob journal/reconciler interfaces and failure tests; no engine implementation. |
| FILE-004 | `src/server/control_state.rs`, `src/server/tenant_migration.rs` | Compact reservations/receipts/projections; preserve CAS/restrictions; extend fenced migration schema and durable write guard. |
| FILE-005 | `src/server/tenant_provisioning/admin.rs` (new), `src/server/tenants.rs`, `src/server.rs`, `src/bin/vlite-server.rs` | Admin lifecycle API/CLI, persisted-ID tenant adapter, module wiring and admission/proof integration. |
| FILE-006 | `src/server/gateway.rs`, `src/server/gateway_mode.rs`, `src/server/gateway_trust.rs` | Route inventory/classification, pending admission, scoped policy feed, no privileged public gateway. |
| FILE-007 | `src/server/tenant_provisioning/aca.rs` (new), `deploy/azure-container-apps/ha/templates.ps1`, `deploy/azure-container-apps/ha/test.ps1` | Future deterministic ARM adapter, reuse/prove role projections and guarded resource plan. Do not edit protected parent deploy scripts. |
| FILE-008 | `src/server/reader_apply/follower.rs`, `src/server/cloud_backup.rs`, `tests/tenant_provisioning_readers.rs` (new) | Reader identity/pin/reseed/drain composition and integration tests; existing archive machinery reused. |
| FILE-009 | `src/server/tenant_provisioning/migration.rs` (new), `src/tenant_log.rs`, `src/server/ops.rs`, `tests/tenant_provisioning_migration.rs` (new) | Authoritative inventory/snapshot coordinator around existing projector/migration helpers, plus bounded access adapters and tests. |
| FILE-010 | `src/server/auto_failover.rs`, `src/server/failover.rs`, `src/server/writer_authority.rs` | Managed-tenant lifecycle interlock only; do not rewrite failover/authority algorithms or enable supervisor. |
| FILE-011 | `src/openapi.json`, `site/api.html`, `docs/API.md`, `docs/HA-ACA.md`, `deploy/azure-container-apps/ha/README.md` | Public route schema/reference and future runbook/qualification documentation. |

An implementation that discovers additional mutation surfaces MUST add a reviewed bounded item before changing them; missing coverage blocks migration rather than silently claiming these candidate files suffice.

## 13. Simplicity Rationale

- **Scope justification:** Every epic below maps to requirements. EPIC-001 is enabling work that cannot be folded into cloud provisioning because crash/concurrency invariants must be testable without cloud access. No epic rebuilds tenant projection, archive replay, gateway routing or authority election.
- **Abstractions check:** One operation state machine and one cloud-effects adapter isolate durable intent from uncertain ARM calls. Reuse `ObjectStore`/CAS instead of a new persistence framework. Tenant projections and pending-read-only proof are necessary to avoid cross-tenant control exposure and premature route activation; they are not a general service mesh.
- **Configuration check:** New managed-workflow selection, worker resource scope, approved profile IDs and operation retry/retention bounds are needed for ownership/cost/recovery. Existing reader/authority/poll/drain settings retain their meanings and caps. No default numbers, all-in-one `enable_provisioning` bypass, new failover threshold, provider plug-in system or new external backing service.
- **Could this be simpler?** The simplest alternative is “create tenant, run existing scaffold, add route.” It fails on scaffold qualification, prerequisites, random reruns, cloud ambiguity, pre-route identity proof, shared-source write admission and rollback safety. A durable VLite reconciler with reuse of current primitives is the smallest coherent design that makes those failures explicit. Offline migration and one reader endpoint deliberately avoid a new replication protocol or scheduler.

## 14. Implementation Plan

EPIC-001 and EPIC-002 are owner-authorized and implemented offline. EPIC-003 is implemented offline as far as its prerequisites permit: ITEM-006/007 are partial and ITEM-008 is blocked (see their status columns and section 15). EPIC-004 is implemented offline against a mockable ARM port only: ITEM-009/010/011 are partial, with no production ARM adapter, actuator stop-proof, clock proof, credential view, profile schema or quota/budget source, so every real path refuses (see section 15). EPIC-005 is implemented offline against the same fake ARM port and test-only proofs: ITEM-012/013 are partial and ITEM-014 is done offline (docs only), so every real reader path still refuses and reader routing stays off (see section 15). EPIC-006 (existing-tenant migration) is **Not Started** and excluded by owner direction. EPIC-007's non-migration offline slice is implemented: ITEM-018 is done at bounded offline scope (runbook and local suite integration, with migration/release documented as excluded). ITEM-019 now has authorization for isolated Azure evaluation, but live execution is blocked on Azure authentication and technical prerequisites, not another routine approval (see section 15). This is not the epic's gate-owner exit. The top notes authorize continuing offline implementation and separate test deployment/evidence collection as prerequisites permit; production rollout, gate acceptance, auto-failover enablement, push, merge and PR remain excluded. Completion is by listed tests/proofs, not estimated time or a historical checkpoint number. Each epic has at most seven unique code/test/doc files; later-discovered surfaces require a separately bounded item. Existing parent epics remain Done at their recorded primitive/scaffold scope.

**Dependency interpretation:** The graph orders offline implementation and local fixtures only. Qualification is a separate execution permission, not a prerequisite for writing its own fake/local tests. EPIC-004/005/006 can produce local code and evidence without passing ACA gates, but cannot execute real cloud operations or migrate a production source. EPIC-007 evaluates the evidence from those implementations under separate authorization; its outcome then controls rollout. No circular “qualify before implementing qualification fixtures” dependency is intended.

### EPIC-001: Durable operation and lifecycle reservation contract — recommended first

**Requirements:** FR-002, FR-005, FR-006, NFR-001, PAT-001. **Dependencies:** existing `Objects`/CAS and migration code only. **Exit:** offline tests prove idempotency, lifecycle arbitration, route receipts and strict-schema compatibility without invoking ARM or changing routing in any live deployment.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-001 | Implement strict tagged desired payload/journal types and create-only principal/key binding; retain immutable complete profile/input content, allocate tenant ID once, preserve request/resource identity, and test duplicate/conflicting/ambiguous writes plus recovery without the original request or current profile catalog using existing fake ObjectStore. No cloud dispatch in this item. | Done (offline) | `src/server/tenant_provisioning.rs`, `src/server/tenant_provisioning/tests.rs`, `src/server.rs` |
| ITEM-002 | Add compact per-tenant lifecycle reservation/cancel/activation receipt to a compatible control schema; atomically publish route/credential restriction changes and receipt. Seal terminal receipts before releasing/reusing reservations. Test concurrency, later-generation readback, lost terminal acknowledgement, payload cap and old-consumer refusal. Preserve existing migration semantics and legacy snapshots. | Done (offline; credential views deferred to EPIC-002) | `src/server/control_state.rs`, `src/server/tenant_migration.rs`, `src/server/tenant_provisioning/tests.rs` |
| ITEM-003 | Specify/test immutable side-effect step and blocked takeover semantics: Blob ownership cannot fence ARM; unknown prior action or live prior actuator prevents conflicting/destructive dispatch. Prove cancellation vs activation has one CAS winner in fake histories. | Done (offline; no production stop-proof, OD-002) | `src/server/tenant_provisioning.rs`, `src/server/tenant_provisioning/tests.rs` |

### EPIC-002: Administrator onboarding and observable operation surface

**Requirements:** FR-002, FR-007, SEC-001, NFR-001. **Dependencies:** EPIC-001, OD-001/002. **Exit:** accepted intent is durable, non-routable, observable and secret-safe; legacy create is unchanged. Cloud execution remains unavailable.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-004 | Add administrator-only create/status/readers/migration/retirement intent endpoints and generation-bound resume/cancel/acknowledge-delivery/replace-credentials/rollback/release actions. Persist action idempotency; reject unavailable transition handlers until their dependent epic is implemented. Create non-routable metadata from the persisted ID. Do not provision from legacy create/public gateway. Test stale/replayed acknowledgements, explicit irreversible release refusal and secret redaction. | Done (offline; REST opt-in via `--tenant-deployments-file`, intent-only, see section 15) | `src/server/tenant_provisioning/admin.rs`, `src/server/tenants.rs`, `src/server.rs`, `src/bin/vlite-server.rs` |
| ITEM-005 | Add equivalent control CLI/status and explicit action verbs using the same generation/idempotency contracts; generic resume cannot acknowledge delivery or release. Register all new routes in classifier/API HTML/OpenAPI. Arguments carry profile/file references, never literal tokens. Validate route inventory and unchanged legacy 201 behavior. | Done (offline, see section 15) | `src/bin/vlite-server.rs`, `src/server/gateway.rs`, `site/api.html`, `src/openapi.json`, `tests/openapi_contract.py` |

### EPIC-003: Non-routable proof, scoped policy and mutation-path interlocks

**Requirements:** FR-001, FR-002, FR-005, SEC-001/002. **Dependencies:** EPIC-001/002, OD-005; source mutation coverage precedes EPIC-006. **Exit:** operation-bound read-only verification is possible without live route/trust bypass; no managed lifecycle bypass remains. Unproven permission/admission coverage is a blocker.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-006 | Produce tenant-only backend projection without credentials/other tenant inventory; implement source-read-start freshness witness, monotonic publication and expiry within the existing `S_ctl`, not transport-read age. Add operation-bound pending read-only validation/proof; keep public gateway unaware of pending routes. Test stalled publisher with healthy projection reads, slow reads, replay/restart/clock regression and blocked clock proof, revocation and inability to mutate. | Partial (offline; contract, backend reader, strict default-off `control_projection` trust-file opt-in, startup and request path tested; production start refused `projection_freshness_unproven` because no approved OD-005 clock-proof source exists; no production publisher, see section 15) | `src/server/tenant_provisioning/projection.rs` (new), `src/server/gateway_trust.rs`, `src/server/gateway_trust_tests.rs`, `src/server/credentials.rs`, `src/server/control_state.rs`, `tests/server_cli.rs`, `docs/API.md` |
| ITEM-007 | Enforce managed reservation at manual/automatic failover and authority-entry paths, and existing control-state edits. Leave auto-failover disabled. Inventory all selected-tenant mutation/background paths; implement central barriers only where coverage is proven, refuse unsupported surfaces, and persist target validation/write-enable guard. Do not claim a route-layer check covers queued work. | Partial (offline; authority claim, manual/automatic failover and control edits guarded; no production holder; an already-serving writer's data/background/queued work is not barriered, see section 15) | `src/server/tenant_provisioning/guard.rs` (new), `src/server/auto_failover.rs`, `src/server/failover.rs`, `src/server/writer_authority.rs`, `src/server/control_state.rs`, `src/server/tenant_provisioning/tests.rs` |
| ITEM-008 | Prove effective tenant-scoped Blob/mount access, including necessary reader pin writes without authority/global-control writes. If existing object placement cannot enforce it, return a concrete blocker and a bounded follow-up for isolated metadata/pin brokering before ACA execution; never broaden grants as a passing test. | Blocked for production (existing placement cannot scope pin writes; reader fleets denied with 409 `pin_scope_unproven`). Follow-up (b) implemented offline only: per-replica reader pin objects plus opt-in `isolated_pins`, proven against a write-scoped fake store; Azure RBAC/ABAC and mount enforcement unqualified (section 15) | `src/server/tenant_provisioning.rs`, `src/server/tenant_provisioning/admin.rs`, `src/server/tenant_provisioning/tests.rs`, `src/openapi.json`, `site/api.html`; follow-up: `src/server/cloud_backup.rs`, `src/server/reader_apply.rs`, `src/server/reader_apply/follower.rs`, `src/server/tenant_provisioning/aca.rs`, `src/bin/vlite-server.rs`, `docs/HA-ACA.md` |

### EPIC-004: Idempotent ACA prerequisite and writer provisioning

**Requirements:** FR-001/002/006, NFR-001/002, SEC-002, PAT-001. **Implementation prerequisites:** EPIC-001..003; unresolved OD-002/005/006 may be represented only as refusal cases in local fixtures. **Execution gates:** resolved profiles/actuator proof, applicable HA qualification and separate operation authorization. **Exit:** mocked exact-resource histories satisfy create sequence; real execution remains gated.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-009 | Implement deterministic ARM effects adapter with complete revision inventory, operation polling/readback, effective prerequisite/ownership checks and persisted action IDs. Prove previous-actuator exclusion; stop if platform condition cannot be enforced. Reuse HA projections/guards and add mocked timeout/collision/overlap cases; do not invoke current blocked script as a success shortcut. | Partial (offline; `aca::Arm` port with mocked create/collision/overlap/incomplete-inventory/unknown/lost-dispatch/failure histories; no production ARM adapter; takeover still needs OD-002 `ActuatorStopped`; HA scripts untouched, see section 15) | `src/server/tenant_provisioning/aca.rs`, `src/server/tenant_provisioning/tests.rs`, `deploy/azure-container-apps/ha/templates.ps1`, `deploy/azure-container-apps/ha/test.ps1` |
| ITEM-010 | Compose private seed, authority claim, archive/revision/readiness/policy proof, candidate credential issuance/private delivery acknowledgement and CAS activation using existing primitives. Implement create-stage replacement/revocation after lost delivery. Preserve seed/archive on crash; refuse claimed-authority reset. Missing proof or delivery keeps route absent; activation creates no readers. | Partial (offline; seed record, authority create/verify behind the guard, readiness proof, delivery/replacement/acknowledgement and CAS activation tested; production refuses `projection_freshness_unproven` (OD-005) and `credential_view_unpublished` (OD-004); no seed engine step, control worker or `vlite-server` wiring, see section 15) | `src/server/tenant_provisioning.rs`, `src/bin/vlite-server.rs`, `src/server/tenant_provisioning/tests.rs` |
| ITEM-011 | Add profile/peak-quota/cost preflight, guarded pre-activation cancel and separate active retirement: CAS route/admission removal, drain/stop proof, owned-pin cleanup, immutable terminal seal and reservation release, always retain DB/archive. Test crashes at each retirement boundary; unknown calls prevent completion. Document provisioned vs verified prerequisites, no singleton adoption and both unchanged HA refusals pending protocol acceptance. | Partial (offline; preflight refusal ladder, cancel cleanup and receipt-verified retirement tested; production profiles refuse `profile_unsupported` (OD-006); no qualification/quota/budget source; no pin cleanup because ITEM-008 is blocked, see section 15) | `src/server/tenant_provisioning/aca.rs`, `deploy/azure-container-apps/ha/README.md`, `src/server/tenant_provisioning/tests.rs` |

### EPIC-005: Live reader lifecycle

**Requirements:** FR-003/006/007, NFR-002, SEC-002. **Implementation prerequisites:** EPIC-003/004 and testable reader profiles. **Execution gates:** OD-003/005, qualified reader proof and separate cloud authorization. **Exit:** private-copy add/remove/drain/reseed/pin sequences pass local integration and never change writer intent; local success does not enable reader routing in ACA.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-012 | Compose durable reader desired-capacity updates with existing enrollment/follower, unique replica ownership and authority/binding/staleness eligibility. Test same-endpoint scale changes and first-endpoint CAS; block frozen-base/replaced-head readers and never mount writer storage. | Partial (offline; anchored `reserve_readers`/`set_readers` with private reader apps, same-endpoint update, first-endpoint CAS (`readers_route`), authority/lineage/binding/epoch/staleness eligibility, reader app/active-revision binding, per-attempt fresh eligibility and replica-name uniqueness tested on fake ARM and test-only `PinScope`/`FleetObservation`; the reservation CAS is bound to the plan's released operation/receipt and route; never-mount-writer-storage is **not enforced** (blocked on the OD-006 profile schema; only app names are compared); production refuses `pin_scope_unproven` (ITEM-008), `profile_unsupported` (OD-006) and `fleet_observation_unavailable` (no observation source); no REST/CLI/control-worker wiring; reader routing stays off, see section 15) | `src/server/tenant_provisioning.rs`, `src/server/tenant_provisioning/aca.rs`, `src/server/reader_apply/follower.rs`, `tests/tenant_provisioning_readers.rs`; bounded addition: `src/server/control_state.rs` (the reader route/receipt CAS must be a lifecycle builder beside `activate`/`retire_route`, which are private to it) and `src/server/tenant_provisioning/tests.rs` (the driver, ARM port and lifecycle builders are `pub(super)`, so an integration test cannot reach them without new public API; the EPIC-004 unit-test convention is reused instead) and `src/server/reader_apply.rs` (one-line `pub(in crate::server)` re-export of the follower pin reason so recovery matches exactly the pins enrollment creates) |
| ITEM-013 | Implement guarded scale-down/drain and durable replacement of failed followers; for untargetable ACA scale-in use writer-only routing until the reduced fleet is verified. Add administrator/control-owned abandoned-pin recovery using existing pin primitives with stopped-replica proof. Test full pins, crash/unknown termination, policy lag, capacity failure and unchanged writer intent. | Partial (offline; unpublish-before-stop `absent` removal that cannot be cancelled after unpublish; a `present` scale-in updates in place with the endpoint still published and seals only once the reduced fleet is verified, writer-only only because reader routing is disabled (withdraw-before-reduction and cancelled-update rollback are ITEM-021 prerequisites); failed-to-healthy fleet observations after a single guarded update tested by EPIC-007 (not proof of ACA revision rolling or replica replacement), overlapping same-attempt removal, and control-owned `recover_pins` (journaled, idempotent, only `reader-apply:<replica>` pins created before a test-only `ReplicaStopped` bound to the tenant and a planned reader app) incl. a full 16-pin set; pins carry no tenant/app/lineage (ITEM-008) and replica names can collide; unknown stop/update/release stay unknown. No production stop proof (OD-002) or replica clock proof (OD-005); replica staleness/epoch eligibility and control-route drift tested, but reader-pin/RBAC policy-propagation lag remains unmodelled (ITEM-008/019); `cloud_backup.rs`/`gateway_mode.rs` reused unchanged, see section 15) | `src/server/cloud_backup.rs`, `src/server/reader_apply/follower.rs`, `src/server/gateway_mode.rs`, `src/server/tenant_provisioning.rs`, `tests/tenant_provisioning_readers.rs` |
| ITEM-014 | Document operator desired/observed fleet status, cleanup debt, eligibility and limits; keep reader routing off until applicable qualification. Do not turn a configured scale ceiling into an SLO. | Done (offline; `docs/HA-ACA.md` "Provisioned reader fleets" documents the unavailable contract, eligibility, removal order, pin recovery, cleanup debt, limits and refusals; reader routing stays off) | `docs/HA-ACA.md` |

### EPIC-006: Supported shared-to-dedicated migration orchestration

**Requirements:** FR-004/005/006, SEC-001/002, PAT-001. **Implementation prerequisites:** EPIC-001..004 and explicit mutation-barrier coverage for each tested fixture. **Execution gates:** complete actual-source coverage/profile proof, OD-004/005/006, applicable migration/storage gates and separate source-operation authorization. **Exit:** two-tenant local fixtures prove isolation, frozen-watermark parity, rekey and conservative rollback; unsupported production profiles still refuse.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-015 | Implement authoritative profile/sidecar/import/policy inventory and selected-tenant drain/snapshot coordinator. Call existing `tenant_log::project` only on a private closed image; preserve qualified names/IDs and `W`, verify selected inventory and other-tenant exclusion. Reject unsupported log operations/build features without widening the projector. | Not Started | `src/server/tenant_provisioning/migration.rs`, `src/tenant_log.rs`, `src/server/ops.rs`, `tests/tenant_provisioning_migration.rs` |
| ITEM-016 | Extend migration schema deliberately to a fenced target profile while retaining legacy authority-off validation behavior; compose rekey/target verification and route+credential+receipt CAS. Keep target read-only through validation, enforce durable source barrier and never trust caller-supplied no-writes proof. | Not Started | `src/server/control_state.rs`, `src/server/tenant_migration.rs`, `src/server/tenant_provisioning/migration.rs`, `tests/tenant_provisioning_migration.rs` |
| ITEM-017 | Wire generation/idempotency-bound delivery acknowledgement and paired credential replacement/revocation actions, proof-only rollback, explicit `ack_irreversible` release before target write-enable, and retained source inventory. Test stale/replayed decisions, interrupted cutover/release, direct/stale source paths, unknown target outcome and preserved other tenants. Generic resume never releases. Missing admission coverage requires a bounded reviewed item before proceeding. | Not Started | `src/server/tenant_provisioning/migration.rs`, `src/server/tenant_migration.rs`, `src/server.rs`, `tests/tenant_provisioning_migration.rs` |

### EPIC-007: Qualification evidence and lifecycle runbooks

**Requirements:** NFR-001/002, FR-007, CON-001. **Implementation prerequisites:** local implementations/fixtures from EPIC-001..006. **Execution gates:** separate evidence/operation authorization and prerequisites of each existing qualification procedure. **Exit:** independent gate owner accepts existing applicable measurements/storage/drills and runbooks; this plan does not predict acceptance or make local fixture creation depend on that outcome.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-018 | Integrate local fault suites and administrator docs for create/status/resume/cancel/readers/migrate/release, schema upgrade/rollback, effective permissions, retained data and uncertainty. Explicitly distinguish implemented local behavior from qualified ACA behavior. | Done (offline, bounded: runbook + local suite integration; migrate/release documented as excluded only; not the gate-owner exit) | `docs/API.md`, `docs/HA-ACA.md`, `deploy/azure-container-apps/ha/README.md`, `src/server/tenant_provisioning/tests.rs` |
| ITEM-019 | Under the 2026-11-24 isolated-test authorization, collect applicable existing evidence for policy propagation, fencing, readers, revision overlap and storage. Keep unresolved timing/stale-acknowledgment, GATE-005 and power-loss proof open until real accepted evidence exists; no policy/threshold edits or Boolean bypass. Record outcomes, costs and verified cleanup without production changes. | Blocked on Azure access (`AADSTS530035`) and technical prerequisites; isolated evaluation authorized, new deployment not executed | `docs/projects/feature-aca-ha/feature-aca-ha.prd.md`, `docs/HA-ACA.md`, `deploy/azure-container-apps/ha/README.md` |

Dependency graph: `EPIC-001 → EPIC-002 → EPIC-003 → EPIC-004 → {EPIC-005, EPIC-006} → EPIC-007`. No implementation command should be executed merely because this graph is complete.

## 15. Change Log

No version bump or historical changelog update was requested. This is a new design companion; completed parent history is retained.

### Planning workflow and review evidence

- Loaded installed `octane-spec-driven-development/agents/Octane.SddPlanner.agent.md`, `skills/octane-planner-plan/SKILL.md`, its `references/octane-prd-authoring-standards.md`, and plugin `config/octane.yaml`. Configured PRD root is `./docs/projects/`; companion nesting keeps the existing HA project together and follows the purpose/component naming convention.
- Purpose review and source research completed. `code-search/*` was unavailable; direct scoped file reads/searches supplied code evidence. One bounded read-only research agent handled only the separate HA scaffold and official public cloud documentation; the planner owned runtime/docs exploration and drafting. No worker pool or canvas.
- Required critical review completed by one read-only architect reviewer over the entire initial 500-line draft, with targeted source checks. No nested reviewers or implementation execution. Review verdict was “Needs Discussion”: three high and two medium contract gaps, no critical finding. The planner addressed all five below; the revised text received a local author consistency check, **not** a second independent review or runtime qualification.

| Finding | Disposition in this companion |
|---------|-------------------------------|
| REVIEW-001 (high): projection rereads could refresh an abandoned source indefinitely | Section 3.4 now defines upstream freshness witness, source-read-start budget, clock/monotonic/restart refusal semantics; TEST-004 and ITEM-006 test stalled publisher despite healthy projection reads. |
| REVIEW-002 (high): digests cannot reconstruct desired inputs after crash | Section 3.3 now defines retained tagged payloads and immutable complete profile/secret-version references; missing content blocks. TEST-001/ITEM-001 cover recovery without original request or mutable profile catalog. |
| REVIEW-003 (high): release/delivery/replacement decisions absent from executable API and create flow | Sections 3.4/3.7 now define create onboarding and generation-bound explicit administrator actions; ITEM-004/005/010/017 and TEST-007/010 cover acknowledgement, replacement/revocation, rollback and irreversible release. |
| REVIEW-004 (medium): retirement and reservation completion underdefined | Section 3.3 defines terminal sealing before reservation reuse; section 3.7 defines separate retirement intent; ITEM-011/TEST-010 cover active retirement and crash recovery without data deletion. |
| REVIEW-005 (medium): apparent qualification dependency cycle | Section 14 and EPIC-004..007 distinguish offline implementation prerequisites from cloud/source execution gates; qualification does not block authoring its local fixtures. |

**Local verification completed (documentation only):**

- Standard-library validation passed: all 15 mandatory numbered sections and required front-matter fields; 7 epics / 19 unique Not Started items; no epic exceeds 7 distinct candidate files; 11 graph nodes / 14 edges with valid endpoints; 8 explicit invariants.
- Relative document links and repository path references were checked; the 7 absent implementation paths are explicitly proposed new files. Corrected the API reference path to existing `site/api.html`. All 22 fully qualified source line-range references checked are within their files; evidence content came from the source reads, not merely this bounds check.
- Untracked companion whitespace/newline checks and `git diff --check -- docs/projects/feature-aca-ha` passed. The tracked parent PRD diff is only the two-line companion link. Git warned of future LF→CRLF normalization for that existing file; no whitespace error was reported.
- No runtime tests, emulator runs, builds, benchmarks, Azure commands, production actions or deployment qualification were performed. No commits, pushes, releases, PRs, worker-pool submissions or main-checkout access. Protected files were not edited.
- Remaining decisions/blockers are OD-001..006 and DEP-002..008, especially prior-actuator exclusion, projection clock/freshness proof, effective pin/mount permissions, complete migration admission, fenced migration profile and existing HA/storage measurements. Review refinements specify refusal behavior; they do not constitute those missing proofs.

### EPIC-001 implementation notes (offline, owner-authorized)

The owner's "go ahead with above fixes" / "Start above together in parallel" instruction authorized EPIC-001 code only. At the time, the earlier "design only" boundary still applied to every other epic (since superseded for offline work; see the note at the top). Cloud execution, deployment, qualification, gate/policy changes, merge and PR creation remain unauthorized.

- **ITEM-001** (`src/server/tenant_provisioning.rs`):
  - Strict tagged `Payload`/`Fleet` types (deny unknown fields; unit-like variants are empty structs, so extra fields are refused).
  - The operation ID is derived from the principal and a hash of the idempotency key; the key is never stored in plaintext.
  - Intent and retained input contents are written create-only, with readback after an ambiguous write. A conflicting retry is refused before anything is written.
  - The tenant ID is allocated once, for `create` only.
  - Recovery needs only the operation ID and the retained content; missing or corrupt input is refused.
- **ITEM-002** (`src/server/control_state.rs`):
  - Optional `lifecycle` slots in the control snapshot. The field is omitted when empty, so legacy snapshot bytes are unchanged; old deny-unknown consumers refuse the new state.
  - Reserve, cancel and activate go through the existing control CAS, with done-checks before writing and readback after failure. Activation publishes the dedicated writer route and its receipt in one generation and leaves existing credential restrictions unchanged.
  - A terminal record is sealed (create-only, digest bound) before the slot is released. A released or ended operation can never reserve or activate again.
  - Any reserved slot and any non-terminal legacy migration exclude each other. `tenant_migration.rs` was not modified; the interlock lives in `Snapshot::prepare` and in validation.
- **ITEM-003**:
  - Each step is persisted as `uncertain` before dispatch and is immutable once settled. An unknown prior action blocks new or destructive dispatch.
  - Claiming over a recorded actuator requires an `ActuatorStopped` proof. Blob ownership alone is refused.
  - Sealing closes the journal by CAS, so no action can be recorded while a terminal record is written.
  - Cancel and activate have exactly one CAS winner.
- **Verification:**
  - Ran `cargo test --features server --lib tenant_provisioning`: 36 passed.
  - Ran `cargo test --features server --lib server::`: 965 passed, 7 ignored.
  - `clippy --lib --tests` is clean for the changed files, and `rustfmt --check` is clean.
  - An independent code review found two high issues: an ended operation could re-reserve its slot, and sealing was not fenced against a live recorder. Both are fixed and covered by tests.
  - No cloud, ARM, emulator or deployment actions were run.
- **Deferred / remaining blockers:**
  - No ARM dispatch adapter and no production `ActuatorStopped` producer (OD-002), so takeover fails closed.
  - Tenant metadata and credential-view onboarding from the persisted tenant ID are deferred to EPIC-002.
  - While a slot holds a receipt, the route's epoch and lineage cannot change, so a failover epoch bump is refused until the slot is released.
  - `migrate` lifecycle operations are not yet bridged to the legacy migration record (EPIC-005).
  - If a released slot is reused by a successor within one recovery window, the residual race fails closed: the stale reservation cannot activate and needs operator repair.
  - Journal phase/blocker reporting is deferred to EPIC-002.
  - MEAS-005/006, GATE-005 and every cloud gate are unchanged and open.

### EPIC-002 implementation notes (offline, owner-authorized)

The owner's instruction to pick up per-tenant writer and reader provisioning next ("please go ahead") authorized EPIC-002 code. At the time, EPIC-003 and later epics were not yet authorized (since superseded for non-migration offline work; see the note at the top). Cloud execution, deployment, qualification, gate/policy changes, existing-tenant migration, push, merge and PR creation remain unauthorized.

- **Owner-decision assumptions (defaults, not gates):**
  - OD-001: onboarding is a new explicit managed workflow. Legacy `POST /v1/tenants` and the public gateway are unchanged and never provision.
  - OD-002: cloud execution stays blocked. Accepted create intent reserves the tenant and stops at `reserved`.
- **ITEM-004** (`src/server/tenant_provisioning/admin.rs`):
  - Administrator-only routes:
    - `POST /v1/admin/tenant-deployments` (create)
    - `POST .../{tenant}/readers`, `.../{tenant}/migrations` and `.../{tenant}/retire` (intent only)
    - `GET /v1/admin/tenant-deployment-operations/{operation}`
    - `POST .../{operation}/{action}`
  - Create builds non-routable tenant metadata from the persisted tenant ID (`tenants::Store::create_with_id`). It then reserves the lifecycle slot unrouted, in the same control generation that adds the tenant (`Snapshot::with_tenant`).
  - Actions are bound to the journal generation, lifecycle generation and receipt digest. A stale token returns 409 `operation_generation_stale`.
  - Action idempotency records are stored per actor and key. A replay returns the stored result; a different request under the same key returns 409.
  - Only `resume` (no-op) and `cancel` can run now. They are refused before anything is persisted:
    - Acknowledge-delivery, replace-credentials and rollback return 409 `transition_unavailable`.
    - Release returns 422 `irreversible_ack_required` without the explicit acknowledgement, otherwise 409.
    - Cancel without a held unrouted reservation returns 409 `transition_unavailable` or `retirement_required`.
  - The status view never contains the idempotency key, its digest, secret contents or tokens. Secret versions appear only as a count.
- **ITEM-005:**
  - `vlite-server tenant-deployment {create|readers|migrate|retire|status|<action verb>}` uses the same accept/act contracts against the control-state store.
  - Arguments are file references only (`--request-file`); no literal tokens.
  - The routes are registered in the gateway classifier (187 REST routes), `site/api.html` and the admin-coverage inventory.
  - All six operations are registered in `src/openapi.json`:
    - Operation IDs: `TenantDeploymentCreate`, `TenantDeploymentSetReaders`, `TenantDeploymentMigrate`, `TenantDeploymentRetire`, `TenantDeploymentOperationGet` and `TenantDeploymentOperationAction`.
    - Strict request schemas: `TenantDeploymentSubmission` (with the four `kind` variants) and `TenantDeploymentAction`.
    - Response schemas: `TenantDeploymentAccepted` and `TenantDeploymentOperation`.
    - Every operation lists its error codes.
  - `tests/openapi_contract.py` derives the six routes from the router source and the action verbs from the handler. It fails if a route, the request/response schema, an error status or a verb is missing.
- **REST runtime opt-in (follow-up commit):**
  - REST is off by default. Without the flag, the routes answer 409 `tenant_deployments_not_configured`.
  - `vlite-server <db> --tenant-deployments-file <json>` serves the routes. The JSON is the existing strict control-state source: `{"target_file", "object", "shared_key_file"?}`. It is read bounded and validated before any database is opened or created; an invalid file refuses startup with `tenant_deployments_invalid:`.
  - The flag requires `VLITE_ADMIN_DIR`. It refuses `--insecure-local` and `--durability none`. Gateway-trust and reader modes already refuse `VLITE_ADMIN_DIR`, so they cannot combine with it. Requests need an authenticated administrator.
  - It reuses the configured `ObjectStore`, control-state, retained-profile and file mechanisms. No new dependency and no cloud actuator are added. Execution stays intent-only (OD-002).
  - For the server's lifetime it holds the admin directory's control-state lock. The offline `tenant-deployment` and `control-state` commands therefore answer `admin_dir_busy` instead of racing it.
  - Recovery mode does not serve these routes.
  - The public gateway and legacy `POST /v1/tenants` are unchanged.
  - Existing-tenant migration is still not implemented. The migrate route only records intent.
- **Deviations (documented, not hidden):**
  - **File count.** Ten code, test and doc files changed, beyond the seven-file guideline. The extra files are required inventory and CLI wiring (`gateway_mode.rs` `CONTROL_STATE_LOCK` visibility, `control_state.rs`, `tests/admin_route_coverage.py`).
  - **Cancelled-create remnant.** A cancelled create leaves a non-routable tenant metadata entry. Cleanup belongs to retirement (EPIC-004 ITEM-011).
- **Verification:**
  - `cargo test --features server --lib tenant_provisioning`: 49 passed.
  - Full `cargo test --features server --lib`: passed, with 0 failures.
  - `cargo clippy --features server --all-targets` and `rustfmt --check` are clean.
  - Route guards pass: `api_route_inventory`, `admin_route_coverage`, `api_reference`, `api_reference_ui` and `openapi_contract`.
  - An independent code review found one medium issue: cancel without a held reservation was persisted before being refused. It is fixed and covered by tests.
  - No cloud, ARM, emulator or deployment actions were run.
- **Follow-up verification (REST opt-in and OpenAPI):**
  - The three new `admin.rs` runtime tests check:
    - The configuration is bounded, strict and validated.
    - The opened runtime holds the admin directory against the CLI.
    - The opened runtime serves the routes with admin authentication.
  - `tests/server_cli.rs::tenant_deployment_routes_are_opt_in_and_refused_before_creating_a_database` checks:
    - Five startup refusals, each leaving no database file.
    - The default answers 409 `tenant_deployments_not_configured`.
    - An opted-in server answers 404 `operation_not_found`.
    - The CLI answers `admin_dir_busy` while the server runs.
  - `tests/openapi_contract.py` failed before registration and passes after it. Removing one registered path makes it fail again.
  - `cargo test --features server --lib`: 2177 passed (`tenant_provisioning`: 52).
  - `cargo clippy --features server --all-targets`, `rustfmt --check`, `git diff --check` and the five route/API guard scripts are clean.

### EPIC-003 implementation notes (offline, owner-authorized)

The owner's question after EPIC-002, "what are you waiting for why you are not pickingup next work items?", is the continuing authorization for non-migration offline implementation as prerequisites permit (superseding note at the top). It covers EPIC-003 offline code and local fixtures. It does not authorize existing-tenant migration orchestration or existing-writer migration barriers (the owner's standing exclusion), Azure/ACA calls, live provisioning, deployment, qualification, gate acceptance, policy relaxation, enabling auto-failover, push, merge or PR creation.

None of the three items is Done. ITEM-006 and ITEM-007 are partial; ITEM-008 is blocked. The EPIC-003 exit ("no managed lifecycle bypass remains") is therefore **not met**. Local fixtures here are not evidence of Azure RBAC, mount enforcement or clock qualification.

- **ITEM-006: tenant-only projection** (`src/server/tenant_provisioning/projection.rs`, new). Status: Partial.
  - **Projection contents.** The `Publisher` derives one tenant's projection (`vlite.control-projection/1`, at most 64 KiB) from a new read of the shared control state. It carries:
    - the tenant record;
    - its admission: either the directory route (`active`), or an operation-bound read-only validator admission (`pending`) for a tenant that is still unrouted and reserved for `create`;
    - the source generation and digest;
    - a witness: revision, `read_start_unix_ms` (the wall time taken *before* the source read) and `deadline = read_start + S_ctl`. Because the clock is read before the source, a slow read uses up the same budget.

    The projection never contains a credential, a credential hash or another tenant's data. Publishing writes nothing to the control state, so the public gateway, which routes from the directory, never sees a pending tenant.
  - **Monotonic publication.**
    - Publication is a compare-and-swap. It refuses a regression of the source, the read start or the revision (`projection_regressed`), a lost race (`projection_conflict` or `projection_indeterminate`) and an unbound pending admission (`projection_pending_unbound`).
    - The publisher keeps no cache, so a restarted or resumed worker can publish only from a new source read.
    - Nothing is written on any refusal.
  - **Freshness at the backend** (`Reader`).
    - A witness is accepted only with a `ClockProof`. Without one, every decision returns 503 `projection_freshness_unproven` (OD-005), a poll refuses before reading the projection at all, and `ClockProof` has no production constructor. No configured number, flag, file, fixture or caller assertion is accepted as clock evidence.
    - The remaining validity, `deadline - wall - uncertainty`, is anchored to the monotonic clock when a revision is first accepted. Rereading the same witness never extends it, so a stalled publisher expires at its original deadline (`projection_stale`) however healthy the projection reads are.
    - If the wall clock falls behind the monotonic clock by more than the uncertainty, the reader returns a sticky `projection_clock_regressed` until it restarts.
    - A missing, foreign, oversized or invalid projection fails closed (`projection_missing`, `projection_invalid` or `projection_unavailable`).
    - Clocks are sampled under the reader's lock (`poll_with` / `current_with`).
  - **Request-path integration** (`src/server/gateway_trust.rs`).
    - Backend trust reads either the full feed or the projection (`View::Full` or `View::Projection`) for admission, quota and readiness generation.
    - Pending admission allows only the bound validator credential for the operation's audience and lineage, and only with a read-only grant (`Grant::allows_only` with Read, Search and GraphRead, in `src/server/credentials.rs`). Anything else returns `pending_validation_read_only`, `forbidden_scope` or `envelope_invalid`.
    - A suspended tenant is refused (`tenant_suspended`).
    - `/ready` reports the source generation but never the admission.
  - **Configuration and startup opt-in (follow-up).**
    - `--gateway-trust-file` accepts `control_projection: {store: {target_file, object, shared_key_file?}, tenant}` *instead of* `control_state`. Exactly one is required (both or neither: `gateway_trust_invalid`), so a projection backend has no shared full-feed fallback. Both members refuse unknown fields; the tenant must be 32 lowercase hex and the object a valid relative name. A legacy trust file is unchanged, and the default is off.
    - `GatewayTrust::start` connects the configured coordination store (an offline client build) and constructs the real `Reader` without a clock proof, because no approved proof source exists (OD-005). The first poll therefore refuses `projection_freshness_unproven` before any projection read, and the binary exits before it opens the database. No production configuration can reach a serving projection backend.
    - With a proof (test-only, through `GatewayTrust::start_projection_on`), the same `start_projection` path performs one accepted read, raises the verifier minimum to its source generation, and starts a `control-projection-poll` thread. The thread raises the minimum on every new witness and logs `control_projection_poll_failed` with its code on refusal. `stop` joins it.
    - `control_state::PollThread` is the poll loop shared by `Feed` and `Reader`; `Feed`'s behaviour is unchanged.
    - Public gateway exposure is unchanged.
  - **Tests.** Ten unit tests cover: tenant-only content, operation binding, publication regression and races, publisher restart, a required clock proof (no store read without one), a stalled publisher despite healthy reads, a slow source read, replayed or altered witnesses, wall-clock regression, and foreign or unreadable projections.
    - `a_trust_file_selects_exactly_one_strictly_validated_control_view` uses the real parser. It accepts a legacy file and a projection file, and refuses: both views, neither, a short or uppercase tenant, a bad object, unknown `clock_uncertainty_ms` / `clock_proof` fields and a missing `store`.
    - `a_projection_backend_does_not_start_without_a_clock_proof` uses the production `start` with a loopback emulator target that nothing listens on; it returns `projection_freshness_unproven`.
    - `a_projection_backend_serves_only_the_pending_validator_read_only` uses the real trust file, `start_projection` and router. Without a proof, start is refused even though a valid projection is published. With a test proof:
      - the validator's GET returns 200, and its POST and any write or management grant are refused;
      - other credentials, other tenants and a generation-1 envelope (below the started minimum of 2) are refused;
      - the tenant is never routed;
      - `/ready` reports generation 2 without admission;
      - after a new source generation is republished, the poll thread raises `/ready` and the minimum to 3, so a generation-2 envelope is `envelope_stale`;
      - with an uncertainty leaving about 0.5 s, the polling backend expires to 503 `projection_stale` and is not extended by rereads.
    - `tests/server_cli.rs` `gateway_commands_validate_configuration_and_keep_keys_out_of_output` starts the real binary with a projection-only trust file. The binary exits without `LISTENING`, reports `projection_freshness_unproven` and creates no database. Both views, a bad tenant and an injected `clock_uncertainty_ms` are each refused with `gateway_trust_invalid`.
  - **Not done:**
    - There is no approved clock-proof source (OD-005). This blocks local implementation on a contract decision: what qualified evidence exists, who attests it, and how a backend receives and revalidates it. It also blocks live qualification, because the evidence must be real. Until both are resolved, projection startup, readiness and admission stay refused.
    - There is no production publisher. It is the control worker in ITEM-010/016, so the publisher and guard-holder composition is EPIC-004 ITEM-010.
    - The private readiness-proof channel is EPIC-004 ITEM-010.
    - Only a `create` reservation can carry a pending admission.
- **ITEM-007: lifecycle interlocks.** Status: Partial.
  - **Durable target guard** (`src/server/tenant_provisioning/guard.rs`, new).
    - `{prefix}lifecycle-guard.json` (`vlite.lifecycle-guard/1`) sits beside `authority.json`.
    - Its states are `held -> write_enabled -> released` (or `held -> released`). Only the holding operation can move it forward, and only forward; a released guard can be held by a later operation.
    - Every write is a compare-and-swap.
    - Every check fails closed: an unreadable guard returns `lifecycle_guard_unavailable`, and an invalid one `lifecycle_guard_invalid`.
  - **Authority entry.** `writer_authority::claim_prepared`, the only production authority `claim`, calls `check_write_enable`. A writer cannot claim authority at startup while the guard is `held` (`lifecycle_write_disabled`). This is the persisted target validation/write-enable guard.
  - **Manual failover.** `failover::execute`, the only production `prepare_transfer`, calls `check_transfer` before preparing the transfer. Both `held` and `write_enabled` refuse with `FailoverError::Lifecycle`, code `lifecycle_reserved`.
  - **Automatic failover.**
    - The supervisor checks the guard before fencing (`Tick::Held`). `execute` checks again after fencing, so a hold that lands during the fence aborts the transfer (`Tick::Aborted`).
    - Auto-failover stays disabled; no default or opt-in changed.
  - **Control-state edits.**
    - `Objects::apply` calls `keep_reservations` (`src/server/control_state.rs`). This covers legacy directory replacement, revoke, suspend/resume and admin re-import, which includes admin tenant deletion.
    - While a tenant's slot is reserved, an edit may change it only by adding a restriction: a suspension, a revocation or an earlier expiry. A changed route, tenant record, tenant-bound credential or slot returns 409 `lifecycle_reserved`, and nothing is written.
    - The managed lifecycle paths themselves (the `tenant_provisioning` CAS and `admin.rs`) use `Objects::update` and are deliberately unguarded.
  - **Mutation inventory (source-grounded).**
    - Authority claim: only `writer_authority::claim_prepared`. Guarded.
    - Authority transfer: only `failover::execute`, which the `failover execute` CLI and `auto_failover` both use. Guarded.
    - Control-state writers:
      - `gateway_mode` publish is create-only.
      - `gateway_mode` update goes through `Objects::apply`. Guarded.
      - `tenant_migration` already refuses a reserved tenant at prepare (`Snapshot` migration insert), and `reserve` refuses while a migration is open.
      - `tenant_provisioning` is the legitimate managed path.
    - **Not barriered:** application data writes, background jobs and queued work inside a writer that is *already serving*. A later hold does not stop that writer. Coverage today rests only on two facts: a reserved `create` tenant is unrouted and cannot be routed by a control edit, and a held target refuses the startup claim. Neither proves a central data-path barrier.
  - **Tests.**
    - `control_state`: two tests (reserved tenants accept only restrictions; a create reservation cannot be routed or deleted by an edit).
    - `guard`: four tests (hold and release, holder-only forward moves, CAS, fail-closed reads).
    - `failover_refuses_a_target_held_by_a_lifecycle_operation`.
    - Two auto-failover drills: a hold before the fence, and a hold during the fence.
    - `startup_refuses_a_claim_until_the_lifecycle_guard_enables_writes`.
    - Two existing `tenant_provisioning` tests changed for the guarded edit. In `directory_replacement_cannot_silently_drop_an_activated_route`, the refusal is now `lifecycle_reserved`, raised before snapshot validation. `legacy_migration_and_lifecycle_reservation_exclude_each_other` now routes T before holding it with a reader update, because a control edit can no longer route a create-reserved tenant.
  - **Not done:**
    - There is no production holder (`hold`, `enable_writes`, `release`); the operation worker is ITEM-010/016.
    - The holder must hold before it starts the target writer, and after holding must re-read the authority to close the check-then-transfer window.
    - An already-serving writer is not stopped.
- **ITEM-008: effective tenant scope.** Status: Blocked.
  - **Assessment of the existing layout.**
    - Reader pins are not separate objects. `Sets::pin` / `release` compare-and-swap the whole set record `{prefix}sets/{id}/record.json`. That record also holds `state`, `manifest_sha256`, the verification fields and `delete_reason`, which together are the set's publication, restore and deletion control.
    - The record is in the same coordination store and prefix as `authority.json`, `auto-failover.json` and `lifecycle-guard.json`.
    - A container- or `{prefix}`-scoped grant therefore also allows authority and guard writes.
    - A narrower `{prefix}sets/` path-conditioned grant would exclude those objects, but it was not qualified here. It would still let a reader rewrite every set's publication and deletion state, because a Blob grant is per object at best and the pin cannot be written separately.
    - The fixture `reader_pins_share_authority_scope_so_reader_fleets_stay_denied` demonstrates this layout: the authority, guard and set record share the store and prefix, and a pin changes only the set record. It proves nothing about Azure RBAC or mount behaviour.
  - **Denial while blocked.**
    - `resume` of a `set_readers` operation for a present fleet returns 409 `pin_scope_unproven` through the shared `act` path, so REST and CLI behave the same.
    - The status `next_step` names the blocker, and `cancel` still releases the reservation.
    - An absent (scale-to-zero) fleet is unaffected.
    - Projection mode is configurable but refuses to start without a clock proof. The managed workflow has no reader actuator (EPIC-005 is not started), and no deploy script or role template was changed.
    - No grant was broadened. The documentation is in `src/openapi.json` and `site/api.html`.
  - **Bounded follow-up, required before any ACA reader execution:**
    - Either (a) broker pin and release through the VLite control role as an operation-bound request, or (b) move pins into per-replica pin objects or a separate container, with layout and janitor changes in `cloud_backup` (EPIC-005 ITEM-012/013).
    - Then qualify the effective grants and mount enforcement in the cloud.
  - **Follow-up (b), offline slice.** Status: implemented locally; production still blocked.
    - **Layout.** A reader pin is its own object, `{prefix}reader-pins/{set}/{pin}.json`, created with `Cond::Create` and deleted on release. `Sets::pin_reader` / `release_reader` / `reader_pins` never write the set record, `authority.json`, `auto-failover.json`, `lifecycle-guard.json` or any data object. `pin_reader` checks the reason, requires a Published set and checks the combined record/reader-pin count before and after the create (`cloud_pins_full`); if the set is no longer Published or the total is over cap it attempts to withdraw its own object. See the non-atomic cap and cleanup limitations below.
    - **Janitor.** Retention protects reader-pinned sets (a listing error records a failure and decides nothing). `delete_set` with Retention returns false and `finish_delete` fails `cloud_set_pinned` (set stays DeletePending) while any reader pin object exists. Race argument: the deleter commits DeletePending before listing pins, and the reader re-reads after creating, so a missed pin cannot be returned successfully; failed withdrawal is logged as retained cleanup debt, not silently treated as removal.
    - **Follower.** `reader_apply.isolated_pins` (default `false`, retaining the record-pin enrollment path) makes enrolment release only its own reader pin objects (matched by reason), pin with `pin_reader` and release with `release_reader`.
    - **Recovery.** `recover_pins` also journals and releases abandoned reader pin objects (same reason, kind and stop-time filter) through `release_reader`. It still requires `PinScope` and `ReplicaStopped`; `PinScope` stays test-only, so present-reader execution still returns 409 `pin_scope_unproven`.
    - **Tests (written first and observed failing — compile failures for the missing API/field, a runtime `1 != 2` for recovery):** `cloud_backup` `reader_pin_objects_need_no_record_authority_or_data_write`, `reader_pin_objects_are_validated_and_bounded`, `retention_and_deletion_wait_for_reader_pin_objects`, `a_reader_pin_racing_a_deletion_withdraws`; `reader_apply` `ha_reader_with_isolated_pins_writes_only_its_reader_pin_objects` (a legacy enrol through the write-scoped view fails `cloud_store_failed`; control objects stay byte-identical through enrol, swap, confirm and stop); `tenant_provisioning` `abandoned_reader_pin_objects_are_released_for_the_proven_stopped_replica`. The write-scoped view is a local fake (`fake::Scoped`) that denies writes outside `{prefix}reader-pins/`; it is evidence of the layout's write needs only, not of Azure RBAC/ABAC conditions or mount behaviour.
    - **Limitations.** Enable `isolated_pins` only after every writer and janitor on the prefix is upgraded (older janitors ignore reader pin objects). The admin forced-unpin route does not see reader pin objects. Isolated mode does not release a replica's legacy record pins. Stray reader pins only delay deletion of that set (liveness, not safety). Cloud qualification of a path-conditioned grant on `{prefix}reader-pins/` and of mount enforcement remains required before ITEM-008 can be Done.
    - **Verification.** `cargo test --features server --lib`: 2292 passed, 0 failed, 10 ignored. `cargo clippy --features server --all-targets -- -D warnings`, `rustfmt --edition 2024 --check` on the six changed `.rs` files and `git diff --check` are clean. No Azure, ARM, ACA, emulator or deployment actions were run.
    - **Bounded correction `80e86b2b` (offline).** Record and reader pins now share the cap: both paths recheck after writing and revert their own over-cap pin. A failed reader withdrawal emits `cloud_reader_pin_withdraw_failed` with the original cause and cleanup error; a failed record revert emits `cloud_pin_revert_failed`. The original error still reaches the caller, and retained pins remain visible to deletion/recovery. Record-pin calls now also refuse a full reader-pin set and fail on an unreadable reader-pin listing; this adds listing I/O/failure points even when follower isolated mode is disabled.
    - **Cap semantics.** This is not an instantaneous storage quota or atomic allocation of pin slots. Overlapping creations can transiently exceed `MAX_PINS` (not necessarily by only one); they can also both refuse. Settled calls respect the combined cap only when rejected-pin cleanup succeeds and all record-pin writers are upgraded. Failed cleanup can leave an over-cap physical count until verified recovery; it is explicit debt, not a successful extra pin. No Azure consistency, permission or production qualification claim is made by the local fake-store tests.
    - **Correction evidence.** Four new deterministic regressions failed on `bdc0562c` and passed on `80e86b2b`: overlapping readers, mixed record/reader overlap, failed reader withdrawal logging and failed record revert logging. The implementer passed 81 `cloud_backup` tests (1 ignored), 2296 library tests (10 ignored), clippy, rustfmt and diff checks; no other Rust source was dirty during that validation. The coordinator independently passed all four `server_reader_apply_cli` integration tests after the correction. The isolated MEAS-004 image remains pinned to earlier `c37e39f6` and does not validate this correction.
    - **Deviation.** Seven files plus this PRD; the extra files are the binary usage line and `docs/HA-ACA.md` for the new opt-in.
- **Deviations (documented, not hidden):**
  - **File count.** 16 files changed, beyond the seven-file guideline: 12 `.rs` files (two new), `src/openapi.json`, `site/api.html` and this PRD. The extra files are the inventoried guard call sites and the request-path view; there was no speculative refactor.
  - **Test order for ITEM-008.** Its two tests were written before the implementation but were not observed failing first.
  - **Candidate file list.** `src/server.rs` and `tenant_migration.rs` from the earlier candidate lists were not needed.
- **Review:**
  - An independent code review of the full diff found no significant issues.
  - It raised two low-severity notes, both currently unreachable in production, and both kept fail-closed:
    1. **Clock drift.** `check_clock` anchors at max(wall, expected), so monotonic-vs-wall drift beyond the uncertainty yields a false but sticky `projection_clock_regressed`. The doc comment now states that a qualified clock proof must bound that drift (OD-005).
    2. **Concurrent polls.** Two concurrent `poll` calls read outside the lock, so the slower one can return a non-sticky `projection_regressed`. This is harmless; the next poll succeeds.
- **Verification (first pass, commits `68ef855c` / `e31fd522`):**
  - `cargo test --features server --lib`: 2200 passed, 0 failed, 10 ignored.
  - The focused set `projection lifecycle tenant_provisioning:: failover writer_authority control_state gateway_trust reader_apply`: 210 passed.
  - `cargo clippy --features server --all-targets -- -D warnings`, `rustfmt --edition 2024 --check` on the 12 `.rs` files and `git diff --check` are all clean.
  - These guard scripts pass: `api_route_inventory`, `admin_route_coverage`, `api_reference` (950 items), `api_reference_ui`, `openapi_contract` and `content_budget`.
  - No Azure, ARM, ACA, emulator, deployment or live-target actions were run.
- **Follow-up: ITEM-006 configuration, startup and request-path wiring.**
  - **Scope.** The `control_projection` opt-in described under ITEM-006 above, plus this PRD's authorization wording.
    - Files: `src/server/gateway_trust.rs`, `src/server/gateway_trust_tests.rs`, `src/server/tenant_provisioning/projection.rs`, `src/server/control_state.rs` (`PollThread` extraction), `src/server/tenant_provisioning.rs` (`is_hex` visibility), `tests/server_cli.rs`, `docs/API.md` and this PRD.
    - The binary needed no change: it already starts the trust before opening the database.
  - **Test order.** The new tests were written before the implementation, but they were not run until it was in place, so they were not observed failing first. A mutation that removed the early no-proof refusal made `every_decision_needs_a_clock_proof` fail; the mutation was then reverted.
  - **Verification:**
    - `cargo test --features server --lib`: 2202 passed, 0 failed, 10 ignored.
    - The same focused set: 212 passed.
    - `cargo test --features server --test server_cli gateway_commands`: 1 passed.
    - `cargo clippy --features server --all-targets -- -D warnings`, `rustfmt --edition 2024 --check` on the six changed `.rs` files and `git diff --check` are all clean.
    - The six guard scripts above pass again (`api_reference` 950 items).
    - No Azure, ARM, ACA, emulator, deployment or live-target actions were run. The loopback emulator targets in the tests point at port 9, and the stores are never read.
  - **Review.** A targeted self-review of the follow-up diff checked four things:
    - there is no full-feed fallback;
    - no production path constructs a `ClockProof`;
    - `Feed` behaviour is unchanged;
    - poll threads are joined by `stop`.

    The parent's instruction to reuse the earlier review context meant no new broad review was run.
- **Residual blockers to local implementation:**
  - **OD-005 clock-proof source contract.** This is a decision, not code: which approved evidence exists, who attests it, and how a backend receives and revalidates it, including the bound on monotonic-vs-wall drift. Until it exists, projection startup, readiness and admission stay refused, and no stand-in is accepted.
  - **The ITEM-010/016 control worker** (EPIC-004 ITEM-010). It is the production projection publisher and the guard holder, and it owns the private readiness-proof transport.
  - **ITEM-008 follow-up (a) or (b)**, a separately bounded reader-pin isolation item. Option (b) is now implemented offline (see ITEM-008 above), but no production `PinScope` source exists, so present-reader execution keeps refusing `pin_scope_unproven`.
  - **Not to be implemented (owner exclusion):** a central data, background and queued-work barrier for an already-serving writer. Only existing-tenant migration needs it, so migration stays refused.
- **Residual blockers to live qualification:**
  - OD-005: a qualified `ClockProof` that also bounds monotonic-vs-wall drift.
  - Read scope of the projection storage.
  - Effective Azure RBAC and mount enforcement for reader pins after the ITEM-008 follow-up.
  - Qualification of the auto-failover interlock before any enablement.
  - MEAS-, GATE- and cloud gates are unchanged and open.
- **EPIC-004 offline handoff** (local code and fixtures only; nothing here enables cloud execution).
  - **Contracts it can use:**
    - **Operation and reservation contract.** The EPIC-001/002 operation/reservation contract and its managed CAS path (`Objects::update`). Unmanaged edits through `Objects::apply` refuse reserved tenants.
    - **Publishing the projection.** `projection::Publisher::new` / `publish(tenant, Option<&Pending>, now)` and `Pending`.
      - Publish from a new source read every time; there is no cache.
      - A pending admission only for an unrouted `create` reservation.
      - Treat `projection_conflict` / `projection_indeterminate` as read-before-retry.
    - **The guard.** `guard::Guards::{new, of, current, hold, enable_writes, release}`.
      - Hold before the target writer starts, then re-read `authority.json` to close the check-then-transfer window.
      - Enable writes only after validation.
      - Release on completion or cancel. Every write is a CAS, and only the holder moves forward.
    - **Backend side.** The `control_projection` trust-file view with its `start_projection` path. Local fixtures use the `#[cfg(test)]` `GatewayTrust::start_projection_on` and `ClockProof::for_test`.
    - **Poll loops.** `control_state::PollThread` for worker poll loops.
  - **What must keep refusing:**
    - Projection startup, readiness and admission without an approved OD-005 proof source (`projection_freshness_unproven`). Do not add a configured or caller-asserted `ClockProof`, and do not add a full-feed fallback.
    - Present reader fleets (`pin_scope_unproven`) until the ITEM-008 follow-up.
    - Authority claim while held (`lifecycle_write_disabled`).
    - Failover of a held target (`lifecycle_reserved`).
    - Auto-failover stays disabled.
    - Existing-tenant migration, including existing-writer barriers, stays excluded.
    - Every Azure/ACA call, provisioning, deployment, gate acceptance, push, merge and PR.

### EPIC-004 implementation notes (offline, owner-authorized)

- **Scope delivered.** All work is local code and in-memory fixtures. No Azure, ACA or ARM call, emulator, deployment, gate acceptance, qualification claim or auto-failover change was made. EPIC-003 stays partial and its refusals are unchanged.
- **ITEM-009: the ARM port** (`src/server/tenant_provisioning/aca.rs`, new).
  - `aca::Arm` has `begin` (returns an operation ID), `poll` and `read` (owner tag, configuration digest, provisioning state and a `Complete`/`Incomplete` revision inventory). Tests use a scripted `MockArm`. There is **no production adapter**, and nothing can construct one outside tests.
  - `Driver::new` claims the operation before any port call, so another worker refuses with `actuator_fence_unproven`. A takeover still needs the OD-002 `ActuatorStopped` proof (test-only `proven_for_test`).
  - Each effect is an immutable journal step. Its request digest binds the owner, resource, operation and configuration, and the ARM operation ID is persisted (`Step.arm_operation`) before polling.
  - Effects resume only by polling that ID and reading back. They never redispatch. A lost dispatch with no ID stays `cloud_outcome_unknown` forever.
  - Success requires an exact readback: our owner tag `vlite-tenant:{tenant}`, the configuration digest, a succeeded state and, for apps, a complete inventory with exactly one active revision running that configuration.
  - Overlap refuses `revision_overlap`, an incomplete inventory refuses `revision_inventory_incomplete`, and drift stays unknown.
  - A foreign owner settles `conflict` (`resource_ownership_conflict`). A refused or failed operation with an absent readback settles `failed:{code}` (`cloud_action_failed`).
  - A resource found before the first dispatch is refused, even with our own tag: there is no adoption.
  - Verified prerequisites are only read: `prerequisite_missing`, `prerequisite_mismatch` or an ownership conflict.
  - The HA PowerShell scaffold (`ha/templates.ps1`, `ha/test.ps1`) was not changed or invoked.
- **ITEM-010: authority, readiness, delivery and activation** (`src/server/tenant_provisioning.rs`).
  - `record_seed` records the seed lineage and archive digest once. A different seed refuses `seed_mismatch`.
  - `establish_authority` holds the lifecycle guard and creates or reads `authority.json` bound to the seed lineage. A record of any other installation, scope or lineage refuses `authority_identity_mismatch` and is never reset, and the guard stays held. Otherwise it enables writes. The route stays absent throughout.
  - `prove` refuses 503 `projection_freshness_unproven` without a `ClockProof`. With one, it refuses `readiness_unproven` unless all of these hold:
    - the authority is Serving at its epoch with a first checkpoint;
    - the lineage is the seed;
    - the head manifest is the archive manifest;
    - the writer app is ours, succeeded and has one active revision in a complete inventory;
    - the required policy generation (> 0) is observed.
  - Credential delivery is a CAS record per operation:
    - `issue_candidate` creates a candidate;
    - `replace_candidate` moves to a new generation and lists the old candidate as revoked;
    - `acknowledge_delivery` is generation-bound;
    - stale actions refuse `delivery_stale`;
    - a lost write acknowledgement refuses `operation_indeterminate` and is resolved by the same call.
  - `activate_create` requires an acknowledged delivery and a proof, and the route must be exactly a dedicated, writer-only, readerless route at the proven lineage and epoch (`activation_route_invalid`).
    - The acknowledged credential must be live and bound to the tenant and lineage in the published view (`credential_view_unpublished`).
    - Every revoked candidate must be absent there (`candidate_revocation_unconfirmed`).
    - It then publishes the route and receipt with one CAS, releases the guard and seals `succeeded`. It is idempotent and creates no readers.
- **ITEM-011: preflight, cancellation cleanup and retirement** (`aca.rs`).
  - `preflight` validates the profile (422), then refuses in order: `qualification_missing`, `quota_unproven`, `quota_exceeded`, `budget_unapproved`. `Profile::resolve` always refuses `profile_unsupported` (OD-006 has no schema). The qualification, quota and budget proofs exist only in tests.
  - Cancellation cleanup runs only on a cancelled, unsealed reservation:
    - It first resolves uncertain creates by owner tag.
    - It then deletes only the apps this operation created, with readback proof.
    - Verified and non-app owned prerequisites and data are retained.
    - Sealing `cancelled` refuses `cleanup_pending` until every created app is deleted or absent.
  - Retirement verifies the expected receipt against the sealed create and the published route (`retirement_receipt_mismatch`), records a plan step and reserves.
    - It then holds the guard permanently (no writer claim or failover), removes exactly that route with a CAS (`retire_route`) and stops each planned app with readback proof that no revision is active.
    - Finally it seals the new `retired` outcome and releases the slot. The database, archive, credentials and apps are retained.
    - An unknown stop blocks completion (`cleanup_pending` / `retirement_incomplete`).
    - Once the route is removed, cancellation refuses `retirement_in_progress`.
    - A lost route-removal acknowledgement resumes by readback.
  - Owned-pin cleanup is not implemented because ITEM-008 is blocked.
- **Behaviour change.** Sealing a cancelled create now requires cleanup of any app it created (`cleanup_pending`). This is intentional and stricter. Operations with no `create-app` steps behave as before.
- **Deviations from listed files.** There is no `src/bin/vlite-server.rs` wiring and no `ha/templates.ps1` or `ha/test.ps1` change. `src/server/control_state.rs` gained `retire_route`, the retired release arm, a `retirement_in_progress` cancel refusal and the `live_credential` readback. `admin.rs` renders `retired` and shares `receipt_digest`.
- **Verification.**
  - `cargo test --features server --lib tenant_provisioning`: 94 passed.
  - Full `cargo test --features server --lib`: 2228 passed, 10 ignored.
  - `cargo clippy --features server --all-targets -- -D warnings` is clean, and rustfmt and `git diff --check` are clean.
- **Remaining blockers** (each refuses today):
  - **OD-002.** There is no production ARM adapter or actuator stop-proof, so takeover and real dispatch are unavailable.
  - **OD-005.** There is no approved `ClockProof` source, so readiness refuses `projection_freshness_unproven`.
  - **OD-004/credential schema.** The published credential view cannot hold a reserved tenant's candidate, so production activation refuses `credential_view_unpublished`.
  - **OD-006.** There are no profile schema, quota or budget sources and no qualification, so preflight refuses.
  - **Missing workers.**
    - The seed-producing engine step is not implemented.
    - The control worker (projection publisher, poll loop, guard holder and readiness transport) is not implemented.
    - The REST/CLI handlers are not wired.
  - **Liveness limits.** A settled failure cannot be retried within the same operation; a new operation is required. A crash before an ARM operation ID is persisted leaves the step unknown until operator resolution.
  - **ITEM-008 follow-up.** This remains for reader pins and pin cleanup. EPIC-005 (readers) and EPIC-006 (migration) are excluded and untouched.

### EPIC-004 review corrections (offline)

These corrections follow the review of a913ebbe. Each finding was reproduced by a focused test that failed on a913ebbe before its fix. ITEM-009, ITEM-010 and ITEM-011 stay **Partial**, and every refusal listed above still applies.

- **Finding 1: an uncertain create is resolved only after its operation has finished.**
  - Polling now has three states:
    - `Succeeded`;
    - `Failed`;
    - `Open`: no operation ID, still running or unknown after the poll budget, or a refused poll.
  - A create is settled `created` only when its own persisted operation is `Succeeded` **and** the readback is exact and ours. This applies to both provisioning and cleanup.
  - While the operation is `Open`, the step stays `cloud_outcome_unknown`. Nothing is deleted, and sealing as cancelled refuses `cleanup_pending`.
  - Our resource present after a `Failed` operation also stays unknown.
  - A lost dispatch, which has no operation ID, is never compensated.
- **Finding 2: a refused dispatch never records a create.**
  - A begin call that `ArmError::Refused` rejected (the port guarantees it took no effect) settles `failed:{code}` immediately, whatever the readback shows. A matching same-tenant resource that appears at that name is therefore never adopted and never deleted by cleanup.
- **Finding 3: readiness is bound to the operation and the exact route.**
  - `Readiness` carries the verified writer-only route, and `prove` validates it. The checks are: the tenant, dedicated, writer-only, readerless, no authority mode, the seed lineage, and the authority epoch (which must be > 0).
  - `Proof` binds the operation ID and the route digest. The readiness digest (`vlite.readiness/2`) includes the route digest.
  - `activate_create` refuses `activation_route_invalid` if the operation, writer, audience, binding generation or any other route field differs from the proven route.
- **Finding 4: a committed activation is recovered.**
  - If the slot already holds this operation's receipt and the published route's digest equals the receipt's, `activate_create` releases the guard and seals `succeeded` from that durable receipt. This happens before the readiness-proof and credential checks, so it still completes after a later checkpoint has changed the readiness digest, or after the credential has expired or been revoked.
  - A supplied route that differs from the committed one refuses `activation_route_invalid`.
- **Finding 5: a cancelled retirement releases its guard.**
  - Retirement journals a `control/guard` hold step before it holds the guard.
  - If a cancellation is found before the route is removed (on entry, or after a failed route-removal CAS), retirement releases the guard only when this operation holds it, journals the release, and refuses `operation_cancelled`. If the cancellation came before the hold, it never holds.
  - `cleanup` also performs this release for a retirement.
  - Sealing a cancelled operation that journaled a hold refuses `cleanup_pending` until the release is journaled.
  - The acquisition race is covered by the persisted-held path: hold, failed CAS, cancel, rerun. The cancel CAS cannot be injected in the middle of `lifecycle_cas` with the in-memory store.
- **Finding 6: retirement after a cancelled retirement.**
  - When the tenant's released slot points at a sealed **cancelled** operation with a settled retirement plan, verification follows the plan's activation ID exactly one hop.
  - That activation must be sealed `succeeded` for the tenant. Its receipt's operation ID and route digest must equal the plan's, its receipt digest must equal the expected receipt, and the current route digest must equal the receipt's.
  - Each later plan names the original activation again, so the chain never grows.
  - Unrelated or stale receipts and `retired` slots still refuse `retirement_receipt_mismatch`.
- **Verification.**
  - `cargo test --features server --lib tenant_provisioning`: 103 passed. This is 94 earlier tests plus 9 new reproductions, and the existing `cancelled_retirement_keeps_the_route` test now asserts the guard state too.
  - `cargo clippy --features server --all-targets -- -D warnings` is clean, and rustfmt and `git diff --check` are clean.
  - Full lib suite: `cargo test --features server --lib` 2237 passed, 10 ignored.

### EPIC-004 second review corrections (offline)

The re-review of f047bb9a found two remaining race variants. Both are fixed test-first. EPIC-004 and ITEM-009/010/011 stay **Partial**, and every earlier refusal is unchanged.

- **A: a dispatched cloud effect is settled only when its operation is terminal.**
  - A create whose operation is still open (running, unpollable or unidentified) stays unknown, even when the readback shows another owner's resource. `conflict` is settled only once the operation has `Succeeded` or `Failed`.
  - A delete or stop is settled `deleted` or `stopped` only when its operation `Succeeded` **and** the readback proves the effect.
  - An apparently finished readback (app absent, revisions inactive) while the operation is still open leaves the step unknown. This keeps the cancelled seal (`cleanup_pending`) and the retirement's reservation from being released.
  - A `Failed` operation settles `failed:<code>`, even when the readback looks done. That is never success, and the seal stays refused.
  - An explicit port refusal (`Refused`: no effect, by contract) is still terminal.
  - Absence read before any dispatch still settles `absent`.
- **B: an uncertain guard acquisition is fenced, not assumed.**
  - New `Guards::fence(tenant, operation)` makes a conditional write of this operation's `released` guard: `Create` over absence, or `Match(etag)` over a released guard or this operation's own guard. It is idempotent over its own release and refused (`lifecycle_guard_held`) while another operation holds the guard.
  - Every earlier conditional hold was conditioned on a revision that the fence replaces, so a delayed hold conflicts if it lands afterwards.
  - A cancelled retirement's `release_guard` (both on the `retire` path and in `cleanup`) settles the hold and journals the release only after the fence write is acknowledged. An unknown fence outcome leaves the hold unresolved, so the seal refuses `cleanup_pending`.
  - Absence, or a predecessor's released guard, is no longer taken as proof.
- **Tests.**
  - The reproductions failed on f047bb9a.
  - A pending create with a foreign owner: running polls, then a `Failed` poll settling `conflict`.
  - A pending delete with an absent readback, and a failed delete.
  - A pending stop with an inactive readback.
  - A delayed conditional hold that lands after cleanup and the seal, using a parking store wrapper, both with no prior guard and over a predecessor's released guard. On f047bb9a the late `Held` landed after the `Cancelled` seal and transfer refused `lifecycle_reserved`. Now it conflicts and the guard stays `released`.
  - A `fence` unit test.
- **Residual limits.**
  - Only a single driver per operation is assumed. A hold issued after the fence by a concurrent driver of the same operation is not modelled.
  - A cancelled retirement whose guard another operation now holds stays unsealed until that holder releases it.
  - Real ARM terminal semantics remain unproven offline.

### EPIC-004 third review corrections (277c6f77 review)

Status remains **Partial** for ITEM-009/010/011; no gate, refusal or qualification changed. Root cause: both findings relied on an unenforced "one executor per attempt" assumption (`claim` admits the same attempt repeatedly). Safety now rests only on the durable journal (record/observe/settle CAS) and conditional guard CAS, not on exclusivity.

- **A (HIGH, overlapping cleanup settles a running delete as absent):** the destructive pre-dispatch absent fast path now applies only to `Decision::Dispatch` (this caller durably recorded a never-dispatched step). `Reconcile`/`Settled` load the durable step and require terminal poll or settled-step proof; a step settled by another executor is accepted only as `done`/absent, any other settled value fails, an uncertain one stays unknown. Coupled create path audited: an overlapping create never adopts or settles another executor's create (pre-existing fail-closed `ownership_conflict`, left unchanged).
- **B (HIGH, paused hold acquires a successor guard version):** a hold step is durably bound (first-writer-wins `observe`) to the one guard version it may replace (absence or a released predecessor etag) before any guard write, and never refreshed. `Guards::hold_bound` writes only with `Create`/`Match(bound)`; it never re-reads and adopts a newer version. Cancellation binds an unbound hold to a fence sentinel (no hold write may follow) or fences the bound version with a conditional Released write (`Guards::fence(bound)`), which writes nothing when the binding was already superseded and never touches another operation's guard. A late executor therefore fails `observe` or conflicts on its conditional write.
- **Evidence:** on 277c6f77 `an_overlapping_cleanup_cannot_settle_a_running_delete_as_absent` failed (cleanup returned Ok) and `a_hold_paused_before_its_guard_read_cannot_acquire_a_successor_version` failed (guard left Held by terminal A instead of successor B's Released). After: focused `tenant_provisioning` 117 passed / 0 failed; full lib 2251 passed / 0 failed / 10 ignored; clippy `-D warnings` clean; rustfmt and `git diff --check` clean. Added regressions cover the A->cancel->B->cancel->late A interleaving (paused before guard read and before guard write), interruption before binding, unknown hold writes (fault before/after) retried against the bound version, overlapping deletes settled by another executor, and overlapping creates.
- **Residual limits:** same-attempt duplicate executors are still admitted but made safe, not excluded; correctness assumes the guard object is never deleted and store etags are never reused (true of the in-memory fake; unproven for real stores); a hold whose bound version was superseded refuses `lifecycle_guard_conflict` and resolves only through cancellation; an uncertain delete/stop step without an ARM operation ID remains unknown (no correlation); all ARM/ACA/store behaviour is mock-only and unqualified. EPIC-003 remains partial, EPIC-005/006 untouched.

### EPIC-005 implementation notes (offline, owner-authorized)

Status: ITEM-012 **Partial**, ITEM-013 **Partial**, ITEM-014 **Done (offline)**. EPIC-005 as a whole is **not** done: its exit and execution gates (OD-003/005, qualified reader proof, cloud authorization) are unmet. No gate, refusal, permission, migration barrier or writer behaviour changed; reader routing stays off.

- **Delivered (local code and fakes only):**
  - `Driver::reserve_readers`/`set_readers` in `aca.rs`. A `set_readers` operation is anchored on the tenant's latest succeeded (or cancelled-then-anchored) receipt and its current route digest (`readers_unanchored`). It records a reader plan (`anchor route target= endpoint= reader=...`) and owns only private reader apps it created; writer apps and other readers are `reader_app_mismatch`. A same-endpoint scale is an in-place guarded `Verb::Update`. The first endpoint publishes through the `control_state::readers_route` CAS (Dedicated, writer-only, epoch > 0, digest in {anchor route, target}). A different endpoint is `reader_endpoint_changed`. The writer app, writer route fields and intent are untouched (asserted by the add, scale, removal and retirement tests).
  - Eligibility (`eligible`) is fail-closed. It requires a valid serving authority with a head (`reader_base_transition` covers frozen-base/replaced-head), and every replica on the same set, lineage, binding and epoch, not failed, and fresh within its bound. Duplicate replica names are refused. Min/max counts refuse (`reader_fleet_below_min`, `reader_fleet_not_reduced`); the max is a limit, not an SLO.
  - Removal (`absent`) unpublishes the endpoint first, then stops readers through the existing destructive journal. `request_cancel` refuses `readers_in_progress` once unpublished. The terminal seal requires every reader stopped (`readers_incomplete`). A `present` scale-in is not withdrawn first: it updates in place and seals only once the reduced fleet is verified (corrected per the 2e53839d review below).
  - `recover_pins` is control-owned abandoned-pin recovery under a held reader reservation. It releases only `reader-apply:<replica>` operation pins (reason shared with enrollment through `follower::pin_reason`) created no later than a `ReplicaStopped` proof, journaled before `Sets::release`. It is idempotent and handles a full 16-pin set; an uncertain release is resolved only by rerunning.
  - Cancel/retire anchoring was generalized. Cancel of a reader add deletes only the reader it created. Retirement must name the latest receipt and also stops owned readers.
  - Same-attempt concurrency: the `set_readers` wrapper returns the sealed release when a concurrent executor of the same attempt sealed first. Effects rely on the existing journal/guard CAS, never on a process mutex or exclusivity. An overlapping same-attempt removal stops the reader exactly once (mutation-checked).
- **Proofs exist only as test constructors.** `PinScope`, `FleetObservation` and `ReplicaStopped` have `#[cfg(test)]` constructors only; production has no source, so real paths refuse `pin_scope_unproven` (409), `profile_unsupported`, `fleet_observation_unavailable` or `replica_stop_unproven`. No caller-provided boolean is accepted as proof.
- **Evidence:** before implementation, `cargo test --features server --lib tenant_provisioning` failed to compile (35 errors: missing `reserve_readers`/`set_readers`/`recover_pins`, `Verb::Update`, `FleetObservation`/`PinScope`/`ReplicaStatus`/`ReplicaStopped`, 3-arg `terminal_ready`). After: 14 new `epic_005` tests; focused `tenant_provisioning` 131 passed / 0 failed; full lib 2265 passed / 0 failed / 10 ignored; clippy `-D warnings` clean; rustfmt and `git diff --check` clean. Mutation check: disabling the wrapper's sealed-terminal check makes `an_overlapping_same_attempt_removal_stops_the_reader_once` fail.
- **Surfaces:** `tests/tenant_provisioning_readers.rs` was not created (the driver is `pub(super)`; no public API was added). The bounded additions are `control_state.rs`, `reader_apply.rs` (one re-export) and `tenant_provisioning/tests.rs`. `cloud_backup.rs` and `gateway_mode.rs` are reused unchanged.
- **2e53839d review corrections (local correction commit):**
  - H1 fixed: `reserve_readers`/`reserve_retirement` reserve through `reserve_planned`. It requires the settled plan's anchor operation and route to still be the tenant's anchor. `control_state::Snapshot::reserve_after` (through `tenant_provisioning::reserve_after`) then takes the slot only if it is still `Released` by that operation and terminal digest and the route digest is unchanged, in the same lifecycle CAS. Otherwise it refuses `readers_unanchored`/`retirement_receipt_mismatch` with no slot, ARM call or route change. Same-operation idempotency is kept (an executor already holding the slot returns `Ok`). Create, admin and other callers keep plain `reserve`.
  - M1 fixed: `eligible` no longer short-circuits on an earlier journaled proof. Every attempt that reaches publication requires a current observation. A publication that already committed (receipt held) is still recovered receipt-first without one.
  - M2 corrected: the docs, this row and the test (`a_present_scale_in_updates_in_place_and_seals_only_once_reduced_while_routing_is_disabled`) now state that a present reduction updates in place while the endpoint stays published. Withdraw-before-reduction and cancelled-update rollback are ITEM-021 prerequisites.
  - M3 partially fixed: `ReplicaStatus` carries `app`/`revision`, and `eligible` reads the reader app (owned, exact, single active revision: `reader_revision_unproven`) and refuses replicas of another app or revision (`replica_ineligible`). `ReplicaStopped` carries `tenant`/`app`, and `recover_pins` refuses a proof whose tenant is not the operation's or whose app is not a planned reader (`replica_stop_unproven`). Blocked: pins carry only `reader-apply:<replica>` (no tenant, app or lineage) and set records have no tenant scope, so recovery scans every set (ITEM-008). The follower's host-name/`pid-<pid>` fallback names can collide. No production observation or stop-proof source exists.
  - M4 documented: never-mount-writer-storage is unimplemented and blocked on the OD-006 profile schema; the app-name check is not that guarantee.
  - Regressions added: stale reader and retirement plans (route changed, route unchanged/different receipt, and atomic races injected right before the slot write), after which fresh operations still reserve. Also: retry with missing, bad or valid observation; committed-receipt recovery; cancelled in-place update (route kept, not rolled back); unknown and running updates never resent; overlapping same-attempt update dispatches once; `prior_action_unknown` pin recovery; uncertain pin-release replay; re-add reusing the stopped reader app; replicas on a replaced revision, another app or another revision; and stop proofs for another tenant or app.
  - Evidence: before the fix, `cargo test --features server --lib epic_005` gave 21 passed / 7 failed. Five H1 stale-plan/race tests reserved (`Ok(())`). The M1 retry with no observation sealed. One fixture had to fail all 4 `Sets::update` attempts. The M3 tests compile only against the new fields; with the new checks neutralized, 3 tests fail. After the fix: `epic_005` 29 passed; focused `tenant_provisioning` 146 passed; full lib 2280 passed / 0 failed / 10 ignored; clippy `-D warnings`, rustfmt and `git diff --check` clean.
- **Remaining blockers (all refuse in real paths):**
  - Superseded by the R1 correction below (REST/CLI non-create submissions no longer reserve).
  - Never-mount-writer-storage enforcement (OD-006 profile schema).
  - Withdraw-before-present-reduction and cancelled-update rollback (ITEM-021 prerequisites).
  - ITEM-008 reader pin scope; follow-up (b) per-replica pin objects is implemented offline (opt-in `isolated_pins`), but effective permissions are unqualified and production still refuses `pin_scope_unproven`.
  - A production ARM adapter with real update/stop semantics and OD-002 stop proof.
  - A replica/authority fleet-observation source.
  - OD-005 replica clock proof for pin recovery.
  - Reader qualification, OD-006 profile/quota/budget, and reader credentials (OD-004).
  - Control-worker/REST/CLI composition.
  - Reader routing qualification (ITEM-021).
  - No separate policy-lag or replacement-only fixture beyond staleness/epoch eligibility and in-place update. (Partly addressed by EPIC-007 ITEM-018 below: regression pins now cover failed-to-healthy fleet observations and out-of-lifecycle route drift; real ACA replacement and reader-pin/RBAC policy-propagation lag remain unmodelled.)
  - A cancelled reader update cannot be rolled back (documented cleanup debt).
- **e36403e1 review correction (R1, local correction commit):**
  - Scope justification: `tenant_provisioning/admin.rs` is a direct caller of the changed reservation logic (`reserve_for` from REST `submit` and CLI `run`), so it is in this bounded correction. No other admin surface, permission or executor changed.
  - Fixed: `admin::reserve_for` reserves only `create` (with its tenant metadata, unchanged). Reader changes, retirement and migration submissions are recorded (phase `accepted`, `observed.reserved` false, next step naming the plan-anchored actuator and the ITEM-008/OD-002 blocker) but take no slot; duplicate submissions stay idempotent. Cancel of an unreserved non-create operation is refused `transition_unavailable` (no slotless seal, which would race a driver's `reserve_after`). `tenant_provisioning::reserve` (plain, unbound) is now `#[cfg(test)]`: production reserves non-create operations only through `reserve_after`, bound at acquisition to the plan's released predecessor and route. This supersedes the H1 note above that admin kept plain `reserve`.
  - Fixed: `reserve_planned` no longer accepts any same-operation held slot. It requires the operation's settled plan and a held `Slot::Reserved` whose `expected_route` equals the plan route; otherwise `readers_unanchored`/`retirement_receipt_mismatch` with no ARM effect. A legitimate same-operation retry (slot acquired by `reserve_after` against its plan) still succeeds; a cancel of such a slot hops through the plan and later changes anchor.
  - Documented limitation (not repaired, no schema change): slots reserved without a plan by builds from EPIC-002 (1d8000b9, admin plain `reserve`) through e36403e1 are refused when their route differs from the plan. When the route is the same but a newer receipt sealed first, they are indistinguishable because `Slot::Reserved` records no predecessor, and the same-operation held-slot path can accept them. Once cancelled, such a planless anchor leaves later reader changes and retirements refused (fail closed; no forged predecessor). Recovery of such local state is out of scope.
  - Note: `recover_pins` binds proofs to `plan.readers()`, which is empty on a first add, so pins of a reader created in the same operation are recoverable only by a later operation (fail closed).
  - Regressions (`tests.rs`, via the real CLI `admin::run` submit/status/cancel; `admin::Command`/`run` became `pub(super)` for that, no new public API): `admin_reader_and_retirement_submissions_hold_no_slot_and_never_poison_anchoring` (readers and retire), `an_admin_submission_reserved_by_its_planned_actuator_cancels_through_its_plan`, `a_duplicate_admin_submission_of_a_stale_plan_takes_no_slot` (route changed and same route/newer receipt), `a_slot_held_without_its_plan_route_is_refused_and_fails_closed`. Each recoverable scenario ends with a fresh reader add and a retirement naming its receipt. The REST test `reader_fleets_are_denied_until_pin_scope_is_proven` now asserts submission takes no slot and cancel is refused without changing the lifecycle entry.
  - Evidence: before the fix, `cargo test --features server --lib epic_005` gave 29 passed / 4 failed (the admin submission was `reserved`; the planless admin slot made the driver and B's add refuse `readers_unanchored`; a held slot with a different route was adopted `Ok(())`). After: `epic_005` 33 passed; focused `tenant_provisioning` 150 passed; full lib 2284 passed / 0 failed / 10 ignored; clippy `-D warnings`, rustfmt and `git diff --check` clean.
  - Remaining blockers otherwise unchanged: ITEM-012 and ITEM-013 stay **Partial**; the production proof, composition (a control worker driving the actuator from accepted submissions) and qualification blockers listed above remain.
  - Worker prerequisite (retained): an accepted, unreserved non-create operation cannot currently be withdrawn (cancel is `transition_unavailable` until its actuator reserves). Before control-worker composition is enabled, cancellation/withdrawal must be atomic with the plan-bound reservation (`reserve_after`), so a withdrawn operation can never later reserve.
- **a42b2c87 review follow-up (docs/comment only):** the final focused review cleared R1 for every current path with no new high-confidence code defect (static review only). The parent independently ran 150 focused `tenant_provisioning` tests passing on a42b2c87. This follow-up corrects the origin of legacy unbound reservations (EPIC-002 1d8000b9 through e36403e1), states in `docs/HA-ACA.md` that a same-route/newer-receipt legacy slot can be accepted rather than claiming universal refusal, and records the withdrawal prerequisite above. No behaviour changed; ITEM-012/013 stay **Partial**, EPIC-005 is not done, and all real gates stay closed.

### EPIC-007 implementation notes (offline, non-migration slice)

- **Scope:** only the non-migration offline slice under the owner's continuing direction (top note). No Azure/ACA call, deployment, qualification campaign, gate/policy/threshold change, auto-failover change, push, merge or PR. EPIC-006 migration is excluded: no migration implementation, data barrier, drill or prerequisite was added. The protected parent HA PRD was not edited.
- **ITEM-018 (Done, bounded offline scope; not the gate-owner exit):**
  - `docs/HA-ACA.md` gains "Managed tenant deployments (local intent only, not ACA-qualified)": a request/local/qualified table, opt-in setup, create, status, resume/cancel, readers and retirement, migration/release **excluded** (boundary only, no workflow), retained data, uncertain outcomes (including legacy unbound slots, no automated repair), schema upgrade/rollback (fail-closed older readers, roll forward, never strip lifecycle state), unproven effective permissions (ITEM-008, OD-006), and a table of local suite selectors with what each does and does not show. The reader-fleet "fixtures do not exist yet" sentence is replaced by the new replacement/route-lag pins.
  - `docs/API.md` gains "Managed tenant deployments (intent only, opt-in, off by default)": routes, opt-in requirements, bodies and the exact refusal codes, checked against `admin.rs`/`tenant_provisioning.rs`.
  - `deploy/azure-container-apps/ha/README.md` (not the protected parent README) adds the intent-only admin surface, the local check commands and what they do not prove.
  - `src/server/tenant_provisioning/tests.rs` (bounded file justification: ITEM-018 asks for local fault-suite integration; this is the existing suite, using existing `epic_005` helpers, no new public API or production change) adds two regression pins for gaps recorded by EPIC-005: `a_failed_replica_update_dispatches_once_and_waits_for_a_healthy_fleet` (a failed replica, alone or beside a healthy one, stays `replica_ineligible`; the fleet seals with an all-healthy observation on that same active revision after one in-place Update, with route digest and writer unchanged and no stop/delete; it does not prove ACA revision rolling or replacement) and `a_route_change_outside_the_lifecycle_refuses_reader_changes_and_retirement` (a directory edit after the reader receipt makes reader changes refuse `readers_unanchored` and retirement refuse `retirement_receipt_mismatch` with no slot or effect; restoring the original route in the fixture re-anchors and retirement succeeds, not an operator instruction to undo a deliberate rebind or writer/epoch move). Both passed on first run: they pin existing behavior, not new behavior. Policy lag in the sense of reader pins/RBAC propagation is not modelled locally and stays with ITEM-008/ITEM-019.
  - Evidence on 6a7bb4ff: the parent independently passed 152 focused `tenant_provisioning` lib tests, 1 `server_cli tenant_deployment` test, 6 OpenAPI-contract tests, the static admin-route inventory and `git diff --check`. The implementer separately reported rustfmt clean. These are local fake/test-only results, not qualification; the commit message does not contain test results.
  - Review follow-up: the independent static review found no production defect. Documentation now scopes retries to the same surface/principal, forbids treating fixture route restoration as an operational rollback, bounds replacement/policy-lag claims, gives verb-specific `--tenant` syntax, and distinguishes 202 submissions from 200 terminal duplicates and auth/body errors from the unconfigured refusal. Test naming/comments were corrected without changing assertions or production behavior. ITEM-013 remains Partial; ITEM-019 and the epic's qualification exit stay blocked.
- **ITEM-019 (updated 2026-11-24):** isolated Azure test deployment/evidence collection is authorized (top note). The initial target refused access, but the owner explicitly selected a different test target and access now succeeds. Full-profile MEAS-004 `m4full24a` has started from clean `c37e39f6`; resource creation and image preparation are in progress. This narrow gateway measurement does not qualify provisioning, isolated reader pins, failover or production SLOs. Real provisioning qualification still needs the implementation/proof prerequisites. This is not waiting for another routine owner approval.
- **Still open (unchanged):** ITEM-008 pins/effective permissions; OD-002 real ARM adapter, prior-actuator and stopped-replica proofs; fleet observation source; OD-005 clock/freshness; OD-006 profile schema, quota/budget and writer mount exclusion; OD-004 credential view/delivery; control worker/private readiness/REST-CLI composition with atomic withdrawal; parent ITEM-021 reader routing; MEAS-005/006; GATE-005 (auto-failover default off). All thresholds unchanged. EPIC-007 is not complete as an epic.

### Isolated Azure qualification preflight (2026-11-24)

- The new authorization permits test deployment/evidence collection, not bypassing `Assert-VLiteHaQualification` or claiming that a deployed test image establishes production readiness. Use separate owned resources and synthetic data; preserve the singleton and existing service dependencies.
- The default Azure CLI subscription was not the established test subscription. Read-only calls explicitly targeting the established Visual Studio Enterprise subscription were refused with `AADSTS530035`. No account context was switched, no alternate access path was attempted and no resources were created, updated or deleted.
- **Superseding target selection:** the owner explicitly designated another subscription for the test deployment. Read-only access, required provider registrations and effective management permissions succeeded on that target. The operator verifies the explicit target on every call rather than using the CLI default. No identity, security-default or host-network policy was changed.
- Recovered prior MEAS-006 run `m6lf22a` from clean `57fc5ee501f58f18f4e7aaf593faed99ff7970dc`: the measurement and ACA job succeeded; the harness's overall failure was its deletion wait. The separate cleanup follow-up confirmed the group absent at `2026-11-23T07:58:23.7692170Z`. The decoded evidence SHA-256 matches `2f26bd251bbc7c12d60d2ebdd00b569c902346447e6bd0bbafa4012a7d52c7cb`; historical summary/postflight were not changed. This corrects the stale "no live MEAS-006 evidence" task description. The evidence explicitly excludes stale-acknowledgment windows, clock skew and SLOs, so it does not close those qualification gaps or justify a blind rerun.
- **Full-profile gateway attempt `m4full24a`:** the earlier MEAS-004 tiny pilot already succeeded; the missing full-load profile is selected instead of repeating it or MEAS-006. The new attempt uses clean `c37e39f6fd43aafedf4e131a25739de91d687467`, a matching Linux native build and the unchanged full-profile settings in the existing disposable harness. Exact-fixture summary/export validation and cleanup passed with zero Azure calls before launch. Local HTTP validation used a private loopback namespace after an independent localhost probe reproduced the host routing failure; shared routes and firewall were left untouched. No overlapping owned measurement groups existed at launch. The fresh tagged group exists and ACR image preparation is running; app deployment, load evidence and verified group deletion remain pending. Results stay `proposal_evidence_only`, with synthetic writer-only/authority-off traffic and fixed gateways, not provisioning or HA acceptance.
- Reader-pin follow-up `bdc0562c` and its mixed-cap/cleanup correction `80e86b2b` are local only (ITEM-008 notes above). The bounded handoff and CLI integration checks passed; production permission and mount proofs remain missing. The gateway attempt does not enable isolated pin mode or include the correction. Production `pin_scope_unproven` remains enforced.
