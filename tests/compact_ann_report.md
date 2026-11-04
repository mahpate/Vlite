# P3c C1 independent reference: partial, blocked on OPEN-ANN-01

This is not a frozen contract, accepted codec or complete C1 qualification.
The reference emits `status: partial_open_byte_rule` and names the blocker.
Normal codec checks use ok/corrupt/stale/unsupported_version/unknown_kind.
The additional `open` result is a reference-only unresolved-rule diagnostic:
it is never a proposed engine error, wire status or corrupt-source finding.

## OPEN-ANN-01: scalar minimum bits for mixed signed zeros

Sources:

- `docs/projects/roadmap/p3c-compact-ann.md:193-207`: persist f64 minima,
  reuse existing scalar training and independently bit-check trained bounds.
- `docs/projects/roadmap/p3c-compact-ann.md:117-120`: deterministic bodies
  for identical logical inputs and identical frame positions.
- `src/quantize.rs:94-123`: training uses `low.min(value)` and
  `high.max(value)` without a signed-zero tie rule.
- [Rust's documented f64 minimum intrinsic](https://doc.rust-lang.org/std/intrinsics/fn.minimum_number_nsz_f64.html):
  equal inputs including +0/-0 may return either input nondeterministically;
  its stabilized version is `f64::min`.

For ascending-ID L2 source rows containing [+0] and [-0], either zero sign
is a valid numeric minimum, but its persisted f64 bits differ. Python's
first-equal `min` would silently pin an additional rule not guaranteed by
Rust's arithmetic contract. Reordering source rows would not settle Rust's
per-operation unspecified tie result. C's deliberate signed-zero preservation
does not define the quantizer minimum's sign.

**Conservative handling:** training returns reference-only `open` when the
final minimum is zero and both zero signs participated in that dimension.
No minimum bytes are chosen for that case. Checks cover both source
precisions and both row orders. Cosine's all-zero excluded rows still have
the specified empty-training +0 parameters and do not trigger this case.
Single-sign zeros and negative minima do not have this tie ambiguity.

**Proposed ruling, not adopted:** canonicalize zero-valued persisted ANN
minima to +0 after existing numeric training, on encode and bind. Keep C's
source signed-zero bits unchanged. Alternatively specify a sign-aware
minimum rule and its bind behavior. Either needs an explicit codec ruling;
do not change public ScalarU8 behavior by implication.

**Pinned scenario:** `OPEN-ANN-01` in the reference. Resolve this item before
claiming a byte-complete oracle or implementing a codec from these fixtures.

## Settled scope represented so far

The envelope and 224-byte binding, settings, topology/scalar/rotated layouts,
B/G/C/set transcripts and envelope guard have independent hand ledgers.
There are ten frame-body ledgers (five per precision), 123 malformed-case
definitions run for each precision, partition arithmetic cases, the capped
level rule, scalar training/half-away coding/norms, logical atomic binding,
foreign-set and same-prefix/full-B checks, end-body checks and 19 named
mutants with restore-to-pass verification.

No P3c Rust implementation was read, written or run. Rotation computation
is excluded; rotated bind compares against explicit independent row witnesses,
not against an implementation of F. Synthetic source sections model D's
opaque hash inputs, not primary source-codec validity. The generic endian
mutant substitutes big-endian packing to simulate a non-little-endian host.

## Remaining before full delivery

Resolve OPEN-ANN-01 and add its approved byte goldens. Finish an explicit
coverage inventory and any missing C.5/source/entry/cross-partition and
precision/numeric boundary witnesses. Complete independent tampered-expected-
golden checks and standalone flat hex artifacts if requested for review.
Do not equate this partial reference's green self-test with full acceptance.

This reference does not implement a file reader, AEAD/checksum validation,
the complete capability table, actual installation or source-generation
seams. `image_complete` is an externally supplied precondition; the separate
end-body helper validates bytes but is not a whole-image reader. All file,
encryption, lifecycle, rebuild, overlay, cache, threading and performance
qualification remains outside this reference. No feature is activated.
