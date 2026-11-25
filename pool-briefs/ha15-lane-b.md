# Task: manual-failover offline slices 1, 2 and 4 (VLite ITEM-044), test-first

Repository: mahpate/Vlite (Rust). You are implementing ONLY slices 1, 2 and 4 of section 6 of the design
report appended below. Slices 3, 5, 6 and 7 and every owner decision in section 7 are out of scope. The report
was written at an older commit; line numbers are approximate. Read the current code first.

## Exclusive write scope
- `src/server/failover.rs` (slice 1 only)
- new file `src/server/failover_proof.rs` (slice 2)
- new file `src/server/failover_harness.rs` (slice 4)
- `src/server.rs`: ONLY add `mod failover_proof;` and `mod failover_harness;` next to the existing
  `mod failover;` line. No other edit to server.rs.
Any change to any other path fails the task. Do NOT touch `src/server/writer_fence.rs` (an unrelated existing
serving gate; do not reuse its name), `tenant_provisioning/*`, routes, dashboard or docs.

## Slice 1 (failover.rs)
Typed failover intent/status and lossless new wire DTOs: every new u64 wire field is a canonical decimal
string (no leading zeros, no sign) that round-trips above 2^53. Do NOT change the existing CLI/wire shape.
New tests: `failover_api_u64_round_trips_above_js_safe_integer`,
`failover_operation_same_key_same_input_is_same_operation`, `failover_operation_same_key_changed_target_conflicts`.

## Slice 2 (failover_proof.rs)
A read-only `WriterFenceProof` parser and a read port trait, exercised only with recorded JSON fixtures in
tests. There must be NO production constructor and no network code. Contract (report section 2): bound to the
exact app resource ID and the authority binding; reads all list pages plus each revision directly; every
continuation link must stay within that exact app's revisions collection (reject cross-app links and cycles);
every revision must have `active == false` AND an explicit integer `replicas == 0` (missing, null, string or
fractional replicas are refused); `runningState == Stopped` is corroboration only, never sufficient; list/direct
disagreement or inventory drift refuses. A Multiple-mode fixture must not count as Single-mode qualification.
New tests: `writer_fence_requires_exact_app_and_authority_binding`, `writer_fence_reads_all_pages_and_direct_revisions`,
`writer_fence_rejects_cross_app_next_link_and_cycles`, `writer_fence_rejects_missing_null_string_or_fractional_replicas`,
`writer_fence_requires_inactive_and_explicit_zero_on_every_revision`,
`writer_fence_refuses_list_direct_disagreement_and_inventory_drift`,
`writer_fence_multiple_mode_probe_fixture_is_not_single_mode_qualification`.

## Slice 4 (failover_harness.rs)
An in-memory, test-only crash/interleaving model of acknowledgement, final recheck and authority CAS (fake
ports, no I/O, no timers). New tests: `failover_changed_report_clears_acknowledgement`,
`failover_head_advance_requires_new_report_and_ack`, `failover_final_recheck_detects_reactivation_and_sends_no_transfer`,
`failover_crash_after_cas_before_settle_recovers_exact_candidate`, `failover_unreadable_authority_stays_indeterminate`,
`failover_unchanged_readback_does_not_authorize_redispatch`, `failover_competing_successor_is_not_attributed_to_this_operation`,
`failover_cancel_racing_transfer_cannot_undo_epoch`, `failover_foreign_attempt_requires_actuator_stopped_proof`.

New modules start with `#![cfg_attr(not(test), allow(dead_code))]`. A proof is only ever built from
authenticated reads, never deserialized from a browser/request body.

## Must keep passing unchanged
All existing `failover` tests, notably `failover_report_id_is_deterministic`,
`failover_report_loss_known_only_with_observation`, `failover_report_stale_after_head_advance`,
`failover_refuses_a_target_held_by_a_lifecycle_operation`, `failover_unknown_applied_succeeds_by_readback`,
`failover_unknown_unreadable_is_indeterminate`, `failover_unknown_not_applied_is_conflict`, and `tests/server_cli.rs`.

## Forbidden
No routes, OpenAPI, `site/api.html`, CLI behaviour change, dashboard, gate, override or actuation. No Azure or
network access. No new dependency or Cargo.toml/Cargo.lock change. Never weaken an existing test.

## Validation (run before finishing; all must pass)
- `cargo test --locked --features server --lib server::failover`
- `cargo test --locked --features server --test server_cli failover`
- `cargo clippy --locked --features server --lib --tests -- -D warnings`
- `cargo fmt --check`
In your final commit message list the new test names and the validation results.

---

# Appended design report (sections 2-3 and 6 apply; line numbers approximate)

# Dashboard-triggered manual failover: design-only research

**Status: INCOMPLETE for production safety validation; design report delivered.** Inspected checkout `1a9a483a`. No source changes, Azure/network calls, credential operations, deployment, authorization, commits or branch changes were performed. Only this report was created. Passing existing offline checks below does not validate the proposed trigger or an infrastructure fence.

**Recommendation (PROPOSED, not an owner decision):** use a dedicated failover coordinator/state machine on the existing Blob-backed operation journal, independently hosted from the writer. Reuse the authority transfer invariants, not the create-worker execution path. The unresolved blockers are a durable infrastructure fence across the transfer interval, independent host availability, credential boundaries, lifecycle integration and crash attribution. Do not ship an executing dashboard route until those are resolved and demonstrated. No gate, threshold, safety check, SLO or measurement policy should change; no bypass/configuration boolean or new external dependency is proposed.

Scope excludes tenant migration (tenant-provisioning EPIC-006), replacement provisioning, route cutover and automatic failover qualification. The HA PRD also uses the label EPIC-006 for writer fencing/failover; references to that PRD's existing requirements are not a proposal to implement tenant migration.

## 1. CONFIRMED: requirements and current contract

### Requirements and qualification

