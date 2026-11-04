# P3c C1 independent Python codec reference

HP-5 is complete within its Python-only C1 scope, under coordinator ruling
**R-HP5-01 (PROVISIONAL)**. This is a reference candidate, not independent
acceptance of an engine codec, a format activation or full P3c qualification.
The contract is `docs/projects/roadmap/p3c-compact-ann.md`, C, D, F.1, H and
I/C1, supplemented by the ruling in the read-only HP-5 assignment.
No Rust source or output supplied expectations.

## R-HP5-01: OPEN-ANN-01 resolved provisionally

Persisted zero scalar minima are +0. Training and descriptor encode
canonicalize -0 minima; decode rejects a stored -0 minimum as malformed.
Bind compares persisted parameters with canonicalized retrained minima.
Nonzero negative minima are preserved. Constant scales remain +0 and codes
remain 0 for both source zero signs. C continues to preserve source bits.
This changes neither public ScalarU8 nor rotation term signed-zero bits.
There is no remaining reference-only `open` outcome.

`scalar-minima.json` hand-pins d=3 parameter bytes and pending descriptor
bytes for both training iteration orders of the same [+0,-0] rows, all-zero
constant dimensions, and constants [-2,+0,1]/[-2,-0,1]. Stable IDs stay with
their rows across iteration reversal. Both orders produce byte-identical
descriptors with the same fixed binding; ordered source content also agrees.
This does not assert identical B/C after swapping stored values between IDs:
that would change the logical source. These are isolated unbound pending
encode ledgers, not additional complete source-bound image goldens.

The fixed descriptor ledger is 440 B: 56 B envelope/table, 8 zero pad,
binding at 64..288, 32 zero pad, settings at 320..352, 32 zero pad,
parameters at 384..440. Parameters contain d=3, reserved=0, three f64
minima, then three +0 scales (56 B). +0 is eight zero bytes, -2 is
`00000000000000c0`, +1 is `000000000000f03f`; codes are `000000`.
For the fixed synthetic B=32 zero bytes, G is the literal independently
hashed generation transcript (lineage 9, S=11, collection 7). The fixture
README records the remaining fixed fields.

Stored -0 minima are independently rejected in first, middle and last
dimensions. A resealed complete scalar set with stored -0 and retrained +0
is rejected specifically at `noncanonical scalar minimum`, not checksum.
A valid source -0/stored +0 bind succeeds and kills raw-bit retraining.

## Explicit coverage inventory

Each name in the self-test registry is counted once; bundled assertions
inside a name, repeated restore checks and mutation trials are not extra
named checks. Plain and optimized execution each report **415 named checks,
18 body-ledger instances, 127 malformed/version definitions per precision,
24 mutants killed, 0 survived, 0 equivalent, and no open byte items**.
The 127 definitions comprise 120 corrupt results and 7 unsupported-version
results per precision; additional semantic negatives are inventoried below.

| Registry category | Named checks | Evidence |
|---|---:|---|
| Original complete source/descriptor/topology/code goldens | 2 | Five hand body ledgers per precision; literal B/G/C transcripts, SHA and set digests; scalar and explicit-witness rotated atomic bind |
| Ruling parameter/descriptor goldens | 8 | Four d=3 ledgers per precision; both zero tie orders, all-zero, nonzero negative minima |
| Ruling positive canonical bind | 2 | Source -0, stored +0, installed atomically |
| Malformed/version fixture definitions | 254 | 123 original plus four zero-minimum definitions, each at both precisions |
| Original named bind negatives | 32 | Missing/duplicate/out-of-order partitions or descriptor; incomplete image; foreign B/G/C/config; same eight-byte prefix/different full B; unsupported atomic skip; known corruption wins; guard and resealed retraining mismatches |
| Additional bind boundaries | 44 | 22 per precision: two valid, 14 corrupt, six stale; topology/code splits differ; gaps/overlaps, global IDs, source ID/revision/ordinal, wrong-level links, recomputed levels, entry tie/max, eligibility and reconstructed codes |
| Numeric source fixtures | 14 | Seven per precision: positive/negative smallest subnormal and largest finite constant bounds; NaN, both infinities and wrong width |
| Unsupported-layout boundaries | 14 | Short unknown binding/settings/family/algorithm layouts skipped without assuming v1 inner lengths; known bad entry/minimum/settings still corrupt |
| Independently tampered expected goldens | 6 | Expected norm bit, source ordinal and source B changed, at both precisions; byte/transcript comparison fails without consulting manifest |
| Empty/pending bundles | 2 | Each checks state 0 with physical rows, complete empty source and all-zero cosine N=0 |
| Source transcript boundary bundles | 2 | u64-max IDs/revisions/expiry, present-zero versus absent expiry, duplicate/descending IDs and row cap before iteration |
| Scalar numeric boundary bundles | 2 | Exact 0..255 scale, half-away adjacent f64 witnesses, norm of decoded (3,4)=5, cosine exclusion and eligible zero-coordinate canonicalization, nonfinite encode refusal |
| Greedy partition fixtures | 10 | Empty; 256/257 nodes; 4096/4097 codes; exact/over 1 MiB target; single-row exact/over 4 MiB hard cap |
| Level fixtures | 3 | First odd output, ID/seed XOR and level-16 cap |
| Standalone malformed topology lists | 6 | Neighbor N, unsorted/self/duplicate links and degree above level-0/upper caps |
| Existing arithmetic/layout bundles | 7 | Padding 64 rejected; half-away/clamps; signed-zero C; 768 bits = 96 B; checked-u64/body caps/file positions; actual end bytes; canonical zero/cosine empty bounds |
| Actual upper-bound layout bundles | 7 | d=65536; 256 nodes/L=4352; E=1152 accepted/E=1153 refused; 4096 code rows; 1/64 sections and 63-byte padding; physical=1,000,000/m=64/ef=4096/rerank=1,16 |
| **Total** | **415** | Manifest hashes cover all four JSON fixture documents |

