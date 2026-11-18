# Task B — HA ITEM-020: reader staleness bound, reader headers, binding generation (code)

Repository: mahpate/Vlite (Rust). You work in a fresh clone at the plan base.
Read `docs/projects/feature-aca-ha/feature-aca-ha.prd.md` rows FR-010 (~309),
FR-011 (310), TEST-006 (389), TEST-011 (394), AC-006 (422), AC-012 (428),
FM-006 (441), FM-009 (444), RD-007 (486), OD-007 (501), sections 3.4.2 (~146),
3.8 (~243), 3.11 (~268) and ITEM-020 (~844) first.

## Goal

A reader-mode server (`--serve-mode reader`) that follows the writer's archive
(ITEM-019, `src/server/reader_apply.rs`) must (1) refuse readiness and reads once
its served data is older than its staleness bound, (2) stamp the response headers
the gateway uses to decide whether to trust a reader answer, and (3) report all of
this in `/v1/admin/serve-status`. Every server also reports a **binding
generation** fingerprint so an operator can copy it into the tenant's
control-state record (`Route.binding_generation`).

## Exclusive write scope (touch nothing else)

- `src/server/serve_mode.rs`
- `src/server.rs`
- `src/server/reserved.rs`
- `src/server/reader_staleness.rs` (new; declare `mod reader_staleness;` in server.rs)
- `docs/API.md`

Do NOT edit `src/server/reader_apply.rs` (being extended in parallel; use its
existing public-to-`super` API read-only), `src/server/gateway.rs`,
`src/server/gateway_mode.rs`, `src/bin/vlite-server.rs`, `tests/server_cli.rs`,
or anything under `deploy/`. Add no new crate dependencies (`sha2 0.10` and
`serde_json` are already non-optional dependencies).

## Existing code you build on

- `src/server/reader_apply.rs` 18-90: `Progress` with `applied() -> (sequence, confirmed_unix_ms)`,
  `confirm(seq, at)`, `set_bound(ms)`, `bound() -> u64` (0 = none),
  `age_ms(now_unix_ms) -> Option<u64>` (None before the first confirm),
  `failure()`. `Service.reader: Option<Arc<reader_apply::Progress>>` (server.rs 254;
  `None` at 352). `Service::reader_failure()` (reader_apply.rs ~108) is already used
  by `ready()` (server.rs ~2373). Test pattern for a reader with progress:
  reader_apply.rs ~182-212 helpers (`file`, `reader`, `status`) and ~280-300
  (`service.reader = Some(progress.clone())`). Several `Progress` methods carry
  `#[cfg_attr(not(test), allow(dead_code))]`; leave that attribute alone (you may not
  edit that file) — clippy passes either way.
- `src/server.rs`: `metrics_blocked_by_memory()` (~391; positions are withheld when
  it is true, see docs/SECURITY.md ~208-215), `service.lineage: Option<u64>`,
  router layers (~1107-1113), admission middleware with the
  `serve_mode::admit` chain (~1587), `ready()` (~2373).
- `src/server/serve_mode.rs`: `admit` (~111), `status` (~147) — serve-status JSON.
- `src/server/reserved.rs` 166-235: `Bindings { set: BindingSet, reserved: Reserved }`
  with `new`, `with_orphans`, `is_configured()`; `Served::current() -> Arc<Bindings>`
  (~289); `Service.memory_bindings: Arc<reserved::Served>` (server.rs 242).
  `crate::memory::binding::to_json(&BindingSet)` (pub(crate), memory/binding.rs ~490)
  is the canonical JSON form (see its use in cloud_recovery.rs ~793).

## Required behaviour

1. **Binding generation fingerprint** (decided; do not change the scheme):
   - `Bindings::binding_generation() -> u64`, computed once in `new` and carried by
     `with_orphans` (same set ⇒ same value); `Default` must give 0.
   - 0 when `!is_configured()` (no bindings). Otherwise: the first 8 bytes of
     `Sha256::digest(serde_json::to_vec(&to_json(&set)))` read big-endian
     (`u64::from_be_bytes(digest[..8].try_into().unwrap())`, idiom from memory.rs
     ~1572), masked to 53 bits (`& ((1 << 53) - 1)`, JSON-safe), and a result of 0
     mapped to 1. Deterministic across processes for the same binding file content.
   - A `Service` helper `binding_generation()` returns
     `self.memory_bindings.current().binding_generation()`.
