#Requires -Version 7.2
[CmdletBinding()]
param(
    [string]$SubscriptionId,
    [string]$Location = 'westus2',
    [string]$Id = ([guid]::NewGuid().ToString('N').Substring(0, 8)),
    [string[]]$Measurements = @('MEAS-007'),
    [double[]]$Tiers = @(0.5, 1, 2),
    [string]$Records,
    [string]$Delta,
    [string]$Dimensions,
    [string]$DurationSecs,
    [string]$Rate,
    [string]$ArchiveIntervalSecs,
    [string]$StalenessBoundMs,
    [string]$ReaderPollMs,
    [string]$ObserveMs,
    [string]$Samples,
    [string]$Repetitions,
    [string]$IntervalMs,
    [string]$OutDir,
    [string]$Image,
    [string]$SourceRevision,
    [string]$FixtureServer,
    [string]$ControlProgram,
    [string]$GatewayProfile,
    [string]$GatewayPollMs,
    [switch]$AllowDirty,
    [switch]$KeepOnFailure,
    [switch]$Plan
)
$ErrorActionPreference = 'Stop'
$script:VLiteMeasRepository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..\..'))
# Container Apps console logs become queryable in Log Analytics several minutes after emission
# (documented as "several minutes"; reported up to ~10 minutes), so the collector waits ~12 minutes
# of sleeps, plus query time, before it declares evidence unavailable. Unverified against a live run.
$script:VLiteMeasLogAttempts = 72
$script:VLiteMeasLogIntervalSecs = 10
. (Join-Path $PSScriptRoot 'measure-gateway.ps1')

function Assert-VLiteMeasName([string]$Name) {
    if ($Name -imatch '\A(asendme.*|vlite-app|vlite-env|vlite-data)\z') {
        throw 'Refusing a protected resource name.'
    }
    if ($Name -cnotmatch '\A[a-z][a-z0-9-]{2,62}\z') { throw 'Invalid measurement resource name.' }
}

