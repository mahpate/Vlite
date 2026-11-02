# Scale-out after P2: branching, object storage, replication

| Field | Value |
|---|---|
| Status | Design recommendation; order adopted 2026-11-02 and backlogged (unscheduled) until the owner revisits it. No new interfaces approved |
| Decision | Gap 11 first, Gap 10 second, Gap 9 third (adopted 2026-11-02) |
| Sources | [Next major](next-major.md), [product gaps](../product-gaps/product-gaps.md), [P2 directory store](p2-directory-store.md) |
| Boundary | Embedded-first, one writer per database; no sharding, distributed transactions or automatic multi-writer operation |
| Validation | Repository evidence reviewed; acceptance below is prospective, not test results or performance claims |

## Constraints and existing evidence

References are repository-relative `file:line` locations at checkout base `c92c8c0`.
They establish current behavior, not that the proposed feature already exists.

| Constraint / building block | Evidence | Consequence |
|---|---|---|
| Sequencing is unresolved | `docs/projects/roadmap/next-major.md:24-46`; `docs/projects/product-gaps/product-gaps.md:54-56,120-121` | Recommend an order without silently changing the approved roadmap |
| P2 has immutable images, sealed WALs and a checksummed manifest | `docs/projects/roadmap/p2-directory-store.md:29-49`; `src/directory.rs:79-139` | Reuse file framing and manifest validation; do not add a second local truth |
| Publication is staged and synced | `src/directory.rs:597-646,705-751,763-807` | Keep old files if manifest directory sync fails; visibility is not a durable acknowledgement |
| Local locking is not distributed fencing | `docs/STORAGE-FEATURES.md:9-16`; `src/directory.rs:152-170` | Separate directories or hosts can both write; a local lock cannot arbitrate leadership |
| Checkpoints preserve record revisions and lineage, not all history | `tests/directory_store.rs:84-161`; `src/changes.rs:240-255` | Preserve CAS; below-floor consumers must bootstrap, not receive invented events |
| Converters are validated copies with constraints | `src/maintenance.rs:204-247`; `docs/projects/roadmap/p2-directory-store.md:165-182` | Keep source untouched; v5 only, no directory commit timestamps |
| Backup preserves lineage; restore creates a new lineage | `src/maintenance.rs:249-323`; `tests/pitr.rs:50-81` | Bootstrap a replica with backup, isolate an experiment with a new identity |
| PITR requires a matching exact byte prefix | `src/logops.rs:88-156`; `tests/pitr.rs:133-218` | Retaining snapshots alone does not promise arbitrary time travel |
| File-shipped replication already validates continuity | `src/replica.rs:143-160`; `tests/replica.rs:96-164` | Retain whole commits, lineage/header checks and explicit divergence |
| Apply syncs, reloads and rolls back on failure | `src/lib.rs:2365-2423`; `tests/replica.rs:199-226` | Current apply is not incremental; crash recovery cannot be presented as transport HA |
| Promotion has no fencing protocol | `src/replica.rs:194-199` | Network failover requires more than calling `promote()` |
| Graph/vector commits and maintenance are integrated | `docs/STORAGE-FEATURES.md:178-238`; `tests/graph_maintenance.rs:495-567` | Fork, publish and replicate graph batches atomically, including attachments and allocation marks |
| Change feeds keep commit boundaries | `tests/change_feed.rs:103-149`; `tests/graph_change_feed.rs:1-8,44-90` | Replicate log bytes, not a reconstructed logical feed |
| TTL is visibility plus committed reclamation | `docs/STORAGE-FEATURES.md:195-209`; `tests/directory_recovery_matrix.rs:196-263` | Preserve deadlines and cascade; remote readers never become sweepers |
| Encrypted bodies authenticate salt and frame prefix | `src/crypto.rs:317-340`; `docs/STORAGE-FEATURES.md:34-53` | Do not patch authenticated frame fields or expose plaintext caches |
| Tenants are policy inside one file | `docs/SECURITY.md:173-196` | A whole-database copy includes other tenants; scoped read credentials cannot export it |
| Transport security includes authorization | `docs/SECURITY.md:65-100` | TLS alone does not authorize replication, publication or branch creation |
| Azure Files remains experimental | `deploy/azure-container-apps/README.md:17-32,101-116` | Do not treat SMB sharing or Container Apps replicas as this design's HA substrate |
| Compact framing is guarded; production source codecs are pending | `docs/projects/roadmap/compact-storage-contract.md:5-7,49-99` | Preserve mandatory source blocks; a mapping is not lazy columnar serving |

