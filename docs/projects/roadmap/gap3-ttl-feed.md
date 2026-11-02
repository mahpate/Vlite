# Gap 3: Rust TTL-aware graph feed

Status: **design accepted 2026-11-02; implemented in Rust (unreleased), phase 2**
(see section 13). Not activated, exposed by server/bindings/CLI, or released.
Slice: `gap3-rust-feed`, phase 1; baseline
`49c77d6f8f2ed87d85b5a570e74457af248bcd45`.
All source anchors below refer to that baseline, not a future implementation.
Owner approval of the exact API and decisions in section 12 precedes code.

## 1. Scope and invariants

| Requirement | Contract anchor | Design obligation |
|---|---|---|
| Explicit feature set; legacy bytes unchanged | [TTL contract](serving-kv-ttl-contract.md), lines 411-432 | Separate opt-in result and tracker projection; do not extend closed legacy enums |
| Own deadlines on updates, effective deadlines on expiry deletes | TTL contract, lines 451-466 | Section 5 mappings |
| Logged reclaim only, deterministic ordering, attachment survival | TTL contract, lines 467-496; attachment invariant at 402-409 and feed acceptance at 985 | Sections 6-7 |
| Resumable bounded reclaim chunks; temporal commits whole | TTL contract, lines 473-485 | Sections 8-9 |
| History preservation or explicit invalidation | TTL contract, lines 487-492 | Section 10, with checkpoint qualification in section 12 |
| Independent oracle and mutation acceptance | TTL contract, feed acceptance at line 985 and mutation list at lines 989-999 | Section 11 |
| One cursor codec and feature set | [Wire conventions](serving-wire-conventions.md), lines 190-200, decisions 8-9 at 271-272 | Section 4; decisions ratified at design level |

Non-goals: serving, MCP, CLI, bindings, gRPC, webhooks, OpenAPI, proto,
TTL write semantics, storage formats, gate/default changes, activation,
performance/size claims, benchmarks, lease work and temporal implementation.
No new top-level wire event or graph operation name. No reader clock.
Phase 2 preserves existing tests unchanged and adds independent acceptance.
The response-cap interpretation and checkpoint issue below are not silently
resolved by changing policy or a storage writer.

## 2. Additive public surface

Names below are exact proposals. Existing `FeedCursor`, `Change`,
`ChangeKind`, `ChangeBatch`, `GraphChange`, their fields and methods remain
source-compatible. In particular, adding fields to `GraphChange` would
break exhaustive construction/matching (`src/property_graph/change.rs:7-73`);
`TemporalChange` begins at `:75` and has its own non-exhaustive declaration.
The new wrapper carries annotations beside the unchanged legacy operation.

```rust
// src/changes.rs; all types reachable as vlite::changes::<Name>.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GraphEventSet { Empty, Ttl }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GraphFeedCursor {
    position: FeedCursor,
    ordinal: Option<u32>,
    graph_events: GraphEventSet,
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct GraphFeedChange {
    pub change: Change,
    pub ttl: Vec<Option<GraphTtl>>,
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct GraphFeedBatch {
    pub changes: Vec<GraphFeedChange>,
    pub next: GraphFeedCursor,
    pub has_more: bool,
    pub head: u64,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum GraphFeedError {
    Engine(Error),
    CursorModeMismatch {
        cursor: GraphEventSet,
        requested: GraphEventSet,
    },
    CursorBeforeHistory { cursor: u64, floor: u64 },
    ResponseTooLarge { bytes: usize, limit: usize },
}

impl GraphFeedCursor {
    pub const fn beginning(graph_events: GraphEventSet) -> Self;
    pub const fn at_commit(position: FeedCursor, graph_events: GraphEventSet) -> Self;
    pub const fn position(&self) -> FeedCursor;
    pub const fn ordinal(&self) -> Option<u32>;
    pub const fn graph_events(&self) -> GraphEventSet;
    pub fn from_token(token: &str) -> Result<Self>;
    pub fn to_token(&self) -> String;
}

pub fn read_log_graph_changes(
    path: impl AsRef<Path>,
    key: Option<&EncryptionKey>,
    cursor: &GraphFeedCursor,
    graph_events: GraphEventSet,
    max_events: usize,
) -> std::result::Result<GraphFeedBatch, GraphFeedError>;

// src/property_graph/ttl_feed.rs: impl Database (not src/lib.rs).
impl Database {
    pub fn graph_changes_after(
        &self,
        cursor: &crate::changes::GraphFeedCursor,
        graph_events: crate::changes::GraphEventSet,
        max_events: usize,
    ) -> std::result::Result<
        crate::changes::GraphFeedBatch, crate::changes::GraphFeedError
    >;
}
```

