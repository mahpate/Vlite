# EPIC-007: reversible shared-pool to dedicated tenant migration

**Design investigation complete; implementation/runtime validation INCOMPLETE.** This is an implementation proposal, not a passed migration gate. TEST-013, AC-013 and GATE-006 have **not** been demonstrated. No migration implementation, Azure resources, production workload or credentials were changed.

Inspected checkout: `7f00be4f70054a58432a53f8096ac7029cdbae59`. Scope: `pool-briefs/T3.md:1-68`. All paths and line references below are repository-relative; `PRD` means `docs/projects/feature-aca-ha/feature-aca-ha.prd.md`. Statements headed **Proposed** describe new behavior, not existing capabilities.

## 1. Decisions and acceptance boundary

**Recommended implementation:** an explicitly invoked, offline-first migration command family, a tenant-filtered **valid existing-format log**, sanitized recovery metadata, fresh credential secrets, and one CAS transition of the shared control-state snapshot. Keep the tenant ID and physical namespace on the dedicated deployment. Support an authority-less dedicated writer first, without pretending that it has HA; integrate the EPIC-006 authority contract for later HA targets.

Controlling requirements:

| Requirement | Design response |
|---|---|
| G-5, FR-015 | Explicit tenant selection; directory supports shared-pool and dedicated; new secrets; retained source and reversible validation phase. PRD `:32,265-274,314`. |
| TEST-013, AC-013 | Two-tenant fixture, full content comparison plus public read probes, rekey checks, users/bindings/orphans, source invariants and reverse route transition. PRD `:396,429`. |
| GATE-006 | Run on a copy, prove rollback, obtain separate approval for an actual tenant. This report does none of those runtime operations. PRD `:467`. |
| OD-011, HA-off | Both deployment kinds; shared-pool remains default. No automatic migration or new standalone requirement. PRD `:300,505,513-519`. |
| ITEM-027/028/029 | Projection and package; access/configuration carry-over; verified transition and rollback. PRD `:858-865`. |

### Two constraints that must not be hidden

1. **A valid log segment is not a tenant dump.** `restore_to` copies whole frames through a database-wide sequence and assigns a new lineage; segment export carries whole commits, the original header and sequence continuity (`src/logops.rs:87-180,205-223,327-438`). The named public functions in that file implement reencrypt, restore and segment export, not namespace projection (`:24-30,99-106,430-438`). A new narrow projector is required; passing a shared backup directly to ordinary restore would move other tenants.
2. **A literal current `--directory-file` flip is insufficient.** Current directory-only updates leave credentials unchanged, whereas gateway authorization compares the credential's lineage with the route's lineage (`src/server/control_state.rs:413-429`; `src/server/gateway_mode.rs:392-411`; `src/server.rs:1434-1445`). Both cutover and rollback need a single atomic route-plus-credential-view edit. Copying an old complete snapshot back would also undo unrelated changes; existing CAS edits deliberately preserve restrictions (`src/server/control_state.rs:200-216,389-429`).

### Reversibility decision: no acknowledged target writes before release

**Proposed default:** the target serves validation reads but rejects client mutations until source release. The source tenant is write-frozen throughout. Rollback then preserves all acknowledged application writes without changing the source copy.

An untouched source plus unrestricted target writes plus lossless rollback by a directory flip is impossible: post-cutover writes do not exist on the source. Do not quietly implement data-losing rollback. Writable probation would require a separately designed, verified reverse-copy/reconciliation protocol, not dual writes or a best-effort log splice. The price of this conservative default is an explicit tenant write-outage from freeze through release. Read availability is restored during validation; normal target writing begins at release.

## 2. State inventory and exact ownership rules

**Proposed selection rule:** tenant `T` is the existing immutable 32-hex ID; select exact prefix `P = "t" + T + "__"`, never a display name, substring, wildcard expansion or a caller-supplied arbitrary prefix. Keep one selected tenant metadata record. Deleted tenants are refused; retain an original suspension and never use migration to resume it. Current admission and route eligibility differ: suspension blocks admission, but a suspended nondeleted tenant may still hold a route (`src/server/tenants.rs:9-20,79-120`). Tenant IDs/names and aggregate limits are persisted, with validation and bounded atomic loading (`:73-87,165-205,240-258`).

