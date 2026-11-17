# Gateway scale, revocation and drain test plan

Test plan for the shared gateway in the staged Azure Container Apps (ACA) HA plan ([PRD](feature-aca-ha.prd.md), EPIC-004 ITEM-017). It covers TEST-003 (statelessness and scale), TEST-004 (revocation and suspension staleness) and TEST-005 (drain). It says what is already tested, what each scenario must show, and what must exist before the scenarios can run. Design background is in [gateway.md](gateway.md).

> **Status (2026-11-17): plan only, NOT RUN.** No gateway mode exists, so no scenario below has been executed and nothing here is evidence for GATE-003. Execution waits for ITEM-014 (routing and envelope minting), ITEM-015 (readiness and drain) and ITEM-016 (rate-limit share), which wait for owner decisions OD-012, OD-005 and OD-006. Production is unchanged.

## Inputs, not defaults

The scenarios take these values as inputs. This plan sets none of them; a run records the values it used.

| Input | Source | State today |
|---|---|---|
| S_ctl and the control-state poll interval | OD-005, from MEAS-005 | Undecided; no default |
| Gateway minimum and maximum replicas | OD-006, from MEAS-004 and a quota check | Minimum 2 warm is a proposal; maximum unmeasured; scale-to-zero is not allowed (FR-004) |
| Termination grace period | ACA revision setting | ACA default 30 s; the maximum is unverified |
| Readiness-propagation delay after SIGTERM | ITEM-015 design | Does not exist yet (see Gaps) |
| Envelope lifetime and clock-skew bound | Envelope configuration (ITEM-010) | Set per run; the bounds are already enforced by library tests |
| Load shape (request mix, concurrency, duration) | Chosen per run from MEAS-004 | Recorded, never tuned to make a run pass |

## Test tiers

| Tier | What runs | Where | Default in CI |
|---|---|---|---|
| (a) Library and in-process router | Existing unit and router tests (below) | `cargo test` | Runs |
| (b) Local multi-process | Several gateway processes, one writer, optional readers, control state in Azurite | Developer machine or CI runner | Ignored, like `tests/cloud_backup_azurite.rs`; opt in through an environment variable |
| (c) ACA rehearsal | Gateway app with N replicas in a non-production ACA environment | Owner-approved non-production subscription | Never; needs owner approval for each run |

Tier (b) cannot share the in-memory `ObjectStore` fake across processes, so it uses Azurite (the `VLITE_AZURITE_BLOB_ENDPOINT` pattern). "Control state unreachable" at tier (b) means stopping Azurite or pointing one replica at an endpoint that refuses connections. Fine-grained fault injection (a store that fails before a given operation) stays at tier (a) through `Op` and `Fault::Before`.

Tier (b) processes are spawned like `tests/server_cli.rs` does (`env!("CARGO_BIN_EXE_vlite-server")`) and stopped with its `stop()` helper: `kill -TERM` on Unix and `CTRL_BREAK` through `GenerateConsoleCtrlEvent` on Windows.

## Already covered at tier (a)

These pass today. They prove the building blocks, not a running gateway.