`Result` in the cursor parser is the existing crate alias, `Result<T, Error>`.
`GraphFeedError` implements `std::fmt::Display::fmt(&self, f:
&mut std::fmt::Formatter<'_>) -> std::fmt::Result`,
`std::error::Error::source(&self) -> Option<&(dyn std::error::Error + 'static)>`
and `From<Error>::from(error: Error) -> Self`; only `Engine` has a source.
Mismatch displays `cursor_mode_mismatch: cursor graph_events {cursor:?} differ from
requested {requested:?}`, naming both sets; size failure displays
`response_too_large: canonical event of {bytes} bytes exceeds {limit} bytes`,
printing the actual size and the limit (16777216).
No public `Error` variant is added; the wrapper is a typed opt-in error.
Later handlers map its mismatch to 409 `cursor_mode_mismatch`, and map
`Engine` with existing rules; they must not inspect error message strings.

`graph_events` selects the exact set, not a boolean or superset negotiation.
`Empty` delegates to the legacy reader and wraps each change with empty `ttl`;
`Ttl` selects the new tracker. Check mismatch before scanning, including at
sequence zero. Conversion by `at_commit` is explicit, never automatic.
The same API rejects temporal entries with existing
`UnsupportedFeature(FLAG_TEMPORAL)` until the temporal slice is accepted;
it must not silently omit them. Future `Temporal` and `TtlTemporal` set
variants are additive because `GraphEventSet` is non-exhaustive.
The existing temporal method retains its present refusal gate
(`src/property_graph/temporal.rs:239-275,278-289`).

Implementation placement: cursor codec, paging, closed-log function and
error type in `changes.rs`; TTL annotation declarations in
`property_graph/change.rs`, re-exported through `property_graph/mod.rs` and
`changes.rs`; `mod ttl_feed;` in `property_graph/mod.rs`. Its `impl Database`
uses the same lineage check, `sync`, scan mutex and known cipher as
`temporal_changes_after`. Descendant module access suffices: **no lib.rs
hook**, loader or manifest change is proposed. Closed-log reading uses the
same reader, with `File::open` for a file and existing `directory::open`
read-shared chain access for a directory, reporting compact skips once.
The legacy closed-log function remains untouched
(`changes.rs:519-531`; the former `:492-504` range is the tail of `scan`).
`export_log` and `ReplicaFollower` remain physical-frame consumers, not
event consumers, so need no change (`lib.rs:2432-2444`, `replica.rs:140-162`).

## 3. Complete annotation declarations

```rust
// src/property_graph/change.rs; also re-exported from vlite::changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GraphRemovalReason { Expired, NodeExpired }

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GraphTtl {
    Updated { expires_at_ms: Option<u64>, expiry_changed: bool },
    ExplicitRemoval,
    ExpiryRemoval {
        removal_reason: GraphRemovalReason,
        expires_at_ms: u64,
        removed_at_ms: u64,
    },
}
```

These are all new variants and fields; no methods, constants or type aliases
besides section 2 are added. `GraphFeedCursor` fields stay private to prevent
invalid split positions. New public result structs are non-exhaustive;
consumers read fields instead of constructing them exhaustively.
For a TTL graph batch, `ttl.len() == operations.len()`; each index annotates
exactly that operation. Non-graph changes and all empty-set changes have
`ttl.is_empty()`. Attached, detached and schema operations have `None`;
node/edge updates and deletes have `Some`. A future temporal projection
must preserve this alignment; the current slice refuses temporal entries.
The constructor/encoder validates alignment and applicability, never zip-
truncates or synthesizes missing annotations.

## 4. Cursor grammar and source compatibility

| Token | Exact emitted grammar |
|---|---|
| Empty set | `vlc1.<lineage:016x>.<sequence:016x>.<crc:08x>` (unchanged) |
| Nonempty set | `vlc2.<lineage:016x>.<sequence:016x>.<ordinal:08x>.<set>.<crc:08x>` |
| Ordinal | `ffffffff` means entire sequence consumed; otherwise last delivered zero-based event ordinal, maximum `fffffffe` |
| Set | `ttl` now; reserved future spellings `temporal`, `ttl+temporal`, fixed registry order, no duplicates |
| Checksum | Existing `format::crc32` of ASCII body before final dot; lower-case zero-padded hex, not authentication |