function Test-VLiteMeasParameters([hashtable]$Options) {
    $config = @{
        SubscriptionId = ''; Location = 'westus2'; Id = ([guid]::NewGuid().ToString('N').Substring(0, 8))
        Measurements = @('MEAS-007'); Tiers = @(0.5, 1, 2); Records = ''; Delta = ''; Dimensions = ''
        DurationSecs = ''; Rate = ''; ArchiveIntervalSecs = ''; StalenessBoundMs = ''; ReaderPollMs = ''; ObserveMs = ''
        Samples = ''; Repetitions = ''; IntervalMs = ''
        OutDir = ''; Image = ''; SourceRevision = ''; AllowDirty = $false; KeepOnFailure = $false
        FixtureServer = ''; ControlProgram = ''; GatewayProfile = ''; GatewayPollMs = ''
        Plan = $false; DirtyPaths = ''
    }
    foreach ($key in $Options.Keys) {
        if (-not $config.ContainsKey($key)) { throw "Unknown measurement option: $key" }
        $config[$key] = $Options[$key]
    }
    if ($config.Id -cnotmatch '\A[a-z0-9]{3,12}\z') { throw 'Id must be lowercase alphanumeric, 3-12 characters.' }
    if (-not $config.Plan -and [string]::IsNullOrWhiteSpace($config.SubscriptionId)) {
        throw 'SubscriptionId is required unless -Plan is used.'
    }
    if ([string]::IsNullOrWhiteSpace($config.Location)) { throw 'Location is required.' }
    $config.Measurements = @($config.Measurements)
    if ($config.Measurements.Count -ne 1 -or
        $config.Measurements[0] -cnotin @('MEAS-007', 'MEAS-002', 'MEAS-006', 'MEAS-004', 'MEAS-005')) {
        throw 'Requested measurements are not implemented; select only one of MEAS-007, MEAS-002, MEAS-006, MEAS-004 or MEAS-005.'
    }
    $meas002 = $config.Measurements[0] -ceq 'MEAS-002'
    $meas006 = $config.Measurements[0] -ceq 'MEAS-006'
    $gateway = $config.Measurements[0] -cin @('MEAS-004', 'MEAS-005')
    if (-not $gateway) {
        foreach ($name in 'FixtureServer', 'ControlProgram', 'GatewayProfile', 'GatewayPollMs') {
            if ($Options.ContainsKey($name)) { throw "$name is not supported for the selected measurement." }
        }
    }
    foreach ($field in @(@('Samples', 1L, 2000L, 100L), @('Repetitions', 1L, 100L, 20L), @('IntervalMs', 5L, 5000L, 50L))) {
        $name = $field[0]
        if ($Options.ContainsKey($name)) {
            if (-not $meas006) { throw "$name is not supported for the selected measurement." }
            $number = 0L
            if ([string]$config[$name] -cnotmatch '\A[0-9]+\z' -or
                -not [long]::TryParse([string]$config[$name], [ref]$number) -or
                $number -lt $field[1] -or $number -gt $field[2]) { throw "$name is outside the probe limits." }
            $config[$name] = $number
        } elseif ($meas006) { $config[$name] = $field[3] }
    }
    if ($meas006) {
        if ($config.AllowDirty -or $config.DirtyPaths) { throw 'MEAS-006 requires clean source; AllowDirty is not supported.' }
        if ($config.KeepOnFailure) { throw 'KeepOnFailure is not supported for MEAS-006.' }
        if (-not $Options.ContainsKey('Tiers')) { $config.Tiers = @(1) }
        if (@($config.Tiers).Count -ne 1 -or $config.Tiers[0] -ne 1) { throw 'MEAS-006 requires exactly tier 1.' }
        if ($config.Image -and $config.Image -cnotmatch '@sha256:[a-f0-9]{64}\z') {
            throw 'MEAS-006 Image must be pinned by digest.'
        }
    }
    $stalenessFields = @(
        @('DurationSecs', 60L, 3600L, 360L), @('Rate', 1L, 200L, 20L),
        @('ArchiveIntervalSecs', 60L, 86400L, 60L), @('StalenessBoundMs', 1000L, 3600000L, 300000L),
        @('ReaderPollMs', 100L, 60000L, 1000L), @('ObserveMs', 50L, 5000L, 100L)
    )
    $refusedFields = if ($meas006) { @('Records', 'Delta', 'Dimensions') + @($stalenessFields | ForEach-Object { $_[0] }) }
        elseif ($meas002) { @('Records', 'Delta', 'Dimensions') } else { @($stalenessFields | ForEach-Object { $_[0] }) }
    foreach ($name in $refusedFields) {
        if ($Options.ContainsKey($name)) { throw "$name is not supported for the selected measurement." }
    }
    if ($meas002 -and -not $Options.ContainsKey('Tiers')) { $config.Tiers = @(1) }
    if ($meas002) {
        $effective002 = @{}
        foreach ($field in $stalenessFields) {
            $name = $field[0]
            $number = $field[3]
            if ($Options.ContainsKey($name)) {
                if ([string]$config[$name] -cnotmatch '\A[0-9]+\z' -or
                    -not [long]::TryParse([string]$config[$name], [ref]$number) -or
                    $number -lt $field[1] -or $number -gt $field[2]) { throw "$name is outside the probe limits." }
            }
            $effective002[$name] = $number
        }
        if ($effective002.ReaderPollMs -ge $effective002.StalenessBoundMs) {
            throw 'ReaderPollMs must be below StalenessBoundMs.'
        }
        if (600 + $effective002.DurationSecs + 2 * (2 * $effective002.ArchiveIntervalSecs + 120) -gt 3600) {
            throw 'MEAS-002 time budget exceeds the 3600 second replica timeout.'
        }
    }
    if (@($config.Tiers).Count -eq 0) { throw 'At least one tier is required.' }
    $seen = @{}
    foreach ($tier in $config.Tiers) {
        $cpu = [double]$tier
        if (-not [double]::IsFinite($cpu) -or $cpu -lt 0.25 -or $cpu -gt 2 -or ($cpu * 4) % 1 -ne 0) {
            throw 'Unsupported CPU tier; use 0.25 through 2 in 0.25 steps.'
        }
        $key = $cpu.ToString('0.##', [Globalization.CultureInfo]::InvariantCulture)
        if ($seen.ContainsKey($key)) { throw 'Duplicate CPU tier.' }
        $seen[$key] = $true
    }
    $effective = @{ Records = 110000L; Delta = 300L; Dimensions = 64L }
    foreach ($field in @(@('Records', 2000000L), @('Delta', 100000L), @('Dimensions', 1024L))) {
        $name = $field[0]
        if ([string]$config[$name] -ne '') {
            $number = 0L
            if ([string]$config[$name] -cnotmatch '\A[0-9]+\z' -or
                -not [long]::TryParse([string]$config[$name], [ref]$number) -or
                $number -lt 1 -or $number -gt $field[1]) { throw "$name is outside the probe limits." }
            $effective[$name] = $number
        }
    }
    if ($effective.Records * $effective.Dimensions -gt 32000000) {
        throw 'Records x Dimensions must not exceed 32000000.'
    }
    if (2 * [Math]::Floor($effective.Delta / 3) + 2 -gt $effective.Records) {
        throw 'Delta is too large for Records.'
    }
    if ($config.SourceRevision -and $config.SourceRevision -cnotmatch '\A[0-9a-f]{40}\z') {
        throw 'SourceRevision must be 40 lowercase hexadecimal characters.'
    }
    if ($config.Image -and $config.Image -cnotmatch '\A[a-zA-Z0-9][a-zA-Z0-9._:/-]*(?:@sha256:[a-f0-9]{64})?\z') {
        throw 'Image must be a container image reference, without credentials or URL schemes.'
    }
    if ($config.Image -match '://') { throw 'Image must not be a URL.' }
    if ([string]::IsNullOrWhiteSpace($config.OutDir)) { throw 'OutDir is required.' }
    $config.OutDir = [IO.Path]::GetFullPath($config.OutDir)
    $comparison = if ($IsWindows) { [StringComparison]::OrdinalIgnoreCase } else { [StringComparison]::Ordinal }
    $root = $script:VLiteMeasRepository.TrimEnd([IO.Path]::DirectorySeparatorChar)
    if ($config.OutDir.Equals($root, $comparison) -or
        $config.OutDir.StartsWith("$root$([IO.Path]::DirectorySeparatorChar)", $comparison)) {
        throw 'OutDir must be outside the repository root.'
    }
    $ancestor = $config.OutDir
    while ($ancestor) {
        if (Test-Path -LiteralPath $ancestor) {
            $item = Get-Item -LiteralPath $ancestor -Force
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw 'OutDir must not traverse symbolic links or junctions.'
            }
        }
        $ancestor = [IO.Path]::GetDirectoryName($ancestor)
    }
    if (Test-Path -LiteralPath $config.OutDir) {
        if (-not (Test-Path -LiteralPath $config.OutDir -PathType Container) -or
            @(Get-ChildItem -LiteralPath $config.OutDir -Force).Count) { throw 'OutDir must not exist or must be empty.' }
    }
    $config.ResourceGroup = "vlite-ha-meas-$($config.Id)"
    $config.Environment = "vlite-meas-$($config.Id)-env"
    $config.Registry = "vlitemeas$($config.Id)"
    $config.Identity = "vlite-meas-$($config.Id)-id"
    $config.StorageAccount = "vlitemeas$($config.Id)st"
    if ($config.StorageAccount -cnotmatch '\A[a-z0-9]{3,24}\z') { throw 'Invalid measurement storage account name.' }
    Assert-VLiteMeasName $config.StorageAccount
    foreach ($name in $config.ResourceGroup, $config.Environment, $config.Registry, $config.Identity) {
        Assert-VLiteMeasName $name
    }
    if ($gateway) { Test-VLiteMeasGatewayParameters $config $Options }
    return $config
}