| Area | Test | File |
|---|---|---|
| Fail closed without a snapshot and after S_ctl; an unreachable store does not refresh the age; a backwards clock is stale (FR-008, FM-004) | `poller_fails_closed_without_a_snapshot_and_after_s_ctl` | `src/server/control_state.rs` |
| A lower generation is refused (RD-004) | `poller_refuses_a_generation_regression` | `src/server/control_state.rs` |
| Revocation and suspension reach a fresh poller; a backend whose floor is raised refuses an older envelope; a stale replica fails closed (library TEST-004) | `revocation_and_suspension_propagate_and_old_generations_are_refused` | `src/server/control_state.rs` |
| Admin mutations are CAS: exclusive create, lost-race retry, unknown outcome reported, contention exhausted | `create_is_exclusive_and_update_advances_generation_by_cas`, `update_retries_a_lost_race_against_the_new_state`, `update_reports_unknown_outcomes_and_exhausted_contention` | `src/server/control_state.rs` |
| Envelope generation floor rises and never falls; skew and lifetime bounds; forged, altered and expired envelopes refused | `generation_floor_rises_and_never_falls`, `expiry_and_issue_time_honour_the_skew_bound_exactly`, `lifetime_is_bounded_and_positive`, `forged_or_altered_envelopes_are_refused`, `expired_credential_is_refused_even_inside_the_envelope_window` | `src/server/principal_envelope.rs` |
| Trusted backends need a valid envelope and keep the highest generation seen (TEST-010, RD-004) | `trusted_mode_requires_a_valid_envelope`, `backend_holds_the_highest_generation_and_accepts_rotation_overlap`, `envelope_principals_are_authorized_like_scoped_credentials`, `standalone_default_is_unchanged` | `src/server/gateway_trust_tests.rs` |
| Writer and reader `/ready` answer 503 `draining` after `begin_drain` | `writer_serves_mutations_and_reports_its_mode` | `tests/server_serve_mode.rs` |
| A real process drains both listeners on the stop signal | `grpc_listener_reports_health_and_both_listeners_drain_on_signal` | `tests/server_cli.rs` |
| Operation classification matches routes and authorization (TEST-002) | `server::gateway::` tests | `src/server/gateway.rs` |

## TEST-003: statelessness and scale

**Setup.** At least the minimum replica count of gateways (an input), one writer per tenant under test, control state at a known generation, no session affinity at the load balancer. At tier (b) a round-robin test client picks the replica per request; at tier (c) ACA ingress does, with sticky sessions off.

| Step | Action | Pass | Fail |
|---|---|---|---|
| 3.1 | Steady load across all replicas | Every response matches what the same request gets from the backend directly (GATE-003 identity check) | Any difference in status, body or authorization outcome |
| 3.2 | Authenticate a sequence on replica A, continue it on replica B (pagination cursor, follow-up read, follow-up write) | Same results as on one replica | Any request that depends on the replica that served the previous one |
| 3.3 | Kill one replica (SIGKILL, not SIGTERM) under load | Only requests in flight on that replica fail; the client sees connection errors, not wrong answers; other replicas continue | Failures on surviving replicas, or any request answered with another tenant's data or authority |
| 3.4 | Restart the killed replica | It reports ready only after loading a control-state snapshot within S_ctl, then serves identically | Ready before a snapshot, or different answers |
| 3.5 | Add replicas up to the maximum under load | New replicas follow 3.4; no change in outcomes | Any authorization or routing difference on new replicas |
| 3.6 | Remove all but one replica, then add back | The writer keeps its authority throughout; no write is refused for authority reasons | Writer authority lost or moved |

**Evidence.** Replica count over time, per-replica request and error counts, the list of requests that failed with their replica and timing, and the identity-check diff (empty on pass).

## TEST-004: revocation and suspension staleness

**Setup.** At least two gateway replicas and a trusted backend. Pause one replica's poller (tier (a) or (b)) or block its store access so it keeps an older snapshot, while the other polls normally.

| Step | Action | Pass | Fail |
|---|---|---|---|
| 4.1 | Revoke a credential, suspend a tenant and change a memory binding through the admin path | Each mutation advances the generation by CAS; concurrent admin mutations either both apply in order or one reports a conflict | A lost update, or two mutations at the same generation |
| 4.2 | Measure time until every polling replica refuses the revoked credential (401) and the suspended tenant (`tenant_suspended`) | Every replica within S_ctl plus the recorded clock error (C-06) | Any replica later than that |
| 4.3 | Send an envelope minted before revocation (generation `A` below the backend floor) directly to the backend | Backend refuses it as stale (RD-004) | Backend accepts it |
| 4.4 | Keep the paused replica past S_ctl | It answers 503 `control_state_stale` for authentication decisions and fails readiness (FR-008, FM-004) | Any allow, or ready while stale |
| 4.5 | Make the store unreachable for every replica past S_ctl | Every replica fails closed with 503; none allows by default (C-07) | Any request authorized from an expired snapshot |
| 4.6 | Present an older generation to a replica after a newer one | Refused as a regression | Replica moves back to the older state |

