# VLite Docker operations

## What you run

Self-hosting means operating **the API service and its persistent storage** on
your own machine or server. `vlite-server` contains the storage engine; it owns
the `.vlite` file and serves authenticated REST and optional gRPC requests.
There is no separate database-storage service.

The **dashboard is optional**. Open the UI served by the API at `/dashboard`,
or run the standalone dashboard container, which proxies requests to the API.
It does not have a separate database. Compose keeps database files in `api-data`
and administrative state in `api-admin-data`; you manage access, HTTPS, backups
and upgrades.

For a managed Azure host with a stable HTTPS endpoint, see the
[experimental Container Apps deployment](../deploy/azure-container-apps/README.md).
It reuses ASENDME's resource group/storage account with a separate VLite share,
blocks startup on failed storage primitive checks, and stops old replicas before
updates. Azure Files crash durability and cross-host locking remain unqualified.

See the [deployment diagrams](https://mahpate.github.io/Vlite/#deployment-options)
for embedded and self-hosted layouts.

VLite publishes versioned Linux `amd64` and `arm64` images to GitHub Container
Registry through the release workflow when a matching version tag is released:

| Image | Purpose |
| --- | --- |
| `ghcr.io/mahpate/vlite-api:<version>` | REST API server |
| `ghcr.io/mahpate/vlite-dashboard:<version>` | Standalone browser UI and same-origin API proxy |
| `ghcr.io/mahpate/vlite:<version>` | Smoke, maintenance, and benchmark tools; retains the `vlite-smoke` entrypoint |

Use the same version for the API and dashboard. The multi-platform image
manifests select the host architecture automatically. Releases also publish
digest references in their notes; pinning those digests gives an immutable
deployment.

## Source builds and CI caching

Source builds require BuildKit (Docker Compose v2/buildx). Cargo downloads and
compilation outputs use locked cache mounts, so source changes do not discard
all dependency compilation. Runtime executables are copied out of the mounts
before packaging; deleting a build cache cannot remove binaries from an image.
The default runtime/API build still runs the full Rust suite. Only an explicitly
CI-qualified build should set `VLITE_IMAGE_TESTS=0`; this does not change features
or the shipped binaries. The `test` target always runs the full suite during
image build, independently of that argument, and does not compile release
binaries or repeat tests when its container starts.

CI persists Cargo downloads and Docker release compilation caches explicitly;
buildx layer cache alone does not export their contents. Large debug/test
artifacts stay in per-run mounts rather than being copied into a second image
for export, which can exhaust hosted-runner disk space. Native Rust caches
are separated by job, OS, compiler and
Cargo inputs. PRs can restore native caches but do not save them. CI runs on
main pushes, PRs and manual dispatch, cancelling superseded validation runs.
Standalone branch pushes can use manual dispatch before a PR is opened.
Only README/docs Markdown-only changes skip the native/container matrix;
dashboard and embedded website assets still receive full checks. Client and
documentation checks remain enabled, and manual dispatch always forces full CI.
`CI required` is the stable aggregate check for branch protection; skipped
documentation-only matrix entries are intentional, not substitutes for a
failed or cancelled job. Release and deployment workflows are not cancelled.

## First deployment

**First time on Windows?** Use the
[Windows setup script](https://github.com/mahpate/Vlite/blob/main/scripts/devbox.ps1):

```powershell
Invoke-WebRequest https://raw.githubusercontent.com/mahpate/Vlite/main/scripts/devbox.ps1 -OutFile devbox.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\devbox.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\devbox.ps1 -Token
```

It checks/installs WSL 2, Ubuntu 24.04 and Docker/Compose after confirmation.
Windows 10 build 19041+ or Windows 11, administrator approval, hardware
virtualization and internet access are required. If installation requests a
restart, save your work, restart and rerun the same command. Complete Ubuntu
first-run setup if prompted. The script never restarts Windows automatically.
No Git or Rust is needed. Docker Desktop users can reuse working WSL integration;
the script does not replace an existing container runtime.

Open <http://127.0.0.1:8082> and complete the [first sign-in](#first-sign-in).
The REST API is at <http://127.0.0.1:18085>. Both listen on loopback only.

| Task | Command |
| --- | --- |
| Show container status | `.\devbox.ps1 -Status` |
| Stop and keep data | `.\devbox.ps1 -Down` |
| Stop and delete the database volumes | `.\devbox.ps1 -Reset` (asks for typed confirmation) |
| Choose ports, distribution or release | `.\devbox.ps1 -ApiPort 18090 -DashboardPort 8090 -Distro Ubuntu -Version vX.Y.Z` |
| Set dimensions for a new data volume | `.\devbox.ps1 -Dimensions 384` |
| Approve prerequisites without prompting | `.\devbox.ps1 -Yes` |

Rerunning the script updates the deployment and keeps the token and data. WSL
stops Docker when its last process exits, so the script keeps the distribution
running until you sign out or reboot; rerun it afterwards. `-Dimensions` applies
only when the data volume is first created. Automatic package installation
supports Ubuntu 22.04 and 24.04.

**First time on Ubuntu 22.04/24.04?** Install Git, clone the repository and run
the bootstrap script:

```sh
sudo apt-get update && sudo apt-get install -y git
git clone https://github.com/mahpate/Vlite && cd Vlite
sudo sh scripts/docker-dashboard.sh
```

It offers to install Docker Engine and Compose v2 from Docker's official
Ubuntu repository, starts published API/dashboard images, checks both URLs
and prints how to read the token. Data and the existing token survive reruns.
The default directory is `~/vlite-deploy` for the user running the script
(`/root/vlite-deploy` under sudo); the dashboard uses
<http://127.0.0.1:8081> and API <http://127.0.0.1:18084>.
Use `--deploy-dir /path` to choose a stable location. `--yes` approves package
installation without prompting. `--version`, `--api-port`, `--dashboard-port`
and `--dimensions` select the release, ports and dimensions of a new database.
Other distributions require a working Docker Engine/Compose installation
already. Neither setup installs Rust or builds images.

### First sign-in

The dashboard opens on a sign-in screen. A new deployment has no accounts and
no default password, so the screen asks for an access token:

1. Paste the API token, without a `Bearer` prefix, and select **Sign in with
   token**.
2. Open **Administration → Users & access** and create your super admin
   account.
3. Sign in with your username and password from then on. Keep the token for
   automation and recovery.

Compose sets `VLITE_TRUSTED_PROXY=1` because a local reverse proxy fronts the
API. Keep the published ports on loopback, or put HTTPS in front of them. See
[dashboard sign-in and accounts](SECURITY.md#dashboard-sign-in-and-accounts).

### Manual setup with Docker already installed

Run Docker Engine with Linux containers and Docker Compose v2. These commands
are suitable for Docker running inside WSL; run them from the repository in
the same WSL distribution that owns the Docker engine. The default published
ports are loopback-only: API `127.0.0.1:18084` and UI `127.0.0.1:8081`.

Create the API bearer-token file once. Do not check `.secrets` into source
control:

```sh
mkdir -p .secrets
chmod 700 .secrets
umask 077
python3 -c 'import secrets; print(secrets.token_urlsafe(32))' > .secrets/api-token
chmod 644 .secrets/api-token
```

The directory remains private; the file must be readable by container UID
`10001` through its read-only secret mount. Do not use an existing token file
with this creation command: it replaces the credential.

Select a published release and start both services. Replace the example
version with a release that exists in GHCR:

```sh
export VLITE_API_IMAGE=ghcr.io/mahpate/vlite-api:vX.Y.Z
export VLITE_DASHBOARD_IMAGE=ghcr.io/mahpate/vlite-dashboard:vX.Y.Z
docker compose --profile dashboard up -d --no-build api dashboard
docker compose ps
```

Open <http://localhost:8081> and complete the [first sign-in](#first-sign-in).
The UI sends requests to its own `/api/` origin; Nginx forwards them to the API container
over the private Compose network. The API is not exposed beyond loopback by
default. Set `VLITE_PORT` or `VLITE_DASHBOARD_PORT` to change the host-side
port while retaining the loopback bind.

The `api` service persists its database in the Compose-managed `api-data`
volume and its administrative snapshots/state in the separate
`api-admin-data` volume mounted at `/admin`. `VLITE_ADMIN_DIR=/admin` is
configured in Compose. The image prepares `/admin` as UID/GID `10001`, which
also initializes the named volume with the service user's writable
permissions. The API token is mounted from `.secrets/api-token` as a Docker
secret. No read-only token is required by default. An optional
`VLITE_READ_ONLY_TOKEN` can be passed from the host environment.

### Advanced capabilities and format upgrades

New collection, hybrid and resumable-import features require opt-in format v4.
The default server creation format remains v3. Deploying a newer image does not
upgrade an existing volume. Use the offline copy-out upgrade procedure in
[CAPABILITIES.md](CAPABILITIES.md), verify the new copy, and explicitly select it
only during an operator-controlled maintenance window. Never overwrite the
active file or remove the original recovery copy.

Automatic index maintenance and scheduled backups are disabled by default.
Admin runtime configuration is process-local and must be reapplied after
restart; backup manifests, scoped credentials and durable audit live in the
private admin volume. Retention deletion has a separate opt-in switch. Protect
both volumes and keep an independent backup: the admin volume is not an off-host
disaster-recovery copy. Scoped credentials must be reissued after selecting a
compacted, upgraded or restored file because its lineage changes.

To use a separate read-only token file, create an untracked Compose override
such as `compose.read-only-token.yaml` and keep the token file private:

```yaml
services:
  api:
    environment:
      VLITE_READ_ONLY_TOKEN_FILE: /run/secrets/read_only_token
    volumes:
      - type: bind
        source: ./.secrets/read-only-token
        target: /run/secrets/read_only_token
        read_only: true
        bind:
          create_host_path: false
```

Start with both Compose files, for example:

```sh
docker compose -f compose.yaml -f compose.read-only-token.yaml \
  --profile dashboard up -d --no-build api dashboard
```

Treat credentials as secrets and do not place them in committed Compose files.

For a source checkout instead of published images, omit both image variables
and use `docker compose --profile dashboard up --build -d api dashboard`.

## Stop and restart

Stop without removing containers or data:

```sh
docker compose stop api dashboard
docker compose start api dashboard
```

Or remove the containers and network while retaining named volumes:

```sh
docker compose down
```

Do **not** add `-v` to `docker compose down` unless permanent deletion of the
database and administrative backup directory is intended. The same Compose
project name and both named volumes are required when bringing the deployment
back up.

The dashboard waits for the API health check before starting. Both services
use an `unless-stopped` restart policy. The API health check requests its
public `/health` endpoint without installing extra tools in the runtime image.
The dashboard health check uses the Nginx image's BusyBox `wget`. Its `/tmp`
is a small tmpfs so Nginx can run with a read-only root filesystem. Nginx
re-resolves the API service name periodically, which lets the proxy recover
after Compose replaces the API container with a new address.

## Upgrade

1. Back up the database and any required administrative files before changing
   versions. Use the administrative backup feature when enabled, and copy
   important backup files outside the Docker volume as a separate recovery
   measure.
2. Set both image variables to the **same** new release tag (or digest refs
   from that release's notes).
3. Pull and recreate only the two services:

   ```sh
   docker compose pull api dashboard
   docker compose --profile dashboard up -d --no-build api dashboard
   docker compose ps
   docker compose logs --tail=100 api dashboard
   ```

Compose reuses `api-data` and `api-admin-data`; do not remove or rename either
volume during an upgrade. Check the release notes and data-format
compatibility before upgrading. A newer server may write a format that an
older server cannot read, so retaining an old image alone is not a sufficient
rollback plan.

## Rollback

1. Stop writes to the API and preserve a copy of the current volume/backups
   before changing images.
2. Restore both `VLITE_API_IMAGE` and `VLITE_DASHBOARD_IMAGE` to the prior
   compatible version (or its pinned digests).
3. Recreate the services without removing volumes:

   ```sh
   docker compose pull api dashboard
   docker compose --profile dashboard up -d --no-build api dashboard
   ```

If the newer server changed the on-disk format, do not point the older image
at that modified database. Restore a pre-upgrade backup to a separate,
validated copy first, then select that copy using the documented
administrative restore procedure. The restore workflow creates and validates
a copy; selecting it takes effect on the next startup rather than replacing a
live database. See [the administrative API reference](CONSOLE-ADMIN.md) for
backup and restore details.

## File compaction

The server can compact its database file while it runs. Compaction is
experimental and off by default: nothing runs until an administrator presses
**Compact now** or turns on the daily window in the dashboard
(**Administration → Backup & recovery → File compaction**). Routes, limits
and reason codes are in [API.md](API.md#file-compaction-experimental-off-by-default).

### Where it is qualified

Compaction is enabled only when all of these are true; otherwise the card
and `GET /v1/admin/compaction` give the reason and nothing starts:

- the container runs on Linux and the database file and `VLITE_ADMIN_DIR`
  are on the same local **ext4** filesystem;
- the server uses durable commits, an admin directory, and no gRPC listener;
- the file is a current-format, unencrypted single file of at most 1 GiB,
  with at least four times its size plus 10% (at least 64 MiB) free.

With the default `compose.yaml`, the `api-data` and `api-admin-data` named
volumes are directories under the Docker data root, so they qualify when that
root is ext4 (`docker info --format '{{.DockerRootDir}}'`, then
`df -T <that directory>`). XFS, btrfs, overlay and bind mounts from other
filesystems, NFS, SMB/Azure Files, and Docker Desktop or native Windows and
macOS hosts are **not** qualified and stay disabled.

### Qualification evidence

The qualification harness (`tests/compaction_qualification.py`) runs the
release `vlite-server` in a Linux container on an ext4 Docker volume. It
measures the filesystem, checks the refusal on an unqualified mount, runs a
scheduled and a manual compaction, and kills the server during a run. The
run recorded for this feature (Linux 6.18 kernel, ext4 volume, process kills
with `SIGKILL`) measured:

| Check | Result |
| --- | --- |
| Filesystem probe | exclusive lock, 8 MiB file `fsync` about 20 ms, atomic rename with directory `fsync`, admin directory on the same device |
| Overlay root filesystem | refused with `unqualified_filesystem` (409) |
| Scheduled run after 73.6 MB growth | 160.5 MB to 6.7 MB, next run after the 24 h cooldown |
| Manual run on a 1.02 GB file under concurrent writes | 8.6 seconds in total, to 7.2 MB, peak RSS 114 MiB; 164 writes were refused with `503`, reads were refused for at most 8.1 seconds, and the last acknowledged write of each of 3,836 written records was present afterward |
| Kill while freezing | the active file did not change, the job was recorded as `interrupted`, and the next run completed |
| Kill after the new file was selected | the restarted server opened the compacted file and recorded the job as succeeded |

Process kills do not prove behavior across power loss or kernel crashes.
Run the harness on your own host before you depend on compaction there.

### Running it safely

- Take or confirm a backup first. From the moment a job starts quiescing until
  it ends, reads, writes and admin routes other than compaction answer `503`
  `maintenance_in_progress`. The measured 1 GB run paused for about 8
  seconds; the time grows with the live data and the disk speed.
- Old change history is dropped. Clients that resume a change feed must take
  a new snapshot, and earlier ETags or graph revisions stop matching.
- The original file is kept in the admin directory as a recovery copy and is
  counted under *Retained recovery copies*. Delete old copies only after you
  have checked the new file.
- To go back, stop the server, then restore a backup or a recovery copy using
  the normal restore procedure. Do not swap files in the volume by hand while
  a compaction selection is pending; the server pins its startup file until
  the job is resolved.

## Cloud backups to Azure Blob

Cloud backups are off by default and are configured by a super administrator
with `PUT /v1/admin/cloud-backup` (there is no dashboard card in this release;
body and routes in
[API.md](API.md#cloud-backups-to-azure-blob-opt-in-off-by-default)). Unlike
the local backup schedule, this policy is saved in
`<admin>/cloud-backup/state.json` and is applied again after a restart. That
one file holds the policy and the run status, replaced together by an atomic
rename. If a `PUT` answers 500 `cloud_backup_policy_uncertain`, the write
could not be confirmed: `GET /v1/admin/cloud-backup` shows the policy actually
in effect. If the file cannot be read back either, the view shows
`"policy_uncertain": true`, no scheduled run starts and the other cloud routes
answer 503 until a reread or a successful `PUT` confirms the policy; check the
admin volume. The same directory holds `installation-id` (the owner of the
prefix) and `staging/` (temporary copies, emptied at startup); keep it on the
private admin volume.

### Azure setup

- Give the container app a managed identity (system- or user-assigned; set
  `target.client_id` for a user-assigned one). No key or connection string is
  stored: the server asks the platform identity endpoint for a token for the
  storage account's Blob endpoint only, and does not follow redirects.
- Use two containers. The data container (`container`) receives the
  content-addressed objects and manifests; the uploader needs create and read
  on it, and delete for retention and cleanup. The coordination
  container (`coordination_container`) holds owner, catalog and set records;
  it needs read, create, overwrite-with-ETag and delete. `Storage Blob Data
  Contributor` scoped to those two containers covers both. Without a
  coordination container both roles share one container.
- Keep the admin directory (`VLITE_ADMIN_DIR`) for files the server writes.
  A cloud backup refuses to run while it holds anything else, such as a token
  file or TLS material (409 `recovery_config_unrecognized`); mount those from
  another path.
- Blob versioning and immutability policies are neither required nor
  qualified: sets name objects by SHA-256, not by version ID, and a
  time-based immutability policy would make retention deletes fail.

### Run, check and restore

1. Save a policy with `enabled: true`, then `POST /v1/admin/cloud-backup/run`
   (or wait for the interval). `GET /v1/admin/cloud-backup` shows the result.
   A run is not idempotent: if you did not see its outcome, list the sets
   before you retry. The status shows the run in progress (`job`, saved
   before any cloud request). A clean shutdown refuses new cloud operations
   (503 `cloud_backup_closed`) and waits up to 10 seconds for the running
   ones. A run still going after that is not stopped: it can no longer write
   the status or start publishing, and the next start counts it in
   `interrupted` (`last_error.code` `cloud_backup_interrupted`), as it does
   a run cut short by a crash or a panic. Collection later removes the set
   it left unpublished. The interval is measured on the wall clock. If the
   clock ran ahead and was then corrected, schedule times in the future are
   pulled back to the current time and `clock_reset_unix_ms` records when,
   so runs are delayed by at most one interval once the clock is stable.
   Repeated jumps forward can start extra runs; repeated jumps back keep
   moving the next run.
2. Drill a set now and then (`POST .../sets/{id}/drill`, or set
   `drill_every`). A drill downloads, verifies, restores to scratch and
   queries the copy without touching the live database.
3. Pin a set before an investigation so retention keeps it. Turn retention on
   only after you have a verified set; it never removes the newest verified
   set, pinned sets, or sets inside `keep_sets` (minimum 3) or `keep_days`
   (minimum 30). Restores and drills hold their own pin (`"kind":
   "operation"`) that an ordinary pin removal refuses. If a restore crashed
   and left one behind, remove it with `DELETE .../pins/{pin}?force=true`
   after making sure no restore of that set is still running on any host; the
   server cannot see restores run elsewhere.
4. To restore into the running installation, call `POST .../sets/{id}/restore`,
   select the returned copy with `POST /v1/admin/restores/select`, restart, and
   reissue scoped credentials (the lineage is new).
5. To restore on a new host, with the server stopped and nothing selected for
   you, run the offline commands. Put the policy's `target` JSON in a file:

   ```sh
   vlite-server cloud-sets --target-file target.json
   vlite-server cloud-restore --target-file target.json --set <set-id> \
     --output /data/restored \
     --admin-username recovery-admin --admin-password-file ./admin-password
   VLITE_ADMIN_DIR=/data/restored/admin vlite-server /data/restored/database.vlite
   ```

   `--output` must not exist yet; its parent must. The command drills the
   set, builds a new-lineage `database.vlite` and an `admin/` directory in a
   private staging directory next to the output (`.vlite-cloud-restore-*`,
   owner-only on Linux), checks that only the new super admin can sign in,
   and then moves the result into place in one rename: the output is either
   complete or absent. It prints a JSON report; a failure prints
   `{"error":...}` on stderr and exits 1, with diagnostics that never repeat
   argument or target-file values. Exit status 3 (`restore_cleanup_failed`)
   means the output is complete and usable but the restore's own pin could
   not be released: the error carries the report and the `DELETE
   .../pins/{pin}?force=true` call that removes it. Imported accounts are disabled
   with unusable passwords; the new super admin is the only working sign-in.
   No credential, session or webhook is restored. Start a server on the
   restored files, then re-create credentials and webhooks.

   If a restore is interrupted, run it again: it removes abandoned staging
   directories first. The final move never replaces anything: if a file or
   directory appears at `--output` while the restore runs, the restore fails
   with `restore_destination_exists` and leaves it untouched. A file system
   that cannot rename without replacing (`restore_publication_unsupported`)
   is refused. Use an `--output` parent that only the operator can write; on
   Windows the staging and output directories inherit its access list.

`--mode seed` and `{"mode":"seed"}` (same-lineage standby seeding) are
refused. Encrypted databases, served memory reservations, in-memory servers
and databases over 1 GiB (`cloud_database_too_large`) are refused with a
reason code. Before any download, a restore or drill checks that the staging
volume has room for twice the database plus the recovery bundle and a 64 MiB
margin (507 `restore_insufficient_space`); on Windows free space is not
checked, so make sure the volume has room.

For a local Azurite emulator only, set `target.endpoint` to its loopback URL,
`target.auth` to `emulator_shared_key`, and point
`VLITE_CLOUD_BACKUP_SHARED_KEY_FILE` (or `--shared-key-file`) at a private
file with the account key. The key is read per process and never written to
the policy; a non-loopback endpoint with a shared key is refused.

## Release and image verification

The release workflow builds and tests the runtime, API, and dashboard targets.
Tagged releases build each of `linux/amd64` and `linux/arm64` on a native
runner (no emulation), push the per-platform images by digest, and then tag
multi-platform manifest lists. Published images compile only the binaries
they ship. The workflow loads the standalone dashboard image
for the browser E2E run, publishes version tags only from matching `vX.Y.Z`
Git tags, checks the published multi-platform manifests, and runs REST, HTTPS,
and browser verification against the published digests. Workflow
dispatches from non-tag refs do not publish images.
