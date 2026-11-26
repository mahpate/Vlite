# ITEM-008: effective reader pin scope and HA production-proof delivery

**Status: INCOMPLETE for production validation.** Report-only investigation of commit `19dfffe8807c640afa784483887dcea96fb15e9c`. No application changes, deployment, Azure API calls, production credentials, commits, branch changes, or pushes were performed. Public Microsoft Learn documentation was read without authentication. Implementation and additional tests below are proposed work, not implemented or qualified by this report.

## Executive finding

The important layout correction already exists: opted-in readers can pin with separate coordination objects instead of rewriting writer set records. ITEM-008 is therefore **not blocked solely on inventing that object layout**. It remains blocked on effective identity permissions, complete tenant-scope inventory, runtime configuration/version compatibility, mounted-storage exclusion, trusted observation and proof delivery. Production constructors for `PinScope`, qualification, quota, budget, clock, fleet-observation and stopped-replica evidence do not exist.

There is also an immediate wiring gap: `New-VLiteHaReaderApplyConfig` does not emit `isolated_pins`; the runtime defaults it to `false`. Applying the proposed restricted reader grant to the current HA reader template would break legacy pin writes, not qualify a reader. Fixing the template alone must not open the gates.

"Dedicated tenant writer/reader" and "ACA Dedicated workload profile" are different requirements. The current managed contract models a tenant-owned writer and one independently scalable reader **app**, not multiple independently managed reader apps. The HA scaffold supports only the ACA `Consumption` profile. Dynamic tenant-owned resources can be delivered on Consumption without claiming Dedicated-tier capacity support. Writer horizontal scale is not the requested safe extension: keep one active database owner and independently scale readers.

## Checks actually executed

| Check | Result and scope |
|---|---|
| `git rev-parse HEAD` and `git diff --check` | Source revision recorded above; initial whitespace check passed. |
| PowerShell isolated execution of the existing grant assertions in `deploy\azure-container-apps\ha\test.ps1:192-293`, with its synthetic configuration/helpers | Passed. This executed the real pure grant builder, exact role/condition assertions, deterministic role/assignment identities, nested/equal prefix refusals, malformed/newline inputs, shared-key refusal, and explicit-empty sibling inventory acceptance. Only the pure block was evaluated; the deployment orchestration and its test-only verifier replacement were not run. |
| Direct call to unmodified `Assert-VLiteHaQualification @{}` | Passed the expected refusal check: `HA deployment is blocked:`. No verifier replacement, deployment invocation, or Azure call. This is evidence of fail-closed behavior only. |
| Direct call to `New-VLiteHaReaderApplyConfig` using the synthetic test configuration | Confirmed generated configuration omits `isolated_pins`. Together with the statically inspected default this establishes the template/runtime wiring gap. |
| Isolated `cargo test --offline --locked --features server --lib reader_pin` | Passed: 9 tests, 0 failed/ignored. Real Rust library tests with fake/scoped stores and test-only proofs; includes isolated follower enrollment and control-owned recovery. Cargo cache, target output, and runtime temporary data were directed outside the checkout; existing registry data was copied rather than modifying shared caches. |
| Isolated `cargo test --offline --locked --features server --lib tenant_provisioning -- --quiet` | Passed: 247 tests, 0 failed/ignored. Local durable journal/CAS, profile, worker gate, fake/loopback ARM, reader and activation contracts; not Azure observations. |
| Isolated `cargo test --offline --locked --features server --lib cloud_backup::tests -- --quiet` | Passed: 53 tests, 0 failed/ignored. Backup set/pin/retention model regressions, including cap races and cleanup debt; not live Blob authorization or consistency qualification. |

**Not executed:** isolated Azure effective-access probes; ACA runtime/manifest/grant propagation; actual writer mount access, durability or crash tests; live clock qualification; complete platform stop observations; deployment; production gate enforcement with accepted real evidence. These are required release evidence and are explicitly unavailable under this assignment. Public documentation and fake-store assertions are not substitutes.

The full HA orchestration suite was deliberately not executed: it replaces the blocked verifier inside its test process (`ha\test.ps1:724-725,783-785,924`). This report used the pure grant block and untouched gate instead. No Azure emulator or measurement/deployment script was started.

## Current implementation: exact evidence

All references are checkout-relative, with 1-based line numbers at the source revision above. Paths are shown with Windows separators; remote Blob object names use `/` because that is the protocol, not a local filesystem path.