- ITEM-025 delivers CLI/library failover. ITEM-043 is design-only; a command builder is explicitly insufficient. ITEM-044 is blocked on control-host architecture, production writer-fence proof, permissions and recovery design. The preferred direction is a dedicated journal-backed operation; gateway hosting is rejected. No implementation/cloud action is authorized by the note (`docs\projects\feature-aca-ha\feature-aca-ha.prd.md:945-984`).
- FR-014 requires operator-driven v1, infrastructure fencing and acknowledged loss; automation must not ship before GATE-005 (`docs\projects\feature-aca-ha\feature-aca-ha.prd.md:316`). OD-003 accepts disclosed possible latest-write loss for manual failover, not automation or production rollout. OD-002 records an agent-delegated per-batch epoch/writer check, not a demonstrated zero zombie window or new owner acceptance. GATE-005 still requires the fencing obligations, TEST-007/008/014/015, the OD-002 decision and a manual drill (`docs\projects\feature-aca-ha\feature-aca-ha.prd.md:253-259,527,557-558`).
- Existing requirements distinguish planned quiesce/flush/final observations from unplanned unknown loss. All old revisions must be deactivated, every revision must have zero replicas, and the inventory must be repeated. Scale settings, one replica, Single mode, dead processes and expired leases are not fences (`docs\HA-ACA.md:240-256`).

### `build_report`

`build_report(store, optional ArchiveStatus, optional WriterObservation)` reads and validates authority. Its boundary is `record.head.or(record.base)`, not a timer, latest upload or status checkpoint. It returns scope, lineage, authority epoch/revision/phase, an eight-character writer prefix, archived head/base, diagnostic status and observation, and `Loss::Known { commits }` or `Unknown` (`src\server\failover.rs:182-219,344-401`).

Known loss is observation sequence minus archived sequence only when the observation is at/after the boundary and not behind a supplied status checkpoint. Later loss remains unknown. A newer status checkpoint is disclosed as unpublished/lost; an archive timestamp is supplied only when the status checkpoint exactly matches the authority sequence. Unknown observations must not become zero. The report ID is SHA-256 over deterministic serialized report content with `report_id` cleared; it is a content identifier, not authentication, a freshness timestamp, an ARM proof or an ETag (`src\server\failover.rs:222-270,344-398`).

CLI inputs: required target file, optional shared-key file, optional archive-status JSON capped at 64 KiB, and paired unsigned sequence/time observations. Execute additionally requires report ID, epoch, revision and old-replica count. CLI `run` rebuilds the report for both verbs and prints `report` plus authoritative `text`; execute adds `result.record` and `result.etag`. Identical optional inputs are therefore necessary between CLI report and execute (`src\server\failover.rs:42-179`; `docs\HA-ACA.md:261-290`).

### `execute`

`execute(store, report, ack)` performs these checks in order:

1. `ack.old_replicas == 0`. This is only an operator assertion; the function calls no ARM verifier.
2. Exact acknowledgement of report ID, expected epoch and expected revision, including recomputation of the report digest.
3. Fresh authority read matching report epoch, revision, scope, lineage, phase and effective archived head/base.
4. Target lifecycle-guard check, immediately before authority transfer.
5. One conditional transfer using the observed authority ETag (`src\server\failover.rs:403-456`).

Transfer validates the record, checked-increments epoch, moves head to base, clears head/writer/claimed time, sets `Recovering`, generates a fresh revision and writes with `If-Match`. An unknown store outcome is read back and accepted only if bytes exactly equal the intended record; a different record conflicts, and unreadable readback is unresolved (`src\server\authority.rs:246-255,378-408`). Success prepares recovery only: it does not restore, claim, start, route to or enable the replacement (`docs\HA-ACA.md:298-322`).

| Existing code | Meaning / handling |
|---|---|
| `failover_fence_unconfirmed` | Supplied replica assertion is nonzero; current code has no server proof. |
| `failover_ack_mismatch` | Wrong acknowledgement or tampered report. |
| `failover_report_stale` | Compared authority fields changed since report. |
| `failover_conflict` | Authority CAS/ownership conflict; inspect authority. |
| `failover_indeterminate` | Mutation cannot be resolved; retain stopped old revisions and read authority, never blindly execute again. |
| `failover_authority_unavailable`, `failover_authority_missing`, `failover_authority_invalid` | Authority cannot safely be used; no fallback authority or initialization. |
| `invalid_arguments` | CLI verb/flag/type/pairing/file validation; target connection can also propagate its own existing recovery errors. |
| `lifecycle_reserved` | Held **or write-enabled** target guard refuses transfer. |
| `lifecycle_guard_unavailable`, `lifecycle_guard_invalid` | Guard cannot be verified; refuse closed. |
| `lifecycle_write_disabled` | Held guard refuses a writer startup claim; not directly emitted by the current transfer check. |

Evidence: `src\server\failover.rs:283-330,409-442`; `src\server\tenant_provisioning\guard.rs:49-87,109-138`; `docs\HA-ACA.md:291-296`.

### Lifecycle interlock is two distinct mechanisms

The control-state snapshot has per-tenant, generation-numbered lifecycle slots; reservation binds operation ID, kind, request digest and expected route. Existing kinds are Create/SetReaders/Migrate/Retire, **not Failover** (`src\server\control_state.rs:68-110,864-930`). The separate `{authority-prefix}lifecycle-guard.json` has Held -> WriteEnabled -> Released transitions, CAS writes, and blocks authority transfer unless absent/released. Holding it does not stop an already-serving writer; holders must re-read authority after acquisition (`src\server\tenant_provisioning\guard.rs:1-19,25-45,126-138`).

Consequently, a failover operation cannot hold that target guard and then call current `execute`: it would correctly refuse itself. A future design must preserve this check, not add an owner exception or temporarily release a foreign guard. The two objects are not one atomic transaction.