2. **Staleness** (new `reader_staleness.rs`): a reader is *stale* when
   `service.reader` is `Some(progress)`, `progress.bound() > 0`, and
   `progress.age_ms(now)` is `None` (never confirmed) or `> bound`. Writers,
   standalone servers and readers without apply progress are never stale.
   - `ready()`: after the `reader_failure` check, a stale reader answers
     503 code `reader_stale` (message without positions).
   - Reads: a stale reader refuses admitted requests with 503 `reader_stale`
     (add it to the `serve_mode::admit` chain or an equivalent point after
     authorization). Exempt `/v1/admin/serve-status` so operators can diagnose.
3. **Reader response headers** — stamp on every response of a reader whose
   `service.reader` is `Some` (and only then; standalone, writer and readers without
   apply progress send none, so HA-off output is byte-identical — AC-010):
   - `X-VLite-Lineage`: `format!("{:016x}", lineage)` (omit if `service.lineage` is None)
   - `X-VLite-Binding-Generation`: decimal `binding_generation()`
   - `X-VLite-Applied-Age-Ms`: decimal age (omit before the first confirm)
   - `X-VLite-Applied-Sequence`: decimal applied sequence, **omitted** when
     `metrics_blocked_by_memory()`.
   No position token. A small response layer is fine. The gateway (parallel task)
   falls back to the writer when lineage or binding generation is missing/mismatched
   or the status is 503.
4. **serve-status** (`serve_mode::status`): add `"binding_generation": <u64>` in all
   modes, and `"reader": null` unless `service.reader` is Some, then
   `{"applied_sequence": <u64|null, null when hidden>, "age_ms": <u64|null>,
   "bound_ms": <u64>, "stale": <bool>, "failed": <bool>}` (no failure text — it may
   name positions). Keep every existing field and `"epoch": null`.
5. gRPC (`grpc.rs`) has no reader gating today: stay REST-only and add one sentence
   to API.md that gRPC staleness gating is not provided.

## Docs (`docs/API.md`)

- Update the serve-status section with `binding_generation` and the `reader` block.
- Document the four reader headers, `reader_stale` (ready + reads), and how an
  operator fills `binding_generation` in a control-state record from the writer's
  serve-status.
- Replace the stale gateway text near lines 1226-1229 ("validated but not acted
  on"): with `read_routing: "readers"` the gateway sends reads to the record's reader
  and falls back to the writer on connect failure/timeout, a 503, or a missing or
  mismatched `X-VLite-Lineage`/`X-VLite-Binding-Generation`; writes always go to the
  writer; `writer_only` (the default) keeps today's behaviour.

## Tests (test-first, in the files you own)

- Fingerprint: 0 with no bindings; stable for equal sets; changes when a binding is
  added or changed; never 0 when configured; ≤ 2^53-1; `with_orphans` keeps it.
- Staleness: fresh reader ready + reads 200; aged past bound → `/ready` 503
  `reader_stale` and a read 503 `reader_stale`; serve-status still 200; bound 0 → never
  stale; never-confirmed with bound > 0 → stale; writer/standalone unaffected.
- Headers: present on a reader with progress (all four); applied-sequence absent when
  memory bindings hide metrics; absent entirely on standalone, writer and a reader
  without progress.
- serve-status: `binding_generation` present in all modes; `reader` block shape.

## Validation you must run before committing

```
cargo fmt --check
cargo clippy -q --features server --lib --tests -- -D warnings
cargo clippy -q --all-features --lib --tests -- -D warnings
cargo clippy -q --features server --bins -- -D warnings
cargo test -q --features server --lib -- server::
cargo test -q --features server --test server_memory
```

All must pass. Commit on your result branch with a message that lists the
behaviour and the new test names.
