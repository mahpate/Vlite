# Gap 2: fenced-lease ledger design

| Status | design accepted 2026-11-02; Phase 2 gated. Docs only; no production code, public API, wire change, or release claim. |
|---|---|
| Slice | `gap2-rust-lease-ledger` |
| Contract | [Serving KV and TTL contract](serving-kv-ttl-contract.md), sections 5 and 12 |
| Scope | Embedded Rust storage feature; serving is a dependent slice |

The recorded rulings below are owner-visible planning defaults, not approval
to implement. They explicitly identify the wording change against the
reviewed contract; the coordinator owns contract cross-references.
Source anchors below refer to the current branch at
the time of this design; line numbers are anchors, not a promise about later
rebases.

### Recorded rulings (2026-11-02)

| # | Ruling and one-line consequence |
|---:|---|
| 1 | Layout direction accepted: V5 flag `0x0080`, auxiliary op 18, existing VREC/VCMT CRC/AEAD framing, and explicit offline `maintenance::enable_lease_ledger` copy-out; no in-place or implicit enablement or create-time default. |
| 2 | Guarantee limited to VLite-managed backup, restore, PITR, directory image restore/rollback, managed copy-out, tail repair and replica promotion, which stamp terminal `RecoveryRequired`; unmanaged filesystem copies/rollbacks are not claimed. This is an owner-visible wording change against contract section 5's copy/branch/rollback fails-closed language. |
| 3 | The 4,096 distinct-row/file cap and 255-byte holder bound are accepted as proposals, surfaced for confirmation against future tenant quotas. |
| 4 | Stamping opted-in ledger backups `RecoveryRequired` is accepted at design level; non-ledger backups remain byte-identical. |
| 5 | Change feed remains fail-closed on op 18 (`Corrupt`); older header readers return `UnsupportedFeature(0x0080)`; no feed work belongs to this slice. |
| 6 | Fallible `Replica::promote` is an owner-visible source-compatibility change requiring explicit owner approval as a Phase-2 prerequisite; no signature changes yet. |
| 7 | Defer public domain inspection/reset and create-with-ledger; ordinary database operations continue under existing mode rules on `RecoveryRequired` files, leases refuse, holder stays explicit, and re-enrollment requires a fresh destination/domain plus downstream cutover. |
| 8 | Phase 2 is not approved and serializes behind P3b and P4 in their shared format/database/maintenance/directory/replica conflict domain; no code starts under this design acceptance. |

## 1. Scope, claims, and non-goals

| In scope | Not claimed or authorized |
|---|---|
| A separate, durable, file-wide fencing-token allocator and per-address lease ledger, behind an explicit file capability. | No REST, gRPC, MCP, CLI, binding, site, route, protobuf, or OpenAPI work. `src/openapi.json` and the proto remain frozen. |
| An opt-in offline converter and crate-level Rust lease operations, subject to API review. | No automatic enablement, format default change, or modification to an existing file unless the owner explicitly runs the converter. |
| A restart barrier, synchronous lease writes, and fail-closed restore/promotion handling for VLite-managed lifecycle operations. | No multi-node allocator, distributed lock, linearizability claim across copied files, or protection of a downstream resource that does not enforce `(fence_domain, fencing_token)`. |
| Explicit bounds, format bytes, storage/error behavior, and acceptance tests. | No performance, size, availability, capacity, or benchmark claim. No benchmark campaign. |
| Preserving ledger state through the listed VLite rewrite/checkpoint paths. | No change to ordinary KV durability, graph semantics, change-feed wire format, policy, gate, or release criterion. |

The existing KV API stores ordinary records and uses revisions as
compare-and-set tokens (`src/kv.rs:1-17`). A record revision is not a
never-reused lease token. The existing public `SnapshotLease` at
`src/records.rs:219` is a reader-retention guard and is not a lease API name.

## 2. Persisted state and minimization

The contract lists `fence_domain`, unsigned high-water, per-key
`(holder, token, deadline, active)` state, and recovery state
(`serving-kv-ttl-contract.md:305-324`). Persist all four. Keeping all four is
deliberate even though the startup barrier invalidates active grants:

| State | Persist? | Why it remains necessary | Crash / replica / inspection consequence |
|---|---|---|---|
| `fence_domain: [u8; 16]` | Yes; immutable during ordinary writes and maintenance carry-forward. | Tokens are only comparable within this domain; it is not the file lineage or a record revision. | A restore that reuses the domain cannot establish a global floor. Promotion and restore therefore set `RecoveryRequired`, not a guessed new token. Inspection may expose the canonical lowercase UUID to the embedded owner; no serving route is added here. |
| `high_water: u64` | Yes; monotonically increasing, including tokens burned before grant publication. | It is the only durable evidence that an allocation was already consumed. It survives restart, grant expiry, revoke, collection drop, and recreation. | A synced reservation replays even if the following grant does not. A backup can preserve it, but a stale restored copy cannot prove it is current. No replica promotion may rely on a replicated value as a global floor. |
| Per-key grant `(collection, namespace, key, holder, token, deadline, active)` | Yes, including revoked/inactive records, up to the file bound. | It implements live-conflict checks, same-holder renew/revoke, deadline checks, and crash replay of the current handle's state. It is not stored in ordinary KV and is not keyed only by a hash. | A process crash does not preserve authority: the next writable open syncs a barrier that invalidates all persisted grants before admitting lease operations. Before that barrier, a failed/ambiguous open cannot serve leases. Replica readers never serve leases. |
| Recovery state | Yes; `Ready` or terminal `RecoveryRequired` in this release. | A restore/promotion/copy-out must not be made usable merely because it can write a new record. | `RecoveryRequired` survives restart and is never cleared in place. The approved operational escape is a fresh destination/domain and explicit downstream re-enrollment, not a recovery API in this slice. |

**Decision B2.** Do not remove per-key grant rows merely because a restart
invalidates them. That would make restore/copy inspection incomplete, erase
the durable explanation for a stale holder, and make compaction carry-forward
dependent on reconstructing current grants from memory. Do not treat
`RecoveryRequired` as a substitute for the high-water: normal restart is
recoverable in the same domain and must issue a strictly larger token.

**Accepted proposal; quota confirmation pending.** The contract does not state a numeric ledger
grant cap. This design proposes a 4,096 distinct-address hard per-file cap,
including inactive entries, in addition to the contract's finite configured
per-tenant quota (section 10, decision (k), lines 906-908 and 1056). The cap is a
safety bound, not a capacity claim. It refuses a new distinct address rather
than evicting an active grant or resetting the high-water. Existing address
renewal/revoke remain possible at the cap.

## 3. Persistent format

### 3.1 Mechanism choice

| Candidate | Evaluation |
|---|---|
| Header flag plus a new V5 WAL operation | **Choose.** A capability bit rejects legacy readers before replay (the precedent is `docs/GRAPH.md:88-92`); op 18 is naturally CRC/AEAD framed, ordered with commits, synced independently, shipped by existing log segments, and copied by existing backups. Recovery and allocator events are explicit replayable records. |
| Compact-catalog critical entry | Reject. The compact catalog is an image capability table under `FLAG_COMPACT` and op 16 (`compact-storage-contract.md:50-92`; `src/compact/mod.rs:661-815`). A lease ledger is required in ordinary non-compact logs too, and cannot be made dependent on compact-image presence. The P3/P4 compact owner does not own this slice. |
| Unflagged op 18 or ordinary KV/record encoding | Reject. A legacy reader could skip or misinterpret it; record revisions are rewritable; ordinary KV writes and collection lifecycle operations must not mutate the ledger. |
| Sidecar or external allocator | Reject for this slice. A sidecar is not part of the one-file logical stream and adds a second atomicity/backup/replica problem. An external non-rollback authority is not the reviewed decision (c). |