| State | Current storage/evidence | Proposed tenant handling |
|---|---|---|
| Collections, vector records, settings | Tenant usage selects `t<id>__*`, including reserved collections (`src/server/tenants.rs:50-70`). Loaded collections include vectors, settings, precision, payload-bearing records, expiries and exact record revisions (`src/format.rs:342-371`). Settings include HNSW, metric, quantization, planner, metadata indexes and text fields (`src/server/collections.rs:70-91`). | Include every selected collection, including empty/evidence collections; all retained vector extensions, metadata, payload bytes, expiry deadlines, revisions and settings. Preserve collection IDs rather than silently rebinding grants. |
| Collection KV | Ordinary one-dimensional vector records carry KV namespace/key/value; versions are record revisions; expiry is part of the existing storage model (`src/kv.rs:1-25,67-80`). | Included automatically as records in selected collections. KV's application namespace is not a tenant ownership signal; do not extract KV from unselected/global collections. |
| Graphs and graph attachments | Tenant deletion checks `t<id>__*` graphs (`src/server/tenants.rs:383-399`); graph grants qualify tenant-local names (`src/server/credentials.rs:101-105,172-185`). Snapshot encoding includes graph IDs, schema, nodes/edges, attachments and ID high-waters, deadlines and temporal edge state (`src/property_graph/frame.rs:331-401`). Graph IDs can be reused after a graph disappears: creation checks the current live ID/name maps (`src/property_graph/engine/state.rs:986-996`). | Include selected graph lifetimes and their complete retained history. Preserve graph/node/edge IDs and revisions. Resolve historical ownership at each graph create/drop, not once per graph ID. Refuse cross-tenant/global attachment or vector-mutation dependencies instead of importing referenced foreign data or silently dropping edges. |
| Memory nodes, evidence, receipts, erasure jobs | Memory and receipts are nodes committed in one graph batch (`src/memory.rs:1-4`). Labels/key families are defined at `:59-75`; binding record is ordinary graph node `binding:v1` / `VliteMemoryBinding` (`src/memory/binding/record.rs:1-5,16-25`). | Include entire selected graphs and evidence collections, not a list of visible memories. Carry retained content, provenance, scope nodes, receipts/outcomes and erasure barriers/jobs without regenerating keys or receipt digests. |
| Binding records | A record stores tenant/binding, generation/state, local graph/evidence names, receipt window and grants digest (`src/memory/binding/record.rs:49-63,143-180`). Decoder validates the holder graph (`:210-240`). | Carry recorded identities unchanged. A record is not a substitute for the grants file; missing grants never become an inferred authorization. |
| Configured bindings, including retired ones | File binding includes tenant, generation, retirement, local graph/evidence names, receipt window and grants; grants carry principal, read/write scopes and raw access (`src/memory/binding.rs:33-66`). Stored names are derived using tenant ID (`:184-202`). | Filter by exact `binding.tenant == T`, preserving order, identity, scope bytes, read-only scopes and retirement timestamp. Validate with the existing parser and rotation rules, never change tenant to `@root`. |
| Orphan reservations | Startup scans records of every live graph, validates them, and returns records absent from the file (`src/memory/startup.rs:50-115`). Orphans reserve graph/evidence with no owner or raw permission (`src/server/reserved.rs:34-37,213-232`). | Include selected orphan graphs, records and evidence even if there is no configured binding. Inventory them separately. Run startup planning with an empty set too; absence of a bindings section does not imply absence of memory. Never adopt an orphan or invent a grant. |
| Reserved prefixes | All `VliteMemory*` labels and reserved node key families are guarded (`src/memory/names.rs:12-25`); actual families include `mem:v1:`, `scope:v1:`, `receipt:v1:`, `receipt-outcome:v1:`, `content:v1:` (`src/memory.rs:59-75`). Reserved resources are keyed by stored tenant-qualified names (`src/server/reserved.rs:4-14,48-64`). | Preserve internal prefixes and `binding:v1` exactly; do not confuse them with the outer `tT__` resource prefix. Do not tunnel export through a newly exposed raw REST route. |
| Dashboard users | `users.json`; recovery identities include normalized username, role, tenant, disabled state, graph bindings/access, but no account ID, version, password hash or session (`src/server/users.rs:1013-1028,1047-1092,1110-1135`). | Select exact tenant-bound users only; preserve roles and graph scopes, import disabled with fresh unusable passwords and fresh account IDs. Require password reset before enablement. No shared super admin travels with the tenant. |
| Credential inventory and grants | `credentials.json` stores ID, exact collection scopes/IDs, operations, lineage, secret hash, times, tenant, limits and graph scopes (`src/server/credentials.rs:214-240`). Recovery carries ID, names/scopes/state/limits/expiry but no hash, lineage or collection IDs (`:253-272,471-493`). | Carry sanitized tenant-bound inventory, plus a migration-specific sanitized **source collection-ID binding table**, so stale explicit grants cannot bind a recreated name. Re-issue secrets; never copy source hashes. See section 5 for stable principal IDs and rollback. |
| Quotas/rates | Tenant aggregate limit in tenant record and usage includes reservations (`src/server/tenants.rs:9-20,50-70`). Credential request rate, total-vector and per-collection limits are recovered (`src/server/credentials.rs:476-493`). | Preserve exact limits, expiry and operations. Recompute target usage, including evidence/KV records; a source already over quota remains over quota, never gets automatic extra capacity. Rate-bucket balances are runtime state, not portable credit. |
| Import jobs | Durable registry stores collection ID/name, state, last chunk/digest, counts, dropped-collection history and maximum job ID (`src/import.rs:49-115,128-138`). REST reservations are separately held in service memory (`src/server.rs:205-207,332-333`; `src/server/collections.rs:7-8`). | Freeze/drain reservations and refuse any selected open job until explicitly finished/aborted on source **before** the export boundary. Carry retained selected closed-job markers/history and selected job high-water; no unrelated job IDs/details or transient upload/reservation handles. Preserve imported records regardless of job-history retention. |
| Webhooks | `webhooks.json` and `webhooks.cursor` are dispatcher-wide; subscriptions have URL, collection/graph/event filters and secret but **no tenant ID** (`src/server/webhooks.rs:13-37,70-88`). Recovery captures only subscription count, explicitly excluding URL/signing secrets (`:119-121`; `src/server/recovery_bundle.rs:31-44,258-284`). | No automatic tenant ownership inference. Report tenant-overlapping subscriptions by operator-private count/reference only; recreate approved target-only subscriptions with new secrets after release. Broad/mixed subscriptions stay global on source. Never copy cursor/delivery status or auto-send historical events. |

### Global state that must NOT move

* Other tenants, unbound/root users and credentials, root/default collection contents, other graphs, shared admin/read-only tokens, sessions/password/credential hashes, TLS/signing keys, audit history, webhook URLs/secrets, provider configuration, filesystem paths and host identity. The recovery exclusion list explicitly excludes these secret/host categories; capture is a cross-store configuration, not a tenant filter (`src/server/recovery_bundle.rs:31-44,65-100,235-296`). Filter before serializing; do not create a full bundle and hope a consumer filters it.
* Shared control-state authority and unrelated directory records. A snapshot holds tenants, sensitive credential state, directory and monotonically incremented generation `A`; updates CAS the single object (`src/server/control_state.rs:40-53,343-386`). Import is not permission to clone its hashes or reset `A`.
* Source archive owner/catalog/head, backup policy/retention schedules, reader pins, deployment/installation identity, compaction state, `next-startup`, disk ANN cache files, request logs, metrics and rate-bucket contents. Recovery's recognized-admin inventory lists these operational file families but does not restore them as tenant configuration (`src/server/recovery_bundle.rs:111-148,428-434`); service setup attaches cloud/ops/webhook runtimes to the admin directory (`src/server/admin.rs:409-452`).
* The original physical log lineage is provenance, **not** target identity. Shared global sequence `S` is retained only as a necessary ordering/revision boundary; source root data/settings and unrelated commit bodies are absent. Target collection allocation derives from selected creates in retained history, not the source's global allocation floor. The file always structurally has default collection ID 0 (`src/format.rs:428-447`); the projected default is empty with neutral settings.

**Unknown-state policy:** inspect source admin state against the existing recognized-entry checks; unknown entries/symlinks require an explicit resolution, not silent omission (`src/server/recovery_bundle.rs:146-173`). Paths are operator inputs only, not package authority.

## 3. Export format and projection algorithm

### Proposed package `vlite.tenant-migration/1`

An owner-only directory, not a new archive codec or crate:

```text
manifest.json             strict, bounded typed metadata and hashes
tenant.vlite              verified tenant-only existing-format log
recovery.json             selected sanitized recovery config and bindings
inventory.json            logical counts/hashes, identities, orphans, probes
```

Use the existing `serde`, `serde_json`, `sha2`, `getrandom` and `tempfile` dependencies (`Cargo.toml:63-77`); no tar/zip/UUID/Azure SDK/Key Vault dependency. Use fixed member names, no archive paths supplied by the manifest, symlinks, hard-link aliases, path traversal or extra files. Bound metadata with existing config/bundle limits; require an explicit operator size/disk budget for the database and total log scan. Stream file hashing.

Manifest fields: schema, migration ID, tenant ID, creation time, source lineage/sequence/history floor/format flags, source freeze generation, selected historic resource ID/name map, package member lengths/SHA-256, canonical inventory digest, `rekey_only:true`, supported projection profile and `namespace_mode:"preserve_tenant"`. Times/IDs are diagnostic, not leases. Do not include source filesystem paths, secrets, hashes of secrets, endpoint signing material, unrelated names or full source credentials.

