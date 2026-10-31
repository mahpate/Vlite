# P3 precision publication: storage proposal

**PROPOSED ONLY; no implementation, API approval or writer enablement.** Resolves the publication blocker in [precision API review](p3-precision-api.md).
The [compact contract](compact-storage-contract.md) remains authoritative unless
the separately identified decisions below are approved. [P3](p3-compact-vectors.md)
keeps Float32 default. Accepted private codec work remains test-only (`src/compact/mod.rs:24-28`); neither production registration nor source writers exist.
All citations below refer to this checkout, not proposed helpers.

## Existing boundaries that the design must preserve

| Evidence | Consequence |
|---|---|
| `src/lib.rs:1310-1389` creates a header, implicit default collection and sequence 0; file writes/syncs only that header (`:1235-1256`), memory copies it (`:1291-1295`) | Empty opt-in needs actual durable state, not a collection inferred from vectors |
| Named create validates, allocates a never-reused ID, commits op 5, then installs (`src/lib.rs:2499-2539`); drop commits op 6 (`:2544-2565`) | Precision and existence must have one recovery boundary, including post-checkpoint creation |
| Settings v1 has no precision (`src/format.rs:861-886`); settings replacement assigns the whole mutable object (`src/lib.rs:2570-2589`) | Do not extend settings or put hidden precision in metadata |
| Flag 16 requires history (`src/format.rs:1254-1258`); op 16 requires compact (`:1336-1337`); image completion is checked (`src/compact/mod.rs:449-451`) | No ordinary op-16 WAL descriptor and no bare compact header |
| WAL flags are graph/encryption only (`src/directory.rs:37`); later headers are masked and compared (`:345-365`, `:395-430`) | An additional ordinary-WAL flag requires an explicit amendment |
| Commit appends/seals, advances sequence only after its selected sync, and poisons on failure (`src/lib.rs:2256-2303`); `append_and_sync` only seeks/writes/syncs (`:3736-3745`) | An append failure is not an assured rollback |
| Periodic/group paths may defer sync (`src/lib.rs:2188-2198`, `:2240-2251`); memory durability is explicitly None (`:1287-1295`) | Preserve existing durability modes; do not claim all acknowledgements are power-loss durable |

Store opt-in is immutable and can be introduced only at absent-destination create or explicit copy-out conversion. Precision is immutable even while empty.
Legacy named create always chooses Float32, including inside an opted-in store;
precise create requires opt-in even when its requested precision is Float32.
Default precision never becomes an inheritance rule. Existing Float32 APIs,
settings, record shapes, query validation and exact scoring remain unchanged.

## Two concrete viable approaches

**A: history-image-only catalog publication.** Allocate critical storage kind
`0x0003` as specified below. Every opted-in store starts with a complete compact
history image containing that catalog, even with no rows. Existing WAL operations
continue to store canonical f32 values. Named create/drop cannot append op 5/6:
under the exclusive writer lock, construct the candidate state at sequence H+1,
write/reload/sync a complete replacement image with floor H+1, then publish it.
For files this requires a new locked, staged replace-and-rebind path; for
directories use a new foreground variant of image installation, retaining the
existing WAL tail only when its state is included; memory swaps a complete buffer.
No manifest fields or sidecar. Settings and ordinary vector writes still append.
Post-floor catalog changes therefore advance the history floor and remove their
feed events; consumers behind that floor must snapshot again. This is viable,
not a missing-path impossibility, but requires that **separate unapproved semantic
decision**, an image rewrite per named create/drop, and platform-safe file
replacement while locks/read generations remain pinned. Standalone WAL headers
remain legacy: they are not complete opted-in stores and lack a reconstructible
base. An authoritative file/directory/memory image always rejects old readers via
its critical catalog (or the unknown compact flag). No descriptor mutation in WAL.

**B: guarded ordinary create + history catalog (recommended).** Allocate flag
32 for immutable store intent, op 17 for a complete named-create-with-precision,
and critical kind `0x0003` for checkpoint catalogs. Initial stores still contain
a complete history image. Ordinary named creates use one op-17 frame, drops
retain op 6, vectors retain existing opcodes with canonical f32 bodies. Empty
stores and every WAL header retain flag 32. Catalog changes do not force a
checkpoint or invalidate history. This adds one ordinary opcode/flag but avoids
a new file-replacement transaction engine and preserves the existing create/feed
commit boundary. **B requires Decision W below; it is not currently authorized.**

