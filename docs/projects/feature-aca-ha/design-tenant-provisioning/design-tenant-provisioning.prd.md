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

> **Superseding note (EPIC-001 only):** the owner later authorized offline implementation of EPIC-001 (ITEM-001..003) — “go ahead with above fixes” and “Start above together in parallel.” That authorization covers EPIC-001 code and fake-store tests only. EPIC-002..007, cloud/Azure execution, deployment, qualification runs, gate/policy changes, merge and PR creation remain unauthorized. See section 15 for the implementation notes.
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
| CON-001 | This task MUST edit only local design docs; no runtime/cloud/main checkout/protected-file/pool/PR actions. Superseded only for EPIC-001's listed offline code/test files by later owner authorization; every cloud/main/pool/PR exclusion still applies. | Explicit task boundary. |
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
| Offline implementation proposal | EPIC-001 first (now authorized and implemented offline), then dependent items after explicit authorization. Fake cloud/store tests and schema fixtures. | No Azure credentials required. Preserve legacy formats/API defaults; no automatic cloud enablement. |
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
| RD-002 | Current-task boundary: design documentation only; no runtime implementation, production deployment or PR. Retain the owner's existing constraint against new external service dependencies. Later narrowed: the owner authorized offline EPIC-001 code only; deployment, cloud execution and PRs stay excluded. | Narrow scope and protect the singleton. Worker-pool nonuse was a planning-execution choice, not a new owner policy. |
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

EPIC-001 is the only authorized epic and is implemented offline (see its status column and section 15). All other work below is **Not Started** and requires separate implementation authorization. Completion is by listed tests/proofs, not estimated time or a historical checkpoint number. Each epic has at most seven unique code/test/doc files; later-discovered surfaces require a separately bounded item. Existing parent epics remain Done at their recorded primitive/scaffold scope.

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
| ITEM-004 | Add administrator-only create/status/readers/migration/retirement intent endpoints and generation-bound resume/cancel/acknowledge-delivery/replace-credentials/rollback/release actions. Persist action idempotency; reject unavailable transition handlers until their dependent epic is implemented. Create non-routable metadata from the persisted ID. Do not provision from legacy create/public gateway. Test stale/replayed acknowledgements, explicit irreversible release refusal and secret redaction. | Not Started | `src/server/tenant_provisioning/admin.rs`, `src/server/tenants.rs`, `src/server.rs` |
| ITEM-005 | Add equivalent control CLI/status and explicit action verbs using the same generation/idempotency contracts; generic resume cannot acknowledge delivery or release. Register all new routes in classifier/API HTML/OpenAPI. Arguments carry profile/file references, never literal tokens. Validate route inventory and unchanged legacy 201 behavior. | Not Started | `src/bin/vlite-server.rs`, `src/server/gateway.rs`, `site/api.html`, `src/openapi.json` |

### EPIC-003: Non-routable proof, scoped policy and mutation-path interlocks

**Requirements:** FR-001, FR-002, FR-005, SEC-001/002. **Dependencies:** EPIC-001/002, OD-005; source mutation coverage precedes EPIC-006. **Exit:** operation-bound read-only verification is possible without live route/trust bypass; no managed lifecycle bypass remains. Unproven permission/admission coverage is a blocker.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-006 | Produce tenant-only backend projection without credentials/other tenant inventory; implement source-read-start freshness witness, monotonic publication and expiry within the existing `S_ctl`, not transport-read age. Add operation-bound pending read-only validation/proof; keep public gateway unaware of pending routes. Test stalled publisher with healthy projection reads, slow reads, replay/restart/clock regression and blocked clock proof, revocation and inability to mutate. | Not Started | `src/server/control_state.rs`, `src/server/gateway_trust.rs`, `src/server.rs`, `src/server/tenant_provisioning.rs` |
| ITEM-007 | Enforce managed reservation at manual/automatic failover and authority-entry paths, and existing control-state edits. Leave auto-failover disabled. Inventory all selected-tenant mutation/background paths; implement central barriers only where coverage is proven, refuse unsupported surfaces, and persist target validation/write-enable guard. Do not claim a route-layer check covers queued work. | Not Started | `src/server/auto_failover.rs`, `src/server/failover.rs`, `src/server/writer_authority.rs`, `src/server/control_state.rs`, `src/server.rs` |
| ITEM-008 | Prove effective tenant-scoped Blob/mount access, including necessary reader pin writes without authority/global-control writes. If existing object placement cannot enforce it, return a concrete blocker and a bounded follow-up for isolated metadata/pin brokering before ACA execution; never broaden grants as a passing test. | Not Started | `src/server/tenant_provisioning.rs`, `src/server/gateway_trust.rs` |

### EPIC-004: Idempotent ACA prerequisite and writer provisioning

