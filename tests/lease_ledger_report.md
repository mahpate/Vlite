# Independent lease-ledger reference

## Result and provenance

The standard-library-only Python oracle implements the unencrypted op-18 frame
codec, contextual replay, a deterministic lease state model, and a fail-first
mutation campaign. It changes no production code, existing oracle, existing
fixture, CI workflow, or CI registration.

Normative sources are `docs/projects/roadmap/gap2-lease-ledger.md` (read in full)
and `docs/projects/roadmap/serving-kv-ttl-contract.md` (including sections 1, 3,
and 5). No lease implementation source, production codec output, Rust executable,
or Rust test was used to obtain expected bytes or state.

The generic frame definition was needed: the inspected `src/format.rs` excerpts
were the prefix/footer constants at lines 94-95, the IEEE CRC implementation and
table polynomial at lines 483-533, and generic prefix/footer construction at
lines 636-659. These establish:

- 36-byte prefix: `VREC`, op at 4, little-endian collection at 5-6,
  reserved at 7, sequence at 8-15, ID at 16-23, count at 24-27,
  body length at 28-31, and prefix CRC at 32-35.
- Reflected IEEE CRC-32 polynomial `0xedb88320`, initial and final XOR
  `0xffffffff`; `CRC("123456789") == 0xcbf43926`.
- Prefix CRC covers bytes `[0, 32)`. Footer is `VCMT`, CRC of the entire
  prefix (including its stored CRC) plus body, then the repeated sequence.
  Footer bytes themselves are outside that CRC.

Golden body/prefix octets were transcribed from the design tables. Their
checksums were calculated once with a separate bit-at-a-time IEEE calculation,
not by importing the oracle. The checked-in hex strings are immutable test
expectations: self-test does not regenerate them. The runtime encoder uses
`struct` and `zlib`; the state model never calls that encoder for token
expectations.

## Counts and commands

| Evidence | Count |
|---|---:|
| Frozen exact-byte positive frames, covering all eight kinds | 12 |
| Malformed single-frame fixtures | 42 |
| Frozen replay vectors, including duplicate/unsorted snapshots | 11 |
| Frozen field/body boundary vectors | 11 |
| Every-byte proper truncations of positive frames | 1,432 |
| Named traces/check groups | 96 |
| Hand-built rule mutants killed | 38 |
| Mutation survivors / equivalent mutants | 0 / 0 |

Both commands below pass and report the same counts, including zero survivors:

```powershell
python -B tests\lease_ledger_oracle.py --self-test
python -O -B tests\lease_ledger_oracle.py --self-test
git diff --check
```

`--json` adds the mutant-to-killing-trace mapping and actual assertion failure;
`--list-traces` lists every named check. Assertions use explicit raising checks,
not Python's removable `assert` statement. Python 3.11 grammar parsing also
passes using `ast.parse(..., feature_version=(3, 11))`. The installed interpreter
is Python 3.15.0; an actual Python 3.11 runtime was unavailable.

## Coverage and fixture interface

`goldens.json` contains `name`, `kind`, `input` fields and `expected_hex`.
Unspecified input fields use the documented `Frame` defaults, including the
fixed UUIDv4 domain, auxiliary sequence 1, zero file-wide fields and inactive
state. Grant byte goldens use physical collection zero to avoid resolving
OPEN-L0-02. They include carried maximum high-water, both snapshot active
values, non-ASCII UTF-8 bytes and the maximum unsigned deadline.

`malformed.json` identifies a positive `base`, optional decoder context and
ordered byte `patches`, with `expected_error`. Offsets are absolute frame
offsets. Each patch refreshes the prefix and footer CRC unless explicitly
disabled, so structural vectors are not accidentally rejected just because
their CRC is stale. Integrity vectors deliberately disable the affected CRC
refresh. `replay.json` references ordered positive frames and specifies state
or an expected error. `bounds.json` declares field byte lengths, complete body
length, and an expected error for invalid cases. Its raw frame builder does
not call the encoder whose preconditions these vectors violate.

