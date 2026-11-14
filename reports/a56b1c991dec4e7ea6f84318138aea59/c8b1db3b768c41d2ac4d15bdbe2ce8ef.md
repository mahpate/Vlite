# Azurite backup/restore requalification

**Result: PASS in the patched temporary copy; FAIL in the unmodified checkout.** The original ignored test reaches offline restore and fails on its obsolete CLI options. Updating only its temporary copy to `--output <new-dir>` makes every original assertion pass. A second, extended execution also passes the requested state-document, secret-absence, no-overwrite, and repeated-restore/pin checks. This is local-emulator qualification, not production Azure qualification.

## Scope and provenance

- Examined HEAD: `0797199c0dd9288a8d6509392efd5a4a20e6aaa5`; requested implementation ancestor: `0c36af13769b95eb5502c4a34cf2ec35be0f9fb5`.
- Executed `git diff 0c36af13 HEAD -- tests\cloud_backup_azurite.rs src\server\cloud_recovery.rs src\server\backup_policy.rs src\bin\vlite-server.rs`: empty. The tested implementation and test therefore match that ancestor on these relevant paths.
- Contract-changing commit: `86a57b57d150301a262b065013bc89b2d1c3f940`, “Publish cloud restores atomically from private staging.”
- Host/toolchain: Windows; Rust/Cargo 1.99.1; Node v26.13.0; npm 11.20.0; installed Azurite 3.37.0.
- Source copy: `git archive HEAD` to ZIP, expanded into `C:\Users\mahpate\AppData\Local\Temp\vlite-azurite-c8b1db3b-checkout`.
- Emulator/install/output directory: `C:\Users\mahpate\AppData\Local\Temp\vlite-azurite-c8b1db3b-emulator`.
- `CARGO_TARGET_DIR` was the emulator directory's `cargo-target` child, outside both source checkouts. `CARGO_HOME`, `TMP`, and `TEMP` were also redirected into that directory. The test's own `tempdir_in(".")` fixtures were inside the disposable source copy, never the original checkout.
- Only this report was created in the original checkout. No source edits, branch switches, commits, pushes, deployment, or real Azure operations were performed.