The package is confidential tenant data. SHA-256 binds members to a trusted manifest but does not authenticate a malicious self-consistent replacement; this is also the stated recovery-bundle trust boundary (`src/server/recovery_bundle.rs:12-18`). Use trusted operator transfer/storage and existing access controls, not a claim that hashes make untrusted packages safe.

### Namespace handling: strip at the logical boundary, reapply on disk

**Proposed:** `local = stored.strip_prefix(P)` exactly once for the manifest's resource name; `target_stored = P + local` on import. Because `T` is unchanged, source and target stored names are identical. Grant collection API names and binding graph/evidence names remain local. This is a deliberate identity-preserving rewrite, not a missed prefix-removal step.

Do **not** physically strip `P`, replace tenant with root or move memory scopes to `@root`. Existing grants qualify names and ownership by tenant, and explicit coverage checks lineage plus collection ID/name (`src/server/credentials.rs:143-168`). Bindings derive stored names the same way, and memory scopes encode tenant into their key input (`src/memory/binding.rs:184-202`; `src/memory.rs:138-170`). Root conversion would require rewriting hashed scope/receipt identities and would expand wildcard semantics. Dedicated is a deployment placement, not a different tenant identity.

### Proposed projector: exact retained-history filtering, not compact-and-recreate

Add a crate-internal maintenance projector adjacent to `logops`; reuse its checked frame traversal and staged `publish_with` pattern. Scope first release to complete, unencrypted, non-timestamped ordinary V5 logs, including existing noncompact history/checkpoint images and temporal capability where raw checked frames are supported. Refuse encryption, timestamp anchors, compact/precision images, unknown flags/operations and incomplete tails with explicit profile errors. Supporting those profiles later requires their own proof, not disabling checks.

Why this strategy: ordinary checkpoint recreation can raise graph revisions, and the existing compaction preservation contract allows that (`src/maintenance/compaction.rs:11-15`). Exact memory receipt/revision preservation must not depend on that weaker contract. Precision revision blocks/P4 codecs are **not a production escape hatch**: precision support returns false outside tests and P4 is test-only (`src/compact/mod.rs:31-63`). Retain the source's supported checked frames instead.

Algorithm:

1. Open a closed source or verified frozen backup through the existing exclusive `maintenance::source`; fully load and require a complete log before any publication (`src/maintenance.rs:75-103`; `src/logops.rs:107-110`). Build the temporal ownership map by decoding collection create/drop and graph create/drop frames, including a leading checkpoint image. Selected dropped historical resources remain selected for their lifetime.
2. Select tenant collection frames and tenant graph frames, including embedded vector changes, imports, settings, vector extensions, deadlines, temporal entries and graph binding/receipt nodes. Validate graph attachment/vector references against the ownership map **at that sequence**. Refuse any frame crossing the tenant boundary in either direction, including an unselected graph writing/attaching selected vectors. Inspect incoming references too; current graph `VectorMutation` still addresses an existing graph (`src/property_graph/engine/state.rs:998-1005`). Do not split an atomic graph batch across ownership domains.
3. Keep the source's existing leading `OP_ADVANCE` and selected restated image frames in legal image order. Omit unselected restated frames; they do not represent separate suffix commits. Copy supported selected graph snapshot bodies unchanged, preserving whatever revisions the source image already exposes. Source replay requires record blocks before graph frames and a snapshot before ordinary graph history (`src/format.rs:2021-2054,2070-2084`).
4. For each **ordinary committed suffix sequence** not otherwise retained, emit an existing-format delete of nonexistent record ID 0 in the empty default collection at that exact sequence: `format::record_frame(sequence, 0, 0, None)`. This is a semantic no-op, not a new opcode. Existing encoding supplies a collection-tagged delete, and loaded delete tolerates absence (`src/format.rs:400-407,697-717`). This preserves sequence continuity, which the loader requires (`:2045-2055`), without carrying any unrelated body, name, job, dimensions or hash. Selected commits remain whole and keep their original sequence/revisions.
5. Write a V5 header with a fresh projection lineage, neutral default-collection configuration, **collection floor zero**, and only supported necessary feature flags. Preserve selected collection/record and graph IDs. The replayed selected creates derive the target collection high-water. Do not copy the source's global allocation floor: current replay rejects a nonleading create at/below that floor, and replacing omitted creates with no-ops can end the leading-create run (`src/format.rs:2247-2270`). IDs of dropped collections no longer represented before a source checkpoint are not recoverable as tenant history; the new lineage and stale-scope refusal prevent reviving an old grant if a future allocation reuses such an ID. Default stays empty; selected collection creation/settings declare their own dimensions. Never initialize from the source default collection body/settings.
6. Publish through full reload, completeness check, fsync and no-clobber publication (`src/maintenance.rs:718-779`). Reopen read-only, run the logical comparison below, and reject any mismatch. The source is never edited. Publish the entire package directory only after every member is complete; reuse the cloud restore's private staging/no-replace directory publication pattern (`src/server/cloud_recovery.rs:628-640,697-720`), extracting a private helper rather than duplicating platform code.

**Important disclosure:** placeholder commits preserve the shared source's ordering density and `S`; this does not hide historical aggregate write volume. Export size/time scales with the retained shared log, not just live tenant size. This is preferable to a lossy shortcut or new storage format. It is not a new replication stream: source segments cannot be appended to the projected lineage. No online catch-up in v1.

### Integrity and semantic verification

**Proposed two independent layers:**

* Member byte lengths and SHA-256; strict manifest/recovery parse and cross-reference certification. Hash canonical length-prefixed fields using explicitly specified ordering and numeric encodings, not `Debug` output, unordered JSON maps or f32 decimal formatting.
* Full source-selected versus target logical inventory: selected resource names/IDs; settings; all record IDs, f32 bits, payload/metadata, vector extensions, revisions and absolute expiries; graph schema, temporal state, node/edge IDs/keys/labels/properties, deadlines/revisions/attachments/high-waters; retained import markers; binding records; configured bindings and orphan reservation set. Compare live and physically retained counts separately at one injected probe clock. Preserve expiry timestamps, not remaining durations.

Per-resource sorted hashes and a root inventory digest make disagreements localizable without printing contents. Also run public read probes on deterministic smallest/middle/largest record IDs per nonempty selected collection, an exact self-search (allow tied identical vectors), representative graph traversal/attachment reads, KV reads and scoped memory recall/receipt/erasure-status reads. Include negative cross-tenant/reserved-resource probes. Counts plus a few samples alone cannot prove content preservation.