| Surface | Evidence and implication |
|---|---|
| Legacy pins | `src\server\cloud_backup.rs:1656-1695,1735-1746`: `pin`/`release` mutate the set record. A grant permitting that mutation also permits mutation of publication/deletion metadata in the same object; object-level ABAC cannot authorize only the JSON `pins` field. |
| Isolated object key and body | `cloud_backup.rs:1177-1182,1748-1800`: `<prefix>reader-pins/<set>/<pin>.json`; `Cond::Create`, then recheck Published state and combined pin count. `cloud_backup.rs:482-501`: body has `id`, `reason`, `created_unix_ms`, `kind`; it has **no tenant, app, revision, principal, binding generation, epoch or enrollment-incarnation fields**. |
| Release/list | `cloud_backup.rs:1829-1880`: validated, idempotent `release_reader`; readable objects returned by `reader_pins`; malformed objects skipped by that decoded view. `reader_pin_names` counts all objects, including malformed ones, so unreadable pins still protect deletion. A skipped malformed object is not proof that no pin exists. |
| Retention/deletion | `cloud_backup.rs:2127-2157,2190-2210,2321-2352`: retention excludes reader-pinned sets; deletion waits after DeletePending; listing errors stop decisions. The post-create Published recheck closes the reader/deleter ordering race under the store's consistency assumptions. |
| Cap/cleanup semantics | `cloud_backup.rs:44,1757-1799,1656-1695`; PRD `docs\projects\feature-aca-ha\design-tenant-provisioning\design-tenant-provisioning.prd.md:823-828`: combined cap is 16, not an atomic physical quota. Overlapping writes can temporarily exceed it; failed rejected-pin cleanup leaves explicit retained debt. `cloud_reader_pin_withdraw_failed` is logged. Do not interpret `MAX_PINS` as safe support for 16 arbitrary reader replicas plus all other pin users. |
| Follower opt-in | `src\server\reader_apply\follower.rs:24-38,244-264`: `isolated_pins` defaults false; isolated enrollment releases same-reason isolated objects, then uses `pin_reader`; legacy enrollment still writes records. Existing isolated enrollment does not remove legacy record pins. |
| Replica identity | `reader_apply\follower.rs:147-150,163-167`: `CONTAINER_APP_REPLICA_NAME`, otherwise `HOSTNAME`, otherwise `pid-<pid>`; pin reason is `reader-apply:<replica>`. No cryptographic or incarnation binding. Production collision/reuse behavior cannot be inferred from fake names. |
| Restricted grant builder | `deploy\azure-container-apps\ha\templates.ps1:592-723`: pure builder, no Azure I/O; custom blob read/write/add/delete role, no control-plane actions; coordination writes only under `<prefix>reader-pins/`; reads under tenant prefix; list request must carry tenant prefix; separate conditioned Blob Data Reader on data container. Output explicitly says `offline_template_effective_access_unqualified`. |
| Scope inventory/identity | `templates.ps1:608-610,630-668,700-720`: required `SiblingPrefixes` is operator-asserted, can be empty, and rejects equality/nesting in either direction. Assignment uses principal object ID; accepted `Target.client_id` is not verified by the builder. All readers within one tenant are cooperative and can modify one another's pin objects. |
| Actual list construction | `cloud_backup.rs:1508-1520,1842-1865`; `src\server\cloud_backup\azure.rs:745-793`: current set/pin lists use prefixed roots; the REST list includes `prefix` on **each** page, bounds pages/object count and refuses duplicate names/repeated markers. This behavior can be established offline for these call paths. It does not establish how Azure evaluates the request-prefix condition or whether a future path remains compatible. |
| HA config gap | `templates.ps1:157-188`: generated reader config has only target/follower/capacity/staleness/poll fields. No `isolated_pins`. This was reproduced by the executed pure check. |
| HA profile/storage | `deploy\azure-container-apps\ha\deploy.ps1:220-222,420-428`; `templates.ps1:415-436,481-500,511-528`: Consumption only; writer min=max=1, no HTTP scale rule; reader uses independent bounds and HTTP scale; writer mounts AzureFile, reader EmptyDir. Desired manifests do not prove live mount exclusion. |
| Strict managed profile | `src\server\tenant_provisioning\aca.rs:195-215,238-313,358-463`: only retained Create writer profiles resolve, owned resources only container apps, singleton names excluded, Single revision required, CPU/memory peak includes rollout overlap. This is not an approved role-specific reader profile, live image qualification, least-privilege verifier, or complete secret detector; neutral command/args keys are explicitly outside its heuristic. |
| Supported ARM IDs | `src\server\tenant_provisioning\resource_id.rs:8-13,25-51`: container apps, managed environments and environment storage bindings only. Role assignments/definitions and storage containers cannot simply be added to the existing `resources` list. Scope evidence needs a separate read-only contract, not a widened generic actuator. |
| Preflight tokens | `aca.rs:554-628`: `Qualification(())`, `BudgetApproval(())`, quota with only `headroom`; constructors are `cfg(test)`. Current preflight checks presence and numeric capacity, not operation/profile/principal/expiry binding. These placeholder tokens cannot be exposed through JSON flags as production proof. |
| Production worker | `src\server\tenant_provisioning\worker.rs:35-61,144-175,178-235`; `src\server\tenant_provisioning\admin.rs:1153-1156`: production passes `Proofs::default`; default scan selects only Create; profile/preflight precede adapter construction and claim; effects follow durable journal. No managed production reader executor is wired. |
| Reader gates | `aca.rs:632-694,1708-1789`; `tenant_provisioning\admin.rs:408-419,538`: PinScope/FleetObservation/ReplicaStopped only constructed in tests; present-reader Resume always refuses `pin_scope_unproven`. Keep REST/CLI parity and cancellation available. |
| Reader apply contract | `aca.rs:1791-1857`: exactly one owned reader app; creates or updates that app without touching writer; verifies fleet before publishing endpoint; removal unpublishes endpoint before stopping. Read routing remains WriterOnly. |
| Fleet eligibility | `aca.rs:1960-2071`: exact desired app readback; current route/head/lineage/epoch, unique replica names, fleet bounds, revision, binding generation and age versus reported bound. Observation has no production source, timestamp/expiry or completeness witness. Merely supplying a smaller replica vector could falsely satisfy scale-in without such a witness. The reported bound also needs binding to the approved reader configuration. |
| Pin recovery | `aca.rs:2080-2175`: needs reader reservation, PinScope and ReplicaStopped; stop binds tenant and planned reader app; candidate pins bind only reason/kind/creation time. Journaled release handles both legacy and isolated objects. This is not proof that a recycled replica name cannot be live, nor that the supplied Sets target belongs to the proof's tenant. |
| ARM stop semantics | `src\server\tenant_provisioning\arm.rs:603-618,1030-1092`: revision `active` is normalized as platform-active **or replicas > 0**; missing replica count is malformed, not zero. The production adapter therefore does not equate inactive flags alone with stopped processes. Completeness, freshness and future dispatch exclusion still need qualification. |
| HA qualification gate | `deploy.ps1:319-327,401-512,656,692`: unconditional blocker; reads prerequisites and checks isolation first, but gate occurs before local private state/cloud mutation and again during writer handoff. User approval for experimental Azure Files is not qualification. |
| Activation remains blocked separately | `src\server\tenant_provisioning.rs:1425-1509,1721-1801`; `tenant_provisioning\projection.rs:359-376`: readiness needs clock evidence; operation-bound route proof requires authority/head/revision/policy, and activation additionally needs acknowledged live credential view/revocations. OD-004 and OD-005 remain necessary even after ITEM-008 is solved. |
| Worker runtime skeleton | `templates.ps1:724-735`: offline manual Job, parallelism/completion=1, retries=0, separate control identity and only control-target projection. This limits invocation count; it does not prove a former actuator cannot dispatch or create a durable private observation channel. |

