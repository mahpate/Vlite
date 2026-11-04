# P3c C1 independent byte fixtures (partial)

These are reference inputs and hand-assembled byte ledgers, not Rust output.
The reference is incomplete pending OPEN-ANN-01 in `tests/compact_ann_report.md`.
No format or implementation acceptance is claimed.

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
123 individually named one-defect repairs of the f32 and f16 ledgers.
`at` replaces bytes, `truncate` retains a prefix, `append` adds bytes.
An unsupported codec also updates prefix version to keep framing coherent.
Default expected outcome is `corrupt`; explicit exceptions are version skips.
The original unmodified ledger is the repair for each malformed case.

`manifest.json` records SHA-256 of each JSON document's canonical content:
UTF-8/ASCII, sorted keys, compact separators, ASCII escapes and no NaN
(`json.dumps(sort_keys=True,separators=(",",":"),ensure_ascii=True,
allow_nan=False)`). No newline participates. The manifest is not included
in its own digest. The named mutation map is checked against the campaign;
each mutant must fail its named scenario and the restored scenario must pass.
The endian mutant simulates an opposite-endian host because a native-endian
cast on this little-endian host would otherwise be observationally equivalent.

Outer frame authentication, capabilities and source validity are explicit
caller preconditions. `check_image_end` validates actual end-body bytes and
prefix transcript; `bind`'s `image_complete` flag is only an external
qualification precondition, not an image reader. Do not treat it as proof of
checksum, AEAD, capability-table or file-level validation.