Reuse the `maintenance::drill` reopen/count/graph-check pattern, but do not overstate it: the existing drill checks exact self-search of the smallest ID and full replayed graph equality, not migration isolation or cross-store policy (`src/maintenance.rs:626-710`). Its strict smallest-ID search assertion may reject a legitimate tied vector; the migration-specific probe accepts any exact-distance-zero tie while separately fetching/comparing the chosen ID's bytes. The migration verifier needs its own tenant and access-policy assertions, not a claim that the unmodified drill proves migration.

## 4. Proposed command surface and publication

Use `vlite-server tenant-migration <subcommand>`; commands below are **new**, not commands that run at this revision. Existing offline recovery is exposed as `cloud_restore`, and currently supports rekey rather than identity-preserving seed mode (`src/server/cloud_recovery.rs:120-134,264-289`). Do not overload cloud seed mode or change legacy restore formats.

```text
tenant-migration prepare --tenant T --control-state-file ctl.json
    --if-generation A --source-route-digest H --output plan-dir
tenant-migration export --plan plan-dir --source-db frozen.vlite
    --source-admin-dir admin --memory-bindings-file bindings.json
    --output package-dir --max-bytes N
tenant-migration import --package package-dir --output dedicated-dir
    --fresh-admin-username NAME --fresh-admin-password-file FILE
    --credentials-output-file FILE --max-bytes N
tenant-migration verify --plan plan-dir --package package-dir
    --target-dir dedicated-dir --output evidence.json
tenant-migration cutover --plan plan-dir --target-file target-route.json
    --verification-file evidence.json --if-generation A
tenant-migration rollback --migration ID --if-generation A
tenant-migration release-source --migration ID --if-generation A
    --ack-irreversible --verification-file evidence.json
tenant-migration status --migration ID
```

All output paths must be absent; all private inputs must be bounded regular files. Omitted memory file is valid only when inventory proves no configured source bindings; orphan inspection is always mandatory. Secrets go to a new owner-only file, never argv, stdout evidence, normal JSON logs or this report. Use Windows ACL protections equivalent to existing private staging, not a Unix-mode-only security claim.

Commands stop on uncertain outcomes and return structured fixed-text errors such as `tenant_migration_profile_unsupported`, `tenant_migration_source_not_frozen`, `tenant_migration_verification_failed`, `tenant_migration_conflict` and `tenant_migration_indeterminate`. Reconcile unknown CAS writes by reading exact migration ID/phase/member digests; never retry as a fresh migration or roll back unrelated control-state updates. Existing control writes already distinguish conflicts from unknown outcomes (`src/server/control_state.rs:332-386`).

**Proposed journaling:** one bounded migration record in the same control-state snapshot as the route: ID, tenant, original route digest, source/target identities, package/evidence digests, phase, freeze boundary, original tenant suspension, protected credential ID set, current generation and source-retention reference. No secret values. Offline package/import filesystem journals are not routing authority. Phase and route/credential changes become visible in one CAS, not two Blob objects.

Add a targeted `Edit::Migration` which compares expected tenant/route/phase/evidence and edits only that tenant's route, admission phase and credential views. Retry CAS only against the newly read snapshot while rechecking all preconditions. Never replace the full directory array from a stale plan, republish an entire admin directory, or decrement generation `A`.

## 5. Import, credential rekey, users and memory

### Fresh dedicated database and sidecars

**Proposed:** import into a brand-new private staging tree, verify package hashes before parsing/replay, and invoke existing `maintenance::restore` on `tenant.vlite` to assign final target lineage `L_d`. Inspect `L_d` from the published database, do not predict it. Restore's existing contract uses a fresh lineage and never replaces an existing path (`src/maintenance.rs:390-425`). Store source lineage `L_s` and projection lineage only as provenance.

Filter/certify sanitized recovery config before import; reuse `users::rekey_import`, `tenants::recovery_import`, recovery certification and readback. The existing bundle importer requires an empty directory, creates fresh admin plus disabled imported users, and does not write credentials/webhooks (`src/server/recovery_bundle.rs:428-488`). Keep that contract unchanged; migration orchestrates its extra issuance step afterward. Fresh admin name must not collide with an imported username and imported accounts receive fresh IDs/unusable hashes (`src/server/users.rs:1191-1255`).

The target sidecar contains only the selected tenant, its disabled accounts, newly issued credential state, bindings and migration evidence. The gateway still does not serve interactive login/dashboard (`src/server/gateway_mode.rs:387-390`). Carried accounts are for controlled writer-side maintenance access after a deliberate mode switch/reset; do not promise dashboard migration through the gateway.

### Rekey-only with stable principal IDs: explicit migration-specific decision

**Proposed default:** keep each selected active API credential's **nonsecret principal ID**, but generate a completely fresh random secret/hash, bind it to `L_d`, and preserve exact operations, graph/local collection scopes, limits and the original absolute expiry. This is a new narrowly scoped **offline migration rekey helper**, not existing issuance behavior: current issue and rotation generate new IDs as well as secrets (`src/server/credentials.rs:986-1007,1077-1095`). Ordinary credential issue/rotate and general recovery import remain unchanged.

Rationale: credential IDs are already sanitized recovery inventory (`src/server/credentials.rs:476-493`); they are the binding-file principals (`src/memory/binding.rs:38-49`). Keeping an ID does not keep a secret. Stable IDs let the unchanged source binding file authorize the re-issued principal after rollback, preserve urgent ID-based revocations, and avoid aliasing two principals or rewriting source memory records. Retire the **old secret**, not the logical principal identity. No grace period for old secrets at cutover.

Implementation conditions, all mandatory:

1. Mint only for source credentials still active at the final issuance/cutover check. Expired/revoked entries remain sanitized inventory/denials, never become active. Require tenant `T`, source binding identity and explicit operator migration authorization. No caller-supplied arbitrary principal ID issuance API.
2. Capture exact source scope `(stored name, collection ID)` through a sanitized typed accessor; do not rely on `RecoveryCredential.collections`, which omits IDs (`src/server/credentials.rs:253-272,476-493`). Compare with target same-ID resources. An active explicit grant to a dropped/recreated collection is refused for operator revocation/resolution; never bind its old name to a new ID. Wildcard remains wildcard **within T**.
3. Create two candidate **views** of each fresh credential: same new secret hash/ID/limits/expiry, but source lineage `L_s` or target `L_d` and exact corresponding scope bindings. Only one view is active in the authoritative snapshot at once. Do not export old hashes. Deliver the new token once through the private credential output file before cutover.
4. Cutover atomically replaces selected source credential views with target views while flipping the route. Rollback atomically changes those same fresh views back to `L_s`. Preserve any intervening revoke, earlier expiry, tenant suspension or reduced grants/limits; never reactivate a currently absent/revoked principal from the package. Changes that cannot be mapped safely block transition, not silently widen access.
5. Both source and target data endpoints must be **gateway-trusted only** during the window. Their old/source `credentials.json` is not accepted for data authentication. That is consistent with the existing trust mode's envelope-only authentication and route/audience/lineage checks (`src/server/gateway_trust.rs:1-19,217-253`), but provisioning must enforce it. A bearer-token-accessible source makes old-token retirement false and must block cutover.
6. Snapshot changes are the durable authority. Any staging/admin credential projection used for a later maintenance mode is reconciled to that authority first, including fresh hashes and current denials; stale admin-dir reimport must not undo the transition. Add explicit migration protection to `reimport`, not just operator documentation: current reimport retains revocation/expiry restrictions but rebuilds credential entries from the supplied directory (`src/server/control_state.rs:200-216`; `src/server/credentials.rs:459-469`).

