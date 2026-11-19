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
    [string]$OutDir,
    [string]$Image,
    [string]$SourceRevision,
    [switch]$AllowDirty,
    [switch]$KeepOnFailure,
    [switch]$Plan
)
$ErrorActionPreference = 'Stop'
$script:VLiteMeasRepository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..\..'))

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
        OutDir = ''; Image = ''; SourceRevision = ''; AllowDirty = $false; KeepOnFailure = $false
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
    if ($config.Measurements.Count -ne 1 -or $config.Measurements[0] -cne 'MEAS-007') {
        throw 'Requested measurements are not implemented; only MEAS-007 is implemented.'
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
    foreach ($name in $config.ResourceGroup, $config.Environment, $config.Registry, $config.Identity) {
        Assert-VLiteMeasName $name
    }
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
            "VLITE_RESOURCE_HOST_NOTE=MEAS-007 temporary ACA job; requested CPU=$cpu memory=$memory")
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

function ConvertFrom-VLiteMeasLog([string[]]$Lines) {
    $begin = $null; $end = $null
    $chunks = @{}
    foreach ($line in $Lines) {
        if ($line -match 'VLITE-MEAS FAILED(?:\s|$)') { throw 'Probe emitted a FAILED marker.' }
        if ($line -match 'VLITE-MEAS BEGIN ([a-f0-9]{64}) ([0-9]+)\s*$') {
            if ($begin) { throw 'Duplicate BEGIN marker.' }
            $begin = @{ Hash = $Matches[1]; Length = [long]$Matches[2] }
        } elseif ($line -match 'VLITE-MEAS END ([0-9]+)\s*$') {
            if ($null -ne $end) { throw 'Duplicate END marker.' }
            $end = [int]$Matches[1]
        } elseif ($line -match 'VLITE-MEAS ([0-9]+) ([A-Za-z0-9+/=]{1,76})\s*$') {
            $sequence = [int]$Matches[1]
            if ($chunks.ContainsKey($sequence)) { throw 'Duplicate log sequence.' }
            $chunks[$sequence] = $Matches[2]
        }
    }
    if (-not $begin) { throw 'Missing BEGIN marker.' }
    if ($null -eq $end) { throw 'Missing END marker.' }
    if ($end -lt 1 -or $chunks.Count -ne $end) { throw 'Log chunk count mismatch or gap.' }
    $payload = [Text.StringBuilder]::new()
    for ($i = 1; $i -le $end; $i++) {
        if (-not $chunks.ContainsKey($i)) { throw 'Log sequence gap.' }
        if ($i -lt $end -and $chunks[$i].Length -ne 76) { throw 'Invalid log chunk length.' }
        $null = $payload.Append($chunks[$i])
    }
    try { $compressed = [Convert]::FromBase64String($payload.ToString()) }
    catch { throw 'Invalid base64 log payload.' }
    if ($compressed.Length -ne $begin.Length) { throw 'Compressed byte length mismatch.' }
    $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($compressed)).ToLowerInvariant()
    if ($hash -cne $begin.Hash) { throw 'Compressed sha256 mismatch.' }
    $inputStream = [IO.MemoryStream]::new($compressed, $false)
    $outputStream = [IO.MemoryStream]::new()
    $gzip = [IO.Compression.GZipStream]::new($inputStream, [IO.Compression.CompressionMode]::Decompress)
    try {
        $gzip.CopyTo($outputStream)
        $bytes = $outputStream.ToArray()
        $utf8 = [Text.UTF8Encoding]::new($false, $true)
        $document = $utf8.GetString($bytes) | ConvertFrom-Json -AsHashtable -NoEnumerate
        if ($document -isnot [System.Collections.IDictionary]) { throw 'Expected a JSON object.' }
    } catch { throw 'Payload must be valid gzip containing a UTF-8 JSON object.' }
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

function Get-VLiteMeasEvidence([string[]]$ConsoleArguments, [string[]]$QueryArguments) {
    $lastFailure = 'Console evidence unavailable.'
    try {
        $lines = @(Invoke-VLiteMeasAz $ConsoleArguments -Raw)
        $bytes = ConvertFrom-VLiteMeasLog $lines
        return @{ Bytes = $bytes; Lines = $lines }
    } catch {
        if ($_.Exception.Message -match 'FAILED marker') { throw }
        $lastFailure = $_.Exception.Message
        Write-Warning 'Console evidence unavailable or incomplete; trying Log Analytics.'
    }
    for ($attempt = 0; $attempt -lt 12; $attempt++) {
        try {
            $result = Invoke-VLiteMeasAz $QueryArguments
            $lines = @(
                if ($result -is [System.Collections.IDictionary] -and $result.Contains('tables')) {
                    foreach ($table in $result.tables) {
                        $column = @($table.columns.name).IndexOf('Log_s')
                        if ($column -lt 0) { throw 'Log Analytics response lacks Log_s.' }
                        foreach ($row in $table.rows) { [string]$row[$column] }
                    }
                } else {
                    foreach ($row in $result) {
                        if ($null -ne $row.Log_s) { [string]$row.Log_s }
                    }
                }
            )
            $bytes = ConvertFrom-VLiteMeasLog $lines
            return @{ Bytes = $bytes; Lines = $lines }
        } catch {
            if ($_.Exception.Message -match 'FAILED marker') { throw }
            $lastFailure = $_.Exception.Message
            if ($attempt -lt 11) { Start-Sleep -Seconds 10 }
        }
    }
    throw "Unable to collect verified measurement evidence: $lastFailure"
}