### Documentation contradictions to correct with implementation

`tenant_provisioning.rs:446-462` and `aca.rs:632-635` describe only the legacy whole-record obstacle; the denial remains correct but the explanation is no longer complete. `docs\HA-ACA.md:982-985` similarly says no tenant-scoped grant exists, while `HA-ACA.md:509-552` accurately documents the isolated layout and unqualified ABAC template. The PRD `:802-816` is a historical assessment followed by the implemented slice at `:817-856`; do not cite its earlier "no reader actuator/template" language as the current entire state. `ha\README.md:243-246` says the managed code has no production ARM adapter, contradicted by `worker.rs:48-61` and the later runbook `HA-ACA.md:1005-1011`.

The correct statement is: **an ARM adapter and offline reader actuator/layout exist; production execution and accepted scope/observation proofs remain blocked**. Documentation history and previously reported test counts are not checks executed by this worker.

## What can be established offline, and what cannot

| Question | Offline determination | Required isolated Azure observation / remaining gap |
|---|---|---|
| Does isolated pinning require record/control/data writes? | Exact code path and fake scoped-store tests can show only reader-pin mutations; legacy behavior remains opt-in-compatible. | Actual running pinned image and projected config must select that path; actual MI actions must be accepted only there. |
| Are ABAC expressions generated deterministically and safely? | Exact role/action/condition strings, input grammars, role/assignment IDs, nesting refusals and no implicit services. | Whether those exact assignments are accepted and enforced on the approved account tier/namespace/API. No local condition interpreter can qualify Azure. |
| Do current list requests include prefix? | Yes for inspected set/pin paths and REST pagination. Add request-recording tests for all follower operations. | Azure must honor prefix conditions on first/continuation pages and distinguish list from object reads. |
| Are effective grants restricted? | Desired assignments only; a captured complete inventory can be parsed offline. | Principal/group/inherited/eligible/active grants, ACLs if applicable, broad alternate assignments, alternate identities, key/SAS/delegation paths and propagation. A restrictive assignment is not a global deny. |
| Is sibling tenant inventory complete? | Detect overlaps and stale/invalid inventory under a specified snapshot/generation. | Source-of-truth ownership and complete inventories cannot be invented from the supplied sibling array. New tenant allocation must preserve disjointness atomically. |
| Can the reader access writer storage? | Template lacks AzureFile mount; identity/template declarations are inspectable. | Actual container mounts/secret projections, environment share bindings, inherited management privileges and ability to change mounts/identities. Desired absence is not enforcement. |
| Are pin lifetime/cap/deletion safe? | Deterministic race and fault fixtures; malformed pins retain protection; explicit cleanup debt. | Blob consistency/permission behavior, failed cleanup and propagation during concurrent runtime operations, real replica-name churn/reuse. |
| Can a stopped replica/actuator never act again? | Model refusal and journal reconciliation; no takeover from elapsed time. | Complete all-revision/all-replica observation and qualified identity/dispatch exclusion. Zero observed replicas once does not prove a paused actuator cannot issue ARM later. OD-002 may require a platform-enforced execution/authorization boundary that this repo has not demonstrated. |
| Can freshness and archive recovery be proven? | Hash/lineage/epoch checks and refusal without clock proof. | Qualified clock uncertainty for each participating instance and authenticated private runtime evidence; actual archive commit/readback and mount recovery. A configured skew number is not measurement. |
| Are capacity and budget approved? | Demand math and signed approval constraints. | Fresh actual headroom/rollout overlap and account/environment constraints; simultaneous tenants require aggregate capacity reservations. A benchmark or cost placeholder is not budget approval. |
| Is reader routing qualified? | WriterOnly gate and endpoint-publication ordering. | Reader serving/staleness/reseed/zone-loss and gateway policy convergence measurements. Keep ITEM-021/MEAS gates closed. |