**Requirements:** FR-001/002/006, NFR-001/002, SEC-002, PAT-001. **Implementation prerequisites:** EPIC-001..003; unresolved OD-002/005/006 may be represented only as refusal cases in local fixtures. **Execution gates:** resolved profiles/actuator proof, applicable HA qualification and separate operation authorization. **Exit:** mocked exact-resource histories satisfy create sequence; real execution remains gated.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-009 | Implement deterministic ARM effects adapter with complete revision inventory, operation polling/readback, effective prerequisite/ownership checks and persisted action IDs. Prove previous-actuator exclusion; stop if platform condition cannot be enforced. Reuse HA projections/guards and add mocked timeout/collision/overlap cases; do not invoke current blocked script as a success shortcut. | Not Started | `src/server/tenant_provisioning/aca.rs`, `src/server/tenant_provisioning/tests.rs`, `deploy/azure-container-apps/ha/templates.ps1`, `deploy/azure-container-apps/ha/test.ps1` |
| ITEM-010 | Compose private seed, authority claim, archive/revision/readiness/policy proof, candidate credential issuance/private delivery acknowledgement and CAS activation using existing primitives. Implement create-stage replacement/revocation after lost delivery. Preserve seed/archive on crash; refuse claimed-authority reset. Missing proof or delivery keeps route absent; activation creates no readers. | Not Started | `src/server/tenant_provisioning.rs`, `src/bin/vlite-server.rs`, `src/server/tenant_provisioning/tests.rs` |
| ITEM-011 | Add profile/peak-quota/cost preflight, guarded pre-activation cancel and separate active retirement: CAS route/admission removal, drain/stop proof, owned-pin cleanup, immutable terminal seal and reservation release, always retain DB/archive. Test crashes at each retirement boundary; unknown calls prevent completion. Document provisioned vs verified prerequisites, no singleton adoption and both unchanged HA refusals pending protocol acceptance. | Not Started | `src/server/tenant_provisioning/aca.rs`, `deploy/azure-container-apps/ha/README.md`, `src/server/tenant_provisioning/tests.rs` |

### EPIC-005: Live reader lifecycle

**Requirements:** FR-003/006/007, NFR-002, SEC-002. **Implementation prerequisites:** EPIC-003/004 and testable reader profiles. **Execution gates:** OD-003/005, qualified reader proof and separate cloud authorization. **Exit:** private-copy add/remove/drain/reseed/pin sequences pass local integration and never change writer intent; local success does not enable reader routing in ACA.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-012 | Compose durable reader desired-capacity updates with existing enrollment/follower, unique replica ownership and authority/binding/staleness eligibility. Test same-endpoint scale changes and first-endpoint CAS; block frozen-base/replaced-head readers and never mount writer storage. | Not Started | `src/server/tenant_provisioning.rs`, `src/server/tenant_provisioning/aca.rs`, `src/server/reader_apply/follower.rs`, `tests/tenant_provisioning_readers.rs` |
| ITEM-013 | Implement guarded scale-down/drain and durable replacement of failed followers; for untargetable ACA scale-in use writer-only routing until the reduced fleet is verified. Add administrator/control-owned abandoned-pin recovery using existing pin primitives with stopped-replica proof. Test full pins, crash/unknown termination, policy lag, capacity failure and unchanged writer intent. | Not Started | `src/server/cloud_backup.rs`, `src/server/reader_apply/follower.rs`, `src/server/gateway_mode.rs`, `src/server/tenant_provisioning.rs`, `tests/tenant_provisioning_readers.rs` |
| ITEM-014 | Document operator desired/observed fleet status, cleanup debt, eligibility and limits; keep reader routing off until applicable qualification. Do not turn a configured scale ceiling into an SLO. | Not Started | `docs/HA-ACA.md` |

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
| ITEM-018 | Integrate local fault suites and administrator docs for create/status/resume/cancel/readers/migrate/release, schema upgrade/rollback, effective permissions, retained data and uncertainty. Explicitly distinguish implemented local behavior from qualified ACA behavior. | Not Started | `docs/API.md`, `docs/HA-ACA.md`, `deploy/azure-container-apps/ha/README.md` |
| ITEM-019 | Under future explicit authorization only, collect applicable existing gate/measurement evidence for policy propagation, fencing, readers, revision overlap and storage. Keep missing MEAS-005/006, GATE-005 and power-loss proof open until real accepted evidence exists; no policy/threshold edits or Boolean bypass. Record outcomes, costs and verified cleanup without production changes. | Not Started | `docs/projects/feature-aca-ha/feature-aca-ha.prd.md`, `docs/HA-ACA.md`, `deploy/azure-container-apps/ha/README.md` |

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

The owner's "go ahead with above fixes" / "Start above together in parallel" instruction authorized EPIC-001 code only. The earlier "design only" boundary still applies to every other epic. Cloud execution, deployment, qualification, gate/policy changes, merge and PR creation remain unauthorized.

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