This design preserves grants/scopes while reissuing secrets and permits rollback without altering source tenant data, binding records or source credential files. It intentionally differs from general recovery's normal new-principal workflow. If stable principal IDs are rejected as policy, **do not claim a pure flip rollback**: new IDs require source binding changes or a separately specified signed-principal translation layer. Recommended default is stable IDs for this one bounded migration operation; no translation layer.

### Bindings and orphan handling

**Proposed:** validate the selected original set against projected recorded bindings before any user/password operations. Keep tenant, graph/evidence local names, receipt window, active/retired state, scopes/read scopes/raw access and per-binding generation unchanged under the stable-ID rekey. There is no grants change merely because secret bytes changed: grants digests include principal IDs, scopes/order/raw access, not credential secrets (`src/memory/binding/record.rs:65-97`).

Do not increase per-binding generation gratuitously. If the operator separately changes a grant, increment that binding's generation with checked overflow and run existing rotation rules; do not conflate this with directory `binding_generation`. Binding identity and grants-rotation checks are enforced at `src/memory/binding.rs:170-181,242-258`.

Run `memory::verify_restored` read-only for configured sets **and an empty set for orphan-only databases**. Current helper opens read-only and startup planning returns orphan counts (`src/server/memory.rs:181-193`). Build target `Bindings::new(selected_set).with_orphans(selected_records_not_in_set)` and verify its reservation-name set against the export inventory. Orphans remain unowned; never copy the source's in-memory registry blindly or remove a record to make startup pass.

Per-route `binding_generation` must be computed from the actual full set served by that backend. Current value is a 53-bit nonzero SHA-derived fingerprint, or zero for an empty configured set; adding orphans does not change it (`src/server/reserved.rs:175-190,213-231`). Thus source full-set fingerprint `B_s` can differ from target subset fingerprint `B_d` even when every selected per-binding generation is unchanged. For orphan-only sets, `B_d == 0` does **not** mean unreserved; keep a full SHA-256 reservation digest in verification evidence.

### Runtime administration limitation

Currently trusted backends refuse `VLITE_ADMIN_DIR`, while the configured admin runtime owns users/credentials, backup policy, ops and webhooks (`src/bin/vlite-server.rs:532-550`; `src/server/admin.rs:409-452`). Do not bypass that restriction by enabling ordinary admin/session authentication in trusted mode.

**Proposed v1:** create/import/rekey sidecars offline in isolated maintenance mode, close that process, then start the target trust-only with the selected bindings file and explicit writer serve mode. Account management remains an offline stop/switch/reset procedure; source users remain untouched. Later HA archival must use the EPIC-006 restricted writer-admin/runtime separation rather than casually removing the startup refusal. This is an explicit integration dependency for HA, not a capability delivered by the report.

## 6. Freeze, verification and cutover state machine

**Proposed authoritative phases:** `prepared -> frozen -> target_verified -> validating -> released`; `frozen|target_verified|validating -> rolled_back`. Durable release is terminal with respect to rollback. Every phase mutation uses expected `A`, source/target route identity and migration ID. An operator can inspect/reconcile a crash; no automatic resume or automatic source release.

### Freeze and drain

1. Preflight on a copy: tenant exists, selected imports closed, package profile supported, references contained, source/target disk budgets sufficient, approved target endpoint/audience distinct, source retention available, original suspension recorded. Reject overlapping migrations of the same tenant.
2. Publish migration freeze admission in shared control state. This is a **new mutation guard**, after ordinary authorization, at gateway and backend; recheck it at commit admission for queued/group-commit work. It covers POST reads by operation classification, imports, collection settings/lifecycle, graph/memory/KV/vector writes and any global admin operation that can touch selected resources. A normal suspension is not enough: it also blocks reads and bootstrap admin can still mutate source data (`docs/SECURITY.md:575-583`).
3. For the first release, require an operator-controlled **source process stop/drain** as the authoritative capture barrier. Stop accepting requests, finish accepted writes, drain disconnected/queued/group work, flush and close. Existing service retirement demonstrates why a disconnected client is not proof of no work: worker permits persist until completion and group commit/schedulers/webhooks are stopped before handing out the database (`src/server.rs:599-641,659-698`). Reuse that lifecycle, not its process-global retirement as a claimed per-tenant fence.
4. Capture/verify source `S`, source tenant/config hashes and the frozen image only after clean close. Freeze selected admin configuration too: credentials, tenant settings and bindings cannot change between data/config captures. Existing recovery stability comparison brackets capture but admits an ABA gap (`src/server/recovery_bundle.rs:299-318`); use actual operator exclusivity/drain here, not that comparison alone.
5. Keep the image and package pinned until explicit release. The live shared source may restart to serve other tenants, but only in trust-only mode with this tenant's mutation fence installed and fresh freeze generation loaded before readiness. Disable source compaction/checkpoint/retention operations that could invalidate the retained rollback image; ordinary unrelated tenant writes need not stop after capture. If a safe per-tenant/global-admin guard is not implemented, keep the whole source stopped. Do not claim uninterrupted other-tenant service for that fallback.

**Source untouched means:** no selected tenant data/config deletion, rewrite, user reset or secret-file replacement; the frozen image is byte-identical throughout. A live shared database may append other tenants' commits, so its whole-file hash is not an appropriate invariant. Compare the selected tenant's complete logical projection instead; TEST-013 should test both the fully stopped byte-invariant case and unrelated live writes.

### Target preparation and directory values

**Proposed:** import, rekey, verify full logical state and negative access cases, then start a private writer with a validation-only mutation guard. Any idempotent memory startup preparation must complete before recording the target validation boundary; unexpected record/scope additions or business-data changes fail verification. Do not call it `reader` mode solely to get write denial: mode is fixed at startup and writer mode requires a writable DB (`src/server/serve_mode.rs:54-88`). The migration guard is independent of writer role.

