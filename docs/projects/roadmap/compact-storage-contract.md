# Compact storage contract (P3, P4, P5)

| Field | Value |
|---|---|
| Status | Interface decisions accepted 2026-10-30; implementation pending. Defines what [P3](p3-compact-vectors.md), [P4](p4-compact-graph.md) and P5 share on top of [P2](p2-directory-store.md) |
| Roadmap | [`next-major.md`](next-major.md); goals in [`storage-goals.md`](../performance/storage-goals.md) |
| Validation | Design review only. Citations are `path:line` on this branch and may drift |

P3 and P4 were designed separately, and P5 adds statistics to the same
checkpoint images. This contract fixes one header flag, one frame operation,
one block header and one kind registry, so codec owners can work in parallel
while shared format, recovery and maintenance code keeps one owner.

## 1. Scope and non-goals

In scope: the flag, the operation, the block header, the kind registry, the
block directory, accessor traits, invariants and slice order. Not in scope:

- **No new public API.** Rust, REST, gRPC, MCP, CLI and bindings stay as
  they are, as do `src/openapi.json`, the proto and
  `tests/fixtures/api_surface/vlite-default.txt`. Any opt-in surface needs
  its own API review (decision 7).
- **float32 stays the default** stored precision; float16 is explicit
  ([storage goals](../performance/storage-goals.md), decision 2).
- **S1 stays open, not waived.** Whether S1 may use the float16 profile is
  still an open release decision (decision 5 there).
- **No benchmarks** before the major release. DO-005 runs once, under its
  unchanged quiet gate ([roadmap](next-major.md)).
- **No codec algorithms.** Column, CSR, code and bitmap layouts stay in the
  P3, P4 and P5 designs. The WAL format, op-12 WAL writes and the manifest
  format do not change.

## 2. Ownership

| Area | Files | Owner |
|---|---|---|
| Format | `src/format.rs`: flags, op allow-list, per-op bounds, load loop | Storage owner |
| Directory | `src/directory.rs`: image flags, generation pinning, unmapping | Storage owner |
| Maintenance and recovery | `src/maintenance.rs`, `src/logops.rs`, `src/replica.rs`, `src/storage.rs` | Storage owner |
| Crypto, errors, change feed | `src/crypto.rs`, `src/lib.rs` (`Error`), `src/changes.rs` | Storage owner |
| Contract module (new) | `src/compact/mod.rs`: header, registry, capability table, block directory, traits | Storage owner |
| P3 codec | `src/compact/vectors.rs` (P3a/b), `src/compact/ann.rs` (P3c); integration in `src/records.rs`, `src/collection.rs`, `src/vectors.rs`, `src/hnsw.rs`, `src/quantize.rs`, `src/ann_snapshot.rs` | P3 owner |
| P4 codec | `src/compact/graph.rs` (may become a folder); integration in `src/property_graph/` (`engine/`, `mod.rs`, `frame.rs`) | P4 owner |
| P5 stats | `src/compact/stats.rs`; integration in `src/planner.rs`, `src/metadata_index.rs`, `src/metadata.rs` | P5 owner |

Codec owners never edit storage-owned files. A needed change goes to the
storage owner as a contract amendment.

## 3. Capability and tag registry

**Header flag.** Refusal works at three levels: flag, op and kind. Add
`FLAG_COMPACT = 16`. The used flags are 1, 2, 4 and 8
(`src/format.rs:17-33`, `src/property_graph/engine/codec.rs:29`). Writers
set it only on `FLAG_HISTORY` checkpoint images that hold at least one block.
Byte copies of such images keep it. WAL headers never carry it: the directory
masks them to `GRAPH | ENCRYPTED` (`src/directory.rs:37`, `:348`). Older
builds refuse an unknown flag with `UnsupportedFeature` before replay or tail
repair (`src/format.rs:1237-1241`). This meets P3's "reject before replay".

**Frame operation.** Add one op, `OP_COMPACT_BLOCK = 16`, shared by all
owners. It is allowed only in v5 with `FLAG_HISTORY | FLAG_COMPACT`, at the
checkpoint sequence `S`, inside the restated prefix, like ops 14 and 15
(`src/format.rs:1543-1546`, `:1584-1588`). This meets P4's "new operation,
do not reinterpret op 15". If the flag is stripped, every build reports op 16
as corrupt (`src/format.rs:1547`, `:1555-1559`).

**Block kinds.** A `u16` kind in each block selects the codec. Owners
allocate kinds inside their own range without editing shared code:

