#Requires -Version 7.2
[CmdletBinding()]
param(
    [hashtable]$Config,
    [switch]$EnableHa,
    [string]$SubscriptionId,
    [string]$ResourceGroup,
    [string]$Location,
    [string]$DeploymentId,
    [string]$Environment,
    [string]$SubnetId,
    [string]$GatewayName,
    [string]$WriterName,
    [string]$ReaderName,
    [string]$StorageAccount,
    [string]$ShareName,
    [string]$WriterStorageName,
    [string]$Image,
    [string]$WorkloadProfileName,
    [string]$GatewayIdentityId,
    [string]$WriterIdentityId,
    [string]$ReaderIdentityId,
    [hashtable]$ControlTarget,
    [hashtable]$ArchiveTarget,
    [string]$WriterAudience,
    [string]$ReaderAudience,
    [string]$ActiveKid,
    [int]$SecretGeneration,
    [string]$SigningKeySourceFile,
    [string]$PublicKeyBundleSourceFile,
    [int]$GatewayMinReplicas,
    [int]$GatewayMaxReplicas,
    [int]$GatewayHttpConcurrency,
    [double]$GatewayCpu,
    [string]$GatewayMemory,
    [double]$WriterCpu,
    [string]$WriterMemory,
    [double]$ReaderCpu,
    [string]$ReaderMemory,
    [int]$ReaderMinReplicas,
    [int]$ReaderMaxReplicas,
    [int]$ReaderHttpConcurrency,
    [long]$ReaderCapacityBytes,
    [long]$ReaderStalenessBoundMs,
    [long]$ReaderPollIntervalMs,
    [long]$SCtlMs,
    [long]$PollIntervalMs,
    [long]$EnvelopeLifetimeMs,
    [long]$MaxLifetimeMs,
    [long]$SkewMs,
    [long]$ReadinessPropagationMs,
    [long]$DrainTimeoutMs,
    [long]$SafetyMarginMs,
    [int]$GatewayTerminationGracePeriodSeconds,
    [int]$WriterTerminationGracePeriodSeconds,
    [int]$ReaderTerminationGracePeriodSeconds,
    [int]$GatewayStartupBudgetSeconds,
    [int]$WriterStartupBudgetSeconds,
    [int]$ReaderStartupBudgetSeconds,
    [long]$WriterStopBudgetMs,
    [long]$ReaderStopBudgetMs,
    [long]$MaxRequestBytes,
    [int]$MaxInFlight,
    [long]$UpstreamTimeoutMs,
    [int]$MaxBuckets,
    [int]$TimeoutSeconds,
    [switch]$AllowUnqualifiedWriterStorage,
    [switch]$EnableReaders,
    [switch]$InitializeTenant,
    [int]$Dimensions,
    [int]$CreateFormat,
    [switch]$Rotate
)

. (Join-Path $PSScriptRoot 'templates.ps1')

function Invoke-VLiteAz([string[]]$Arguments) {
    # Do not forward native diagnostics: a failed manifest request can echo secrets.
    $output = & az @Arguments --only-show-errors --output json 2>$null
    if ($LASTEXITCODE -ne 0) {
        throw 'Azure CLI failed. No automatic rollback or writer reactivation was attempted.'
    }
    if (-not $output) {
        if (($Arguments[0..2] -join ' ') -eq 'containerapp revision deactivate') { return }
        throw 'Azure CLI returned no JSON; refusing an unverifiable result.'
    }
    try {
        $result = ($output -join "`n") | ConvertFrom-Json -AsHashtable -NoEnumerate -ErrorAction Stop
    } catch {
        throw 'Azure CLI returned invalid JSON; response contents are withheld to protect secrets.'
    }
    return ,$result
}