function New-VLiteMeasPlan([hashtable]$Config) {
    $sub = '<subscription>'
    $rg = $Config.ResourceGroup
    $base = "/subscriptions/$sub/resourceGroups/$rg"
    $identity = "$base/providers/Microsoft.ManagedIdentity/userAssignedIdentities/$($Config.Identity)"
    $registry = "$base/providers/Microsoft.ContainerRegistry/registries/$($Config.Registry)"
    $imageName = "$($Config.Registry).azurecr.io/reader-resource:$($Config.SourceRevision)"
    $expires = [DateTime]::UtcNow.AddDays(1).ToString('yyyy-MM-ddTHH:mm:ssZ')
    function Step([string]$Name, [string[]]$Arguments, $Cpu = $null, [string]$Memory = '', [string]$Tier = '') {
        [pscustomobject]@{
            Step = $Name; Arguments = @($Arguments + @('--subscription', $sub))
            Cpu = $Cpu; Memory = $Memory; Tier = $Tier
        }
    }
    Step 'CheckGroup' @('group', 'exists', '--name', $rg)
    if ($Config.Measurements[0] -ceq 'MEAS-006') {
        Step 'CheckRegistryName' @('acr', 'check-name', '--name', $Config.Registry)
        Step 'CheckStorageName' @('storage', 'account', 'check-name', '--name', $Config.StorageAccount)
    }
    Step 'CreateGroup' @('group', 'create', '--name', $rg, '--location', $Config.Location, '--tags',
        "vliteMeasurement=$($Config.Id)", 'vliteMeasurementOwner=ha-harness', "vliteMeasurementExpires=$expires")
    Step 'CreateRegistry' @('acr', 'create', '--name', $Config.Registry, '--resource-group', $rg,
        '--sku', 'Basic', '--admin-enabled', 'false')
    if ($Config.Image) {
        Step 'ImportImage' @('acr', 'import', '--name', $Config.Registry, '--resource-group', $rg,
            '--source', $Config.Image, '--image', "reader-resource:$($Config.SourceRevision)")
    } else {
        Step 'BuildImage' @('acr', 'build', '--registry', $Config.Registry, '--resource-group', $rg,
            '--platform', 'linux/amd64', '--timeout', '7200', '--no-logs',
            '--file', 'deploy/azure-container-apps/ha/measure.dockerfile',
            '--image', "reader-resource:$($Config.SourceRevision)",
            '--build-arg', "VLITE_RESOURCE_SOURCE_REVISION=$($Config.SourceRevision)",
            '--build-arg', "VLITE_RESOURCE_SOURCE_DIRTY_PATHS=$($Config.DirtyPaths)",
            $script:VLiteMeasRepository)
    }
    Step 'CreateIdentity' @('identity', 'create', '--name', $Config.Identity, '--resource-group', $rg)
    Step 'GrantPull' @('role', 'assignment', 'create', '--assignee-object-id', '<principal-id>',
        '--assignee-principal-type', 'ServicePrincipal', '--role', 'AcrPull', '--scope', $registry)
    if ($Config.Measurements[0] -ceq 'MEAS-002') {
        Step 'CreateStorage' @('storage', 'account', 'create', '--name', $Config.StorageAccount,
            '--resource-group', $rg, '--location', $Config.Location, '--sku', 'Standard_LRS', '--kind', 'StorageV2',
            '--allow-shared-key-access', 'false', '--allow-blob-public-access', 'false',
            '--min-tls-version', 'TLS1_2', '--https-only', 'true')
        Step 'CreateArchiveContainer' @('storage', 'container-rm', 'create', '--storage-account', $Config.StorageAccount,
            '--resource-group', $rg, '--name', 'archive')
        Step 'CreateCoordinationContainer' @('storage', 'container-rm', 'create', '--storage-account', $Config.StorageAccount,
            '--resource-group', $rg, '--name', 'coordination')
        Step 'GrantBlob' @('role', 'assignment', 'create', '--assignee-object-id', '<principal-id>',
            '--assignee-principal-type', 'ServicePrincipal', '--role', 'Storage Blob Data Contributor',
            '--scope', "$base/providers/Microsoft.Storage/storageAccounts/$($Config.StorageAccount)")
    }
    if ($Config.Measurements[0] -ceq 'MEAS-006') {
        Step 'CreateStorage' @('storage', 'account', 'create', '--name', $Config.StorageAccount,
            '--resource-group', $rg, '--location', $Config.Location, '--sku', 'Standard_ZRS', '--kind', 'StorageV2',
            '--allow-shared-key-access', 'false', '--allow-blob-public-access', 'false',
            '--min-tls-version', 'TLS1_2', '--https-only', 'true')
        Step 'CreateCoordinationContainer' @('storage', 'container-rm', 'create', '--storage-account', $Config.StorageAccount,
            '--resource-group', $rg, '--name', 'coordination')
        Step 'GrantBlob' @('role', 'assignment', 'create', '--assignee-object-id', '<principal-id>',
            '--assignee-principal-type', 'ServicePrincipal', '--role', 'Storage Blob Data Contributor',
            '--scope', "$base/providers/Microsoft.Storage/storageAccounts/$($Config.StorageAccount)/blobServices/default/containers/coordination")
    }
    Step 'CreateEnvironment' @('containerapp', 'env', 'create', '--name', $Config.Environment,
        '--resource-group', $rg, '--location', $Config.Location, '--enable-workload-profiles', 'true')
    foreach ($tier in $Config.Tiers) {
        $cpu = ([double]$tier).ToString('0.##', [Globalization.CultureInfo]::InvariantCulture)
        $memory = (([double]$tier * 2).ToString('0.##', [Globalization.CultureInfo]::InvariantCulture)) + 'Gi'
        $suffix = $cpu.Replace('.', '')
        $job = "vlite-meas-$($Config.Id)-c$suffix"
        Assert-VLiteMeasName $job
        $envVars = @("VLITE_RESOURCE_SOURCE_REVISION=$($Config.SourceRevision)",
            "VLITE_RESOURCE_SOURCE_DIRTY_PATHS=$($Config.DirtyPaths)",
            "VLITE_RESOURCE_HOST_NOTE=$($Config.Measurements[0]) temporary ACA job; requested CPU=$cpu memory=$memory")
        if ($Config.Measurements[0] -ceq 'MEAS-002') {
            $envVars += @('VLITE_MEAS_KIND=MEAS-002', 'VLITE_MEAS002_TARGET_B64=<target-b64>')
            foreach ($field in @(
                @('DurationSecs', 'DURATION_SECS'), @('Rate', 'RATE'), @('ArchiveIntervalSecs', 'ARCHIVE_INTERVAL_SECS'),
                @('StalenessBoundMs', 'STALENESS_BOUND_MS'), @('ReaderPollMs', 'READER_POLL_MS'), @('ObserveMs', 'OBSERVE_MS')
            )) {
                if ([string]$Config[$field[0]] -ne '') { $envVars += "VLITE_MEAS002_$($field[1])=$($Config[$field[0]])" }
            }
        }
        if ($Config.Measurements[0] -ceq 'MEAS-006') {
            $envVars += @('VLITE_MEAS_KIND=MEAS-006', 'VLITE_MEAS006=1',
                "VLITE_MEAS006_ACA_RUN_ID=$($Config.Id)", 'VLITE_MEAS006_CLIENT_ID=<client-id>',
                "VLITE_MEAS006_SAMPLES=$($Config.Samples)", "VLITE_MEAS006_REPETITIONS=$($Config.Repetitions)",
                "VLITE_MEAS006_INTERVAL_MS=$($Config.IntervalMs)")
        }
        foreach ($field in 'Records', 'Delta', 'Dimensions') {
            if ([string]$Config[$field] -ne '') { $envVars += "VLITE_RESOURCE_$($field.ToUpperInvariant())=$($Config[$field])" }
        }
        Step 'CreateJob' (@('containerapp', 'job', 'create', '--name', $job, '--resource-group', $rg,
            '--environment', $Config.Environment, '--trigger-type', 'Manual',
            '--workload-profile-name', 'Consumption', '--replica-timeout', '3600',
            '--replica-retry-limit', '0', '--parallelism', '1', '--replica-completion-count', '1',
            '--cpu', $cpu, '--memory', $memory, '--container-name', $job, '--image', $imageName,
            '--mi-user-assigned', $identity, '--registry-server', "$($Config.Registry).azurecr.io",
            '--registry-identity', $identity, '--env-vars') + $envVars) ([double]$tier) $memory $suffix
        Step 'StartJob' @('containerapp', 'job', 'start', '--name', $job, '--resource-group', $rg) ([double]$tier) $memory $suffix
        Step 'PollExecution' @('containerapp', 'job', 'execution', 'show', '--name', $job, '--resource-group', $rg,
            '--job-execution-name', '<execution>') ([double]$tier) $memory $suffix
        Step 'ReadLogs' @('containerapp', 'job', 'logs', 'show', '--name', $job, '--resource-group', $rg,
            '--execution', '<execution>', '--container', $job, '--tail', '300', '--format', 'text') ([double]$tier) $memory $suffix
        $query = "ContainerAppConsoleLogs_CL | where tostring(column_ifexists('ContainerGroupName_s', column_ifexists('ContainerGroupName_g', ''))) startswith '<execution>' | order by TimeGenerated asc | project Log_s"
        Step 'QueryLogs' @('monitor', 'log-analytics', 'query', '--workspace', '<workspace-id>',
            '--analytics-query', $query, '--timespan', 'PT2H') ([double]$tier) $memory $suffix
    }
    Step 'DeleteGroup' @('group', 'delete', '--name', $rg, '--yes', '--no-wait')
    Step 'WaitDeleted' @('group', 'exists', '--name', $rg)
}

