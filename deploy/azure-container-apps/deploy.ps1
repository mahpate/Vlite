#Requires -Version 7.2
[CmdletBinding()]
param(
    [string]$SubscriptionId,
    [string]$ResourceGroup = 'asendme-rg',
    [string]$StorageAccount,
    [string]$AsendmeEnvironment = 'asendme-env',
    [ValidateLength(3, 32)][ValidatePattern('^vlite-[a-z0-9]+(-[a-z0-9]+)*$')][string]$Environment = 'vlite-env',
    [ValidateLength(3, 32)][ValidatePattern('^vlite-[a-z0-9]+(-[a-z0-9]+)*$')][string]$AppName = 'vlite-app',
    [ValidateLength(3, 63)][ValidatePattern('^vlite-[a-z0-9]+(-[a-z0-9]+)*$')][string]$ShareName = 'vlite-data',
    [string]$Image = 'ghcr.io/mahpate/vlite-api:v0.6.0',
    [string]$ProbeImage = 'python:3.12-slim-bookworm',
    [ValidateRange(1, 65536)][int]$Dimensions = 1536,
    [ValidateSet(0.5, 1, 2, 4)][double]$Cpu = 1,
    [ValidateRange(1, 1024)][int]$ShareSizeGiB = 10,
    [ValidateRange(60, 3600)][int]$TimeoutSeconds = 600,
    [string]$StateDirectory = (Join-Path (Join-Path $HOME '.vlite') 'azure'),
    [switch]$AllowExperimentalAzureFiles
)

$ErrorActionPreference = 'Stop'

function Invoke-VLiteAz([string[]]$Arguments) {
    $output = & az @Arguments --only-show-errors --output json
    if ($LASTEXITCODE -ne 0) {
        throw "Azure CLI failed ($($Arguments[0..([Math]::Min(2, $Arguments.Length - 1))] -join ' ')). No automatic rollback was attempted."
    }
    if ($output) { ($output -join "`n") | ConvertFrom-Json -AsHashtable }
}