## Options and decision

| First item | Agent-memory value | Dependencies and reuse | Main risk |
|---|---|---|---|
| Gap 11: local forks | Isolated consolidation, prompt experiments, destructive tests and recovery rehearsals | P2 snapshot boundary, immutable image, private WAL, converters and validation | Identity, encrypted copies and filesystem COW portability |
| Gap 10: object tier | Durable cold snapshots, portable read-only memory generations | P2 files plus publication catalog, provider CAS, cache and retention | Upload/publication races, authorization, keys and storage bills |
| Gap 9: network replication | Read followers and recovery on a second host | Existing segments plus transport, retention, durable progress and fencing authority | Lost acknowledged writes or two writers after a partition |

**Recommend Gap 11 -> Gap 10 -> Gap 9.** Start with bounded local branching,
not SQL/Git-style merge. It directly enables safe agent-memory experiments
without a cloud account, transport service or consensus dependency.
P2 supplies the committed boundary, revision-preserving image and private WAL.
The same identity/publication tests then support object-generation snapshots,
which can later bootstrap remote followers.

This is dependency leverage, not a claim that network replication needs either
branching or object storage: existing backup plus segment shipping is sufficient
for its bootstrap. Its lower position reflects the additional failover contract.
If a deployment requires HA before experiments or cold storage, move Gap 9
ahead explicitly and fund fencing; never rebrand asynchronous copying as HA.

Gap 11's cheap-copy goal is conditional: filesystem reflinks may share extents,
but ordinary copies remain ordinary copies. Encrypted forks initially require
independent cipher domains and a full rewrite. Neither case justifies a size,
latency, availability or cloud-cost claim. P2's eager loading remains unchanged.

## Patterns from other systems

These are architectural comparisons, not feature-parity promises.
The linked first-party references were checked during proposal review.
Recheck provider/version details before selecting implementation dependencies.
LiteFS documents limited support and stale-node lease risks, reinforcing the
fencing requirement below. The libSQL row describes its embedded-replica model,
not the newer Turso Database engine.

