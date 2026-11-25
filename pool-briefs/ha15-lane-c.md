# Task: close the plaintext `env[].value` gap in the writer-profile secret check (VLite), test-first

Repository: mahpate/Vlite (Rust). Small, self-contained hardening of `check_body`/`secret()` in
`src/server/tenant_provisioning/aca.rs`. Read the current code first.

## Background
`check_body` refuses a writer profile body that carries secret material with the exact message
`writer profile bodies must not carry secret material`. Today `secret()` is only a key-name heuristic
(password, secret, connectionstring, accountkey, sastoken, sharedkey, privatekey at any depth; keys ending in
`secretref` are exempt). Its own doc comment admits plaintext under a neutral key, e.g. a container
`env: [{"name": "VLITE_AUTH_TOKEN", "value": "<token>"}]`, is NOT detected. The production HA templates emit
no env entries, so an allow-list breaks no shipped profile.

## Exclusive write scope
Only `src/server/tenant_provisioning/aca.rs` and `src/server/tenant_provisioning/tests.rs`.
Any change to any other path fails the task. No new files, no Cargo.toml/Cargo.lock change.

## Required behavior (fail closed)
Wherever a key spelled `env` in ANY letter case appears at any depth of a profile body:
1. Its value must be an array; otherwise refuse.
2. Every entry must be an object whose keys are exactly `name` plus exactly one of `secretRef` or `value`
   (case-sensitive; any other key, a case twin such as `Value`, both sources, or neither source is refused).
   `name` must be a non-empty string; `secretRef`/`value` must be strings.
3. `{name, secretRef}` is accepted for any name (it is a reference, not material).
4. `{name, value}` is accepted ONLY when `name` exactly equals one entry of a small `const` allow-list of
   non-secret scalar settings the server reads. Use exactly: `VLITE_SERVE_MODE`, `VLITE_TRUSTED_PROXY`,
   `VLITE_ADMIN_DIR`, `VLITE_ADMIN_USERNAME`, `VLITE_SESSION_IDLE_MINUTES`, `VLITE_SESSION_MAX_HOURS`,
   `VLITE_AUTO_MAINTENANCE_INTERVAL_SECS`, `VLITE_BACKUP_INTERVAL_SECS`, `VLITE_BACKUP_RETENTION_ENABLED`,
   `VLITE_BACKUP_RETAIN_LAST`, `VLITE_BACKUP_RETAIN_DAYS`, `VLITE_BACKUP_DRILL_EVERY`,
   `VLITE_BACKUP_DRILL_MAX_BYTES`, `VLITE_SLOW_QUERY_MS`, `VLITE_METRICS_WITH_MEMORY`, `RUST_LOG`.
   Every other name with a `value` is refused (including VLITE_AUTH_TOKEN, VLITE_READ_ONLY_TOKEN,
   VLITE_ENCRYPTION_KEY, VLITE_ENCRYPTION_KEY_HEX, VLITE_EMBEDDING_KEY, VLITE_EMBEDDING_URL, any lowercase or
   unknown name).
5. All refusals above use the existing message `writer profile bodies must not carry secret material`.
Keep the existing key-name heuristic exactly as is (it still applies everywhere, including inside env entries),
and update the `secret()` doc comment to describe the new rule and the remaining residual (plaintext in other
neutral keys such as `args`/`command` is still not detected). Do not change the owner-tag, case-twin (`at`) or
`activeRevisionsMode Single` checks.

## Tests (write first, in tests.rs next to `bodies_are_objects_without_secret_material_or_the_owner_tag`)
Add `env_values_are_accepted_only_for_listed_non_secret_names` and
`env_entries_carry_only_a_name_and_one_source`, using the existing helpers (`refused`, `resolves`, `writer`,
`container`). Cover: allowed value accepted; secret-looking and unknown names with value refused; `Env`/`ENV`
spelling scanned; non-array env; non-object entry; missing name; empty name; both sources; neither; extra key;
`Value` case twin; non-string value/secretRef; an env array nested elsewhere (e.g. an init container).
The existing test must pass unchanged, including its accepted `env: [{"name": "a", "secretRef": "registry"}]`.

## Must keep passing
Every test in `server::tenant_provisioning` (lib) and `tests/server_cli.rs` tenant-deployment tests.

## Forbidden
No public API, route, OpenAPI, CLI, dashboard, doc or `site/api.html` change. No change to gates, ARM adapter,
worker or digests. Never weaken an existing assertion.

## Validation (run before finishing; all must pass)
- `cargo test --locked --features server --lib server::tenant_provisioning`
- `cargo clippy --locked --features server --lib --tests -- -D warnings`
- `cargo fmt --check`
In your final commit message list the new test names and the validation results.