| Field | shared-pool source | authority-less dedicated | later HA dedicated |
|---|---|---|---|
| `tenant` | Original `T` | Same `T` | Same `T` |
| `kind` | `shared-pool` | `dedicated` | `dedicated` |
| `writer`, `audience` | Original endpoint/audience | Verified target endpoint, new audience | Verified authority-bound writer endpoint/audience |
| `reader`, `read_routing` | None, `writer_only` | None, `writer_only` | Initially none, `writer_only`; reader enrollment is separate |
| `lineage` | Actual `L_s` | Actual restored `L_d != L_s` | `L_d` equals the new authority's lineage |
| `epoch` | Absent | Absent, with explicit proposed `authority_mode:"off"` | Exact current positive authority `E_d`, not `A`, not the source's absent epoch |
| `binding_generation` | Actual served full-set `B_s` | Actual served selected-set `B_d` | `B_d`, matching any later reader |

**Necessary schema change:** current `Route::valid` requires a positive epoch for every dedicated record and forbids epochs/readers on shared-pool (`src/server/control_state.rs:59-65,81-124`). Add optional `authority_mode`, omitted for all legacy records. Explicit `"off"` permits dedicated with no epoch and no reader; omitted/HA dedicated keeps the existing positive-epoch requirement. Reject `"off"` with an epoch. Do not fake `E=1` for an authority-less writer, and do not weaken validation of old malformed dedicated records. Keep legacy standalone snapshots byte-shape-compatible when the field is absent.

For HA, follow `pool-briefs/epic006-design-report.md:38-47,53-104,193-230`: distinct dedicated authority scope, stable combined authority/head CAS, fresh writer incarnation, new lineage, and per-batch authority checks. This is a design input, **not evidence of implementation**. Create a new dedicated authority scope at E=1 only if truly fresh; otherwise use the verified current E without resetting it. Authority-off scope has no election/fencing claim. Never promote one namespace of the shared physical log as an HA writer.

### Atomic cutover and in-flight requests

1. Confirm full target evidence, source fence, imported accounts/quotas/bindings/orphans, fresh tokens delivered, target trust-only ingress, and fresh target readiness. Store evidence binds package hash, `T`, `L_d`, `S`, `B_d`, target endpoint/audience and phase; any change invalidates it.
2. In one targeted control-state CAS, install dedicated route, activate fresh target-lineage credential views, retire source secret hashes from the active snapshot, and set phase `validating`. `A_cut = current A + 1`; do not reset global generation or change other tenants. Preserve original suspension rather than treating freeze as a permission to resume.
3. Keep both backend mutation guards closed until the control generation is confirmed. Source backend retains a durable source-parking rule for this migration; no cached old route may reopen writes. Target permits authorized validation reads only.
4. Requests authenticated before freeze/cutover may finish reading the frozen source, or fail with stale-envelope/route errors. Source cannot commit after its barrier; pending mutations either completed before `S` or explicitly failed. New views and routes are published together, preventing new-token/wrong-lineage mixtures.
5. Wait for old control caches/envelopes to be rejected: confirm participating backends at least `A_cut`, and either confirm gateways or drain/restart unconfirmed old revisions; disconnected/stale caches must fail closed. There is no invented fixed sleep: use configured S_ctl, envelope lifetime/skew and actual drain evidence. The current backend poller raises minimum accepted A independently of requests and checks audience/lineage (`src/server/gateway_trust.rs:143-180,237-253`).
6. Never retry an ambiguous mutation during transition; surface unknown outcome and inspect source before proceeding. Existing forwarding holds one snapshot/target for a request (`src/server/gateway_mode.rs:392-416,445-450`). Extend transition tests for requests paused before authorization, after snapshot capture and inside group admission.

`A`, writer `E`, per-binding generation and binding-set fingerprint are different domains. Current serve status emits null epoch, and current forwarding documentation says epoch response checking is not applied (`src/server/serve_mode.rs:154-169`; `src/server/gateway_mode.rs:30-31`). HA cutover must depend on EPIC-006's actual response/startup/ack checks; this migration feature cannot claim HA merely by writing an epoch field.

## 7. Rollback and source release

### Proposed rollback

Allowed only before a successful release CAS:

1. Gate/drain target validation requests; target must have no acknowledged client mutations since import verification. If evidence shows any, fail closed with `rollback_requires_reconciliation`; never silently discard them.
2. Verify retained source identity and tenant logical inventory, original source binding-set/reservation fingerprint, source trust-only readiness and no conflicting migration. Preserve revocations/suspensions/expiry restrictions accepted since preparation.
3. One CAS changes the **route back to shared-pool** and switches the same freshly re-issued credential views to `L_s`; preserve ID/scopes/limits and all current denials. Restore `writer`, audience, absent epoch, no reader, `writer_only`, actual current `B_s`; use the original route only as a validated tenant-specific template, not an old whole snapshot.
4. Advance `A` again; never decrement A/E or reuse old envelopes. Wait for the reverse propagation/drain barrier. Then clear the migration freeze for the source, without clearing a pre-existing user suspension, and park target permanently. Fresh tokens continue to work; original pre-migration secrets never return.
5. Quarantine target artifacts; do not delete the only evidence of a failed migration. Repeated rollback/status commands reconcile the same migration operation. A concurrent release versus rollback has one winner on the control-state CAS.

Thus rollback is an operator directory transition with an inseparable credential-view edit, not data copying. Source users/data/binding records remain intact. The literal existing directory-only CLI is insufficient and must not be the runbook.

### Proposed `release-source`

Definition: **the durable, irreversible relinquishment of the guaranteed untouched rollback source, and authorization for the dedicated target to begin accepting client writes**. It is not an implicit timeout, successful import, health check, retention job or immediate deletion command.

Require current `validating` phase, expected A/migration/target identity, successful content/access verification, delivery of fresh credentials, confirmed propagation/drain barriers, unchanged target validation state, and `--ack-irreversible`. CAS phase to `released` while leaving the source parked. Only a target that has freshly observed that exact released record may open its mutation gate. A crash after release but before opening writes is safely unavailable, not reversible by pretending release did not happen.

Release first records the terminal decision; later unpinning/retention housekeeping is idempotent and can only run after confirming that decision. On an unknown CAS outcome, both sides stay frozen until exact readback. No cross-object transaction between Blob state and local writes is assumed.

After release, standard rollback command returns `migration_source_released`; a later move back is a new migration with a new freeze/export/verification. Explicit later source cleanup may retire/remove bindings only according to ordinary memory rules and may delete selected resources/backups when approved. Release does not bypass orphan reservations or retirement receipt windows, purge shared data, revoke global users or modify another tenant. Default: retain physical source data until a separate reviewed cleanup.

## 8. Test-first implementation and validation plan

All tests below are **proposed**, not executed migration checks. Use temporary directories/private fixtures, fake clocks and existing in-memory ObjectStore fault patterns; keep emulator tests separately marked. Do not use production accounts or introduce a storage dependency.