## 2. PROPOSED: dedicated operation, journal and recovery

This section describes contracts to implement only after separate authorization. It is not existing behavior.

### Persisted records and ownership

Reuse `Operations`' existing Blob store, create-only immutable intent/content records, principal-scoped idempotency, journal generation and ETag CAS. Existing IDs derive from schema/principal/key; changed content with the same key conflicts. Each journal action is persisted `Uncertain` before dispatch, later settled from observation; an unresolved action prevents another action. A foreign attempt cannot take over without `ActuatorStopped`, which currently has only a test constructor (`src\server\tenant_provisioning.rs:266-344,481-585,716-906`).

Add a distinct typed failover intent/status/terminal outcome in that infrastructure, not a create profile or a migration payload. Retain:

- Server-resolved target binding: scope, installation, lineage, authority store/prefix identity (no credential bytes), exact old ACA app/environment/resource identity, expected route/receipt digest and lifecycle generation where managed.
- Immutable reason/mode and optional diagnostic inputs with their provenance and content digests; server-generated report bytes/text and report generation.
- Operation/attempt IDs; journal generation; expected authority record and ETag; complete fence evidence digest and reactivation-exclusion evidence reference; exact acknowledged report ID/epoch/revision and actor.
- Before transfer dispatch, the **exact intended successor bytes/revision**, expected old ETag, action digest and acknowledgement reference. Result/terminal retains exact authority record/ETag observed, not merely `epoch == old + 1`.

Current `prepare_transfer` generates its successor revision inside the write call. A durable wrapper around it alone loses that value on a crash. A surgical future refactor must separate pure candidate construction from guarded single-CAS application, retaining the current public behavior and every validation/guard. Persist the candidate before dispatch so a recovering process can attribute exact readback. A journal generation, lease or epoch increment by itself cannot prove which process performed a transfer.

### Phase machine

| Phase | Durable transition / permitted next action |
|---|---|
| `accepted` | Create/reconcile immutable intent; no ARM mutation, no authority write. |
| `reserved` | Acquire generation/route-bound lifecycle reservation and exclusive operation ownership for the authority target; re-read binding and authority. Foreign lifecycle guard remains a refusal. |
| `awaiting_writer_stop` | Operator stops all old revisions under separately agreed ownership. Coordinator only observes; planned quiesce happens before stop. |
| `verifying_fence` | Server obtains complete fresh ARM evidence and reactivation-exclusion evidence. Failure stays blocked, not successful. |
| `report_ready` | After verified stop, server builds and privately retains exact report. Expose report/text and generation for operator review. No transfer yet. |
| `acknowledged` | CAS records explicit acknowledgement of that report ID, epoch, authority revision, report generation and operation/lifecycle generations. No general approval string. |
| `transfer_pending` | Record exact candidate/expected ETag as an uncertain journal action. Recheck ownership, reservation, binding, authority, every revision and the exclusion interval immediately before the single authority CAS; preserve the existing final lifecycle-guard check. |
| `reconciling` | Any crash/timeout/unknown write permits observation only until transfer outcome and any former actuator are conclusively resolved. No second transfer dispatch. |
| `transfer_prepared` | Exact intended successor confirmed: incremented epoch, Recovering, no writer, expected base. Seal a failover-specific terminal record, then CAS-release reservation. Clearly state “replacement recovery not performed.” |
| `cancelled` / `refused` | Terminal only when no side effect remains uncertain and no old attempt can still dispatch. Cancellation never reactivates old revisions. |

`report_ready` and `acknowledged` can be implemented as typed CAS journal events rather than inventing a second independent mutable state file. Persisted observations are evidence/history, never indefinitely reusable proof. A failed final recheck clears the acknowledgement's eligibility and requires a newly produced report and explicit acknowledgement. No default validity duration is invented here.

### Serialization without weakening the lifecycle guard

For an already managed, exclusively bound dedicated target, add `Failover` to the **control-state lifecycle slot** and reserve against exact route/receipt/generation. Do not acquire or special-case the authority's Held/WriteEnabled guard. Existing provisioning for that tenant is excluded by the reservation; current `execute` still requires target guard absent/released. Extend typed sealing/release rules with a failover result, rather than fabricating a create activation receipt or a retirement. Current terminal types and release predicates cannot represent this result (`src\server\tenant_provisioning.rs:344-366,915-1035`; `src\server\control_state.rs:1243-1285`).

Target uniqueness must be proved, not inferred from a tenant ID. A per-tenant slot alone cannot serialize different tenants sharing one writer/authority. Owner must decide initial supported scope: a bounded first implementation could support only proven one-to-one managed dedicated bindings and refuse everything else, without weakening existing behavior. If shared/unmanaged targets must be supported, design a target-keyed CAS reservation in the same Blob coordination store and make every relevant lifecycle acquisition honor it; that extra cross-object protocol is unresolved. Do not silently use one tenant's slot to protect a shared writer. No tenant migration is needed or proposed.

Legacy/manual/supervisor competitors do not gain permission to ignore guards. An exact old authority ETag ensures only one conflicting candidate lands, but does not stop another ARM principal reactivating the old app. Test the existing guard-acquisition/authority-re-read interleavings. A reservation or successful journal claim is not infrastructure fencing, and does not provide production actuator-takeover proof.

### Idempotency, staleness and uncertain outcomes