| Kinds | Owner | Use |
|---|---|---|
| `0x0000` | none | Invalid |
| `0x0001-0x00FF` | Storage | `0x0001` capability table; rest reserved |
| `0x0100-0x01FF` | P3 | Source columns: IDs, revisions, expiry, dense f32/f16, named dense |
| `0x0200-0x02FF` | P3 | Derived ANN: topology, codes, transform parameters |
| `0x0300-0x03FF` | P4 | Graph base: ID maps, edge table, CSR, dictionaries, labels, properties, keys, attachments, deadlines |
| `0x0400-0x04FF` | P5 | Derived statistics, bitmaps, filter indexes |
| `0x0500-0xEFFF` | none | Reserved; allocated only by amending this contract |
| `0xF000-0xFFFF` | Tests | Test-only; release builds reject them |

**Mandatory or optional.** Every block is *critical* (source data) or
*derived*. Kinds `0x01xx` and `0x03xx` are critical; `0x02xx` and `0x04xx`
are derived. An unknown or unsupported critical kind or version makes a
compact-aware build refuse the image. A derived block that is authentic but
unknown, stale or of an unsupported version is skipped, rebuilt by the
background path, and reported. That reuses the `Stale` and
`UnsupportedVersion` outcomes of `src/ann_snapshot.rs:71-83`. A checksum,
authentication or structure failure is `Corrupt` for every kind. It is never
treated as a cache miss.

**Version negotiation.** The capability table is the first op-16 frame of
the image, appears exactly once, and has object, partition and digest zero.
Its single section has sorted, unique eight-byte entries: kind (`u16`),
codec version (`u16`), flags (`u16`, bit 0 critical) and reserved zero (`u16`).
The table does not list itself; every other block must have a matching entry.
Entries must be used. Readers accept only explicitly implemented versions,
not every smaller version. The flag covers old builds; the table covers
new codecs. Unsupported critical data fails closed before state is published.

## 4. Block and directory contract

**Frame.** A block is one standard frame: 36-byte prefix, body, 16-byte
footer (`src/format.rs:63-64`, `:582-607`). Prefix fields: `collection` holds
the owning collection or graph ID (both `u16`;
`src/property_graph/engine/model.rs:12`). The prefix `id` holds the kind in
bits 0-15, a partition number in bits 16-47 and zero above. `count` is the
item count. The reserved byte is zero. Encrypted frames are sealed one at a
time, with the prefix as AAD (`src/crypto.rs:317-353`).

**Block header.** The plaintext body starts with 32 little-endian bytes:

| Bytes | Field | Rule |
|---|---|---|
| 0-1 | kind | Must equal the prefix kind |
| 2-3 | codec version | At least 1 |
| 4-5 | flags | Bit 0 critical; must match the kind range and the capability table. Other bits zero |
| 6-7 | section count | 1 to 64 |
| 8-15 | object | Owning ID in bits 0-15, partition in bits 16-47, bits 48-63 zero; must match the prefix |
| 16-23 | source digest | Derived blocks: digest of the bound source blocks; critical blocks: zero |
| 24-27 | first section offset | From body start, after header, table and padding |
| 28-31 | reserved | Zero |

A section table of `(offset u32, length u32)` follows. Offsets count from
the body start. They must ascend, must not overlap, and must stay in bounds.
Codecs check every count with checked arithmetic before allocating, and use
checked 32-bit ordinals as P4 requires.

**Length bounds.** `next_frame` caps a body at 256 MiB
(`src/format.rs:1349`, `:1372`). This contract caps a block body at 64 MiB,
the P2 batch size (`src/format.rs:1986`), because an encrypted block opens
into one heap buffer (`src/crypto.rs:357`). Each codec declares a smaller
target; P2 record blocks use 1 MiB (`src/format.rs:57`).

**Alignment.** Section starts are 64-byte aligned relative to the file
offset. The writer pads before the first section. Alignment is advisory,
because `reencrypt` changes every frame's size by 28 bytes
(`src/crypto.rs:20-21`, `src/logops.rs:56-60`). A reader checks alignment
and copies misaligned or encrypted sections into bounded scratch.

**No absolute offsets on disk.** At open, the directory builds an in-memory
block directory from the image's frame prefixes. Each entry holds the kind,
object, partition, file offset and length. The directory lives and dies with
its image generation.

**Manifest.** Unchanged: `file <name>` lines plus a checksum
(`src/directory.rs:33`, `:79-141`). A compact image is still `image-N.vlite`.
No per-block entries, sidecars or new file kinds; the manifest stays the
only publication authority, as P3 and P4 both ask.

**Codec registration.** `src/compact/mod.rs` owns concrete checked dispatch,
not a plugin system. Add handlers only as codecs land; unsupported critical
kinds fail with `UnsupportedFeature`. Do not add empty codec modules or
general-purpose traits just to reserve names. These are the required duties:

