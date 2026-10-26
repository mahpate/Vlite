# Azure Container Apps (experimental)

Deploy one VLite API owner, including `/dashboard`, behind a stable HTTPS
Container Apps hostname without managing Kubernetes. The defaults reuse
ASENDME's `asendme-rg` resource group and discover its storage account from
the `asendme-env` environment's `admin-operations` storage binding **in your
selected subscription**. This avoids copying a deployment record from another
subscription. The script never switches subscriptions.

VLite gets a separate `vlite-data` file share, `vlite-env` environment and
`vlite-app` app. It never changes ASENDME's app, environment, admin share, account
settings or keys. Both workloads still share storage-account capacity, limits
and failure domain. A storage key authorizes the account, not just this share;
protect deployment access and coordinate key rotation with ASENDME.

## Storage support boundary

This is an **evaluation deployment option, not production qualification**.
Azure Files is a network filesystem. A Linux Python init container tests writable
access as VLite UID 10001, file sync, exclusive lock exclusion in a second
process (including SMB's access-denied contention result), successful lock
acquisition after release, atomic replacement and directory sync on the actual mount before the
API can start. A missing mount or failed operation blocks startup; there is no
ephemeral fallback and locks are never disabled. The probe writes and removes
only its own temporary scratch directory, not database data.

These checks do not prove cross-host locking, power-loss durability or workload
performance. Before production, test acknowledged writes across abrupt
termination/restart, tenant and credential persistence, second-owner rejection
across replicas, interrupted deployments, recovery, backup/restore, and sustained
write latency on this exact account/share. If the probe fails, inspect its error;
do not bypass it. Use a VM or AKS with managed disk instead.

## Deploy

Use PowerShell 7.2+, Azure CLI with its `containerapp` extension, and an account
authorized to create Container Apps environments/apps and file shares, and read
the existing storage account's keys. Providers `Microsoft.App` and
`Microsoft.OperationalInsights` must already be registered. The existing resource
group/account must be colocated. New resources, always-on compute, environment
logging and storage incur Azure charges.

```powershell
az login
az account set --subscription YOUR_SUBSCRIPTION_ID
az extension add --name containerapp --upgrade
.\deploy\azure-container-apps\deploy.ps1 `
  -SubscriptionId YOUR_SUBSCRIPTION_ID `
  -Dimensions 1536 `
  -AllowExperimentalAzureFiles
```

Replace `1536` with your embedding model's dimension. This is creation-only;
existing database headers win. The script explicitly creates **format 4** for
tenant collections. It defaults to the published `v0.4.1` API image and Python
3.12 probe image; use `-Image` and `-ProbeImage` to pin verified image digests.
`-Cpu 1` allocates 2 GiB RAM; supported CPU choices are 0.5, 1, 2 and 4, with
twice that number of GiB RAM. Size for the live dataset, indexes and replay peaks.
`-ShareSizeGiB` sets only a new share's quota (default 10 GiB).

Override `-ResourceGroup`, `-AsendmeEnvironment` or `-StorageAccount` when needed.
The destination environment, app and share names must start with `vlite-`.
An existing app/environment must have this script's ownership tag; an existing
share must have its ownership metadata. It will not adopt someone else's data.
Changing the existing mount to another account/share is rejected.

The API uses an Azure Container Apps secret. Its original admin bearer token is
stored under `$HOME\.vlite\azure\<deployment-hash>\admin-token`, in a private
directory, never printed or committed. Back it up privately; a rerun refuses to
rotate a missing token on an existing app. A temporary private JSON/YAML manifest
is removed after the Azure request. Do not enable PowerShell/Azure debug logging
or transcripts during deployment: the storage binding uses an account key.

On success the script prints API and dashboard HTTPS URLs only after the exact
new revision's VLite container is actually ready and an authenticated admin request succeeds. Read the token
locally from the printed file path and paste it into the dashboard. Applications
should use tenant-bound credentials instead of this bootstrap admin token.
Configure a custom domain through Container Apps if desired; updating the same
app preserves its service hostname. HTTPS ingress is public and authenticated;
private ingress/VNet isolation requires separate network configuration.

## Updates and failures

Run the same command with the desired image. All old VLite revisions are
deactivated, and **every revision must report zero replicas** before changing
the storage binding or starting a replacement. This intentionally causes
downtime. Single revision mode and min/max replicas 1 are additional limits,
not a substitute for writer handoff. Do not update through the portal, activate
old revisions, scale out, or run concurrent deployments from different machines.
A local deployment lock prevents overlapping runs on one machine only.

If deployment fails after stopping the old revisions, they stay stopped:
there is no automatic rollback that could introduce a second writer. Inspect
revision/replica state and init/API logs, fix the error, and rerun. Keep the same
app/environment/share and preserve the token. Never delete the share to fix a
deployment. Nothing here provides database replication or zero-downtime HA.
Keep independent backups outside this account and exercise recovery.

Local validation without Azure changes:

```powershell
pwsh -NoProfile -File .\deploy\azure-container-apps\test.ps1
python .\tests\azure_container_apps.py
```

References: [Container Apps storage mounts](https://learn.microsoft.com/azure/container-apps/storage-mounts),
[revision management](https://learn.microsoft.com/azure/container-apps/revisions-manage),
and [VLite Docker operations](../../docs/DOCKER-OPERATIONS.md).