function Set-VLitePrivateDirectory([string]$Path) {
    if ((Test-Path -LiteralPath $Path) -and
        ((Get-Item -LiteralPath $Path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Private state must not be a symbolic link or junction.'
    }
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

function Write-VLiteHaPrivateJson([string]$Path, [hashtable]$Value) {
    # CreateNew refuses stale manifests, symlinks, and accidental secret replacement.
    $options = [IO.FileStreamOptions]::new()
    $options.Mode = [IO.FileMode]::CreateNew
    $options.Access = [IO.FileAccess]::Write
    $options.Share = [IO.FileShare]::None
    if (-not $IsWindows) {
        $options.UnixCreateMode = [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite
    }
    $stream = [IO.FileStream]::new($Path, $options)
    try {
        $bytes = [Text.UTF8Encoding]::new($false).GetBytes(
            ($Value | ConvertTo-Json -Depth 30 -Compress -ErrorAction Stop))
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    } finally { $stream.Dispose() }
}

function Get-VLiteHaStateRoot {
    Join-Path $HOME '.vlite'
}

function Read-VLiteHaPrivateJson([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Original material must be a regular private file, not a link.'
    }
    try {
        $value = Get-Content -Raw -LiteralPath $Path -ErrorAction Stop |
            ConvertFrom-Json -AsHashtable -ErrorAction Stop
    } catch {
        throw 'Original private state could not be read as JSON; contents are withheld. Restore original material.'
    }
    if ($value -isnot [hashtable]) { throw 'Original private state must be a JSON object; restore original material.' }
    return $value
}

function ConvertTo-VLiteHaOrderedValue($Value) {
    if ($Value -is [Collections.IDictionary]) {
        $ordered = [ordered]@{}
        foreach ($key in @($Value.Keys | Sort-Object -CaseSensitive)) {
            $ordered[$key] = ConvertTo-VLiteHaOrderedValue $Value[$key]
        }
        return $ordered
    }
    if ($Value -is [array]) {
        return ,@($Value | ForEach-Object { ConvertTo-VLiteHaOrderedValue $_ })
    }
    return $Value
}

function Assert-VLiteHaDeploymentParameters([hashtable]$Config) {
    if ($Config.EnableHa -isnot [bool] -or -not $Config.EnableHa) {
        throw 'HA is opt-in. Pass -EnableHa explicitly; no Azure call was made.'
    }
    foreach ($key in @('EnableReaders', 'InitializeTenant', 'Rotate', 'AllowUnqualifiedWriterStorage')) {
        if ($Config.ContainsKey($key) -and $Config[$key] -isnot [bool]) {
            throw "$key must be an explicit Boolean, not a string or number."
        }
    }
    if ($Config.Rotate) { throw 'Automatic rotation is unsupported. Use the reviewed manual generation rollout procedure.' }
    foreach ($key in @('SubscriptionId', 'ResourceGroup', 'Location', 'DeploymentId', 'Environment',
        'SubnetId', 'GatewayName', 'WriterName', 'StorageAccount', 'ShareName', 'WriterStorageName',
        'Image', 'WorkloadProfileName', 'GatewayIdentityId', 'WriterIdentityId', 'WriterAudience', 'ActiveKid')) {
        if ($Config[$key] -isnot [string] -or [string]::IsNullOrWhiteSpace($Config[$key])) {
            throw "Required HA input: $key."
        }
    }
    foreach ($key in @('SubscriptionId', 'ResourceGroup', 'DeploymentId')) {
        if ($Config[$key] -notmatch '^[a-zA-Z0-9][a-zA-Z0-9_.-]*$' -or
            $Config[$key].EndsWith('.')) { throw "Unsafe state path component: $key." }
    }
    foreach ($key in @('Environment', 'GatewayName', 'WriterName', 'ShareName', 'WriterStorageName', 'ReaderName')) {
        if ($key -eq 'ReaderName' -and -not $Config.EnableReaders -and -not $Config[$key]) { continue }
        if ($Config[$key] -notmatch '^[a-z][a-z0-9-]{1,30}[a-z0-9]$' -or $Config[$key] -match '--') {
            throw "Invalid HA resource name: $key."
        }
        if ($Config[$key] -in @('vlite-app', 'vlite-env', 'vlite-data')) {
            throw "Refusing parent singleton resource name: $key."
        }
    }
    $names = @($Config.GatewayName, $Config.WriterName)
    if ($Config.EnableReaders) { $names += $Config.ReaderName }
    if (@($names | Select-Object -Unique).Count -ne $names.Count) { throw 'HA app names must be distinct.' }
    if ($Config.Image -cnotmatch '^[^\s@]+@sha256:[a-fA-F0-9]{64}$') {
        throw 'Image is mandatory and must be pinned by @sha256:<64 hex>; tags alone are refused.'
    }
    if (-not $Config.AllowUnqualifiedWriterStorage) {
        throw 'Writer Azure Files is unqualified. -AllowUnqualifiedWriterStorage permits nonproduction evaluation only.'
    }
    foreach ($key in @('SecretGeneration', 'GatewayMinReplicas', 'GatewayMaxReplicas',
        'GatewayHttpConcurrency', 'SCtlMs', 'PollIntervalMs', 'EnvelopeLifetimeMs', 'MaxLifetimeMs',
        'MaxRequestBytes', 'MaxInFlight', 'UpstreamTimeoutMs', 'MaxBuckets',
        'TimeoutSeconds')) {
        if (-not $Config.ContainsKey($key) -or $Config[$key] -isnot [ValueType] -or
            $Config[$key] -is [bool] -or $Config[$key] -le 0 -or
            [decimal]$Config[$key] % 1 -ne 0) { throw "Required positive integer HA input: $key." }
    }
    foreach ($key in @('SkewMs', 'ReadinessPropagationMs', 'DrainTimeoutMs', 'SafetyMarginMs')) {
        if (-not $Config.ContainsKey($key) -or $Config[$key] -isnot [ValueType] -or
            $Config[$key] -is [bool] -or $Config[$key] -lt 0 -or
            [decimal]$Config[$key] % 1 -ne 0) { throw "Required nonnegative integer HA input: $key." }
    }
    if ($Config.GatewayMinReplicas -lt 2 -or $Config.GatewayMaxReplicas -lt $Config.GatewayMinReplicas -or
        $Config.GatewayMaxReplicas -gt 1000) { throw 'Gateway replicas require 2 <= min <= mandatory max <= 1000.' }
    if ($Config.PollIntervalMs -ge $Config.SCtlMs) { throw 'PollIntervalMs must be positive and below SCtlMs.' }
    if ($Config.EnvelopeLifetimeMs -gt $Config.MaxLifetimeMs) { throw 'EnvelopeLifetimeMs exceeds MaxLifetimeMs.' }
    if ($Config.WorkloadProfileName -cne 'Consumption') {
        throw 'Only Consumption is supported; Dedicated requires measured capacity qualification.'
    }
    $roles = @('Gateway', 'Writer')
    if ($Config.EnableReaders) {
        $roles += 'Reader'
        foreach ($key in @('ReaderMaxReplicas', 'ReaderMinReplicas', 'ReaderHttpConcurrency',
            'ReaderCapacityBytes', 'ReaderStalenessBoundMs', 'ReaderPollIntervalMs')) {
            if (-not $Config.ContainsKey($key) -or $Config[$key] -isnot [ValueType] -or
                $Config[$key] -is [bool] -or $Config[$key] -le 0 -or
                [decimal]$Config[$key] % 1 -ne 0) { throw "Required positive integer reader input: $key." }
        }
        if ($Config.ReaderMaxReplicas -lt $Config.ReaderMinReplicas -or $Config.ReaderMaxReplicas -gt 1000) {
            throw 'Reader replicas require 1 <= min <= mandatory max <= 1000.'
        }
        if (-not $Config.ReaderAudience) { throw 'ReaderAudience is required with EnableReaders.' }
    }
    foreach ($role in $roles) {
        $cpu = $Config["${role}Cpu"]
        $memory = $Config["${role}Memory"]
        if ($cpu -isnot [ValueType] -or $cpu -is [bool] -or $cpu -lt 0.25 -or $cpu -gt 4 -or
            [decimal]$cpu % 0.25 -ne 0 -or $memory -notmatch '^([0-9]+(?:\.[0-9]+)?)Gi$' -or
            [decimal]::Parse($Matches[1], [Globalization.CultureInfo]::InvariantCulture) -ne
                [decimal]$cpu * 2) { throw "Explicit Consumption CPU/memory pair required for $role." }
        if (-not $Config["${role}IdentityId"]) { throw "Existing ${role}IdentityId is required." }
    }
    if ($Config.InitializeTenant -and
        ($Config.Dimensions -lt 1 -or $Config.Dimensions -gt 65536 -or $Config.CreateFormat -notin @(3, 4, 5))) {
        throw 'InitializeTenant requires explicit Dimensions (1..65536) and CreateFormat (3, 4, or 5).'
    }
    foreach ($key in @('ControlTarget', 'ArchiveTarget')) {
        $target = $Config[$key]
        if ($target -isnot [hashtable] -or $target.auth -cne 'managed_identity' -or
            -not $target.account -or -not $target.container -or -not $target.coordination_container -or
            -not $target.prefix -or -not $target.client_id -or $target.account -ne $Config.StorageAccount) {
            throw "$key requires an explicit managed-identity Blob target in the approved storage account."
        }
        $allowed = @('account', 'container', 'coordination_container', 'prefix', 'auth', 'client_id')
        if (@($target.Keys | Where-Object { $_ -notin $allowed }).Count) {
            throw "$key contains unsupported fields; shared keys and connection strings are prohibited."
        }
    }
    if ($Config.ControlTarget.container -in @($Config.ArchiveTarget.container, $Config.ArchiveTarget.coordination_container) -or
        $Config.ControlTarget.coordination_container -in @($Config.ArchiveTarget.container, $Config.ArchiveTarget.coordination_container)) {
        throw 'Control and tenant archive containers must be isolated; prefixes are not an RBAC boundary.'
    }
    Test-VLiteHaParameters -Config $Config | Out-Null
}

function Assert-VLiteHaOwned($Resource, [hashtable]$Config, [string]$Role) {
    if ($Resource -isnot [hashtable] -or $Resource.tags.vliteHaDeployment -cne $Config.DeploymentId -or
        ($Role -and $Resource.tags.vliteHaRole -cne $Role)) {
        throw 'Refusing an unowned resource or an HA role mismatch; adoption is not supported.'
    }
}

function Get-VLiteHaArray([string[]]$Arguments) {
    $result = Invoke-VLiteAz $Arguments
    if ($result -isnot [array]) { throw 'Missing or malformed Azure inventory; a complete JSON array is required.' }
    return ,$result
}

function Assert-VLiteHaIsolation([hashtable]$Config, [string]$EnvironmentId, [string[]]$RequiredBackends) {
    $apps = Get-VLiteHaArray @('containerapp', 'list', '--subscription', $Config.SubscriptionId,
        '--resource-group', $Config.ResourceGroup)
    $routes = Get-VLiteHaArray @('containerapp', 'env', 'http-route-config', 'list',
        '--subscription', $Config.SubscriptionId, '--resource-group', $Config.ResourceGroup,
        '--name', $Config.Environment)
    if ($routes.Count -ne 0) { throw 'Environment HTTP routes are prohibited; no routes were modified.' }
    $expected = @($Config.WriterName)
    if ($Config.EnableReaders) { $expected += $Config.ReaderName }
    foreach ($app in $apps) {
        if ($app.tags.vliteHaDeployment -ceq $Config.DeploymentId -and
            $app.tags.vliteHaRole -in @('writer', 'reader') -and $app.name -notin $expected) {
            throw 'Unknown owned backend in HA inventory; refusing topology drift.'
        }
    }
    foreach ($name in $RequiredBackends) {
        $matches = @($apps | Where-Object { $_.name -eq $name })
        if ($matches.Count -ne 1) { throw 'Expected backend is missing or duplicated in HA inventory.' }
        $app = $matches[0]
        $role = if ($name -eq $Config.WriterName) { 'writer' } else { 'reader' }
        Assert-VLiteHaOwned $app $Config $role
        $properties = $app.properties
        if ($properties.managedEnvironmentId -ne $EnvironmentId -or
            ($properties.environmentId -and $properties.environmentId -ne $EnvironmentId)) {
            throw 'Backend environment does not match the approved HA environment.'
        }
        $ingress = $properties.configuration.ingress
        if ($ingress.external -isnot [bool] -or $ingress.external -or
            $ingress.allowInsecure -isnot [bool] -or $ingress.allowInsecure -or
            $ingress.transport -cne 'http' -or $ingress.targetPort -ne 8080 -or
            @($ingress.additionalPortMappings | Where-Object { $null -ne $_ }).Count) {
            throw 'Backend ingress is not explicitly internal HTTPS-only HTTP on port 8080 without extra ports.'
        }
    }
}

function Assert-VLiteHaQualification([hashtable]$Config) {
    # No Boolean attestation can substitute for the missing executable protocols.
    throw ('HA deployment is blocked: no approved verifier is wired for the digest-bound linux/amd64 ' +
        'runtime capabilities, complete route inventory, effective container-scoped identity grants, ' +
        'quota/storage qualification, control-directory writer-only routing, and recovery/authority ' +
        'create-only initialization or expected-epoch CAS handoff with quiesce/loss acknowledgement. ' +
        'Provision these operational interfaces before enabling rollout; no authority PUT or invented CLI flag is used.')
}

function Stop-VLiteRevisions([hashtable]$Config) {
    $name = if ($Config.WriterName) { $Config.WriterName } else { $Config.AppName }
    if (-not $name -or $name -in @('vlite-app', 'vlite-env', 'vlite-data') -or
        -not $Config.SubscriptionId -or -not $Config.ResourceGroup -or
        -not $Config.ContainsKey('TimeoutSeconds') -or $Config.TimeoutSeconds -lt 0) {
        throw 'Explicit HA writer, subscription, resource group and stop timeout are required.'
    }
    $known = @{}
    $deadline = [DateTime]::UtcNow.AddSeconds($Config.TimeoutSeconds)
    do {
        $revisions = Get-VLiteHaArray @('containerapp', 'revision', 'list', '--name', $name,
            '--subscription', $Config.SubscriptionId, '--resource-group', $Config.ResourceGroup)
        foreach ($revision in $revisions) {
            if (-not $revision.name -or $revision.properties.active -isnot [bool]) {
                throw 'Malformed revision inventory; refusing to infer a stopped writer.'
            }
            $known[$revision.name] = $true
            if ($revision.properties.active) {
                Invoke-VLiteAz @('containerapp', 'revision', 'deactivate', '--name', $name,
                    '--subscription', $Config.SubscriptionId, '--resource-group', $Config.ResourceGroup,
                    '--revision', $revision.name) | Out-Null
            }
        }
        $remaining = 0
        foreach ($revisionName in $known.Keys) {
            $replicas = Get-VLiteHaArray @('containerapp', 'replica', 'list', '--name', $name,
                '--subscription', $Config.SubscriptionId, '--resource-group', $Config.ResourceGroup,
                '--revision', $revisionName)
            $remaining += $replicas.Count
        }
        if ($remaining -eq 0) {
            # Re-enumerate to catch revisions introduced during the drain.
            $confirmed = Get-VLiteHaArray @('containerapp', 'revision', 'list', '--name', $name,
                '--subscription', $Config.SubscriptionId, '--resource-group', $Config.ResourceGroup)
            $stable = $true
            foreach ($revision in $confirmed) {
                if (-not $revision.name -or $revision.properties.active -isnot [bool]) {
                    throw 'Malformed confirmation inventory; refusing to infer a stopped writer.'
                }
                if (-not $known.ContainsKey($revision.name) -or $revision.properties.active) { $stable = $false }
            }
            if ($stable) { return }
        }
        if ([DateTime]::UtcNow -ge $deadline) {
            throw 'Old HA writer replicas did not stop. Refusing a second database owner; no rollback or reactivation.'
        }
        Start-Sleep -Seconds 5
    } while ($true)
}

function Wait-VLiteHaReady([hashtable]$Config, [string]$Name, [string]$Revision, [int]$Minimum) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Config.TimeoutSeconds)
    do {
        $app = Invoke-VLiteAz @('containerapp', 'show', '--subscription', $Config.SubscriptionId,
            '--resource-group', $Config.ResourceGroup, '--name', $Name)
        if ($app.properties.latestRevisionName -eq $Revision -and
            $app.properties.latestReadyRevisionName -eq $Revision) {
            $replicas = Get-VLiteHaArray @('containerapp', 'replica', 'list', '--subscription',
                $Config.SubscriptionId, '--resource-group', $Config.ResourceGroup,
                '--name', $Name, '--revision', $Revision)
            $ready = @($replicas | Where-Object {
                @($_.properties.containers | Where-Object {
                    $_.name -eq 'vlite' -and $_.ready -ceq $true -and $_.started -ceq $true
                }).Count -eq 1
            })
            if ($replicas.Count -ge $Minimum -and $ready.Count -eq $replicas.Count) { return }
        }
        if ([DateTime]::UtcNow -ge $deadline) { throw 'Exact HA revision replicas did not become ready; no rollback attempted.' }
        Start-Sleep -Seconds 5
    } while ($true)
}

function Invoke-VLiteHaDeployment([hashtable]$Config) {
    $ErrorActionPreference = 'Stop'
    $Config = $Config.Clone()
    # Runtime paths are fixed secret projections, never the operator's key inputs.
    $Config.ControlTargetFile = '/run/vlite/control-target.json'
    $Config.ControlObject = 'control.json'
    $Config.SigningKeyFile = '/run/vlite/signing-key'
    $Config.PublicKeyBundleFile = '/run/vlite/public-keys.json'
    $Config.ReaderTargetFile = '/run/vlite/archive-target.json'
    $Config.FollowerDir = '/data'
    Assert-VLiteHaDeploymentParameters $Config
    $account = Invoke-VLiteAz @('account', 'show')
    if ($account.id -ne $Config.SubscriptionId) {
        throw 'Selected Azure subscription does not match SubscriptionId; the script never switches subscriptions.'
    }
    $subscription = "/subscriptions/$($Config.SubscriptionId)"
    $groupId = "$subscription/resourceGroups/$($Config.ResourceGroup)"
    $environmentId = "$groupId/providers/Microsoft.App/managedEnvironments/$($Config.Environment)"
    $environment = Invoke-VLiteAz @('containerapp', 'env', 'show', '--subscription', $Config.SubscriptionId,
        '--resource-group', $Config.ResourceGroup, '--name', $Config.Environment)
    Assert-VLiteHaOwned $environment $Config ''
    if ($environment.id -ne $environmentId -or $environment.location -ne $Config.Location -or
        $environment.properties.zoneRedundant -isnot [bool] -or -not $environment.properties.zoneRedundant -or
        $environment.properties.provisioningState -ne 'Succeeded' -or
        $environment.properties.vnetConfiguration.infrastructureSubnetId -ne $Config.SubnetId -or
        @($environment.properties.workloadProfiles | Where-Object {
            $_.name -ceq 'Consumption' -and $_.workloadProfileType -ceq 'Consumption'
        }).Count -ne 1) {
        throw 'An existing owned, ready, zone-redundant workload-profile HA environment on the approved subnet is required.'
    }
    if (-not $Config.SubnetId.StartsWith("$subscription/", [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The approved subnet must be in the selected subscription.'
    }
    $subnet = Invoke-VLiteAz @('network', 'vnet', 'subnet', 'show', '--ids', $Config.SubnetId,
        '--subscription', $Config.SubscriptionId)
    if ($subnet.id -ne $Config.SubnetId -or
        @($subnet.delegations | Where-Object { $_.serviceName -eq 'Microsoft.App/environments' }).Count -ne 1) {
        throw 'Approved existing subnet delegation could not be verified.'
    }
    $storage = Invoke-VLiteAz @('storage', 'account', 'show', '--subscription', $Config.SubscriptionId,
        '--resource-group', $Config.ResourceGroup, '--name', $Config.StorageAccount)
    if ($storage.id -ne "$groupId/providers/Microsoft.Storage/storageAccounts/$($Config.StorageAccount)" -or
        $storage.primaryLocation -ne $Config.Location -or $storage.sku.name -notin @('Standard_ZRS', 'Premium_ZRS') -or
        $storage.enableHttpsTrafficOnly -isnot [bool] -or -not $storage.enableHttpsTrafficOnly) {
        throw 'Approved colocated ZRS HTTPS-only storage account is required; no account settings will be changed.'
    }
    $share = Invoke-VLiteAz @('storage', 'share-rm', 'show', '--subscription', $Config.SubscriptionId,
        '--resource-group', $Config.ResourceGroup, '--storage-account', $Config.StorageAccount, '--name', $Config.ShareName)
    if ($share.metadata.vliteHaDeployment -cne $Config.DeploymentId) {
        throw 'Writer share must already exist with matching vliteHaDeployment ownership metadata.'
    }
    $binding = Invoke-VLiteAz @('containerapp', 'env', 'storage', 'show', '--subscription', $Config.SubscriptionId,
        '--resource-group', $Config.ResourceGroup, '--name', $Config.Environment, '--storage-name', $Config.WriterStorageName)
    if ($binding.properties.azureFile.accountName -ne $Config.StorageAccount -or
        $binding.properties.azureFile.shareName -ne $Config.ShareName -or
        $binding.properties.azureFile.accessMode -cne 'ReadWrite') {
        throw 'Existing writer storage binding differs; refusing to adopt or repoint storage.'
    }
    $roles = @('writer')
    if ($Config.EnableReaders) { $roles += 'reader' }
    $roles += 'gateway'
    $identities = @{}
    foreach ($role in $roles) {
        $identityId = $Config["${role}IdentityId"]
        if (-not $identityId.StartsWith("$subscription/", [StringComparison]::OrdinalIgnoreCase)) {
            throw 'App identities must be existing user-assigned identities in the selected subscription.'
        }
        $identity = Invoke-VLiteAz @('identity', 'show', '--ids', $identityId, '--subscription', $Config.SubscriptionId)
        if ($identity.id -ne $identityId -or -not $identity.clientId -or -not $identity.principalId) {
            throw 'Existing app identity could not be verified.'
        }
        $identities[$role] = $identity
    }
    if ($Config.ControlTarget.client_id -ne $identities.gateway.clientId -or
        $Config.ArchiveTarget.client_id -ne $identities.writer.clientId) {
        throw 'Control/archive target client IDs do not match the explicitly assigned identities.'
    }
    foreach ($container in @($Config.ControlTarget.container, $Config.ControlTarget.coordination_container,
        $Config.ArchiveTarget.container, $Config.ArchiveTarget.coordination_container) | Select-Object -Unique) {
        $found = Invoke-VLiteAz @('storage', 'container', 'show', '--account-name', $Config.StorageAccount,
            '--name', $container, '--auth-mode', 'login', '--subscription', $Config.SubscriptionId)
        if ($found.name -ne $container) { throw 'Approved Blob container could not be verified.' }
    }
    $apps = Get-VLiteHaArray @('containerapp', 'list', '--subscription', $Config.SubscriptionId,
        '--resource-group', $Config.ResourceGroup)
    $existing = @{}
    foreach ($role in $roles) {
        $name = $Config["${role}Name"]
        $matches = @($apps | Where-Object { $_.name -eq $name })
        if ($matches.Count -gt 1) { throw 'Duplicate app inventory; refusing reconciliation.' }
        if ($matches.Count) {
            $app = $matches[0]
            Assert-VLiteHaOwned $app $Config $role
            if ($app.properties.managedEnvironmentId -ne $environmentId -or
                ($app.properties.environmentId -and $app.properties.environmentId -ne $environmentId) -or
                $app.identity.userAssignedIdentities -isnot [hashtable] -or
                -not $app.identity.userAssignedIdentities.ContainsKey($Config["${role}IdentityId"])) {
                throw 'Existing HA app environment/identity binding differs; unsafe adoption is refused.'
            }
            if ($role -eq 'writer' -and @($app.properties.template.volumes | Where-Object {
                $_.storageType -ceq 'AzureFile' -and $_.storageName -eq $Config.WriterStorageName
            }).Count -ne 1) { throw 'Existing writer storage volume differs; refusing to move the database.' }
            $existing[$role] = $app
        }
    }
    $backends = @($roles | Where-Object { $_ -ne 'gateway' -and $existing.ContainsKey($_) } |
        ForEach-Object { $Config["${_}Name"] })
    Assert-VLiteHaIsolation $Config $environmentId $backends
    if ($Config.InitializeTenant -and $existing.Count) {
        throw 'InitializeTenant is allowed only for a brand-new deployment, never an existing tenant or restart.'
    }
    # This gate intentionally blocks the current release before local or Azure mutation.
    Assert-VLiteHaQualification $Config

    $private = Get-VLiteHaStateRoot
    foreach ($component in @('', 'azure-ha', $Config.SubscriptionId, $Config.ResourceGroup, $Config.DeploymentId)) {
        if ($component) { $private = Join-Path $private $component }
        Set-VLitePrivateDirectory $private
    }
    $lockPath = Join-Path $private 'deployment.lock'
    if ((Test-Path -LiteralPath $lockPath) -and
        ((Get-Item -LiteralPath $lockPath -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw 'Deployment lock must not be a symbolic link.'
    }
    $lock = [IO.File]::Open($lockPath, 'OpenOrCreate', 'ReadWrite', 'None')
    $manifest = $null
    $secrets = $null
    try {
        $statePath = Join-Path $private 'deployment.json'
        $generationPath = Join-Path $private "generation-g$($Config.SecretGeneration).json"
        $hasState = Test-Path -LiteralPath $statePath
        $retained = @(Get-ChildItem -LiteralPath $private -Force | Where-Object { $_.Name -ne 'deployment.lock' })
        if (-not $hasState -and ($existing.Count -or $retained.Count)) {
            throw 'Original deployment state is missing; refusing regeneration or implicit rotation.'
        }
        if ($hasState) {
            $state = Read-VLiteHaPrivateJson $statePath
            if ($state.Generation -ne $Config.SecretGeneration -or $state.EnvironmentId -ne $environmentId -or
                $state.WriterName -ne $Config.WriterName -or $state.GatewayName -ne $Config.GatewayName -or
                $state.ReaderName -ne $Config.ReaderName -or $state.ShareName -ne $Config.ShareName -or
                $state.StorageAccount -ne $Config.StorageAccount -or
                $state.WriterStorageName -ne $Config.WriterStorageName) {
                throw 'Original deployment binding/generation differs; migration and automatic rotation are unsupported.'
            }
            if ($Config.InitializeTenant) { throw 'Initialization was already attempted; never retry empty-database creation.' }
            if (-not (Test-Path -LiteralPath $generationPath)) {
                throw 'Original generation material is missing; refusing to regenerate or rotate secrets.'
            }
            $secrets = Read-VLiteHaPrivateJson $generationPath
        } else {
            if (-not $Config.InitializeTenant) {
                throw 'A brand-new tenant requires explicit InitializeTenant and create-only authority/empty-scope qualification.'
            }
            foreach ($key in @('SigningKeySourceFile', 'PublicKeyBundleSourceFile')) {
                if (-not $Config[$key] -or -not (Test-Path -LiteralPath $Config[$key] -PathType Leaf)) {
                    throw "Supply original offline gateway-keygen material using $key; no keys are generated implicitly."
                }
            }
            $secrets = @{
                "gw-seed-g$($Config.SecretGeneration)" = [IO.File]::ReadAllText($Config.SigningKeySourceFile)
                "verify-g$($Config.SecretGeneration)" = [IO.File]::ReadAllText($Config.PublicKeyBundleSourceFile)
            }
        }
        $g = "g$($Config.SecretGeneration)"
        $files = @{
            "gateway-config-$g" = New-VLiteHaGatewayConfig -Config $Config
            "control-target-$g" = $Config.ControlTarget
            "archive-target-$g" = $Config.ArchiveTarget
        }
        foreach ($role in @('writer', 'reader') | Where-Object { $_ -eq 'writer' -or $Config.EnableReaders }) {
            $backendConfig = $Config.Clone()
            $backendConfig.Audience = $Config["${role}Audience"]
            $files["$role-trust-$g"] = New-VLiteHaTrustConfig -Config $backendConfig
            $target = $Config.ControlTarget.Clone()
            $target.client_id = $identities[$role].clientId
            $files["$role-control-target-$g"] = $target
            $target = $Config.ArchiveTarget.Clone()
            $target.client_id = $identities[$role].clientId
            $files["$role-archive-target-$g"] = $target
        }
        if ($Config.EnableReaders) {
            $files["reader-config-$g"] = New-VLiteHaReaderApplyConfig -Config $Config
        }
        foreach ($key in $files.Keys) {
            $value = ConvertTo-VLiteHaOrderedValue $files[$key] |
                ConvertTo-Json -Depth 30 -Compress -ErrorAction Stop
            if ($hasState -and $secrets[$key] -cne $value) {
                throw 'Generation-suffixed configuration is immutable; use the reviewed manual rotation procedure.'
            }
            $secrets[$key] = $value
        }
        foreach ($key in $secrets.Keys) {
            if ($key -cnotmatch "^[a-z][a-z0-9-]*-$g$" -or $secrets[$key] -isnot [string] -or
                [string]::IsNullOrWhiteSpace($secrets[$key])) { throw 'Invalid original immutable generation material.' }
        }
        if (-not $secrets["gw-seed-$g"] -or -not $secrets["verify-$g"]) {
            throw 'Original signing seed/public bundle is missing; refusing regeneration.'
        }
        if (-not $hasState) {
            Write-VLiteHaPrivateJson $generationPath $secrets
            Write-VLiteHaPrivateJson $statePath @{
                Generation = $Config.SecretGeneration; EnvironmentId = $environmentId
                WriterName = $Config.WriterName; GatewayName = $Config.GatewayName; ReaderName = $Config.ReaderName
                StorageAccount = $Config.StorageAccount; ShareName = $Config.ShareName
                WriterStorageName = $Config.WriterStorageName
                InitializationAttempted = $true
            }
        }
        $revision = "g$($Config.SecretGeneration)-$([Guid]::NewGuid().ToString('N').Substring(0, 12))"
        $readyBackends = @($backends)
        foreach ($role in $roles) {
            $name = $Config["${role}Name"]
            $roleConfig = $Config.Clone()
            $roleSecrets = @{}
            $roleSecrets["verify-$g"] = $secrets["verify-$g"]
            $roleSecrets["control-target-$g"] = if ($role -eq 'gateway') {
                $secrets["control-target-$g"]
            } else { $secrets["$role-control-target-$g"] }
            switch ($role) {
                'writer' {
                    $roleConfig.Audience = $Config.WriterAudience
                    $roleSecrets["backend-trust-$g"] = $secrets["writer-trust-$g"]
                    $template = New-VLiteHaWriterTemplate -Config $roleConfig -EnvironmentId $environmentId -Secrets $roleSecrets -Revision $revision
                    $minimum = 1
                }
                'reader' {
                    $roleConfig.Audience = $Config.ReaderAudience
                    $roleSecrets["backend-trust-$g"] = $secrets["reader-trust-$g"]
                    $roleSecrets["archive-target-$g"] = $secrets["reader-archive-target-$g"]
                    $roleSecrets["reader-config-$g"] = $secrets["reader-config-$g"]
                    $template = New-VLiteHaReaderTemplate -Config $roleConfig -EnvironmentId $environmentId -Secrets $roleSecrets -Revision $revision
                    $minimum = $Config.ReaderMinReplicas
                }
                'gateway' {
                    $roleSecrets["gw-seed-$g"] = $secrets["gw-seed-$g"]
                    $roleSecrets["gateway-config-$g"] = $secrets["gateway-config-$g"]
                    $template = New-VLiteHaGatewayTemplate -Config $roleConfig -EnvironmentId $environmentId -Secrets $roleSecrets -Revision $revision
                    $minimum = $Config.GatewayMinReplicas
                }
            }
            $template.tags.vliteHaDeployment = $Config.DeploymentId
            $template.tags.vliteHaRole = $role
            $template.tags.vliteHaQualification = 'nonproduction-unqualified-writer-storage'
            Assert-VLiteHaIsolation $Config $environmentId $readyBackends
            $manifest = Join-Path $private "manifest-$([Guid]::NewGuid().ToString('N')).json"
            try {
                Write-VLiteHaPrivateJson $manifest $template
                if ($role -eq 'writer' -and $existing.ContainsKey('writer')) {
                    Stop-VLiteRevisions $Config
                    # A future verifier must perform the post-stop expected-epoch CAS here.
                    Assert-VLiteHaQualification $Config
                }
                $verb = if ($existing.ContainsKey($role)) { 'update' } else { 'create' }
                Invoke-VLiteAz @('containerapp', $verb, '--subscription', $Config.SubscriptionId,
                    '--resource-group', $Config.ResourceGroup, '--name', $name, '--yaml', $manifest) | Out-Null
            } finally {
                if (Test-Path -LiteralPath $manifest) { Remove-Item -LiteralPath $manifest -Force }
                $manifest = $null
            }
            Wait-VLiteHaReady $Config $name "$name--$revision" $minimum
            if ($role -ne 'gateway' -and $name -notin $readyBackends) { $readyBackends += $name }
            Assert-VLiteHaIsolation $Config $environmentId $readyBackends
        }
        Write-Warning 'Nonproduction evaluation only: Azure Files durability and HA availability are not qualified. Routing remains writer-only.'
    } finally {
        if ($manifest -and (Test-Path -LiteralPath $manifest)) { Remove-Item -LiteralPath $manifest -Force }
        $secrets = $null
        $lock.Dispose()
    }
}

if ($MyInvocation.InvocationName -ne '.') {
    $inputs = if ($Config) { $Config.Clone() } else { @{} }
    foreach ($key in $PSBoundParameters.Keys) {
        if ($key -eq 'Config') { continue }
        $value = $PSBoundParameters[$key]
        if ($value -is [Management.Automation.SwitchParameter]) { $value = $value.IsPresent }
        $inputs[$key] = $value
    }
    # The command-line approval cannot be smuggled in through a parameter file.
    $inputs.EnableHa = $EnableHa.IsPresent
    Invoke-VLiteHaDeployment $inputs
}