`None` ordinal means commit boundary; `Some(n)` means events through n of
that commit have been consumed. `position()` alone is deliberately lossy
inside a commit: its docs must warn not to use it as a legacy resume cursor.
Zero lineage requires zero sequence and `None`; ordinal-bearing positions
require nonzero lineage and sequence. New v2 parser requires exact widths,
lowercase spelling and canonical set; trailing parts, overflow, unknown set,
bad checksum or impossible position return `InvalidCursor`. Do not tighten
the historically permissive v1 parser (`changes.rs:50-81`).
`GraphFeedCursor::from_token` accepts both versions: v1 means `Empty` and
commit boundary, v2 means the encoded nonempty set. `to_token` for `Empty`
delegates directly to `FeedCursor::to_token`, producing identical v1 bytes.
Do not silently rebind a parsed v1 cursor to TTL or vice versa.

Accepted ruling: the wire-level "FeedCursor owns both tokens" does not mean
changing the existing `FeedCursor::from_token -> Result<FeedCursor>` to return
a lossless split position while retaining `FeedCursor { lineage, sequence }`.
Keep that type and parser v1-only; the additive `GraphFeedCursor` owns the
lossless v1/v2 parser and encoder. The legacy parser rejects v2, never drops
its ordinal/set. No global side table, encoded sequence bits or hidden token
state is allowed.

## 5. Exact Rust-to-wire field mapping

Existing operation payload and names come from the JSON-feature-gated
`GraphChange::to_json` (`changes.rs:532-610`), with source/destination naming
unchanged.
`Deadline` below means JSON null or a canonical decimal string, never a number.
The wrapper is internal Rust structure, not an additional JSON envelope.

| Operation / origin | `ttl[index]` | Added wire fields in TTL projection |
|---|---|---|
| Node/edge upsert, ordinary patch | `Updated { own, false }` | `expires_at_ms: Deadline`, `expiry_changed:false` |
| `NodeExpiry` | `Updated { logged Option, true }` on otherwise-empty `NodePatched` | Empty label/set/unset arrays/maps; `expiry_changed:true`, required `expires_at_ms`, including null |
| `EdgeExpiry` | Same on otherwise-empty `EdgePatched` | Empty set/unset; same required fields |
| Explicit node/edge delete | `ExplicitRemoval` | `removal_reason:"explicit"`; OMIT deadline and removal clock |
| Reclaim node delete | `ExpiryRemoval { Expired, own, at_ms }` | `removal_reason:"expired"`, both times decimal strings |
| Reclaim edge delete | `ExpiryRemoval { reason, effective, at_ms }` | `removal_reason:"expired"` or `"node_expired"`, both times decimal strings |
| Attached/detached/schema | `None` | No added fields, especially no node deadline on detach |
| Vector put | No annotations | Existing `ChangeKind::Put.expires_at_ms` unchanged; graph-frame puts remain `None` |

Here `own` in the table denotes `expires_at_ms: Option<u64>` in `Updated`.
New entities start with `None`; following deadline entries emit separate
patches even in the same commit. Replacing an existing entity preserves its
deadline, as engine apply does. Set followed by immediate reclaim emits the
deadline patch before deletes; clearing emits explicit null, not omission.
An attached record survives reclaim unchanged and never inherits node TTL
(`feed.rs:107-130`, `state.rs:511-530`).
Empty-set decoding/encoding retains today's omission of TTL entries and
reclaims, including its legacy tracker behaviour, not just similar values.

## 6. Tracker state and replay

Use a distinct TTL projection behind shared validated framing; never repair
the legacy projection in a way that changes legacy output on later entries.
No reader-clock argument, `SystemTime` call, visibility filter or lazy expiry.

| Per-graph retained state | Replay rule / invariant |
|---|---|
| Name, node ID -> key and optional deadline | Put preserves existing deadline; new ID starts null; expiry overwrites including clear; delete removes |
| Edge ID -> optional key, source ID, destination ID, optional own deadline | Validate endpoints exist; put allocates a fresh ID; patches retain deadline; remove all state on delete |
| `(node ID, collection ID)` -> record ID | Validate node/collection, uniqueness and attachment ownership; detach before forgetting node key |
| Node/edge allocator high-water | Advance on new IDs and track monotone values on `GraphHighWater`; validate against observed IDs, emit nothing |
| Collection ID -> name/dimensions | Existing scanner map; missing referenced collection is `Corrupt`, not empty-string substitution |
| Graph ID lifecycle | Create initializes, drop requires valid empty graph then discards all state, including deadlines/high-water |

Memory is proportional to live unreclaimed nodes, edges, attachments and
their key/name bytes plus graph/collection counts, not history length.
No vectors, properties or adjacency index need be retained. Due scanning may
inspect every live edge; no performance claim. Reclaim output may be large;
emit chunks incrementally, not a permanently retained expansion; one decoded
frame and returned page are additional transient memory. There is no new
hard memory quota or eviction that could omit a committed event.