- Submission retries with the same principal/key and exact input return the same operation. Action retries have their own principal/key and exact request digest. Lost HTTP responses must lead to status inspection, not a newly keyed failover.
- Acknowledgement binds the **server-retained** report, never a browser-uploaded replacement. Report/binding/input/authority changes require a fresh report and fresh explicit acknowledgement. Compare full authority inputs and ETag, including head advancement; known report loss is not known total loss. Malformed or unknown response schemas fail closed.
- A proven pre-dispatch failure can be recorded as such. Once transfer might have been sent, journal action remains uncertain until readback settles it. Same exact candidate present means applied; unreadable authority means `failover_indeterminate`; another successor or later authority means conflict/needs investigation, not attribution to this operation.
- Reading the old authority unchanged is **not alone** proof a timed-out request or paused actuator cannot still apply its CAS. Keep the action/reservation unresolved until the former sender/request is proven unable to dispatch/apply. Do not generate a new candidate or use the unchanged read as permission for blind retry. CAS state does not kill a process. Current create-worker takeover remains refused in production (`src\server\tenant_provisioning.rs:325-344,750-774`; `src\server\tenant_provisioning\worker.rs:14-25`).
- A crash after exact authority success but before journal settle/seal is recovered by matching the persisted candidate bytes, then settling, sealing and releasing in that order. Never seal with an uncertain step. A concurrent cancellation cannot recall an already dispatched transfer; show the observed transfer outcome instead.
- After possible or confirmed transfer, leave old revisions stopped, never decrement epoch/overwrite authority, and never reactivate an old revision as rollback. Replacement startup is a later separately qualified action. These preserve the current runbook (`docs\HA-ACA.md:298-327`).

## 3. Writer-fence proof: CONFIRMED gaps and PROPOSED evidence

### What exists and what the supplied probe establishes

The existing ARM adapter pins Microsoft.App API `2026-07-01`. It bounds pagination, follows all pages, rejects duplicate revision names, requires explicit active/replica fields, and defines a revision as effectively active when flagged active **or** replicas remain. Missing replicas are malformed, never zero. Stop deactivates flagged-active revisions; stop polling checks the effective-active inventory (`src\server\tenant_provisioning\arm.rs:37-42,290-358,451-464,710-746`).

However `aca::Revision` retains only name, derived active and config digest, not raw active/replica counts, exact revision resource ID or proof observation times. This is not a typed production writer-fence proof. The worker is create-only and production preflight cannot produce the qualification/quota/budget proofs; reader-fleet proofs are also test-only (`src\server\tenant_provisioning\aca.rs:1-40,119-165`; `src\server\tenant_provisioning\worker.rs:1-55`). Do not reuse those test constructors as production authorization.

**Supplied external evidence, not reproduced/independently verified here:** the assignment reports a disposable Multiple-mode probe with API `2026-07-01`, agreeing list/direct reads: running `active=true, replicas=1`; deactivated `active=false, replicas=0, runningState=Stopped`. This supports explicit-field parser fixtures for that case only. No raw artifact, digest, request history, timing/consistency envelope, pagination race or Single-mode result was supplied. It proves neither Single-mode safety nor an interval without reactivation. Repository evidence does not convert it into qualification.

### Proposed server-owned `WriterFenceProof` contract

Construct internally only from authenticated ARM reads plus independently substantiated exclusion evidence; never deserialize a browser claim into a proof.

1. **Exact binding:** resolve from trusted retained deployment material, not URL/name supplied by the browser. Pin subscription/resource group/provider/app resource ID, environment, operation/target identity, authority installation/scope/lineage and expected writer deployment/route binding. Compare returned app and revision IDs to the exact app; tags/config digest are supporting checks, not sole identity. Detect deletion/recreation or unverifiable resource incarnation and refuse. The current strict resource-ID grammar is reusable (`src\server\tenant_provisioning\resource_id.rs:1-58`).
2. **Complete enumeration:** read app metadata, enumerate all active **and inactive** revisions to terminal `nextLink`, retain every page and the complete revision set digest. Enforce existing response/page bounds; reject loops, duplicates, malformed entries, errors, truncation or exceeded bounds. Empty/unexplained/missing app or revision data is not zero-replica proof; require separate proven disposition or refuse.
3. **Constrain continuation requests:** reuse origin/no-credentials/no-redirect restrictions, additionally bind every continuation to the expected app's revisions collection and API contract, not just the broad ARM origin. Existing `under`/`send` restrict to base origin/path and do not retry; base-only checks do not prove app identity (`src\server\tenant_provisioning\arm.rs:124-199,325-358`).
4. **Per revision:** retain exact ID/name, raw `active`, explicit numeric replica count, server read interval and API version. Require `active == false` **and** explicit `replicas == 0` for every revision. Reject omitted/null/string/negative/fractional/ambiguous values; do not infer zero from absent replica instances, scale min/max, lack of traffic, runningState, failed readiness or a 404. Treat `runningState=Stopped` as corroboration, never a substitute. Retain list/direct disagreement as a refusal.
5. **Cross-check and freshness:** direct-read each listed revision, repeat full list enumeration, compare sets and bindings; refuse additions, disappearance or any drift. Record all observations rather than trusting one latest revision. No cross-page snapshot atomicity or ARM consistency bound is established by these repeated reads.
6. **Final verification:** immediately before transfer, repeat exact binding/full enumeration/direct checks and confirm reservation, report and authority. Reuse no cached stopped result across an acknowledgement wait. Persist evidence privately with digests, not tokens or raw ARM request headers. A server-owned proof is necessary but not sufficient without the next interval invariant.

### Reactivation between verification and transfer