Envelope defects include prefix disagreement, flags/reserved/high object
bits, section counts, offset/length maxima, overlap, nonzero padding,
trailing bytes and truncation. Descriptor defects cover bounded fields,
settings and scalar/rotation parameter representations. Topology lengths,
sentinels, L/E/r bounds and node fields precede row access. Code lengths,
counts, dimensions, constant codes, finite terms, recomputed scalar norms
and unused rotated sign bits are checked. Actual end-body tests cover the
prefix transcript/count and truncated/corrupted ends; they are not an image
authentication implementation.

Unsupported binding/settings/rotation discriminators are read before
assuming the unknown inner layout's v1 size. Supported binding and parameter
invariants still take precedence over a recoverable settings-version skip.
The corresponding seven boundary definitions pin this C.5 behavior.

## Mutation evidence

The complete 24-entry mutant-to-case map is committed in `manifest.json`
and reproduced in CLI output. Every named killer is first green, fails with
its mutation active, and passes after restoration. New ruling mutations:

| Mutant | Named killer |
|---|---|
| `encode-raw-minimum` | `all-zero-constant-p1` (supplied -0 parameter must encode as +0) |
| `accept-negative-zero-minimum` | `minimum-negative-zero-middle-p1` |
| `bind-raw-minimum` | `bind-canonical-retrained-minus-zero-p1` |
| `first-equal-minimum` | `zero-tie-minus-first-p1` |
| `canonicalize-negative-minimum` | `negative-minimum-preserved-p1` |

The first-equal mutant is the rejected implementation that persists Python's
first-equal result without final canonicalization. Changing only an internal
zero-tie choice while retaining canonicalization is observationally equivalent
and is not a surviving injected mutant. The endian mutant explicitly packs
big-endian bytes to model an opposite-endian host: literal native-endian
packing on this little-endian host would also be equivalent.

## Exclusions and open items

OPEN-ANN-01 is closed by the provisional ruling; no OPEN-ANN-02 or later
unspecified byte rule was found. Ratification/independent acceptance of the
provisional ruling remains a coordinator decision, not an oracle blocker.

Rotation numerics, RNG permutations/sign generation, rotated row arithmetic
and approximate scoring are not modeled. Rotated bodies are structural
fixtures and bind accepts only explicitly supplied independent row witnesses.
Primary source section validity is a caller precondition; synthetic opaque
source sections exercise D's hash grammar, not the primary source codec.

This Python oracle is not a production bounded-memory file reader. Frame
authentication/checksum, capabilities, real image assembly/installation,
encryption, generation pins, storage lifecycle, cache/budget admission,
overlay/search/rerank, worker races, native/WASM reproducibility, recall and
performance remain outside HP-5. `image_complete` is an externally supplied
qualification precondition. None of those excluded tasks is claimed green.
No shared dispatch, public API, gate, CI registration or activation changed.

## Verification

Normal and optimized ANN self-tests pass. All 16 required normal/optimized
float16 and graph oracle regression invocations pass. The independent
minima worksheet validates the eight new descriptor ledgers without
importing the oracle; Python 3.11 grammar parsing passes.

The installed interpreter is Python 3.15.0; only that runtime is available
(`py -0p`). Actual Python 3.11 execution could not be run. The reference
uses only Python 3.11 syntax and standard-library APIs, but grammar checking
is not claimed to replace that runtime check.

Changed files contain no CR bytes, and `git diff --check` passes. No Rust,
cargo, dependency install or prohibited Git state-changing operation ran.