Initialization replays from log start even when the requested position is
later. Op-15 snapshots may span frames; their `NodeExpiry`/`EdgeExpiry`
entries directly follow entities (`state/snapshot.rs:264-283`); hydrate them
without reporting history at/below the floor. In the scanner, op-15 is
handled with op-12 at `changes.rs:431-436`; record blocks are skipped at
`:439`, compact blocks are accepted only under `check.is_some()` at
`:442-471`, with the `Supplement::GraphSnapshot` expansion compiled only for
tests at `:443-470`; unknown operations fail at `:472`. Use existing
snapshot/image validation, including closing `GraphHighWater`, rather than
treating op-15 as an ordinary commit. `logops.rs:385-390` performs an
additional temporal-capability decode check when `FLAG_PRECISION` is clear;
that check is not the scanner's general compact-image guard. Apply the same
rule to compact graph supplements when their existing test-only gate permits
them; do not open it.

For logged reclaim clock t, compute N = nodes with own deadline <= t;
E = edges with own deadline <= t UNION edges whose source or target is in N.
This equals `GraphState::due` (`state.rs:394-401`): its deadline index
uses <= (`state.rs:67-72`), then unions both-direction incidence into a set.
Scanning edges and using a set gives the same membership for parallel edges,
self-loops and edges incident to two due nodes, each exactly once.
For each e in E, effective = minimum of present own/source/target deadlines.
If own == effective, reason is `Expired`, including ties; otherwise
`NodeExpired`. The effective deadline must exist and be <= t.
Node removal uses its own deadline. Compute annotations before erasing state.

Reject inconsistent logged references, missing keys/endpoints/attachments,
reused allocated IDs or duplicate keys/owners, high-water decreases, impossible snapshot order,
ordinal overflow and invalid annotation shape with `Error::Corrupt {
offset: frame_start, reason: <static reason> }`, wrapped as `Engine`.
Use checked lookups/arithmetic, never indexing/expect/panic or default keys.
This is not a new whole-log validation policy for the legacy reader.

## 7. Expansion and ordering

At each `Reclaim { at_ms }`, flush preceding ordinary run, snapshot the due
sets and required names, then emit only the following ordered operations.

| Phase | Sort key | State transition |
|---|---|---|
| Edge deletes | Edge ID ascending | Remove E, deadlines and endpoint associations once |
| Detaches | `(node ID, collection name UTF-8 bytes, record ID)` | Remove attachments of N; resolve names before sorting |
| Node deletes | Node ID ascending | Remove N and deadlines after collecting detaches |

This is contract order, not collection numeric-ID order. Distinct logged
reclaims remain distinct runs even if adjacent; an empty reclaim emits no
event and still consumes its logged sequence. Flush at graph lifecycle,
vector and reclaim boundaries; preserve every entry's position. Explicit
edge deletion follows its logged entry. Explicit node deletion requires no
incident edges and silently releases its attachments in engine state
(`state.rs:1080-1093`, `mutate.rs:421-424`): emit the explicit node delete,
clear tracker attachments, and do not invent reclaim detach events.
An explicit logged detach still emits its existing event.
Drop/clear/remove/recreate cannot leave stale tracker entries.

**Observed ordering difference:** engine `due` returns nodes in
`(deadline, node ID)` order, and apply deletes each node with its attachments
(`state.rs:394-401,1249-1259`), not global detach-then-ID-sorted node order.
The engine emits no feed events for that internal order. The design sorts
the feed projection per contract; final state is identical. This difference
needs owner acknowledgement, not an engine mutation or a contract rewrite.

## 8. Deterministic chunk sizing

Choose a **changes.rs-owned, JSON-feature-independent canonical byte
counter** for reclaim events; not a count-only Rust feed or server re-split.
Existing `GraphChange::to_json` is feature-gated and lacks TTL annotations;
there is no complete TTL event encoder today (`changes.rs:532-631`, with
`to_json` at `:532-610` and `ChangeKind::graph_json` at `:612-631`).
Only reclaim deletes/detaches need this counter: no metadata, floating-point
or schema serialization is involved.