| System and reference | Copy for VLite | Do not copy / important distinction |
|---|---|---|
| [LanceDB / Lance](https://docs.lancedb.com/) | Versioned datasets over immutable files; pin a snapshot before reading | Do not import a table format or imply P2 is already columnar/lazy; embedded OSS and managed serving differ |
| [Turbopuffer](https://turbopuffer.com/docs/architecture) | Separate durable object data from disposable local query caches | Do not assume its cloud service architecture, economics or latency apply to a small embedded database |
| [Litestream](https://litestream.io/how-it-works/) | Snapshot plus ordered WAL archive; rehearse restore and retention | Disaster recovery is not a writable replica or zero-loss failover; SQLite WAL rules are not VLite framing |
| [LiteFS](https://docs.fly.io/litefs) | Single-primary discipline, lease ownership and explicit read consistency | Do not require FUSE, proxy routing or SQLite transaction files in the embedded engine |
| [libSQL / Turso](https://docs.turso.tech/introduction) | Local read replica with visible synchronization position and deliberate write routing | Do not add SQL compatibility or silently forward an embedded mutation to a remote writer |
| [Neon](https://neon.com/docs/introduction/branching) | Isolated branch identity, retained ancestor and independent compute/storage lifecycle | Do not copy PostgreSQL page servers, WAL consensus or claim arbitrary PITR from P2 checkpoints |
| [Dolt](https://www.dolthub.com/docs/introduction/what-is-dolt/) | Explicit branch heads and conflicts if merging is ever approved | Do not promise automatic vector/graph merges, SQL semantics or Git-like commits now |
| [lakeFS](https://docs.lakefs.io/) | Reference-based retention and immutable object publication | A versioned object namespace is not a graph transaction engine or database merge algorithm |
| [Qdrant](https://qdrant.tech/documentation/scaling/distributed_deployment/) | Explicit replica membership, acknowledgement policy and recovery state | Do not introduce shard consensus, resharding or cluster control into every embedded open |
| [Weaviate](https://docs.weaviate.io/weaviate/concepts/replication-architecture) | Name consistency levels and expose lag/conflict behavior | Do not inherit leaderless repair semantics for VLite's single ordered graph/vector log |
| [Milvus](https://milvus.io/docs/architecture_overview.md) | Distinguish ingestion log, sealed segments and query-serving readiness | Do not require a message broker and distributed control plane for local libraries |

## Shared invariants

A generation is a fully validated committed prefix identified by database
lineage, floor, head, format/capabilities, file lengths and content digests.
Local `MANIFEST` remains P2's authority; proposed remote catalog/provenance
metadata must not masquerade as existing P2 manifest fields.
Snapshot acquisition holds the source lock or a future explicit generation pin.
Never enumerate files without that protection or ship an active mutable tail.

Commit visibility, local durable sequence, replicated durable sequence and
object-published head are different positions. Do not infer durability from
`sequence` or `ReplicaStatus.primary_head`; the latter is only the latest
advertised head (`src/replica.rs:129-138`).
Periodic primary acknowledgements retain their documented OS/power-loss window
(`docs/STORAGE-FEATURES.md:241-251`); Full acknowledges only after local sync.
Fork/export publication additionally requires a successfully synced boundary.
Indeterminate publication is an explicit error requiring inspection, not retry
with a new identifier or silent fallback to an earlier generation.

Treat backup data as whole-database privileged data. Database ciphertext does
not include dashboard users, audit stores, credentials or tenant configuration.
Provision those separately and preserve deny-unmapped authorization on serving.
A stale restored tenant policy must not reopen revoked access.
Use private paths, bounded parsers, allowlisted object names and digest checks.
Catalog metadata needs TLS and authenticated publication; CRC detects accidents,
not a malicious publisher. Never log keys, tokens, payloads or signed URLs.

Preserve record TTL and graph node/edge cascade at each operation's clock.
At equal sequence but different clocks, TTL-visible results may differ.
Followers/readers hide due entities without emitting synthetic delete commits.
Preserve attachment ownership until committed reclamation, graph batch limits,
schemas, IDs and high-water marks; a node expiry does not delete its vectors.
Retained branches/backups still contain bytes after logical expiry or forget:
privacy erasure needs retention/key retirement across every copy, not just TTL.

Checkpoint floor is not an audit-history guarantee. Temporal graph validity
would preserve retained closed/future facts, not resurrect TTL-erased facts
(`docs/projects/roadmap/gap4-temporal-graph.md:121,155`).
Directory timestamp PITR remains unsupported. Retain explicit old generations
and contiguous archived WAL if earlier sequence restore is required; an old
backup may cease to be a byte prefix after a checkpoint, so use its own chain.

Compact critical blocks must decode or fail closed; authentic unsupported
derived blocks may be reported and rebuilt under the compact contract.
Corruption is never a cache miss. New identity makes lineage-bound ANN/index
data stale; never patch a checksum to bless it. Encrypted data cannot acquire
plaintext sidecars, disk spills or shared plaintext caches.
No new production compact writer or block capability is approved here.

## Gap 11: minimal local branching

**Existing base:** P2 images and fresh WALs; validated copy-out/restore identity;
revision-preserving conversion; graph replay and TTL tests in the evidence table.
There is no branch catalog, reflink operation or shared-reference GC today.

First fork a closed/quiesced source into a new destination that must not exist.
Acquire its maintenance lock, validate the complete chain and capture head `S`.
Initially reject an incomplete source; do not repair or checkpoint it implicitly.
An image at `S` with an empty active WAL is the COW candidate.
A source with later WAL commits needs a private restated image at `S`; expose
that as materialization, not a cheap fork. Single files use a validated image
conversion path, subject to P2's timestamp/version refusals.

For a supported plaintext reflink, create a distinct destination image inode
sharing immutable extents, replace only its file-header lineage and header CRC,
and validate the result. Do not hardlink then patch: that changes the parent.
Keep the history floor and record revisions; allocate a new lineage so parent
cursors and replica segments fail. Compact lineage-dependent metadata requires
its owning codec's validated rewrite/rebuild or explicit refusal.
Give the branch its own matching WAL header, LOCK and ordinary P2 MANIFEST.
Without reflink support, explicitly select a validated copy/materialization.

Preserve snapshot-local record CAS values; new commits start above `S`.
Cross-branch revision numbers can coincide: external task fences must include
branch/database identity, not use a bare sequence as a universal token.
Graph revisions retain the current P2 restatement rule; do not promise that
every graph `expected` token survives a newly materialized snapshot.

Encrypted forks use a fresh cipher salt/domain and authenticated rewrite of
the image, even when the operator intentionally supplies the same root key.
Generate matching private WAL encryption metadata; never copy a mutable WAL.
Cheap encrypted sharing with per-segment keys would change P2's one-descriptor
contract and is deferred to explicit format/security approval.

Sync destination image and WAL, validate, publish manifest, sync destination
and its parent before reporting success. No writer can open a staged branch.
A crash leaves no accepted branch or one complete branch; ambiguous final
directory publication is reported for inspection. Destination writers use
their own local lock. Parent and branch are intentionally separate databases,
not split-brain primaries; no automatic merge or follower promotion occurs.

TTL continues against real read clocks; branching does not pause deadlines.
An expired record is not revived by moving to a fresh identity. Preserve graph
attachments, allocation marks and future/retained temporal facts.
Rollback means stop the current owner and select a verified new fork/restore,
not replace a live manifest with an old one or reuse external fencing tokens.

Operations cost: private WAL/indexes, clone metadata, full-copy fallback,
encrypted rewrite, COW write amplification and future checkpoint duplication.
Each branch owns separate file names/inodes, so ordinary local deletion cannot
unlink the parent's image. No cross-branch reference-count GC is needed yet.
Quota and branch-count limits belong to the caller until an admin API is approved.

## Gap 10: minimal object-storage tier

**Existing base:** P2 immutable images/sealed logs and packed backups;
verified backup/drill lifecycle (`src/server/ops.rs:583-648`);
encrypted packed-directory parity (`tests/directory_recovery_matrix.rs:280-330`).
There is no object client, remote manifest or lazy remote reader today.

Keep a local disk writer. Publish a validated snapshot at `S` plus any sealed,
contiguous WAL through `H` as immutable, content-addressed objects.
Never append an object or use object storage as an OS-lock filesystem.
A remote generation descriptor lists exact object keys, lengths, digests,
lineage, floor/head, cipher identity and capabilities.
Use unique immutable generation names; update a small named head with provider
conditional replacement (S3 version/ETag precondition or Azure Blob equivalent).
Specify/test exact provider semantics; ETags are not portable content hashes.

Upload objects, complete multipart uploads, verify exact versions/checksums,
then publish the generation descriptor and conditionally advance the head.
Retries reuse object identity; a torn upload never becomes a listed object.
An uncertain head update is reconciled by reading its exact generation/version.
Two publishers racing the same expected head yield one winner; the loser
reports conflict. A publisher ownership epoch complements CAS to reject an
old process that reacquires fresh versions; absent that authority use a single
operator-controlled publisher, not automatic cross-host ownership.

Local commit ACK is unchanged; publication ACK requires all referenced objects
and the conditional head update to be confirmed. Cloud outage does not claim
an unpublished commit is remotely durable. Bound retained local backlog and
report/backpressure when its configured limit is reached.
An object replica is a pinned read generation, never a promotable writer.

First reader downloads a complete generation into a private staging directory,
checks all files and crypto/graph invariants, then installs a normal P2 store
read-only. Cache entries key on immutable digest/version, not a mutable name.
A stale head may serve an explicitly requested historical generation; it must
not satisfy a caller's minimum head or claim freshness from a TTL timer.
Missing objects/corrupt bytes fail explicitly; never select "latest" by listing.
Lazy range reads and genuinely small serverless residency wait for P3-P5
accessors and independent acceptance; downloads alone do not solve eager RAM.

Ship ciphertext unchanged; supply database keys only to authorized readers.
Provider-managed encryption supplements, not replaces, database encryption.
Use least-privilege bucket/container identities and private network policy.
Cache ciphertext on disk; authenticate before use and bound decrypted memory.
Key rotation publishes a re-encrypted, new-lineage generation and requires
reader/replica rebootstrap (`src/logops.rs:15-22`).
Old keys remain necessary for retained generations until explicit retirement.

Keep TTL, graph and compact semantics from the shared invariants.
Pin readers, branches and backup/PITR chains in an authoritative retention
catalog; readers use bounded renewable pins and stop acquiring data if expired.
GC uses catalog reachability plus a grace period and conditional recheck, not
object age alone. Retained WAL enables only explicitly covered restore targets.
Rollback publishes a new head revision referencing a verified retained read
generation; writable rollback creates a fresh lineage and fences the old owner.

Operations cost: stored versions, requests/range GETs, egress, duplicate uploads,
local staging/cache, retention scans and key management. Expose actual counters
and configured budgets; do not assert lower costs without workload accounting.

## Gap 9: minimal network replication and failover

**Existing base:** `Database::export_log`, `LogSegment`, `ReplicaFollower`,
backup bootstrap, synced apply/rollback and graph parity. Apply currently
reloads the whole file and invalidates ANN (`src/lib.rs:2380-2407`).
No transport, election, durable leader epoch or distributed lease is supplied.

Start one primary with read-only pull followers on private local disks.
Bootstrap a checkpointed primary from a pinned validated backup at `S`,
then request whole-commit segments after the follower's durable position.
Negotiate lineage, exact header/cipher, capabilities and leadership epoch.
Use bounded authenticated transport, cancellation, reconnect/backoff and
database-level replication grants; normal tenant reads cannot export all data.
This is an internal proposal, not approval of REST routes, proto or wire fields.

Primary syncs an export boundary before advertising it as durable.
Follower ACK follows append, sync, successful replay/validation and durable
progress publication. A lost ACK is resolved by querying durable status;
do not feed duplicates into `apply()` and call `ReplicaDiverged` success.
Persist progress atomically, reconcile against validated disk state on restart,
and never trust a cursor that is ahead of the file.
Keep current all-or-nothing state installation; optimize replay separately.

Asynchronous replication does not change primary ACK, so failover may lose
acknowledged primary writes beyond the chosen follower's durable sequence.
An optional synchronous policy would ACK only after local Full durability plus
the configured number of durable follower ACKs; unavailable followers block
or return an explicit indeterminate result, never downgrade to async silently.
Timeout after commit may mean success: callers reconcile idempotency keys.
Read-your-writes requires waiting for an explicit lineage/epoch/sequence token.

Do not enable automatic promotion initially. Planned handoff stops/fences the
old writer, syncs `H`, waits for follower durable `H`, transfers ownership, then
opens the replacement writer. Unplanned handoff requires an independent durable
fencing authority or infrastructure proof that the old host cannot write.
Lease expiry alone is insufficient: every write gate and external side effect
must reject old epochs, including after pause/resume and lease-service partition.
No authority/quorum means no promotion; local LOCK and KV revisions cannot
provide cross-host consensus. Old primaries rejoin read-only and rebootstrap,
never merge divergent suffixes or regain ownership by rolling back metadata.

Retention pins WAL until followers catch up, subject to bounded space policy.
A follower below the checkpoint floor reboots from a new backup; never retain
unbounded history for a disconnected peer. Corrupt immutable files fail closed;
only unacknowledged active tail damage is repairable.
TLS/mTLS plus scoped authorization protects transfer; followers need the same
database key descriptor. Rotation/restore/compaction starts a new lineage and
requires coordinated rebootstrap, with no plaintext index shortcut.

Preserve TTL clocks, complete graph batches, attachments and compact capability
checks. Lagging readers may see old facts, but due TTL data stays hidden.
Operations cost: follower disks/RAM, replay/index rebuilds, bandwidth, retained
WAL, certificates, fencing service, failover drills and on-call ownership.

## Acceptance matrix

All cases require restart verification, explicit errors and unchanged sources.
Run plain/encrypted and vector/graph variants; compare equal clocks/sequences.
Extend `tests/directory_store.rs`, `tests/directory_recovery_matrix.rs`,
`tests/replica.rs`, `tests/change_feed.rs`, `tests/graph_change_feed.rs`,
`tests/graph_maintenance.rs`, `tests/graph_ttl.rs`, `tests/pitr.rs` and
`tests/compact_storage.rs`; add transport/provider fault injection only when
those implementation slices exist. Physical sharing needs filesystem-level
extent accounting, not identical file contents or elapsed-time comparisons.

| Case | Gap 11 branch | Gap 10 objects | Gap 9 replication |
|---|---|---|---|
| Crash before publication | No accepted branch; parent unchanged | Unlisted upload is never readable as a generation | No ACK for unsynced/unvalidated suffix |
| Crash after publication / sync failure | Complete branch or reported indeterminate destination; inspect | Reconcile exact head version; no duplicate success | Durable status survives; ambiguous primary commit is reconciled |
| Partition / unavailable dependency | Offline local fork remains independent; no network lock needed | Existing pinned reads only; unpublished head never called durable | No automatic promotion; sync policy refuses silent downgrade |
| Torn upload / transfer | Partial copy/clone fails validation | Incomplete multipart/object cannot enter descriptor | Partial segment leaves old state; reopen repairs active tail only |
| Stale manifest / head | Reject mismatch; do not reconstruct from filenames | Version/digest and minimum-head checks; no listing fallback | Reject stale epoch, wrong lineage, gap and ahead cursor |
| Rollback | Fresh fork identity; parent tokens rejected | New catalog revision; writable restore needs new identity/fence | Fence old owner; no divergent suffix merge or epoch decrement |
| Concurrent owners | One writer per branch; no shared mutable inode | CAS conflict; stale publisher rejected by ownership policy | Pause/resume old primary cannot commit after handoff |
| Keys / corrupt bytes | Fresh cipher domain; wrong key and corrupt image fail | Ciphertext cache; missing key/object fails explicitly | Header/AEAD mismatch leaves follower state unchanged |
| TTL / graph / history | Deadlines, CAS, attachments and IDs preserved; parent cursor rejected | Pin generation; floor/PITR targets explicit; no TTL resurrection | Whole graph commit; clock-aware visibility; below-floor bootstrap |
| Compact / compatibility | Critical source parity; stale derived index reported | Unknown critical kinds refused before serving | Capability mismatch rejects bootstrap before ACK |
| Delete / retention | Deleting branch cannot damage parent | Pins protect active generations; GC races rechecked | Slow follower resyncs when bounded retained history is gone |

## Implementation slices and exit criteria

| Slice | Scope | Exit before proceeding |
|---|---|---|
| 0: common snapshot contract | Storage-owner review of identity, pins, sync failures, compact and crypto boundaries | Exact generation/ACK definitions accepted; no public schema changes |
| 11a: isolated copy fork | Closed source, private image/WAL, new lineage, explicit materialization and fresh encryption domain | Source-byte invariance, crash publication, CAS/graph/TTL parity and old-cursor refusal |
| 11b: genuine COW fork | Supported plaintext reflink adapter with explicit full-copy alternative | Independent inodes; extent sharing verified on qualified filesystems; mutation/deletion cannot alter parent |
| 11c: lifecycle qualification | Branch cleanup, restore selection, format/key refusals and compact integration | Full acceptance column passes; document supported platforms and encrypted full-copy limitation |
| 10a: object generations | One provider, sealed upload, verified descriptor, conditional head, explicit backlog | Torn uploads and conflicting publishers cannot publish incomplete generations |
| 10b: read-only materialization | Private ciphertext cache, pinned generation, complete local validation | Stale/missing/corrupt data refuses; tenant policy and key boundaries survive restore |
| 10c: retention and drills | Pin catalog, GC races, archived sequence chains, cost counters | Delete/rollback/key-retirement drills pass; lazy/serverless claims remain separately gated |
| 9a: pull replication | Approved internal transport, backup bootstrap, resumable synced segment ACK | Crash/lost-ACK/partition/capability cases pass without duplicate apply or mutation exposure |
| 9b: fenced handoff | Independent ownership authority, durable epochs, old-writer rejection | Planned handoff proves target head; unplanned drill discloses loss and excludes old owner |
| 9c: stronger ACK policy | Optional synchronous replication and minimum-position reads | No downgrade on timeout/partition; acknowledged prefix survives the specified fault model |

## Risks and open decisions

| Open decision / risk | Recommendation |
|---|---|
| Is this order approved? | Adopted 2026-11-02: 11 -> 10 -> 9, backlogged until revisited; only an explicit HA requirement should reorder it |
| What counts as Gap 11 completion? | Qualify real COW on named supported plaintext filesystems; label full-copy/encrypted paths honestly, not universal cheap branches |
| Portable sharing, online forks, merge? | Defer hardlink catalogs, live generation pins and merging; start quiesced, independent-inode forks |
| Encrypted COW versus isolation? | Fresh cipher domain/full rewrite first; per-segment keys need separate format/security review |
| Revision and external claim safety? | Retain record revisions locally, scope external fences to database identity and leader epoch; audit graph restatement separately |
| First object provider? | Decided 2026-11-02: Azure Blob first to match current deployment context, with native local disks; S3 follows the same semantic matrix |
| Remote manifest and pin format? | Separate versioned catalog under storage-owner review; do not reinterpret current P2 MANIFEST or reuse age-only retention |
| Offline object readers and erasure? | Bounded renewable pins, explicit retained-copy policy and key retirement; do not promise immediate deletion from historical copies |
| Timestamp PITR on directories? | Keep unsupported; implement only retained sequence-generation chains until a separate timestamp format decision |
| Leader authority and failover loss? | External durable fencing authority for HA; manual infrastructure fencing first; async loss bound is the actual missing durable suffix |
| Sync replication default? | Decided 2026-11-02: async read followers first, explicitly not zero-loss HA; synchronous ACK and availability tradeoff still require owner approval |
| Tenant/admin restore scope? | Whole-database operator privilege, separate policy backup/restore and revocation review; no tenant-scoped raw export |
| Compact blocks and lazy remote reads? | Require source-codec/maintenance parity before enablement; lazy readers wait for P3-P5 accessors |
| Public APIs and competitor verification? | Review Rust names/internal transport first; approve wire/OpenAPI/proto changes separately and verify linked first-party references before implementation |

This document changes no runtime, wire schema, benchmark, deployment or release
gate. No performance campaign is warranted for this design-only change.
DO-005 remains at the major release under the unchanged quiet-host policy.