The backup-only requirements consulted were [PRD section 3.3, especially isolated recovery and rekey-only access](https://github.com/mahpate/Vlite/blob/0c36af13769b95eb5502c4a34cf2ec35be0f9fb5/docs/projects/feature-backup-ha/feature-backup-ha.prd.md#33-recovery-set-publication-and-restore), corresponding to `FR-001` through `FR-003`. This focused test does not certify every requirement in those sections.

## Executed checks and timing

Wall times below include Cargo startup/compilation where applicable; test-harness time excludes compilation. Individual extended assertions were not separately timed.

| Step | Result | Measured time | Evidence |
|---|---|---:|---|
| Install Azurite in an isolated external directory | PASS | 118.30 s | `npm install azurite --no-audit --no-fund`, exit 0; 371 packages installed |
| Start in-memory Blob emulator and verify responsiveness | PASS | Not separately timed | Background `azurite-blob` entry point via Node, flags below; unauthenticated localhost probe returned HTTP 400, demonstrating service response |
| Unmodified ignored integration test | FAIL, expected failure reproduced | 169.83 s wall; 18.20 s harness | Cargo exit 101; 0 passed, 1 failed; exact error below |
| Contract-only temporary patch, all original assertions retained | PASS | 37.50 s wall; 25.61 s harness | Cargo exit 0; 1 passed, 0 failed |
| Extended temporary test: state, GET responses, two new-directory restores, pin listings, existing-output preservation, original recovery assertions | PASS | 37.93 s wall; 35.98 s harness | Cargo exit 0; 1 passed, 0 failed; observations below |
| Stop emulator and delete both temporary trees | PASS | 23.65 s deletion wall time | Both `Test-Path` results false; post-stop localhost probe HTTP 000 / curl exit 7 |

The npm install emitted a dependency deprecation warning and a warning that one dependency's install script was not approved/run. Installation and the emulator nevertheless worked; no shared npm configuration was changed.

Azurite's installed Blob executable entry point was invoked directly as:

```text
node .\node_modules\azurite\dist\src\blob\main.js --blobHost 127.0.0.1 --skipApiVersionCheck --inMemoryPersistence
```

All three Cargo runs used the temporary source copy, the same running emulator, and:

```text
VLITE_AZURITE_BLOB_ENDPOINT=http://127.0.0.1:10000/devstoreaccount1
cargo test --features server --test cloud_backup_azurite -- --ignored --nocapture
```

The environment assignment above describes the process environment; PowerShell set it with `$env:VLITE_AZURITE_BLOB_ENDPOINT`. These were actual executable/server/emulator runs, not mocked-storage or fixture-only checks.

## Verified closed

### Obsolete restore invocation: exact reproduction and temporary correction

The unmodified execution panicked at `tests\cloud_backup_azurite.rs:401:5`. Its exact command-error JSON was:

```json
{"error":{"code":"invalid_arguments","message":"argument 7 after cloud-restore is not an option of this command; usage: vlite-server cloud-sets --target-file <json> [--shared-key-file <path>] | vlite-server cloud-restore --target-file <json> [--shared-key-file <path>] --set <set-id> --output <new-dir> --admin-username <name> --admin-password-file <path> [--mode rekey|seed] (default rekey: new lineage, imported accounts disabled, no credentials or sessions restored; seed is deferred HA scope and refused). The output directory must not exist; its parent must; it receives database.vlite and admin/ in one step. The target file holds the cloud backup policy's target JSON; the emulator shared key falls back to VLITE_CLOUD_BACKUP_SHARED_KEY_FILE. Prints a JSON report; nothing is selected or started. A database literally named cloud-sets or cloud-restore needs a ./ prefix."}}
```

Original options at lines 391-394 are `--database` and `--admin-dir`; the parser rejects the first of them before attempting cloud restore. The temporary correction supplies one nonexistent output directory and derives `database.vlite` and `admin` beneath it. No production implementation was changed.

The first rerun used only that contract correction, without any supplemental assertions. It passed. Thus the original recovery guarantees were not weakened to obtain a pass:

| Original guarantee | Executed result |
|---|---|
| Offline report declares `rekey_only` and `pin_released` true | PASS |
| Source lineage matches; restored lineage differs | PASS |
| Restorer password absent from restore report | PASS |
| Restored admin has no `credentials.json`, `webhooks.json`, or `cloud-backup` | PASS |
| Recovered server starts independently with its fresh bearer token | PASS |
| Vector 3 and vector 114's full 400 KiB payload are restored | PASS |
| Restored database exceeds 4 MiB, exercising staged-block upload | PASS |
| Fresh administrator signs in | PASS, HTTP 200 |
| Original owner's password cannot sign in | PASS, HTTP 401 |
| Imported owner remains disabled and must change password | PASS |
| Recovered installation has no restored cloud-backup policy | PASS |

### One versioned state document and secret-free observed views

The extended run read the actual source installation's `cloud-backup\state.json`, parsed it, and asserted exactly three top-level fields: `format`, `policy`, `status`; `format == 1`; policy/status are objects and equal their corresponding GET response values. It also checked that no separate `policy.json` or `status.json` existed and that the emulator key was absent from the document.

Observed document values, abridged for readability:

```json
{
  "format": 1,
  "policy": {
    "enabled": false,
    "interval_secs": 3600,
    "keep_sets": 3,
    "keep_days": 30,
    "retention_enabled": false,
    "drill_every": 24,
    "orphan_grace_hours": 24,
    "target": {
      "account": "devstoreaccount1",
      "auth": "emulator_shared_key",
      "container": "vlite-data-c848adf5951f",
      "coordination_container": "vlite-coord-c848adf5951f",
      "endpoint": "http://127.0.0.1:10000/devstoreaccount1",
      "prefix": "e2e/source/"
    }
  },
  "status": {
    "runs": 1,
    "published": 1,
    "failures": 0,
    "drills": 1,
    "drill_failures": 0,
    "interrupted": 0,
    "job": null,
    "pending": null,
    "last_error": null,
    "last_set": "20261114T022632Z-e79c3c68bf16dfa1"
  }
}
```

Other persisted status fields contained attempt/success times, drill details, collection details, and scheduling counters. The observed drill recorded 6,145,028 verified database bytes, format 5, 23 live records, 11 commits, and no incomplete tail.

Both explicitly checked `GET /v1/admin/cloud-backup` responses (configured source and recovered installation) returned HTTP 200 and excluded the actual key used by the process:

```text
EVIDENCE source_cloud_backup_get status=200 shared_key_absent=true
EVIDENCE recovered_cloud_backup_get status=200 shared_key_absent=true
```

**Static corroboration, not an additional executed test:** `backup_policy.rs:194-202,390-398` defines/encodes one `Document` containing format, policy, and status; `Status::job` at lines 150-151 is optional and nested under status, not a fourth top-level document field. `backup_policy.rs:668-680` constructs the GET view from policy/status and metadata. `cloud_backup.rs:159-181` defines the persisted/serialized target without a key field. An active non-null job and process-crash persistence were not separately exercised by this task.

### Two restores release their pins; existing output remains untouched

Two consecutive offline restores used the same published set and two previously nonexistent output directories. Both succeeded, reported `rekey_only=true` and `pin_released=true`, preserved the source lineage in their reports, and produced distinct new lineages:

| Operation | Source lineage | New lineage | Pins listed immediately afterward |
|---|---|---|---|
| Restore one | `3a951d6c499b8ff9` | `c3291eee0b7a142f` | `[]` |
| Restore two | `3a951d6c499b8ff9` | `521c4f885c9138dd` | `[]` |
| Rejected attempt to reuse restore one's output | Unchanged | Unchanged | `[]` |

The pin lists came from actual `cloud-sets` calls and selection of the matching set's `record.pins`, not solely from trusting `pin_released`:

```text
EVIDENCE pins phase=after_restore_one set=20261114T022632Z-e79c3c68bf16dfa1 pins=[]
EVIDENCE pins phase=after_restore_two set=20261114T022632Z-e79c3c68bf16dfa1 pins=[]
EVIDENCE pins phase=after_existing_output_refusal set=20261114T022632Z-e79c3c68bf16dfa1 pins=[]
```

Successful CLI reports do not expose the transient operation-pin ID; the requested post-operation lists are empty. No claim is made to have observed the pins while the restores were in flight.

Reusing restore one's output returned failure with this exact JSON:

```json
{"error":{"code":"restore_destination_exists","message":"C:\\Users\\mahpate\\AppData\\Local\\Temp\\vlite-azurite-c8b1db3b-checkout\\.tmpj3yAsl\\recovered already exists; a restore only creates a new --output directory (if an interrupted restore left it empty, remove it and retry)"}}
```

A recursive snapshot before and after the refused operation compared every relative entry, its file/directory kind, and all file bytes; all five entries were unchanged. The recovered server was started only afterward and passed all original data/authentication assertions. This checks a populated, previously restored output directory. Metadata timestamps, ACLs, symlinks, racing creation, and other existing-path types were not covered by this runtime check.

## Still open

**The checked-in ignored test is still stale.** The runtime issue demonstrated here is its old invocation, not a failure of the current `--output` implementation under this scenario. Because this assignment permits only a report, the successful temporary correction was not applied to repository source.

Recommendation: make the minimal CLI-contract update shown in the diff below and retain every original assertion. Consider adopting the additional pin-list/no-overwrite/state checks as regression coverage, separately from the minimal fix. Running the ignored test in a dedicated local-emulator CI job would catch future CLI/test contract drift.

The absence of a shared key is verified for the observed successful GET responses and corroborated by their serialization definitions; the word “never” is not an exhaustive guarantee for every possible future state, error, or code path.

## New

**No additional runtime defect was found in the requested checks.** New evidence is the successful contract-only rerun, independent empty pin lists after two restores, preservation of a populated output tree on refusal, and the actual version-1 state-document shape. These observations do not constitute a complete backup feature review.

### Checks not run / limits

- No required local-emulator step was blocked or omitted.
- Real Azure managed identity/RBAC, Azure object-version semantics, cloud retention enforcement, deployment/storage durability, and performance/RPO/RTO qualification were not run; they are outside the permitted environment and cannot be inferred from Azurite.
- No complete test suite, clippy run, Linux runtime run, fault injection, or concurrent restore stress test was performed. The targeted Cargo runs compiled the server and exercised the requested ignored integration target.
- The persisted state was inspected at quiescence. It shows `status.job: null`; the optional active-job schema is statically confirmed, not dynamically lifecycle-qualified here.
- Recovered-server authentication/read checks exercise vector data and account isolation, not all graph/KV/CAS workload shapes described in the PRD.

### Evidence references

All implementation line references below are to the unmodified requested ancestor, also identical on the four explicitly compared paths at tested HEAD:

| Reference | Relevant evidence |
|---|---|
| [Integration test](https://github.com/mahpate/Vlite/blob/0c36af13769b95eb5502c4a34cf2ec35be0f9fb5/tests/cloud_backup_azurite.rs#L379-L457) | Obsolete invocation; original rekey, lineage, exclusion, server, data, and account assertions |
| [Cloud restore preflight](https://github.com/mahpate/Vlite/blob/0c36af13769b95eb5502c4a34cf2ec35be0f9fb5/src/server/cloud_recovery.rs#L250-L321) | New-output-only contract; `restore_destination_exists` before connection/restoration |
| [Restore/pin lifecycle](https://github.com/mahpate/Vlite/blob/0c36af13769b95eb5502c4a34cf2ec35be0f9fb5/src/server/cloud_recovery.rs#L440-L461) | Operation pin acquired, restore executed, pin release reflected in success report |
| [Staged publication and report](https://github.com/mahpate/Vlite/blob/0c36af13769b95eb5502c4a34cf2ec35be0f9fb5/src/server/cloud_recovery.rs#L485-L596) | New database/admin output, lineage-changing restore, import, publication, returned layout |
| [State document](https://github.com/mahpate/Vlite/blob/0c36af13769b95eb5502c4a34cf2ec35be0f9fb5/src/server/backup_policy.rs#L131-L202) | Status and optional job; versioned document schema |
| [Serialized target](https://github.com/mahpate/Vlite/blob/0c36af13769b95eb5502c4a34cf2ec35be0f9fb5/src/server/cloud_backup.rs#L150-L181) | Auth mode/reference fields, no persisted key field |

Execution output excerpts and the full temporary test diff are preserved in this report because all disposable logs, fixtures, caches, binaries, emulator data, and source copy were deleted after execution.

## Exact temporary patch

This is the final unified diff against the original test, including the supplemental evidence checks. Only the diff's file headers are normalized to the repository-relative test name; hunks are unchanged from `git diff --no-index`. No assertions were removed. The contract-only successful intermediate run comprised just the comment/layout/CLI-option changes, without any supplemental evidence code.

```diff
diff --git a/tests/cloud_backup_azurite.rs b/tests/cloud_backup_azurite.rs
index c6b833d..b1ee98c 100644
--- a/tests/cloud_backup_azurite.rs
+++ b/tests/cloud_backup_azurite.rs
@@ -217,6 +217,43 @@ fn cli(args: &[&std::ffi::OsStr]) -> (bool, Value, String) {
     (output.status.success(), value, stderr)
 }
 
+fn tree_snapshot(root: &Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
+    fn visit(root: &Path, path: &Path, entries: &mut Vec<(std::path::PathBuf, Option<Vec<u8>>)>) {
+        let relative = path.strip_prefix(root).unwrap().to_path_buf();
+        if path.is_dir() {
+            entries.push((relative, None));
+            for entry in std::fs::read_dir(path).unwrap() {
+                visit(root, &entry.unwrap().path(), entries);
+            }
+        } else {
+            entries.push((relative, Some(std::fs::read(path).unwrap())));
+        }
+    }
+    let mut entries = Vec::new();
+    visit(root, root, &mut entries);
+    entries.sort_by(|left, right| left.0.cmp(&right.0));
+    entries
+}
+
+fn assert_no_pins(target_file: &Path, key_file: &Path, set_id: &str, phase: &str) {
+    let (ok, listing, stderr) = cli(&[
+        "cloud-sets".as_ref(),
+        "--target-file".as_ref(),
+        target_file.as_os_str(),
+        "--shared-key-file".as_ref(),
+        key_file.as_os_str(),
+    ]);
+    assert!(ok, "{stderr}");
+    let set = listing["sets"]
+        .as_array()
+        .unwrap()
+        .iter()
+        .find(|set| set["set_id"] == set_id)
+        .unwrap();
+    assert_eq!(set["record"]["pins"], json!([]), "{listing}");
+    println!("EVIDENCE pins phase={phase} set={set_id} pins={}", set["record"]["pins"]);
+}
+
 #[test]
 #[ignore = "needs a local Azurite Blob endpoint in VLITE_AZURITE_BLOB_ENDPOINT"]
 fn azurite_backup_restore_and_isolated_rekey_recovery() {
@@ -356,16 +393,31 @@ fn azurite_backup_restore_and_isolated_rekey_recovery() {
     let (status, view) = source.admin("GET", "/v1/admin/cloud-backup", Value::Null);
     assert_eq!(status, 200, "{view}");
     assert!(!view.to_string().contains(key_text.trim()), "{view}");
+    println!("EVIDENCE source_cloud_backup_get status={status} shared_key_absent=true");
     assert!(
         source_admin
             .join("cloud-backup")
             .join("state.json")
             .is_file()
     );
+    let state_text = std::fs::read_to_string(source_admin.join("cloud-backup/state.json")).unwrap();
+    let state: Value = serde_json::from_str(&state_text).unwrap();
+    assert_eq!(state["format"], 1);
+    assert_eq!(state.as_object().unwrap().len(), 3);
+    assert!(state["policy"].is_object());
+    assert!(state["status"].is_object());
+    assert_eq!(state["policy"], view["policy"]);
+    assert_eq!(state["status"], view["status"]);
+    assert!(state["status"].as_object().unwrap().contains_key("job"));
+    assert!(state["status"]["job"].is_null());
+    assert!(!state_text.contains(key_text.trim()));
+    assert!(!source_admin.join("cloud-backup/policy.json").exists());
+    assert!(!source_admin.join("cloud-backup/status.json").exists());
+    println!("EVIDENCE state_document={state}");
     let source_lineage = restored["source_lineage"].clone();
     drop(source);
 
-    // Isolated, offline rekey-only restore from the cloud into a new file.
+    // Isolated, offline rekey-only restore from the cloud into a new directory.
     let target_file = root.join("target.json");
     std::fs::write(&target_file, target.to_string()).unwrap();
     let (ok, listing, stderr) = cli(&[
@@ -379,8 +431,9 @@ fn azurite_backup_restore_and_isolated_rekey_recovery() {
     assert_eq!(listing["sets"][0]["set_id"], set_id, "{listing}");
     let restorer_password = root.join("restorer.password");
     std::fs::write(&restorer_password, RESTORER_PASSWORD).unwrap();
-    let copy = root.join("recovered.vlite");
-    let copy_admin = root.join("recovered-admin");
+    let output = root.join("recovered");
+    let copy = output.join("database.vlite");
+    let copy_admin = output.join("admin");
     let (ok, report, stderr) = cli(&[
         "cloud-restore".as_ref(),
         "--target-file".as_ref(),
@@ -389,10 +442,8 @@ fn azurite_backup_restore_and_isolated_rekey_recovery() {
         key_file.as_os_str(),
         "--set".as_ref(),
         set_id.as_ref(),
-        "--database".as_ref(),
-        copy.as_os_str(),
-        "--admin-dir".as_ref(),
-        copy_admin.as_os_str(),
+        "--output".as_ref(),
+        output.as_os_str(),
         "--admin-username".as_ref(),
         "restorer".as_ref(),
         "--admin-password-file".as_ref(),
@@ -407,6 +458,64 @@ fn azurite_backup_restore_and_isolated_rekey_recovery() {
     assert!(!copy_admin.join("credentials.json").exists());
     assert!(!copy_admin.join("webhooks.json").exists());
     assert!(!copy_admin.join("cloud-backup").exists());
+    println!("EVIDENCE restore_one rekey_only={} pin_released={} source_lineage={} lineage={}",
+        report["rekey_only"], report["pin_released"], report["source_lineage"], report["lineage"]);
+    assert_no_pins(&target_file, &key_file, &set_id, "after_restore_one");
+
+    let output_two = root.join("recovered-two");
+    let (ok, report_two, stderr) = cli(&[
+        "cloud-restore".as_ref(),
+        "--target-file".as_ref(),
+        target_file.as_os_str(),
+        "--shared-key-file".as_ref(),
+        key_file.as_os_str(),
+        "--set".as_ref(),
+        set_id.as_ref(),
+        "--output".as_ref(),
+        output_two.as_os_str(),
+        "--admin-username".as_ref(),
+        "restorer".as_ref(),
+        "--admin-password-file".as_ref(),
+        restorer_password.as_os_str(),
+    ]);
+    assert!(ok, "{stderr}");
+    assert_eq!(report_two["rekey_only"], true, "{report_two}");
+    assert_eq!(report_two["pin_released"], true, "{report_two}");
+    assert_eq!(report_two["source_lineage"], source_lineage, "{report_two}");
+    assert_ne!(report_two["lineage"], source_lineage, "{report_two}");
+    assert_ne!(report_two["lineage"], report["lineage"], "{report_two}");
+    assert!(output_two.join("database.vlite").is_file());
+    assert!(output_two.join("admin").is_dir());
+    for excluded in ["credentials.json", "webhooks.json", "cloud-backup"] {
+        assert!(!output_two.join("admin").join(excluded).exists());
+    }
+    println!("EVIDENCE restore_two rekey_only={} pin_released={} source_lineage={} lineage={}",
+        report_two["rekey_only"], report_two["pin_released"], report_two["source_lineage"], report_two["lineage"]);
+    assert_no_pins(&target_file, &key_file, &set_id, "after_restore_two");
+
+    let before = tree_snapshot(&output);
+    let (ok, _, stderr) = cli(&[
+        "cloud-restore".as_ref(),
+        "--target-file".as_ref(),
+        target_file.as_os_str(),
+        "--shared-key-file".as_ref(),
+        key_file.as_os_str(),
+        "--set".as_ref(),
+        set_id.as_ref(),
+        "--output".as_ref(),
+        output.as_os_str(),
+        "--admin-username".as_ref(),
+        "restorer".as_ref(),
+        "--admin-password-file".as_ref(),
+        restorer_password.as_os_str(),
+    ]);
+    assert!(!ok, "an existing output must be refused");
+    let refusal: Value = serde_json::from_str(stderr.trim()).unwrap();
+    assert_eq!(refusal["error"]["code"], "restore_destination_exists", "{refusal}");
+    let after = tree_snapshot(&output);
+    assert_eq!(before, after, "the existing output tree must stay byte-for-byte unchanged");
+    println!("EVIDENCE existing_output_refusal={refusal} unchanged_entries={}", before.len());
+    assert_no_pins(&target_file, &key_file, &set_id, "after_existing_output_refusal");
 
     // The recovered copy starts on its own, with a new token: data is back,
     // only the fresh admin can sign in, the imported owner stays disabled.
@@ -443,6 +552,8 @@ fn azurite_backup_restore_and_isolated_rekey_recovery() {
     assert_eq!(owner["must_change_password"], true, "{owner}");
     let (status, view) = recovered.admin("GET", "/v1/admin/cloud-backup", Value::Null);
     assert_eq!(status, 200, "{view}");
+    assert!(!view.to_string().contains(key_text.trim()), "{view}");
+    println!("EVIDENCE recovered_cloud_backup_get status={status} shared_key_absent=true");
     assert!(
         view["policy"].is_null(),
         "no cloud policy is restored: {view}"
```