Recommend B as the smallest end-to-end publication choice, initially using legacy source frames for *both* precisions. This qualifies immutable policy and
canonical values, not half-sized disk storage, bounded reads or P3 performance.
Do not register `0x0100` merely to activate the numeric conversion routine.

## Decision W: SEPARATE UNAPPROVED contract amendment for B

Amend only these rules: permit new `FLAG_PRECISION = 0x0020` in V5 image/WAL
headers; add it to directory `WAL_FLAGS`; permit new `OP_COLLECTION_PRECISE = 17`
only with that flag, only in the ordinary suffix. Flag 16 and op 16 remain
history-image-only. Flag 32 excludes timestamps, is retained by every segment,
and is never added by in-place header mutation. An authoritative flag-32 stream
must begin with `HISTORY | COMPACT` and a complete catalog image. A standalone
flag-32 WAL without its base is refused as corrupt, including a header-only WAL;
header parsing may recognize it, but full replay cannot infer default precision.
Directory concatenation hides later headers as today, and validates their flag
agreement; losing/substituting an unguarded segment is corrupt, not legacy fallback.

Registry entries proposed here:

| Entry | Exact meaning / support |
|---|---|
| Header `0x0020` | Immutable precision-policy store; unsupported readers return `UnsupportedFeature(32)` |
| Ordinary op `17` | Atomic named create, dimensions, settings and precision; no update mode |
| Storage `0x0003`, v1, critical | Complete precision catalog; required exactly once per flag-32 image |
| Storage `0x0004`, v1, critical | Import registry image, Decision P below; exactly once, including empty registry |
| Storage `0x0005`, v1, critical | Graph revision supplement, Decision P below; per graph, partitioned |
| P3 `0x0100`, v1, critical | Existing primary dense rows specification, still production-disabled in this slice |

Allocation of the three storage kinds and their registration is **unapproved**. The existing registry recognizes only source/derived ranges, and its table
currently permits only derived codecs (`src/compact/mod.rs:58-64`, `:369-405`).
Add concrete check/apply/encode handlers, not placeholder acceptance. Unknown
storage versions/kinds remain fatal; authentic derived blocks may still be skipped.

**Refusal proof:** today's header parser checks unknown flags before descriptor
opening or frame replay (`src/format.rs:1246-1251`, `:1280-1290`), so it refuses
flag 32 even on an empty image/WAL or encrypted file without a key. Readers
predating compact also refuse flag 16. A compact-aware reader lacking catalog
support refuses its critical table entry. If flag 32 alone is stripped and CRC
recomputed, the catalog still refuses current readers; new readers reject a
catalog without flag 32. Op 17 without flag 32 is corrupt. Arbitrarily rewriting
all framing and deleting authentic source is not a compatibility guarantee.

## Exact B bytes (little-endian; no changes to existing settings/frame bodies)

Header remains 32 bytes: magic [0,8), V5 u32 [8,12), default dimensions u32 [12,16), lineage u64 [16,24), collection high-water u16 [24,26), flags u16
[26,28), CRC32 [28,32). Existing encoder: `src/format.rs:549-563`, `:577-587`.
Initial flags are `8 | 16 | 32 | 4`, plus 1 when encrypted; WAL flags are `32 | 4`, plus 1. Encryption keeps the existing 64-byte descriptor and frame
sealing; prefix is AAD (`src/crypto.rs:317-353`). No plaintext spill.

Op 17 uses the existing 36-byte prefix/16-byte footer layout
(`src/format.rs:590-617`): magic `VREC`; op at 4; collection [5,7)=0;
reserved byte 7=0; sequence u64 [8,16)=H+1; ID u64 [16,24)=new collection
ID in 1..65535; count u32 [24,28)=0; body length u32 [28,32); prefix CRC
[32,36). Body: schema u16 at 0 =1, precision u8 at 2 (1=f32, 2=f16),
zero bytes [3,8), then the *unchanged* op-5 create body at 8:
name length u8, name bytes, dimensions u32, settings-v1 bytes.
Existing create encoder is `src/format.rs:828-840`. Body length is
8 + 1 + name_length + 4 + settings_length, capped at 16,460 bytes
(63-byte name, 16,384-byte settings). Check bounds/reserved fields before
allocation; unknown schema/precision returns `UnsupportedFeature(32)`, malformed structure is corrupt.
Footer is `VCMT`, CRC32 of prefix+body, same sequence u64; encryption adds
its existing seal overhead, not a second commit frame. Op 5 remains unchanged
for legacy streams and history restatement; reject it in a flag-32 ordinary suffix.