function New-VLiteMeas002Target([hashtable]$Config, [string]$ClientId) {
    $target = [ordered]@{
        account = $Config.StorageAccount; container = 'archive'; coordination_container = 'coordination'
        prefix = 'meas002/'; auth = 'managed_identity'; client_id = $ClientId
    } | ConvertTo-Json -Compress
    return [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($target))
}

function New-VLiteMeasFailure([string]$Reason, [string]$Message) {
    # Reason is a static code from this script and is safe to persist; the message never is for MEAS-006.
    $failure = [InvalidOperationException]::new($Message)
    $failure.Data['VLiteMeasReason'] = $Reason
    return $failure
}

function ConvertFrom-VLiteMeasLog([string[]]$Lines) {
    $begin = $null; $end = $null
    $chunks = @{}
    foreach ($line in $Lines) {
        if ($line -match 'VLITE-MEAS FAILED(?:\s+([0-9]{1,3}))?(?:\s|$)') {
            $failure = New-VLiteMeasFailure 'FailedMarker' 'Probe emitted a FAILED marker.'
            # Only a bounded numeric process exit code is kept; the rest of the line is discarded.
            if ($Matches[1] -and [int]$Matches[1] -le 255) { $failure.Data['VLiteMeasExitCode'] = [int]$Matches[1] }
            throw $failure
        }
        if ($line -match 'VLITE-MEAS BEGIN ([a-f0-9]{64}) ([0-9]+)\s*$') {
            if ($begin) { throw (New-VLiteMeasFailure 'DuplicateMarker' 'Duplicate BEGIN marker.') }
            $begin = @{ Hash = $Matches[1]; Length = [long]$Matches[2] }
        } elseif ($line -match 'VLITE-MEAS END ([0-9]+)\s*$') {
            if ($null -ne $end) { throw (New-VLiteMeasFailure 'DuplicateMarker' 'Duplicate END marker.') }
            $end = [int]$Matches[1]
        } elseif ($line -match 'VLITE-MEAS ([0-9]+) ([A-Za-z0-9+/=]{1,76})\s*$') {
            $sequence = [int]$Matches[1]
            if ($chunks.ContainsKey($sequence)) { throw (New-VLiteMeasFailure 'DuplicateMarker' 'Duplicate log sequence.') }
            $chunks[$sequence] = $Matches[2]
        }
    }
    if (-not $begin) { throw (New-VLiteMeasFailure 'MissingBegin' 'Missing BEGIN marker.') }
    if ($null -eq $end) { throw (New-VLiteMeasFailure 'MissingEnd' 'Missing END marker.') }
    if ($end -lt 1 -or $chunks.Count -ne $end) { throw (New-VLiteMeasFailure 'ChunkGap' 'Log chunk count mismatch or gap.') }
    $payload = [Text.StringBuilder]::new()
    for ($i = 1; $i -le $end; $i++) {
        if (-not $chunks.ContainsKey($i)) { throw (New-VLiteMeasFailure 'ChunkGap' 'Log sequence gap.') }
        if ($i -lt $end -and $chunks[$i].Length -ne 76) { throw (New-VLiteMeasFailure 'ChunkLength' 'Invalid log chunk length.') }
        $null = $payload.Append($chunks[$i])
    }
    try { $compressed = [Convert]::FromBase64String($payload.ToString()) }
    catch { throw (New-VLiteMeasFailure 'Base64' 'Invalid base64 log payload.') }
    if ($compressed.Length -ne $begin.Length) { throw (New-VLiteMeasFailure 'LengthMismatch' 'Compressed byte length mismatch.') }
    $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($compressed)).ToLowerInvariant()
    if ($hash -cne $begin.Hash) { throw (New-VLiteMeasFailure 'ChecksumMismatch' 'Compressed sha256 mismatch.') }
    $inputStream = [IO.MemoryStream]::new($compressed, $false)
    $outputStream = [IO.MemoryStream]::new()
    $gzip = [IO.Compression.GZipStream]::new($inputStream, [IO.Compression.CompressionMode]::Decompress)
    try {
        $gzip.CopyTo($outputStream)
        $bytes = $outputStream.ToArray()
        $utf8 = [Text.UTF8Encoding]::new($false, $true)
        $document = $utf8.GetString($bytes) | ConvertFrom-Json -AsHashtable -NoEnumerate
        if ($document -isnot [System.Collections.IDictionary]) { throw 'Expected a JSON object.' }
    } catch { throw (New-VLiteMeasFailure 'PayloadDecode' 'Payload must be valid gzip containing a UTF-8 JSON object.') }
    finally { $gzip.Dispose(); $inputStream.Dispose(); $outputStream.Dispose() }
    return ,$bytes
}