The bit and op are unassigned in the current V5 registry. Header bits in use
are `1` encrypted, `2` timestamps, `4` graph, `8` history, `16` compact,
`32` precision, and `64` temporal (`src/format.rs:18-81`,
`src/property_graph/engine/codec.rs:32-34`). `0x0080` is the proposed
`FLAG_LEASE_LEDGER`. The current frame-operation registry is:

| Op | Current meaning |
|---:|---|
| 0 | Unassigned / invalid |
| 1-10 | Vector put, delete, batch, extended put, collection create, collection drop, collection settings, metadata update, expiring put, named-vector extension |
| 11 | Commit time anchor |
| 12 | Graph operation |
| 13 | History advance |
| 14 | Record block |
| 15 | Graph snapshot |
| 16 | Compact block |
| 17 | Precision collection create |
| **18** | **Unassigned; proposed `OP_LEASE_LEDGER`, valid only with `FLAG_LEASE_LEDGER`** |

Op 18 does not collide with 1-17 (`src/format.rs:18-81,1520-1704`;
`src/property_graph/engine/codec.rs:32-34`). The loader must reject op 18
without the flag and reject the flag on a version other than V5. A reader
that does not support the bit follows the existing header path:
`read_log_header_with` checks `flags & !known_flags` and returns
`Error::UnsupportedFeature(flags & !known_flags)` before replay or tail
repair (`src/format.rs:1442-1452`). No new generic `Error` variant is needed
for older-reader refusal.

### 3.2 Op-18 prefix and body

Op 18 uses the existing 36-byte `VREC` prefix and 16-byte `VCMT` footer
(`src/format.rs:94-95,636-646`). It is a V5 auxiliary frame: its prefix and
footer carry the sequence of the **next ordinary commit** (as op 11's time
anchor does), but it does not consume that sequence or change record
revisions, `Loaded.sequence`, or `commits_seen`. Multiple ledger frames may
share that sequence; physical log order orders ledger mutations. This lets
checkpoint snapshots follow the compact image without renumbering concurrent
commits or changing the history floor.

| Prefix field | Required value |
|---|---|
| operation | `18` |
| collection | `0` |
| reserved byte | `0` |
| sequence | Exactly `Loaded.sequence + 1`; never a record revision; repeats for consecutive op-18 frames before that ordinary commit |
| id | `0` |
| count | `0` |
| payload length | Exactly `64 + namespace_len + key_len + holder_len`, bounded below |

All multibyte integers are little-endian. The fixed 64-byte body header is:

| Offset | Width | Field | Rule |
|---:|---:|---|---|
| 0 | 4 | Magic | ASCII `VLL1` |
| 4 | 2 | Schema version | `1`; unknown nonzero versions refuse with `UnsupportedFeature(FLAG_LEASE_LEDGER)` |
| 6 | 1 | Kind | `1=DomainInit`, `2=Reserve`, `3=Acquire`, `4=Renew`, `5=Revoke`, `6=StartupBarrier`, `7=RecoveryRequired`, `8=SnapshotGrant` |
| 7 | 1 | State flags | bit 0 is `RecoveryRequired`; bit 1 is the grant's `active` value for grant kinds; bits 2-7 zero |
| 8 | 16 | Domain | Raw UUID octets, same value in every frame |
| 24 | 8 | High-water | Unsigned `u64`; non-decreasing and nonzero tokens never exceed it |
| 32 | 8 | Token | `0` except `Reserve`, `Acquire`, `Renew`, `Revoke`, `SnapshotGrant` |
| 40 | 8 | Deadline | Absolute Unix milliseconds; `0` for kinds without a grant deadline |
| 48 | 2 | Namespace length | `0..=255` UTF-8 bytes; nonempty for grant kinds |
| 50 | 2 | Key length | `0..=1024` UTF-8 bytes; nonempty for grant kinds |
| 52 | 2 | Holder length | `0..=255` UTF-8 bytes; nonempty for grant kinds |
| 54 | 2 | Reserved | Zero |
| 56 | 8 | Reserved | All zero |
| 64 | variable | Namespace, key, holder | Exact UTF-8 bytes in that order; no normalization |

`frame.collection` is the physical `u16` collection ID for grant kinds and
zero for all file-wide kinds. Grant payload byte fields are bounded by the
reviewed namespace/key constraints (`serving-kv-ttl-contract.md:164-185`);
the proposed 255-byte holder bound is a storage bound and requires owner
confirmation against future tenant quotas. The input holder is a stable, opaque, non-secret principal identifier, not a
bearer token, credential, or arbitrary request body.

The loader handles op 18 after prefix/body bounds and frame authentication but
before the ordinary contiguous-commit check. It requires its sequence to be
exactly `loaded.sequence + 1`, applies the ledger event, advances only `end`,
and leaves `loaded.sequence` and `commits_seen` unchanged. The next ordinary
commit may use that same sequence. In a timestamped file, op 18 is exempt from
the mandatory time-anchor check because it is not a user commit; an
unconsumed op-11 anchor beside op 18 is invalid, and all ordinary timestamped
commit rules remain unchanged.

| Kind | Validation and state effect |
|---|---|
| `DomainInit` | First ledger frame only; no grant fields, valid domain, state `Ready`, flags `0`. Initial conversion writes high-water `0`; compact/checkpoint snapshots write the carried high-water. |
| `Reserve` | No key fields; `token == high_water`, nonzero, and strictly above prior high-water. The next op-18 frame must be `Acquire` with the same token, unless a crash leaves it at EOF or the next frame is a startup/recovery barrier; those cases burn the token. An unrelated ordinary commit while a reservation is pending is corruption. |
| `Acquire` | Active flag set; grant row has nonzero token equal to current high-water; token equals the immediately preceding matching reservation. Replaces only an expired/inactive row for the same physical address. |
| `Renew` | Active flag set; same address, holder and token as an active prior row; token unchanged; `now < prior deadline`; new deadline is checked `now + ttl`. |
| `Revoke` | Active flag clear; same address, holder and token as the active row; persists the row with `active=false`. |
| `StartupBarrier` | Flags `0`, no grant fields; only valid from `Ready`; replay marks every grant inactive, clears a pending reservation, and preserves domain/high-water. |
| `RecoveryRequired` | Only recovery-required bit set; no grant fields; terminal state bit set, all grants inactive; used by VLite-managed restore/promotion/copy-out paths. |
| `SnapshotGrant` | Active bit carries the persisted grant's active value; a compaction/checkpoint restatement after the image end replays one row without changing domain/high-water. Snapshot rows are sorted by `(collection, namespace bytes, key bytes)`. |

For kinds 3-5 and 8, the final eight bytes of variable data are not inferred:
the three exact byte strings end at `payload_len`. The parser rejects a length
sum mismatch, invalid UTF-8, nonzero reserved bytes, unknown kind/flags,
duplicate snapshot address, non-canonical domain, token zero, deadline overflow,
or an invalid kind/state combination as `Error::Corrupt` at the frame offset.
It bounds lengths from the prefix before allocation.

`MAX_LEASE_GRANTS_PER_FILE = 4096`. The largest single body is
`64 + 255 + 1024 + 255 = 1598` bytes, below `MAX_LEASE_FRAME_BODY = 2048`;
the frame is separately bounded by `check_frame_prefix`. Total live/inactive
rows are capped before allocation or write. The hard cap and 255-byte holder
bound are proposals, not reviewed contract facts.

### 3.3 Integrity and encryption