Kind `0x0003`: standard compact envelope v1, critical=1, owner/object/digest
zero, partition 0, one section, frame ID=3, frame count=N in 1..64.
Section starts with opt-in u32=1 and reserved u32=0, then N eight-byte entries:
collection ID u16, precision u8 (1/2), reserved u8=0, dimensions u32.
IDs strictly ascending; exactly all live collections, including ID 0; dimensions must match header/op-5 declarations. No omitted-collection defaulting in opted-in
images. Section length=8+8N; envelope first offset=40+p, section table
`(40+p,8+8N)`, p in 0..63 zero bytes for advisory alignment; body ends there.
Frame at floor S follows the capability table and precedes source records.
Table entry bytes for it are `03 00 01 00 01 00 00 00`. Even a rowless catalog is a real critical block, so the accepted nonempty-table,
at-least-one-block and image-end requirements are not weakened.

## Decision P: SEPARATE UNAPPROVED preservation allocations, not hidden metadata

The API review also promises closed-import state and original graph revisions on conversion. Today's image writer emits only open imports
(`src/maintenance.rs:767-776`), while the registry carries max-job, dropped
bindings and closed-order state (`src/import.rs:83-95`, `:115-159`, `:192-203`).
Graph snapshots set node/edge revisions to S (`src/property_graph/engine/state/snapshot.rs:129`,
`:166`). Neither helper alone satisfies that promise.

Propose kind `0x0004`, owner 0/partition 0/digest 0, one section: max-job u64,
job count u32, closed count u32; then ascending-job entries:
job u64, collection u16, state u8 (0=open,1=finished,2=aborted), dropped u8
(0/1), name length u8, three zeros, last chunk u64, committed u64, skipped u64,
digest [32], name bytes; then closed-order job IDs u64 oldest first.
Fixed entry size=72 before name. Frame count=job count; zero is valid. Cap at 1,088 jobs, 1,024 closed, names 1..63 and 155,088 section bytes;
validate counts/lengths, unique IDs <=max-job, exact closed membership, and
live bindings or historical dropped bindings before installation.
Conversion refuses open jobs as reviewed; checkpoints preserve them. This stores the bounded observable registry and allocation high-water, not
discarded log history or evicted closed-status details.

Kind `0x0005`: graph owner, digest 0; two sections, nodes then edges, each starts
with entry count u32 and reserved u32=0, followed by ascending `(entity ID u64,
revision u64)` entries; frame count is their sum. Max 4,096 entries/partition,
65,552 combined section bytes; partition runs start at zero, concatenate in order separately for each entity class, and cover
every retained node/edge exactly once. Emit one zero-count block for an empty
graph. Revisions must be 1..S. Load after that graph's op-15 snapshot and restore
revisions before serving; reject missing/extra IDs. This is a storage supplement,
not reinterpretation of op 15 or allocation from the P4 range. Existing graph
snapshot bodies retain entity IDs, schema, attachments and next node/edge IDs
(`src/property_graph/frame.rs:217-306`). Preserve graph IDs and existing reusable
graph-ID semantics, not an invented global high-water (`src/property_graph/engine/state.rs:739-747`, `:780-786`).
Table entries are `04 00 01 00 01 00 00 00` and `05 00 01 00 01 00 00 00`;
list/use 5 only when graphs exist. No source codec limits change.
Exact image order: advance, op-5/7 declarations, table, kind 3, kind 4, op-14 records per collection, op-10 vectors, op-15 graph runs each followed by
its kind-5 run, optional derived blocks, end. No open-import marker duplication.

## Commit, publication and recovery transitions

Initial create: absent -> construct complete candidate image -> reload/validate catalogs/end -> sync -> publish -> return locked handle. Reserve S=1 even empty;
head=floor=1, default descriptor exists, no record revision or change event.
File create must extend staged no-clobber publication (`src/maintenance.rs:594-624`) with lock acquisition/retention before final-name visibility, not return a header-only store. Memory validates then installs the whole byte buffer;
`to_bytes`/`open_from_bytes` preserve it (`src/lib.rs:1298-1308`).
Directory create lists `image-1.vlite` plus empty guarded `wal-2.log` in one
unchanged manifest, extending `src/directory.rs:911-950`; never take its
sequence-zero WAL-only branch for opt-in.