function Invoke-VLiteMeasAz {
    param([string[]]$Arguments, [switch]$Raw)
    # Native errors and build logs can contain credentials or subscription identifiers.
    $format = if ($Raw) { @() } else { @('--output', 'json') }
    $output = & az @Arguments --only-show-errors @format 2>$null
    if ($LASTEXITCODE -ne 0) { throw 'Azure CLI measurement command failed; diagnostics withheld.' }
    if ($Raw) { return $output }
    if (-not $output) {
        if (($Arguments[0..1] -join ' ') -in @('group delete', 'acr build', 'acr import')) { return }
        throw 'Azure CLI returned no JSON; refusing an unverifiable result.'
    }
    try { return ,(($output -join "`n") | ConvertFrom-Json -AsHashtable -NoEnumerate) }
    catch { throw 'Azure CLI returned invalid JSON; diagnostics withheld.' }
}

function Wait-VLiteMeasExecution([string[]]$Arguments) {
    $deadline = [DateTime]::UtcNow.AddSeconds(4200)
    do {
        $execution = Invoke-VLiteMeasAz $Arguments
        $status = [string]$execution.properties.status
        if (-not $status) { throw 'Job execution response lacks a status.' }
        if ($status -in @('Succeeded', 'Failed', 'Stopped', 'Canceled', 'Cancelled')) { return $status }
        if ($status -notin @('Running', 'Processing', 'Pending', 'Unknown')) {
            throw 'Unrecognized job execution status.'
        }
        Start-Sleep -Seconds 10
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Timed out waiting for job execution.'
}

function Wait-VLiteMeasDeleted([string[]]$Arguments) {
    $deadline = [DateTime]::UtcNow.AddSeconds(600)
    do {
        $exists = Invoke-VLiteMeasAz $Arguments
        if ($exists -isnot [bool]) { throw 'Group existence check did not return a boolean.' }
        if (-not $exists) { return }
        Start-Sleep -Seconds 5
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Timed out waiting for resource group deletion.'
}

function Get-VLiteMeasEvidenceOutcome($Failure) {
    # Maps a failure to static per-source codes only; exception text is never read.
    $reason = [string]$Failure.Data['VLiteMeasReason']
    $outcome = switch ($reason) {
        'FailedMarker' { 'JobFailed' }
        { $_ -in 'MissingBegin', 'MissingEnd', 'ChunkGap' } { 'Incomplete' }
        { $_ -in 'DuplicateMarker', 'ChunkLength', 'Base64', 'LengthMismatch', 'ChecksumMismatch', 'PayloadDecode' } { 'IntegrityFailure' }
        'ResponseShape' { 'Unavailable' }
        default { $reason = 'RetrievalFailed'; 'Unavailable' }
    }
    return @($outcome, $reason)
}

function Get-VLiteMeasEvidence([string[]]$ConsoleArguments, [string[]]$QueryArguments,
    [Collections.IDictionary]$Diagnostic = [ordered]@{}) {
    $lastFailure = 'Console evidence unavailable.'
    $Diagnostic.consoleEvidence = 'NotQueried'; $Diagnostic.logAnalyticsEvidence = 'NotQueried'
    $Diagnostic.logAnalyticsAttempts = 0
    function Stop-OnFailedMarker($Failure) {
        if ($Failure.Data['VLiteMeasReason'] -ceq 'FailedMarker') {
            $Diagnostic.category = 'JobFailed'; $Diagnostic.reason = 'FailedMarker'
            $Diagnostic.jobExitCode = $Failure.Data['VLiteMeasExitCode']
            return $true
        }
        return $false
    }
    $consoleReason = $null
    try {
        $lines = @(Invoke-VLiteMeasAz $ConsoleArguments -Raw)
        $bytes = ConvertFrom-VLiteMeasLog $lines
        $Diagnostic.consoleEvidence = 'Verified'
        return @{ Bytes = $bytes; Lines = $lines }
    } catch {
        $outcome, $consoleReason = Get-VLiteMeasEvidenceOutcome $_.Exception
        $Diagnostic.consoleEvidence = $outcome
        if (Stop-OnFailedMarker $_.Exception) { throw }
        $lastFailure = $_.Exception.Message
        Write-Warning 'Console evidence unavailable or incomplete; trying Log Analytics.'
    }
    $queryReason = $null
    $attempts = $script:VLiteMeasLogAttempts
    for ($attempt = 0; $attempt -lt $attempts; $attempt++) {
        $Diagnostic.logAnalyticsAttempts = $attempt + 1
        try {
            $result = Invoke-VLiteMeasAz $QueryArguments
            $lines = @(
                if ($result -is [System.Collections.IDictionary] -and $result.Contains('tables')) {
                    foreach ($table in $result.tables) {
                        $column = @($table.columns.name).IndexOf('Log_s')
                        if ($column -lt 0) { throw (New-VLiteMeasFailure 'ResponseShape' 'Log Analytics response lacks Log_s.') }
                        foreach ($row in $table.rows) { [string]$row[$column] }
                    }
                } else {
                    foreach ($row in $result) {
                        if ($null -ne $row.Log_s) { [string]$row.Log_s }
                    }
                }
            )
            $bytes = ConvertFrom-VLiteMeasLog $lines
            $Diagnostic.logAnalyticsEvidence = 'Verified'
            return @{ Bytes = $bytes; Lines = $lines }
        } catch {
            $outcome, $queryReason = Get-VLiteMeasEvidenceOutcome $_.Exception
            $Diagnostic.logAnalyticsEvidence = $outcome
            if (Stop-OnFailedMarker $_.Exception) { throw }
            $lastFailure = $_.Exception.Message
            if ($attempt -lt $attempts - 1) { Start-Sleep -Seconds $script:VLiteMeasLogIntervalSecs }
        }
    }
    # Evidence that arrived but failed transport verification outranks evidence that never arrived.
    if ($Diagnostic.logAnalyticsEvidence -ceq 'IntegrityFailure') {
        $Diagnostic.category = 'EvidenceIntegrity'; $Diagnostic.reason = $queryReason
    } elseif ($Diagnostic.consoleEvidence -ceq 'IntegrityFailure') {
        $Diagnostic.category = 'EvidenceIntegrity'; $Diagnostic.reason = $consoleReason
    } else {
        $Diagnostic.category = 'EvidenceUnavailable'; $Diagnostic.reason = $queryReason
    }
    throw "Unable to collect verified measurement evidence: $lastFailure"
}

function Assert-VLiteMeas006Evidence([byte[]]$Bytes, [hashtable]$Config, [string[]]$PrivateValues) {
    $document = [Text.Encoding]::UTF8.GetString($Bytes) | ConvertFrom-Json -AsHashtable
    function Keys($Object, [string[]]$Expected) {
        if ($Object -isnot [Collections.IDictionary] -or
            (($Object.Keys | Sort-Object) -join ',') -cne (($Expected | Sort-Object) -join ',')) {
            throw (New-VLiteMeasFailure 'Schema' 'MEAS-006 evidence has an invalid schema.')
        }
    }
    Keys $document @('schema', 'measurement', 'scope', 'source', 'requested', 'metrics', 'limits')
    if ($document.schema -cne 'vlite-meas006/1' -or $document.measurement -cne 'MEAS-006' -or
        $document.scope -cne 'aca-blob') { throw (New-VLiteMeasFailure 'Schema' 'MEAS-006 evidence has an invalid schema or scope.') }
    $canonical = $document | ConvertTo-Json -Depth 20 -Compress
    foreach ($value in $PrivateValues) {
        if ($value -and $canonical.Contains($value, [StringComparison]::OrdinalIgnoreCase)) {
            throw (New-VLiteMeasFailure 'PrivateIdentifier' 'MEAS-006 evidence contains private identifiers.')
        }
    }
    Keys $document.source @('revision', 'working_tree_dirty', 'dirty_paths')
    if ($document.source.revision -cne $Config.SourceRevision -or
        $document.source.working_tree_dirty -isnot [bool] -or $document.source.working_tree_dirty -or
        $document.source.dirty_paths -isnot [array] -or $document.source.dirty_paths.Count -ne 0) {
        throw (New-VLiteMeasFailure 'Provenance' 'MEAS-006 evidence provenance differs from the clean pinned source.')
    }
    Keys $document.requested @('gate_samples', 'transfer_repetitions', 'recheck_interval_ms')
    foreach ($field in @(@('gate_samples', 'Samples'), @('transfer_repetitions', 'Repetitions'), @('recheck_interval_ms', 'IntervalMs'))) {
        $value = $document.requested[$field[0]]
        if ($value -isnot [long] -or $value -ne $Config[$field[1]]) { throw (New-VLiteMeasFailure 'RequestedSettings' 'MEAS-006 evidence requested settings differ.') }
    }
    $metrics = @('verify_before_commit', 'verify_before_ack', 'commit_then_ack', 'transfer_to_observed_fenced')
    Keys $document.metrics $metrics
    foreach ($name in $metrics) {
        $metric = $document.metrics[$name]
        $transfer = $name -ceq 'transfer_to_observed_fenced'
        $keys = @('unit', 'count', 'p50', 'p95', 'max')
        if ($transfer) { $keys += @('interval_ms', 'transfer_clock') }
        Keys $metric $keys
        $count = if ($transfer) { $Config.Repetitions } else { $Config.Samples }
        $unit = if ($transfer) { 'milliseconds' } else { 'microseconds' }
        if ($metric.count -isnot [long] -or $metric.count -ne $count -or $metric.unit -cne $unit) {
            throw (New-VLiteMeasFailure 'Metrics' 'MEAS-006 evidence has incomplete metric samples.')
        }
        foreach ($key in 'p50', 'p95', 'max') {
            if (($metric[$key] -isnot [long] -and $metric[$key] -isnot [double]) -or
                -not [double]::IsFinite($metric[$key]) -or $metric[$key] -lt 0) {
                throw (New-VLiteMeasFailure 'Metrics' 'MEAS-006 evidence has invalid metric observations.')
            }
        }
        if ($metric.p50 -gt $metric.p95 -or $metric.p95 -gt $metric.max -or
            ($transfer -and ($metric.interval_ms -isnot [long] -or $metric.interval_ms -ne $Config.IntervalMs -or
                $metric.transfer_clock -cne 'immediately-before-transfer-CAS'))) {
            throw (New-VLiteMeasFailure 'Metrics' 'MEAS-006 evidence has inconsistent metric observations.')
        }
    }
    Keys $document.limits @('claim', 'not_measured', 'gates', 'transfer')
    foreach ($key in 'claim', 'gates', 'transfer') {
        if ($document.limits[$key] -isnot [string] -or [string]::IsNullOrWhiteSpace($document.limits[$key])) {
            throw (New-VLiteMeasFailure 'Limits' 'MEAS-006 evidence lacks measurement limits.')
        }
    }
    if ($document.limits.not_measured -isnot [array] -or
        ($document.limits.not_measured -join ',') -cne 'stale_acknowledgment_window,clock_skew,slo') {
        throw (New-VLiteMeasFailure 'Limits' 'MEAS-006 evidence lacks measurement limits.')
    }
}

function Invoke-VLiteMeas([hashtable]$Options) {
    $config = Test-VLiteMeasParameters $Options
    $meas006 = $config.Measurements[0] -ceq 'MEAS-006'
    if (-not $config.SourceRevision) {
        $revision = & git -C $script:VLiteMeasRepository rev-parse HEAD 2>$null
        if ($LASTEXITCODE -ne 0 -or $revision -cnotmatch '\A[0-9a-f]{40}\z') { throw 'Cannot determine source revision.' }
        $config.SourceRevision = [string]$revision
    }
    if (-not $config.Plan) {
        if ($meas006) {
            $head = & git -C $script:VLiteMeasRepository rev-parse HEAD 2>$null
            if ($LASTEXITCODE -ne 0 -or $head -cnotmatch '\A[0-9a-f]{40}\z' -or $head -cne $config.SourceRevision) {
                throw 'MEAS-006 SourceRevision must match the clean checkout HEAD.'
            }
        }
        $dirty = @(& git -C $script:VLiteMeasRepository status --porcelain --untracked-files=all 2>$null)
        if ($LASTEXITCODE -ne 0) { throw 'Cannot determine source dirty paths.' }
        # The probe expects newline-separated porcelain entries; the clean value is empty.
        $config.DirtyPaths = $dirty -join "`n"
        if ($dirty.Count -gt 0 -and -not $config.AllowDirty) {
            if ($meas006) { throw 'Dirty working tree; MEAS-006 requires clean pinned source.' }
            throw 'Dirty working tree; pass -AllowDirty to record it explicitly.'
        }
    }
    if ($config.Measurements[0] -cin @('MEAS-004', 'MEAS-005')) { return (Invoke-VLiteMeasGateway $config) }
    $steps = @(New-VLiteMeasPlan $config)
    if ($config.Plan) {
        foreach ($step in $steps) {
            Write-Information ("{0}: az {1}" -f $step.Step, (ConvertTo-Json -InputObject $step.Arguments -Compress)) -InformationAction Continue
        }
        return $steps
    }
    function Arguments($Step, [string]$Execution = '', [string]$Workspace = '', [string]$Principal = '') {
        @($Step.Arguments | ForEach-Object {
            $_.Replace('<subscription>', $config.SubscriptionId).Replace('<execution>', $Execution).
                Replace('<workspace-id>', $Workspace).Replace('<principal-id>', $Principal).
                Replace('<target-b64>', $targetB64).Replace('<client-id>', $clientId)
        })
    }
    function Invoke-ResourceStep($Step) {
        $result = Invoke-VLiteMeasAz (Arguments $Step)
        if ($meas006) {
            $base = "/subscriptions/$($config.SubscriptionId)/resourceGroups/$($config.ResourceGroup)"
            $expected = switch ($Step.Step) {
                'CreateGroup' { $base }
                'CreateRegistry' { "$base/providers/Microsoft.ContainerRegistry/registries/$($config.Registry)" }
                'CreateIdentity' { "$base/providers/Microsoft.ManagedIdentity/userAssignedIdentities/$($config.Identity)" }
                'CreateStorage' { "$base/providers/Microsoft.Storage/storageAccounts/$($config.StorageAccount)" }
                'CreateCoordinationContainer' { "$base/providers/Microsoft.Storage/storageAccounts/$($config.StorageAccount)/blobServices/default/containers/coordination" }
                'CreateEnvironment' { "$base/providers/Microsoft.App/managedEnvironments/$($config.Environment)" }
                'CreateJob' { "$base/providers/Microsoft.App/jobs/vlite-meas-$($config.Id)-c1" }
            }
            if ($expected -and ($result -isnot [Collections.IDictionary] -or $result.id -ine $expected)) {
                throw 'MEAS-006 resource response is missing its expected ID or references a foreign resource.'
            }
        }
        return $result
    }
    $targetB64 = ''
    $clientId = ''
    $exists = Invoke-VLiteMeasAz (Arguments $steps[0])
    if ($exists -isnot [bool]) { throw 'Group existence check did not return a boolean.' }
    if ($exists) { throw 'Measurement resource group already exists; refusing to reuse or delete it.' }
    if ($meas006) {
        foreach ($check in $steps | Where-Object { $_.Step -in 'CheckRegistryName', 'CheckStorageName' }) {
            $availability = Invoke-VLiteMeasAz (Arguments $check)
            if ($availability.nameAvailable -isnot [bool] -or -not $availability.nameAvailable) {
                throw 'MEAS-006 resource name is unavailable; refusing preexisting or foreign resources.'
            }
        }
    }
    $null = [IO.Directory]::CreateDirectory($config.OutDir)
    $summary = [ordered]@{
        measurement = $config.Measurements[0]; sourceRevision = $config.SourceRevision
        dirty = [bool]$config.DirtyPaths; resourceGroup = $config.ResourceGroup
        status = 'Running'; cleanupStatus = 'Pending'; results = [Collections.Generic.List[object]]::new()
        failedStep = $null; cleanupFailedStep = $null
    }
    if ($meas006) {
        $summary.measurementStatus = 'Running'
        $summary.requested = [ordered]@{ Samples = $config.Samples; Repetitions = $config.Repetitions; IntervalMs = $config.IntervalMs }
        $summary.failureDiagnostic = $null
    }
    $failed = $true; $principal = ''; $workspace = ''; $execution = ''; $current = $null; $readDiagnostic = $null
    try {
        foreach ($step in $steps | Select-Object -Skip 1) {
            switch ($step.Step) {
                'DeleteGroup' { continue }
                'WaitDeleted' { continue }
                'QueryLogs' { continue }
                'CheckRegistryName' { continue }
                'CheckStorageName' { continue }
                'CreateIdentity' {
                    $identity = Invoke-ResourceStep $step
                    $principal = [string]$identity.principalId
                    if (-not $principal) { throw 'Created identity lacks a principal ID.' }
                    if ($config.Measurements[0] -ceq 'MEAS-002' -or $meas006) {
                        $clientId = [string]$identity.clientId
                        if (-not $clientId) { throw 'Created identity lacks a client ID.' }
                        if (-not $meas006) { $targetB64 = New-VLiteMeas002Target $config $clientId }
                    }
                }
                'GrantPull' { $null = Invoke-VLiteMeasAz (Arguments $step -Principal $principal) }
                'GrantBlob' { $null = Invoke-VLiteMeasAz (Arguments $step -Principal $principal) }
                'CreateEnvironment' {
                    $environment = Invoke-ResourceStep $step
                    $workspace = [string]$environment.properties.appLogsConfiguration.logAnalyticsConfiguration.customerId
                    if (-not $workspace) { throw 'Created environment lacks its Log Analytics workspace ID.' }
                }
                'CreateJob' {
                    $current = [ordered]@{
                        tier = "c$($step.Tier)"; cpu = $step.Cpu; memory = $step.Memory
                        executionName = $null; status = 'Creating'; sha256 = $null; fileName = $null
                        sourceRevision = $config.SourceRevision; dirty = [bool]$config.DirtyPaths
                    }
                    if ($meas006) { $current.executionStatus = $null }
                    $summary.results.Add($current)
                    $null = Invoke-ResourceStep $step
                }
                'StartJob' {
                    for ($attempt = 0; $attempt -lt 6; $attempt++) {
                        try {
                            $started = Invoke-VLiteMeasAz (Arguments $step)
                            $execution = [string]$started.name
                            if (-not $execution) { throw 'Job start did not return an execution name.' }
                            if ($meas006 -and $execution -cnotmatch "\Avlite-meas-$($config.Id)-c1-[a-z0-9]+\z") {
                                throw 'MEAS-006 job start returned a foreign or invalid execution name.'
                            }
                            break
                        } catch {
                            if ($attempt -eq 5) { throw }
                            Write-Warning 'Job start failed; retrying while the AcrPull role propagates.'
                            Start-Sleep -Seconds ([Math]::Min(10 * [Math]::Pow(2, $attempt), 60))
                        }
                    }
                    $current.executionName = $execution
                    $current.status = 'Running'
                }
                'PollExecution' {
                    $current.status = Wait-VLiteMeasExecution (Arguments $step $execution)
                    if ($meas006) { $current.executionStatus = $current.status }
                }
                'ReadLogs' {
                    $query = $steps | Where-Object { $_.Step -eq 'QueryLogs' -and $_.Tier -eq $step.Tier }
                    # Static codes only; raw logs, native errors and exception text are never recorded.
                    $readDiagnostic = [ordered]@{ category = $null; reason = $null; jobExitCode = $null
                        consoleEvidence = 'NotQueried'; logAnalyticsEvidence = 'NotQueried'; logAnalyticsAttempts = 0 }
                    $evidence = Get-VLiteMeasEvidence (Arguments $step $execution) (Arguments $query $execution $workspace) $readDiagnostic
                    $begin = $evidence.Lines | Where-Object {
                        $_ -match 'VLITE-MEAS BEGIN ([a-f0-9]{64}) ([0-9]+)\s*$'
                    } | Select-Object -First 1
                    $null = $begin -match 'VLITE-MEAS BEGIN ([a-f0-9]{64})'
                    $current.sha256 = $Matches[1]
                    if ($current.status -ne 'Succeeded') {
                        $readDiagnostic.category = 'JobNotSucceeded'; $readDiagnostic.reason = 'ExecutionNotSucceeded'
                        throw 'Measurement job did not succeed.'
                    }
                    if ($meas006) {
                        try { Assert-VLiteMeas006Evidence $evidence.Bytes $config @($clientId, $principal, $workspace, $config.SubscriptionId) }
                        catch {
                            $readDiagnostic.category = 'EvidenceRejected'
                            $readDiagnostic.reason = if ($_.Exception.Data['VLiteMeasReason']) { [string]$_.Exception.Data['VLiteMeasReason'] } else { 'Unclassified' }
                            throw
                        }
                    }
                    $current.fileName = "$($config.Measurements[0])-c$($step.Tier).json"
                    [IO.File]::WriteAllBytes((Join-Path $config.OutDir $current.fileName), $evidence.Bytes)
                    $summary | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $config.OutDir 'summary.json') -Encoding utf8
                }
                default { $null = Invoke-ResourceStep $step }
            }
        }
        $summary.status = 'Succeeded'
        if ($meas006) { $summary.measurementStatus = 'Succeeded' }
        $failed = $false
    } catch {
        $summary.status = 'Failed'
        $summary.failedStep = $step.Step
        if ($meas006) { $summary.measurementStatus = 'Failed' }
        if ($null -ne $current -and $current.status -notin @('Failed', 'Stopped', 'Canceled', 'Cancelled')) { $current.status = 'Failed' }
        if ($meas006 -and $step.Step -eq 'ReadLogs' -and $null -ne $readDiagnostic) {
            if (-not $readDiagnostic.category) { $readDiagnostic.category = 'Unclassified' }
            $summary.failureDiagnostic = $readDiagnostic
        }
        if ($meas006) { throw "MEAS-006 failed at $($step.Step); diagnostics withheld." }
        throw
    } finally {
        try {
            if ($failed -and $config.KeepOnFailure) {
                $summary.cleanupStatus = 'RetainedOnFailure'
                Write-Warning "Measurement failed; retained tagged resource group $($config.ResourceGroup). Delete it manually."
            } else {
                try {
                    $delete = $steps | Where-Object Step -eq DeleteGroup
                    $wait = $steps | Where-Object Step -eq WaitDeleted
                    $cleanupStep = 'DeleteGroup'
                    $null = Invoke-VLiteMeasAz (Arguments $delete)
                    $cleanupStep = 'WaitDeleted'
                    Wait-VLiteMeasDeleted (Arguments $wait)
                    $summary.cleanupStatus = 'Deleted'
                } catch {
                    $summary.status = 'Failed'
                    $summary.cleanupStatus = 'Failed'
                    $summary.cleanupFailedStep = $cleanupStep
                    throw "MEASUREMENT CLEANUP FAILED: delete tagged resource group $($config.ResourceGroup) manually."
                }
            }
        } finally {
            $summary | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $config.OutDir 'summary.json') -Encoding utf8
        }
    }
}

if ($MyInvocation.InvocationName -ne '.') { Invoke-VLiteMeas -Options $PSBoundParameters }
