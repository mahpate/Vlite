#Requires -Version 7.2
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'measure.ps1')

function Assert($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Assert-Throws([scriptblock]$Action, [string]$Pattern) {
    $failure = $null
    try { & $Action | Out-Null } catch { $failure = $_.Exception.Message }
    Assert ($failure -and $failure -match $Pattern) "Expected refusal /$Pattern/; received: $failure"
}

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
$scratch = Join-Path ([IO.Path]::GetTempPath()) "vlite-meas-test-$([guid]::NewGuid().ToString('N'))"
$null = New-Item -ItemType Directory -Path $scratch
$options = @{
    Id = 't26'; SourceRevision = 'a' * 40; OutDir = (Join-Path $scratch 'output')
    Plan = $true
}

function New-TestLog([string]$Json = '{"sample":true,"text":"resource evidence"}') {
    $stream = [IO.MemoryStream]::new()
    $zip = [IO.Compression.GZipStream]::new($stream, [IO.Compression.CompressionLevel]::Optimal, $true)
    $bytes = [Text.Encoding]::UTF8.GetBytes($Json)
    $zip.Write($bytes, 0, $bytes.Length)
    $zip.Dispose()
    $compressed = $stream.ToArray()
    $stream.Dispose()
    $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($compressed)).ToLowerInvariant()
    $base64 = [Convert]::ToBase64String($compressed)
    "2026-11-18T00:00:00Z VLITE-MEAS BEGIN $hash $($compressed.Length)"
    'unrelated platform message'
    $sequence = 0
    for ($i = 0; $i -lt $base64.Length; $i += 76) {
        $sequence++
        "2026-11-18T00:00:01Z VLITE-MEAS $sequence $($base64.Substring($i, [Math]::Min(76, $base64.Length - $i)))"
    }
    "2026-11-18T00:00:02Z VLITE-MEAS END $sequence"
}