| Layer / proposed file | Assertions and traceability |
|---|---|
| `src/tenant_log.rs` unit tests | Fail first on export containing any second-tenant/default canary. Cover two tenant prefixes, near-prefix collision, selected empty collection, sparse IDs, dropped/recreated collection names, graph ID reuse across tenant lifetimes, ordinary suffix no-op replacement, leading history image, corrupt/incomplete source, supported temporal frames and unsupported profiles. Verify exact selected records/revisions/graph receipts, empty neutral default, no foreign retained payload, valid reopened sequence S and fresh lineage. ITEM-027 / TEST-013. |
| `src/server/tenant_migration.rs` unit tests | Filter users/credentials/bindings before serialization; selected config cross-reference failures; stale explicit grant refusal; stable principal ID but different secret/hash; no hash/password/session/URL leakage; expiry/revoke/quota/rate preservation; fresh super-admin collision; orphan-only and retired binding sets; per-binding generation versus computed B; unsupported/unknown sidecar state refuses. ITEM-028 / TEST-013. |
| `src/server/control_state.rs` and `credentials.rs` existing unit modules | New targeted edit changes only T, atomic route/credential lineage visibility, no A regression/overflow, explicit authority-off dedicated validation, legacy dedicated E validation unchanged, concurrent revocation/earlier expiry/tenant suspension/reimport cannot undo migration or revive old secrets. CAS 412, unknown-write exact readback, rollback/release race and stale evidence all fail closed. ITEM-029 / AC-013. |
| `tests/tenant_migration.rs` new integration | Real closed shared file with two tenants: vectors/settings/metadata/TTL/vector extensions/KV, selected graph and evidence, active/retired/orphan bindings, users, explicit/wildcard graph/collection credentials, quotas and closed imports. Export/import, full hashes/counts, deterministic read sample, old secret denied/new accepted, imported accounts disabled, source byte digest unchanged. TEST-013 positive path. |
| `tests/tenant_migration.rs` continuation | Inject target restart/import failure at every publication step, existing/symlink output, wrong hash/length/unknown schema/profile, cross-boundary graph reference in either direction, foreign canary, corrupt orphan record, empty configured bindings with orphan, missing evidence, selected open imports, expired creds, recreated named scope. No partial public target and no changes to source or preexisting paths. |
| `tests/tenant_migration_gateway.rs` new integration | Two gateways/source/target using existing shared-control harness. Target has new L, no target writes before release, POST reads remain reads, source freezes queued/disconnected/group-commit writes, old requests/caches fail rather than commit after S. Cutover reads match sample; reverse directory transition works with the same fresh token and source binding raw/read-scope behavior. Source selected hash unchanged while another tenant appends writes. TEST-013 / AC-013. |
| Same integration: release | At release linearization exactly one CAS wins against rollback; before release all write routes reject, after confirmed release target accepts and source refuses. Lost release response reconciles; stale generation never opens writes; post-release rollback rejected; retention cannot remove pinned source early. AC-013 / FR-015. |
| `tests/gateway_azurite.rs` extension | Separately ignored Azurite test for actual CAS/propagation/unknown transition recovery on a copy. Authority-off case first; authority-on variant depends on EPIC-006 fault tests and verified epoch/head behavior. Local fake store success is not Azure/ACA qualification. |
| `tests/tenants.rs`, `tests/server_cli.rs`, existing recovery/memory suites | Existing direct standalone tenant/quota/isolation, credential issuance/rotate new-ID behavior, recovery disabled-user/rekey-only semantics, memory prefix/as-missing/orphan contracts, authority-off default startup and CLI errors remain unchanged. TEST-016 / AC-010 argument. |

New CLI subcommands should be documented in `docs/API.md` and the existing CLI reference inventory when wired; this design adds **no public REST routes**. If implementation instead adds a route, its owner must update OpenAPI/site API rows/classification and reserved-as-missing coverage, per PRD `:316-317,399`.

Suggested implementation commands (future, not run here):

```text
cargo test -p vlite --features server --lib tenant_log
cargo test -p vlite --features server --lib tenant_migration
cargo test -p vlite --features server --lib control_state
cargo test -p vlite --features server --test tenant_migration --test tenant_migration_gateway
cargo test -p vlite --features server --test tenants --test server_cli --test server_memory_reserved_as_missing
python -B tests\api_reference.py
python -B tests\api_route_inventory.py
cargo fmt --all -- --check
```

Then run existing recovery/credential/memory targeted unit selectors and the project's existing build/lint checks for touched features. Do not list nonexistent proposed integration targets as currently runnable. Run the separate emulator command only with an operator-provided isolated Azurite endpoint, external target/cache directories and no production credentials.

**GATE-006 evidence checklist:** named fixture revision/profile, commands/results, package/evidence digests, source unchanged evidence, target semantic inventory and reads, negative auth/reservation cases, new secret acceptance/old secret rejection, imported user state, successful reverse flip with fresh token, release race proof, and owner target-tenant approval. No gate passes from this document or a created report file.

### HA-off/no-regression argument

Proposed migration state is absent by default; all legacy serializers omit new optional fields. `standalone` remains default, with no control-state requirement, startup file or new store, and the guard is a no-op unless explicitly configured for a migration. Existing ordinary restore/reencrypt/log-segment formats and issue/rotate endpoints remain unchanged. Current mode defaults and empty-reservation no-op precedent are explicit (`src/server/serve_mode.rs:24-36`; `src/server/reserved.rs:4-11`).

Do not alter an existing single-ACA deployment, its ingress, keys or admin directory to install this feature. A tenant can remain shared indefinitely. Gateway fronting/migration are separate operator opt-ins; no claim that shared-pool routing automatically provides HA or that this investigation qualifies ACA concurrency/storage limits.

## 9. Risks, open decisions and recommended defaults

These are recommended decisions under the delegated best-judgment scope, not owner production approvals.