**Evidence.** Mutation timestamps, the generation after each, per-replica first-refusal time, measured propagation (feeds MEAS-005 only when recorded as a measurement run), the clock error used, and the status codes from 4.3 to 4.6.

## TEST-005: drain

**Setup.** One gateway replica under test plus at least one other, a writer with a slow test route or large request so requests are in flight, and readiness polled every 100 ms or faster.

| Step | Action | Pass | Fail |
|---|---|---|---|
| 5.1 | Send SIGTERM while reads and writes are in flight | `/ready` answers 503 `draining` before the replica stops accepting connections (FR-009) | Listener closes while still reporting ready |
| 5.2 | Wait up to the grace period | All in-flight requests complete with their normal result | An in-flight request dropped while inside the grace period |
| 5.3 | Break the connection to the writer after the request bytes were sent (fault proxy closes without a response) | The client gets an error that states the outcome is unknown; the writer received the write exactly once (FR-007, RD-010) | The gateway retries, or reports success or a definite failure |
| 5.4 | Refuse the connection to the writer before any byte is sent | The gateway may retry, within its retry bound | A retry after any byte was sent |
| 5.5 | Count effects in the writer (change sequence, as read by webhooks, or record counts) | No duplicate effect for any write in 5.1 to 5.4 | Any write applied twice |
| 5.6 | Exceed the grace period with a request that cannot finish | The platform stops the replica; that request fails at the client; others are unaffected | Other replicas affected |

Tier (b) needs a small fault proxy between gateway and writer for 5.3 and 5.4. It is test code in `tests/`, not product code.

**Evidence.** Timeline of SIGTERM, readiness flip, last accepted connection and process exit; per-request outcome; writer effect counts before and after.

## Traceability

| Requirement | Scenarios |
|---|---|
| AC-003 (FR-003, FR-004) | 3.1 to 3.6 |
| AC-004 (FR-008, FR-012, C-06) | 4.1 to 4.6 |
| AC-005 (FR-007, FR-009) | 5.1 to 5.6 |
| C-07, FM-004 | 3.3, 4.4, 4.5 |
| RD-004 | 4.3, 4.6 |
| RD-010 | 5.3, 5.5 |
| GATE-003 | 3.1 is its identity check; this plan supplies the TEST-003 to 005 part only. The gate also needs TEST-001, TEST-002 and TEST-010 green, S_ctl and replica counts proposed from MEAS-004/005, and a separate owner deployment approval. Tier (c) runs only with that approval. |

## Gaps that block execution

- **No gateway mode (ITEM-014, OD-012).** Nothing exists to start as a gateway process; minting, routing and retry handling are unbuilt.
- **No gateway readiness (ITEM-015, OD-005).** Readiness on snapshot age needs S_ctl and the poll interval.
- **No readiness-propagation window.** In `src/bin/vlite-server.rs` the signal task calls `begin_drain()` and sends the stop at the same moment, so the listener stops accepting as readiness fails. Step 5.1 needs ITEM-015 to keep accepting for a delay after readiness fails, inside the grace period; the delay is an input and is not set here.
- **No rate-limit share (ITEM-016, OD-006).** Load steps must record whether edge limits fired; per-replica share follows section 3.4.7 and needs the replica maximum.
- **Ambiguous-write error shape.** ITEM-014 defines the exact status and code for step 5.3; this plan only requires that it is neither success nor a definite failure, and not retried.

## Related, out of scope here

- TEST-014 (clock skew and pause) and TEST-015 (replica churn during ACA revision overlap) have their own rows in the PRD.
- MEAS-004 and MEAS-005 are measurements. A TEST run here does not replace them, and a measurement run must not be tuned to pass a TEST.
