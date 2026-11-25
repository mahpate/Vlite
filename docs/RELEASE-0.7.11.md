# VLite 0.7.11 upgrade and rollback

VLite 0.7.11 adds an **opt-in, unqualified automatic-failover supervisor** with
an off-by-default switch in the dashboard, a dashboard view of the server's
HA role and replication status, and fixes to the experimental
high-availability building blocks introduced in [0.7.9](RELEASE-0.7.9.md) and
first published in [0.7.10](RELEASE-0.7.10.md). Check
[published releases](https://github.com/mahpate/Vlite/releases) for
downloadable artifacts; a Git tag alone does not publish them.

The database, admin-state, `memory-bindings-v1` and recovery-bundle formats are
unchanged from 0.7.10. No data migration is required. With none of the HA
commands, flags or files, a server runs exactly as 0.7.10 does. No benchmark
campaign was run for this patch; earlier measurements do not measure 0.7.11.

This is not production HA. Automatic failover is **unqualified**: the
failover drills and loss-report review of GATE-005 have not passed, and when
it is on, recent commits can be lost without a person reviewing the loss
report. There is still one writer per database and no qualified RPO, RTO or
availability SLO. Read
[Opt-in HA operations on Azure Container Apps](HA-ACA.md) before trying any of
it, and only in an isolated environment.

## What is new

### Automatic failover supervisor (unqualified, off by default)

`vlite-server failover auto --config <json>` runs an operator-started
supervisor that probes the writer's `/ready`. After a configured number of
consecutive failures for the same authority epoch, it fences the old writer
with an operator-supplied command, takes a loss report and transfers writer
authority exactly as `failover execute` does, then optionally starts a
replacement. It makes one attempt and exits. Nothing changes unless the switch
is on **and** a supervisor is running.

- **The switch.** One policy object beside the authority record
  (`<prefix>auto-failover.json`) turns every supervisor of that target on or
  off. A missing or unreadable policy counts as off. Change it with
  `vlite-server failover policy show|enable|disable`, the dashboard
  (**Administration → Backup & recovery → Automatic failover**, super
  administrators only) or `GET`/`PUT /v1/admin/auto-failover` on the writer.
  Turning it on needs an explicit unqualified acknowledgement and the
  revision last read, so concurrent changes are refused. Turning it off
  always writes a fresh disabled revision; the command-line
  `failover policy disable` is the primary kill switch because it needs
  neither the dashboard nor a running writer. Changes are audited as
  `auto_failover_policy_changed`. Open development administration refuses
  the `PUT` with 401 `admin_auth_required`.
- **Probing.** Only HTTP 200 from `/ready` with `status: "ready"`,
  `mode: "writer"` and the expected epoch counts as healthy. Unauthorized,
  redirected or throttled probes and a mismatched identity hold the
  supervisor instead of counting as failures. It acts only on a writer it has
  itself seen healthy in the same epoch.
- **Fencing and cutoff.** `fence_command` must stop every old writer replica
  and print exactly `{"old_replicas":0}`. The switch is read again before
  every probe, before and after fencing, and immediately before transfer;
  disabling before that final check aborts promotion, but a transfer already
  past it may complete, and a running fence command cannot be recalled.
- **Outcomes.** A promotion prints JSON with `state` `promoted` and the full
  loss report, and exits 0. Otherwise it exits nonzero with
  `auto_failover_standby_failed` or `auto_failover_aborted: <reason>`.

The private config, its limits, every hold and abort reason, and the
back-out procedure are in
[Automatic failover](HA-ACA.md#automatic-failover-unqualified-off-by-default)
and the [API reference](API.md#automatic-failover-unqualified-opt-in-off-by-default).
Run exactly one supervisor per authority target, and turn the switch off
before any planned maintenance, restart or manual failover of the writer.

### Dashboard: server role and replication

The live monitor gains a **Server role and replication** panel built on
`GET /v1/admin/serve-status`. It shows the configured role, write behavior,
draining, data position, archive health and
[reader freshness](API.md#serve-status-and-reader-freshness). With agent
memory on, sign-in and this panel still work while metrics stay off. The
dashboard shows status and the automatic-failover switch; it does not perform
failover.

### Gateway, archive and harness fixes

- **Gateway drain.** Requests still running when `drain_timeout_ms` passes
  are cancelled without retry: 503 `draining` when nothing or only a read was
  sent, 502 `write_outcome_unknown` for a sent write, and a stream is cut
  short. Releasing slots and stopping the control-state poller get at most
  1 s each (`poller_stop_timeout`), so a gateway exits within about
  `readiness_propagation_ms` + `drain_timeout_ms` + 2 s.
- **Gateway slots.** A request body that stalls for longer than
  `upstream_timeout_ms` gets 408 `request_body_timeout` with nothing sent
  upstream, and a client that stops reading a response loses it
  (`gateway_client_stalled`); neither holds an in-flight slot indefinitely.
- **Archive status.** An idle archive run reports the index's latest
  checkpoint as `tip`, so a checkpoint whose commit an earlier run could not
  confirm is shown without being counted again.
- **Archive size check.** The legacy single-segment path now refuses an
  encoded segment over 64 MiB with `pitr_delta_too_large` before anything is
  uploaded.
- **OpenAPI.** The backup-policy `PUT` schema lists `archive`.
- **Measurement harnesses.** The offline MEAS-004/005/006 harnesses for the
  gateway, control-state and writer-fencing measurements are hardened. They
  run nothing against production and do not change any measurement result.

## Upgrade

Verify release checksums and back up the database, private bindings file and
admin state. Stop 0.7.10 before starting 0.7.11; keep one writer per database
file. Preserve existing authentication, durability, proxy, storage and memory
settings. No HA feature, including automatic failover, is enabled by the
upgrade. Keep a separate dashboard image at the same version as the API.

When upgrading from 0.7.8 or earlier, also read the
[0.7.9 upgrade notes](RELEASE-0.7.9.md#upgrade).

## Rollback

Before rolling back, turn the automatic-failover switch off with
`vlite-server failover policy disable` and stop every supervisor. Then stop
0.7.11 before starting 0.7.10. Formats are compatible; no reverse migration is
required. 0.7.10 ignores `auto-failover.json` and has neither the supervisor
nor the dashboard panels. For earlier rollback targets, follow the
[0.7.10 rollback guidance](RELEASE-0.7.10.md#rollback).

## Compatibility and known limits

The [0.7.9 limits](RELEASE-0.7.9.md#compatibility-and-known-limits) apply,
except that an unqualified automatic failover now exists and the dashboard
shows HA status. In short:

- Automatic failover is unqualified and not for production. Nothing prevents
  a second supervisor for the same target, policy and transfer are not
  atomic, and the fence command's report is the only proof that the old
  writer stopped.
- One writer per database. Gateway and reader replicas add read capacity and
  isolation, not write capacity.
- No ACA-qualified deployment. Gateway load and control-state measurements
  have not run on ACA, and the writer-fencing measurement has not produced a
  usable ACA result; see the
  [HA measurement summary](HA-ACA.md#what-has-been-measured).
- Azure Files remains experimental, not power-loss or cross-host writer-lock
  qualified.