| Risk/decision | Recommended default / trade-off |
|---|---|
| Writable reversible window | Keep target mutations blocked until release. More write downtime, but fulfills untouched-source/lossless-flip rollback without a new replication protocol. |
| Namespace stripping | Keep T and P physically; strip/reapply only in typed inventory/API names. Avoids root wildcard expansion and memory digest rewrites. |
| Credential principal IDs | Migration-only stable IDs, fresh secrets, atomic lineage views. Explicit deviation from normal new-ID rotation, justified by unchanged source bindings and revocation continuity. Never copy old hashes or enable direct bearer ingress. |
| Hidden global coupling in log frames | Validate historic ownership and reject cross-domain graph dependencies. No implicit foreign-resource inclusion, even when a global admin could previously create that relationship. |
| Large shared log | Full retained-history scan/placeholders can be expensive. Require explicit budget/size ceiling and disk preflight; no invented performance numbers. A compact tenant-image codec is later work, not an excuse to activate test-only precision code. |
| Encrypted/timestamp/compact source | Explicitly unsupported in v1, no silent decrypt/drop/downgrade. Report the profile early before freeze. Encrypted cloud recovery is already refused as unqualified (`src/server/recovery_bundle.rs:47-54`); local migration encryption requires a separate custody/projection test path. |
| TTL and erasure | Preserve absolute deadlines/barriers and retained physical bytes. Probe at one controlled clock; document time-dependent reads. No secure-erasure claim or resetting receipt windows during migration. |
| Binding fingerprint collision / orphans | Keep existing B for routing compatibility, full reservation/set SHA-256 in evidence. Orphan-only B=0 still has real reservations. No reader routing during this stage. |
| User/dashboard expectations | Carry authorization identities disabled; reset deliberately in maintenance mode. No session/password portability or gateway login promise. |
| Webhook ownership/delivery | Manual reviewed recreation after release with new secrets, no cursor migration/history replay. Shared subscriptions remain source/global. Report exact re-creation requirements privately. |
| Default shared app administrative exposure | Refuse cutover unless all source/target tenant data ingress is trust-only or demonstrably fenced. Stopping the source for the reversible interval is the conservative fallback, with explicit shared-pool downtime. |
| Authority-less dedicated schema | Explicit authority off, no E and no HA claims; legacy dedicated positive E stays required. HA target waits for EPIC-006 runtime/archival wiring. |
| Multiple operators/restarts | One active migration per tenant, immutable package/evidence, expected-A CAS and exact unknown readback. No local journal used as shared authority. |
| Source release/retention | Manual terminal CAS, no automatic timeout, separate physical cleanup. Pin the verified source before admitting cutover. Release first; unpin later. |
| Config evolution/reimport | Protect migrated credential views against stale admin-directory reimport in code. Keep restriction precedence on every transition; full-snapshot restore is forbidden. |

## 10. Parallel-safe implementation split and test-first order

Ownership below is exclusive **within each wave**. Tests belong to the same owner as their production surface. Shared-file handoffs are serialized; do not assign multiple agents `src/server.rs`, `control_state.rs` or the binary simultaneously.

| Task / wave | Exclusive write ownership | Prerequisites and test-first completion |
|---|---|---|
| A: typed contract + log projection / W1 | New `src/tenant_log.rs`; `src/logops.rs`; `src/lib.rs`; new `tests/tenant_log.rs` | No implementation prerequisite. First define typed package/projection evidence contract without server dependencies and write failing tenant-canary/image/history tests. Implement checked projector, pure logical scanner and no-clobber file output. Keep module public exposure minimal. |
| B: sanitized migration state + rekey / W1 | New `src/server/tenant_migration.rs`; `src/server/recovery_bundle.rs`; `src/server/credentials.rs`; `src/server/users.rs`; `src/server/tenants.rs`; `src/server/reserved.rs` | Parallel with A against a pinned contract sketch. First failing filter/no-secret/stale-ID/orphan/rekey tests; implement pure selection/issuance/import helpers. Do not change ordinary recovery or rotate behavior. Coordinator wires the new module after handoff. |
| C: authoritative transitions + admission / W2 | `src/server/control_state.rs`; `src/server/gateway.rs`; `src/server/gateway_mode.rs`; `src/server/gateway_trust.rs`; `src/server/serve_mode.rs`; `src/server/group_commit.rs`; `src/server.rs` | A/B contract integrated first; consumes credential-view types. Write CAS/revoke/reimport/race and queued-write freeze tests before state-machine/admission code. Own all new server module declarations. No binary edits in this task. |
| D: import/publication orchestration + CLI / W3 | `src/server/cloud_recovery.rs`; new `src/server/tenant_migration_cli.rs`; `src/bin/vlite-server.rs`; `tests/server_cli.rs`; `Cargo.toml` only if explicit test registration is needed | A/B/C integrated. First failing partial publication/CLI/unknown-result tests. Extract existing private staging helpers safely; orchestrate import/verify/cutover/rollback/release. Add module declaration through a serialized handoff from C, not a concurrent edit. No new dependencies. |
| E: end-to-end migration gate / W4 | New `tests/tenant_migration.rs`, `tests/tenant_migration_gateway.rs`; `tests/gateway_azurite.rs`; fixture code inside these tests | Depends on integrated A-D. Write full TEST-013 fixture/assertions first, observe failures, then verify actual implementation. Any required production fixes go back to the file owner in a serialized repair, not overlapping edits. Keep emulator qualification distinct. |
| F: operator documentation / W4 | `docs/API.md`; `docs/SECURITY.md`; `docs/MEMORY.md`; `docs/projects/feature-aca-ha/feature-aca-ha.prd.md`; CLI inventory/reference files identified by existing checks | Parallel with E after D CLI contract is fixed. Document outage/release point, fresh-secret behavior, supported profiles, stable principal IDs, disabled users and literal route-only flip limitation. Update item/gate status only from E's actual evidence. No dashboard asset edits. |

Contract review before W1 must settle the placeholder projection profile, stable-principal migration rekey and read-only reversible window as explicit decisions. This is a dependency handshake, not another runtime service or schema abstraction. HA-specific E/head tests belong to EPIC-006 ownership and can develop independently, but HA target enablement depends on their integration and actual passing checks.

## 11. Checks executed, not executed and remaining uncertainty

### Actually executed in this investigation

* `git --no-pager status --short`: clean starting worktree; revision recorded with `git --no-pager rev-parse HEAD`.
* `python -B tests\api_reference.py` with bytecode writes disabled: **PASS**, all **942** public API items documented.
* `python -B tests\api_route_inventory.py` with bytecode writes disabled: **PASS**, **11 tests**.
* Static source/PRD/design-reference inspection using read/search tools. This is evidence for the current-code statements, **not a runtime migration test**.
* Report artifact check: UTF-8 decoding, nonempty/size/line bounds, explicit citation path/range resolution and Git changed-path inventory. An initial run found an incorrect brief line range; it was corrected before the final check. This validates report delivery only, not the proposed behavior.

The two Python checks are existing baseline documentation/route checks; they do not exercise projection, credential rekey, migration, fencing, rollback or source release. No fixture-only executable migration check was run.

### Not run / validation remains INCOMPLETE

* TEST-013 / AC-013, proposed unit/integration/fault suites: **NOT RUN**; this is a report-only assignment and these migration surfaces/test targets are not implemented here. No claim that a seeded tenant migrated or rollback passed.
* Rust build/test/lint/format: **NOT RUN**; no Rust/manifests were changed, and compiling baseline code cannot validate the proposed new behavior. Available tool discovery found cargo/rustc; this is not a tool-unavailability claim.
* Azurite/Azure/ACA, live deployment, production performance/failover and HA authority qualification: **NOT RUN**; not authorized or required execution for this design task. No production endpoint or credentials were used.

Remaining uncertainty: projection correctness across every supported historical frame needs implementation tests; the exact practical shared-log scan/disk cost is unmeasured; EPIC-006's trusted runtime/authority integration must be reconciled at implementation time; and operator acceptance of the deliberately read-only reversible window and stable-principal rekey is a recorded design decision, not a production approval. GATE-006 remains open until the copy-based executable evidence and tenant approval exist.