Named create: validate name/spec/policy -> allocate tentative ID/H+1 -> encode one op 17 -> seal/append/select existing durability -> install collection and
precision/high-water together. Validation failure changes nothing; no separate
descriptor append. Recovery accepts a complete authenticated frame wholly,
or retains the old catalog for an incomplete suffix; malformed complete frames
are corrupt. Op 6 removes the descriptor with the empty collection; it consumes
one sequence, does not reclaim its ID. Settings op 7 never alters precision.
Ordinary create/drop preserve lineage/floor; no record revision is manufactured.

Checkpoint: seal/sync/rotate, capture sealed head S, build/reload/sync image with catalogs and original record revisions, publish manifest, swap pinned generation;
retain later WAL at S+1 onward. Existing paths: `src/directory.rs:599-619`,
`:675-689`, `:705-750`, `:775-808`. Keep flag 32 in rotation headers/image flags.
Use the catalog as of S, not the live handle's later state. Checkpoint retains
lineage/head and advances floor to S; preserve revisions via op 14 and kind 5.

Pre-publication build/validation failure: discard only staging; old source/base and committed tail remain authoritative. Append I/O/sync failure:
`CommitIndeterminate`, poison, reopen to determine whole-frame outcome; do not
retry precise create under a guessed new ID. Manifest replace followed by sync
uncertainty is publication-indeterminate: retain both generations, poison for a
catalog-changing publication, and reopen the surviving manifest. Existing
`publish_manifest` returns false rather than an error after such uncertainty
(`src/directory.rs:764-772`); new create/publication callers must distinguish it,
and must not invoke `create_locked`'s unconditional error cleanup after visibility
(`:857-875`). Unix parent sync exists; Windows has no directory-sync promise
(`src/directory.rs:143-150`); preserve platform guarantees, do not invent stronger ones.

## End-to-end source and transport trace

| Path | Required behavior / actual integration site |
|---|---|
| Ordinary/conditional/ordered batch writes | Canonicalize owned dense inputs once before encoding and install the same bits; preserve existing conditions/order/one frame (`src/lib.rs:1922-1958`, `:1989-2026`, `:2063-2131`). Float16 overflow rejects precommit, Float32 preserves finite bits |
| Graph+vector atomic batch | Canonicalize by each target collection before constructing graph body; validate/encode/apply that identical body and vector list in one op 12 (`src/property_graph/mod.rs:699-720`, `:831-837`). Replay verifies canonical bits before applying (`src/property_graph/frame.rs:83-145`) |
| Named dense/history/reembedding; sparse | Canonicalize every dense version in the shared vector commit path (`src/vectors.rs:612-631`, `:646-684`, `:732-769`); keep sparse f32 unchanged. Preserve configured dimensions per dense name |
| Import | Include precision overflow in existing reject/skip validation; compute retry identity from submitted inputs plus validity/policy, not rounded inputs (`src/import.rs:473-488`); commit accepted canonical rows and marker together |
| Mixed/empty images | Catalog includes all precisions and empty collections. Initially all records use unchanged op-14 f32 source bodies; named/sparse data use op 10. Preserve payload, metadata, absolute expiry, histories and attachments |
| Optional later dense columns | Production `0x0100` registration/writers need their own qualification. Keep v1 component cap 1 MiB and row cap 4,096; cannot emit zero rows or exceed either. No new numeric version/allocation here |
| Reopen/repair | Header guard -> complete image catalogs -> ordinary suffix. No serving partial catalog; incomplete image is corrupt, never repairable; incomplete suffix only follows existing explicit tail repair. Encrypted decode authenticates before catalog/source use |
| Backup/restore/key rotation | Backup copies bytes (`src/maintenance.rs:249-266`); restore changes lineage/header only and retains flags (`:289-325`); reencrypt preserves flags except encryption and transcodes all frames (`src/logops.rs:25-66`). Keep descriptors and canonical bits; reencrypt gets fresh salt/nonces/new lineage |
| Copy-out precision conversion | Exclusive read-only source (`src/maintenance.rs:72-81`), absent destination, preflight all retained dense values, new lineage, head/floor=max(source head,1), retain numeric revisions/high-waters; no old log history. Add preserve-all-state mode, not `now=0`: existing TTL filters (`:729-731`, `:753-761`; `src/property_graph/frame.rs:239-266`) would still drop deadline 0. Use kinds 4/5 for reviewed preservation. Source bytes never change |
| Other rewrites/directory conversion | Preserve policy/catalogs and canonical bits; explicitly refuse before output until faithful. Extend `write_v5_image` (`src/maintenance.rs:677-788`) and `build_image` flags (`src/directory.rs:784-793`), including empty conversion; never drop flag 32 as a rebuildable cache |
| Feed | Catalog/image frames emit nothing. Op 17 updates the feed collection map and emits existing `CollectionCreated` shape without adding public precision fields (`src/changes.rs:372-384`); feed is not a complete policy-replication protocol. Floor/lineage checks stay (`:195-208`, `:244-251`) |
| Export | Copy complete op-17 frames without translation; preserve guarded header, scan catalogs/end even when returning only suffix. Before floor returns `CursorBeforeHistory` (`src/logops.rs:324-402`); no bootstrap-at-zero for these history images |
| Replica | Bootstrap from validated backup at/after floor (`src/replica.rs:85-101`); suffix replay installs precision with existence, never recanonicalizes. Preserve exact header/lineage checks (`:143-159`, `:212-225`); header-changing checkpoint requires fresh backup bootstrap, not a silent relaxed comparison |

