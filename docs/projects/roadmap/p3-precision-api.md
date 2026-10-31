# P3 precision API: proposal for review

**Status: PROPOSED, not approved or implemented.** This requests explicit Rust
API review on the accepted compact foundation; it authorizes no writer.
The concurrent private numeric/column codec work does not approve this surface.

## Accepted goals versus proposed decisions

Accepted: [P3](p3-compact-vectors.md) keeps float32 default, makes float16
per-collection and lossy, preserves public records/search, and requires explicit
copy-out for populated conversion. The [compact contract](compact-storage-contract.md),
section 8 decision 7, accepts a private-first gate followed, **after API review
and qualification**, by one store opt-in at create or convert plus precision.
Decision 8 requires faithful maintenance or refusal; source writers remain gated.

Proposed below: exact Rust names, separate immutable precision descriptors,
constructor arguments, conversion scope and failure/compatibility contracts.
S1's float32-default target remains open and unchanged. No benchmarks,
measurement claims, binary16 algorithm, new internal-slice knobs, or changes to
`src/openapi.json`, proto, wire APIs, CLI or bindings are proposed.

## Two small viable designs

| Design | New surface | Tradeoff |
|---|---|---|
| A (recommended) | Additive compact constructors with existing `CreateOptions` plus a precision argument; separate named-collection creation and accessors | One enum, no wrapper types; explicit method call is the store opt-in |
| B | Same operations, but a new `CompactCreateOptions { options: CreateOptions, default_precision: VectorPrecision }` wrapper | Groups arguments but adds a public construction contract without additional behavior |

Recommend A: reuse existing backing-specific patterns without changing exhaustive
`CreateOptions` construction (`src/lib.rs:935-944`). Neither design puts precision
in mutable `CollectionSettings` (`src/collection.rs:27-50`): replacing settings
with defaults must never convert float16 back to float32 or vice versa.

## Exact PROPOSED signatures

`VectorPrecision` is defined in private `collection`, re-exported at crate root
alongside current collection types (`src/lib.rs:48-51`). No existing type gains
a field. Default is implemented explicitly as `Float32`.

```rust
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorPrecision { Float32, Float16 }
impl Default for VectorPrecision {
    fn default() -> Self { Self::Float32 }
}

impl Database {
    pub fn create_compact(
        path: impl AsRef<Path>, dimensions: usize,
        options: CreateOptions, default_precision: VectorPrecision,
    ) -> Result<Self>;
    pub fn create_compact_directory(
        path: impl AsRef<Path>, dimensions: usize,
        options: CreateOptions, default_precision: VectorPrecision,
    ) -> Result<Self>;
    pub fn create_compact_in_memory(
        dimensions: usize, options: CreateOptions,
        default_precision: VectorPrecision,
    ) -> Result<Self>;
    pub fn create_collection_with_precision(
        &mut self, name: &str, spec: CollectionSpec,
        precision: VectorPrecision,
    ) -> Result<CollectionInfo>;
    pub fn is_compact(&self) -> bool;
    pub fn collection_precision(&self, name: &str) -> Result<VectorPrecision>;
}

// In public module vlite::maintenance:
pub fn convert_to_compact(
    source_path: impl AsRef<Path>, destination: impl AsRef<Path>,
    precisions: &[(&str, VectorPrecision)], key: Option<&EncryptionKey>,
) -> Result<Inspection>;
```

The signatures use existing root imports and `std::path::Path`; the maintenance
return type is existing `maintenance::Inspection` (`src/maintenance.rs:22-31`).
Precision applies to primary and named **dense** vectors, including model-history
versions; sparse vectors retain their existing float32 representation. No new
field is added to `VectorSpec`, `Record`, `AnnOptions`, `CollectionSpec` or
`CollectionInfo`. Use `collection_precision(&info.name)` when listing.

```rust
use vlite::{CollectionSpec, CollectionSettings, CreateOptions, Database,
            VectorPrecision};
let mut db = Database::create_compact(
    "half.vlite", 3, CreateOptions::default(), VectorPrecision::Float16)?;
db.put(1, &[0.1, 0.2, 0.3], b"payload")?; // legacy API, canonical stored f32
db.create_collection("exact", CollectionSpec {
    dimensions: 3, settings: CollectionSettings::default(),
})?; // always Float32, NOT inherited from default
db.create_collection_with_precision("small", CollectionSpec {
    dimensions: 3, settings: CollectionSettings::default(),
}, VectorPrecision::Float16)?;
assert_eq!(db.collection_precision("default")?, VectorPrecision::Float16);
db.set_collection_settings("small", CollectionSettings::default())?;
assert_eq!(db.collection_precision("small")?, VectorPrecision::Float16);
db.close()?;
vlite::maintenance::convert_to_compact(
    "half.vlite", "converted.vlite",
    &[("default", VectorPrecision::Float32)], None)?;
// With a caller-supplied owned EncryptionKey named key:
let encrypted = CreateOptions { encryption: Some(key), ..CreateOptions::default() };
let sealed = Database::create_compact_directory("sealed", 3, encrypted, VectorPrecision::Float16)?;
let memory = Database::create_compact_in_memory(3, CreateOptions::default(), VectorPrecision::Float32)?;
// Convert encrypted sources with Some(&source_key); output keeps that key.
```