## Concrete implementation proposal (not applied)

### 1. Separate evidence collection, verification and mutation

Use the existing Rust dependencies (`serde`, `serde_json`, `sha2`, `ring`, `reqwest`, `tempfile`) and PowerShell/approved Azure CLI; no external service, additional database or package is necessary. Add one small internal evidence module under `tenant_provisioning`, plus a dedicated HA collection script only when implementation is authorized. Do not repurpose a deployment switch as an attestation, and do not add public HTTP admin access to trusted reader backends.

Three inputs have different authority:

1. **Qualification certificate:** owner-approved findings from an explicitly authorized isolated collection using the exact release image, relevant platform/API/tier/namespace characteristics, grant expressions, pin layout and measurement contracts. Its identity is a signed or otherwise authenticated immutable digest; collection alone does not approve it.
2. **Target binding/inventory witness:** exact operation, tenant, target, principal/client identity mapping, resource/config digests, sibling/control snapshot generation, effective grants and live mounts. Synthetic identities cannot be relabeled as production identities. A later authorized target check must establish the equivalence/binding; it is unavailable in this task.
3. **Fresh runtime witness:** observation interval, complete revision/replica inventory, authenticated follower state, authority/head/policy/binding, current headroom, clock qualification and dispatch exclusion when required. Short-lived facts cannot be replaced by a historical qualification certificate.

Store approved immutable evidence and private runtime witnesses in the existing **control-owned** operation-content/projection namespace, inaccessible for reader writes. Raw sensitive observations stay in owner-private output; only explicit nonsecret fields enter review reports. Reuse retained-content digest verification and the journal/CAS protocol, not reader-writable blobs.

Suggested strict envelope fields:

```text
schema = "vlite.tenant-ha-evidence/1"
purpose = qualification | pin_scope | quota | budget | fleet | stopped
evidence_id, run_id, collector_revision, collector_image_digest
subject:
  operation_id, request_digest, tenant_id, profile_digest, scaling_rule_digest
  image_digest, platform, resource_config_digests
  target_fingerprint, reader_pin_layout_version, isolated_pins
  identity_resource_id, principal_object_id, client_id, identity_inventory_digest
  sibling_inventory_digest, control_generation, effective_grants_digest
  environment_id, workload_profile, account_kind/tier/namespace/API
observed_start, observed_end, expires_at, clock_evidence_digest
artifacts: [{kind, sha256, bytes, complete}]
checks: [{id, expected, actual, status, observation_digest}]
approval: {approved_purposes, subject_digest, limits, signer_id, signature}
```

These are private evidence fields, not permission to log resource IDs or credentials publicly. Actual evidence may contain opaque IDs needed for exact binding; published summaries use approved opaque fingerprints. Never include bearer tokens, MI headers/endpoints containing secret material, signing seeds, account keys, SAS strings, connection strings, raw CLI diagnostics or app-secret bodies. Whitelist safe fields **before** serialization; do not dump a raw response and hope regex redaction catches everything.

Strictly reject unknown/duplicate keys, wrong schema/purpose, ambiguous ARM-ID casing aliases, conflicting identity IDs, oversized inputs, duplicate/missing artifacts, digest mismatch, nonfinite/overflow values and incomplete checks. Bound both total bytes and list counts. Preserve byte hashes for retained documents; use a separately defined canonical structural digest for profile/target/evidence subject. `digest::config_digest` already supplies profile semantics; do not silently interchange that digest with a raw file SHA-256.

For signature verification, use the existing `ring` capability with an owner-approved key distinct from gateway request-signing keys. A signature proves provenance, not the truth or freshness of the observations. Trust-anchor provisioning/revocation is an explicit owner decision; until that contract exists, return the current blocker.