The model uses full `(physical collection, namespace bytes, key bytes)` tuple
addresses, immutable domains, file-wide high-water, inactive historical rows,
pending reservations, terminal recovery state, and an injected clock clamped
by `max(wall, last)`. A deliberately supplied equal-sum hash collision proves
that distinct full addresses remain distinct; it is not a claim about a
production hash function.

Named checks cover acquire/renew/revoke, same-holder requirements, live
conflicts without allocation, older/equal/newer tokens, deadline-minus-one /
deadline / deadline-plus-one, expired and revoked refusal, UTF-8 byte bounds,
TTL bounds and checked addition, token exhaustion, the 4,096-address bound
before writes, unavailable modes, ordinary-operation separation, downstream
highest-token enforcement, restart barriers, and mixed lifecycle monotonicity.
Renew can shorten a deadline and never allocates a token. Allocating another
address does not invalidate an older live grant.

Crash checks cover before reserve append; a synced reserve with no grant;
absent, complete and partial failed-sync outcomes for reserve/acquire/renew/
revoke; grant synced before response; absent/complete/partial startup-barrier
failure; and recovery-stamp publication failure. Complete frames may replay,
partial frames refuse normal open, explicit repair requires recovery, and a
failed open never returns a lease-capable handle. A separate lost-response
trace leaves the still-running owner healthy and the grant live: transport
ambiguity alone does not imply a storage failure or automatic retry.

Maintenance/replica checks are state-carry/stamp assertions only, not simulated
production converters, manifests, replication I/O or filesystem atomics.
Compaction, upgrade, re-encryption, directory conversion and checkpoint retain
domain, floor, rows, capability and ordinary sequence. Backup, restore, PITR,
directory restore, managed rollback/copy and promotion require terminal
recovery before exposure. Ordinary database writes remain possible in a
writable recovery-required model. No claim is made for unmanaged copies.
The model also checks old/new complete checkpoint carry states.

Replay checks preserve auxiliary physical order without advancing
`Loaded.sequence`, ordinary commit count or history floor; require the next
ordinary sequence, including nonzero checkpoint floors; reject ordinary
commits with a pending reservation; allow EOF reservations to burn; reject
duplicate/unsorted snapshot addresses; and reject an unconsumed timestamp
anchor beside op 18. Full CRC verification precedes schema dispatch. Older
reader refusal is modeled at the capability gate before replay/repair.

The largest legal v1 body is 1,598 bytes. Bodies of 2,047 and 2,048 bytes
cannot satisfy the individual v1 field limits, despite passing the prefix's
2,048-byte allowance; both refuse as structural corruption. A 2,049-byte
declared body refuses before reading/allocating the body, even when only a
prefix is supplied.

`Truncated`, `Poisoned`, `ResponseLost`, and `InjectedBeforeAppend` are internal
oracle observations, not proposed Rust error variants. The oracle's API
checks flatten the specified lease/storage outcomes into symbolic codes.
`QuotaExceeded` is the symbolic admission outcome from the serving contract,
not an invented Rust enum arm (OPEN-L0-05). Collection/type and non-V5 refusals
assert rejection without fixing an unspecified public error class.

## Mutation evidence

Each row names the actual changed rule and the check that kills it. Failures
are either an explicit state/result assertion or a specified error expectation.
Unexpected Python exceptions are not counted as mutation kills.

