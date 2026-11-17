# Gateway scale, revocation and drain test plan

Test plan for the shared gateway in the staged Azure Container Apps (ACA) HA plan ([PRD](feature-aca-ha.prd.md), EPIC-004 ITEM-017). It covers TEST-003 (statelessness and scale), TEST-004 (revocation and suspension staleness) and TEST-005 (drain). It says what is already tested, what each scenario must show, and what must exist before the scenarios can run. Design background is in [gateway.md](gateway.md).

> **Status (2026-11-17): tier (b) partly executed locally; tier (c) NOT RUN.** The gateway MVP is wired as an opt-in mode (`vlite-server gateway`, `--gateway-trust-file`; see [API.md](../../API.md#gateway-mode-and-gateway-trusted-backends-experimental-opt-in-off-by-default)). `tests/gateway_azurite.rs` runs the tier (b) steps listed under [Tier (b) execution](#tier-b-execution) against a loopback Azurite with local test values. That is not evidence for GATE-003 or any MEAS item: no ACA run, no load, no measured S_ctl or replica counts. Production is unchanged.

## Inputs, not defaults

The scenarios take these values as inputs. This plan sets none of them; a run records the values it used. In the MVP each is a required configuration field with no default.

| Input | Source | State today |
|---|---|---|
| S_ctl and the control-state poll interval | OD-005, from MEAS-005 | Undecided. Gateway and trust files: `s_ctl_ms`, `poll_interval_ms` |
| Gateway minimum and maximum replicas | OD-006, from MEAS-004 and a quota check | Minimum 2 warm is a proposal and an ACA setting only; maximum unmeasured, gateway file `max_replicas`; scale-to-zero is not allowed (FR-004) |
| Termination grace period | ACA revision setting | ACA default 30 s; the maximum is unverified. Gateway file `drain_timeout_ms` bounds in-flight completion |
| Readiness-propagation delay after SIGTERM | ITEM-015 | Gateway and trust files: `readiness_propagation_ms` |
| Envelope lifetime and clock-skew bound | Envelope configuration (ITEM-010) | Gateway file `envelope_lifetime_ms`; trust file `max_lifetime_ms`, `skew_ms` |
| Load shape (request mix, concurrency, duration) | Chosen per run from MEAS-004 | Recorded, never tuned to make a run pass |

The tier (b) run uses S_ctl 2000 ms, poll 200 ms, `max_replicas` 2, envelope lifetime and `max_lifetime_ms` 5000 ms, skew 1000 ms, readiness propagation 1500 ms and drain timeout 5000 ms. They are local test inputs, not proposals.

## Test tiers

| Tier | What runs | Where | Default in CI |
|---|---|---|---|
| (a) Library and in-process router | Existing unit and router tests (below) | `cargo test` | Runs |
| (b) Local multi-process | Two gateway processes and two gateway-trusted backends (one `shared-pool` with two tenants, one `dedicated`), control state in Azurite; no readers | Developer machine or CI runner | Ignored, like `tests/cloud_backup_azurite.rs`; run `tests/gateway_azurite.rs` with `--ignored` and `VLITE_AZURITE_BLOB_ENDPOINT` |
| (c) ACA rehearsal | Gateway app with N replicas in a non-production ACA environment | Owner-approved non-production subscription | Never; needs owner approval for each run |

Tier (b) cannot share the in-memory `ObjectStore` fake across processes, so it uses Azurite (the `VLITE_AZURITE_BLOB_ENDPOINT` pattern). "Control state unreachable" at tier (b) means deleting the coordination container, so every read fails. Fine-grained fault injection (a store that fails before a given operation) stays at tier (a) through `Op` and `Fault::Before`.

Tier (b) processes are spawned like `tests/server_cli.rs` does (`env!("CARGO_BIN_EXE_vlite-server")`) and stopped with its `stop()` helper: `kill -TERM` on Unix and `CTRL_BREAK` through `GenerateConsoleCtrlEvent` on Windows.

## Already covered at tier (a)

These pass today. They prove the building blocks; the running processes are tier (b) below.

| Area | Test | File |
|---|---|---|
| Fail closed without a snapshot and after S_ctl; an unreachable store does not refresh the age; a backwards clock is stale (FR-008, FM-004) | `poller_fails_closed_without_a_snapshot_and_after_s_ctl` | `src/server/control_state.rs` |
| A lower generation is refused (RD-004) | `poller_refuses_a_generation_regression` | `src/server/control_state.rs` |
| Revocation and suspension reach a fresh poller; a backend whose floor is raised refuses an older envelope; a stale replica fails closed (library TEST-004) | `revocation_and_suspension_propagate_and_old_generations_are_refused` | `src/server/control_state.rs` |
| Admin mutations are CAS: exclusive create, lost-race retry, unknown outcome reported, contention exhausted | `create_is_exclusive_and_update_advances_generation_by_cas`, `update_retries_a_lost_race_against_the_new_state`, `update_reports_unknown_outcomes_and_exhausted_contention` | `src/server/control_state.rs` |
| Envelope generation floor rises and never falls; skew and lifetime bounds; forged, altered and expired envelopes refused | `generation_floor_rises_and_never_falls`, `expiry_and_issue_time_honour_the_skew_bound_exactly`, `lifetime_is_bounded_and_positive`, `forged_or_altered_envelopes_are_refused`, `expired_credential_is_refused_even_inside_the_envelope_window` | `src/server/principal_envelope.rs` |
| Trusted backends need a valid envelope for a tenant routed to them and keep the highest generation seen; suspension and stale control state fail closed (TEST-010, RD-004) | `trusted_mode_requires_a_valid_envelope_for_a_tenant_routed_here`, `backend_holds_the_highest_generation_and_accepts_rotation_overlap`, `envelope_principals_are_authorized_like_scoped_credentials`, `suspension_and_a_stale_control_state_fail_closed`, `standalone_default_is_unchanged` | `src/server/gateway_trust_tests.rs` |
| Gateway forwarding rules: caller authority, proxy and hop-by-hop headers dropped; one bearer; upstream URL confusion refused; unsafe paths refused; per-replica bucket capacity and refill, bounded bucket table, share below one refused; configuration flags | `server::gateway_mode::` tests | `src/server/gateway_mode.rs` |
| A streamed response longer than the upstream timeout arrives intact; a stalled stream is cut short after the idle timeout and the client sees the body fail; failures say whether a request was sent (502 `backend_unavailable` or `backend_failed`, 504 `backend_timeout`, 502 `write_outcome_unknown` for writes) | `a_streamed_body_longer_than_the_timeout_arrives_intact`, `a_stalled_stream_is_cut_short_after_the_timeout`, `upstream_failures_say_whether_anything_was_sent` | `src/server/gateway_mode.rs` |
| Gateway, keygen and control-state commands refuse invalid, unknown or contradictory configuration and keep keys out of output | `gateway_commands_validate_configuration_and_keep_keys_out_of_output` | `tests/server_cli.rs` |
| Writer and reader `/ready` answer 503 `draining` after `begin_drain` | `writer_serves_mutations_and_reports_its_mode` | `tests/server_serve_mode.rs` |
| A real process drains both listeners on the stop signal | `grpc_listener_reports_health_and_both_listeners_drain_on_signal` | `tests/server_cli.rs` |
| Operation classification matches routes and authorization (TEST-002) | `server::gateway::` tests | `src/server/gateway.rs` |
| Routing rules: every tenant request targets its writer; a minted envelope verifies only at its target; only unsent writes are retried; older-epoch responses are refused; directory records are validated and survive a re-import | `server::gateway::tests::routing::` tests, `directory_round_trips_and_survives_a_reimport`, `invalid_directory_records_are_refused` | `src/server/gateway.rs`, `src/server/control_state.rs` |

## Tier (b) execution

`tests/gateway_azurite.rs` (`gateway_and_trusted_backends_serve_isolate_and_fail_closed`) starts real `vlite-server` processes against a loopback Azurite: `gateway-keygen`, `control-state publish` and `update`, two gateways, two trusted backends and a standalone server for the expected answers. It asserts outcomes over HTTP. Run:

```powershell
$env:VLITE_AZURITE_BLOB_ENDPOINT = 'http://127.0.0.1:10000/devstoreaccount1'
cargo test --features server --test gateway_azurite -- --ignored
```

Failing-first and passing runs, with the standalone no-regression checks, are recorded in [evidence/epic-004-red.txt](evidence/epic-004-red.txt) and [evidence/epic-004-green.txt](evidence/epic-004-green.txt).

| Step | Executed at tier (b) | What the test shows |
|---|---|---|
| 3.1 | Partly | The same reads through either gateway equal the standalone server's status and body; one request at a time, no load |
| 3.2 | Yes | A write through gateway 1 is read through gateway 2 |
| 3.3 | No | No SIGKILL under load |
| 3.4 | Partly | A gateway stopped and restarted serves from the shared state; every gateway and backend loads a snapshot before `/ready` answers 200 |
| 3.5, 3.6 | No | No scaling or load |
| 4.1 | Partly | Revoke, suspend and resume each advance the generation through `control-state update` (CAS); concurrent admin mutations are tier (a) only |
| 4.2 | Partly | Both gateways refuse the revoked credential (401) and the suspended tenant (`tenant_suspended`) within 3 × S_ctl; a backend with no traffic advances its `minimum_generation` from its own poll. Not a propagation measurement |
| 4.3 | Tier (a) | `backend_holds_the_highest_generation_and_accepts_rotation_overlap` |
| 4.4, 4.5 | Yes, for a store outage | With the coordination container deleted, the running gateway and both backends answer `/ready` 503 within 4 × S_ctl, the gateway denies with 503 `control_state_stale`, and the poll failure is logged as `control_state_poll_failed`. A single paused replica is not separately tested |
| 4.6 | Yes | A control state recreated at a lower generation is refused (`control_state_regressed`); gateway and backend stay not ready |
| 5.1 | Yes | After the stop signal the gateway logs `draining` and `/ready` answers 503 `draining` while the listener is still open; the process exits cleanly |
| 5.2 | No | No request is in flight during the drain |
| 5.3 | Yes | A relay closes the connection after receiving the full write: 502 `write_outcome_unknown`, the relay forwarded exactly one request (no replay), and the write is readable afterwards through the restored route |
| 5.4 | No | The MVP makes zero retries, so a refused connection is 502 `backend_unavailable`; not exercised by a process test |
| 5.5 | Partly | For 5.3 only |
| 5.6 | No | Platform behavior; tier (c) |

The same test also covers isolation (two tenants on a `shared-pool` backend and a `dedicated` tenant), denial at the gateway (missing or admin bearer, admin route, dot segments, forged `X-VLite-Principal` and `Cookie`), the trusted backend refusing direct bearers and forged principals, edge rate limiting (429 `rate_limited` with `Retry-After`, and 503 `rate_limit_share_below_one`), keygen output secrecy, and the `gateway_started` status line saying that replicas add no write capacity (AC-011).

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
| 5.3 | Break the connection to the writer after the request bytes were sent (fault proxy closes without a response) | The client gets `502 write_outcome_unknown`, which states the outcome is unknown; the writer received the write exactly once (FR-007, RD-010) | The gateway retries, or reports success or a definite failure |
| 5.4 | Refuse the connection to the writer before any byte is sent | The MVP gateway makes zero retries and answers 502 `backend_unavailable`. A later retrying gateway may retry only within its bound (the library allows a write retry only when nothing was sent) | A retry after any byte was sent |
| 5.5 | Count effects in the writer (change sequence, as read by webhooks, or record counts) | No duplicate effect for any write in 5.1 to 5.4 | Any write applied twice |
| 5.6 | Exceed the grace period with a request that cannot finish | The platform stops the replica; that request fails at the client; others are unaffected | Other replicas affected |

Tier (b) uses a small relay between gateway and writer for 5.3 (`Relay` in `tests/gateway_azurite.rs`, test code only). 5.4 needs no proxy: point the directory at a closed port.

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

## Remaining gaps

The MVP is wired, so nothing blocks tier (b). What remains:

- **Not run at tier (b):** 3.3, 3.5, 3.6, 5.2, 5.4 and 5.6 (above), a single paused replica for 4.4, and memory-binding propagation in 4.1.
- **Tier (c) NOT RUN.** It needs a non-production ACA environment, Key Vault or managed-identity key delivery, and owner approval for each run. It is the only tier that can feed GATE-003.
- **No measurements.** S_ctl, the poll interval, `max_replicas` and the readiness-propagation delay are inputs. MEAS-004, MEAS-005 and MEAS-007 remain open, and so do OD-005 and OD-006.
- **No fencing (OD-002).** The gateway does not verify a writer epoch on responses. The directory epoch is a static startup value, not fencing.
- **Rate-limit overshoot.** Each replica enforces its share of `rps` independently. During rollout or above `max_replicas`, the total can exceed the tenant's limit; this is unqualified production behavior. Backend vector quotas still apply.
- **Epoch response header.** No response header carries a writer epoch yet; that waits for OD-002.

## Related, out of scope here

- TEST-014 (clock skew and pause) and TEST-015 (replica churn during ACA revision overlap) have their own rows in the PRD.
- MEAS-004 and MEAS-005 are measurements. A TEST run here does not replace them, and a measurement run must not be tuned to pass a TEST.