Snippets are proposed caller code inside a function returning `vlite::Result`;
these new methods do not exist today.

## Behavior and compatibility matrix

| Operation/capability | Proposed behavior |
|---|---|
| All old constructors | Unchanged non-opted store, float32; formats/features/durability unchanged (`src/lib.rs:1209-1297`) |
| Compact file/directory/memory create | V5 only, default precision argument; path destinations absent; memory remains `Durability::None` |
| Encryption | All three creates support existing `CreateOptions::encryption`; conversion preserves encryption under supplied source key, never silently decrypts |
| Commit timestamps / V3-V4 compact requests | Explicit refusal; no silent option override. Legacy constructors still support their existing combinations |
| `create_collection` in any store | Float32, existing naming/limits/return shape; no store opt-in or precision inheritance |
| `create_collection_with_precision` | Requires an already opted-in store, even for Float32; same named-collection validation; `default` remains reserved |
| Default collection | Precision fixed by compact create, otherwise Float32; selectable by conversion name `"default"` |
| Settings replacement | Replaces only existing mutable settings; immutable precision survives default replacement, clone/edit and reopen |
| Open/read-only/open-from-bytes | Discover persisted opt-in/precision automatically; no new open knob. Legacy images report false/Float32 |
| Conversion source/destination | Closed complete V5 file or directory to absent **file**; memory conversion has no new API. Pack memory with `to_bytes` externally first |
| Directory conversion output | Use faithful existing `convert_to_directory[_with_key]` on the resulting file; no second precision choice |
| Unsupported build/backing/path | Explicit refusal, never eager float32 fallback, plaintext spill or precision downgrade |

`is_compact` reports persisted store intent, not just whether a source column is
currently present. Empty stores and empty collections must retain it. Precision
is immutable even while empty: drop/recreate a named collection or copy out.
Query vectors stay f32 and use existing validation, not half canonicalization;
exact scoring compares stored canonical values with unchanged f64 accumulation
and tie order. Immediate and reopened borrowed records expose identical f32 bits.

## Explicit conversion contract

Calling `convert_to_compact` is the store-level opt-in. Each unique named
override chooses destination precision; omitted collections preserve their
source precision (legacy means Float32), never inherit `"default"`. An empty
slice is format-only opt-in, not lossy conversion. Unknown names and duplicate
overrides fail before publication. Both precision directions require this call;
Float16 to Float32 does not restore discarded input bits.

Hold the existing exclusive read-only source lock (`src/maintenance.rs:72-81`).
Require completeness and preflight all canonicalization before publication.
Source bytes remain untouched on success or failure; destination must be absent.
Publish a revalidated, synced compact image using existing no-clobber semantics
(`src/maintenance.rs:594-624`), preserving encryption with fresh output salt.
No destination-key argument: intentional key change remains existing `reencrypt`.

Always assign a new lineage, including format-only conversion. Preserve IDs,
collection/graph high-water marks, settings, payloads, metadata, dense histories,
sparse values, attachments and **absolute TTLs**, including currently expired
state; this is not TTL compaction. Preserve current record/graph revision numbers,
but assign the destination a new lineage. Numeric `Expected::Revision` values
are not lineage-bound: callers must reread; conversion cannot automatically reject
a reused matching number (`src/lib.rs`, `Expected` and conditional writes).
Create a history floor at the source head (reserve sequence 1 for an empty head);
subsequent commits advance past it. Nonzero old-lineage feed cursors fail `CursorCompacted`;
replicas and external ETags require a fresh snapshot/identity, not a token transplant.
The zero-lineage beginning cursor is not an old-source identity; history-floor
validation still applies (`src/changes.rs:25-45`, `:195-208`, `:247`).
Old log history is not copied. Refuse open import jobs with `ImportConflict`;
preserve closed-job markers/high-water state. Refuse timestamped sources.

## Integration prerequisites (not implementation approval)

| Source entry points | Required integration |
|---|---|
| `src/lib.rs:1209-1389`, `:2499-2589`; `src/collection.rs:100-178` | Additive create routing, immutable internal descriptors, accessor health/name checks; settings replacement cannot overwrite them |
| `src/format.rs:861-953`, `:1442`; `src/compact/mod.rs` | Versioned persisted store/collection descriptors outside mutable settings; checked critical P3 kind, legacy defaulting, decode and writer dispatch |
| `src/directory.rs:911`; `src/lib.rs:1687`; `src/maintenance.rs:677` | Persist intent and precision atomically at creation and named-create commits, then through every checkpoint; pin generations |
| `src/lib.rs:1922-2131`; `src/collection.rs:433-494` | Shared precommit canonicalization for single, conditional and ordered batch writes; WAL and installed state use the same values |
| `src/import.rs:453-545`; `src/vectors.rs:633-772`; `src/property_graph/mod.rs:848` | Import validation/policy and raw submitted-digest retry identity; dense versions/reembedding and graph/vector atomic batches |
| `src/records.rs:55`, `:176-256`; `src/hnsw.rs`, `src/ann_snapshot.rs` | Canonical borrowed reads/snapshots, exact and ANN source access, precision-bound derived caches |
| `src/maintenance.rs:190-455`, `:594-720`; `src/logops.rs:25-180`; `src/replica.rs`, `src/changes.rs` | Faithful backup/restore/reencryption, compact/upgrade/directory conversion, graph enable/strip, feeds/export/replica, or explicit refusal before output |