### 2. Replace placeholder proof tokens with context-bound verified types

Keep constructors private and accept no deserialized `PinScope`/`Qualification` directly. Make proof values store the fields needed to enforce use, rather than `()`:

```rust
// Proposed shape; names/helpers require implementation and review.
struct VerifiedPinScope {
    evidence_digest: String,
    operation_id: String,
    request_digest: String,
    tenant_id: String,
    profile_digest: String,
    target_fingerprint: String,
    principal_object_id: String,
    client_id: String,
    grants_digest: String,
    sibling_inventory_digest: String,
    pin_layout_version: u32,
    expires_unix_ms: u64,
}
```

Use equivalent bindings for qualification, budget, quota and fleet proofs; budget additionally binds numeric cost/replica/time limits, quota binds aggregate demand and reservation generation, fleet binds the complete observed universe plus time interval and approved reader bounds. Verification returns typed `Result` errors; retain current missing-proof error codes and add narrowly scoped invalid/stale/mismatch blockers where needed. Do not convert verification errors into empty/default proof values.

**Required verification algorithm:**

1. Load immutable retained evidence through the trusted control role; validate bounds/schema/provenance/digests.
2. Match purpose, operation/request/tenant/profile/scaling-rule, exact target/client/principal/config/image, supported layout and approved platform qualification.
3. Verify complete disjoint scope inventory, effective access and mount exclusion; refuse unknown alternate permissions, credentials or incomplete inventory.
4. Check freshness using the approved clock contract, both the observation interval and expiry, with uncertainty deducted conservatively. An evidence file with a timestamp is not a clock proof.
5. Compare desired peak demand and budget bounds; acquire a control-owned aggregate capacity reservation using CAS before any app dispatch. Reconcile that reservation with immutable operation journal; never release it merely because a worker lease elapsed.
6. Revalidate relevant generations/expiry immediately before each irreversible dispatch, route publication and pin recovery. Record the exact evidence digest with the journaled request. Drift or expiry blocks **new effects**; authorized read-only reconciliation may still resolve an already dispatched uncertain operation.

`preflight` currently receives a Profile without operation context (`aca.rs:592-628`); add that context rather than merely making constructors public. `recover_pins` must verify the supplied Sets target fingerprint against the tenant/evidence before listing or deleting. `eligible` must require authenticated, complete, fresh observations and compare the reported staleness bound/capacity/config to the approved profile, not only `age <= reported_bound`.

Clock qualification, private runtime observation provenance and previous-actuator exclusion remain fundamental upstream contracts. Implementing the envelope parser must not fabricate these proof types.

### 3. Wire every dynamic-reader surface, without broadening writer access

* Define a strict retained reader-profile and scaling-rule schema for `Payload::SetReaders`, currently unsupported by `Profile::resolve`. Resolve both retained digests; verify role=reader, exactly one owned app, correct tenant app, private ingress, approved MI mapping, EmptyDir-only follower storage, no writer/admin credentials or writer mount bindings, approved image, bounds and `isolated_pins=true`. Do not treat arbitrary JSON body plus a valid config digest as reader qualification.
* Extend worker selection/execution explicitly for SetReaders only after plan anchoring and production proof contracts exist. A present fleet must pass scope/profile/qualification/quota/budget before claiming or calling ARM; the latest receipt must anchor the reservation and endpoint. Removal requires route unpublish, proven stop and safe recovery before sealing. Preserve cancel/withdrawal races and unknown-outcome reconciliation.
* Replace the unconditional present-reader Resume refusal only with the same verified executor path for both CLI and REST. Missing proof must still return `pin_scope_unproven`; `next_step` must name the missing evidence without emitting sensitive contents. Do not let Resume directly manufacture a reader reservation.
* Make `New-VLiteHaReaderApplyConfig` explicitly emit isolated pin mode only in the future qualified reader path; retain the legacy runtime default for existing installations. Test the exact secret-projected JSON and actual runtime field set. All writer/janitor binaries serving that prefix must honor reader pin objects **before** enabling it; certificates bind every relevant version.
* Keep `New-VLiteHaReaderStorageGrants` a pure renderer. Applying roles/assignments is a separately authorized action, not an implicit side effect of tenant creation. Replace caller-asserted sibling inventory with a digest-bound authoritative control snapshot, or require an approved snapshot plus CAS-controlled disjoint prefix allocation. Verify the principal/client mapping separately.
* Keep the production HA gate before private state or cloud mutation. A future `Assert-VLiteHaQualification` must consume verified context-bound evidence through a shared contract, recheck at both handoff sites and fail on missing/unknown checks. Keep `AllowUnqualifiedWriterStorage` strictly nonproduction; neither it nor an evidence filename makes Azure Files production-qualified.
* Keep route read policy WriterOnly until independent reader routing qualification. Publishing a reader endpoint is not permission to change policy to ReaderPreferred. Keep Create activation behind authority/archive/readiness/clock/credential-delivery gates.
* For ACA Dedicated-tier support, add a distinct explicit qualified workload-profile schema and measured capacity envelope; change the Consumption-only checks only when that qualification exists. Retain demand overlap and single-writer constraints. Multiple reader apps instead of replicas would require a new plan/resource/fleet schema; it is not a transparent `maxReplicas` change.

