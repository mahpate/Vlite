# P3c C1 independent byte fixtures

These are reference inputs and hand-assembled byte ledgers, not Rust output.
R-HP5-01 (PROVISIONAL) resolves OPEN-ANN-01 with canonical persisted +0
minima. No format/implementation acceptance or activation is claimed.

`goldens.json` has schema `vlite.compact-ann-bytes`, version 1. Its frame
ledgers describe plaintext op-16 **bodies**, with prefix identity/count in
the fixture wrapper, not authenticated full-file frames. Both f32 and f16
source transcripts carry literal B/G/C digests. Source sections are opaque,
already-checked primary section inputs to D's hash grammar; these synthetic
sections do not qualify the independently owned primary source codec.

Every frame ledger has a literal envelope/table, exact body length and
absolute body-relative section offsets. Unlisted bytes are zero. `$guard`
is the first eight raw B bytes, `$precision` is one byte, and `$binding`
concatenates `binding_parts`. Other substitutions select explicit source,
settings, parameter and set-digest hex. No encoder supplies golden offsets.

Hand byte ledger: descriptor sections 64/320/384, ending at 408 or 424;
topology sections 128/192/256/320/384, ending at 384 (empty neighbors);
scalar sections 64/128/192, ending at 200; rotated sections 64/128/192,
ending at 224. Binding is exactly 224 bytes; settings 32; scalar parameters
24 for d=1; rotated parameters 40. Reassembly was separately cross-checked
using fixed offsets and SHA-256, without importing the reference.
The same topology/code payloads give the same set digest for both precisions.

The scalar witness is one constant component 1: minimum 1, scale +0,
code 0, decoded norm 1. The rotated witness supplies sign byte 01 and
terms (1,1,+0,+0) explicitly. It is a layout/reconstruction-input witness,
**not** a claim that the excluded rotation algorithm produces those terms.
The 768-dimension check separately pins 96 sign bytes and signed-zero terms.

Partition sizes include envelope/table, five or three times 63 worst-case
padding bytes, and fixed section headers/sentinels. Topology fixed overhead
is 427; code fixed overhead 261. Synthetic single-row sizes near 4 MiB test
admission arithmetic, not a claim that d<=65,536 can produce such a code row.

`malformed.json`, schema `vlite.compact-ann-malformed`, version 1, lists
127 individually named malformed/version definitions per source precision:
123 original definitions plus four in `minima_cases`.
`at` replaces bytes, `truncate` retains a prefix, `append` adds bytes.
An unsupported codec also updates prefix version to keep framing coherent.
Default expected outcome is `corrupt`; explicit exceptions are version skips.
The original unmodified ledger is the repair for each malformed case.

`scalar-minima.json` adds four d=3 scalar parameter/descriptor ledgers per
precision. The two tie cases traverse the same stable-ID rows in opposite
training orders; both hold their binding fixed and produce identical bytes.
All-zero constants require +0 minimum/scale and code 0; [-2,0,1] constants
pin nonzero negative minima. These eight instances are isolated unbound
pending encode witnesses, not complete source-bound file images. Their
fixed binding has collection 7, d=3, precision 1/2, L2/scalar/state 0,
lineage 9, sequence 11, physical count 2, N=P=Q=0, entry ffffffff,
m=ef=2, max level 0, quantizer/build version 1, both seeds 0 and zero
set digest. Synthetic B/C are zero; G is independently SHA-256 of
`VLCANN-GENERATION-1`, little-endian 9/11/7 and 32 zero B bytes.
Binding and settings start at 64 and 320; parameters at 384 end at 440.
There are 56 header/table bytes and zero gaps of 8/32/32 bytes. Parameters
are header `0300000000000000`, 24 minimum bytes and 24 +0 scale bytes.

The three positional -0 definitions patch body offsets 392/400/408.
The bind negative uses a complete zero set, changes its first minimum
and independently reseals its set digest; retrained source minimum is +0.
The positive bind case in `scalar-minima.json` uses source -0 and stored +0.

`boundaries.json` pins separately named semantic bind cases, independent
small-ID level worksheet outputs, f32/f16 finite/nonfinite/width witnesses,
unsupported-layout boundaries and three deliberate expected-golden
tamper kinds. Tampered expected norm/ordinal/B comparisons bypass manifest
hashes; they must independently fail and restore to pass in both precisions.
Additional actual count/size/source/numeric boundary bundles live in the
Python reference and are explicitly inventoried in `tests/compact_ann_report.md`.

`manifest.json` records SHA-256 of all four fixture JSON documents' canonical content:
UTF-8/ASCII, sorted keys, compact separators, ASCII escapes and no NaN
(`json.dumps(sort_keys=True,separators=(",",":"),ensure_ascii=True,
allow_nan=False)`). No newline participates. The manifest is not included
in its own digest. The named mutation map is checked against the campaign;
each mutant must fail its named scenario and the restored scenario must pass.
The campaign has 24 injected mutants, including all five R-HP5-01 failures;
plain and optimized runs each require zero survivors.
The endian mutant simulates an opposite-endian host because a native-endian
cast on this little-endian host would otherwise be observationally equivalent.

Outer frame authentication, capabilities and source validity are explicit
caller preconditions. `check_image_end` validates actual end-body bytes and
prefix transcript; `bind`'s `image_complete` flag is only an external
qualification precondition, not an image reader. Do not treat it as proof of
checksum, AEAD, capability-table or file-level validation.