Conversion's matching numeric revision is not lineage-bound; callers must reread. Old-lineage feed cursors fail; replicas restart. Backup alone keeps lineage.
Replica append/reload rollback is a separate existing path (`src/lib.rs:2369-2422`):
failed truncate/sync poisons; do not present it as proof of primary append rollback.
All original source files remain untouched on conversion, restoration and rotation; only explicitly selected dense components lose precision in the new destination.

## One owner and bounded qualification

**One storage owner** owns this entire shared-format slice: envelope writer, registry/catalog codecs/check/apply in `src/compact/mod.rs`; header/op-17 bounded
decode and replay in `src/format.rs`; immutable loaded/collection state and
create routing in `src/lib.rs`/`src/collection.rs`; directory publication,
maintenance, feed/export/replica and crypto integration in their cited files.
Domain canonicalization reuses private numeric logic; no parallel owner edits shared framing. First handoff is private B catalog/create/recovery qualification
with legacy source bodies, not public constructors or source-0100 enablement.

| Bounded test family | Required outcomes |
|---|---|
| 12 base cases | file/directory/memory x plain/encrypted x empty/populated: immediate/serialized/reopened intent, mixed f32/f16 bits, settings replacement unchanged |
| Named lifecycle | empty f16 create after checkpoint, legacy f32 create beside it, drop/recreate new ID, duplicate/default/limit errors; sequences/catalog remain atomic |
| Initial image failures | cut each byte of small empty golden through end block, plus exact frame boundaries; no partial store served or repaired; wrong key/tampered prefix/body refused |
| Publication failpoints | stage write/reload/sync, final rename, manifest replace, directory sync, generation swap/retirement; old/new whole state, indeterminate classification and retained source/generations |
| WAL failures | op-17 prefix/body/footer truncations; append/sync failure; complete unacknowledged create survives wholly or absent; poisoned handle, no half collection/policy |
| Old readers | pre-compact, current compact-without-new-registry, registry-aware-without-op17: empty/populated/encrypted, independent guarded WAL, feed/export/repair/replica; refusal before writes/repair |
| Preservation/transport | deadline 0/already-expired records and graph, non-head node/edge revisions, closed/dropped import bindings/max-job, dense histories, source byte equality; backup/restore/rotation/export/replay |
| Canonical/atomic | existing float16 fixtures, signed zero/ties/overflow, mixed graph targets, duplicate-ID batches, reject/skip import retry; no rows/graph/marker/revision changes on validation failure |

**Unresolved decisions:** approve B's exact flag-32/op-17 amendment versus A's catalog-change floor semantics; allocate/register kinds 3/4/5 and their preservation
contracts; review platform publication-indeterminate handling. Public API enablement,
production source-0100 writers and performance qualification remain separate gates.