| Mutant | Named killing trace/check |
|---|---|
| `clock_rollback` | `clock_rollback_cannot_resurrect_observed_expiry` |
| `ttl_zero` | `ttl_zero_is_invalid_without_writes` |
| `ttl_max` | `ttl_maximum_and_over_maximum` |
| `deadline_wrap` | `checked_deadline_overflow_and_exact_u64_edge` |
| `reserve_after_grant` | `synced_reservation_burns_token_before_grant` |
| `restart_high_water` | `restart_barrier_invalidates_grants_preserves_domain_and_floor` |
| `revoke_high_water` | `revoke_preserves_floor_and_successor_is_strictly_greater` |
| `publish_before_sync` | `crash_acquire_sync_complete` |
| `automatic_retry` | `ambiguous_after_grant_sync_never_automatically_retries` |
| `renew_token` | `renew_keeps_token_and_can_shorten_deadline` |
| `holder_match` | `renew_rejects_foreign_holder` |
| `domain_match` | `renew_rejects_foreign_domain` |
| `token_match` | `renew_rejects_foreign_token` |
| `expired_match` | `renew_rejects_deadline_plus_0` |
| `inactive_match` | `renew_rejects_revoked` |
| `hash_identity` | `physical_collection_namespace_and_collision_addresses_are_distinct` |
| `snapshot_order` | `snapshot_rows_sorted_unique_and_high_water_unchanged` |
| `clear_flag` | `compaction_carries_domain_floor_rows_flag_and_sequence` |
| `carry_domain` | `upgrade_carries_domain_floor_rows_flag_and_sequence` |
| `carry_high_water` | `checkpoint_carries_domain_floor_rows_flag_and_sequence` |
| `restore_ready` | `restore_stamps_terminal_recovery_before_publication` |
| `late_barrier` | `unexposed_handle_cannot_serve_before_barrier` |
| `periodic_only` | `acquire_reserves_and_syncs_before_publication` |
| `ordinary_writer` | `ordinary_record_kv_import_drop_never_write_ledger` |
| `character_count` | `utf8_byte_bounds_and_no_normalization` |
| `domain_format` | `domain_input_is_canonical_lowercase_uuid4` |
| `missing_flag` | `malformed_prefix_body_integrity_and_capability_vectors` |
| `prefix_crc` | `malformed_prefix_body_integrity_and_capability_vectors` |
| `footer_crc` | `malformed_prefix_body_integrity_and_capability_vectors` |
| `sequence` | `malformed_prefix_body_integrity_and_capability_vectors` |
| `body_bound` | `body_bound_edges_2047_2048_2049_and_max_valid_1598` |
| `field_bounds` | `body_bound_edges_2047_2048_2049_and_max_valid_1598` |
| `zero_token` | `malformed_prefix_body_integrity_and_capability_vectors` |
| `zero_deadline` | `malformed_prefix_body_integrity_and_capability_vectors` |
| `older_reader` | `older_reader_refuses_capability_before_replay_or_repair` |
| `feed_skip` | `change_feed_remains_fail_closed_on_op18` |
| `cap` | `distinct_address_cap_includes_inactive_and_existing_renew_revoke_work` |
| `reservation_required` | `reservation_requires_immediately_matching_acquire` |

The shared matching rule covers both renew and revoke; separate unmutated
traces exercise both operations. The sync rule models exclusion from periodic
flusher/group-commit-only acknowledgement. Production-only mutations (routing
real Rust callers through a writer, actual AEAD corruption, or CI/API/site
gate removal) cannot be performed by this independent Python reference.

## OPEN items: no invented byte expectations

### OPEN-L0-01: encrypted frames

Design section 3.3: "Op 18 is encrypted exactly like other frames" and
"authenticated ciphertext on disk when the file is encrypted." Section 9 asks
for "known CRC/footer and encrypted-frame fixtures." The standard library has
no suitable AEAD implementation. No home-built cipher, ciphertext/tag golden,
encrypted round-trip or authentication claim is supplied. Only plaintext CRC
framing and verification-before-schema ordering are checked.

### OPEN-L0-02: collection prefix contradiction

Section 3.2 prefix table says `collection | 0`. Its next paragraph says
"`frame.collection` is the physical `u16` collection ID for grant kinds and
zero for all file-wide kinds." A positive byte vector using a nonzero grant
collection would choose between these statements. All grant byte goldens use
physical collection zero; neither acceptance nor refusal of nonzero grant
prefixes is frozen. The state model still distinguishes physical collection
IDs as explicitly required by sections 2 and 5.3.

### OPEN-L0-03: replay cannot prove temporal grant preconditions