| Option | Evaluation / status |
|---|---|
| Exclusive stop ownership plus enforceable exclusion of every other activation/deployment writer throughout proof -> CAS -> immediate post-check | Preferred safety direction to investigate, **unproven**. Operator/controller must account for CI/reconcilers, app updates/new revisions, role holders and in-flight requests. RBAC changes alone have propagation and existing-request questions; a statement of exclusivity is not enforcement evidence. No role change is authorized here. |
| Journal/Blob reservation honored by all VLite controllers | Useful serialization, but **not** a fence against ARM users, external pipelines, stale deployers or ACA behavior. Cannot qualify alone. |
| Repeat list/direct reads immediately before CAS and monitor/re-enumerate after CAS | Detects some drift and should be tested. **Cannot prevent or reliably detect** a transient reactivation in the final read-to-CAS gap; post-transfer detection can be too late to prevent acknowledgements. Not a sufficient production fence. |
| Epoch checks, gateway epoch stamping, failover CAS, Single revision mode, scale settings or deactivation response alone | Existing additional defenses/observations, not substitutes for infrastructure fencing. Per-batch check has an explicitly unresolved check-to-ack window; Single mode was not probed. |
| Stop or delete the entire exact old app | Potential stronger operational exclusion, but destructive availability/configuration consequences and recreation/in-flight action behavior remain **unproven**. Not recommended as an assumed-safe implementation shortcut or authorized action. Would need explicit owner selection and isolated evidence using existing resources. |

ARM read plus Blob CAS spans different services; there is no demonstrated atomic “still stopped, then transfer” primitive here. A post-check failure must keep replacement serving blocked and incident state visible; it must never be treated as a successful safe handoff or trigger old-revision rollback. If no enforceable interval fence can be demonstrated, actual execution remains blocked; additional observations do not lower that requirement.

## 4. PROPOSED control hosts and least-privilege separation

| Host option | Availability and trade-off |
|---|---|
| Independently deployed VLite control app serving the admin dashboard/API and dedicated failover coordinator | Best fit to investigate for a browser trigger and durable status while writer is stopped. Must have its own available compute/ingress, authentication/admin files and health, not the stopped writer's database/admin process. Current `Service`-based admin API is not already a qualified database-free control-host mode. |
| Independent operator/control process on an already available host, serving the same bounded admin surface | Smaller platform change, but host uptime, secure transport, credential custody and restart recovery become operational obligations. It cannot depend on the writer-hosted dashboard loading after stop. |
| Independent ACA Job as executor, with a separate always-available control app/API for submission/report/status | Separates request lifetime from actuation. A Job alone cannot serve the dashboard. Job-start/restart/duplicate-execution and attempt-stop evidence require a separate qualified design; do not assume the create worker already provides it. |
| Writer-hosted admin page/API; HA reader; gateway route | Writer disappears at zero replicas. Reader has read-only serving semantics and no approved authority-write role. Gateway hosting is explicitly rejected; do not grant it admin/authority-write access. |

Confirmed constraints: current tenant deployment runtime holds an OS lock on its own admin directory and needs configured administrator authentication, refusing open-admin use (`src\server\tenant_provisioning\admin.rs:100-130,747-774`). Existing native ACA secrets project only each role's named files; gateway gets no admin token and writer templates have no admin directory (`deploy\azure-container-apps\ha\README.md:125-143`). OD-009 chooses ACA app secrets, not Key Vault (`docs\projects\feature-aca-ha\feature-aca-ha.prd.md:564`).

**Permission boundaries to validate, not grants to create now:**

- **Browser:** authenticated super-admin API access only; no ARM credential, authority SAS/shared key, arbitrary target URL, local secret path or permission to create a proof.
- **Stopper:** initial minimal design leaves stop actions with the operator's separately controlled existing ARM identity/tooling. It needs only the selected app's required deactivation permissions/readback; actual Azure action names and scope effectiveness must be verified later. Do not give the verifier app-update/activation permissions merely to stop revisions. A future control-owned stopper would be a separate journaled actuator and owner decision, not an automatic side effect of report generation.
- **Verifier:** narrowly scoped read of the bound container app, complete revisions and direct revision state; replica-read only if the selected proof additionally needs it. Expected candidate actions are `Microsoft.App/containerApps/read` and `Microsoft.App/containerApps/revisions/read`, with provider behavior/RBAC still unverified here. No activate/deactivate/write/delete, secret-list, subscription-wide discovery or grant administration. Only the server verifier constructs the proof, even when the operator performed stop.
- **Coordinator:** Blob read/conditional-write for its operation journal and exact target authority; read target lifecycle guard; appropriate control-state CAS only for agreed lifecycle reservation/release. No authority reset/delete/geo-fail, no archive-history mutation, no tenant credential minting, and no gateway signing seed requirement. Reporting needs authority/status diagnostics, not archive-write access. Exact object/prefix least privilege must be demonstrated; an account shared key is not a narrow target grant.
- Reuse existing managed identity where supported, ACA app-secret projections for private target/auth material, and role-specific files under the control host's `VLITE_ADMIN_DIR`. Never copy the writer's mutable admin directory into a second concurrently writable process, mount all app secrets or store credentials in Blob journal/report/logs. Per-process file locks do not coordinate separate ACA filesystems; Blob CAS is the cross-process authority.

Combining verifier and authority writer in one control process is simpler but broadens compromise impact; logical typed ports still help testing, not identity isolation. Splitting them reduces privilege but requires trustworthy operation-bound evidence delivery in the existing Blob store. The owner must select the topology and approve exact custody/scopes after evidence; neither topology is selected by this report.

## 5. PROPOSED dashboard and HTTP contract

### User experience

Use the tenant-deployment panel's explicit load/status model, named controls, text-only rendering and operation inspector, but a separate **Manual failover** flow with clear “prepares transfer only” status. Existing deployment UI keeps idempotency keys for exact retries and displays refusal explanations; some existing cancel flows persist request tokens, which must not be copied wholesale for sensitive report/ack material (`src\dashboard\admin-tools.js:736-813,818-860,1935-2075`; `tests\admin_tools.test.cjs:1689-1750`).

Flow: select a trusted bound target -> show planned/unplanned prerequisites and independently available control-host identity -> record operation -> operator stops writer -> server verifies and produces report -> show exact server `text`, scope/lineage/boundary/loss and ID/epoch/revision -> require explicit typed acknowledgement -> submit one execute intent -> show durable phase/result and CLI inspection fallback. “Recorded”, “old writer verified stopped”, “transfer prepared” and “replacement serving” must never be conflated.

