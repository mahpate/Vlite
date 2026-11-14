---
goal: Design Azure Blob recovery backups and an asynchronously replicated, externally fenced VLite standby
version: 1
date_created: 2026-11-13
last_updated: 2026-11-13
owner: VLite maintainers
tags: [design, backup, high-availability, azure, replication, safety]
---

# Introduction

**Status: REVIEWED DESIGN PROPOSAL; implementation and deployment require separate approval.** This plan adds off-host recovery backups first, then one warm asynchronous standby, safe switchover, and finally qualified automatic failover. It does not authorize implementation, cloud changes, production operations, releases, commits, pushes, or pull requests.

The source deployment is Azure Container Apps (ACA) with Azure Files. It is experimental and not qualified for power-loss durability or distributed ownership. **Migration to a separately approved, qualified host/volume topology is a prerequisite for HA, not an accomplished change.** Blob backup alone does not qualify the existing deployment for HA.

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in RFC 2119.

**Cross-reference conventions**: `FR-` functional requirements; `NFR-` non-functional requirements; `FM-` failure modes; `AC-` acceptance criteria; `RD-` proposal decisions; `ASM-` assumptions; `ALT-` alternatives; `GATE-` approval/evidence gates; `SRC-` repository evidence; `AZ-` Azure evidence; `EPIC-` phases; `ITEM-` work items. No separate requirements artifact is needed.

**Evidence boundaries**:

- Pushed baseline: `mahpate/Vlite@25003dd9dace64f5829a4cf2afad23ab8d78edcc`, version 0.7.5.
- Local design integration base: `c61aae91093501dc13a5cc22bac3c65b62259823`. Its compaction changes are committed only locally, not assumed pushed, released, deployed, or approved for this source deployment.
- Source paths and line ranges labeled **local** refer to that local HEAD, not to an available GitHub commit. Published baseline links use the full pushed SHA.
- Existing memory, graph, KV, CAS, and local-backup functionality MUST be reused. Old freeze/backlog documents are not evidence that already shipped functionality is missing.

## 1. Goals and Non-Goals

- **Goal 1**: Recover database content and safe access configuration from a separately protected Azure Blob account, with verified publication and repeatable recovery drills.
- **Goal 2**: Maintain one active writer and one independently stored warm standby with private authenticated replication, durable progress, and honest lag reporting.
- **Goal 3**: Prevent split-brain promotion through external VM power fencing and authoritative ownership, including process pauses and ambiguous responses.
- **Goal 4**: Expose truthful backup, role, readiness, lag, recovery, and promotion status in existing administration/dashboard surfaces.
- **Goal 5**: Deliver independent milestones and measurable acceptance targets without promising zero-loss async HA or arbitrary point-in-time recovery.

### In Scope

- Immutable/versioned snapshot recovery sets; managed identity, encryption, safe access-policy capture, retention protection, isolated restore/drills.
- Zone-local primary/standby on separate supported durable volumes; asynchronous commit-complete batches; restartable reseeding.
- Externally serialized ownership, fail-closed write admission, power-off proof, stable endpoint readiness, explicit client incarnation handling.
- Manual controlled switchover first; automatic promotion only when fencing, access-policy freshness, and an independently established loss bound all pass.
- Explicit integration with local compaction's authoritative selection, generation retirement, history floor, and retained source files.

### Out of Scope (deferred)

- Synchronous data replication/consensus, multiwriter, active/active, sharding, remote object-tier database storage, forks, or the entire scale-out roadmap.
- Region HA, failover of the witness to a geo-replicated secondary, zero-loss guarantees for asynchronous failures, arbitrary PITR from snapshots, continuous log archival in v1.
- General incremental storage-engine replay rewrite, unlimited source sizes, major-release benchmarking, live standby reads, or extending compaction's unsupported-format matrix.
- Unqualified gRPC/embedded-writer HA. V1 HA is the stock REST server under the managed owner; unsupported mutation entry points MUST refuse HA mode rather than bypass its fence.
- Automatic restoration of stale accounts/credentials, deletion of compaction recovery originals, secret values in manifests, or automatic return to the former primary.

## 2. Terminology

| Term | Definition |
|------|------------|
| Recovery set | One verified database snapshot plus a typed, sanitized admin/access configuration bundle and a manifest binding both. |
| `L`, `E`, `G` | Database lineage, externally assigned HA ownership epoch, and local database-selection generation. These are distinct; a Blob ETag is not `E`. |
| `A` | Monotonic access-policy generation covering credentials, tenant state, account changes, bindings, and revocations. |
| `Hobs` | Last source head observed by a receiver, with observation time and freshness. It is not necessarily the source's final head. |
| `D` | Highest complete commit durably persisted, verified, and replayable on the standby for the exact `(L,E,G)` stream. |
| `Uack` | Externally durable upper bound on all successful client mutation acknowledgements in the closing epoch; established by acknowledged-write receipts, not a standby heartbeat. |
| History floor `F` | Earliest complete replay boundary retained by the selected source. Compaction can advance it to checkpoint `S` while retaining `L`. |
| Incarnation | API-visible HA branch identifier, changed on promotion/restore; prevents stale conditional requests from being interpreted in a new branch. |
| Fence proof | Trusted external evidence that the specified former writer VM incarnation is powered off and cannot be restarted by the application/controller. Not a TCP failure, route update, local lock, or lease-expiry claim. |
| Ready writer | Correct owner/epoch, selected source and policy loaded, data and required indexes ready, and all authority checks valid. TCP listening is insufficient. |
| RPO / RTO | Recovery-point loss and time to usable authenticated service, measured separately by failure class; targets below are unmeasured until qualification. |

## 3. Solution Architecture

### 3.1 Evidence and reuse boundaries

| ID | Evidence | Consequence |
|----|----------|-------------|
| SRC-BASE-001 | Pushed `src/server/ops.rs:77-99,566-649,761-889,797-821,1035-1106`; `src/server/admin.rs:1172-1188` | Reuse synced local snapshots, verification, scheduled jobs and metrics. Current schedule defaults to off/24 hours and is process-local; manifest publication is separate from backup publication, uses CRC32, and retention has no consumer pins. Persisting cloud policy and publication/GC is new work. |
| SRC-BASE-002 | Pushed `src/maintenance.rs:157-195,389-446,633-710`; `src/server/admin.rs:1190-1307` | Existing drills are same-host data samples, not server/auth/HA recovery. Ordinary restore changes lineage. Admin selection is restart-only. Recovery seed copies and ordinary restores MUST use different explicit identity policies. |
| SRC-BASE-003 | Pushed `src/logops.rs:214-322,349-421`; `src/replica.rs:40-111,126-215`; `src/lib.rs:2602-2663` | Existing segments carry header/lineage/sequence/hash; follower syncs and fully replays, rebuilding/discarding indexes. Nonempty duplicate apply fails; restart resets remembered source head to local position, so apparent zero lag is not fresh evidence. `promote` only returns a database and provides no fence. |
| SRC-BASE-004 | Pushed `src/server/features.rs:64-130,791-826`; `src/server.rs:1230-1308`; `src/server/grpc.rs:2248-2275` | REST export is base64 JSON with soft batch bounds, not a production receiver. Memory reservations/bindings block export; gRPC replica fields are placeholders. Add a private operator channel without weakening tenant/memory protections. |
| SRC-BASE-005 | Pushed `src/durability.rs:11-72,90-119,138-190`; `src/lib.rs:1695-1705,2396-2522,3918-3947` | Default durability is periodic/60 seconds, not fsync-before-ACK. Snapshot/export force sync; qualified HA MUST select Full/group-sync semantics explicitly on every open and handle indeterminate commits. |
| SRC-BASE-006 | Pushed `src/server/credentials.rs:161-235,405-595`; `src/server/tenants.rs:76-170,237-367`; `src/server/users.rs:135-164,657-702` | Auth/tenant/accounts and revocations live outside DB frames; tenant deletion spans stores. Collection grants bind lineage/ID/name; graph grants are name-based. Preserving bytes alone cannot certify current authorization or graph identity continuity. |
| SRC-BASE-007 | Pushed `src/memory/binding.rs:31-66`; `src/memory/binding/record.rs:1-56`; `src/server/providers.rs:94-160,305-369`; `src/server/webhooks.rs:12-109` | Memory grants and providers need external configuration/secrets; DB provenance is not an upstream source lifecycle registry. Webhook secrets/cursors are sidecars; starting restored integrations can create external effects. |
| SRC-BASE-008 | Pushed `src/collection.rs:132-182`; `src/kv.rs:1-58,419-575`; `src/property_graph/frame.rs:197-342`; `src/server/ops.rs:825-836,1149-1176` | KV/graph canonical frames already exist; do not build separate copiers. Index readiness differs from sequence parity. Key-aware engine APIs exist, but keyless admin verification/drill/selection paths mean encrypted end-to-end recovery is not automatically supported. |
| SRC-BASE-009 | Pushed `src/dashboard/console-tools.js:1793-1823,2025-2058`; `src/dashboard/admin-tools.js:520-540`; `docs/projects/roadmap/scale-out.md:3-14,274-304` | Reuse existing Backup/Replication UI patterns. Downloaded JSON is not a raw segment; roadmap scope is prospective and its older source references are not current implementation evidence. |
| SRC-LOCAL-001 | Local `src/server/compaction.rs:1-14,291-425`: `write_atomic`, `selected`, `selection_guard` | Mutable checksummed state and authoritative selection are separate. A committed compaction selection pins startup and currently refuses ordinary restore reselection. HA MUST NOT delete that authority or write only `next-startup`. |
| SRC-LOCAL-002 | Local `src/server/compaction.rs:595-704,799-1059`; `src/server.rs:547-607`: `Managed::prepare`, `serve_recovery`, `Service::successor`, `retire` | Reuse database-independent authentication, generation reconstruction, permit draining and authenticated recovery. The existing namespace fence is local maintenance coordination, NOT external HA fencing. |
| SRC-LOCAL-003 | Local `src/server/compaction.rs:1122-1141,1202-1259` | Compaction qualifies Linux local ext4, same device for active DB/admin, at most 1 GiB; refuses Azure Files and unsupported state. Existing history consent is lineage-level operator acknowledgement, not a durable replica checkpoint protocol. |
| SRC-LOCAL-004 | Local `src/server/compaction.rs:1477-1533,1556-1686,1750-1908` | Source snapshot/build/selection and cancellation have irreversible boundaries. Authority loss MUST dominate every code path that currently reopens maintenance admission. |
| SRC-LOCAL-005 | Local `src/maintenance/compaction.rs:1-15,90-160,273-390`: `scan`, `compare`, `transform` | Frozen-copy directory conversion then packing preserves lineage/global sequence/allocator high-waters; graph revisions can advance to `S`. Existing refusals and stale-CAS re-read rules remain. |
| SRC-LOCAL-006 | Local `src/server/admin.rs:267-440,1303-1340`; `src/bin/vlite-server.rs:700-739,788-805` | Control state can precede database opening. Restore/promotion needs a recovery-capable selection transition, not an assumed hot swap; managed compaction is not enabled with gRPC. |