| Method | Called by | Duty |
|---|---|---|
| `check` | Load loop | Validate header, sections and invariants; no state change |
| `apply` | Load loop, at the end of a run | Install the decoded base into `Loaded` |
| `encode` | `write_v5_image` | Emit blocks for one collection or graph, deterministically |
| `bind` | Load, background rebuild | Derived only: compare the source digest; report stale |
| `graph_keys` | `src/changes.rs` | P4 only: graph keys for later change events (decision 10) |

## 5. Accessor contract

All accessors are crate-private and borrow from a pinned image generation.

| Owner | Accessor | Contract |
|---|---|---|
| P3 | Vector access | By row ordinal or public ID: a mapped `&[f32]` when plain and aligned, or float16 decoded into bounded scratch. Rows ascend by public ID, with revision and optional expiry |
| P3 | Generation-owned records | Keeps `RecordMap::get` and `ReadSnapshot::get` returning `&Record` (`src/records.rs:55`, `:253-261`). Records decode into a cache owned by the generation. Entries are never evicted while borrowed and count toward S2 |
| P4 | Adjacency access | Same order and cursor as `Graph::adjacency`: (relationship, edge ID) ascending, type filter, `after` cursor, self-loop once for `Both` (`src/property_graph/engine/state.rs:305-329`). Dictionary codes keep string order |
| P4 | Entity access | Public ID to ordinal, key lookup, labels, projected properties, attachments, deadlines. Replaces the `&'db Graph` borrow in `GraphRef` (`src/property_graph/mod.rs:119-128`) |
| P5 | Row ranges and masks | Per block: first and last public ID and row count. Per generation: a deleted-row bitmap. Expiry stays a per-row deadline checked at read time |

Reads layer the WAL overlay over the base. The newest committed version
wins, and deletes and expiry apply before any candidate leaves an accessor.
P5 statistics only skip blocks that cannot match, like the candidate
supersets in `src/metadata_index.rs:1-6`. They never decide visibility.

**Lifetimes.** A generation is one image segment, its mapping and its block
directory, held by an `Arc`. Accessors, borrowed records and slices cannot
outlive it. `ReadSnapshot` can outlive its database (`src/records.rs:156`),
so it holds the generation, not the chain; its retention limits are
unchanged (`src/records.rs:123-126`). Publication: build off the read path,
reload to validate, fsync (`src/directory.rs:775-808`); rename and replace
the manifest (`:764-772`); swap the generation under a short lock; unmap,
then delete, after the last reader leaves (Windows, `:741-750`).

Without mappings (WASM, `src/storage.rs:1-3`, or a failed map,
`src/directory.rs:185-198`), accessors read owned buffers. Neither path may
hide an I/O or authentication error.

## 6. Invariants

| Topic | Rule |
|---|---|
| Publication | Only a manifest replace publishes. Leftover unlisted files are removed on writable open (`src/directory.rs:826-842`) |
| Verification | No block byte is used before its frame passes the footer checksum and, if encrypted, authentication (`src/format.rs:2132-2152`) |
| Crash and torn tail | Blocks exist only in images, so a torn or corrupt block is corruption, never repaired. Only the active WAL allows torn-tail repair |
| Encryption | No plaintext sidecars or spill files. Derived blocks are sealed like source blocks, or rebuilt in memory. `disk_ann` sidecars stay off for encrypted stores (`src/disk_ann.rs:143-154`) |
| Record TTL | Records expired at checkpoint time are not written (`src/maintenance.rs:729-731`). Later expiry is masked at read time |
| Graph TTL | A due node hides itself and every incident edge (`src/property_graph/engine/state.rs:122`, `:361-370`). Checkpoints drop due entities, and the edges of dropped nodes (`src/maintenance.rs:671-674`, `:774-783`). Reopen never revives them |
| Backup, restore | Backup copies bytes and keeps offsets (`src/maintenance.rs:249-266`). Restore rewrites only the header and keeps its flags (`:303-325`) |
| Re-encryption | Transcodes frame by frame and keeps every flag but encryption (`src/logops.rs:41-60`). Offsets move, so alignment is advisory |
| Compact, upgrade | Legacy export is allowed only if it preserves values, settings and future write semantics. A populated float16 collection cannot silently lose its precision policy. Until a faithful export exists, refuse it explicitly; never advertise this as restoring discarded float32 bits |
| Convert | `convert_to_directory` writes blocks only when opted in; otherwise its output is as today (`src/directory.rs:911-951`) |
| Replicas | Segments cannot start before an image's floor (`src/logops.rs:346-353`). A follower bootstraps from a backup copy (`src/replica.rs:85-88`), so it needs a compact-aware build |
| Change feed | Restated blocks emit no events. Op 16 gets its own arm next to ops 14 and 15 (`src/changes.rs:394-403`) |
| Determinism | Plaintext bodies are identical for any thread count, like the HNSW build (`src/hnsw.rs:39-43`). Encrypted bytes differ only by random nonces (`src/crypto.rs:328-329`) |
| Mixing | An image may mix legacy and compact objects. One collection or graph uses exactly one form per image |