Section 3.2 says Acquire "Replaces only an expired/inactive row" and Renew
requires "`now < prior deadline`; new deadline is checked `now + ttl`."
The same section's 64-byte header table stores a deadline but no event `now`
or TTL. Auxiliary op 18 is also "exempt from the mandatory time-anchor check."
Therefore a byte-only replay cannot distinguish all live-overwrite or
bad-renew-arithmetic histories. The API state model enforces these rules with
its injected clock; replay checks structural deadlines, positive tokens and
matching durable identities. No byte golden invents an event clock, infers
expiry from today's wall clock, or claims to prove missing deadline arithmetic.
Zero grant deadlines are rejected because checked unsigned `now + positive
ttl` cannot produce zero; maximum-u64 deadlines remain valid.

### OPEN-L0-04: recovery-required snapshot restatement order

Section 3.2 requires DomainInit "state `Ready`, flags `0`" and describes
RecoveryRequired as "terminal state bit set, all grants inactive." Section 8
requires carry output to retain "the same domain/high-water and every grant
row" in "`DomainInit` + `SnapshotGrant` op-18 frames". It does not specify the
placement of a recovery marker when rewriting an already-terminal ledger, or
whether snapshot rows may occur after that terminal marker. Ready snapshots
and terminal state-carry assertions are covered separately. No terminal
snapshot stream ordering is frozen.

### OPEN-L0-05: quota and collection error taxonomy

Section 2 says the distinct-row cap "refuses a new distinct address"; the
serving contract section 10 says exhaustion is "`quota_exceeded`".
Section 5.1's complete proposed `LeaseError` enum has `LeaseHeld`,
`StaleFencingToken`, `LeaseUnavailable`, `FencingRecoveryRequired`,
`InvalidInput`, and `Storage`, but no quota variant. It also says
"`lease_store` requires ... a one-dimensional existing collection", without
giving the exact typed lease errors for missing/wrong-dimensional collections.
Cap refusal, no allocation/write, and non-creating collection refusal are
checked. No Rust quota arm or collection error mapping is frozen.

### OPEN-L0-06: non-V5 flag refusal class

Section 3.1 says "reject the flag on a version other than V5", but does not
name that refusal's exact error class. The exact
`UnsupportedFeature(0x0080)` older-reader and unknown nonzero body-schema
outcomes are separately specified and tested. Non-V5 flag checks assert
refusal only; there is no fixed non-V5 error golden.

### OPEN-L0-07: invalid clock read outcome

Section 5.3 says "Update `last_lease_clock_ms = now` only after a valid clock
read" but proposes a test source "`FnMut() -> u64`", which cannot express a
fallible read, and specifies no invalid-read error mapping. The model rejects
an injected out-of-range clock without updating its clamp. It does not freeze
a production error class or invent an acknowledged clock fallback.

## Proposed CI integration, not applied

In the workflow's existing pre-routing oracle step, add these Linux CI lines:

```sh
python tests/lease_ledger_oracle.py --self-test
python -O tests/lease_ledger_oracle.py --self-test
```

Register `"lease_ledger_oracle"` in the existing oracle tuple in
`tests/ci_workflows.py`'s `test_graph_oracles_run_before_ci_routing`, retaining
the assertion that the plain command precedes `ci_policy.py route`. Add an
equivalent assertion for the optimized command before routing. Neither
`ci.yml` nor `ci_workflows.py` is edited here.

Changed deliverables are `tests\lease_ledger_oracle.py`, this report, and
`tests\fixtures\kv\lease-ledger-v1\{goldens,malformed,replay,bounds}.json`.
All are new files with consistent CRLF line endings. There are no dependency
additions.

Checks not run: Python 3.11 runtime execution (not installed); AEAD fixtures
and encrypted integration (OPEN-L0-01); Rust/cargo, production acceptance,
maintenance I/O, API/site gates, and CI integration (outside this Python-only
scope). The Python reference and its own mutants are not evidence that a
production ledger passes acceptance.