All `SRC-BASE` ranges above are pinned to [the pushed baseline](https://github.com/mahpate/Vlite/tree/25003dd9dace64f5829a4cf2afad23ab8d78edcc). The [full independent report](https://github.com/mahpate/Vlite/blob/eb936afa4202affd9f3e1aec91ca1fb8f2df5d81/reports/5b44b03d5e384ed5a871f2c5694cda33/b7731f6d6914400d8891332a607c9138.md) includes complete citations, limitations and test inventory. Its results are static evidence, not executed recovery/HA qualification.

### 3.2 Recommended hosting topology

```text
Clients -- stable TLS name --> Standard L4 Load Balancer
                                  | role-aware readiness
                        +---------+----------+
                        |                    |
                   Linux VM A           Linux VM B
                   active writer        warm standby
                   managed ext4 disk    independent managed ext4 disk
                        |                    ^
                        +-- private mTLS ----+
                        |
             frozen recovery set --> separate protected Blob backup account

Both host controllers --> regional ZRS authority account (one primary endpoint)
                       --> Azure ARM powerOff for the OTHER named VM
                       --> current access-policy bundle / secret references
```

- Recommend **two Linux VMs in separate availability zones, each with its own managed data disk**, ext4, database/admin on the same local filesystem per host. A managed disk is locally attached storage from the guest's perspective; its actual VM/filesystem/cache/flush profile still requires qualification. Never mount one writable database volume on both nodes.
- Separate OS/data volumes, systemd-supervised boot into `FENCED`, TLS on both servers, private replication subnet/NSG rules, managed identities, a stable DNS name and Standard Load Balancer. No third always-on VM or L7 gateway is required in the proposed small topology.
- One controller agent per VM competes for the same authority record/lease. It may power off only its specifically allowlisted peer. V1 controller identities MUST NOT have VM start permissions. Peer restart/rejoin is an explicit operator workflow; boot always requires new authorization before writer service.
- A **separate regional ZRS control account** holds ownership, policy pointers and acknowledgement receipts. It is not the immutable backup container. Account credentials, deletion rights and lifetime are isolated from the application backup uploader.
- The backup account is separately protected, with versioning/soft-delete/immutability policy chosen together for supported Azure account/container features; lifecycle deletion cannot override legal hold or locked retention. ZRS protects against a zone outage, not logical corruption. Optional separate-region backup copies incur additional cost and do not provide region HA.
- AKS is viable only where a team already operates it: separate zones/PVCs, qualified CSI/disk semantics, and node-level external fencing still required. Pod eviction, Kubernetes Lease objects and PVC attachment are not proof that a paused writer stopped.
- Current ACA + Azure Files is rejected as the HA substrate. Keep it explicitly experimental until the approved migration; backup uploader development may ship independently, but backup consistency on this source retains its existing durability caveat.

### 3.3 Recovery-set publication and restore

1. Reuse the existing local scheduler/job path to request a backup. Resolve and pin the **actually selected** source `(L,E,G,path identity,F)`, not the original CLI path. Serialize configuration capture with source selection and access-policy changes; take a short checkpoint barrier, drain actual work including admin persistence outside the DB lock, sync, and freeze a database snapshot. A DB owner lock alone is insufficient. Validate cross-store invariants and refuse a partial tenant/account transition. Resume serving only if owner authority remains valid.
2. Produce a typed bundle of credential hashes/grants, accounts/password hashes, tenant state, memory-binding configuration when supported, feature settings and safe runtime configuration. Bind it to `A`, resource identities and DB lineage. Inventory every sidecar; unrecognized security-relevant state MUST fail bundle certification. Token files, TLS private keys, provider keys, webhook signing secrets and DB encryption keys are **references to separately recoverable secrets**, not plaintext manifest fields.
3. Do not copy the admin directory wholesale. Exclude sessions, locks, caches, local absolute paths, in-progress jobs and machine identity. Serialize a normalized selected-database identity and completed maintenance provenance, not portable copies of old selection paths. Audit files may be separately archived under a declared policy; they are not silently omitted from compliance requirements or treated as application state.
4. Manifest schema v1 MUST contain recovery-set UUID, writer software/format versions, installation identity, `L/E/G/F/S`, UTC times, length and SHA-256 per object, complete-commit/tail verification result, safe relative names, policy capture revision/digest, secret reference versions, restore compatibility flags, and retention/legal-hold references. `A` is present only after the current-policy authority exists; without it the manifest MUST declare `rekey_only`, never claim current authorization. A checksum detects corruption, not an authorized malicious rewrite: protected account permissions, immutable version identities and manifest authenticity are part of trust.
5. Upload immutable, uniquely named data/config objects first. Retry staged/block uploads by object identity; verify committed object length/hash (including a streamed read-back during certification). Create the immutable manifest last with create-if-absent conditions and pinned object version IDs, then commit its exact identity through the per-set coordination record described below. Consumers accept only manifests referenced by a committed `PUBLISHED` record; an orphan manifest is not a recovery set. A `latest` pointer is optional convenience, never recovery authority.
6. Persist local job state with write-new/file-sync/rename/directory-sync. A successful cloud PUT with an unknown response is reconciled by exact key/version/hash; it is not retried as a second logical backup or declared successful without verification. Partial uploads and orphaned data never become recoverable catalog entries.
7. Retention uses immutable committed manifests and serialized pins, not a blind age-based prefix delete. Keep a default minimum of three verified sets and 30 days, subject to owner approval and capacity; never delete the last verified set, held sets, active restores/drills, selected files, or replica seed pins. Delete permissions belong to a separate janitor/operator identity. Legal hold/immutability refusal is expected, audited, and capacity-alerted. Staging-orphan cleanup has a separate grace period, but a grace period alone is not protection against a paused publisher.
8. Restore downloads to **new isolated durable storage**, verifies all hashes/formats/lineage, opens and replays the DB, validates auth/resource bindings and required indexes, then exercises representative graph/KV/vector/CAS reads. Never drill against the live database/admin directory or redirect the production endpoint.
9. A recovery set is not permission authority for returning old credentials to service. **The minimum backup milestone is explicitly rekey-only**: it restores non-secret configuration and account/grant inventory disabled, uses ordinary restore's new lineage, establishes fresh break-glass administration through a private operator channel, and requires reviewed regrants. It cannot certify old credentials as current. After EPIC-009/010, continuity-capable recovery reconciles exact current `A` and secret versions before readiness. That mode uses a verified lineage-preserving backup copy under the managed owner, not ordinary `maintenance::restore`, with a new API incarnation and explicit former-source fencing before writable activation. If policy cannot be established, remain isolated/not-ready or use separately approved rekey-only disaster recovery. No automatic rollback of revocations.
10. Restore selection is a new transaction in the managed selection owner: stage verified candidate and normalized control bundle, record intent, publish authoritative selection, reopen a whole generation, then mark success. Uncertain selection never falls back to the original. Cancellation before committed intent cleans only unselected artifacts; after intent it completes recovery or stays fenced.

**Publication/pin/deletion protocol (backup-only; independent of HA authority)**:

- A mutable, versioned coordination container holds one never-reused per-set record with state `STAGING`, `PUBLISHED`, `DELETE_PENDING`, or terminal `TOMBSTONED`; immutable payload/manifest objects are in the protected data container. State updates use the record's Blob lease plus ETag CAS. The lease serializes cooperating catalog actors, not database writes.
- Create `STAGING` before uploading. Publish immutable data and manifest, then CAS `STAGING → PUBLISHED` with the exact manifest version/hash while still authorized by the current record lease. This CAS is the recovery-set publication linearization point. If a paused publisher loses its lease and a janitor tombstones staging, its late object uploads are orphans and its final CAS fails; it cannot recreate/reuse the set ID.
- A restore/seed acquires a durable pin by CAS on the same `PUBLISHED` record **before reading any object**. Janitor CAS to `DELETE_PENDING` requires zero pins and retention eligibility, and freezes further acquisitions/publication. Failed/unknown pin updates must reconcile before reading. Pins are not automatically expired while a consumer might run; crashed jobs require explicit safe reconciliation, with leaked pins alerting rather than permitting unsafe deletion.
- A separate leased catalog decision record serializes deletion eligibility across sets, including minimum/last-verified protection. Janitor records the deletion intent and protected survivor identities before CASing the target to `DELETE_PENDING`; concurrent janitors cannot each delete the other's assumed survivor. Ambiguous catalog decisions stop GC. Surviving sets stay protected until the decision completes/reconciles.
- Only after committed `DELETE_PENDING` may the janitor delete the exact versions, respecting immutable retention, then record `TOMBSTONED`. Coordination tombstones are not removed/recreated by data janitors. A paused publisher may create late orphan bytes but can never publish them; later cleanup reclaims them without exposing them as a set. No blanket prefix-delete privilege is granted to the uploader.
- Apply equivalent serialized source pins to local retention, selection and compaction. A cloud pin does not itself protect a local source file.

### 3.4 Replication protocol and progress

- Run an **unserved standby follower**, not a second ordinary writer. Seed from a pinned verified **lineage-preserving backup** at `S` (never ordinary lineage-changing restore), apply commit-complete batches after `S`, and keep all user mutations, schedulers, webhook dispatch and TTL mutation workers disabled on the standby.
- Replication transport is private mTLS with installation/node identity and short-lived certificates; it is not a tenant data token. Reuse existing export/follower validation, but add an explicit protocol envelope: schema, installation, `L/E/G/F`, batch ID, start/end sequence, complete-commit marker, payload length/hash, observed source head/time and access-policy requirement.
- Enforce maximum accepted **complete-commit** size, bounded in-flight queue, backpressure, retry jitter, checksummed staging, no caller paths, anti-replay, and capability negotiation. Existing export byte limits are soft; reject an oversized complete commit/profile rather than split it or overrun memory. No split commit may advance `D`. The transport wrapper deduplicates by certified batch identity/progress; the underlying follower is **not idempotent** for nonempty duplicate apply. After a lost ACK, reconcile actual durable progress and request the remaining suffix instead of blindly retrying.
- Bind the stream to a seed hash and chained preceding boundary digest in addition to exact header/lineage/sequence and authorized `E/G`. Existing same-lineage/sequence checks alone cannot prove identical historical prefix. Header changes, gaps, changed source selection or incompatible formats require reconciliation/reseed, even if lineage stayed constant.
- Persist bytes first, sync, validate replayability, then publish the receiver checkpoint with atomic durable replacement; acknowledge only after both are established. On restart derive progress from actual valid durable bytes and the committed checkpoint. Existing reopen can retain complete commits beyond the last batch ACK. Extra complete bytes cannot advance reported `D` until their prefix digest is verified against the authorized stream; otherwise rebuild from the pinned seed and certified batches. A torn checkpoint must not advertise bytes that are missing, and a poisoned follower handle is closed/rebuilt, not reused.
- Show **received**, **durable**, **applied**, and **service/index-ready** positions separately. `D` is not just the largest received number. Report `(Hobs-D)`, source-observation age, bytes queued, last durable time, floor and seed progress; display `unknown/stale` when the source observation is stale. Never call it current RPO.
- Existing follower apply replays the whole file and rebuilds/discards indexes; v1 MUST NOT market that as an incremental production engine. Batch/coalesce off the serving path, bound source size/rate, and measure catch-up capacity. A targeted replay/index-lifecycle optimization is allowed only with parity tests; a general incremental engine rewrite triggers re-estimation.
- Proposed initial qualification envelope is the stock REST, plain v5 single-file profile, source at most 1 GiB, representative vector/graph/KV/CAS mix, up to 100 mutation commits/s and 1 MiB/s serialized changes. These are **test inputs, not supported or measured capacity**. Data/feature inventory may lower the qualified envelope. Memory-reserved, temporal, encrypted, TTL/import-bearing, configured-webhook and gRPC combinations require explicit recovery/replication evidence before inclusion; v1 refuses HA for them. Do not silently relax memory export authorization or activate restored external integrations. No existing product feature is removed or reimplemented; these features continue outside the new qualified HA profile.
- Qualified HA explicitly selects Full durability, or equivalently proved source group-sync-before-response, on every initial open and successor generation. The existing periodic default is not an acceptable HA acknowledgement contract.
- Replication and backup source pins are durable retention references. Falling below `F`, including a partial boundary at checkpoint `S`, requires a new seed, never a silently advanced cursor. Same lineage does not imply continuous history.

### 3.5 Authority, external fencing and promotion

**Safety invariant**: before a new writer admits ANY data operation, the former writer MUST be externally incapable of executing writes, ownership MUST be durably advanced, and local selection/access state MUST be validated. Availability is sacrificed when proof is unavailable.

The regional authority record includes schema, installation ID, monotonic explicit epoch `E`, owner VM resource ID and boot identity, allowed peer, selected source identity, state, policy generation/digest `A`, acknowledgement-receipt pointer, and promotion operation ID. It is updated with both lease authorization and compare-and-swap on record contents. A lease is serialization for this record, **not fencing of local disks**.

| State | Required behavior |
|-------|-------------------|
| `FENCED` / boot | Control-only authenticated recovery; no user DB reads/writes, schedulers or mutation workers. Acquire no writer capability merely because local files exist. |
| `FOLLOWING` | Receive/apply the authorized stream only; no ordinary writer service. Report durable progress and readiness prerequisites. |
| `WRITER` | Every admission and background mutation has the current owner capability. Successful client mutation acknowledgement requires source durability and an authority receipt as described below once qualified automatic mode is enabled. |
| `AUTHORITY_UNCERTAIN` | Close admission immediately, stop dispatch/queues, drain only while safe, and expose not-ready. Unknown renewal results count as loss until independently reconciled; do not extend a local deadline on an unanswered request. |
| `FENCING_PEER` | Successor holds serialized authority but is not a writer. Issue powerOff for the exact former VM incarnation; wait for trusted terminal fence completion and power state proof. Unknown/failed/stale operation remains fenced. |
| `PROMOTION_PENDING` | Freeze old-epoch authority receipts; establish final loss bound, current `A`, selection, lineage, durable `D` and index readiness; persist promotion intent and a new epoch/incarnation. |
| `WRITER_READY` | Open the complete new generation only after all checks. Enable the role-aware readiness endpoint, then permit load-balancer routing. |
| `REJOIN_REQUIRED` | Returning former primary remains quarantined; preserve its divergent data for forensics, never merge it or auto-failback. Wipe/reseed only by an authorized workflow. |

**Fencing mechanism/proof**:

- Use the Azure VM external power-fencing pattern, preferably a maintained fence agent on a specifically qualified OS/controller package. A tested adapter may wrap that agent; do not treat a shell command exit, HTTP 202, or initial ARM operation ID as completed fencing.
- Bind proof to the intended VM resource/unique identity, known former owner epoch, fence operation ID, terminal operation success and trusted powered-off/deallocated status. V1 allows no automatic restart action. Deny application identities VM start privileges, disable unsafe automatic restart/run paths, and audit privileged starts.
- Qualification MUST establish the chosen Azure power-off operation's terminal semantics and test pause/storage-write scenarios. The safety argument assumes trusted Azure compute/control-plane behavior and enforced Azure RBAC. If those semantics cannot be established for the chosen agent/platform, **automatic promotion is disabled**; an operator must complete equivalent STONITH/power-off isolation and record proof. A manual button cannot waive proof.
- Once the old VM is actually powered off, its paused user process and outstanding guest writes cannot execute further, and it cannot communicate with clients; independent disks prevent those old writes from reaching the new writer's disk. Writes that completed before fence completion may be lost asynchronously and must be included in the loss analysis. Route changes alone provide no such proof.
- Check admission at REST, service mutation execution, queued/group commit, background maintenance and selection boundaries. Local authority deadlines reduce exposure but are not the split-brain proof: a paused process can miss timers. Power fencing remains mandatory even if the primary claims it drained or its lease expired.
- Lease acquisition/renewal uses a monotonic local safety budget shorter than the requested server lease. A delayed response must be matched to the exact operation, owner and epoch; an expired old lease may be renewed in Azure if not superseded, so expired local capabilities never silently reopen service. Reconciliation requires authoritative record/lease ownership and a fresh ready-generation transition.
- The witness is single-region/single authoritative endpoint. On regional witness unavailability both sides lose availability. Do not redirect it to a GRS/GZRS secondary; asynchronous geo-copy could resurrect an old epoch.

**Independently bounded asynchronous loss**:

1. A standby's last observed lag is insufficient after a primary partition. Default mode ships with automatic promotion disabled. Manual controlled switchover can prove an exact drained barrier; uncontrolled failure cannot assume that barrier.
2. Qualified automatic mode adds **durable acknowledgement receipts** to the authority service. Before returning any successful user mutation response, the primary syncs its local commit and durably publishes, through the lease-holding controller, a receipt for its epoch and acknowledged sequence range. Group-commit responses may share a receipt. Receipts cover every mutation surface; uninstrumented writes make automatic mode unavailable. An uncertain receipt publication MUST NOT produce a successful response.
3. Use one controller serialization point for receipt/policy/selection changes on the leased authority record. Write an immutable receipt body containing epoch/range/prior receipt head, then CAS the authority record to a head that includes that body and `Uack = max(current Uack, receipt end)`. **The successful, durably confirmed CAS—not body upload—is the acknowledgement linearization point.** CAS retries reread and merge newer `Uack`, policy and selection fields; they may never regress/overwrite them. Rebased receipt chains use a new immutable body identity. An unknown CAS is reconciled by reading the current record and proving inclusion before returning success.
4. After authority takeover and external fencing, close the old epoch through the **same authority-record CAS serialization point**, reconciling all accepted prior operations and freezing its receipt head/`Uack`. Stale appends cannot succeed against a closed epoch/new lease. A receipt can overestimate success if the client never received its response; it must never underestimate a successful acknowledgement. Persist receipt bodies/versions and closure; do not infer `Uack` from lease ETags or discover it by a potentially incomplete listing.
5. Require the candidate to be the verified prefix of that epoch/lineage and use `max(0,Uack-D)` as a conservative commit-sequence loss upper bound, not a count of rows. Gaps can overestimate loss. Unknown/future schema, unresolved writes, missing receipt chain, policy uncertainty or incompatible stream means **no promotion**.
6. **V1 prohibits nonzero-loss promotion**, manual or automatic. Automatic eligibility requires `D >= Uack`; otherwise remain fenced/unavailable or follow a separately approved isolated disaster restore/rekey workflow, not an unsafe promote override. This is a **fail-closed eligibility rule**, not an RPO=0 service guarantee: async failures can leave the service unavailable indefinitely. No seconds-based loss bound is inferred from old `Hobs`. Lossy promotion is a deferred branch/checkpoint-format design and must be re-estimated.
7. Receipts add a synchronous external **metadata** dependency and latency to successful mutation acknowledgements; data replication remains asynchronous. This is not a synchronous data quorum. If this cost is rejected, retain backups + standby + safe manual switchover, and automatic fencing/status only; automatic promotion on unknown loss remains disabled.
8. Promote to a new API incarnation. HA-mode mutation clients MUST supply the current incarnation on conditional writes and resource-identity-sensitive operations; stale/missing incarnation is rejected with a re-read-required error before mutation. Native revision values are not silently assumed globally unique across an unacknowledged discarded branch. Retain the existing contiguous allocator: the next commit is `D+1`, which exceeds `Uack` because eligibility requires `D>=Uack`. **Do not insert a sequence gap or advance marker into an existing log.** Validate resource/allocator/binding identities and suspend ambiguous grants. Unknown outcome retries use application idempotency/reconciliation; v1 does not promise exactly-once requests.

**Controlled switchover**: close old admission, drain actual work, sync, record final `S` and `A`, require standby `D=S` with current policy, shut down/power-fence old VM, commit new epoch/incarnation/selection, rebuild service/indexes, then route. If any step is uncertain, remain unavailable or keep the still-authorized old writer only before irreversible handoff. No forced manual promotion when loss policy is unknown.

### 3.6 Access policy, readiness and compaction interactions

- Treat access configuration as a versioned recovery object, not merely files copied asynchronously. Admin changes affecting permissions/revocations publish a complete immutable policy bundle and durably advance authoritative `A` before success. The active runtime installs it atomically; a failed/ambiguous policy publication fences affected authentication/administration until resolved. Promotion requires exact current `A`, never a best-effort stale copy.
- Preserve existing hashed credentials and lineage/ID bindings during same-lineage compaction. During continuity-capable failover/restore, validate that every grant binds to an existing matching tenant/resource identity; absent or ambiguous resources disable that grant rather than rebinding by name. Baseline graph grants are name-based: the HA policy bundle must add a capture-time graph identity constraint (or suspend the grant on recreation), not falsely claim existing graph ID binding. Tenant deletion, suspension, account disablement, token revocation and secret rotation are included. Before that authority is delivered, backup recovery remains rekey-only.
- Sessions remain node/process-local and are invalidated across promotion; users sign in again. Ordinary API credentials can continue only after current policy and secret-version validation. Revoked credentials and old admin token-file versions MUST fail on the new node; credential continuity is not revocation rollback.
- Upstream provenance/source IDs are data, not a generic source/connector lifecycle service. Capture provider/binding references and recovery prerequisites; do not claim replicated provenance restores upstream access or replays source revocation/deletion. Restored providers/webhooks remain disabled until configuration/secret policy is validated. Forget/delete/TTL does not erase historical backups: retention/key-retirement and legal requirements remain explicit operational policy.
- The new writer readiness endpoint evaluates epoch/lease confidence, fence proof, selection recovery completion, DB replay, policy `A`, binding validation, required provider/feature initialization and required indexes. Health is liveness only. Load-balancer probes are not promotion authorization.
- **ITEM-007 owns the shared selection-transaction foundation** for restore/reseed/promotion. EPIC-011 consumes it; EPIC-014 only integrates HA compaction/floor/pin rules. One managed selection transaction coordinates all callers; do not build competing marker writers. Reuse authoritative selection semantics, add typed transition records/dependency pins, and never rewrite completed compaction history or change its preservation contract.
- Before compaction advances `F` to `S`, suspend automatic promotion, close all old-generation source/export pins, obtain durable acknowledgements from registered consumers for either checkpoint/reseed, and retain a verified seed at the new floor. A mere old lineage-level `resnapshot_allowed` setting is insufficient for the HA consumer.
- After compaction selects the candidate, publish `(L,new G,F=S)` and seed/checkpoint manifest as one recoverable transition; the standby either adopts the certified same checkpoint or reseeds. Promotion remains disabled until its stream, source generation and `A` match. Preserve old sources until all dependent backup/upload/reseed operations release pins; no automatic deletion of compaction originals in v1.
- Loss of authority during freeze/build/cancel/recovery MUST prevent **all** current maintenance paths from setting the writer back to open. Before selection intent, cancel and reconstruct only as fenced; after committed/uncertain selection, finish reconciliation on the selected candidate, never reopen the old source. The compaction `OPEN` flag cannot override the HA owner gate.
- Graph revisions advanced to `S` still require clients to re-read before CAS. Memory/graph/KV semantics are reused; unsupported feature combinations are truthfully unavailable, not downgraded.

### 3.7 Configuration and operator/API surface

| Setting/surface | Proposed behavior and reason |
|-----------------|------------------------------|
| `backup.cloud.enabled` | Default false; explicit account/container/managed-identity references; no arbitrary URLs or secrets from browser inputs. |
| Backup interval/retention | Persist validated policy; default proposed 60-minute interval, minimum three verified sets and 30-day age protection. Existing local schedule/retention remains the execution path. |
| Upload bounds | Bounded parallelism, retry budget and staging quota. Exhaustion alerts and fails backup without corrupting live service. |
| `ha.enabled` / profile | Default false; compiled/evidence-backed profile and installation/node allowlist. No unsafe filesystem/fencing override flag. |
| Replication | Allowlisted private peer/certificate identity, batch/queue limits, supported format envelope. |
| `ha.auto_promote` | Default false; enabled only after gates and owner approval; receipts mandatory. Qualified default loss budget is zero potentially missing acknowledged commit sequences. |
| Loss policy | V1 fixed policy `require_complete_acknowledged_prefix`; `D>=Uack` required. No nonzero-loss override. Workload/commit-size envelope is explicit; unknown bound always blocks. Seconds-lag display is not this policy. |
| Lease/authority | Proposed 60-second lease with renewal at most every 10 seconds, local fail-closed deadline no later than 30 seconds from conservative request start; values are fault-test parameters, not an RTO guarantee. |
| Admin REST | Installation-admin-only jobs/status for cloud backups, restore drills, replication/reseed, switchover and HA policy; explicit `Need::Admin`, CSRF/same-origin checks and bounded request schemas. |
| Dashboard | Extend Backup & recovery and Replication cards: off-host verification age, retained bytes, last drill, `D/Hobs` freshness, `E/G/A`, fence phase, seed status, disabled reasons and estimated loss interval. Never show secret material or a green badge for stale evidence. |

## 4. Requirements

**Summary**: Recovery correctness and single-writer safety take precedence over availability. All targets require qualification; nothing in this proposal enables production.

**Items**:

| ID | Requirement |
|----|-------------|
| FR-001 | Publish only complete, verified, immutable recovery sets to a separate protected Blob account, reusing local snapshot/scheduler primitives. |
| FR-002 | Persist backup policy, protect retained/pinned artifacts, reconcile partial/unknown uploads, and run restores/drills off live storage. |
| FR-003 | Recover safe DB and admin/access state without reviving revoked credentials, disabled tenants or stale sessions. |
| FR-004 | Replicate complete batches privately with durable exact-source progress, bounded queues and explicit reseed/floor behavior. |
| FR-005 | Enforce one active writer using externally serialized ownership and proven peer fencing; ambiguity MUST fail closed. |
| FR-006 | Provide safe controlled switchover and qualified automatic promotion only under independently established loss policy. |
| FR-007 | Publish truthful role/readiness/dashboard/status and stable endpoint behavior; health/routing MUST NOT substitute for fencing. |
| FR-008 | Preserve compaction selection, source pins, cancellation safety, lineage/CAS rules and credentials across generation changes. |
| SEC-001 | Use managed identity, least privilege, private authenticated replication, encrypted transport/storage and separate deletion/fencing identities. |
| SEC-002 | Require current access-policy generation and HA incarnation; stale grants/tokens/conditional writes MUST fail closed. |
| NFR-001 | Never acknowledge standby progress before durable commit-complete bytes/checkpoint; never promise zero-loss async HA or snapshot-only arbitrary PITR. |
| NFR-002 | Qualify bounded size/workload/RPO/RTO and fault/soak behavior; unsupported profiles expose a reason and cannot be overridden through the dashboard. |
| NFR-003 | Bound resource use, retention growth, retries and audit cardinality; emit operational evidence sufficient to diagnose every blocked promotion. |
| CON-001 | Host migration from experimental ACA/SMB is an explicit prerequisite; Linux local ext4 compaction qualification is not Azure VM HA qualification. |
| CON-002 | No implementation/cloud operations/version bump/main-checkout edits are part of authoring this PRD. |

## 5. Risk Classification

**Risk**: 🔴 HIGH RISK

**Summary**: This adds distributed failure handling to a single-process database. The hardest work is proving ownership, access-state continuity and bounded loss—not sending snapshot bytes to Blob.

**Items**:

| ID | Risk or assumption | Mitigation / approval consequence |
|----|--------------------|-----------------------------------|
| RISK-001 | Azure Files/ACA is not the qualified durability/fencing substrate. | GATE-002 migration and host qualification before HA. Backup-only rollout keeps caveat. |
| RISK-002 | Full-file follower replay/index work may not sustain desired write rate on 8 GiB VMs. | Early workload gate, bounded batches, memory/index profiling; reduce profile or re-estimate, not optimistic incremental claims. |
| RISK-003 | Stale observed lag hides writes after a partition. | Authority receipts before success and frozen `Uack`; no promotion from `Hobs` alone. |
| RISK-004 | New access-policy publication and incarnation requirements affect clients/latency; backup-only lacks that authority. | First backup milestone is explicitly rekey-only; later continuity requires EPIC-009/010. Measure metadata latency, identify all mutation paths and old-token behavior. |
| RISK-005 | ARM fencing can be slow or unavailable. | No seconds-level guarantee; block promotion, alert and retain manual STONITH gate. |
| RISK-006 | Selection/floor transitions overlap backup/replication/cancel. | One transaction owner, durable pins, generation-aware streams, crash injection. |
| RISK-007 | Restoring old sidecars resurrects access or binds reused resource IDs. | Current `A`, identity validation, fail-closed grants, rekey-only isolated disaster mode. |
| RISK-008 | Locked retention or runaway full snapshots exhaust budget/storage. | Quotas, forecasted retained bytes, separate delete authority, explicit grace/hold handling. |
| ASM-001 | Initial request is zone HA, not region HA; one writer and one standby suffice. | Revisit design/cost/effort if region recovery automation is required. |
| ASM-002 | Initial target is at most 1 GiB and REST/plain-v5; real workload/feature inventory is not yet supplied. | GATE-001 measures source size/rate/features before implementation promises. |
| ASM-003 | Trusted Azure power-fence semantics, RBAC and host boot policy can be qualified. | If disproved, no automatic promotion; safe manual recovery only. |
| ASM-004 | Owner accepts acknowledgement-metadata latency and incarnation-aware clients for qualified automatic mode. | If not, do not claim unattended bounded-loss failover; retain manual milestone. |

## 6. Dependencies

**Summary**: Backup can ship before HA. HA depends on host migration, verified replica durability, current access policy, externally proved fencing and a client/loss-policy contract.

**Items**:

- **DEP-001**: Existing local compaction commits must be reviewed/integrated through a separately authorized workflow before code relies on them. This document does not push them.
- **DEP-002**: Approved Azure VM/disks/network/identity/backup/control-account deployment, quota and region/zone availability; no provisioning during planning.
- **DEP-003**: Chosen Rust Azure Blob/identity client versions and license/MSRV review; minimal retry/conditional-write surface. No custom cryptography.
- **DEP-004**: Supported fence agent/controller package and OS, managed identity ARM connectivity, peer allowlist, power-state semantics and permissions proof.
- **DEP-005**: Current source inventory, client mutation/CAS compatibility, access-state/secret recovery inventory, acceptable data-loss policy and operational owner.
- **DEP-006**: Existing source lock/sync/verification/replay APIs; any discovered invariant gap is a re-estimation gate rather than permission for an unbounded engine rewrite.
- **DEP-007**: Azure facts/costs in section 14 are parent-supplied official-document/public-price evidence, not subscription quotes or measured service capacity.

## 7. Quality & Testing

**Summary**: Extend existing tests, add deterministic fake Blob/authority/fencer fault injection, then run bounded real Linux/ext4 and Azure disposable-environment qualification after separate approval. No major-release benchmark campaign.

**Items**:

- **TEST-001**: Recovery-set schema, hashes, conditional publication, crash/retry/unknown results and retention reference invariants.
- **TEST-002**: Restore/drill isolation, data parity, auth/revocation/tenant/binding continuity, secret reference failure and readiness.
- **TEST-003**: Commit-complete replication, duplicate/reorder/gap/torn tails, durable restart progress, lineage/floor transitions and bounded catch-up.
- **TEST-004**: Model/state-machine tests for authority/epoch/fencing and delayed/out-of-order responses; tests for all mutation entry points and receipt-before-success.
- **TEST-005**: Full-process pause, partition, storage write stalls, group queue/disconnect work, controller death, ARM unknown responses, stale owner restart and route/probe staleness.
- **TEST-006**: Compaction freeze/build/selection/cancel crashes with backup/reseed/promotion interleavings; pins and access generation preserved.
- **TEST-007**: Dashboard/admin RBAC, same-origin/CSRF, disabled reasons, redaction and stale telemetry.
- **TEST-008**: Focused 1 GiB workload test, source-rate sweep, receiver headroom, heap/replay/index time, and two-week fault/soak observation. AI assistance cannot replace real elapsed soak time.

### Acceptance Criteria

| ID | Criterion | Verification | Traces To |
|----|-----------|--------------|-----------|
| AC-001 | Crash/pause before/after each upload or publication-CAS stage exposes either no committed set or a complete hash-valid set; unknown PUT reconciles exact identity; tombstoned staging cannot publish a manifest. | Fault-injected object-store tests, expired publisher pause and isolated Blob drill. | FR-001, FR-002, FM-001 |
| AC-002 | Last verified set, legal hold, active seed/restore and selected/compaction source pins survive retention; concurrent janitors cannot delete all survivors; pin acquisition and delete intent are mutually exclusive. | Property tests at pin/delete/catalog-CAS boundaries and least-privilege denial checks. | FR-002, SEC-001, NFR-003 |
| AC-003 | Drill restores DB plus safe admin bundle to new storage; no live path, production endpoint or production credential mutation occurs. | Before/after path/identity assertions and recovery report. | FR-001, FR-002, CON-002 |
| AC-004 | Backup-only recovery disables all imported credentials/accounts and requires fresh admin/regrants. After current-policy authority ships, continuity mode rejects revoked API/admin credentials, disabled/deleted tenants/accounts and stale sessions; valid exact-bound grants remain valid; missing/ambiguous resources do not rebind. | Separate rekey-only and `A-1/A/A+1` continuity matrices; graph name recreation; force missing authority. | FR-003, SEC-002, FM-006 |
| AC-005 | Every reported `D` survives restart with complete replayable bytes and exact lineage/generation; duplicate/gap/torn batches cannot advance progress. | Process abort at every bytes/sync/checkpoint/ACK boundary. | FR-004, NFR-001, FM-002 |
| AC-006 | Vector/graph/KV data, schema, IDs, allocation high-waters and CAS behavior match certified prefix; stale branch/incarnation writes reject without mutation. | Mixed fixture including graph/record deletes/recreate, KV CAS and allocator exhaustion boundaries. | FR-003, FR-004, SEC-002 |
| AC-007 | Paused or partitioned former writer cannot execute after new writer admission; no promotion without terminal peer fence proof. | Pause old process/VM, delay ARM and storage writes, attempt stale connections; correlate external fence and first new ACK. | FR-005, FM-003, FM-004 |
| AC-008 | Unknown/delayed renewal, old-owner renew after expiry, controller death, stale probes and lost ARM connectivity produce not-ready/blocked promotion, never two writers. | Deterministic fake-clock/response permutations plus approved host tests. | FR-005, FR-007, FM-003, FM-005 |
| AC-009 | Controlled switchover certifies final `S/A`, standby `D=S`, peer off and new ready service; every pre/post-intent crash recovers one selected source or remains fenced. | Restart at each transition, validate no acknowledged-write loss for this controlled case. | FR-006, FR-008 |
| AC-010 | Every successful mutation has a receipt included by monotonic authority CAS; after fence/closure, frozen `Uack` bounds acknowledged loss. `D=100/Uack=120` refuses promotion; `D>=Uack` next commit remains contiguous and survives reopen/export/follower apply. | Independent client ACK ledger; reorder receipts 101/102, force CAS retries/unknown replies/epoch closure; verify no pointer/policy regression or stale append. | FR-006, NFR-001, FM-007 |
| AC-011 | Compaction preserving `L` but advancing `G/F` forces required seed/consumer acknowledgements; no pinned deletion, stale-source export or reopening by cancellation after authority loss. | Interleaving tests at freeze/build/select/reopen and restored selection authority. | FR-008, FM-008 |
| AC-012 | `/ready` stays false until epoch, fence, selected DB, current `A`, bindings and required indexes/providers pass. TCP `/health`, route switch or stale probe never authorizes promotion. | Fake LB plus authenticated end-to-end canary. | FR-007, FR-005, FM-005 |
| AC-013 | Unsupported filesystem/features/gRPC/legacy clients cannot bypass HA admission; installation-admin authorization occurs before job lookup. | Route/mutation inventory, cross-tenant hidden-resource and CSRF tests. | SEC-001, SEC-002, CON-001 |
| AC-014 | Scoped identities cannot delete backups, start peer VMs, read unapproved secrets or write outside designated object prefixes; logs/UI contain no secret values. | Policy tests and credential-free audit inspection. | SEC-001, NFR-003 |
| AC-015 | Recorded workload run measures end-to-end recovery, durability and lag with source size/rate/features; failing rate/latency profile cannot be enabled. | Qualification report with client ACKs, durable checkpoints, timestamps and stop criteria. | NFR-002, NFR-001, CON-001 |
| AC-016 | Planning changes are confined to this artifact; protected deployment files retain specified SHA-256 values; no operations/implementation/release changes occur. | Final git status/diff and file hashes. | CON-002 |

| Failure ID | Failure class | Required safe outcome |
|------------|---------------|-----------------------|
| FM-001 | Process crash, partial upload, wrong hash, Blob unavailable, retention lock | No published incomplete backup; current service unaffected unless local staging quota demands backpressure; alert on age/capacity. |
| FM-002 | Replica disk full/torn write/reorder/source floor jump | `D` does not lie; stop apply; retain source pins or mark reseed required; promotion disabled. |
| FM-003 | Primary pause/partition/delayed renewal/unknown authority response | Local admission closes when observed; external fencing still mandatory before peer admission; no self-reported expiry proof. |
| FM-004 | Storage write in flight, ARM fencing unknown, old VM reappears | No successor until trusted off proof; preserve divergent former disk; boot into quarantine, never auto-start old writer. |
| FM-005 | Controller death, stale LB probe, stale routes, old open connections | Availability may fail; no ownership change from probe/routing; new owner only via durable promotion state. |
| FM-006 | Stale credentials/tenant state/secret version or auth store corruption | Not ready, deny old grants; isolated recovery/rekey only with approval. |
| FM-007 | Stale lag looks healthy but final source/ACK position unknown | No automatic or forced unsafe promotion; inspect receipt chain/final barrier or remain unavailable. |
| FM-008 | Compaction/restore selection ambiguity, cancel after authority loss | Never reopen predecessor or set HA writer-ready from local maintenance `OPEN`; reconcile authoritative selection fenced. |

**Proposed, unmeasured service targets**:

| Scenario | Initial acceptance target | Measurement / exclusions |
|----------|---------------------------|--------------------------|
| Backup freshness | Hourly schedule; verified cloud publication within 15 minutes of checkpoint; last successful checkpoint age at most 75 minutes in the qualified steady-state profile. | Manifest checkpoint time to verified publication; missed jobs alert. Not a guarantee during Blob/source outage. |
| Backup recovery | At most 60 minutes to authenticated/index-ready service from a verified <=1 GiB recovery set on a pre-provisioned supported recovery host. | Start restore authorization to successful canary. Provisioning, unavailable secrets/authority, human approvals and region outages reported separately. |
| Healthy replication | p95 durable observation lag at most 5 seconds and receiver sustained capacity at least 1.5x tested source rate. | Independent source/receiver sequence/time logs for the specified 100 commits/s, 1 MiB/s test input; stale observations marked unknown. Not failure RPO. |
| Controlled switchover | No loss of acknowledged writes at certified barrier; target at most 10 minutes unavailable. | Final old ACK/barrier through new authenticated write/read/CAS canary; includes fencing and index readiness. |
| Eligible automatic failover | Target at most 15 minutes for tested single-zone/host failure with available authority/ARM and eligible durable prefix/policy. | Failure injection to first successful ready-writer canary; split detection/fence/replay/index/routing phases. Not guaranteed if fencing or loss evidence is unavailable. |
| Uncontrolled async loss | Report exact recovered prefix and conservative `Uack-D` bound when certified; otherwise `unknown` and no promotion. | Compare independently captured client ACK ledger to recovered data. Never claim seconds-RPO or zero-loss availability from observed lag. |

## 8. Security Considerations

- **Data handling**: Snapshot payloads, credential/password hashes, tenant/grant maps and audit records are sensitive. Require server-side encryption, TLS, private endpoints where approved, narrow data-plane RBAC, and no secret-bearing public report artifacts. Customer-managed keys are optional only with separately tested key recovery/availability.
- **Input validation**: Versioned bounded manifests, exact object version IDs, relative generated names, no symlinks/path traversal, strict schema/feature compatibility and cryptographic hashes. Do not deserialize arbitrary admin directories or execute object-provided commands.
- **Access control**: Separate uploader, restore reader, retention janitor and authority/fencer identities. Backup upload cannot delete; application cannot control VM power; fencer cannot start VMs or alter unrelated resources. Installation admin—not tenant admin—controls topology/promotion/drills.
- **Secrets**: Managed identity replaces storage account keys in runtime. Key Vault/approved secret-store references require recovery policy and current versions; no keys/passwords in manifests, UI, logs or source. New TLS certificates/node identities are provisioned on rejoin.
- **Revocation and replay**: Bind policy and receipts to installation/epoch; replayed bundles/batches from an old epoch fail. Unknown latest policy is a readiness failure. Emergency recovery without authority invalidates imported credentials rather than trusting stale secrets.

## 9. Deployment & Rollback

| Gate | Required approval/evidence |
|------|----------------------------|
| GATE-001 Design/profile | Owner approves this design, source feature/workload inventory, rekey-only backup milestone, refusal of nonzero-loss promotion, metadata-ack latency and client incarnation contract; reconcile unpublished compaction dependency. |
| GATE-002 Host migration | Separate deployment plan/approval migrates ACA/SMB to supported VM/disk/ext4 profile; verify flushed durability, private networking, secret recovery and actual capacity. No migration is implicit in this PRD. |
| GATE-003 Backup usability | Complete recovery-set publication, rekey-only safe configuration recovery, retention guards and isolated drill pass before minimum backup is declared usable. Current-credential continuity is not claimed before EPIC-009/010 qualification. |
| GATE-004 Manual HA | Replica durable replay/performance, role readiness, external fence semantics and controlled switchover fault tests pass. Automatic promotion remains off. |
| GATE-005 Automatic HA | Receipt-before-success and loss-bound proofs cover every supported mutation, incarnation clients deployed, current `A` assured, fencing/partition/pause/soak tests pass; explicit owner enables policy. |
| GATE-006 Operations | Operators accept runbooks, alerts, retention costs, on-call ownership, blocked-promotion behavior and tested recovery/failback procedure. |

- Roll out code disabled. First validate backup in an isolated environment, then authorize source snapshots/uploads explicitly. Enabling cloud backup does not enable HA or compaction.
- Migrate hosts only after verified recoverability and a separately scheduled outage. Freeze old source, preserve it and its admin state, restore to new isolated host, validate before changing routing. Do not share the old SMB volume as both new nodes' database.
- Add standby in observe-only mode; measure sustained lag/catch-up. Exercise manual controlled switchover before enabling automatic fencing/promotion.
- Before selection/promotion intent, cancellation can clean unselected artifacts. After intent, reconcile forward or remain fenced; never route to the predecessor merely because the new generation failed to open.
- Roll back a binary only if it understands the selected format, access schema and ownership/selection authority. Otherwise keep a compatible recovery-capable binary and restore a verified set into isolation. An old binary ignoring HA state MUST NOT open the managed source.
- Disable automatic mode without deleting authority/receipts/pins. Old primary rejoin is reseed-only after quarantine and forensic preservation; no merge or automatic failback.

## 10. Resolved Decisions

These are recommended design decisions for approval, not authorization to execute.

| ID | Decision | Rationale |
|----|----------|-----------|
| RD-001 | Ship recovery backups before standby/automatic failover. | Existing snapshot/recovery primitives provide independent value; distributed safety has different qualification cost. |
| RD-002 | Recommend two zonal Linux VMs with independent managed ext4 disks; require explicit migration. | Fits current local-file/generation design and permits external VM fencing without pretending ACA/SMB is qualified. |
| RD-003 | Use Blob only for recovery objects and control metadata, not active DB storage. | Avoids approving object-tier architecture or changing storage semantics. |
| RD-004 | Lease/CAS serialize authority; external peer power-off proves fencing. | Blob lease scope does not include local disks, paused processes or queued writes. |
| RD-005 | Default automatic promotion off; v1 promotes only a certified complete acknowledged prefix (`D>=Uack`). | Stale observed lag cannot prove a failure RPO; lossy sequence rebasing is not supported by the current contiguous log. |
| RD-006 | Metadata acknowledgement receipts are required for unattended bounded-loss promotion. | Makes final successful-ACK upper bound independent of a failed source; exposes latency/availability trade-off. |
| RD-007 | Preserve one selection owner and whole-generation startup; add HA as a dominating owner gate. | Avoids stale caches, bypassing compaction authority and cancellation reopening an unauthorized writer. |
| RD-008 | Authoritative access generation and incarnation-aware conditional clients. | Prevents stale revocations, resource rebinding and CAS ABA across a lost branch. |
| RD-009 | Zone HA only; never geo-failover the authority witness. | Geo-redundancy is asynchronous and can resurrect stale control state. |
| RD-010 | No automatic deletion of compaction originals or general engine rewrite. | Keeps recovery invariants and implementation scope bounded. |
| RD-011 | First backup milestone is rekey-only; later continuity depends on current-policy authority. | Avoids either revocation rollback or a hidden dependency cycle in the independent backup release. |

## 11. Alternatives Considered

| Alternative | Pros | Cons | Decision |
|-------------|------|------|----------|
| ALT-001 Current ACA + Azure Files standby | Least deployment change | Unqualified power-loss/ownership semantics; replica scaling is not fencing | Rejected for HA; backup-only experimental use remains separately labeled. |
| ALT-002 VMs + independent disks | Direct ownership, clear peer power fencing, small control surface | OS patching/on-call, ARM latency, required migration | Recommended after qualification. |
| ALT-003 AKS + zonal PVCs | Fits an existing Kubernetes operating model | Cluster/CSI/node fencing complexity and cost; Lease/eviction insufficient | Conditional alternative if already operated; estimate must be revised. |
| ALT-004 Lease blob and health-based route flip alone | Small amount of code | Does not stop paused/local writer; stale lag/health misleading | Rejected. |
| ALT-005 Manual safe switchover only | Lower complexity; exact drained barrier | Does not satisfy unattended recovery when primary unavailable | Independent milestone/fallback if metadata latency or fencing qualification rejected. |
| ALT-006 Synchronous replica consensus | Stronger acknowledged-write availability/loss semantics | New data quorum/consensus architecture, latency and scope | Deferred; request does not approve it. |
| ALT-007 Async promotion from last observed lag | Fast, no external metadata write latency | Cannot establish final acknowledged loss after partition | Rejected. |
| ALT-008 ACK receipts plus conservative fail-closed promotion | Independent acknowledged-loss upper bound, async data path | External metadata dependency and client compatibility work; not always available | Selected for qualified automatic mode, subject to approval. |
| ALT-009 Snapshot-only with arbitrary PITR claim | Simple marketing | Snapshots lack intervening history | Rejected; bounded snapshot recovery points only. |
| ALT-010 Lossy promotion with allocator bumped above missing suffix | Could accept an outdated replica | Existing replay requires contiguous sequences; append-only advance markers do not repair a mid-log gap | Deferred; v1 rejects `D<Uack` instead of adding a new branch/checkpoint format. |

## 12. Files

**Files Affected** below are proposed future implementation changes, not files changed by planning. Paths marked **new** are deliberate proposed modules. Each epic limits its owned working set; shared owner/lifecycle files must be serialized between implementers.

- **FILE-001**: `src/server/ops.rs`, `src/server/admin.rs`, `src/maintenance.rs` — reuse snapshots/verification/schedule/selection.
- **FILE-002**: **new** `src/server/recovery_bundle.rs`, `src/server/cloud_backup.rs`, `src/server/backup_policy.rs` — typed configuration capture, Blob publication and durable policy/catalog/pins.
- **FILE-003**: `src/server/credentials.rs`, `src/server/users.rs`, `src/server/tenants.rs`, **new** `src/server/access_policy.rs` — current access generation, safe restore, revocations and bindings.
- **FILE-004**: `src/replica.rs`, `src/logops.rs`, `src/lib.rs`, `src/server/features.rs`, **new** `src/server/replica_agent.rs` — existing primitives plus network envelope/checkpoint/source pin lifecycle.
- **FILE-005**: **new** `src/server/ha.rs`, `src/server/authority.rs`, `src/server/fencing.rs`, `src/server/ack_receipts.rs` — role/epoch ownership, external proof and receipt policy.
- **FILE-006**: `src/server.rs`, `src/server/group_commit.rs`, `src/bin/vlite-server.rs`, `src/server/compaction.rs` — dominating admission gate, receipts, boot/readiness, generation selection and cancellation safety.
- **FILE-007**: `src/dashboard/admin-tools.js`, `src/dashboard/console-tools.js`, `src/dashboard/app.js`, `src/dashboard/index.html` — existing backup/replication administration surfaces.
- **FILE-008**: `src/openapi.json`, `docs/API.md`, `docs/CONSOLE-ADMIN.md`, `docs/STORAGE-FEATURES.md`, `docs/DOCKER-OPERATIONS.md`, `docs/SECURITY.md` — precise feature/profile contracts and runbooks.
- **FILE-009**: **new** `deploy/azure-vm-ha/README.md`, `deploy/azure-vm-ha/main.bicep`, `deploy/azure-vm-ha/cloud-init.yaml`, `deploy/azure-vm-ha/rbac.bicep` — separately approved topology and bootstrap artifacts; never silently repurpose the dirty ACA files.
- **FILE-010**: `Cargo.toml`, `Cargo.lock` — approved SDK dependencies/features only; no release version bump.
- **FILE-011**: **new** `tests/cloud_backup.rs`, `tests/recovery_bundle.rs`, `tests/replica_agent.rs`, `tests/server_ha.rs`, `tests/ha_faults.py`, `tests/ha_qualification.py`; existing `tests/server_compaction.rs`, dashboard/ops/replication tests — deterministic and bounded host tests.
- **FILE-012**: `docs/projects/feature-backup-ha/feature-backup-ha.prd.md` — sole planning output; no changes to completed compaction PRD/code or pre-existing dirty deployment files.

## 13. Simplicity Rationale

- **Scope justification**: Every epic below maps to requirements. Hosting/profile, receipt and fault-qualification work are enabling work that cannot be hidden inside a Blob upload task: they establish different safety proofs and approval gates. The manual milestone deliberately precedes automatic failover.
- **Abstractions check**: New narrow interfaces are `BackupObjectStore` (production Blob plus deterministic fault fake), `AuthorityStore` (conditional owner/policy/receipt operations), and `FenceProvider` (qualified Azure agent plus fault fake). They are not a general cloud/provider framework. One managed selection owner and one typed access bundle avoid independent competing implementations.
- **Configuration check**: The settings in section 3.7 exist for explicit storage protection, bounded resource use, topology identity or loss approval. No general cron engine, plugin system, arbitrary command runner, storage-provider matrix or unsafe override is needed.
- **Could this be simpler?** Copying a `.vlite` file to Blob and starting another replica on missed health checks would be shorter, but would omit access state, crash publication, current revocations, selection authority, actual durable lag and protection from a paused old writer. Reusing the existing local backup/follower/generation machinery while qualifying one topology is the smallest credible route. If the owner accepts manual recovery instead of automatic failover, the receipt/automatic epics can be omitted explicitly rather than weakening the safety contract.

## 14. Implementation Plan

All tasks are **Not Started** and blocked on GATE-001 until separate implementation approval. Estimates include coding, focused tests and review, not production authority. Task descriptions name prerequisites and observable completion; no task authorizes a live operation.

### EPIC-001: Profile and safety contracts — 4–6 person-days

Requirements: NFR-002, CON-001, FR-005. Dependencies: design approval. Deliverable: executable envelope and ADR contracts; no infrastructure deployment.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-001 | Inventory source size/rate/features, all supported mutation/ACK paths, source/admin/secret state and current client CAS behavior. Define schema/compatibility contracts for recovery set, stream identity, epoch, policy generation and incarnation; unknown features fail profile certification. Validate AC-013/015 with fixtures. | Not Started | `docs/projects/feature-backup-ha/feature-backup-ha.prd.md`, `src/server.rs`, `src/server/admin.rs`, `src/replica.rs`, `tests/server_ha.rs` |
| ITEM-002 | Define authority/fence state model, external assumptions and automatic loss policy; reproduce delayed lease/unknown response scenarios with a deterministic fake before production adapter work. Review latency/client trade-offs and record owner approval. | Not Started | `src/server/ha.rs`, `tests/server_ha.rs`, `docs/DOCKER-OPERATIONS.md` |

### EPIC-002: Typed recovery/access bundle — 6–10 person-days

Requirements: FR-001, FR-003, SEC-002. Dependencies: EPIC-001.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-003 | Add bounded versioned recovery-set/admin bundle schema, safe configuration/secret-reference inventory and exact DB/policy identity binding. Exclude sessions/locks/paths, refuse unknown security state, and serialize capture with full local admin/data persistence barrier and invariant checks. Add the policy-publisher hook contract without implementing online authority yet. Verify AC-003/004/006. | Not Started | `src/server/recovery_bundle.rs`, `src/server/admin.rs`, `src/server/credentials.rs`, `src/server/users.rs`, `src/server/tenants.rs`, `src/server.rs`, `tests/recovery_bundle.rs` |
| ITEM-004 | Implement read-only bundle verification and isolated **rekey-only** import: new restore lineage, imported accounts/credentials disabled, fresh private break-glass bootstrap and explicit regrants. Mark continuity unavailable until EPIC-009/010; prove old revocations cannot be undone. | Not Started | `src/server/recovery_bundle.rs`, `src/server/credentials.rs`, `src/server/users.rs`, `src/server/tenants.rs`, `tests/recovery_bundle.rs` |

### EPIC-003: Blob publication — 5–8 person-days

Requirements: FR-001, SEC-001. Dependencies: EPIC-002.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-005 | Add managed-identity Blob client with bounded uploads, immutable object/version identities, hashes/read-back, manifest-last creation, per-set publication CAS and unknown-result reconciliation. Only a committed PUBLISHED record references a recoverable manifest; stale staging owners cannot publish. Verify AC-001/014. | Not Started | `src/server/cloud_backup.rs`, `src/server/recovery_bundle.rs`, `Cargo.toml`, `Cargo.lock`, `tests/cloud_backup.rs` |

### EPIC-004: Scheduled cloud policy and retention — 4–7 person-days

Requirements: FR-002, NFR-003. Dependencies: EPIC-003.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-006 | Extend existing local backup scheduling/submission rather than adding a second scheduler. Persist policy/jobs, serialize per-set pin versus DELETE_PENDING CAS and cross-set minimum-survivor catalog decisions, retain terminal tombstones, and safely reconcile crashed owners. Separate staged-orphan cleanup from deletion; no pin timeout may delete a live consumer's data. Verify AC-001/002. | Not Started | `src/server/ops.rs`, `src/server/backup_policy.rs`, `src/server/cloud_backup.rs`, `src/server/admin.rs`, `tests/cloud_backup.rs`, `tests/server_ops.rs` |

### EPIC-005: Isolated recovery and drills — 5–8 person-days

Requirements: FR-002, FR-003, FR-008. Dependencies: EPIC-004.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-007 | Own the shared typed selection-transaction foundation consumed by restore/reseed/promotion; extend current pinned-compaction authority rather than deleting it or using competing markers. Implement isolated download/verify/open/index/rekey-only drill and normalized selected identity, forbid fallback after intent, and expose explicit identity-preserving seed versus lineage-changing restore modes. Verify AC-003/004/006 and pre/post-intent cancellation. | Not Started | `src/server/recovery_bundle.rs`, `src/server/admin.rs`, `src/server/compaction.rs`, `tests/recovery_bundle.rs`, `tests/server_compaction.rs`, `docs/DOCKER-OPERATIONS.md` |

### EPIC-006: Host migration package and qualification harness — 5–8 person-days

Requirements: CON-001, SEC-001, NFR-002. Dependencies: EPIC-001; actual migration additionally requires EPIC-015 and separate GATE-002 approval.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-008 | Produce two-zonal-VM/private-network/independent-disk topology, least-privilege managed identities, separate backup/control storage, fail-closed boot and peer powerOff-only permissions. Validate templates offline; execute disposable-host disk/flush/lock tests only with explicit infrastructure approval. Do not edit the existing dirty ACA artifacts. | Not Started | `deploy/azure-vm-ha/README.md`, `deploy/azure-vm-ha/main.bicep`, `deploy/azure-vm-ha/cloud-init.yaml`, `deploy/azure-vm-ha/rbac.bicep`, `tests/ha_qualification.py`, `docs/DOCKER-OPERATIONS.md` |

### EPIC-007: Private replication agent — 6–10 person-days

Requirements: FR-004, NFR-001, SEC-001. Dependencies: EPIC-002 and EPIC-001 stream contract.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-009 | Wrap existing export/follower primitives with private authenticated envelope, complete bounded batches, source pins, durable bytes-before-checkpoint-before-ACK ordering, seed/prefix/header lineage proof and restart reconciliation. Deduplicate above non-idempotent follower apply; reject unsupported memory/export profiles without loosening tenant guards. Verify AC-005. | Not Started | `src/replica.rs`, `src/logops.rs`, `src/server/features.rs`, `src/server/replica_agent.rs`, `src/server/backup_policy.rs`, `tests/replica_agent.rs`, `tests/server_ha.rs` |

### EPIC-008: Replay, index readiness and capacity — 7–10 person-days

Requirements: FR-004, FR-007, NFR-002. Dependencies: EPIC-007.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-010 | Batch/coalesce existing full replay on an unserved standby, separate received/durable/applied/index-ready positions, and implement bounded reseed restart. Test mixed graph/KV/vector/CAS and index restoration; measure receiver headroom/memory at specified source sizes/rates. If a general incremental replay rewrite is needed, stop/re-estimate instead of silently expanding scope. | Not Started | `src/replica.rs`, `src/lib.rs`, `src/server/replica_agent.rs`, `src/server/observability.rs`, `tests/replica_agent.rs`, `tests/ha_qualification.py`, `docs/STORAGE-FEATURES.md` |

### EPIC-009: Current access-policy authority — 5–8 person-days

Requirements: FR-003, SEC-002. Dependencies: EPIC-002 schema/hooks; EPIC-010 authority adapter for completion. It may develop against the EPIC-001 interface fake in parallel; this is not a dependency of the rekey-only backup milestone.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-011 | Publish immutable policy bundles and monotonic authoritative generation before successful admin changes; atomically install current policy on source/standby, validate exact resource bindings and secret versions, invalidate sessions on promotion, and fail closed on ambiguity. Verify AC-004/014, including tenant disable/delete and credential rotation during replication lag. | Not Started | `src/server/access_policy.rs`, `src/server/authority.rs`, `src/server/credentials.rs`, `src/server/users.rs`, `src/server/tenants.rs`, `tests/recovery_bundle.rs`, `tests/server_ha.rs` |

### EPIC-010: Authority and external fence controller — 7–10 person-days

Requirements: FR-005, SEC-001. Dependencies: EPIC-001, EPIC-006 qualified platform for live evidence.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-012 | Implement lease+conditional-record epoch state, conservative monotonic deadlines, unknown-response reconciliation, durable promotion intents and Azure peer powerOff adapter. Bind terminal proof to exact owner/VM/operation; forbid automatic start. Verify AC-007/008 with deterministic fakes then approved real agent/platform. | Not Started | `src/server/authority.rs`, `src/server/fencing.rs`, `src/server/ha.rs`, `tests/server_ha.rs`, `tests/ha_faults.py`, `deploy/azure-vm-ha/README.md` |

### EPIC-011: Manual safe switchover and incarnation gate — 6–10 person-days

Requirements: FR-005, FR-006, FR-008, SEC-002. Dependencies: EPIC-005 shared selection foundation and EPIC-008/009/010; no automatic promotion.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-013 | Add dominating role/owner admission at service/queues/background paths, fail-closed boot, exact drained `S/A` switchover, fresh generation selection and incarnation-aware conditional mutation contract. Retired/returning primary cannot reenter service; validate high-waters/bindings and stale-CAS rejection. Verify AC-006/007/009/013. | Not Started | `src/server/ha.rs`, `src/server.rs`, `src/server/group_commit.rs`, `src/bin/vlite-server.rs`, `src/server/admin.rs`, `tests/server_ha.rs`, `docs/API.md` |

### EPIC-012: Routing and dashboard administration — 4–7 person-days

Requirements: FR-007, SEC-001. Dependencies: EPIC-004 APIs for backup slice; EPIC-011 for HA slice.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-014 | Extend existing backup/replication controls and admin-only status/actions; show durable progress/freshness, policy/selection/epoch, fencing phase and disabled reasons. Add readiness-qualified stable routing and sanitized job results; stale probes do not authorize actions. Verify AC-012/013/014 and UI tests. | Not Started | `src/dashboard/admin-tools.js`, `src/dashboard/console-tools.js`, `src/dashboard/index.html`, `src/server/ha.rs`, `src/openapi.json`, `tests/admin_tools.test.cjs`, `docs/CONSOLE-ADMIN.md` |

### EPIC-013: Acknowledgement receipts and qualified automation — 7–10 person-days

Requirements: FR-006, NFR-001. Dependencies: EPIC-009/010/011.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-015 | Gate every supported successful mutation response on source durability and immutable receipt inclusion by monotonic authority-record CAS; CAS retries preserve newer receipt/policy/selection fields. Serialize epoch closure on the same record; reconcile unknown writes, require exact-source `D>=Uack`, keep contiguous sequences and refuse lossy promotion. Measure metadata latency and verify reordered receipts, stale appends, AC-010 and complete mutation coverage. | Not Started | `src/server/ack_receipts.rs`, `src/server/authority.rs`, `src/server/ha.rs`, `src/server.rs`, `src/server/group_commit.rs`, `tests/server_ha.rs`, `tests/ha_faults.py` |

### EPIC-014: Compaction, selection and source-pin integration — 5–8 person-days

Requirements: FR-008, FR-004. Dependencies: EPIC-005/007/011; no completed compaction feature redesign.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-016 | Consume ITEM-007's selection transaction; integrate compaction-only floor/consumer/source pins, suspend promotion across floor transitions, require seed acknowledgement, and make authority loss dominate pre/post-selection cancellation/recovery. Preserve graph revision rules and credential context. Do not reimplement the shared transaction or create a reverse dependency. Verify AC-011 at every compaction crash point. | Not Started | `src/server/compaction.rs`, `src/server/ha.rs`, `src/server/replica_agent.rs`, `src/server/backup_policy.rs`, `src/server/admin.rs`, `tests/server_compaction.rs`, `tests/server_ha.rs` |

### EPIC-015: Backup release qualification — 4–6 person-days

Requirements: FR-001/002/003, NFR-002, SEC-001. Dependencies: EPIC-005.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-017 | Run recovery-set failure/retention/rekey-only auth/drill matrix in disposable local and separately approved Azure environment; record exact profile/version, time/bytes, source caveats and runbooks. Ship minimum usable backup only after GATE-003; no current-credential-continuity claim or automatic source enablement. Verify AC-001/002/003, rekey-only AC-004, AC-014/015. | Not Started | `tests/cloud_backup.rs`, `tests/recovery_bundle.rs`, `tests/ha_qualification.py`, `docs/DOCKER-OPERATIONS.md`, `docs/SECURITY.md`, `docs/STORAGE-FEATURES.md` |

### EPIC-016: HA fault/soak qualification and operations — 8–10 person-days

Requirements: FR-005/006/007/008, NFR-002/003. Dependencies: EPIC-006 through EPIC-014; manual subset may run after EPIC-011/014.

| Task | Description | Status | Relevant Files |
|------|-------------|--------|----------------|
| ITEM-018 | Run pause/partition/lease ambiguity/ARM failure/in-flight write/owner-return/old-token/CAS/compaction matrix, independent ACK-ledger comparison, bounded workload sweep and two-week observation. Record unsafe or unavailable outcomes, promotion evidence and measured phase latencies; failures keep gate closed. | Not Started | `tests/ha_faults.py`, `tests/ha_qualification.py`, `tests/server_ha.rs`, `tests/replica_agent.rs`, `docs/DOCKER-OPERATIONS.md`, `deploy/azure-vm-ha/README.md` |
| ITEM-019 | Complete operator runbooks, backup age/lag/policy/fence alerts, quarantine/reseed and rollback drills; obtain GATE-004/005/006 approvals separately. Report active risks and do not enable automation merely because tests pass. | Not Started | `docs/DOCKER-OPERATIONS.md`, `docs/SECURITY.md`, `docs/CONSOLE-ADMIN.md`, `deploy/azure-vm-ha/README.md` |

### 14.1 Engineering estimates and critical path

One person-day is approximately eight focused engineering hours, including focused testing/review. These are bottom-up planning ranges, not bids; workload/profile and fence semantics are not yet measured.

| Epic | Person-days | Main uncertainty |
|------|-------------|------------------|
| 001 Profile/contracts | 4–6 | Unsupported source/client combinations |
| 002 Recovery/access bundle | 6–10 | Sidecar/secret/identity completeness |
| 003 Blob publication | 5–8 | SDK conditional/version/unknown-result behavior |
| 004 Schedule/policy/retention | 4–7 | Existing policy persistence and pin integration |
| 005 Restore/drills/selection | 5–8 | Compaction-authority transition and current policy |
| 006 VM migration package | 5–8 | Host/RBAC/flush qualification; approvals excluded |
| 007 Replication transport/progress | 6–10 | Durable checkpoint and complete commit framing |
| 008 Replay/index/capacity | 7–10 | Whole-file replay throughput and memory |
| 009 Current access policy | 5–8 | All mutation/revocation paths and secret versions |
| 010 Authority/fencing | 7–10 | External terminal-proof semantics |
| 011 Manual switchover/incarnation | 6–10 | Mutation coverage and client compatibility |
| 012 Dashboard/routing | 4–7 | Existing UI/API integration and readiness |
| 013 ACK receipts/automation | 7–10 | Coverage, latency, unknown publication results |
| 014 Compaction integration | 5–8 | Floor/selection/cancel interleavings |
| 015 Backup qualification | 4–6 | Recovery/auth/retention evidence |
| 016 HA qualification/runbooks | 8–10 | Fault reproducibility and fixes |
| **Base total** | **88–136** | Includes tests/review; no hidden cloud migration |
| **Contingency** | **22–48** | 25% of low / 35% of high, rounded |
| **Planning total** | **110–184 person-days** | Re-estimate if profile or safety proof changes |

| Milestone | Cumulative base effort | Delivery assumptions |
|-----------|------------------------|----------------------|
| Minimum usable off-host backup | 28–45 days: 001–005 + 015 | CLI/admin API, persisted schedule, safe disabled access inventory and **rekey-only** isolated drill; current-credential continuity and dashboard enhancements follow. About 35–61 days with contingency. |
| Manual safe switchover | 76–119 days: all except 013 and automatic part of 016 | EPIC-016 decomposes into 3 days for manual qualification/runbooks and 5–7 days for automatic fault/soak work. Includes backups, qualified hosts, current policy, external fencing and incarnation contract. About 95–161 days with contingency. |
| Qualified automatic failover | 88–136 days base; 110–184 with contingency | Adds receipts/automation and full fault/soak gate; not guaranteed available for every failure. |

- **One engineer**: roughly 22–37 working weeks for full effort; plan **24–40 elapsed weeks** allowing bounded qualification/soak and integration, assuming approvals/test infrastructure arrive promptly. Backup alone roughly 7–12 working weeks plus approval/calendar gaps.
- **Bounded three-engineer team, agent-assisted**: recovery/storage engineer, replication/runtime engineer, and platform/test engineer; **12–18 elapsed weeks** for full proposal under prompt approvals. Backup milestone roughly 4–7 elapsed weeks. Shared `server.rs`/selection/auth files have explicit single-owner integration windows; this is not three independent features.
- AI workers may help adapters, fixtures, UI and documentation. They do not divide engineering uncertainty by worker count, prove Azure fencing semantics, supply production approval, or shorten a two-week soak.
- Critical path: profile/contracts → recovery/access model and qualified host/fence model → durable follower/current policy → managed manual switchover/compaction integration → receipts/automation → full fault/soak/operational approval. Backup publication/retention, platform preparation and replication transport can overlap after contracts.
- The ranges exclude waiting for Azure quota/security/procurement, major client-library rewrites, general incremental replication/consensus, region HA, sources above the initial envelope and repairing unqualified storage formats. Those trigger a new estimate, not silent contingency consumption.

### 14.2 Azure evidence and illustrative operating budget

Official evidence supplied and verified by the parent on 2026-11-13:

| ID | Source | Design consequence |
|----|--------|--------------------|
| AZ-001 | [Lease Blob REST](https://learn.microsoft.com/en-us/rest/api/storageservices/lease-blob) | Lease constrains writes/deletes of that blob only; finite lease 15–60 seconds; expiry does not revoke local disk writes. An expired lease can be renewed without intervening acquisition/modification; acquisition after expiry can take an additional minute. Lease operations do not advance ETag. |
| AZ-002 | [Azure VM power fencing pattern](https://learn.microsoft.com/en-us/azure/sap/workloads/high-availability-guide-rhel-pacemaker?tabs=msi#create-a-fencing-device) | Managed-identity external power fencing with least-privilege VM actions and ARM connectivity dependency. Sample `power_timeout=240` / `stonith-timeout=900` does not promise seconds-RTO or qualify VLite/Ubuntu/Pacemaker automatically. |
| AZ-003 | [Azure Storage redundancy](https://learn.microsoft.com/en-us/azure/storage/common/storage-redundancy) | ZRS synchronously replicates across zones; GRS/GZRS cross-region copy is async. No automatic witness geo-failover or region-RPO=0 claim. |
| AZ-004 | [Azure Retail Prices API](https://prices.azure.com/api/retail/prices) | Public PAYG USD East US illustration, not subscription pricing, validated sizing or a quote. |

| Illustrative resource | Public unit price supplied by parent | Monthly illustration |
|-----------------------|--------------------------------------|----------------------|
| Two Linux Standard_D2as_v5 (2 vCPU/8 GiB each), non-Spot | $0.086/hour each, 730 hours | $125.56 |
| Two P4 LRS 32 GiB OS disks | $5.2795/month each | $10.559 |
| Two P6 LRS 64 GiB data disks | $10.207/month each | $20.414 |
| Standard Load Balancer included rules meter | $0.025/hour | $18.25 |
| 100 GB **retained** General Block Blob v2 Hot ZRS | $0.026/GB-month | $2.60 |
| **Illustrative fixed subtotal** | Rounded | **$177.38/month** |
| Blob writes / reads | $0.0625 / $0.004 per 10,000 | Usage additional |

- **Small full HA planning allowance: $220–350/month**, only if this two-VM topology, low traffic and small workload are adequate. Not an all-in quote; authority receipt transactions may materially increase with batch/rate and must be modeled.
- Excluded from the fixed subtotal: IP/outbound connectivity, private endpoints, controller resource overhead, authority-account transactions/storage, Key Vault, monitoring/log retention, drills, egress/cross-zone/cross-region traffic, taxes and support. Controller agents run on the two proposed VMs; a third VM or L7 gateway changes the budget.
- **Backup-only incremental allowance: $10–40/month**, assuming at most 100 GB retained, light jobs/drills and no always-on drill VM. Hourly full snapshots at the 1 GiB profile retained for 30 days would exceed 100 GB; retention schedule/size must be used to calculate real retained bytes. Do not confuse active DB size with retained backup volume.
- P6 capacity/IOPS and 8 GiB RAM are unvalidated; P10/Premium-v2 or larger VMs may be needed. Production sizing follows replay/index tests, not this price illustration.
- Existing ACA costs are not included in the full HA estimate. Temporary migration overlap incurs both old and new costs. Separate-region backup copying is optional and separately billed.
- Receipt transaction estimator: at least **two writes per acknowledged batch** (immutable receipt body + authority-record CAS), so billable writes approximately 2 × acknowledged batches/second × 2,628,000 seconds/month, plus retries, lease/read/policy/catalog operations. One batch/second is about 5.26 million writes/month before extras. Conditional retries may create extra orphan receipt bodies. Batching and control-account tariff require approval; do not blindly apply the backup tariff to another account. At the quoted Hot-ZRS write tariff this alone would illustrate about $32.85/month at one batch/second, or $328.50 at ten; the $220–350 allowance is therefore **low-traffic only**, not the 100-commit/s qualification workload budget.

### 14.3 Independent research and planning review

- Planner workflow: `octane-planner-plan`, with configured artifact root `./docs/projects/`. `code-search` was attempted first but its MCP connection required authentication; local read-only Git/source inspection was used for unpublished compaction integration.
- One canonical worker-pool **report-only** baseline research task: plan `5b44b03d5e384ed5a871f2c5694cda33`, task `b7731f6d6914400d8891332a607c9138`, worker `cpc-mahpa-46psc`, pinned pushed base `25003dd9dace64f5829a4cf2afad23ab8d78edcc`. **Succeeded**, one attempt, zero repairs, no integration requested. Report commit `eb936afa4202affd9f3e1aec91ca1fb8f2df5d81` adds only its 430-line report at `reports/5b44b03d5e384ed5a871f2c5694cda33/b7731f6d6914400d8891332a607c9138.md`; no feature/main/source push or unpublished snapshot. Static/artifact checks and JavaScript syntax passed, not runtime recovery/failover tests.
- Worker outcome reference: `worker-artifacts://cpc-mahpa-46psc/b7731f6d6914400d8891332a607c9138-a1/outcome.json`; report branch `copilot/peer/devboxpool/pinned-baseline-backup-replication-and-r-b7731f6d6914400d8891332a607c9138-a1` was fetched without checkout/merge. No active pool work remains.
- Independent local reviewer: `backup-ha-plan-review`, custom `deep-review:skeptic`; review ID **SKEPTIC-BACKUP-HA-01**, initial reviewed SHA-256 `ECD667831610EC8D28C478D5B3A3E706D2351E97FE1E150E8D735951F2DBB252`. Local review was necessary because the PRD and compaction integration are unpublished; no workspace snapshot was sent remotely.

| Review finding | Resolution |
|----------------|------------|
| High: lossy promotion would violate contiguous replay sequence | V1 prohibits `D<Uack` promotion. Preserve `D+1`; defer branch/checkpoint rebasing. Added explicit 100/120 refusal/reopen/export test. |
| High: backup milestone lacks current-policy authority | Explicit rekey-only first milestone; current-credential continuity depends on EPIC-009/010. Separate schema/capture from online publisher; no dependency cycle. |
| Medium: shared selection ownership/dependencies ambiguous | ITEM-007 owns foundation; EPIC-011 explicitly depends on EPIC-005; EPIC-014 only compaction/floor integration. |
| High: receipt pointer can regress under reordered CAS retries | Define immutable body plus monotonic merged authority CAS as ACK linearization point, same-record epoch closure, inclusion reconciliation and reordered-receipt tests. |
| High: paused publication/pin races GC | Per-set leased CAS publication/pin/tombstone state and serialized cross-set survivor decision; orphan manifests not catalog entries; no unsafe pin expiry. |
| Nonblocking: wrong replication paths, implicit milestone arithmetic and operation budget | Correct to `replica.rs`/`logops.rs`/`features.rs`/`lib.rs`; explicitly decompose final qualification; count two receipt writes and state low-traffic budget limit. |

**Closure review: SKEPTIC-BACKUP-HA-02**, reviewer `backup-ha-review-closure` (`deep-review:skeptic`), **PASS** on 2026-11-13. Artifact-only targeted review verified all five fixes, dependency ownership, corrected paths and estimate/cost arithmetic; no remaining executable design blocker was identified within that scope. This is planning review, not implementation/fencing qualification. Source/HA tests were not run as part of this design-only task.

Final planning validation checked the mandatory 15 sections, 16 unique epics, 19 unique items, estimate arithmetic, source-path corrections and workspace guardrails. No active workers remain. The only authored worktree file is this PRD; the three pre-existing dirty ACA deployment files remain untouched, unstaged and uncommitted. Neither local HEAD nor `origin/main` changed.

## 15. Change Log

- 2026-11-13: Initial design proposal prepared; no product version/changelog change, implementation or deployment authorization.