Canonical bytes are the compact UTF-8 JSON of the existing event envelope:
`{"change":B,"collection":"","committed_at_ms":T,"ordinal":O,"sequence":"S"}`.
B has keys `graph,operations,revision,type` in that order; revision is string,
type is `"graph_batch"`. T is existing numeric timestamp or null; O is a JSON
integer. S, revision and all graph IDs/times are shortest unsigned decimal.
Operation object keys sort by ASCII name; absent fields remain absent,
edge key can be null, arrays keep order, with exactly one comma between
elements and no whitespace. Escape quote/backslash with `\"`/`\\`, use
`\b,\t,\n,\f,\r` for those controls, `\u00xx` lowercase for remaining
U+0000..U+001F, and raw UTF-8 otherwise (including slash and non-ASCII).
The counter counts these exact bytes, including envelope, keys and commas,
with checked arithmetic. This defines canonicalization, not RFC 8785.

Greedily append the next operation if candidate count <= 256 and canonical
event size <= 1,048,576, recomputing with that chunk's actual event ordinal.
Otherwise close the nonempty chunk and retry the operation in the next.
One operation alone may exceed 1 MiB but not 16,777,216 canonical bytes;
above that return typed `ResponseTooLarge`, without delivering a prefix
or advancing the request cursor. Never emit an empty chunk.
Ordinals count ALL events in the commit, not operations or just reclaims.
Do not merge ordinary runs into reclaim chunks. Non-reclaim runs remain
unsplit; any commit containing temporal entries remains whole, even if it
also contains reclaim. Until temporal support exists it is refused.

`gap3-feed-events` must consume these exact chunks/ordinals, flatten
annotations using section 5, and prove its canonical reclaim JSON equals
the counter with independent literal bytes. It must not split again by
transport or filter; filtered consumers advance over hidden chunks without
renumbering. A transport's actual encoded cap can stop before a whole event
and return a prefix cursor, or reject an indivisible event without advance.
It must not emit an empty continuing page. Section 12 records the REST
cap-policy ambiguity; this slice introduces no global REST response layer.

## 9. Paging semantics

Clamp `max_events` to 1..=10,000 as today. For TTL-only commits, reclaim
chunks are page boundaries; ordinary unsplit units retain soft-limit
behaviour (one unit can exceed the requested event count). To avoid
arbitrary splitting, ordinary events between reclaim runs form indivisible
units, preserving vector/lifecycle order; temporal commits are one unit.
The empty-set scanner retains whole-commit soft paging exactly: its
`changes.len() + events.len() > max_events` check is at
`changes.rs:477-480`, including existing head behaviour.

On resume, rebuild pre-state, deterministically expand the same commit,
skip through the cursor's ordinal inclusive, then deliver later events.
Validate an ordinal identifies an existing event; otherwise `InvalidCursor`.
A split cursor at the last event is valid; normal output normalizes it
to commit boundary when the complete commit was consumed.
An end-of-commit cursor skips the complete sequence. `head` is the last
committed sequence in the captured scan snapshot; `has_more` means another
deliverable event exists after `next` in that snapshot, not an estimate.
Eventless frames advance progress at commit boundaries; no eventless frame
can manufacture a split cursor or cause an unresumable empty page.
No changes leaves a valid input cursor unchanged unless eventless commits
were consumed. A lineage-zero beginning binds to current lineage.
Existing lineage/ahead/upgrade errors remain typed `Engine(Error)`.

## 10. Maintenance and physical paths

| Reader / rewriter and baseline anchor | Opt-in behaviour; phase-2 named proof |
|---|---|
| Open handle / closed file (`lib.rs:2414-2427`, `changes.rs:519-531`) | Shared durable scanner; `ttl_live_closed_parity` |
| Directory chain/checkpoint (`lib.rs:1727-1750`, `directory.rs:826-842`) | Same lineage, floor; reject lost history explicitly; `ttl_checkpoint_floor` |
| Image builder (`maintenance.rs:881-897,1063-1084`) | Hydrate deadlines, never fabricate deletes from dropped entities; `ttl_image_deadlines` |
| Compaction/upgrade (`maintenance.rs:509-567`) | New lineage rejects old cursors; only output's actual logged history is reported; `ttl_rewrite_lineage` |
| Backup/restore/PITR (`maintenance.rs:193-219,395-442`, `logops.rs:94-99,167-168`) | Backup keeps bytes/lineage, restore changes lineage, PITR cuts whole commits; `ttl_backup_restore` |
| Re-encryption (`logops.rs:22-51`) | New lineage, no synthetic deletes; `ttl_encrypted_history` |
| Export/follower (`logops.rs:327-416`, `replica.rs:140-162`) | Ship unchanged authenticated frames, reconstruct identical TTL events; `ttl_replica_export` |
| Compact/precision image checks (`changes.rs:255-263,426-472`, `logops.rs:385-390`) | Retain `ImageCheck` validation/skips and the separate temporal-capability check; `ttl_compact_precision_images` |
| Precision conversion / directory relocation (`compact/catalog/tests/convert.rs:162-205`, `maintenance.rs:208-219`) | Precision conversion's new lineage rejects old cursors; preserving directory relocation uses the floor rule, not fabricated deletes; `ttl_compact_precision_images`, `ttl_checkpoint_floor` |
| `enable_graph` / `strip_graph` (`property_graph/maintain.rs:176-202`, `rewrite` at `:218`; `strip_graph` body at `:202-223`) | Conversion creates new lineage; stripped output has no graph events; `ttl_graph_conversion` |
| Restart | Slice obligation: after reopen, replay only logged reclaim clocks and produce the same resume result; never infer removal from current time; `ttl_restart_no_clock` |