Keep report and acknowledgement in page memory only. On target/input changes, navigation, sign-out or new report generation clear them; no raw report, secrets, paths or acknowledgement in URLs, local/session storage, telemetry or request inspector. Redacted operation ID may be retained server-side for rediscovery via authenticated listing. Render arbitrary server strings as text. Unknown code/schema, stale status and disconnected control host disable execution, never offer “force”. On indeterminate outcome explain “leave old revisions stopped; inspect authority”; do not reuse the existing generic “try again shortly” message as a transfer retry instruction. Use canonical decimal strings for u64 epoch/sequence/generation wire fields in the **new** API, parsed losslessly into Rust u64; do not route them through JS `Number` or change the existing CLI wire shape.

### Candidate routes (all new, not currently implemented)

| Route | Request / result |
|---|---|
| `POST /v1/admin/failover-operations` | Idempotency key + server-known target binding ID + reason/mode + optional frozen diagnostics. `202` accepted with operation ID and status; acceptance performs no transfer. No arbitrary ARM/Blob endpoint/path. |
| `GET /v1/admin/failover-operations` | Bounded, deterministic paged authorized operation summaries for restart/reload recovery. |
| `GET /v1/admin/failover-operations/{operation}` | Current durable phase, operation/lifecycle/report generations, binding summary, refusals, uncertainty and transfer result; no implied live fence from historical evidence. |
| `POST /v1/admin/failover-operations/{operation}/report` | Idempotency key + expected generations. Queue fresh server verification and report only after stop. Any new report generation invalidates previous acknowledgement. Return `202` pending or inspectable report-ready status. |
| `POST /v1/admin/failover-operations/{operation}/execute` | Idempotency key, expected generations and exact `ack_report_id`, `expected_epoch`, `expected_revision`. CAS-record acknowledgement/transfer intent. Return `202` pending, not handoff success. Browser supplies neither proof nor `old_replicas`; server internally derives the required zero assertion only from qualified proof. |
| `POST /v1/admin/failover-operations/{operation}/reconcile` | Idempotent request for observation/recovery of this operation, never permission to dispatch a second transfer. |
| `POST /v1/admin/failover-operations/{operation}/cancel` | Generation-bound idempotent cancellation request. No authority undo or old-app restart; cannot terminally release a possibly live/uncertain actuator. |

All routes require explicit configured administrator authentication; reject read-only, scoped/tenanted, tenant editor/viewer and gateway-envelope principals **before** target existence lookup or ARM/Blob side effects. Open-admin must refuse. Reuse standard authentication/authorization/audit machinery; do not put these endpoints on gateway serving mode. The gateway's inventory still needs classifier rows so it recognizes and **denies** these routes, not access to execute them.

HTTP mappings are **proposed**: `401` missing/invalid authentication or `admin_auth_required` open-admin refusal; `403` `read_only`/`forbidden_scope`; `404 operation_not_found` only after authorization; existing malformed-body statuses and `422` strict input validation. Preserve `409` for `failover_fence_unconfirmed`, `failover_ack_mismatch`, `failover_report_stale`, `failover_conflict`, `lifecycle_reserved`, `lifecycle_write_disabled`, generation/request/step conflicts and `actuator_fence_unproven`. Use `503` for unavailable/missing/invalid authority, unreadable/invalid guard, unresolvable operation store and `failover_indeterminate`. Proposed additional fixed codes: `failover_binding_mismatch`/`failover_reactivation_unproven` (`409`), `failover_fence_unavailable`/`failover_fence_invalid` (`503`). These distinguish definite nonzero/active evidence from inability to obtain proof. Final names require API review. An asynchronous operation's GET may return `200` with a blocked/indeterminate phase and code; that is retrieval success, not failover success.

Audit acceptance/acknowledgement/final-check/transfer/refusal/reconciliation with actor, operation, report/evidence digests and old/new epoch/revision. Retain full loss report privately in operation evidence, not unrestricted logs/telemetry. Existing audit event patterns are at `src\server\tenant_provisioning\admin.rs:723-745,776-783`; failover currently logs refusal/prepared metadata but not an authenticated REST actor (`src\server\failover.rs:336-341,444-453`).

### Required registrations and regression coverage

These are implementation obligations, not edits made here:

- Router registrations plus `tests\api_route_inventory.py`, including `test_gateway_classifier_matches_registered_routes` and explicit reserved-resource classification where applicable (`tests\api_route_inventory.py:174-253`). Routes in a new nested module must be visible to existing route discovery; do not assume recursive scanning.
- `tests\admin_route_coverage.py`: each dashboard action needs a named real control; update inventory/discovery deliberately. Its existing deployment-action restriction must remain scoped to deployment actions, not be weakened into arbitrary failover execution (`tests\admin_route_coverage.py:104-123,133-178`).
- `site\api.html` `data-api` rows, `src\openapi.json` paths and strict typed request/result/error schemas, and `docs\API.md` semantics. Existing analogous deployment surfaces: `site\api.html:1607-1613`; `src\openapi.json:38-44,243-247`; `docs\API.md:1673-1706`.
- Gateway `ROUTES`, authorization/reader-admission classification and real-router matching, without adding routes to `PUBLIC` or making the gateway an admin host (`src\server\gateway.rs:97-149,305-336,795-856`). Test trusted-backend and gateway user principals remain unable to administer (`src\server\gateway_trust_tests.rs:182-271,387-438`).
- Isolation: authenticated admin success, unauthenticated/open-admin/read-only/scoped/tenanted denial, cross-target and cross-operation binding, no side effects on denial. Existing route-level tenant guard regression is `a_tenanted_principal_is_refused_on_every_route_that_spans_the_database` (`src\server.rs:4354-4418`). Do not substitute static inventory for requests.
- `tests\server_memory_reserved_as_missing.rs`: new operation-ID/target-ID admin routes do not intrinsically address collection/graph names. Add applicable admin non-resource/isolation tests, not fabricated `CASES` entries. Add a reserved-name `CASES` row only if an actual new route names/exposes such resources; preserve the inventory completeness check (`tests\server_memory_reserved_as_missing.rs:192,353-416,550-606`).
- Extend exact reviewed `APPROVED_OPENAPI_ADDITIONS`/edits in `tests\graph_surface_parity.rs`; do not re-pin baseline to conceal drift. Run `reg008_openapi_allowlist_accepts_baseline_and_approved_additions`, `reg008_openapi_allowlist_rejects_unapproved_drift`, and `reg008_served_openapi_matches_baseline_except_approved_additions` (`tests\graph_surface_parity.rs:150-160,816-871,942-986`).
- Add named handler chains to `tests\dashboard_feature_coverage.py`, unit cases to `tests\admin_tools.test.cjs`, mocked Playwright exercises to `tests\admin_e2e.py`, and recapture tours with `scripts\capture_connected_dashboard.py` after **any** dashboard asset change. Existing tour checks hash source assets (`tests\dashboard_feature_coverage.py:1-9,287-325`; `tests\admin_e2e.py:920-984,1125-1238`; `tests\dashboard_tours.py:23-67`; PRD `:982`).
- Complete the PRD-required three-agent deep review before implementation acceptance. This report is not that review and launches no agents (`docs\projects\feature-aca-ha\feature-aca-ha.prd.md:982`).