function Invoke-VLiteMeas([hashtable]$Options) {
    $config = Test-VLiteMeasParameters $Options
    if (-not $config.SourceRevision) {
        $revision = & git -C $script:VLiteMeasRepository rev-parse HEAD 2>$null
        if ($LASTEXITCODE -ne 0 -or $revision -cnotmatch '\A[0-9a-f]{40}\z') { throw 'Cannot determine source revision.' }
        $config.SourceRevision = [string]$revision
    }
    if (-not $config.Plan) {
        $dirty = @(& git -C $script:VLiteMeasRepository status --porcelain --untracked-files=all 2>$null)
        if ($LASTEXITCODE -ne 0) { throw 'Cannot determine source dirty paths.' }
        # The probe expects newline-separated porcelain entries; the clean value is empty.
        $config.DirtyPaths = $dirty -join "`n"
        if ($dirty.Count -gt 0 -and -not $config.AllowDirty) { throw 'Dirty working tree; pass -AllowDirty to record it explicitly.' }
    }
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
                Replace('<workspace-id>', $Workspace).Replace('<principal-id>', $Principal)
        })
    }
    $exists = Invoke-VLiteMeasAz (Arguments $steps[0])
    if ($exists -isnot [bool]) { throw 'Group existence check did not return a boolean.' }
    if ($exists) { throw 'Measurement resource group already exists; refusing to reuse or delete it.' }
    $null = [IO.Directory]::CreateDirectory($config.OutDir)
    $summary = [ordered]@{
        measurement = 'MEAS-007'; sourceRevision = $config.SourceRevision
        dirty = [bool]$config.DirtyPaths; resourceGroup = $config.ResourceGroup
        status = 'Running'; cleanupStatus = 'Pending'; results = [Collections.Generic.List[object]]::new()
    }
    $failed = $true; $principal = ''; $workspace = ''; $execution = ''; $current = $null
    try {
        foreach ($step in $steps | Select-Object -Skip 1) {
            switch ($step.Step) {
                'DeleteGroup' { continue }
                'WaitDeleted' { continue }
                'QueryLogs' { continue }
                'CreateIdentity' {
                    $identity = Invoke-VLiteMeasAz (Arguments $step)
                    $principal = [string]$identity.principalId
                    if (-not $principal) { throw 'Created identity lacks a principal ID.' }
                }
                'CreateEnvironment' {
                    $environment = Invoke-VLiteMeasAz (Arguments $step)
                    $workspace = [string]$environment.properties.appLogsConfiguration.logAnalyticsConfiguration.customerId
                    if (-not $workspace) { throw 'Created environment lacks its Log Analytics workspace ID.' }
                }
                'CreateJob' {
                    $current = [ordered]@{
                        tier = "c$($step.Tier)"; cpu = $step.Cpu; memory = $step.Memory
                        executionName = $null; status = 'Creating'; sha256 = $null; fileName = $null
                        sourceRevision = $config.SourceRevision; dirty = [bool]$config.DirtyPaths
                    }
                    $summary.results.Add($current)
                    $null = Invoke-VLiteMeasAz (Arguments $step)
                }
                'StartJob' {
                    for ($attempt = 0; $attempt -lt 6; $attempt++) {
                        try {
                            $started = Invoke-VLiteMeasAz (Arguments $step)
                            $execution = [string]$started.name
                            if (-not $execution) { throw 'Job start did not return an execution name.' }
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
                'PollExecution' { $current.status = Wait-VLiteMeasExecution (Arguments $step $execution) }
                'ReadLogs' {
                    $query = $steps | Where-Object { $_.Step -eq 'QueryLogs' -and $_.Tier -eq $step.Tier }
                    $evidence = Get-VLiteMeasEvidence (Arguments $step $execution) (Arguments $query $execution $workspace)
                    $begin = $evidence.Lines | Where-Object {
                        $_ -match 'VLITE-MEAS BEGIN ([a-f0-9]{64}) ([0-9]+)\s*$'
                    } | Select-Object -First 1
                    $null = $begin -match 'VLITE-MEAS BEGIN ([a-f0-9]{64})'
                    $current.sha256 = $Matches[1]
                    if ($current.status -ne 'Succeeded') { throw 'Measurement job did not succeed.' }
                    $current.fileName = "MEAS-007-c$($step.Tier).json"
                    [IO.File]::WriteAllBytes((Join-Path $config.OutDir $current.fileName), $evidence.Bytes)
                    $summary | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $config.OutDir 'summary.json') -Encoding utf8
                }
                default { $null = Invoke-VLiteMeasAz (Arguments $step) }
            }
        }
        $summary.status = 'Succeeded'
        $failed = $false
    } catch {
        $summary.status = 'Failed'
        if ($null -ne $current -and $current.status -notin @('Failed', 'Stopped', 'Canceled', 'Cancelled')) { $current.status = 'Failed' }
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
                    $null = Invoke-VLiteMeasAz (Arguments $delete)
                    Wait-VLiteMeasDeleted (Arguments $wait)
                    $summary.cleanupStatus = 'Deleted'
                } catch {
                    $summary.status = 'Failed'
                    $summary.cleanupStatus = 'Failed'
                    throw "MEASUREMENT CLEANUP FAILED: delete tagged resource group $($config.ResourceGroup) manually."
                }
            }
        } finally {
            $summary | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $config.OutDir 'summary.json') -Encoding utf8
        }
    }
}

if ($MyInvocation.InvocationName -ne '.') { Invoke-VLiteMeas -Options $PSBoundParameters }