Approved checkpoint rule: old sequence < floor already fails
`CursorBeforeHistory` (`changes.rs:270-279`); a split cursor at sequence
== floor also fails with typed `GraphFeedError::CursorBeforeHistory { cursor, floor }`, because
the image cannot reconstruct its remaining chunks. An end-of-commit cursor
== floor remains valid. This explicitly deviates from the contract's literal
"retained reclaim resume or lineage invalidation": the approved Rust feed
uses the existing history-floor invalidation, without a new lineage or writer
edit. No fictitious deletes from image compaction.
Compact graph supplement handling is currently `cfg(test)`; test acceptance
must distinguish gated fixtures from production support, never activate it.

## 11. Independent acceptance and fail-first plan

Phase-2 commit 1 captures and commits **before production edits** legacy
default-set bytes/tokens for hand-built deadline/reclaim logs, including
subsequent mutations demonstrating today's stale tracker behaviour.
Use literal expected JSON and fixed lineage/clock fixtures, not a golden
regenerated by the changed encoder. No tests/builds run in phase 1.

| Named test | Independent oracle / required cases |
|---|---|
| `ttl_replay_model` | Hand-written test model, no FeedGraphs/engine due helper: generated set/clear/delete/attach/reclaim histories; parallel edges, self-loops, endpoint minimum, ties, zero/u64 max |
| `legacy_feed_bytes` | Pre-edit fixtures for Empty live/closed output, exact v1 bytes, ordinals and whole commits; run existing five feed/maintenance/recovery/replica suites unchanged |
| `api_surface_gate` | `python scripts/api_surface.py check` under stable rustdoc; exact additions listed below, property_graph entries auto-allowed, all changes paths hand-added; never run `baseline` |
| `ttl_operation_goldens` | Literal set/clear/update/delete/detach bytes, explicit omissions vs null, explicit node deletion clears attachments without invented detaches, set-immediate-reclaim, preserved upsert TTL, vectors null, attached record unchanged |
| `ttl_reclaim_order` | Hand-built non-ID deadline order and collection-name-vs-ID order; edges, detaches, nodes each exactly once; repeated reclaims and dropped/recreated graphs |
| `ttl_chunk_boundaries` | Literal independent byte builder: 255/256/257 ops, 1 MiB minus/exact/plus one, UTF-8/escaping, ordinal digit transitions, single oversized op and >16 MiB refusal |
| `ttl_resume_every_chunk` | Concatenation from EVERY boundary equals hand-built full stream, limits 0/1/10000/above maximum, eventless commits, invalid/final ordinal, truthful head/has_more |
| `ttl_cursor_goldens` | Independently calculated CRC v1/v2 literal tokens, Empty/Ttl mismatch BOTH ways, checksummed altered set, corrupt widths/CRC/ordinal/lineage, explicit at_commit conversion |
| `ttl_tracker_corrupt` | Hand-built checksummed invalid references, snapshot ordering, duplicate ownership, decreasing high-water, missing detach target -> Corrupt, no panic |
| `ttl_restart_no_clock` | Fixed bytes with deadlines passed under two reader wall clocks, no logged reclaim -> no removal; appended logged reclaim -> exact logged at_ms |
| `ttl_checkpoint_floor` | Hand-built floor image plus suffix: below floor and split-at-floor errors, end-at-floor resumes; no silently lost committed reclaim |
| `ttl_image_deadlines` | Hand-built multi-frame op-15 including deadlines/high-water, then suffix reclaim; compare exact expected keys/reasons/detaches |
| `ttl_rewrite_lineage`, `ttl_backup_restore`, `ttl_graph_conversion` | Independent source/output header/floor and expected projection; compact/checkpoint omit due entities without fictitious deletes; restore/PITR and enable/strip invalidate correctly |
| `ttl_live_closed_parity`, `ttl_replica_export` | Literal physical source fixture, exact export bytes, follower replay vs independent model; directory chain/images and restart |
| `ttl_encrypted_history`, `ttl_compact_precision_images` | Known plaintext fixture/oracle after authenticated decoding, wrong key/corrupt image refusal; default gates unchanged, gated image fixtures clearly marked |
| `ttl_temporal_whole` | Test-only temporal activation: shared paging cannot split temporal commits; public reader still refuses while unsupported |