No separate checksum is added. Existing prefix CRC covers prefix bytes
`0..32`, footer CRC covers on-disk frame bytes before the footer, and
`verified_frame` authenticates encrypted frame bodies through the existing
AEAD path (`src/format.rs:636-646,2654-2674`). Op 18 is encrypted exactly like
other frames: the body, including holder and key identity, is plaintext only
in memory and authenticated ciphertext on disk when the file is encrypted.
The V5 header flag remains covered by the existing header CRC; it is not an
AEAD substitute.

No new header bytes or encryption descriptor fields are introduced. A
supported build checks the schema version after frame verification. An older
build rejects `0x0080` as `UnsupportedFeature(0x0080)` before replay; a future
build that understands the flag but not schema version 1 also refuses rather
than skipping ledger state. Structural damage remains `Corrupt`, not a
capability miss.

## 4. Enablement, conversion, and file lifecycle

| Path | Design |
|---|---|
| Existing file without ledger | Remains unchanged/readable by every older reader. No lazy opt-in on open, first lease, KV write, import, or collection creation. |
| Enablement | One explicit offline copy-out, `maintenance::enable_lease_ledger(source, destination, key)`, modeled on `enable_graph` and P2 conversion (`src/property_graph/maintain.rs:176-200`; `src/maintenance.rs:219-256`). Source stays untouched; destination must not exist and is validated before publication. |
| New empty ledger file | Create an ordinary V5 source, then explicitly convert it. The converter emits a fresh `DomainInit`; standard `Database::create*` and `create_directory` do not set the flag. |
| Converted file | Preserve ordinary database state and supported graph/temporal/precision capabilities; create fresh V5 lineage as required by the rewrite, fresh UUID domain, high-water 0, no grants, state `Ready`. Unsupported feature combinations refuse before output is published. |
| In-place append | Not allowed. Setting a header capability bit while the file is open is not atomic with the required converter, breaks older-reader refusal guarantees, and leaves incomplete migration states. |
| Reversal | No in-place `strip_lease_ledger` in this release. Removing the flag while dropping high-water would allow token reuse. A future explicit lossy conversion needs its own owner decision and downstream-reenrollment warning. |
| Writable open | Load and validate the whole file; if ledger state is `Ready`, append and sync `StartupBarrier` before returning an operation-capable `Database`. If the barrier cannot be written/synced, open returns `CommitIndeterminate` or the explicit I/O error and the handle is poisoned/not returned. |
| Read-only open | Load the state but do not write the barrier. Lease methods return `LeaseUnavailable`; ordinary read-only database operations are unchanged. This check occurs after `read_only` is set, not in the shared loader. |
| Directory read-shared open | No barrier write; lease operations are unavailable. |
| Replica open | No barrier write while follower; lease operations are unavailable. `promote` stamps a recovery-required state before exposing a writable database (see section 7). |
| Backup | For a ledger-enabled source, copy all ordinary bytes and append a synced `RecoveryRequired` op-18 frame to the destination; this preserves domain/high-water but makes the archive unusable for leases. Non-ledger backups remain byte-identical. Accepted at design level as an opted-in exception to `backup_inner`'s current byte-identical copy behavior (`src/maintenance.rs:193-208,371-388`). |
| In-memory / `Durability::None` | No durable lease service. Return `LeaseUnavailable`; never synthesize an acknowledged durable token in memory. `Durability::None` is only valid for in-memory handles (`src/durability.rs:32-44`, `src/lib.rs:1642-1665`). |

An open of a lease-enabled file always invalidates grants written by the prior
process before any lease request is admitted. A process that opens the
file read-only, as a replica, or in shared mode does not silently claim the
barrier was performed. The ordinary database may still be read as allowed by
its existing mode.

## 5. Rust API and error mapping

The API is Rust-only in this slice. Serving later adapts these typed results
to the already-reviewed codes; this design adds no route, wire type, server
`ApiError` arm, or proto/openapi entry. `LeaseStore` is distinct from
`SnapshotLease`.

### 5.1 Proposed minimal public surface

Signatures below are proposed exactly; names and errors require API review.
The holder is supplied by the embedded caller and later serving derives it
from the authenticated stable principal.

```rust
pub struct LeaseAcquireRequest<'a> {
    pub namespace: &'a str,
    pub key: &'a str,
    pub holder: &'a str,
    pub ttl_ms: u64,
}

pub struct LeaseRenewRequest<'a> {
    pub namespace: &'a str,
    pub key: &'a str,
    pub holder: &'a str,
    pub fence_domain: &'a str,
    pub fencing_token: u64,
    pub ttl_ms: u64,
}

pub struct LeaseRevokeRequest<'a> {
    pub namespace: &'a str,
    pub key: &'a str,
    pub holder: &'a str,
    pub fence_domain: &'a str,
    pub fencing_token: u64,
}

pub struct LeaseGrant {
    pub namespace: String,
    pub key: String,
    pub fence_domain: String,
    pub fencing_token: u64,
    pub expires_at_ms: u64,
}

pub enum LeaseError {
    LeaseHeld,
    StaleFencingToken,
    LeaseUnavailable,
    FencingRecoveryRequired,
    InvalidInput(&'static str),
    Storage(crate::Error),
}

pub struct LeaseStore<'a> {
    db: &'a mut Database,
    collection: u16,
}

impl Database {
    pub fn lease_store(&mut self, collection: &str)
        -> std::result::Result<LeaseStore<'_>, LeaseError>;
}

impl LeaseStore<'_> {
    pub fn acquire(&mut self, request: LeaseAcquireRequest<'_>)
        -> std::result::Result<LeaseGrant, LeaseError>;
    pub fn renew(&mut self, request: LeaseRenewRequest<'_>)
        -> std::result::Result<LeaseGrant, LeaseError>;
    pub fn revoke(&mut self, request: LeaseRevokeRequest<'_>)
        -> std::result::Result<bool, LeaseError>;
}

// In `src/maintenance.rs`:
pub fn enable_lease_ledger(
    source: impl AsRef<std::path::Path>,
    destination: impl AsRef<std::path::Path>,
    key: Option<&EncryptionKey>,
) -> crate::Result<Inspection>;
```