## 6. Ordered test-first implementation slices (PROPOSED)

“Offline-implementable now” means technically implementable/testable without Azure after separate implementation authorization; this assignment authorizes **none** of those changes. New exact test names below are proposed, not tests claimed to exist or pass. No production constructor may use fixture proofs.

| Order / status | Bounded implementation and exact test names |
|---|---|
| 1. **Offline-implementable now** | Freeze current library/CLI behavior, then add typed failover intent/status and lossless new wire DTOs. Existing regressions: `failover_report_id_is_deterministic`, `failover_report_loss_known_only_with_observation`, `failover_report_stale_after_head_advance`, `failover_refuses_a_target_held_by_a_lifecycle_operation`. New: `failover_api_u64_round_trips_above_js_safe_integer`, `failover_operation_same_key_same_input_is_same_operation`, `failover_operation_same_key_changed_target_conflicts`. |
| 2. **Offline-implementable now** | Read-only proof parser/port and recorded-response fixtures; no production proof constructor. New: `writer_fence_requires_exact_app_and_authority_binding`, `writer_fence_reads_all_pages_and_direct_revisions`, `writer_fence_rejects_cross_app_next_link_and_cycles`, `writer_fence_rejects_missing_null_string_or_fractional_replicas`, `writer_fence_requires_inactive_and_explicit_zero_on_every_revision`, `writer_fence_refuses_list_direct_disagreement_and_inventory_drift`, `writer_fence_multiple_mode_probe_fixture_is_not_single_mode_qualification`. Preserve existing `a_missing_replica_count_never_proves_a_stop`, `an_unprovable_inventory_is_incomplete` (`src\server\tenant_provisioning\arm.rs:1651,1837`). |
| 3. **Offline-implementable now for state-machine/model tests; production target scope BLOCKED** | Add journal phases, typed terminal result, lifecycle reservation/sealing rules and exact successor persistence. New: `failover_reservation_excludes_provisioning_without_holding_target_guard`, `failover_foreign_held_or_write_enabled_guard_still_refuses`, `failover_same_target_different_tenants_refuses_unproven_serialization`, `failover_terminal_seals_before_reservation_release`, `failover_transfer_candidate_is_persisted_before_dispatch`. Use dedicated one-to-one fixtures only; broader target support awaits owner scope/serialization decision. |
| 4. **Offline-implementable now** | In-memory crash/interleaving harness for acknowledgement, final recheck and authority CAS. New: `failover_changed_report_clears_acknowledgement`, `failover_head_advance_requires_new_report_and_ack`, `failover_final_recheck_detects_reactivation_and_sends_no_transfer`, `failover_crash_after_cas_before_settle_recovers_exact_candidate`, `failover_unreadable_authority_stays_indeterminate`, `failover_unchanged_readback_does_not_authorize_redispatch`, `failover_competing_successor_is_not_attributed_to_this_operation`, `failover_cancel_racing_transfer_cannot_undo_epoch`, `failover_foreign_attempt_requires_actuator_stopped_proof`. Existing readback tests: `failover_unknown_applied_succeeds_by_readback`, `failover_unknown_unreadable_is_indeterminate`, `failover_unknown_not_applied_is_conflict` (`src\server\failover.rs:871-904`). |
| 5. **Offline-implementable now as a non-actuating API/UI prototype; executing runtime BLOCKED** | Router/DTO/auth/audit tests and mocked dashboard flow, all using isolated fake ports and existing refusals for unavailable production capabilities. New Rust names: `failover_routes_reject_open_admin_and_non_admin_before_lookup`, `failover_execute_rejects_browser_proof_and_arbitrary_target`, `failover_action_replay_does_not_enqueue_twice`, `failover_audit_binds_actor_report_and_epoch`, `gateway_failover_routes_remain_admin_only`, `failover_admin_routes_address_no_collection_or_graph`. New JS function `manualFailoverTests` and browser exercise `exercise_manual_failover`: tampering, large integers, unknown loss/code, stale report, sign-out clearing, no storage/telemetry, dropped POST response followed by status/reconcile only, writer stopped but independent control page usable. Update all inventories/docs/parity in section 5 together; recapture remains a later authorized artifact-producing step. |
| 6. **BLOCKED: owner topology/custody and isolated live permission evidence** | Qualify real independent host, exact ACA secret/admin-file projections, least-privilege ARM reads/Blob CAS, complete inventories in intended mode(s), exclusions and stopped-actuator evidence. Proposed live test names: `manual_failover_control_host_survives_writer_zero`, `manual_failover_verifier_cannot_mutate_aca`, `manual_failover_authority_writer_is_target_scoped`, `manual_failover_all_revisions_fence_survives_transfer_interval`, `manual_failover_single_mode_explicit_zero_evidence`, `manual_failover_crashed_actuator_cannot_later_dispatch`. None is run/qualified here. Missing production evidence stays a refusal; do not add an override. |
| 7. **BLOCKED: approved isolated end-to-end drill and review** | Demonstrate stop -> fresh report -> acknowledgement -> final proof -> single transfer -> reconciliation and, separately, qualified replacement restore/claim/readiness checks without old-app reactivation. Proposed drill `manual_failover_dashboard_crash_and_reactivation_drill`; run applicable existing TEST-007/008/014/015 and process suites, independent of automatic-failover enablement. Complete three-agent review, mocked `admin_e2e`, connected dashboard recapture and tour checks. No production rollout or gate approval follows automatically from an isolated pass. |