Each acceptance must assert an expected value/order/error, not merely compare
two production readers to each other. Cross-reader comparisons supplement,
never replace, the independent model and hand-built fixture assertions.
The byte-limit and oversized-operation cases also exercise the private chunk
builder with synthetic operation strings and an independent JSON byte builder:
do not bypass engine key/name limits to manufacture a supposedly valid log.
Those are defensive counter/paging unit fixtures, not a claim that today's
valid reclaim entries can naturally reach every byte boundary before the
256-operation boundary (`engine/limits.rs:13-16`).

| Deliberate phase-2 mutation | Test that must fail |
|---|---|
| Reorder chunks or reset ordinals per reclaim | `ttl_reclaim_order`, `ttl_resume_every_chunk` |
| Reader clock drives deletion | `ttl_restart_no_clock` |
| Accept another-set cursor, including v1 for TTL | `ttl_cursor_goldens` |
| Skip endpoint cascade or duplicate self-loop/two-endpoint edge | `ttl_replay_model` |
| Copy node TTL to attached record/vector put | `ttl_operation_goldens` |
| Omit detach or sort by numeric collection ID | `ttl_reclaim_order` |
| Clear interpreted as omission/preserve old deadline | `ttl_operation_goldens`, `ttl_replay_model` |
| `<` instead of `<=`; own deadline wins when later or loses an exact tie | `ttl_replay_model` |
| Ignore snapshot deadline / retain reclaimed key | `ttl_image_deadlines`, `ttl_replay_model` |
| Count Unicode characters instead of escaped UTF-8 bytes | `ttl_chunk_boundaries` |
| Split ordinary/temporal commits or alter legacy projection | `ttl_temporal_whole`, `legacy_feed_bytes` |
| Accept split cursor at checkpoint floor | `ttl_checkpoint_floor` |
| Truncate expansion / return empty continuing page | `ttl_resume_every_chunk` |
| Default missing referenced key or panic on corruption | `ttl_tracker_corrupt` |

Record each mutation's failing assertion; an un-killed mutation blocks slice
acceptance. No benchmark campaign or release criterion is added.

## 12. Documentation, public API gate and accepted rulings

Phase 2 adds exactly these `site/api.html` data-api rows, beside the current
feed rows (`site/api.html:530-531,799-803`), with unreleased labels:

| Exact key | Row meaning |
|---|---|
| `rust:Database::graph_changes_after` | Feature-selected durable embedded feed; typed opt-in errors |
| `rust:fn:read_log_graph_changes` | Same feed from closed file/directory and optional encryption key |
| `rust:GraphFeedCursor::beginning` | Beginning bound to selected graph_events |
| `rust:GraphFeedCursor::at_commit` | Explicit commit-boundary selection/conversion |
| `rust:GraphFeedCursor::position` | Legacy sequence/lineage, insufficient for split resume |
| `rust:GraphFeedCursor::ordinal` | Last consumed event or complete commit |
| `rust:GraphFeedCursor::graph_events` | Exact bound set |
| `rust:GraphFeedCursor::from_token` | Lossless v1/v2 parsing |
| `rust:GraphFeedCursor::to_token` | Canonical token, Empty remains v1 |

The scanner inventories functions and inherent methods, NOT bare types,
variants, fields or trait implementations (`tests/api_reference.py:225-294`).
Document all seven new types and their variants/fields in the corresponding
method rows' descriptions; do not invent stale `rust:Type` data-api keys.
REG-001 is a separate required gate: `.github/workflows/ci.yml:114-116`
runs `python scripts/api_surface.py check` in the stable-toolchain job. It
builds docs with
`cargo doc --no-deps --offline --locked --quiet -p vlite --target-dir target/api-surface`
and compares rustdoc `all.html` with
`tests/fixtures/api_surface/vlite-default.txt`. Its exact planned fixture
additions (kind plus path, sorted into their sections) are:

```text
enums vlite::changes::GraphEventSet
enums vlite::changes::GraphFeedError
functions vlite::changes::read_log_graph_changes
structs vlite::changes::GraphFeedBatch
structs vlite::changes::GraphFeedChange
structs vlite::changes::GraphFeedCursor
```