try {
    function Invoke-VLiteMeasAz { throw 'Azure must not be called in Plan' }
    function az { throw 'az must not be called in Plan' }
    function git { throw 'git must not be called in Plan with SourceRevision' }
    $planned = @(Invoke-VLiteMeas -Options $options 6>$null)
    Assert (($planned.Step -join ',') -eq 'CheckGroup,CreateGroup,CreateRegistry,BuildImage,CreateIdentity,GrantPull,CreateEnvironment,CreateJob,StartJob,PollExecution,ReadLogs,QueryLogs,CreateJob,StartJob,PollExecution,ReadLogs,QueryLogs,CreateJob,StartJob,PollExecution,ReadLogs,QueryLogs,DeleteGroup,WaitDeleted') 'Plan order differs'
    $text = $planned | ConvertTo-Json -Depth 10
    foreach ($name in 'vlite-ha-meas-t26','vlitemeast26','vlite-meas-t26-env','vlite-meas-t26-id','vlite-meas-t26-c05','vlite-meas-t26-c1','vlite-meas-t26-c2') {
        Assert ($text.Contains($name)) "Missing name $name"
    }
    Assert ($text.Contains('<subscription>')) 'Plan must redact subscription'
    $group = ($planned | Where-Object Step -eq CreateGroup).Arguments
    Assert ($group -contains 'vliteMeasurement=t26') 'Missing owner id tag'
    Assert ($group -contains 'vliteMeasurementOwner=ha-harness') 'Missing owner tag'
    Assert (($group | Where-Object { $_ -like 'vliteMeasurementExpires=*' }) -match 'T.*Z$') 'Missing UTC expiry'
    $expiryTag = $group | Where-Object { $_ -like 'vliteMeasurementExpires=*' }
    $expiry = [DateTimeOffset]::Parse($expiryTag.Split('=', 2)[1])
    Assert (($expiry - [DateTimeOffset]::UtcNow).TotalHours -gt 23.9 -and
        ($expiry - [DateTimeOffset]::UtcNow).TotalHours -le 24) 'Expiry must be one day from now'
    Assert (-not (Test-Path -LiteralPath $options.OutDir)) 'Plan must not create output directories'
    $jobs = @($planned | Where-Object Step -eq CreateJob)
    Assert (($jobs.Cpu -join ',') -eq '0.5,1,2') 'CPU defaults differ'
    Assert (($jobs.Memory -join ',') -eq '1Gi,2Gi,4Gi') 'Memory must be twice CPU'
    foreach ($job in $jobs) {
        Assert ($job.Arguments -contains "VLITE_RESOURCE_SOURCE_REVISION=$('a' * 40)") 'Missing source revision'
        Assert ($job.Arguments -contains 'VLITE_RESOURCE_SOURCE_DIRTY_PATHS=') 'Clean provenance must be empty'
        Assert ($job.Arguments -contains '--registry-identity') 'Missing managed registry identity'
    }
    Assert ($text.Contains('7200') -and $text.Contains('linux/amd64')) 'Missing build constraints'
    $scriptPlan = @(& (Join-Path $PSScriptRoot 'measure.ps1') @options 6>$null)
    Assert (($scriptPlan.Step -join ',') -ceq ($planned.Step -join ',')) 'Script entrypoint Plan differs'
    $redacted = $options.Clone(); $redacted.SubscriptionId = 'never-print-this-subscription'
    $redactedPlan = @(Invoke-VLiteMeas -Options $redacted 6>$null)
    Assert (-not (($redactedPlan | ConvertTo-Json -Depth 10).Contains($redacted.SubscriptionId))) 'Plan leaked the subscription'
    $custom = $options.Clone(); $custom.Tiers = @(0.25, 0.75, 1.25, 1.5, 1.75)
    $custom.Records = '100'; $custom.Delta = '3'; $custom.Dimensions = '8'
    $customPlan = @(New-VLiteMeasPlan (Test-VLiteMeasParameters $custom))
    Assert ((@($customPlan | Where-Object Step -eq CreateJob).Memory -join ',') -eq '0.5Gi,1.5Gi,2.5Gi,3Gi,3.5Gi') 'Fractional tier memory differs'
    Assert (($customPlan | ConvertTo-Json -Depth 10).Contains('VLITE_RESOURCE_RECORDS=100')) 'Missing custom input'
    $imageOptions = $options.Clone(); $imageOptions.Image = 'example.invalid/probe:tag'
    $imagePlan = @(New-VLiteMeasPlan (Test-VLiteMeasParameters $imageOptions))
    Assert ($imagePlan.Step -notcontains 'BuildImage') 'Image must skip build'
    Assert ($imagePlan.Step -contains 'ImportImage') 'Supplied image must be copied into the temporary registry'

    foreach ($name in 'asendme-rg','ASENDME-prod','vlite-app','vlite-env','vlite-data') {
        Assert-Throws { Assert-VLiteMeasName $name } 'protected'
    }
    foreach ($entry in @(
        @{ Id = 'ABc' }, @{ Id = 'aa' }, @{ Id = 'a-b' }, @{ Id = ('a' * 13) },
        @{ Tiers = @(0.3) }, @{ Tiers = @(2.25) }, @{ Tiers = @() }, @{ Tiers = @(1,1) },
        @{ Measurements = @('MEAS-001') }, @{ Measurements = @('MEAS-007','MEAS-004') },
        @{ Records = '0' }, @{ Records = '2000001' }, @{ Delta = '100001' },
        @{ Dimensions = '1025' }, @{ Records = '2000000'; Dimensions = '17' },
        @{ Records = '2'; Delta = '3' }, @{ Records = '+123' }, @{ Records = "123`n" },
        @{ OutDir = $repo }, @{ OutDir = (Join-Path $repo 'inside') },
        @{ Plan = $false }, @{ SourceRevision = ('A' * 40) }, @{ Id = "abc`n" },
        @{ Image = 'https://user:password@example.invalid/image' }
    )) {
        $bad = $options.Clone()
        foreach ($key in $entry.Keys) { $bad[$key] = $entry[$key] }
        Assert-Throws { Test-VLiteMeasParameters $bad } '.+'
    }
    Set-Content -LiteralPath (Join-Path $scratch 'occupied') -Value 'not empty'
    $bad = $options.Clone(); $bad.OutDir = $scratch
    Assert-Throws { Test-VLiteMeasParameters $bad } 'empty'
    $boundary = $options.Clone()
    $boundary.Records = '2000000'; $boundary.Delta = '100000'; $boundary.Dimensions = '16'
    $null = Test-VLiteMeasParameters $boundary
    $boundary.Records = '2'; $boundary.Delta = '1'; $boundary.Dimensions = '1024'
    $null = Test-VLiteMeasParameters $boundary

    $lines = @(New-TestLog)
    $decoded = ConvertFrom-VLiteMeasLog $lines
    Assert ([Text.Encoding]::UTF8.GetString($decoded) -ceq '{"sample":true,"text":"resource evidence"}') 'Log round trip differs'
    Assert-Throws { ConvertFrom-VLiteMeasLog @($lines | Where-Object { $_ -notmatch ' END ' }) } 'END'
    Assert-Throws { ConvertFrom-VLiteMeasLog @($lines | Where-Object { $_ -notmatch ' BEGIN ' }) } 'BEGIN'
    Assert-Throws { ConvertFrom-VLiteMeasLog @($lines | Where-Object { $_ -notmatch 'VLITE-MEAS 1 ' }) } 'gap|count'
    Assert-Throws { ConvertFrom-VLiteMeasLog ($lines + $lines[2]) } 'duplicate'
    Assert-Throws { ConvertFrom-VLiteMeasLog @($lines | ForEach-Object { $_ -replace '(BEGIN )[a-f0-9]{64}', ('${1}' + ('0' * 64)) }) } 'sha256'
    Assert-Throws { ConvertFrom-VLiteMeasLog @($lines | ForEach-Object { $_ -replace '(BEGIN [a-f0-9]{64} )\d+', '${1}1' }) } 'length'
    Assert-Throws { ConvertFrom-VLiteMeasLog ($lines + 'VLITE-MEAS FAILED 1') } 'FAILED'
    Assert-Throws { ConvertFrom-VLiteMeasLog @(New-TestLog '[]') } 'JSON object'
    Assert-Throws { ConvertFrom-VLiteMeasLog @($lines + $lines[0]) } 'BEGIN'

    $source = (Get-Content -Raw (Join-Path $repo 'Dockerfile')) -split '(?m)^FROM source AS test'
    $docker = Get-Content -Raw (Join-Path $PSScriptRoot 'measure.dockerfile')
    foreach ($copy in [regex]::Matches($source[0], '(?m)^COPY .+$')) {
        Assert ($docker.Contains($copy.Value.TrimEnd("`r"))) "Missing source input: $($copy.Value)"
    }
    Assert (([regex]::Matches($docker, '(?m)^FROM ')).Count -eq 1) 'Measurement image must be single stage'
    Assert ($docker.Contains('FROM rust:1.89-bookworm')) 'Wrong toolchain image'
    Assert ($docker.Contains('cargo build --locked --release --features json --example ha-reader-apply')) 'Missing example build'
    Assert ($docker -match '(?m)^USER (?!root)') 'Image must run non-root'

    foreach ($failureCase in @(
        @{ Step = 'CreateRegistry'; Command = 'acr create'; Keep = $false; CleanupFails = $false },
        @{ Step = 'CreateRegistry'; Command = 'acr create'; Keep = $true; CleanupFails = $false },
        @{ Step = 'BuildImage'; Command = 'acr build'; Keep = $false; CleanupFails = $false },
        @{ Step = 'BuildImage'; Command = 'acr build'; Keep = $true; CleanupFails = $false },
        @{ Step = 'BuildImage'; Command = 'acr build'; Keep = $false; CleanupFails = $true }
    )) {
        $keep = $failureCase.Keep
        $script:failureCase = $failureCase
        $script:calls = [Collections.Generic.List[string]]::new()
        function Invoke-VLiteMeasAz {
            param([string[]]$Arguments, [switch]$Raw)
            $script:calls.Add(($Arguments -join ' '))
            $command = $Arguments[0..1] -join ' '
            if ($command -eq $script:failureCase.Command) {
                throw 'injected mid-run failure never-print-this-subscription'
            }
            switch ($command) {
                'group exists' { return $false }
                'group create' { return @{ name = 'temporary' } }
                'acr create' { return @{ name = 'temporary-registry' } }
                'group delete' {
                    if ($script:failureCase.CleanupFails) { throw 'injected cleanup failure' }
                    return
                }
                default { throw 'unexpected Azure stub invocation' }
            }
        }
        $run = $options.Clone(); $run.Plan = $false; $run.SubscriptionId = 'never-print-this-subscription'
        $run.AllowDirty = $true; $run.KeepOnFailure = $keep
        $run.OutDir = Join-Path $scratch "failure-$($failureCase.Step)-$keep-$($failureCase.CleanupFails)"
        function git { $global:LASTEXITCODE = 0; return }
        Assert-Throws { Invoke-VLiteMeas -Options $run } 'injected|CLEANUP FAILED'
        Assert ((@($script:calls | Where-Object { $_ -like 'group delete *' }).Count -eq 1) -eq (-not $keep)) 'Failure cleanup policy differs'
        Assert (Test-Path (Join-Path $run.OutDir 'summary.json')) 'Failure must persist a summary'
        $saved = Get-Content -Raw (Join-Path $run.OutDir 'summary.json')
        $summary = $saved | ConvertFrom-Json
        Assert ($summary.failedStep -eq $failureCase.Step) 'Setup failure must identify its step even after cleanup'
        $cleanupStep = if ($failureCase.CleanupFails) { 'DeleteGroup' } else { $null }
        Assert ($summary.cleanupFailedStep -eq $cleanupStep) 'Cleanup failure must be separate from the setup failure'
        Assert (-not $saved.Contains($run.SubscriptionId)) 'Failure summary leaked native diagnostics'
    }

    $script:sampleLines = @(New-TestLog)
    function Start-Sleep { param([int]$Seconds) }
    foreach ($mode in 'Console', 'TableFallback', 'RowFallback', 'Retry', 'FailedJob', 'CleanupFailure', 'CleanupWaitFailure', 'ExistingGroup') {
        $script:mode = $mode
        $script:startAttempts = 0
        $script:deleteRequested = $false
        $script:recorded = [Collections.Generic.List[object]]::new()
        function Invoke-VLiteMeasAz {
            param([string[]]$Arguments, [switch]$Raw)
            $script:recorded.Add(@($Arguments))
            switch -Regex ($Arguments -join ' ') {
                '^group exists ' {
                    if ($script:mode -eq 'CleanupWaitFailure' -and $script:deleteRequested) { throw 'injected deletion wait failure' }
                    return ($script:mode -eq 'ExistingGroup')
                }
                '^group delete ' {
                    if ($script:mode -eq 'CleanupFailure') { throw 'injected deletion failure' }
                    $script:deleteRequested = $true
                    return
                }
                '^identity create ' { return @{ principalId = 'principal-001' } }
                '^containerapp env create ' {
                    return @{ properties = @{ appLogsConfiguration = @{ logAnalyticsConfiguration = @{ customerId = 'workspace-001' } } } }
                }
                '^containerapp job create ' { return @{ name = 'job-001' } }
                '^containerapp job start ' {
                    $script:startAttempts++
                    if ($script:mode -eq 'Retry' -and $script:startAttempts -eq 1) { throw 'role propagation' }
                    return @{ name = 'execution-001' }
                }
                '^containerapp job execution show ' {
                    $status = if ($script:mode -eq 'FailedJob') { 'Failed' } else { 'Succeeded' }
                    return @{ properties = @{ status = $status } }
                }
                '^containerapp job logs show ' {
                    if ($script:mode -in 'TableFallback', 'RowFallback') { throw 'console unavailable' }
                    return $script:sampleLines
                }
                '^monitor log-analytics query ' {
                    Assert ($Arguments -contains 'workspace-001') 'Workspace placeholder was not resolved'
                    Assert (($Arguments -join ' ') -match 'execution-001') 'Execution placeholder was not resolved'
                    if ($script:mode -eq 'TableFallback') {
                        $rows = [Collections.Generic.List[object]]::new()
                        foreach ($line in $script:sampleLines) { $rows.Add(@($line)) }
                        return @{ tables = @(@{ columns = @(@{ name = 'Log_s' }); rows = $rows.ToArray() }) }
                    }
                    return ,@($script:sampleLines | ForEach-Object { @{ Log_s = $_ } })
                }
                '^(group create|acr create|acr build|role assignment create) ' { return @{ ok = $true } }
                default { throw "Unexpected mock command: $($Arguments[0..1] -join ' ')" }
            }
        }
        $run = $options.Clone(); $run.Plan = $false; $run.SubscriptionId = 'never-print-this-subscription'
        $run.Tiers = @(0.5); $run.OutDir = Join-Path $scratch "run-$mode"
        function git { $global:LASTEXITCODE = 0; return }
        if ($mode -eq 'ExistingGroup') {
            Assert-Throws { Invoke-VLiteMeas -Options $run } 'already exists'
            Assert ($script:recorded.Count -eq 1) 'Existing resource group must never be mutated or deleted'
            continue
        }
        if ($mode -in 'FailedJob', 'CleanupFailure', 'CleanupWaitFailure') {
            Assert-Throws { Invoke-VLiteMeas -Options $run } 'did not succeed|CLEANUP FAILED'
        } else {
            Invoke-VLiteMeas -Options $run
            $outputFile = Join-Path $run.OutDir 'MEAS-007-c05.json'
            Assert ((Get-Content -Raw $outputFile) -ceq '{"sample":true,"text":"resource evidence"}') 'Collected bytes changed'
        }
        $saved = Get-Content -Raw (Join-Path $run.OutDir 'summary.json')
        Assert (-not $saved.Contains($run.SubscriptionId)) 'Summary leaked subscription'
        $summary = $saved | ConvertFrom-Json
        Assert ($summary.results.Count -eq 1) 'Summary result count differs'
        Assert ($summary.results[0].executionName -eq 'execution-001') 'Execution missing from summary'
        $expectedCleanup = if ($mode -in 'CleanupFailure', 'CleanupWaitFailure') { 'Failed' } else { 'Deleted' }
        Assert ($summary.cleanupStatus -eq $expectedCleanup) 'Cleanup status differs'
        $expectedStep = if ($mode -eq 'FailedJob') { 'ReadLogs' } else { $null }
        Assert ($summary.failedStep -eq $expectedStep) 'Measurement failure step differs'
        $expectedCleanupStep = switch ($mode) {
            'CleanupFailure' { 'DeleteGroup' }
            'CleanupWaitFailure' { 'WaitDeleted' }
            default { $null }
        }
        Assert ($summary.cleanupFailedStep -eq $expectedCleanupStep) 'Cleanup failure step differs'
        if ($mode -eq 'Retry') { Assert ($script:startAttempts -eq 2) 'Role propagation did not retry start' }
        $allArguments = $script:recorded | ConvertTo-Json -Depth 10
        Assert (-not ($allArguments -match '<subscription>|<execution>|<principal-id>|<workspace-id>')) 'Unresolved placeholder in live command'
    }

    function git { $global:LASTEXITCODE = 0; return ' M src/example.rs' }
    $run.OutDir = Join-Path $scratch 'dirty-refusal'
    Assert-Throws { Invoke-VLiteMeas -Options $run } 'Dirty working tree'
    $run.AllowDirty = $true; $run.OutDir = Join-Path $scratch 'dirty-allowed'
    $script:mode = 'Console'
    Invoke-VLiteMeas -Options $run
    $dirtySummary = Get-Content -Raw (Join-Path $run.OutDir 'summary.json') | ConvertFrom-Json
    Assert $dirtySummary.dirty 'Dirty provenance was not preserved'
    $dirtyCommands = $script:recorded | ConvertTo-Json -Depth 10
    Assert ($dirtyCommands.Contains('VLITE_RESOURCE_SOURCE_DIRTY_PATHS= M src/example.rs')) 'Porcelain provenance changed'
    'Measurement harness offline tests passed.'
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