### 4. Pin recovery: preserve safety over convenient cleanup

The present tenant-prefix ABAC design provides tenant isolation, **not per-reader hostile isolation**. Any same-tenant reader can delete another reader's protection. That assumption must be accepted explicitly or the pin authority must move to a control-owned broker. Metadata fields alone do not change object permissions.

For robust automatic cleanup, introduce an enrollment incarnation bound to app/revision/replica/principal and operation/binding, not merely the mutable replica-name reason. The control role verifies it with the stopped proof and rechecks no live matching incarnation before release. Because `Pin` denies unknown fields, add a versioned reader-pin format with coordinated reader/janitor/recovery rollout; do not append fields to existing JSON and expect all versions to ignore them. Preserve old records/objects and treat ambiguous legacy pins as manual verified cleanup debt.

A lease/TTL is only a liveness mechanism if a reader loses readiness and stops using the protected set before expiry under qualified clock/scheduling bounds. **Never delete a pin solely because its timestamp is old.** A resumed/paused reader might still depend on it. If per-reader ABAC segments are selected, redesign key enumeration, conditions and completeness proofs together; currently keys are set-first and tenant-wide.

Treat 16 combined pins as a feasibility constraint. Count operator/restore/drill pins, two follower generations, concurrent reseeding/enrollment and cleanup debt at requested maximum fleet size. Require measured bounded occupancy; reject configurations that cannot fit, or design a qualified control-broker/atomic allocation protocol. Do not merely raise `MAX_PINS` to make 1,000 HA reader replicas appear supported.

## Safe isolated Azure collection specification (future, separately authorized)

This section is a specification, not an executed runbook or authorization to call Azure.

Use preapproved disposable resources and synthetic tenant A/B prefixes on the approved account class, with the candidate release image and isolated MI identities. Setup/assignment creation is explicitly separate from collection. Collection must verify exact subscription/resource/environment/identity/prefix allowlists and ownership; refuse singleton names, production resources, broad ancestor scopes and unknown inventories. The tool must not select/switch subscriptions, install extensions, look up keys, create production grants or apply application deployments. No negative destructive probe touches real application objects.

Collect read-only control-plane inventories with approved observer identity; run **data-plane probes from the actual isolated reader identity/runtime**, never from the more privileged observer/CLI login. Authenticate the identity-to-principal/client mapping and actual process/image/config/mount context without exporting tokens.

| Required probe | Safe synthetic fixture and expected evidence |
|---|---|
| Positive isolated pin create/read/delete | Create-only PUT on a published synthetic set's own tenant reader-pin path, get/list it, release idempotently. Capture method/path fingerprints, conditional headers excluding auth, status and service request IDs; control/data sentinels remain unchanged. |
| Forbidden coordination writes | On non-live sentinel blobs representing `sets/.../record.json`, authority, lifecycle guard, auto-failover and operation/projection objects, attempt create/overwrite/add/delete as reader. Expected authorization denial, not 404/not-found success. Fixture owner verifies hashes/ETags and existence after each attempt. |
| Forbidden data writes | Synthetic manifest/archive/database blobs: create/overwrite/add/delete denied; allowed tenant reads still succeed. Do not run destructive probes against an actual authority or archive. |
| Cross-tenant scope | Read/list/create/update/delete against existing tenant B sentinels and control prefixes denied; distinguish absent-resource errors from denied authorization. Test nested/equal prefix rejection offline before collection. |
| List prefix behavior | Own prefix and deeper pin/set prefixes accepted; empty/missing/root/sibling prefix denied. Exercise continuation pages, including empty pages with marker, using the actual client/API. Every request page must retain prefix; all expected objects must appear without foreign names. |
| Alternate permission paths | Complete direct/inherited/group/active grants and conditions, role definitions, applicable ACLs, attached identities and management rights; refuse any unexplained broader path. Desired two assignments alone are insufficient. Do not mint a real SAS/delegation key to prove its absence; use approved safe permission observations or explicitly authorized throwaway probes and never publish secrets. |
| Propagation | Record repeated positive/negative outcomes over a measured interval after isolated assignment/condition changes. Positive success alone is insufficient; premature or intermittent forbidden success makes the run incomplete/failed. Capture token/cache renewal context without token values. A fixed sleep is not proof of convergence. |
| Runtime pin/retention | Actual isolated follower enroll/swap/confirm/stop and controlled concurrent deletion/cap faults. Use only synthetic data; verify no writer/control/data mutation, no successful fetch after deletion, retained cleanup debt on failed withdrawal and eventual verified cleanup. |
| Mount exclusion | Actual reader sees only private follower storage and permitted secret projections; cannot read writer share, mount binding secrets or use a management path to attach them. Qualified observation methodology required; absence in generated manifest is not enough. |
| Fleet/stop | Complete all-revision/all-replica observations before/after scale-in or stop, with app/revision/incarnation binding and runtime readiness state; missing page/count, drain or restart invalidates completion. No stop proof from inactive flags, HTTP 200, Job completion or elapsed lease alone. |
| Clock/recovery | Independently approved uncertainty bounds and actual commit/flush/recovery evidence on mounted storage. This probe protocol is not implemented; label incomplete until it exists. |