**Blocking storage prerequisite:** decide/qualify atomic descriptor publication,
including an empty initial image and named collections created after its floor.
The accepted compact flag belongs only on history images containing blocks,
never WAL headers; op-12/settings and manifest formats remain unchanged.
Do not merely set the flag on an ordinary create header or append op-16 to WAL.
Old readers must refuse before replay, including empty opted-in stores.
Private numeric codecs alone cannot provide this path. Until every promised
backing has it, keep the public surface unshipped and the gate crate-private.
Faithful maintenance retains opt-in and precision; an already opted-in source is
not permission to silently opt in a legacy source. Rewrites may discard only
rebuildable derived blocks, not immutable descriptors or canonical source values.

## Existing-error mapping and review gates

| Failure | Existing `Error` only |
|---|---|
| Non-opted store / unsupported critical codec or backing | `UnsupportedFeature(16)` (compact flag) |
| Unsupported compact format / timestamps | `UnsupportedVersion(version)` / `UnsupportedFeature(2)` (timestamp flag; `src/format.rs:19`) |
| Bad dimensions / vector width / NaN, infinity or half overflow | `InvalidDimensions` / `DimensionMismatch` / `NonFiniteVector`; overflow must not commit infinity |
| Bad names, duplicate/existing collections, limits, reserved default | Existing `InvalidCollectionName`, `CollectionExists`, `TooManyCollections`, `DefaultCollectionImmutable` |
| Unknown conversion override / duplicate override / open imports | `CollectionNotFound` / `InvalidCollectionSettings("duplicate precision override")` / `ImportConflict` |
| Revision failure / corrupt descriptors or blocks / incomplete source | `RevisionMismatch` / `Corrupt { offset, reason }` / `IncompleteTail` |
| Encryption key/options failures | `EncryptionKeyRequired`, `EncryptionKeyMismatch`, `NotEncrypted`, `InvalidEncryptionOptions` |
| Read-only/locking, existing destination, I/O/publication uncertainty | `ReadOnly`, `Locked`, `Io` (including AlreadyExists), `CommitIndeterminate`, `Poisoned`, `PublicationIndeterminate`; no success-shaped fallback |

Baseline review inventory: one top-level `enum vlite::VectorPrecision` and one
`fn vlite::maintenance::convert_to_compact`; six new `Database` methods above.
No existing item removal, field edit or new `Error`. `scripts/api_surface.py:17-23`
indexes top-level items, **not methods/fields**: its future approved additions
must accompany downstream compile tests of unchanged constructors and exhaustive
`CreateOptions`, `Record`, `AnnOptions`, `CollectionSpec` construction, settings
clone/default replacement, borrowed reads and snapshot lifetimes. Compile the
proposed calls across default/no-default-feature and supported WASM configurations.
Do not edit the baseline for this proposal.

Qualification tests must cover persisted opt-in/precision for empty/populated
file/directory/memory and encrypted images; immediate/reopened canonical bits
using existing P3 fixtures; float32 oracle/tie order; named dense histories;
single/batch/graph/import paths, retry digests and existing reject/skip policies;
conditional revisions; rejected invalid/overflow atomicity (no rows, revision, marker or
graph change); TTL visibility/deadlines; snapshots; failure/crash publication;
backup/restore/restore-to/reencryption and faithful maintenance or refusal;
old/unsupported-reader rejection before replay/repair; source unchanged and new
conversion lineage. Codec readiness, these integration gates and explicit API
approval remain unresolved prerequisites; none is established by this document.

## Coordinator review

Design A is the preferred additive shape, not authorization to ship its API.
Existing `FeedCursor` carries lineage, whereas numeric conditional-write
revisions do not; the conversion contract above distinguishes them explicitly.
Finite input that would become infinity in the selected precision is rejected
as `NonFiniteVector` before commit; document that interpretation for callers.

Private codec review accepted `4870875`. For kind `0x0100` version 1, the
1 MiB component limit and 4,096-row limit are format constraints, not just
writer batching targets. Keep writer tuning within them; accepting larger
blocks requires a separately reviewed codec version and compatibility tests.

**HOLD public enablement and baseline changes** until a concrete, reviewed
descriptor-publication design covers empty stores and post-floor named creates
for every promised backing. It must spell out durable encoding, atomicity,
recovery, feed/export/replica behavior and old-reader refusal; any necessary
change to the accepted history-only compact contract requires a separate
explicit decision. Do not disguise this missing prerequisite as API approval.