Future targeted Rust commands should select `failover`, `tenant_provisioning::guard`, relevant journal/ARM tests, router isolation and `graph_surface_parity` first; preserve existing create-worker preflight/takeover tests, especially `every_gate_refuses_in_order_before_the_claim_and_the_adapter` and `a_foreign_claim_is_never_taken_over` (`src\server\tenant_provisioning\worker.rs:604,640`). New dependencies are unnecessary for the offline slices.

## 7. Explicit owner decisions and live evidence still required

No decision in this list is made by this report:

1. Select the independent control host/topology and its required availability/security boundary. Decide how an operator reaches/loads it when the writer is already unavailable. Current writer/gateway deployments do not answer that.
2. Select who stops the old writer and owns the exclusion interval: operator-controlled stop versus a separately designed control actuator. Identify all deployment/reactivation authorities and enforceable prevention/detection obligations. Decide the response to detected reactivation; zero-count polling alone is insufficient.
3. Approve exact initial target scope (one-to-one managed dedicated versus other layouts), trusted binding provenance/incarnation checks, cross-target/tenant serialization, and typed terminal release semantics. Do not equate tenant migration work with failover scope.
4. Approve exact ARM-read and Blob authority/journal/control-state permissions, credential custody/rotation/projection and audited operator access using existing ACA secrets and `VLITE_ADMIN_DIR`. Demonstrate actual platform enforcement, not merely JSON fixture allowlists.
5. Resolve production stopped-actuator/in-flight-request proof and recovery after an unchanged authority read or a later competing successor. Decide escalation/custody for operations that cannot safely terminally release; never use a timeout alone as proof of stoppage.
6. Resolve report/evidence freshness and timing requirements from measured platform behavior, without inventing/defaulting a new safe window here. Keep exact acknowledgement and fresh final verification regardless.
7. Preserve OD-003's existing manual-loss acceptance; obtain any separately required owner acceptance of the residual zombie window under OD-002. Do not infer it from that earlier loss acceptance or the delegation note.
8. Obtain recorded isolated ARM evidence for all intended revision modes, multiple revisions/pages, inactive draining revisions, explicit zero fields, list/direct agreement, drift/reactivation, restart/redeploy, in-flight management requests, RBAC propagation and control-host survival. The supplied Multiple-mode sample is only one observation.
9. Produce applicable paused/double-writer/partial-upload/CAS/clock/process evidence and manual drill records; retain existing GATE-005 and production gates unchanged. Decide replacement recovery/routing as separately scoped work; transfer-prepared is not service recovery.
10. Separately authorize any implementation, isolated cloud exercise and deployment. This research and the earlier dashboard request do not authorize any of them.

## Checks and limitations

### Actually executed in this checkout (offline; existing behavior only)

| Command | Observed outcome / category |
|---|---|
| `python -B tests\api_route_inventory.py` | PASS, 11 tests. Static source/OpenAPI/classifier inventory regression checks. |
| `python -B tests\admin_route_coverage.py` | PASS, 69 method/actions, 58 paths, 68 named controls, 1 backend-only. Static inventory, not authenticated HTTP validation. |
| `python -B tests\api_reference.py` | PASS, 951 public API items, including 188 REST items. Static documentation/source parity. |
| `node tests\admin_tools.test.cjs` | PASS, “admin tools tests passed.” Existing JS VM/mock-DOM fixtures, not a running browser/server/ACA test. |
| `python -B tests\dashboard_feature_coverage.py` | PASS, 188 registered method/routes, 155 named-control links, 33 exclusions, 11 memory-mode refusals. Static/mock contract coverage. |
| `git --no-pager status --short`; `git rev-parse --short HEAD` | Initially clean; inspected revision `1a9a483a`. Final report-only change/UTF-8/size checked after writing. |

Python bytecode generation was disabled. No build/runtime fixture directory, local server or other background process was created by this task.

### Static inspection only / checks not run

All Rust behavior descriptions are source/test inspection, **not executed Rust evidence**. No `cargo test`, build, clippy, Rust process drill or Azurite test was run; this was a report-only assignment, and executing the proposed tests is impossible because they do not exist yet. Existing test names cited above establish available regression contracts, not results from this session.

No authenticated runtime isolation tests, real browser/Playwright tests, connected tour capture or `dashboard_tours.py` were run. Tour recapture would produce out-of-scope artifacts; the report-only task changes no dashboard assets. No dependency installation was attempted.

No live Azure/ARM/Blob/ACA/RBAC/secret/network validation could be run under the explicit no-network/no-authorization assignment. The supplied probe was not repeated. The proposed fence, control-host availability, permission enforcement, actuator takeover safety and end-to-end failover therefore remain **INCOMPLETE / NOT VALIDATED**, with no claim that ITEM-043/044 or any gate passed.