Collector output must have immutable per-check artifacts and a summary with `status=complete|incomplete|failed`, explicit failed step, missing-check IDs, source/image/config/grant/inventory fingerprints, bounded collection interval and separate cleanup outcome. Preserve successful artifacts after partial failure but do not create an approval-shaped success record. Nonzero exit on missing/unknown/failed required check. Capture only safe structured fields; native diagnostics may contain secrets.

Use process-private temporary directories and logs, explicit deadlines/poll budgets, no implicit retries of conditional writes, and readback reconciliation of ambiguous outcomes. Cleanup targets only owned synthetic fixtures and processes started by the collection. Preserve original failed outcome if later cleanup succeeds. Cleanup failure is recorded separately and blocks the relevant acceptance policy, never rewritten as qualification success.

Isolated evidence can qualify a **class of behavior** for that exact context; it cannot prove production effective grants or that a future paused actuator is fenced. Later authorized target inventories, trusted observation delivery, drift controls and capacity admission are still required. If those cannot be supplied, dynamic production execution remains unavailable rather than accepting a "lab passed" flag.

## Concrete additional test plan

These tests are specifications to implement with the code change. They were not all executed here.

| Test group / placement | Cases and exact acceptance criterion |
|---|---|
| Existing reader-pin regressions, `cloud_backup.rs:4445-4759`, `reader_apply.rs:539-616`, `tenant_provisioning\tests.rs:4063-4125` | Execute scoped-write enrollment/swap/stop, validated/bounded pins, retention blocking, pin/deletion race, overlapping/mixed cap allocation and failed cleanup logging. Assert unchanged control/set/data bytes; denied legacy path; only own-reason cleanup; idempotent recovery. These remain local model evidence, not Azure ABAC proof. |
| Evidence parser/verifier in internal Rust module | Valid minimally complete fixture accepted for its exact context; unknown/duplicate fields, too-large/count-overflow, wrong schema/purpose/signature/artifact digest, missing checks, forged completeness or cleanup, wrong operation/tenant/profile/image/target/principal/client/grant/inventory rejected. Public JSON cannot construct proof types. |
| Freshness/clock | At conservative expiry boundary refuse; just inside bound accept only with qualified clock; future/rollback/uncertainty-overrun refuse; proof expires between preflight and dispatch/publication/recovery => zero **new** mutations. Permit bounded observation-only reconciliation of recorded uncertain effects. |
| Grant/layout/mount wiring | Pure template emits `isolated_pins=true` only for the qualified candidate path; legacy default remains false; projected field set matches Rust parser; exact conditions unchanged; no reader writer-share/admin/control mutation privileges. Reject mixed old janitor/writer versions and evidence claiming tenant isolation from plain prefix alone. |
| Effective access fixture evaluator | Direct conditioned grants plus an inherited broad grant must reject even when intended assignments match. Incomplete group/ACL/identity/sibling inventory rejects. Treat evaluator results as parser/policy tests only, never platform qualification. |
| Worker/admission, extend `worker.rs:640-785` | Every missing/stale/mismatched proof blocks before adapter/claim/ARM; exactly one selection; zero port calls on gate failure. Present reader requires retained scaling rule and latest plan anchor. Cancellation/withdrawal wins CAS => no reservation or dispatch. No retry/takeover without actuator exclusion. |
| Aggregate quota/budget | Peak exactly equals proven available demand accepted; exceed any dimension by one unit rejected; integer overflow rejected. Two tenant operations racing for one remaining capacity slot => one CAS winner, one refusal and no overcommit. Unknown capacity-claim outcome is reconciled, not released/duplicated. |
| Reader fleet, extend `tests.rs` `epic_005` | Incomplete pages/replica vector, stale observation, foreign/duplicate incarnation, recycled name, extra draining replica, wrong revision/set/lineage/epoch/binding, changed authority head, larger-than-approved staleness/capacity bound rejected; zero endpoint publication. Scale-in needs proof of absence of all removed incarnations. Existing endpoint never silently changed. |
| Recovery | Wrong Sets target/tenant/app/incarnation/stop-clock rejected before any listing/deletion; a newer/live same-name incarnation's pins survive; operator/other replica pins survive; unreadable pins remain debt; release uncertain => same recorded action reconciled, never deletion by TTL. |
| Activation | Scope/qualification success alone must not publish route. Missing clock, uncommitted archive head, wrong policy/revision, unacknowledged or unpublished credential, unreconciled revocation all refuse. Route replacement after readiness digest rejected; committed CAS crash recovers exact receipt without redispatch. |
| HA gate/collector PowerShell | Offline mode/invalid scope/missing approval => no Azure invocation and no private state. Mock recorder rejects unknown argv. Failed/malformed/partial inventory/probe => nonzero incomplete output; privileged observer's success cannot substitute for reader probe. Test secret-field whitelist with synthetic token/key canaries and no raw diagnostics in output. No test approval reaches production gate. |
| Approved isolated Azure suite | Execute every required row in the collection matrix on exact approved context. Require positive and negative outcomes, complete inventories, bound propagation and cleanup, reviewer approval and trusted target/runtime binding. Any unavailable mandatory row => INCOMPLETE; no production proof issued. |