Emission order in an image: header, advance frame, collection and settings
frames, capability table, then per collection ascending its source blocks,
derived blocks and statistics. Vector and import frames follow, then per
graph ascending an op-15 snapshot or P4 blocks, then graph statistics. Graph
blocks load after every record, so P4 validates attachments there.

## 7. Integration order and merge plan

| Slice | Owner | Delivers | Must add tests for |
|---|---|---|---|
| 0. Interface freeze | Storage | Checked block envelope and capability table, flag/op recognition guarded by complete validation, block directory and change-feed handling. No production compact writer or source codec yet; unsupported critical kinds fail closed | Golden fixtures; old-build refusal; missing/duplicate tables; explicit versions; unknown critical or derived kind; torn/corrupt image; encrypted framing; unchanged legacy reopen/conversion; backup, restore, re-encryption, replica and feed refusal where an image cannot be served faithfully |
| 1a. P3a codec | P3 | Precision settings, canonical float16, vector-column blocks, explicit conversion | P3's codec and float edge cases, plus golden blocks, malformed sections, encrypted blocks, reopen equality |
| 1b. P4 codec | P4 | ID maps, edge table, CSR, dictionaries, properties, deadlines; reads checkpoints with the gate on | P4's oracle, plus golden blocks, parallel edges, self-loops, sparse IDs, TTL cascade, worker-count parity |
| 1c. P5 stats | P5 | Statistics and bitmaps as derived blocks | Stale binding is rebuilt, never served; skipping is conservative; corrupt fails |
| 2. Shared wiring | Storage | Writers enabled per opted-in object, generation pinning, crash matrix | Extends `tests/directory_recovery_matrix.rs`, `tests/encryption.rs`, `tests/replica.rs` and `tests/graph_compat.rs` with compact images |
| 3. Later slices | Each owner | P3b-d, P4 mutations and import, filter-aware ANN | The acceptance lists in the P3 and P4 designs |

Rules for avoiding conflicts:

- Slice 0 builds on the accepted Gap 3 commit `cf146fe`, which
  touches `src/changes.rs`, `src/format.rs` and `src/property_graph/`.
- Slices 1a-1c touch only their own `src/compact/` module, domain files
  and test files. Every new kind gets a golden fixture in the same change.
- No slice adds a public `Error` variant (`#[non_exhaustive]`,
  `src/lib.rs:296-298`) or edits the OpenAPI file or the proto.

## 8. Decisions and remaining gates

Decisions 1-6 and 8-10 are accepted below. Decision 7 fixes the private-first
rollout; its public opt-in still needs API review. Decision 11 remains open.
These decisions authorize implementation, not performance or release claims.

| # | Question | Decision |
|---|---|---|
| 1 | Refusal: P3 asks for a capability checked before replay; P4 asks for a new op | Both: `FLAG_COMPACT` plus one shared op 16 |
| 2 | Numbers | Flag 16, op 16 and the ranges above are reserved here; no collision in the accepted base `cf146fe`. Shared-code integration checks later contributions against this registry |
| 3 | Error for an unknown critical kind in a compact-aware build | Reuse `UnsupportedFeature(FLAG_COMPACT)`; no new variant |
| 4 | Verification timing: P4 wants full, linear checks at open; P3's S2 needs bounded residency; F4 asks for reopen time independent of size | Eager full checks in slices 1a and 1b. Lazy, cached checks later, with honest F4 and F6 timings |
| 5 | Alignment: 64 bytes comes from `storage-goals.md`, not the P3 design | 64 bytes, advisory, with a copy fallback |
| 6 | Persisted ANN: inside the image or a sidecar? P3 is unclear | Derived blocks inside the image; existing sidecars keep their current, unencrypted-only role |
| 7 | Opt-in: P3 picks precision when a collection is created; P4 asks for an explicit conversion or format opt-in | Crate-private gate until qualified. Then one store-level opt-in at create or convert, plus P3's per-collection precision, after API review |
| 8 | Does compaction emit blocks? | No production writer in slice 0. Later codecs must support faithful maintenance or reject it explicitly; legacy export is not an implicit precision downgrade |
| 9 | Are P5 statistics critical? | No, derived and rebuildable |
| 10 | Change feed: replacing op 15 loses the graph keys `src/changes.rs:394-399` reads. P4 does not cover this | P4 provides `graph_keys` through the registry |
| 11 | S1 acceptance profile | Stays open and is not waived; decided before release |
