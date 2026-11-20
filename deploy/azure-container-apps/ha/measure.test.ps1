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

    Assert (-not ($text.Contains('VLITE_MEAS_KIND='))) 'MEAS-007 must not set the measurement kind'
    $options002 = $options.Clone(); $options002.Measurements = @('MEAS-002')
    $options002.SubscriptionId = 'never-print-this-subscription'
    $planOutput = @(Invoke-VLiteMeas -Options $options002 6>&1)
    $plan002 = @($planOutput | Where-Object { $_ -isnot [System.Management.Automation.InformationRecord] })
    Assert (($plan002.Step -join ',') -ceq 'CheckGroup,CreateGroup,CreateRegistry,BuildImage,CreateIdentity,GrantPull,CreateStorage,CreateArchiveContainer,CreateCoordinationContainer,GrantBlob,CreateEnvironment,CreateJob,StartJob,PollExecution,ReadLogs,QueryLogs,DeleteGroup,WaitDeleted') 'MEAS-002 plan order differs'
    $printed002 = $planOutput | Out-String
    Assert (-not $printed002.Contains($options002.SubscriptionId)) 'MEAS-002 printed plan leaked subscription'
    Assert (-not $printed002.Contains('client-secret-001')) 'MEAS-002 printed plan leaked client ID'
    Assert ($printed002.Contains('<target-b64>')) 'MEAS-002 plan must retain the target placeholder'
    $scriptPlan002 = @(& (Join-Path $PSScriptRoot 'measure.ps1') @options002 6>$null)
    Assert (($scriptPlan002.Step -join ',') -ceq ($plan002.Step -join ',')) 'MEAS-002 script entrypoint Plan differs'
    foreach ($step002 in $plan002) {
        Assert ($step002.Arguments[-2] -ceq '--subscription' -and $step002.Arguments[-1] -ceq '<subscription>') 'MEAS-002 step lacks subscription placeholder'
    }
    $storage = ($plan002 | Where-Object Step -eq CreateStorage).Arguments
    Assert (($storage -join ' ') -ceq 'storage account create --name vlitemeast26st --resource-group vlite-ha-meas-t26 --location westus2 --sku Standard_LRS --kind StorageV2 --allow-shared-key-access false --allow-blob-public-access false --min-tls-version TLS1_2 --https-only true --subscription <subscription>') 'Storage security arguments differ'
    foreach ($container in @(@('CreateArchiveContainer', 'archive'), @('CreateCoordinationContainer', 'coordination'))) {
        $args002 = ($plan002 | Where-Object Step -eq $container[0]).Arguments
        Assert (($args002 -join ' ') -ceq "storage container-rm create --storage-account vlitemeast26st --resource-group vlite-ha-meas-t26 --name $($container[1]) --subscription <subscription>") 'Container must use the control plane'
    }
    $grant002 = ($plan002 | Where-Object Step -eq GrantBlob).Arguments
    Assert (($grant002 -join ' ') -ceq 'role assignment create --assignee-object-id <principal-id> --assignee-principal-type ServicePrincipal --role Storage Blob Data Contributor --scope /subscriptions/<subscription>/resourceGroups/vlite-ha-meas-t26/providers/Microsoft.Storage/storageAccounts/vlitemeast26st --subscription <subscription>') 'Blob role arguments differ'
    $job002 = @($plan002 | Where-Object Step -eq CreateJob)
    Assert ($job002.Count -eq 1 -and $job002[0].Cpu -eq 1 -and $job002[0].Memory -ceq '2Gi') 'MEAS-002 must default to one 1 vCPU / 2Gi tier'
    foreach ($env002 in @(
        'VLITE_MEAS_KIND=MEAS-002', 'VLITE_MEAS002_TARGET_B64=<target-b64>',
        "VLITE_RESOURCE_SOURCE_REVISION=$('a' * 40)", 'VLITE_RESOURCE_SOURCE_DIRTY_PATHS=',
        'VLITE_RESOURCE_HOST_NOTE=MEAS-002 temporary ACA job; requested CPU=1 memory=2Gi'
    )) { Assert ($job002[0].Arguments -ccontains $env002) "Missing MEAS-002 environment: $env002" }
    Assert (@($job002[0].Arguments | Where-Object { $_ -match '^VLITE_MEAS002_(?!TARGET_B64)' }).Count -eq 0) 'Omitted driver options must not be sent'
    $custom002 = $options002.Clone(); $custom002.Tiers = @(0.5, 2)
    $optionFields002 = @(
        @('DurationSecs', 60, 3600, 'DURATION_SECS', '360'),
        @('Rate', 1, 200, 'RATE', '20'),
        @('ArchiveIntervalSecs', 60, 86400, 'ARCHIVE_INTERVAL_SECS', '60'),
        @('StalenessBoundMs', 1000, 3600000, 'STALENESS_BOUND_MS', '300000'),
        @('ReaderPollMs', 100, 60000, 'READER_POLL_MS', '1000'),
        @('ObserveMs', 50, 5000, 'OBSERVE_MS', '100')
    )
    foreach ($field in $optionFields002) { $custom002[$field[0]] = $field[4] }
    $customPlan002 = @(New-VLiteMeasPlan (Test-VLiteMeasParameters $custom002))
    $customJobs002 = @($customPlan002 | Where-Object Step -eq CreateJob)
    Assert (($customJobs002.Memory -join ',') -ceq '1Gi,4Gi') 'MEAS-002 explicit tiers changed'
    foreach ($field in $optionFields002) {
        Assert ($customJobs002[0].Arguments -ccontains "VLITE_MEAS002_$($field[3])=$($field[4])") 'Missing custom MEAS-002 option'
        foreach ($value in @(([long]$field[1] - 1), ([long]$field[2] + 1), '', '+100', "100`n", '1.5', '9223372036854775808')) {
            $bad002 = $options002.Clone(); $bad002[$field[0]] = [string]$value
            Assert-Throws { Test-VLiteMeasParameters $bad002 } "$($field[0]) is outside"
        }
        foreach ($value in @($field[1], ([long]$field[1] + 1), ([long]$field[2] - 1), $field[2])) {
            $edge002 = $options002.Clone(); $edge002[$field[0]] = [string]$value
            if ($field[0] -eq 'StalenessBoundMs') { $edge002.ReaderPollMs = '100' }
            if ($field[0] -in 'DurationSecs', 'ArchiveIntervalSecs' -and $value -ge ([long]$field[2] - 1)) {
                Assert-Throws { Test-VLiteMeasParameters $edge002 } 'budget'
            } else { $null = Test-VLiteMeasParameters $edge002 }
        }
        $bad007 = $options.Clone(); $bad007[$field[0]] = $field[4]
        Assert-Throws { Test-VLiteMeasParameters $bad007 } 'not supported'
    }
    foreach ($name in 'Records', 'Delta', 'Dimensions') {
        $bad002 = $options002.Clone(); $bad002[$name] = '1'
        Assert-Throws { Test-VLiteMeasParameters $bad002 } 'not supported'
    }
    foreach ($entry in @(
        @{ Measurements = @('MEAS-002','MEAS-007') }, @{ Measurements = @('MEAS-007','MEAS-002') },
        @{ Measurements = @('MEAS-001') }, @{ Measurements = @('MEAS-003') }, @{ Measurements = @('MEAS-004') },
        @{ Measurements = @('MEAS-005') }, @{ Measurements = @('MEAS-008') },
        @{ Measurements = @() }, @{ Measurements = @('meas-002') },
        @{ Tiers = @() }, @{ Tiers = @(0.3) }, @{ Tiers = @(1,1) },
        @{ ReaderPollMs = '1000'; StalenessBoundMs = '1000' },
        @{ ReaderPollMs = '1001'; StalenessBoundMs = '1000' },
        @{ DurationSecs = '2521' }, @{ ArchiveIntervalSecs = '601'; DurationSecs = '360' }
    )) {
        $bad002 = $options002.Clone()
        foreach ($key in $entry.Keys) { $bad002[$key] = $entry[$key] }
        Assert-Throws { Test-VLiteMeasParameters $bad002 } '.+'
    }
    $budget002 = $options002.Clone(); $budget002.DurationSecs = '2520'
    $null = Test-VLiteMeasParameters $budget002
    $budget002.DurationSecs = '360'; $budget002.ArchiveIntervalSecs = '600'
    $null = Test-VLiteMeasParameters $budget002
    $target002 = New-VLiteMeas002Target (Test-VLiteMeasParameters $options002) 'client-secret-001'
    $targetJson002 = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($target002))
    $target = $targetJson002 | ConvertFrom-Json -AsHashtable
    Assert ((($target.Keys | Sort-Object) -join ',') -ceq 'account,auth,client_id,container,coordination_container,prefix') 'Target key set differs'
    Assert ($target.account -ceq 'vlitemeast26st' -and $target.container -ceq 'archive' -and
        $target.coordination_container -ceq 'coordination' -and $target.prefix -ceq 'meas002/' -and
        $target.auth -ceq 'managed_identity' -and $target.client_id -ceq 'client-secret-001') 'Target values differ'
    Assert (-not $target.ContainsKey('endpoint') -and -not $targetJson002.Contains("`n")) 'Target must be compact with no endpoint'

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
    Assert ($docker.Contains("COPY benchmarks/ha_reader_resource_tests.rs ./benchmarks/ha_reader_resource_tests.rs`nCOPY benchmarks/ha_meas002.rs ./benchmarks/ha_meas002.rs") -or
        $docker.Contains("COPY benchmarks/ha_reader_resource_tests.rs ./benchmarks/ha_reader_resource_tests.rs`r`nCOPY benchmarks/ha_meas002.rs ./benchmarks/ha_meas002.rs")) 'Missing MEAS-002 COPY in order'
    Assert ($docker.Contains('cargo build --locked --release --features server --bin vlite-server --example ha-meas002')) 'Missing MEAS-002 server/example build'
    Assert ($docker.Contains('case "${VLITE_MEAS_KIND:-MEAS-007}" in')) 'Missing measurement kind switch'
    Assert ($docker.Contains('base64 -d >"$work/target.json"')) 'Missing target decoding'
    Assert ($docker.Contains('*) printf "VLITE-MEAS FAILED 2\n"; exit 2 ;;')) 'Unknown measurement kind must fail'
    Assert (-not ($docker -match '\beval\b')) 'Entrypoint must not use eval'
    foreach ($field in $optionFields002) {
        Assert ($docker.Contains('[ -z "${VLITE_MEAS002_' + $field[3] + ':-}" ] || set -- "$@" --')) 'Missing explicit option forwarding'
    }
    Assert ($docker.Contains('cargo test --locked --release --features server --lib --no-run --message-format=json')) 'Missing real MEAS-006 release lib test build'
    Assert ($docker.Contains('test "$(wc -l < /tmp/meas006-executable)" -eq 1')) 'Artifact selection must refuse missing/ambiguous matches'
    Assert ($docker.Contains('cp "$(cat /tmp/meas006-executable)" /usr/local/bin/vlite-meas006-tests')) 'Missing fixed executable path'
    Assert ($docker.Contains('COPY deploy/azure-container-apps/ha/meas006-collect.sh /usr/local/bin/meas006-collect.sh')) 'Missing existing collector'
    Assert ($docker.Contains('MEAS-006) set -- /bin/sh /usr/local/bin/meas006-collect.sh /usr/local/bin/vlite-meas006-tests ;;')) 'MEAS-006 must use the existing collector and transport'
    Assert ($docker.Contains('if [ "${VLITE_MEAS_KIND:-MEAS-007}" != MEAS-006 ]; then')) 'MEAS-006 must not expose test diagnostics'

    $options006 = $options.Clone(); $options006.Measurements = @('MEAS-006')
    $options006.SubscriptionId = 'never-print-this-subscription'
    $output006 = @(Invoke-VLiteMeas -Options $options006 6>&1)
    $plan006 = @($output006 | Where-Object { $_ -isnot [Management.Automation.InformationRecord] })
    Assert (($plan006.Step -join ',') -ceq 'CheckGroup,CheckRegistryName,CheckStorageName,CreateGroup,CreateRegistry,BuildImage,CreateIdentity,GrantPull,CreateStorage,CreateCoordinationContainer,GrantBlob,CreateEnvironment,CreateJob,StartJob,PollExecution,ReadLogs,QueryLogs,DeleteGroup,WaitDeleted') 'MEAS-006 plan order differs'
    Assert (-not (($output006 | Out-String).Contains($options006.SubscriptionId))) 'MEAS-006 printed plan leaked subscription'
    $script006 = @(& (Join-Path $PSScriptRoot 'measure.ps1') @options006 6>$null)
    Assert (($script006.Step -join ',') -ceq ($plan006.Step -join ',')) 'MEAS-006 CLI entrypoint differs'
    foreach ($step006 in $plan006) {
        Assert ($step006.Arguments[-2] -ceq '--subscription' -and $step006.Arguments[-1] -ceq '<subscription>') 'MEAS-006 subscription placeholder differs'
    }
    $storage006 = ($plan006 | Where-Object Step -eq CreateStorage).Arguments
    Assert (($storage006 -join ' ') -ceq 'storage account create --name vlitemeast26st --resource-group vlite-ha-meas-t26 --location westus2 --sku Standard_ZRS --kind StorageV2 --allow-shared-key-access false --allow-blob-public-access false --min-tls-version TLS1_2 --https-only true --subscription <subscription>') 'MEAS-006 storage security arguments differ'
    $blob006 = ($plan006 | Where-Object Step -eq GrantBlob).Arguments
    Assert (($blob006 -join ' ') -ceq 'role assignment create --assignee-object-id <principal-id> --assignee-principal-type ServicePrincipal --role Storage Blob Data Contributor --scope /subscriptions/<subscription>/resourceGroups/vlite-ha-meas-t26/providers/Microsoft.Storage/storageAccounts/vlitemeast26st/blobServices/default/containers/coordination --subscription <subscription>') 'MEAS-006 grant must be container-scoped'
    $jobs006 = @($plan006 | Where-Object Step -eq CreateJob)
    Assert ($jobs006.Count -eq 1 -and $jobs006[0].Cpu -eq 1 -and $jobs006[0].Memory -ceq '2Gi') 'MEAS-006 must use exactly tier 1'
    foreach ($argument in @('vlite-meas-t26-c1', 'VLITE_MEAS_KIND=MEAS-006', 'VLITE_MEAS006=1',
        'VLITE_MEAS006_ACA_RUN_ID=t26', 'VLITE_MEAS006_CLIENT_ID=<client-id>', 'VLITE_MEAS006_SAMPLES=100',
        'VLITE_MEAS006_REPETITIONS=20', 'VLITE_MEAS006_INTERVAL_MS=50',
        "VLITE_RESOURCE_SOURCE_REVISION=$('a' * 40)", 'VLITE_RESOURCE_SOURCE_DIRTY_PATHS=')) {
        Assert ($jobs006[0].Arguments -ccontains $argument) "Missing MEAS-006 job argument: $argument"
    }
    foreach ($pair in @(@('--trigger-type', 'Manual'), @('--replica-timeout', '3600'), @('--replica-retry-limit', '0'),
        @('--parallelism', '1'), @('--replica-completion-count', '1'), @('--cpu', '1'), @('--memory', '2Gi'))) {
        $index = [Array]::IndexOf($jobs006[0].Arguments, $pair[0])
        Assert ($index -ge 0 -and $jobs006[0].Arguments[$index + 1] -ceq $pair[1]) 'MEAS-006 job CLI shape differs'
    }
    $fields006 = @(@('Samples', 1L, 2000L, 'SAMPLES'), @('Repetitions', 1L, 100L, 'REPETITIONS'), @('IntervalMs', 5L, 5000L, 'INTERVAL_MS'))
    foreach ($field in $fields006) {
        foreach ($value in @('', '+1', "10`n", '1.5', '9223372036854775808', ($field[1] - 1), ($field[2] + 1))) {
            $bad006 = $options006.Clone(); $bad006[$field[0]] = [string]$value
            Assert-Throws { Test-VLiteMeasParameters $bad006 } "$($field[0]) is outside"
        }
        foreach ($value in @($field[1], ($field[1] + 1), ($field[2] - 1), $field[2])) {
            $edge006 = $options006.Clone(); $edge006[$field[0]] = [string]$value
            $edgePlan = @(New-VLiteMeasPlan (Test-VLiteMeasParameters $edge006))
            Assert (($edgePlan | Where-Object Step -eq CreateJob).Arguments -ccontains "VLITE_MEAS006_$($field[3])=$value") 'MEAS-006 bounded value was not forwarded'
        }
        foreach ($legacy in @($options, $options002)) {
            $badLegacy = $legacy.Clone(); $badLegacy[$field[0]] = '10'
            Assert-Throws { Test-VLiteMeasParameters $badLegacy } 'not supported'
        }
    }
    foreach ($entry in @(@{ AllowDirty = $true }, @{ DirtyPaths = ' M src/lib.rs' }, @{ KeepOnFailure = $true },
        @{ Tiers = @(0.5) }, @{ Tiers = @(1, 2) }, @{ Tiers = @() }, @{ Image = 'example.invalid/probe:tag' },
        @{ Records = '10' }, @{ DurationSecs = '60' }, @{ ResourceGroup = 'asendme-rg' },
        @{ Identity = 'foreign' }, @{ StorageAccount = 'vlite-data' }, @{ Measurements = @('MEAS-006', 'MEAS-007') })) {
        $bad006 = $options006.Clone()
        foreach ($key in $entry.Keys) { $bad006[$key] = $entry[$key] }
        Assert-Throws { Test-VLiteMeasParameters $bad006 } '.+'
    }
    $pinned006 = $options006.Clone(); $pinned006.Image = "example.invalid/probe@sha256:$('b' * 64)"
    $null = Test-VLiteMeasParameters $pinned006

    function New-TestDocument006 {
        $metrics = @{}
        foreach ($name in 'verify_before_commit', 'verify_before_ack', 'commit_then_ack') {
            $metrics[$name] = @{ count = 100; unit = 'microseconds'; p50 = 1.0; p95 = 2.0; max = 3.0 }
        }
        $metrics.transfer_to_observed_fenced = @{
            count = 20; unit = 'milliseconds'; p50 = 50.0; p95 = 51.0; max = 52.0
            interval_ms = 50; transfer_clock = 'immediately-before-transfer-CAS'
        }
        return @{
            schema = 'vlite-meas006/1'; measurement = 'MEAS-006'; scope = 'aca-blob'
            source = @{ revision = 'a' * 40; working_tree_dirty = $false; dirty_paths = @() }
            requested = @{ gate_samples = 100; transfer_repetitions = 20; recheck_interval_ms = 50 }
            metrics = $metrics
            limits = @{ claim = 'not an SLO or HA qualification'; gates = 'one writer'; transfer = 'includes CAS'
                not_measured = @('stale_acknowledgment_window', 'clock_skew', 'slo') }
        }
    }
    $config006 = Test-VLiteMeasParameters $options006
    foreach ($mutation in @(
        { param($d) $d.schema = 'unknown' }, { param($d) $d.scope = 'local-fake' },
        { param($d) $d.source.revision = 'b' * 40 }, { param($d) $d.source.working_tree_dirty = $true },
        { param($d) $d.source.dirty_paths = @(' M src/lib.rs') }, { param($d) $d.source = $null },
        { param($d) $d.requested.gate_samples = 101 }, { param($d) $d.requested.gate_samples = '100' },
        { param($d) $d.metrics.Remove('commit_then_ack') },
        { param($d) $d.metrics.unknown = $d.metrics.commit_then_ack },
        { param($d) $d.metrics.commit_then_ack.count = 0 }, { param($d) $d.metrics.commit_then_ack.p50 = $null },
        { param($d) $d.metrics.commit_then_ack.p95 = -1 }, { param($d) $d.metrics.commit_then_ack.max = 0 },
        { param($d) $d.metrics.transfer_to_observed_fenced.interval_ms = 51 },
        { param($d) $d.limits.claim = 'client-secret-001' },
        { param($d) $d.client_id = 'client-secret-001' }
    )) {
        $document006 = New-TestDocument006
        & $mutation $document006
        $bytes006 = [Text.Encoding]::UTF8.GetBytes(($document006 | ConvertTo-Json -Depth 10))
        Assert-Throws { Assert-VLiteMeas006Evidence $bytes006 $config006 @('client-secret-001') } 'MEAS-006 evidence'
    }

    function Start-Sleep { param([int]$Seconds) }
    foreach ($case006 in 'Success', 'ExistingGroup', 'ForeignRegistry', 'ForeignStorage', 'UnknownAvailability',
        'ForeignGroupResponse', 'ForeignIdentity', 'ForeignStorageResponse', 'ForeignJobResponse', 'ForeignExecution',
        'MissingClient', 'BadEvidence', 'BadChecksum', 'CleanupFailure', 'BothFailures', 'WaitFailure', 'Dirty', 'WrongRevision') {
        $script:case006 = $case006
        $script:delete006 = $false
        $script:calls006 = [Collections.Generic.List[object]]::new()
        function git {
            $global:LASTEXITCODE = 0
            if ($args -contains 'rev-parse') {
                if ($script:case006 -eq 'WrongRevision') { return ('b' * 40) }
                return ('a' * 40)
            }
            if ($script:case006 -eq 'Dirty') { return ' M src/lib.rs' }
        }
        function Invoke-VLiteMeasAz {
            param([string[]]$Arguments, [switch]$Raw)
            $script:calls006.Add(@($Arguments))
            $base006 = '/subscriptions/never-print-this-subscription/resourceGroups/vlite-ha-meas-t26'
            switch -Regex ($Arguments -join ' ') {
                '^group exists ' {
                    if ($script:case006 -eq 'WaitFailure' -and $script:delete006) { return $null }
                    return ($script:case006 -eq 'ExistingGroup')
                }
                '^acr check-name ' { return @{ nameAvailable = ($script:case006 -ne 'ForeignRegistry') } }
                '^storage account check-name ' {
                    if ($script:case006 -eq 'UnknownAvailability') { return @{ nameAvailable = 'true' } }
                    return @{ nameAvailable = ($script:case006 -ne 'ForeignStorage') }
                }
                '^group create ' {
                    if ($script:case006 -eq 'ForeignGroupResponse') { return @{ id = '/subscriptions/foreign/resourceGroups/foreign' } }
                    return @{ id = $base006 }
                }
                '^acr create ' { return @{ id = "$base006/providers/Microsoft.ContainerRegistry/registries/vlitemeast26" } }
                '^acr build ' { return }
                '^identity create ' {
                    $id006 = "$base006/providers/Microsoft.ManagedIdentity/userAssignedIdentities/vlite-meas-t26-id"
                    if ($script:case006 -eq 'ForeignIdentity') { $id006 = '/subscriptions/foreign/resourceGroups/foreign' }
                    $client006 = if ($script:case006 -eq 'MissingClient') { '' } else { 'client-secret-001' }
                    return @{ id = $id006; principalId = 'principal-001'; clientId = $client006 }
                }
                '^storage account create ' {
                    if ($script:case006 -eq 'ForeignStorageResponse') { return @{ id = "$base006/providers/Microsoft.Storage/storageAccounts/foreign" } }
                    return @{ id = "$base006/providers/Microsoft.Storage/storageAccounts/vlitemeast26st" }
                }
                '^storage container-rm create ' { return @{ id = "$base006/providers/Microsoft.Storage/storageAccounts/vlitemeast26st/blobServices/default/containers/coordination" } }
                '^role assignment create ' {
                    $i = [Array]::IndexOf($Arguments, '--assignee-object-id')
                    Assert ($Arguments[$i + 1] -ceq 'principal-001') 'MEAS-006 role must use the identity principal'
                    return @{ ok = $true }
                }
                '^containerapp env create ' {
                    return @{ id = "$base006/providers/Microsoft.App/managedEnvironments/vlite-meas-t26-env"
                        properties = @{ appLogsConfiguration = @{ logAnalyticsConfiguration = @{ customerId = 'workspace-001' } } } }
                }
                '^containerapp job create ' {
                    Assert ($Arguments -ccontains 'VLITE_MEAS006_CLIENT_ID=client-secret-001') 'Job must receive the client ID, not principal ID'
                    Assert ($Arguments -ccontains 'vlite-meas-t26-c1') 'Live job name differs'
                    Assert (-not (($Arguments -join ' ') -match '<[^>]+>')) 'Live MEAS-006 job has unresolved placeholders'
                    if ($script:case006 -eq 'ForeignJobResponse') { return @{ id = "$base006/providers/Microsoft.App/jobs/vlite-app" } }
                    return @{ id = "$base006/providers/Microsoft.App/jobs/vlite-meas-t26-c1" }
                }
                '^containerapp job start ' {
                    if ($script:case006 -eq 'ForeignExecution') { return @{ name = 'foreign-execution' } }
                    return @{ name = 'vlite-meas-t26-c1-abc1234' }
                }
                '^containerapp job execution show ' { return @{ properties = @{ status = 'Succeeded' } } }
                '^containerapp job logs show |^monitor log-analytics query ' {
                    $d006 = New-TestDocument006
                    if ($script:case006 -in 'BadEvidence', 'BothFailures') { $d006.limits.claim = 'client-secret-001' }
                    $lines006 = @(New-TestLog ($d006 | ConvertTo-Json -Depth 10 -Compress))
                    if ($script:case006 -eq 'BadChecksum') {
                        $lines006 = @($lines006 | ForEach-Object { $_ -replace '(BEGIN )[a-f0-9]{64}', ('${1}' + ('0' * 64)) })
                    }
                    if ($Raw) { return $lines006 }
                    return ,@($lines006 | ForEach-Object { @{ Log_s = $_ } })
                }
                '^group delete ' {
                    $script:delete006 = $true
                    if ($script:case006 -in 'CleanupFailure', 'BothFailures') { throw 'secret native cleanup diagnostics' }
                    return
                }
                default { throw 'Unexpected MEAS-006 mock invocation' }
            }
        }
        $live006 = $options006.Clone(); $live006.Plan = $false
        $live006.OutDir = Join-Path $scratch "meas006-$case006"
        if ($case006 -eq 'Success') { $captured006 = @(Invoke-VLiteMeas -Options $live006 *>&1) }
        else { Assert-Throws { Invoke-VLiteMeas -Options $live006 *> $null } 'MEAS-006|already exists|Dirty working tree|CLEANUP FAILED|clean checkout HEAD' }
        if ($case006 -in 'Dirty', 'WrongRevision') {
            Assert ($script:calls006.Count -eq 0) 'Invalid MEAS-006 provenance must fail before Azure'
            continue
        }
        if ($case006 -in 'ExistingGroup', 'ForeignRegistry', 'ForeignStorage', 'UnknownAvailability') {
            Assert (-not $script:delete006) 'Preexisting/foreign resources must not be deleted'
            Assert (@($script:calls006 | Where-Object { $_ -contains 'create' }).Count -eq 0) 'Preflight refusal must not mutate resources'
            continue
        }
        Assert $script:delete006 'MEAS-006 must always request deletion'
        $saved006 = Get-Content -Raw (Join-Path $live006.OutDir 'summary.json')
        $summary006 = $saved006 | ConvertFrom-Json
        $expectedFailed006 = switch ($case006) {
            'ForeignGroupResponse' { 'CreateGroup' }
            { $_ -in 'ForeignIdentity', 'MissingClient' } { 'CreateIdentity' }
            'ForeignStorageResponse' { 'CreateStorage' }
            'ForeignJobResponse' { 'CreateJob' }
            'ForeignExecution' { 'StartJob' }
            { $_ -in 'BadEvidence', 'BadChecksum', 'BothFailures' } { 'ReadLogs' }
            default { $null }
        }
        Assert ($summary006.failedStep -ceq $expectedFailed006) 'MEAS-006 measurement failure must be separate'
        Assert ($summary006.requested.Samples -eq 100 -and $summary006.requested.Repetitions -eq 20 -and
            $summary006.requested.IntervalMs -eq 50) 'MEAS-006 summary must record effective options'
        $expectedMeasurement006 = if ($expectedFailed006) { 'Failed' } else { 'Succeeded' }
        Assert ($summary006.measurementStatus -ceq $expectedMeasurement006) 'MEAS-006 cleanup must not overwrite measurement status'
        $expectedCleanup006 = if ($case006 -in 'CleanupFailure', 'BothFailures', 'WaitFailure') { 'Failed' } else { 'Deleted' }
        Assert ($summary006.cleanupStatus -ceq $expectedCleanup006) 'MEAS-006 cleanup status differs'
        $expectedCleanupStep006 = if ($case006 -eq 'WaitFailure') { 'WaitDeleted' } elseif ($expectedCleanup006 -eq 'Failed') { 'DeleteGroup' } else { $null }
        Assert ($summary006.cleanupFailedStep -ceq $expectedCleanupStep006) 'MEAS-006 cleanup failure step differs'
        $evidencePath006 = Join-Path $live006.OutDir 'MEAS-006-c1.json'
        if ($expectedFailed006) { Assert (-not (Test-Path $evidencePath006)) 'Invalid MEAS-006 evidence must not be saved' }
        else {
            $evidence006 = Get-Content -Raw $evidencePath006
            $doc006 = $evidence006 | ConvertFrom-Json
            Assert ($doc006.schema -ceq 'vlite-meas006/1' -and @($doc006.metrics.PSObject.Properties).Count -eq 4) 'MEAS-006 JSON document differs'
        }
        foreach ($private006 in 'client-secret-001', 'principal-001', 'workspace-001', $live006.SubscriptionId) {
            Assert (-not $saved006.Contains($private006)) 'MEAS-006 summary leaked private identifiers'
            Assert (-not (($captured006 | Out-String).Contains($private006))) 'MEAS-006 console leaked private identifiers'
            if (Test-Path $evidencePath006) { Assert (-not $evidence006.Contains($private006)) 'MEAS-006 evidence leaked private identifiers' }
        }
    }

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
    foreach ($mode in 'Console', 'TableFallback', 'RowFallback', 'Retry', 'FailedJob', 'CleanupFailure', 'CleanupWaitFailure', 'Meas002', 'MissingClientId', 'ExistingGroup') {
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
                '^identity create ' {
                    if ($script:mode -eq 'Meas002') { return @{ principalId = 'principal-001'; clientId = 'client-secret-001' } }
                    return @{ principalId = 'principal-001' }
                }
                '^storage (account|container-rm) create ' { return @{ ok = $true } }
                '^role assignment create ' {
                    $assigneeIndex = [Array]::IndexOf($Arguments, '--assignee-object-id')
                    Assert ($assigneeIndex -ge 0 -and $Arguments[$assigneeIndex + 1] -eq 'principal-001') 'AcrPull assignment must use the created identity principal'
                    return @{ ok = $true }
                }
                '^containerapp env create ' {
                    return @{ properties = @{ appLogsConfiguration = @{ logAnalyticsConfiguration = @{ customerId = 'workspace-001' } } } }
                }
                '^containerapp job create ' {
                    if ($script:mode -eq 'Meas002') {
                        $encodedTarget = @($Arguments | Where-Object { $_ -like 'VLITE_MEAS002_TARGET_B64=*' })[0].Split('=', 2)[1]
                        $liveTarget = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($encodedTarget)) | ConvertFrom-Json
                        Assert ($liveTarget.client_id -ceq 'client-secret-001' -and $liveTarget.account -ceq 'vlitemeast26st') 'Live target placeholder was not resolved'
                    }
                    return @{ name = 'job-001' }
                }
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
                '^(group create|acr create|acr build) ' { return @{ ok = $true } }
                default { throw "Unexpected mock command: $($Arguments[0..1] -join ' ')" }
            }
        }
        $run = $options.Clone(); $run.Plan = $false; $run.SubscriptionId = 'never-print-this-subscription'
        $run.Tiers = @(0.5); $run.OutDir = Join-Path $scratch "run-$mode"
        if ($mode -in 'Meas002', 'MissingClientId') {
            $run.Measurements = @('MEAS-002'); $run.Remove('Tiers')
        }
        function git { $global:LASTEXITCODE = 0; return }
        if ($mode -eq 'MissingClientId') {
            Assert-Throws { Invoke-VLiteMeas -Options $run } 'lacks a client ID'
            $missingSummary = Get-Content -Raw (Join-Path $run.OutDir 'summary.json') | ConvertFrom-Json
            Assert ($missingSummary.failedStep -ceq 'CreateIdentity' -and $missingSummary.cleanupStatus -ceq 'Deleted') 'Missing client ID must fail and clean up'
            Assert $script:deleteRequested 'Missing client ID must request deletion'
            continue
        }
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
            if ($mode -eq 'Meas002') { $outputFile = Join-Path $run.OutDir 'MEAS-002-c1.json' }
            Assert ((Get-Content -Raw $outputFile) -ceq '{"sample":true,"text":"resource evidence"}') 'Collected bytes changed'
        }
        $saved = Get-Content -Raw (Join-Path $run.OutDir 'summary.json')
        Assert (-not $saved.Contains($run.SubscriptionId)) 'Summary leaked subscription'
        Assert (-not $saved.Contains('client-secret-001')) 'Summary leaked identity client ID'
        $summary = $saved | ConvertFrom-Json
        $expectedMeasurement = if ($mode -eq 'Meas002') { 'MEAS-002' } else { 'MEAS-007' }
        Assert ($summary.measurement -ceq $expectedMeasurement) 'Summary measurement differs'
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
        Assert (-not ($allArguments -match '<subscription>|<execution>|<principal-id>|<workspace-id>|<target-b64>')) 'Unresolved placeholder in live command'
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