Candidate offline commands for the implementation review are `cargo test --offline --locked --features server --lib reader_pin`, `cargo test --offline --locked --features server --lib tenant_provisioning`, and the pure HA template/collector tests with a rejecting Azure mock. A wider suite is appropriate after changes touch profile resolution, admission, pin recovery or activation. Library tests are not a linux/amd64 release-image qualification.

## Fundamental gaps and delivery recommendation

1. **No accepted production evidence authority/channel.** There is no trusted collector-to-worker/backend contract, approval/revocation policy or complete live fleet source. A local file parser is necessary but insufficient.
2. **Effective scope is a whole access graph.** ABAC conditions only constrain their assignment; broad alternate grants can still authorize access. Per-tenant cooperative pin permissions do not protect one reader from another within the tenant. Current sibling inventory is not independently complete.
3. **Actuator exclusion is not a journal lease.** CAS selects an operation owner but cannot stop an already authorized paused process's later ARM request. The repo explicitly refuses takeover without proof. This may be a platform feasibility blocker, not just missing plumbing.
4. **Clock and storage qualification remain unresolved.** Configured times, ZRS, Single revision, fixed writer replicas and `/ready` do not prove power-loss durability, recovery, bounded staleness or stop/fencing.
5. **Reader config/version/capacity assumptions are not wired end-to-end.** Default legacy pins contradict restricted-write reader templates; old janitors can ignore isolated pins; reader-pin incarnation and 16 combined pins constrain safe fleet sizes.
6. **Activation has independent credential/archive gates.** Solving ITEM-008 alone cannot deliver a routable dedicated tenant. Existing measurement reports do not certify those gates: `ha\measure.md:538-539` explicitly says no MEAS-005 evidence exists and the described collections do not qualify serving/SLOs/gates.

Recommended delivery order: implement strict offline evidence/schema and mock checks; correct docs and role-specific reader configuration while retaining default refusals; establish owner-approved observation/clock/actuator/approval contracts; collect explicitly authorized isolated observations; bind/revalidate target facts; only then wire production constructors/executor paths and separately qualify routing/workload tiers. If platform dispatch exclusion or effective access cannot be demonstrated, retain fail-closed gates and choose a narrower control-broker/manual lifecycle rather than claim automatic HA.

## Public sources and limitations

Read on 2026-11-26 UTC, unauthenticated:

* [Microsoft Learn: Storage ABAC](https://learn.microsoft.com/en-us/azure/storage/blobs/storage-auth-abac), sections "Overview of conditions in Azure Storage" and feature status: Blob ABAC is available for supported tiers/attributes. This establishes feature existence, not this deployment's enforcement.
* [Microsoft Learn: Blob actions and attributes](https://learn.microsoft.com/en-us/azure/storage/blobs/storage-auth-abac-attributes), sections "Suboperations" and "Azure Blob Storage actions and suboperations": List uses blob `read` plus `Blob.List`; suboperation/attribute compatibility matters. Exact VLite condition enforcement remains unobserved.
* [Microsoft Learn: Conditions overview](https://learn.microsoft.com/en-us/azure/role-based-access-control/conditions-overview), "What are role assignment conditions?": conditions filter a role assignment and cannot explicitly deny access. Therefore matching the two proposed assignments is insufficient evidence of least privilege.

No credentials were accessed or included. No previous-session or historical PRD test result is represented as current execution. Actual production validation is unavailable under the task's no-Azure/no-deployment restriction, so this report does not claim ITEM-008 or any production release gate passed.

## Completion addendum

The executed Rust selections overlap: their counts are per invocation, not a count of unique tests. All three commands completed successfully using Rust/Cargo 1.99.1 on Windows with `--offline --locked`; no dependency manifests or lockfiles changed, no packages were installed, and no Azure/emulator/deployment check ran. These tests use test-only proof constructors to exercise model contracts; no production gate implementation was changed or opened.

Final delivery verification covers the assigned report's UTF-8 encoding, nonempty content, byte limit, whitespace and exclusive changed-path scope. Task-specific temporary build/cache/fixture output is removed before handoff. Production qualification remains **INCOMPLETE**, regardless of local suite success.