function Set-VLitePrivateDirectory([string]$Path) {
    [IO.Directory]::CreateDirectory($Path) | Out-Null
    if ($IsWindows) {
        $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
        $acl = [Security.AccessControl.DirectorySecurity]::new()
        $acl.SetAccessRuleProtection($true, $false)
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
        [IO.FileSystemAclExtensions]::SetAccessControl([IO.DirectoryInfo]::new($Path), $acl)
    } else {
        [IO.File]::SetUnixFileMode($Path, [IO.UnixFileMode]::UserRead -bor
            [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute)
    }
}

function New-VLiteTemplate($Config, [string]$EnvironmentId, [string]$Token, [string]$Revision) {
    $mount = @{ volumeName = 'vlite-data'; mountPath = '/data' }
    @{
        location = $Config.Location
        tags = @{ vliteDeployment = 'v1' }
        properties = @{
            managedEnvironmentId = $EnvironmentId
            configuration = @{
                activeRevisionsMode = 'Single'
                secrets = @(@{ name = 'admin-token'; value = $Token })
                ingress = @{
                    external = $true; targetPort = 8080; transport = 'http'
                    allowInsecure = $false
                    traffic = @(@{ latestRevision = $true; weight = 100 })
                }
            }
            template = @{
                revisionSuffix = $Revision
                terminationGracePeriodSeconds = 60
                scale = @{ minReplicas = 1; maxReplicas = 1; rules = @() }
                volumes = @(@{
                    name = 'vlite-data'; storageType = 'AzureFile'; storageName = "$($Config.AppName)-data"
                    mountOptions = 'uid=10001,gid=10001,file_mode=0600,dir_mode=0700'
                })
                initContainers = @(@{
                    name = 'storage-check'; image = $Config.ProbeImage
                    command = @('python', '-c', (Get-Content -Raw -LiteralPath (Join-Path $PSScriptRoot 'storage_probe.py')))
                    resources = @{ cpu = 0.25; memory = '0.5Gi' }
                    volumeMounts = @($mount)
                })
                containers = @(@{
                    name = 'vlite'; image = $Config.Image
                    command = @('/usr/local/bin/vlite-server')
                    args = @('/data/vectors.vlite', '--create-if-missing', "$($Config.Dimensions)",
                        '--create-format', '4', '--bind', '0.0.0.0:8080', '--allow-remote')
                    env = @(
                        @{ name = 'VLITE_AUTH_TOKEN'; secretRef = 'admin-token' },
                        @{ name = 'VLITE_ADMIN_DIR'; value = '/data/admin' }
                    )
                    resources = @{ cpu = $Config.Cpu; memory = "$($Config.Cpu * 2)Gi" }
                    volumeMounts = @($mount)
                    probes = @(
                        @{ type = 'Startup'; httpGet = @{ path = '/health'; port = 8080 }
                            periodSeconds = 5; timeoutSeconds = 3; failureThreshold = 120 },
                        @{ type = 'Readiness'; httpGet = @{ path = '/health'; port = 8080 }
                            periodSeconds = 10; timeoutSeconds = 3; failureThreshold = 3 },
                        @{ type = 'Liveness'; httpGet = @{ path = '/health'; port = 8080 }
                            periodSeconds = 30; timeoutSeconds = 3; failureThreshold = 3 }
                    )
                })
            }
        }
    }
}

function Stop-VLiteRevisions($Config) {
    $revisions = @(Invoke-VLiteAz @('containerapp', 'revision', 'list', '--name', $Config.AppName,
        '--resource-group', $Config.ResourceGroup))
    foreach ($revision in $revisions) {
        if ($revision.properties.active) {
            Invoke-VLiteAz @('containerapp', 'revision', 'deactivate', '--name', $Config.AppName,
                '--resource-group', $Config.ResourceGroup, '--revision', $revision.name) | Out-Null
        }
    }
    $deadline = [DateTime]::UtcNow.AddSeconds($Config.TimeoutSeconds)
    do {
        $remaining = 0
        foreach ($revision in $revisions) {
            $replicas = @(Invoke-VLiteAz @('containerapp', 'replica', 'list', '--name', $Config.AppName,
                '--resource-group', $Config.ResourceGroup, '--revision', $revision.name))
            $remaining += $replicas.Count
        }
        if ($remaining -eq 0) { return }
        if ([DateTime]::UtcNow -ge $deadline) { throw 'Old VLite replicas did not stop. Refusing a second database owner.' }
        Start-Sleep -Seconds 5
    } while ($true)
}

function Wait-VLiteReady($Config, [string]$Revision, [string]$Token) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Config.TimeoutSeconds)
    do {
        $app = Invoke-VLiteAz @('containerapp', 'show', '--name', $Config.AppName,
            '--resource-group', $Config.ResourceGroup)
        if ($app.properties.latestRevisionName -eq $Revision -and
            $app.properties.latestReadyRevisionName -eq $Revision) {
            $replicas = @(Invoke-VLiteAz @('containerapp', 'replica', 'list', '--name', $Config.AppName,
                '--resource-group', $Config.ResourceGroup, '--revision', $Revision))
            $ready = @($replicas | Where-Object {
                @($_.properties.containers | Where-Object { $_.name -eq 'vlite' -and $_.ready -and $_.started }).Count -eq 1
            })
            if ($replicas.Count -ne 1 -or $ready.Count -ne 1) {
                if ([DateTime]::UtcNow -ge $deadline) { throw 'VLite container is not ready. Inspect storage-check init logs before retrying.' }
                Start-Sleep -Seconds 5
                continue
            }
            $url = "https://$($app.properties.configuration.ingress.fqdn)"
            try {
                $response = Invoke-WebRequest "$url/v1/admin/tenants" -Headers @{ Authorization = "Bearer $Token" } `
                    -SkipHttpErrorCheck -TimeoutSec 30
                if ($response.StatusCode -eq 200) {
                    $body = $response.Content | ConvertFrom-Json -AsHashtable
                    if ($body.ContainsKey('items')) { return $url }
                }
                Write-Warning "VLite authenticated readiness returned HTTP $($response.StatusCode); waiting for ingress."
            } catch [System.Net.Http.HttpRequestException], [System.Threading.Tasks.TaskCanceledException], [System.TimeoutException] {
                Write-Warning 'VLite ingress is not reachable yet; waiting for propagation.'
            }
        }
        if ([DateTime]::UtcNow -ge $deadline) {
            throw 'VLite did not become ready. Inspect the storage-check init container and VLite logs. No rollback was attempted.'
        }
        Start-Sleep -Seconds 5
    } while ($true)
}

function Invoke-VLiteDeployment($Config) {
    if (-not $Config.SubscriptionId) { throw 'Pass -SubscriptionId explicitly; the script never changes your selected subscription.' }
    if (-not $Config.AllowExperimentalAzureFiles) {
        throw 'Azure Files is unqualified for VLite production use. Pass -AllowExperimentalAzureFiles for an isolated evaluation.'
    }
    $account = Invoke-VLiteAz @('account', 'show')
    if ($account.id -ne $Config.SubscriptionId) { throw 'Selected Azure subscription does not match -SubscriptionId. No resources changed.' }
    if (-not $Config.StorageAccount) {
        $Config.StorageAccount = Invoke-VLiteAz @('containerapp', 'env', 'storage', 'show',
            '--name', $Config.AsendmeEnvironment, '--resource-group', $Config.ResourceGroup,
            '--storage-name', 'admin-operations', '--query', 'properties.azureFile.accountName')
        if (-not $Config.StorageAccount) { throw 'Cannot discover ASENDME storage. Pass -StorageAccount explicitly.' }
    }
    $group = Invoke-VLiteAz @('group', 'show', '--name', $Config.ResourceGroup)
    $storage = Invoke-VLiteAz @('storage', 'account', 'show', '--name', $Config.StorageAccount,
        '--resource-group', $Config.ResourceGroup)
    $Config.Location = $group.location
    if ($storage.primaryLocation -ne $Config.Location) { throw 'Storage account and resource group locations differ. Choose colocated resources.' }
    $apps = @(Invoke-VLiteAz @('containerapp', 'list', '--resource-group', $Config.ResourceGroup))
    $existing = @($apps | Where-Object name -eq $Config.AppName)
    $environments = @(Invoke-VLiteAz @('containerapp', 'env', 'list', '--resource-group', $Config.ResourceGroup))
    $environment = @($environments | Where-Object name -eq $Config.Environment)
    $shares = @(Invoke-VLiteAz @('storage', 'share-rm', 'list', '--storage-account', $Config.StorageAccount,
        '--resource-group', $Config.ResourceGroup))
    $share = @($shares | Where-Object name -eq $Config.ShareName)
    if ($share.Count) {
        $share = @(Invoke-VLiteAz @('storage', 'share-rm', 'show', '--storage-account', $Config.StorageAccount,
            '--resource-group', $Config.ResourceGroup, '--name', $Config.ShareName))
    }
    if ($existing.Count -and $existing[0].tags.vliteDeployment -ne 'v1') { throw 'Refusing to modify an app not owned by this VLite deployment.' }
    if ($environment.Count -and $environment[0].tags.vliteDeployment -ne 'v1') { throw 'Refusing to modify a non-VLite environment.' }
    if ($share.Count -and $share[0].metadata.vliteApp -ne $Config.AppName) { throw 'Refusing to mount an existing share not owned by this VLite app.' }
    if ($existing.Count) {
        $volume = @($existing[0].properties.template.volumes | Where-Object storageName -eq "$($Config.AppName)-data")
        if ($volume.Count -ne 1 -or -not $environment.Count -or
            $existing[0].properties.managedEnvironmentId -ne $environment[0].id -or -not $share.Count) {
            throw 'Existing VLite storage/environment differs. Refusing to move the database.'
        }
        $binding = Invoke-VLiteAz @('containerapp', 'env', 'storage', 'show',
            '--name', $Config.Environment, '--resource-group', $Config.ResourceGroup,
            '--storage-name', "$($Config.AppName)-data", '--query',
            'properties.azureFile.{account:accountName,share:shareName}')
        if ($binding.account -ne $Config.StorageAccount -or $binding.share -ne $Config.ShareName) {
            throw 'Existing VLite mount points at a different account/share. Refusing to move the database.'
        }
    }
    $identity = "$($account.id)/$($Config.ResourceGroup)/$($Config.AppName)"
    $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData(
        [Text.Encoding]::UTF8.GetBytes($identity))).ToLowerInvariant().Substring(0, 16)
    $private = Join-Path $Config.StateDirectory $hash
    Set-VLitePrivateDirectory $private
    $lock = [IO.File]::Open((Join-Path $private 'deployment.lock'), 'OpenOrCreate', 'ReadWrite', 'None')
    $manifest = Join-Path $private 'deployment.json'
    try {
        $tokenPath = Join-Path $private 'admin-token'
        if (-not (Test-Path -LiteralPath $tokenPath)) {
            if ($existing.Count) { throw "Existing deployment requires its original token at $tokenPath; refusing to rotate it." }
            $token = [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32)).ToLowerInvariant()
            [IO.File]::WriteAllText($tokenPath, $token)
        }
        $token = [IO.File]::ReadAllText($tokenPath).Trim()
        if ($token -cnotmatch '^[a-zA-Z0-9_-]{32,256}$') { throw 'Invalid private admin token.' }
        if (-not $share.Count) {
            Invoke-VLiteAz @('storage', 'share-rm', 'create', '--resource-group', $Config.ResourceGroup,
                '--storage-account', $Config.StorageAccount, '--name', $Config.ShareName,
                '--quota', "$($Config.ShareSizeGiB)", '--metadata', "vliteApp=$($Config.AppName)") | Out-Null
        }
        if (-not $environment.Count) {
            $environment = @(Invoke-VLiteAz @('containerapp', 'env', 'create', '--name', $Config.Environment,
                '--resource-group', $Config.ResourceGroup, '--location', $Config.Location,
                '--tags', 'vliteDeployment=v1'))
        }
        if ($existing.Count) {
            Write-Warning 'Stopping every VLite revision before updating. This causes downtime; failure leaves the old revisions stopped.'
            Stop-VLiteRevisions $Config
        }
        $keys = @(Invoke-VLiteAz @('storage', 'account', 'keys', 'list', '--account-name', $Config.StorageAccount,
            '--resource-group', $Config.ResourceGroup))
        try {
            Invoke-VLiteAz @('containerapp', 'env', 'storage', 'set', '--name', $Config.Environment,
                '--resource-group', $Config.ResourceGroup, '--storage-name', "$($Config.AppName)-data",
                '--azure-file-account-name', $Config.StorageAccount, '--azure-file-account-key', $keys[0].value,
                '--azure-file-share-name', $Config.ShareName, '--access-mode', 'ReadWrite') | Out-Null
        } finally { $keys = $null }
        $revision = "r$([DateTime]::UtcNow.ToString('yyyyMMddHHmmss'))-$([Guid]::NewGuid().ToString('N').Substring(0, 6))"
        $template = New-VLiteTemplate $Config $environment[0].id $token $revision
        $template | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath $manifest -Encoding utf8
        $verb = if ($existing.Count) { 'update' } else { 'create' }
        Invoke-VLiteAz @('containerapp', $verb, '--name', $Config.AppName,
            '--resource-group', $Config.ResourceGroup, '--yaml', $manifest) | Out-Null
        $url = Wait-VLiteReady $Config "$($Config.AppName)--$revision" $token
        Write-Host "VLite API: $url"
        Write-Host "Dashboard: $url/dashboard"
        Write-Host "Admin token remains private at $tokenPath"
        Write-Warning 'Storage primitive checks passed, not crash-durability qualification. Do not put production data here yet.'
    } finally {
        if (Test-Path -LiteralPath $manifest) { Remove-Item -LiteralPath $manifest -Force }
        $token = $null
        $lock.Dispose()
    }
}

if ($MyInvocation.InvocationName -ne '.') {
    Invoke-VLiteDeployment @{
        SubscriptionId = $SubscriptionId; ResourceGroup = $ResourceGroup; StorageAccount = $StorageAccount
        AsendmeEnvironment = $AsendmeEnvironment
        Environment = $Environment; AppName = $AppName; ShareName = $ShareName
        Image = $Image; ProbeImage = $ProbeImage; Dimensions = $Dimensions; Cpu = $Cpu
        ShareSizeGiB = $ShareSizeGiB; TimeoutSeconds = $TimeoutSeconds; StateDirectory = $StateDirectory
        AllowExperimentalAzureFiles = $AllowExperimentalAzureFiles.IsPresent
    }
}