The following implementation items stay crate-private; signatures show the
proposed ownership boundary (the structs' fields remain private):

```rust
// `src/lease_ledger.rs`; fields are crate-visible, never public API.
pub(crate) const FLAG_LEASE_LEDGER: u16 = 0x0080;
pub(crate) const OP_LEASE_LEDGER: u8 = 18;
pub(crate) const MAX_LEASE_GRANTS_PER_FILE: usize = 4096;
pub(crate) const MAX_LEASE_HOLDER_BYTES: usize = 255;
pub(crate) const MAX_LEASE_FRAME_BODY: usize = 2048;

pub(crate) struct LeaseLedger {
    pub(crate) domain: [u8; 16],
    pub(crate) high_water: u64,
    pub(crate) grants: std::collections::BTreeMap<LeaseAddress, LeaseGrantState>,
    pub(crate) recovery: LeaseRecoveryState,
}
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct LeaseAddress {
    pub(crate) collection: u16,
    pub(crate) namespace: String,
    pub(crate) key: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LeaseGrantState {
    pub(crate) holder: String,
    pub(crate) token: u64,
    pub(crate) deadline_ms: u64,
    pub(crate) active: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeaseRecoveryState {
    Ready,
    RecoveryRequired,
}
pub(crate) enum LeaseFrame {
    DomainInit { domain: [u8; 16] },
    Reserve { domain: [u8; 16], high_water: u64, token: u64 },
    Acquire { domain: [u8; 16], high_water: u64, collection: u16,
              namespace: String, key: String, grant: LeaseGrantState },
    Renew { domain: [u8; 16], high_water: u64, collection: u16,
            namespace: String, key: String, grant: LeaseGrantState },
    Revoke { domain: [u8; 16], high_water: u64, collection: u16,
             namespace: String, key: String, grant: LeaseGrantState },
    StartupBarrier { domain: [u8; 16], high_water: u64 },
    RecoveryRequired { domain: [u8; 16], high_water: u64 },
    SnapshotGrant { domain: [u8; 16], high_water: u64, collection: u16,
                    namespace: String, key: String, grant: LeaseGrantState },
}

pub(crate) fn decode_frame(
    collection: u16,
    body: &[u8],
    offset: u64,
) -> crate::Result<LeaseFrame>;
pub(crate) fn encode_frame(frame: &LeaseFrame) -> crate::Result<Vec<u8>>;
pub(crate) fn apply_frame(
    ledger: &mut Option<LeaseLedger>,
    frame: LeaseFrame,
    offset: u64,
) -> crate::Result<()>;
pub(crate) fn snapshot_frames(ledger: &LeaseLedger)
    -> crate::Result<Vec<LeaseFrame>>;
pub(crate) fn new_domain() -> crate::Result<[u8; 16]>;
```

`src/lib.rs` owns private dispatch and sync, not the format codec:

```rust
impl Database {
    pub(crate) fn append_lease_frame(
        &mut self,
        frame: &lease_ledger::LeaseFrame,
    ) -> crate::Result<()>;
    pub(crate) fn complete_lease_startup_barrier(&mut self)
        -> crate::Result<()>;
}
```

Open-mode selection must also be explicit; `Replica::open` currently shares
the ordinary internal open path, so it cannot be allowed to trigger a writable
barrier by accident:

```rust
enum LeaseOpenMode {
    Writable,
    ReadOnly,
    Replica,
}

// Signature-only declaration; method bodies are specified in prose above.
impl Database {
    fn open_backed(
        file: LockedFile,
        key: Option<&EncryptionKey>,
        mode: LeaseOpenMode,
    ) -> crate::Result<Self>;
    fn open_inner_mode(
        path: &std::path::Path,
        key: Option<&EncryptionKey>,
        mode: LeaseOpenMode,
    ) -> crate::Result<Self>;
    pub(crate) fn open_replica_inner(
        path: &std::path::Path,
        key: Option<&EncryptionKey>,
    ) -> crate::Result<Self>;
}
```

`Replica` mode remains writable only to the internal segment-apply path; it
does not run `StartupBarrier`, does not expose a mutable `Database` through
`Replica::db`, and never creates a lease-capable handle. `promote` performs
the durable recovery stamp before returning a normal `Database`.

`append_lease_frame` stamps the next ordinary sequence (`self.sequence + 1`),
writes one op-18 frame, performs `sync_all`, then updates only file end and
ledger state. It does **not** increment `self.sequence`; the next ordinary
commit consumes the stamped sequence. It cannot be called from ordinary
record/KV/import/drop code. Production method definitions are associated
`Database` methods.

`lease_store` requires the ledger capability and a one-dimensional existing
collection, following `kv_view`'s non-creating behavior; it never creates
the collection. The holder is 1..=255 UTF-8 bytes. Namespace is 1..=255,
key 1..=1024, TTL 1..=86,400,000 ms, and token is nonzero. Empty, malformed,
overlong, or overflowed values return `LeaseError::InvalidInput`. The methods
take `&mut self`, so ordinary mutation and lease operations on one handle
serialize on the same owner.

### 5.2 Taxonomy

| `LeaseError` | Reviewed serving code | Meaning |
|---|---|---|
| `LeaseHeld` | `lease_held` | A live grant exists; disclose no holder and allocate no token. |
| `StaleFencingToken` | `stale_fencing_token` | Domain/token/holder mismatch, expired, inactive, or missing grant. |
| `LeaseUnavailable` | `lease_unavailable` | No durable writable owner: read-only, replica/shared, memory-only, unsupported durability, or failed capability. |
| `FencingRecoveryRequired` | `fencing_recovery_required` | Persisted terminal recovery state. |
| `InvalidInput` | `invalid_input` | Bounds, TTL arithmetic, token exhaustion, invalid identity or domain format. |
| `Storage(crate::Error)` | `commit_indeterminate` for `Error::CommitIndeterminate`; otherwise existing storage mapping | Preserve the storage cause; never turn an indeterminate write into success. |

There are no new `crate::Error` variants in this proposal. `LeaseError` is
the typed feature result; the later serving adapter maps it to the contract's
409/422/503 codes in sections 5 and 10 of
`serving-kv-ttl-contract.md:925-946`. `Storage(Error::CommitIndeterminate(_))`
is 503 `commit_indeterminate` and the handle is poisoned, matching
`src/server.rs:1577-1593`.

### 5.3 Clock and identity

Use the existing Unix millisecond source, with a ledger-local monotonic clamp:
`now = max(current_time_ms(), last_lease_clock_ms)`. Update
`last_lease_clock_ms = now` only after a valid clock read. Deadline is checked
`now.checked_add(ttl_ms)`; overflow is `InvalidInput`, never wraparound.
Inject a crate-private `FnMut() -> u64` clock in tests; no public clock
override and no sleep-based test.

The lease state is file-wide but keyed by `(physical collection id,
namespace UTF-8 bytes, key UTF-8 bytes)`. It is independent of KV records and
of each collection's name/reuse lifecycle. It does not disclose holder
identity on conflicts. An identity string is principal-bound, not transferable
by scope alone, and no admin takeover is added.

### 5.4 REG-001 public API surface gate

The stable-toolchain CI job runs `python scripts/api_surface.py check`
(`.github/workflows/ci.yml:114-116`). It builds
`cargo doc --no-deps --offline --locked --quiet -p vlite --target-dir target/api-surface`
and compares rustdoc's public item index to
`tests/fixtures/api_surface/vlite-default.txt`. Only additions under
`vlite::property_graph::` are auto-allowed; every other addition and every
removal fails until the fixture is hand-edited. Never run the script's
`baseline` command to accept this feature (`scripts/api_surface.py:1-24,60-64,110-130`).
Neither command runs in this docs-only phase.

The existing gate explicitly excludes inherent methods and fields
(`scripts/api_surface.py:22-24`); the fixture has no method-line kind.
Do not invent fixture lines for methods: their exact paths are listed below,
with site coverage and REG-002 compile compatibility instead. Public lease
types live in the already-public `vlite::kv` module, not at crate root;
`src/lease_ledger.rs` remains private, with no public re-exports.

| Exact public path | Exact REG-001 fixture line (or exclusion) | `site/api.html` data-api key |
|---|---|---|
| `vlite::Database::lease_store` | None: inherent method, not indexed | `rust:Database::lease_store` |
| `vlite::kv::LeaseStore` | `structs vlite::kv::LeaseStore` | `rust:LeaseStore` |
| `vlite::kv::LeaseStore::acquire` | None: inherent method, not indexed | `rust:LeaseStore::acquire` |
| `vlite::kv::LeaseStore::renew` | None: inherent method, not indexed | `rust:LeaseStore::renew` |
| `vlite::kv::LeaseStore::revoke` | None: inherent method, not indexed | `rust:LeaseStore::revoke` |
| `vlite::kv::LeaseAcquireRequest` | `structs vlite::kv::LeaseAcquireRequest` | `rust:LeaseAcquireRequest` |
| `vlite::kv::LeaseRenewRequest` | `structs vlite::kv::LeaseRenewRequest` | `rust:LeaseRenewRequest` |
| `vlite::kv::LeaseRevokeRequest` | `structs vlite::kv::LeaseRevokeRequest` | `rust:LeaseRevokeRequest` |
| `vlite::kv::LeaseGrant` | `structs vlite::kv::LeaseGrant` | `rust:LeaseGrant` |
| `vlite::kv::LeaseError` | `enums vlite::kv::LeaseError` | `rust:LeaseError` |
| `vlite::maintenance::enable_lease_ledger` | `functions vlite::maintenance::enable_lease_ledger` | `rust:fn:enable_lease_ledger` |

Fixture kinds follow neighbouring `enums`, `functions`, and `structs` entries
(`tests/fixtures/api_surface/vlite-default.txt:44-68,72-90,131-134`).
The converter key corrects the earlier proposed
`rust:maintenance::enable_lease_ledger`: the site scanner uses
`rust:fn:<name>` for public free functions (`tests/api_reference.py:252-262`).
Each Phase-2 commit must hand-add its applicable fixture lines and matching
`api.html` rows in the same commit that introduces the item, never later.
The type-only site keys above retain the proposed design identifiers, but
the current `tests/api_reference.py` scanner enforces methods and free
functions, not standalone type declarations, and rejects extra keys.
Document types in rows without unsupported `data-api` attributes; emit the
listed method/free-function keys exactly. Do not change either gate.

Deferred public surface: `lease_domain()` inspection; any runtime
reenrollment/reset operation; `Database::create_with_lease_ledger`; route,
client, CLI, gRPC, MCP, FFI and WASM lease APIs; batch/transaction lease API.
None is needed to implement this ledger slice, and each requires separate
contract/API review.

## 6. Durability, synchronization, and crash model

### 6.1 Write ordering and synchronization

All operations execute under the exclusive database owner lock. Do not run
inside `durable_group` or server group commit: `durable_group` is currently
documented for record put/delete/metadata/batch only, with individual frame
boundaries (`src/lib.rs:2280-2331`); the future lease adapter must not weaken
that invariant.

| Operation | Exact durable order |
|---|---|
| Acquire | Validate mode, input, current state, TTL, and quota without writing. Append `Reserve(token=high_water+1)` stamped with `sequence+1`; `sync_all`; only on success update in-memory high-water. Append `Acquire` at the same auxiliary sequence with the same token/address/holder/deadline; `sync_all`; only on success publish the grant in memory and return it. |
| Renew | Validate matching live grant and `now < deadline`; append `Renew` stamped with `sequence+1`, same token and new checked deadline; `sync_all`; update memory and return only after success. |
| Revoke | Validate matching live grant; append `Revoke(active=false)` stamped with `sequence+1`; `sync_all`; update memory and return `true` only after success. |
| Startup barrier | During writable open, after full replay and before handle exposure, append `StartupBarrier` stamped with `sequence+1`, sync, then mark every replayed grant inactive in memory. |
| Recovery-required stamp | Copy-out writer emits `RecoveryRequired` after its data/image and before validating/publishing the destination; auxiliary sequence is the next ordinary commit sequence and output is not published unless complete and reloaded successfully. Promotion must sync this frame before returning a writable `Database`. |

Lease frames are auxiliary and do not consume ordinary commit sequence
numbers. The loader accepts them only at the next ordinary sequence and
preserves their physical order; the following data frame, if any, consumes
that sequence. This follows the non-consuming time-anchor precedent, while
allowing multiple separately synced lease events to share that position.
Lease frames bypass the periodic flusher even when the handle is in
`Durability::Periodic`; use the existing append-and-sync boundary and
`LockedFile::sync_all` path (`src/lib.rs:2267-2371`). `Durability::Full`
already syncs each ordinary frame. `Durability::None` cannot provide a
file-backed durable state and is rejected; memory-only handles also refuse.
The periodic flusher's `sync_all` and failure-poisoning behavior is described
in `src/durability.rs:91-127`.

If write or sync fails after a lease frame may have reached disk, return
`Storage(Error::CommitIndeterminate(_))`, poison the handle, and serve no more
lease operations. Do not roll back in-memory high-water, truncate the frame,
retry automatically, or tell the caller it failed safely. A later writable
open replays a complete synced reservation and keeps its token burned. If a
failed write leaves an incomplete tail, normal open rejects it; any explicit
tail-repair path on a ledger file must set `RecoveryRequired` before lease use,
because a possibly burned reservation cannot be proven from the truncated
bytes.

### 6.2 Crash points

| Fault point | Recovery invariant |
|---|---|
| Before reservation append | No token was allocated; retry may use `high_water + 1`. |
| During reservation append, before a successful sync | No grant was returned. If frame is complete it replays and burns the token; if the tail is incomplete the file refuses ordinary open and repair sets `RecoveryRequired`. Never infer the allocation from an error. |
| After reservation sync, before grant append | High-water is advanced; token is burned with no grant. Next acquire uses a larger token. |
| During grant append / before its sync | Caller gets indeterminate error; handle is poisoned. Replay may contain the grant, but next open's barrier invalidates it. The reservation cannot be reused. |
| After grant sync, before response | Grant may be live until reopen/deadline; caller must treat ownership as ambiguous and reacquire. Reopen invalidates it and retains high-water. |
| During renew sync | Error is indeterminate; handle is poisoned. Either old or new deadline may replay, but reopen invalidates both. No retry on same handle. |
| During revoke sync | Error is indeterminate; handle is poisoned. Reopen invalidates all grants regardless of whether revoke replayed. |
| During startup barrier append/sync | Do not return an operational handle. A complete barrier replays; incomplete tail refuses open/repair requires recovery. |
| During directory checkpoint manifest/image publication | Existing P2 rule selects an old or new complete manifest/image. The image must contain the same high-water/domain and grant snapshot, and any unverified published outcome remains indeterminate. |
| During restore / promotion publication | Do not expose a writable lease-capable result until a durable `RecoveryRequired` marker is present. If not provable, the operation fails closed and must not serve leases. |

The central invariant is: **a token is returned only after both reservation
and grant are synced; no returned token or successfully synced reservation is
ever reused within the same domain.** This is stronger than returning after
the database's ordinary commit under periodic durability.

## 7. Fence domain and fail-closed recovery

### 7.1 Domain and recovery state

Generate a new domain from OS randomness (`getrandom` is already used by
`format::new_lineage`, `src/format.rs:545-555`). It is 16 octets encoded as a
canonical lowercase UUID string for external representation; set RFC 4122
version-4 and variant bits. It is independent of the existing random `u64`
change-feed lineage. The domain stays constant through normal restart,
compaction, upgrade, encryption-key rotation, directory conversion/checkpoint,
and backup. High-water is likewise carried forward.

| Persisted state | Lease operations | Transition |
|---|---|---|
| `Ready` | Allowed only on an exclusive writable file-backed owner after this open's synced startup barrier. | VLite's writable open path writes the barrier; ordinary lease operations keep state Ready. |
| `RecoveryRequired` | Always fail with `FencingRecoveryRequired`; no acquire, renew, or revoke. | VLite's restore, PITR, supported rollback/copy, or replica-promotion path stamps it. Terminal for this file in this release; no in-place API clears it. |
| `Unavailable` | Fail with `LeaseUnavailable`. | The caller's read-only, shared, replica, memory, or durability-none open mode prevents serving. It is ephemeral and not persisted as a file recovery state. |

`RecoveryRequired` deactivates every grant and persists the old domain and
high-water for diagnosis; no token from it can be issued. Ordinary record/KV
reads and writes remain subject to the existing handle-mode and durability
rules; recovery state is not a blanket database read-only restriction. The supported
operator path is a fresh output/domain plus explicit downstream re-enrollment
before traffic is cut over. There is no automatic token-floor recovery,
runtime admin endpoint, route, or in-place reset in this slice.

### 7.2 VLite-managed transition paths

| Event | Detection / stamping point | Result |
|---|---|---|
| Ledger-enabled backup | `maintenance::backup_inner` knows the loaded flag; after copying source bytes, append/sync a `RecoveryRequired` frame to the staged destination and reload it before publish. | Archive retains high-water/domain but cannot serve leases. Non-ledger backups remain byte-identical. |
| `maintenance::restore` | The restore writer already rewrites lineage at `src/maintenance.rs:397-450`. For ledger files, append `RecoveryRequired` under the copied ledger domain before staged output validation/publication. | Restored output refuses leases. |
| `logops::restore_to` (PITR) | It already emits a fresh lineage and copies only frames through the cut (`src/logops.rs:99-185`). When source has ledger flag, append `RecoveryRequired` after the cut; it must not use a pre-cut high-water as authoritative. | PITR output refuses leases. |
| Directory checkpoint image restore / manifest rollback through VLite | A supported restore/conversion path must stamp `RecoveryRequired` in the output before manifest publication. A normal checkpoint carries ledger state and is not itself a restore. `src/directory.rs:733-760,974-1014`. | Restored output refuses leases. |
| VLite copy/branch operation | No generic branch API exists in this slice. Any future VLite-managed copy-out that creates an independently writable descendant must stamp `RecoveryRequired`; ledger-enabled backups also stamp, while non-ledger backups remain byte-identical. | Managed descendant refuses leases until fresh-domain operator procedure. |
| Rollback through a supported VLite operation | Any operation that selects an earlier log/image must stamp `RecoveryRequired`; explicit tail repair of an incomplete lease tail does likewise. | Rolled-back output refuses leases. |
| `Replica::promote` | `src/replica.rs:194-198` currently consumes the read-only follower directly. Before return, append and sync `RecoveryRequired`; failure returns error and must not expose the database as writable. | Promoted file refuses leases; replication lag can never be treated as a safe global floor. |

The change-feed lineage is not a fence domain: lineage changes on rewrites and
restore, serves cursor continuity, is only 64 bits, and cannot prove that a
copied ledger is newer (`src/changes.rs:3-6`; `src/maintenance.rs:347`,
`src/logops.rs:167-168`). Changing lineage alone therefore never clears or
proves fencing recovery.

**Recorded guarantee scope (2026-11-02).** A raw filesystem copy, external
rollback, or restoring an old directory manifest without calling a VLite
operation is indistinguishable from the original bytes with only an
in-file ledger. The accepted design stamps VLite-managed ledger backups
`RecoveryRequired`, preserving byte identity only for non-ledger backups.
Detecting arbitrary copies/rollbacks while
preserving the same domain still requires an external non-rollback floor,
which decision (c) explicitly rejected. The owner-visible wording change
against contract section 5's "copy/branch/rollback fails closed" is:
"The fencing guarantee covers VLite-managed operations only; unmanaged
out-of-band filesystem copies and rollbacks are not claimed." The coordinator
integrates this cross-reference; this slice does not edit the contract.

## 8. Maintenance and lifecycle carry-forward

`Carry` means retain the same domain/high-water and every grant row in output;
the first writable open still invalidates grants via `StartupBarrier`.
`Stamp` means retain the ledger but mark `RecoveryRequired`. In all cases a
rewrite must reload and validate the finished output before publication.

| Path | State action | Code ownership point | Required proof |
|---|---|---|---|
| Compaction | Carry ledger state in sorted `DomainInit` + `SnapshotGrant` op-18 frames after the compact image end; assign normal contiguous sequence numbers after image floor. | `maintenance::write_v5` / `write_v5_image` (`src/maintenance.rs:901+`) | `compaction_carries_domain_high_water_and_grants` |
| Upgrade | Carry on supported V5 rewrites; pre-V5 input cannot already have the flag. Never silently clear a ledger bit. | `maintenance::upgrade`, `write_v5` (`src/maintenance.rs:545-610`) | `upgrade_preserves_ledger_snapshot` |
| Re-encryption | Carry domain/high-water/grants; each op-18 frame is verified plaintext then resealed by the existing frame path; do not strip the flag. | `logops::reencrypt` (`src/logops.rs:25-70`) | `reencrypt_preserves_encrypted_ledger` |
| `convert_to_directory` | Carry into checkpoint image, preserve same lineage/domain; add ledger bit to `directory::WAL_FLAGS`. | `maintenance::convert_inner`; `directory::create_converted` and `Layout::from_first` (`src/maintenance.rs:219-256`; `src/directory.rs:31-37,974-1014`) | `directory_conversion_carries_ledger` |
| Directory checkpoint | Carry snapshot in image as op-18 auxiliary frames at `floor+1`; neither loaded commit sequence nor `history_floor` advances. | `Database::start_checkpoint`, `Chain::rotate/install`, `directory::build_image` (`src/lib.rs:1713-1810`; `src/directory.rs:602-650,733-760,827-850`) | `checkpoint_preserves_ledger_and_sequence` |
| Backup | Carry domain/high-water/grant evidence, then stamp `RecoveryRequired` on the ledger-enabled archive so opening the copy cannot create a second active allocator. Non-ledger backup bytes stay unchanged. | `maintenance::backup_inner` (`src/maintenance.rs:371-388`) | `ledger_backup_stamps_recovery_required`; `legacy_backup_remains_byte_identical` |
| Restore / PITR | Stamp terminal `RecoveryRequired`; keep the carried domain/high-water for evidence. | `maintenance::restore_inner`; `logops::restore_to` (`src/maintenance.rs:411-450`; `src/logops.rs:99-185`) | `restore_and_pitr_fail_closed` |
| Collection drop/recreate | No ledger write. Collection IDs are never reused within a lineage; the distinct physical ID remains in historical ledger rows. | Existing `Database::drop_collection` path; no call into ledger module | `drop_recreate_does_not_reset_token_or_domain` |
| Replica shipping | Exporter buffers auxiliary op18 frames and ships them with the next ordinary commit; follower replays them but cannot serve leases. Trailing auxiliary-only state is not represented as a data-sequence advance; promotion fails closed. | `read_segment`, `replica::replica_extend` (`src/logops.rs:342-420`; `src/lib.rs:2457-2508`; `src/replica.rs:140-180`) | `replica_replays_ledger_but_refuses_leases` |
| Change feed | No lease event is emitted. A reader that sees op18 refuses; it must not skip the frame and advance its cursor as if no state were present. Existing unknown-operation match returns `Corrupt` (`src/changes.rs:301-468`). Older executables refuse earlier on the unknown header bit. | No `src/changes.rs` ownership in this slice; retain fail-closed default | `change_feed_refuses_lease_frame` |

The checkpoint snapshot is emitted after the compact image's end block at
sequence `floor + 1`. `compact::ImageCheck` validates and closes the image
when it sees the first post-floor frame (`src/compact/mod.rs:759-815,
1629-1635`); op 18 is then an auxiliary ledger frame, not an unknown compact
catalog entry. This avoids editing `src/compact/**`. Since op 18 does not
consume the sequence, directory image validation still expects
`Loaded.sequence == floor`, and the first subsequent ordinary commit is
`floor + 1`.

The change-feed scanner and log-segment exporter must not mistake the
auxiliary sequence for an ordinary commit. The feed keeps its existing
fail-closed unknown-op behavior. Segment export buffers op-18 frames and
includes them only together with the following ordinary commit at that same
sequence; it never splits an auxiliary batch at the `max_bytes` boundary and
never reports an op-18-only tail as a committed advance. Thus a follower
replays lease state before the associated user commit. If no later user commit
exists, the follower may lack the tail ledger events; promotion is still
stamped `RecoveryRequired`, so stale follower state can never serve leases.

No record, KV, import, or collection-drop code writes op 18 or mutates ledger
state. Enforce this structurally:

1. Keep append/write helpers in a new private `src/lease_ledger.rs`; do not
   expose a generic "append ledger event" helper from `src/kv.rs`.
2. Have only `LeaseStore` methods and named maintenance/recovery hooks call
   those helpers. Existing `Database::apply`, record `commit_frame`, KV
   put/delete, import commit, and collection create/drop remain unchanged
   except for normal sequence/replay support.
3. Make writer constructors for op 18 private to the ledger module. Add a
   fail-first mutation test that temporarily routes a record/KV/import/drop
   operation through the ledger writer and proves the ownership assertion
   fails.
4. Require the exact `FLAG_LEASE_LEDGER` check for all lease operations; a
   missing flag is unavailable, never "initialize on first call."

## 9. Test and mutation acceptance

No code tests run in Phase 1. The Phase-2 suite is required to include the
following independently accepted tests. The state-machine oracle is hand-built
from the contract and deterministic traces; it does not call Rust's writer or
decoder to produce expected state.

| Test group | Named tests and assertions |
|---|---|
| Independent model | `lease_oracle_traces_match_contract`: independent maps keyed by `(physical collection, namespace bytes, key bytes)`, high-water, domain, recovery state, injected clock, and downstream highest-token state. Assert results/errors and relational monotonicity; never assert allocator output by asking the writer. |
| Golden bytes | `lease_ledger_golden_v1`: exact DomainInit, Reserve, Acquire, Renew, Revoke, StartupBarrier, RecoveryRequired and SnapshotGrant bytes; known CRC/footer and encrypted-frame fixtures. |
| Bounds / malformed | `lease_frame_rejects_bad_prefix_and_body`: op 18 without flag, bad reserved bytes, unknown kind/flags/version, length overflow, duplicate/out-of-order snapshot rows, invalid UTF-8/domain, zero token, bad deadline, and 2,049-byte body. |
| Truncation | `lease_frame_truncation_is_not_a_commit`: truncate at every prefix/body/footer boundary; no partial frame is applied. Explicit tail repair on a flagged file makes lease use fail with recovery required. |
| Older reader | `lease_flag_uses_existing_unsupported_feature_path`: run parser fixture without `FLAG_LEASE_LEDGER` support; expect exactly `UnsupportedFeature(0x0080)` before replay or repair. |
| REG-001 / site surface | In each public-item-introducing Phase-2 commit, hand-add the exact section 5.4 fixture lines and site rows; run `python scripts/api_surface.py check` and `python tests/api_reference.py`. Prove removing a required fixture line fails REG-001 and removing a required method/free-function key fails site coverage. Methods have no REG-001 fixture entries; never regenerate the baseline. |
| Crash injection | `lease_crash_points_never_reuse_token`: inject deterministic failures before append, after append/before sync, after reserve sync, after grant append, after grant sync/before response, renew/revoke sync, and startup barrier sync. Use the existing append/sync injection seam or a test-only sync closure; no sleeps. |
| Encryption | `encrypted_lease_frames_round_trip_and_authenticate`: preserve ledger under encryption, fail modified ciphertext/tag, ensure holder/key fields are not plaintext on disk. |
| Maintenance | `compaction_carries_domain_high_water_and_grants`, `upgrade_preserves_ledger_snapshot`, `reencrypt_preserves_encrypted_ledger`, `directory_conversion_carries_ledger`, `checkpoint_preserves_ledger_and_sequence`, `ledger_backup_stamps_recovery_required`, `legacy_backup_remains_byte_identical`, `restore_and_pitr_fail_closed`. |
| Collection / replica / feed | `drop_recreate_does_not_reset_token_or_domain`, `replica_replays_ledger_but_refuses_leases`, `promotion_is_recovery_required`, `change_feed_refuses_lease_frame`. |
| Non-owners | `record_kv_import_drop_never_write_ledger`: perform every existing record/KV/import/drop path and compare decoded ledger bytes and state to the independent pre-operation snapshot. |
| API / sync | `lease_writes_sync_in_periodic_mode`, `lease_writes_refuse_none_readonly_replica_shared`, `failed_lease_sync_poisons_handle`; assert no result is returned before successful sync. |

Required fail-first mutation list (each mutation must fail at least one named
assertion):

| Mutation | Test that must fail |
|---|---|
| Replace `max(wall,last)` with wall time | `lease_oracle_traces_match_contract` clock rollback trace |
| Use unchecked deadline addition or permit TTL zero/over-max | `lease_oracle_traces_match_contract` and `lease_frame_rejects_bad_prefix_and_body` |
| Increment token after grant rather than syncing reservation first | `lease_crash_points_never_reuse_token` |
| Reuse token after restart or discard high-water on revoke | `lease_crash_points_never_reuse_token` |
| Publish acquire result before grant sync | `lease_writes_sync_in_periodic_mode` and crash-after-grant-sync trace |
| Automatically retry acquire/renew/revoke after an indeterminate result | `lease_crash_points_never_reuse_token` and `lease_oracle_traces_match_contract` ambiguous-result trace |
| Let renew allocate a new token | `lease_oracle_traces_match_contract` renew trace |
| Omit holder/domain/token comparison | `lease_oracle_traces_match_contract` cross-holder/domain trace |
| Let expired or revoked grants renew/revoke successfully | `lease_oracle_traces_match_contract` deadline/revoke trace |
| Treat same hash as full key identity | `lease_oracle_traces_match_contract` collision-address fixture |
| Accept duplicate/unsorted snapshot grant entries | `lease_frame_rejects_bad_prefix_and_body` |
| Skip unknown op 18 or clear its flag during rewrite | `older_reader`, `change_feed_refuses_lease_frame`, or maintenance carry test |
| Reset domain or high-water on compact/upgrade/reencrypt/checkpoint | corresponding maintenance test |
| Let a restore or promotion leave state Ready | `restore_and_pitr_fail_closed` / `promotion_is_recovery_required` |
| Apply recovery barrier only after serving begins | `lease_crash_points_never_reuse_token` startup barrier fault |
| Route lease writes through periodic flusher/group commit only | `lease_writes_sync_in_periodic_mode` |
| Allow ordinary record/KV/import/drop to append op 18 | `record_kv_import_drop_never_write_ledger` |

## 10. Phase-2 commits and shared-file serialization

Phase 2 remains gated, including isolated codec/test work. Once separately
approved, commits remain local and independently acceptable. Shared storage
integration serializes behind both P3b and P4; do not parallelize edits to
their shared loader/publication files or race the TTL feed owner.

| Order | Commit boundary | Files / functions | Dependency after separate Phase-2 approval |
|---:|---|---|---|
| 1 | Private codec and independent state oracle, no production hooks | New `src/lease_ledger.rs` types/codec/oracle tests; test-gated until capability is wired. | Isolated work only after separate Phase-2 approval; this design acceptance authorizes no early code. |
| 2 | V5 capability and replay integration | `src/format.rs`: flag registry in `read_log_header_with`; `frame_allowed`, `checked_body_len`, `load_inner`, `Loaded`, op-18 replay and frame constructor. | Wait until P3's `src/format.rs` loader/generation change is merged or coordinate a single serialized integration commit. |
| 3 | Database mode, durable hooks, opt-in API | `src/lib.rs`: `Database` ledger state, writable-open barrier sequencing around `open_backed`/`open_inner`, read-only and memory checks, dedicated append+sync path adjacent to `commit_frame_with_sync`; `src/kv.rs`: `LeaseStore` methods and public API. | Wait for P3 `src/lib.rs` generation integration; do not change unrelated hooks. |
| 4 | Offline converter and compacted output | `src/maintenance.rs`: enablement converter, restore stamping, `write_v5`/`write_v5_image` snapshot; `src/logops.rs`: `reencrypt` and PITR carry/stamp. | Wait for P3 loader/image writer contract before writing op-18 post-image state. |
| 5 | Directory, recovery, replica | `src/directory.rs`: `WAL_FLAGS`, `Layout::from_first`, `build_image`, `create_converted`, checkpoint sequence verification; `src/replica.rs`: apply capability and promotion barrier. | Wait for P3 `src/directory.rs` generation/checkpoint changes. |
| 6 | Full acceptance | ledger integration tests, targeted maintenance/replica tests, then contract/roadmap status cross-links by coordinator. Public fixture lines and site rows must already accompany their introducing commits 3-4 (section 5.4). | Run only after commits 2-5; no production enablement without independent acceptance. |

**Serialization contract for shared ownership:**

Every shared-file row below waits for both P3b and P4 in that conflict domain,
in addition to the narrower function dependencies listed.

| Shared surface | Required order |
|---|---|
| `src/format.rs` | P3 loader/generation work first; then add the capability/op in its same frame-validation/replay pipeline. Rebase and resolve once, never parallel-edit the same match arms. |
| `src/lib.rs` | P3 `Loaded`/generation/commit integration first; then thread ledger state through `open_backed`, writer/open mode, and dedicated synchronous append. Do not change `durable_group` semantics. |
| `src/directory.rs` | P3 generation/checkpoint writer first; then add ledger flag to WAL mask and serialize post-image ledger frames without changing compact catalog order. |
| `src/maintenance.rs` | Coordinate after format/API integration; do not let an existing writer mask away the new flag or publish without reloading. |
| `src/logops.rs` | Add preservation/stamping after op-18 frame verification and loader support; never copy only the header bit while dropping frames. |
| `src/replica.rs` | Permit replication replay of the flag/op but keep follower lease API unavailable; stamp promotion before returning writable state. |
| `src/changes.rs` | This slice does not edit it. The TTL-feed owner must be told op 18 is internal state and that its current default arm fails closed; do not silently skip it. The lease slice must not race the TTL feed edit. If the owner later decides feeds must continue across lease frames, that is a separately reviewed, explicitly owned change. |
| `src/compact/**` | No change. Place checkpoint/compact ledger snapshot frames after the compact image end at sequence greater than its floor, so `ImageCheck` closes the image before ordinary op-18 replay. |

The contract row `serving-kv-ttl-contract.md:1014` already assigns these
implementation files and excludes `src/compact/**`, `src/changes.rs`, and
server surfaces. This plan preserves that boundary.

## 11. Contract disagreements and open decisions

### 11.1 Code facts that differ from the planned feature

| Existing fact | Design consequence |
|---|---|
| `src/kv.rs:1-17,69-100` stores KV in normal records; no lease lifecycle or allocator exists. | Add a separate module and WAL semantics; do not reuse revisions or record APIs. |
| V5 open validates/replays before `Database` is returned; read-only sets `read_only` after `open_backed` (`src/lib.rs:1441-1510`). | Split or defer startup-barrier work by open mode so a read-only/replica handle never writes during shared parsing. |
| `commit_frame` syncs ordinary commits only for Full/non-flusher modes; Periodic marks writes for the flusher (`src/lib.rs:2267-2371`). | Op 18 requires an explicit always-sync append path; do not inherit ordinary periodic behavior. |
| `FLAG_LEASE_LEDGER=0x0080` and op 18 are not currently registered. | Older builds will reject the unknown flag through the existing `UnsupportedFeature` path; new builds add prefix/replay support. |
| `directory::WAL_FLAGS` omits any lease bit; checkpoint output carries only selected flags and currently expects `check.sequence == floor` (`src/directory.rs:31-37,827-850,974-1014`). | Add the bit and allow the sequence tail from post-image ledger snapshots while retaining the original history floor. |
| `maintenance::compact`, `upgrade`, `restore` and `logops` rewrite flags/lineage and currently do not know a ledger (`src/maintenance.rs:397-610`; `src/logops.rs:25-185`). | Every writer must preserve frames and explicitly stamp restoration, not copy a flag alone. |
| `Replica::promote` currently returns the follower `Database` directly (`src/replica.rs:194-198`). | A fallible sync-before-return promotion is a source-compatibility change requiring explicit owner approval before Phase 2; no signature changes yet. |
| `changes.rs` has no op-18 branch; unhandled operations return `Corrupt` (`src/changes.rs:301-468`). | Existing behavior is fail-closed, not silent skip. A future change-feed support decision must be coordinated with the separate feed owner. |
| Raw filesystem copy/rollback has no in-file signal and no external floor is approved. | The claim can only cover VLite-managed operations unless decision (c) changes. |

### 11.2 Recorded answers and remaining prerequisites

| Question | Recorded answer / prerequisite |
|---|---|
| Does the numeric max of 4,096 distinct grant rows per file and holder bound 255 bytes fit the serving contract's quota model? | Accepted as proposals; confirmation against future tenant quotas remains pending. |
| Should a ledger-enabled change feed continue past internal lease commits? | No in this slice: keep fail-closed behavior and do not edit `changes.rs`. |
| Does "copy/branch/rollback" mean only VLite-managed operations, or arbitrary filesystem actions? | Accepted scope is VLite-managed paths only; unmanaged copies/rollbacks are explicitly not claimed. |
| May `maintenance::backup` stop being byte-identical only for opted-in ledger files, so the backup is stamped `RecoveryRequired`? | Accepted at design level; preserve byte identity for every non-ledger file and stamp the opted-in archive. |
| Is a read-only inspection method for `fence_domain` needed before serving? | Defer; the `LeaseGrant` returns the domain on successful acquire, and no public inspection surface is necessary for the ledger slice. |
| Should writable `Database::open` itself fail when recovery state is required, or may ordinary records remain writable while lease operations refuse? | Keep ordinary database behavior unchanged; only lease operations fail with `FencingRecoveryRequired`, and the restored handle cannot serve leases. |
| Is the public Rust holder argument acceptable, or should the embedded API use a single local owner identity? | Keep explicit `holder` in request types; it is required to test the contract's principal-bound semantics and is supplied by the serving adapter later. |
| Should the public `Replica::promote` signature change from infallible to `Result<Database>`? | Explicit owner approval of this source-compatibility change remains a Phase-2 prerequisite; design acceptance does not approve the signature change. |
| How is downstream re-enrollment acknowledged after `RecoveryRequired`? | No in-place/runtime recovery API in this release. Create a fresh ledger-enabled destination/domain and explicitly re-enroll downstream resources before cutover; keep restored output blocked. |

## 12. Proposed status text for integration

Do not edit either roadmap document in this slice. The coordinator may
integrate these updated status sentences following design acceptance.

**`serving-kv-ttl-contract.md` section 12, immediately after the `gap2-rust-lease-ledger` row:**

> **Design status.** The `gap2-rust-lease-ledger` design is recorded in
> [gap2-lease-ledger.md](gap2-lease-ledger.md); design accepted 2026-11-02;
> Phase 2 gated behind separate approval and P3b/P4 serialization. Its format choice is V5
> capability flag `0x0080` plus op 18, with an explicit copy-out converter,
> always-synced lease writes, and fail-closed VLite-managed recovery.

**`next-major.md`, in `Serving design` after the KV and TTL contract status:**

> The [Gap 2 lease-ledger design](gap2-lease-ledger.md) is a docs-only
> storage-format proposal for `gap2-rust-lease-ledger`; it is not implemented
> or active, and no existing file is opted in by default.