`GraphTtl` and `GraphRemovalReason` are defined at
`vlite::property_graph::GraphTtl` and
`vlite::property_graph::GraphRemovalReason`; these entries are allowed by the
property-graph prefix; rustdoc item lines are `enums
vlite::property_graph::GraphTtl` and `enums
vlite::property_graph::GraphRemovalReason` (auto-allowed, not hand-added to
the fixture). If rustdoc also lists the re-export aliases, the additional
exact fixture lines are `enums vlite::changes::GraphTtl` and `enums
vlite::changes::GraphRemovalReason`; those are not auto-allowed and must be
hand-added.
Inherent methods on existing `vlite::Database` and `GraphFeedCursor`, fields,
variants and trait implementations do not produce `all.html` fixture lines.
They remain covered by unchanged-signature compile tests and the `site/api.html`
keys above. Any addition outside `vlite::property_graph::` fails until its
fixture line is hand-added in the same commit; every removal fails. Never run
the fixture's baseline command.

No README/current-facing website claims or schema-file edits in this phase.
No scanner changes are needed for re-exports and the new impl module.

| Accepted ruling / remaining clarification | Decision |
|---|---|
| Graph cursor / wire decision 8 | Accepted: additive `GraphFeedCursor`, lossless v1/v2 parser and encoder; legacy `FeedCursor` and v1-only parser unchanged. |
| Reclaim ordering | Accepted: sorted feed projection of edge deletes, detaches, then node deletes; engine apply unchanged. |
| History floor | Accepted: typed `CursorBeforeHistory` for old history and a split cursor at floor; whole-commit cursor at floor remains valid. This deviates from the contract's literal "retained reclaim resume or lineage invalidation"; no new lineage or writer edit. |
| Response cap | Accepted: canonical 16 MiB per-event limit for the opt-in feed only. Global REST response-cap decision 3 remains open for the serving slice; no REST/transport layer here. |
| Decisions 8 and 9 | Ratified: exact `Empty`/`Ttl` set; split only within TTL reclaim runs; ordinary event units between reclaim runs indivisible; future mixed temporal/reclaim commits whole and oversized units refuse without cursor advance. |
| Canonical counter | Accepted: private `changes.rs` JSON-feature-independent counter. Later server proves byte equality and never re-splits or renumbers. |
| Compact graph supplements | Accepted: test-only fixtures; production refusal unchanged. |
| Transport representation | Serving design must resolve how the canonical single-event cap interacts with encoded envelope bytes, without introducing a global REST policy. |

These are accepted planning defaults, not implementation, activation,
relaxation or a release criterion. Static inspection verified cited source
anchors; phase 2 must establish runtime behaviour and all listed test oracles.

## 13. Implementation record (phase 2)

Implemented as local commits on top of the accepted design; nothing is
activated, exposed by server, bindings, CLI, OpenAPI or proto, or released.

1. Default-feed byte goldens captured before any production edit.
2. `GraphFeedCursor` v1/v2 codec and public types (`ttl_cursor_goldens`).
3. Private TTL tracker and reclaim projection (`ttl_replay_model`,
   `ttl_reclaim_order`, `ttl_tracker_corrupt`, `ttl_operation_goldens`).
4. Chunking of event units with the canonical per-event limit; typed
   `GraphFeedError` Display prints its fields.
5. Reader: `Database::graph_changes_after` and
   `vlite::changes::read_log_graph_changes`, snapshot-image deadline
   hydration, checkpoint floor, temporal whole commits, live/closed parity and
   path tests (`tests/graph_ttl_feed.rs`, `tests/graph_ttl_paths.rs`).

Accepted deviations from the text above:

1. `site/api.html` rows and fixture lines land with the functions (step 5),
   not with the types.
2. Re-export aliases need no extra fixture lines; the surface check passes
   without them.
3. The reader-level cursor-mode mismatch check landed with the reader.
4. Operation 15 snapshot hydration landed with the reader (step 5).
5. A temporary `dead_code` allowance on the private modules existed between
   steps 3 and 5 and is removed.
6. Reclaim test frames carry a trailing ordinary entry, matching the writer.
7. On a full page the legacy `head` is the first sequence that did not fit.

Observed behaviour recorded by tests: checkpoint and compaction images omit
entities already due at the writer clock without logging deletes, so the
feed reports no removal for them; reads already hide them. Compact graph-block
images are covered only by `cfg(test)` fixtures (production still refuses
them). Precision conversion is covered through the new-lineage rule. The
reader's "cursor commit not found" `InvalidCursor` branch is defensive and was
not reached by any fixture.
