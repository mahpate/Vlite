#Requires -Version 7.2
# Offline MEAS-004/005 orchestration tests. Every Azure CLI call, local fixture/analysis process,
# clock and sleep is mocked; nothing here contacts Azure, ACR, Docker or a real /measurement root.
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'measure.ps1')

function Assert($Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Assert-Throws([scriptblock]$Action, [string]$Pattern) {
    $failure = $null
    try { & $Action | Out-Null } catch { $failure = $_.Exception.Message }
    Assert ($failure -and $failure -match $Pattern) "Expected refusal /$Pattern/; received: $failure"
}

$scratch = Join-Path ([IO.Path]::GetTempPath()) "vlite-meas-gw-test-$([guid]::NewGuid().ToString('N'))"
$null = New-Item -ItemType Directory -Path $scratch
$revision = 'a' * 40
$subscription = '00000000-1111-2222-3333-444444444444'
$base = "/subscriptions/$subscription/resourceGroups/vlite-ha-meas-t26"

function ConvertTo-Gzip([byte[]]$Bytes) {
    $stream = [IO.MemoryStream]::new()
    $zip = [IO.Compression.GZipStream]::new($stream, [IO.Compression.CompressionLevel]::Optimal, $true)
    $zip.Write($Bytes, 0, $Bytes.Length); $zip.Dispose()
    $result = $stream.ToArray(); $stream.Dispose()
    return ,$result
}
function Get-Sha([byte[]]$Bytes) { [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($Bytes)).ToLowerInvariant() }
function New-Frame([string]$Json, [string]$Tag = 'VLITE-MEAS') {
    $compressed = ConvertTo-Gzip ([Text.Encoding]::UTF8.GetBytes($Json))
    $base64 = [Convert]::ToBase64String($compressed)
    "$Tag BEGIN $(Get-Sha $compressed) $($compressed.Length)"
    $sequence = 0
    for ($i = 0; $i -lt $base64.Length; $i += 76) {
        $sequence++
        "$Tag $sequence $($base64.Substring($i, [Math]::Min(76, $base64.Length - $i)))"
    }
    "$Tag END $sequence"
}
function Source { [ordered]@{ revision = $script:SourceRevision; dirty_paths = @($script:SourceDirty) } }
function Report([Collections.IDictionary]$Fields) {
    $document = [ordered]@{ qualification = 'proposal_evidence_only'; source = (Source) }
    foreach ($key in $Fields.Keys) { $document[$key] = $Fields[$key] }
    return ($document | ConvertTo-Json -Depth 10 -Compress)
}

$roleFiles = @{
    gateway = @('gateway.json', 'public.json', 'signing.key', 'measurement.json')
    writer = @('trust.json', 'public.json', 'backend.token', 'measurement.json')
    reader = @('trust.json', 'public.json', 'backend.token', 'measurement.json')
    driver = @('gateway.json', 'trust.json', 'public.json', 'signing.key', 'backend.token', 'directory.json', 'manifest.json',
        'load.expected.json', 'suspension.expected.json', 'measurement.json', 'load-0.token', 'suspension-0.token',
        'admin/credentials.json', 'admin/tenants.json')
}

# Builds a fixture prepare summary and per-role export files matching measure-gateway-fixture.py.
# $Tamper may edit: summary, bundles[role], exports[role], files[role], metadata[role].
function New-Fixture([long]$Started, [scriptblock]$Tamper) {
    $fixtureSha = 'f' * 64
    $state = @{ summary = $null; bundles = @{}; exports = @{}; files = @{}; metadata = @{}; text = @{} }
    foreach ($role in 'driver', 'gateway', 'writer', 'reader') {
        $state.metadata[$role] = [ordered]@{ schema = 'vlite-meas-artifacts/1'; source = (Source); fixture_sha256 = $fixtureSha; lineage = 7 }
        $files = [ordered]@{}
        foreach ($name in $roleFiles[$role]) { $files[$name] = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes("SECRET-$role-$name")) }
        $state.files[$role] = $files
        $state.bundles[$role] = [ordered]@{ schema = 'vlite-meas-bundle/1'; role = $role; generation = 't26'
            bootstrap_dir = '/measurement/bootstrap'; source = (Source); files = $files }
    }
    if ($Tamper) { & $Tamper $state 'pre' }
    $receipts = [ordered]@{}
    foreach ($role in 'driver', 'gateway', 'writer', 'reader') {
        $state.files[$role]['measurement.json'] = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(
            ($state.metadata[$role] | ConvertTo-Json -Depth 5 -Compress)))
        $compressed = ConvertTo-Gzip ([Text.Encoding]::UTF8.GetBytes(($state.bundles[$role] | ConvertTo-Json -Depth 10 -Compress)))
        $payload = [Convert]::ToBase64String($compressed)
        # The driver spans multiple projections to exercise ordered reassembly.
        $size = if ($role -ceq 'driver') { [int][Math]::Ceiling($payload.Length / 3) } else { 3000 }
        $secrets = @(for ($i = 0; $i * $size -lt $payload.Length; $i++) {
            [ordered]@{ name = "m-$role-t26-$($i + 1)"; value = $payload.Substring($i * $size, [Math]::Min($size, $payload.Length - $i * $size)) }
        })
        $state.exports[$role] = [ordered]@{ schema = 'vlite-meas-secrets/1'; role = $role; generation = 't26'
            bootstrap_dir = '/measurement/bootstrap'; sha256 = (Get-Sha $compressed); compressed_bytes = $compressed.Length; secrets = $secrets }
        $receipts[$role] = [ordered]@{ file = "/measurement/exports/$role.json"; sha256 = (Get-Sha $compressed)
            compressed_bytes = $compressed.Length; projections = $secrets.Count }
    }
    $sha12 = $fixtureSha.Substring(0, 12)
    $state.summary = [ordered]@{
        schema = 'vlite-meas-fixture-prep/1'; qualification = 'proposal_evidence_only'; source = (Source)
        bootstrap_dir = '/measurement/bootstrap'; control_target_file = '/measurement/control-target.json'
        fixture_sha256 = $fixtureSha; generation = 't26'; credentials_expire_not_before_unix_ms = $Started + 7200000
        exports = $receipts
        image = [ordered]@{ context_dir = '/measurement/image'; fixture_file = '/opt/vlite-measure/fixture/database.vlite'
            base_image = "vlitemeast26.azurecr.io/gateway-measure@sha256:$('b' * 64)"
            image = "vlitemeast26.azurecr.io/gateway-measure-fixture:t26-$sha12"
            planned_build = @('az', 'acr', 'build', '--registry', 'vlitemeast26', '--platform', 'linux/amd64', '--no-logs',
                '--file', '/measurement/image/Dockerfile', '--image', "gateway-measure-fixture:t26-$sha12", '/measurement/image') }
        executed = @()
    }
    if ($Tamper) { & $Tamper $state 'post' }
    foreach ($role in 'driver', 'gateway', 'writer', 'reader') {
        $state.text["/measurement/exports/$role.json"] = $state.exports[$role] | ConvertTo-Json -Depth 10 -Compress
    }
    return $state
}

function Get-Replicas([string]$App, [int]$Count, [string]$Suffix = 'x') {
    $role = switch -Wildcard ($App) { '*-gw' { 'gateway' } '*-wr' { 'writer' } '*-rd' { 'reader' } }
    $list = @(for ($i = 1; $i -le $Count; $i++) {
        @{ name = "$App--r1-$Suffix$i"; id = "$base/providers/Microsoft.App/containerApps/$App/revisions/$App--r1/replicas/$App--r1-$Suffix$i"
            properties = @{ createdTime = '2026-11-18T00:00:00Z'
                containers = @(@{ name = $role; ready = $true; runningState = 'Running'; restartCount = 0 }) } }
    })
    return ,$list
}

function New-MutationReport([string]$Profile = 'pilot', [long]$PollMs = 100) {
    $count = if ($Profile -ceq 'full') { 20 } else { 3 }
    $deadline = if ($Profile -ceq 'full') { 30000 } else { 1000 }
    $revocations = [int][Math]::Floor($count / 2)
    [ordered]@{ schema = 'vlite-meas005/1'; stage = 'mutations'; complete = $false
        mutation_and_ingress_checks_complete = $true
        requested = [ordered]@{ mutations = $count; seed = 1; poll_interval_ms = $PollMs; observation_deadline_ms = $deadline
            labels = [ordered]@{ gateway_replicas = '2'; writer_replicas = '1'; reader_replicas = '1'
                gateway_cpu = '1'; gateway_memory = '2Gi'; writer_cpu = '1'; writer_memory = '2Gi'
                reader_cpu = '0.5'; reader_memory = '1Gi'; driver_cpu = '1'; driver_memory = '2Gi'
                routing = 'writer_only'; load = 'idle' } }
        mutations = @(for ($i = 0; $i -lt $count; $i++) {
            $kind, $status = if ($i -lt $revocations) { 'revocation', 401 }
                elseif (($i - $revocations) % 2 -eq 0) { 'suspension', 403 } else { 'resumption', 200 }
            [ordered]@{ index = $i; kind = $kind
                outcome = 'acknowledged'; generation = 2 + $i; observation_deadline_ms = $deadline
                behavior = [ordered]@{ affected_status = $status; unaffected_semantic_match = $true
                    old_generation_envelope = 'envelope_stale'; scope = 'ingress_behavior_only_not_replica_coverage' } }
        }) }
}

function Reset-Mock([string]$Measurement, [string[]]$Dirty = @()) {
    $script:SourceDirty = @($Dirty)
    $script:Now = 1800000000000L
    $script:Calls = [Collections.Generic.List[string]]::new()
    $script:BuildStatus = 'Succeeded'
    $script:LocalCalls = [Collections.Generic.List[object]]::new()
    $script:Manifests = @{}
    $script:FixturePlan = $null
    $script:RootEmpty = $true
    $script:RootEmptySequence = [Collections.Generic.List[bool]]::new()
    $script:GroupExists = $false
    $script:DeleteFails = $false
    $script:FixtureCleanupExit = 0
    $script:PrepareExit = 0
    $script:Tamper = $null
    $script:SummaryLines = $null
    $script:JobStatus = @{ 'vlite-meas-t26-pub' = 'Succeeded'; 'vlite-meas-t26-drv' = 'Succeeded' }
    $script:Fixture = $null
    $script:AfterReplicaSuffix = 'x'
    $script:SourceRevision = $revision
    $script:OnCall = $null
    $script:ConsoleDownPublish = $false
    $script:QueryReadyAfter = 0
    $script:QueryCount = 0
    $script:PublishLog = New-Frame (Report ([ordered]@{ schema = 'vlite-meas-runtime/1'; operation = 'publish'; generation = 1
        control_state_published = $true; authority_mode = 'off'; read_routing = 'writer_only' }))
    $script:DriverLog = if ($Measurement -ceq 'MEAS-005') {
        $script:MutationJson = Report (New-MutationReport)
        $script:CompanionJson = Report ([ordered]@{ schema = 'vlite-meas005-driver-clock/1'; scope = 'job_local_driver_process_only'
            binding = [ordered]@{ job_name = 'vlite-meas-t26-drv'; job_execution = 'vlite-meas-t26-drv-abc12' } })
        # The companion precedes the unchanged mutation frame; console lines may carry a prefix.
        @(New-Frame $script:CompanionJson 'VLITE-CLOCK' | ForEach-Object { "2026-11-18T00:00:01Z $_" }) + @(New-Frame $script:MutationJson)
    } else {
        New-Frame (Report ([ordered]@{ schema = 'vlite-meas004/1'; order = 'ABBA'; windows = @(@{ rate = 5 }) }))
    }
    $script:ClockReceipt = [ordered]@{ schema = 'vlite-meas005-driver-clock-check/1'; operation = 'check_driver_clock'
        complete = $true; incomplete_reasons = @() }
    $script:ClockExit = 0
    $script:ClockRuntime = $null
    $script:ClockCompanion = $null
    $script:AnalysisLog = New-Frame (Report ([ordered]@{ schema = 'vlite-meas005/1'; stage = 'analysis'; complete = $true; replicas = @() }))
    $script:AnalysisExit = 0
    $script:ObserverMissing = ''
    $script:TamperInventory = $null
    $script:TamperObserver = $null
}

$script:VLiteMeasIsLinux = $true
function Get-VLiteMeasUnixMs { return $script:Now }
function Start-Sleep { param([int]$Seconds, [int]$Milliseconds) $script:Now += [long]$Seconds * 1000 + $Milliseconds }
function Test-VLiteMeasFixtureRootEmpty {
    if ($null -ne $script:RootEmptySequence -and $script:RootEmptySequence.Count -gt 0) {
        $next = $script:RootEmptySequence[0]; $script:RootEmptySequence.RemoveAt(0); return $next
    }
    return $script:RootEmpty
}
function Get-VLiteMeasFixtureFile([string]$Path) {
    $script:Calls.Add("read $Path")
    if (-not $script:Fixture.text.ContainsKey($Path)) { throw 'missing fixture file' }
    return $script:Fixture.text[$Path]
}
function Invoke-VLiteMeasLocal([string]$FilePath, [string[]]$Arguments, [hashtable]$Environment) {
    Assert ($FilePath -ceq 'python3') 'Local tool must be python3'
    Assert ($Environment.VLITE_RESOURCE_SOURCE_REVISION -ceq $revision -and $Environment.ContainsKey('VLITE_RESOURCE_SOURCE_DIRTY_PATHS')) 'Provenance env missing'
    $script:LocalCalls.Add(@{ Arguments = $Arguments; Environment = $Environment })
    if ($Arguments -contains 'prepare') {
        $planFile = $Arguments[[array]::IndexOf($Arguments, '--plan') + 1]
        $script:FixturePlan = [IO.File]::ReadAllText($planFile) | ConvertFrom-Json -AsHashtable
        $script:Fixture = New-Fixture $script:Now $script:Tamper
        if ($script:PrepareExit) { return @{ ExitCode = 1; Output = @('VLITE-MEAS FAILED 1') } }
        $lines = if ($script:SummaryLines) { & $script:SummaryLines $script:Fixture } else { @($script:Fixture.summary | ConvertTo-Json -Depth 20 -Compress) }
        return @{ ExitCode = 0; Output = $lines }
    }
    if ($Arguments -contains 'cleanup') {
        if ($script:FixtureCleanupExit) { return @{ ExitCode = 1; Output = @('VLITE-MEAS FAILED 1') } }
        return @{ ExitCode = 0; Output = @('{"schema":"vlite-meas-fixture-cleanup/1","removed":true}') }
    }
    $runtimeFile = $Arguments[[array]::IndexOf($Arguments, '--config') + 1]
    $runtime = [IO.File]::ReadAllText($runtimeFile) | ConvertFrom-Json -AsHashtable
    $script:LocalCalls[$script:LocalCalls.Count - 1].Operation = $runtime.operation
    # Both offline consumers must receive the exact verified mutation bytes, not re-serialized values.
    $mutationHex = [Convert]::ToHexString((ConvertFrom-VLiteMeasLog @($script:DriverLog)))
    if ($runtime.operation -ceq 'check_driver_clock') {
        $script:ClockRuntime = $runtime
        $script:ClockCompanion = [IO.File]::ReadAllText($runtime.clock_evidence_file)
        $mutationBytes = [IO.File]::ReadAllBytes($runtime.mutation_evidence_file)
        Assert ([Convert]::ToHexString($mutationBytes) -ceq $mutationHex) 'Clock check must receive the exact mutation evidence bytes'
        $receipt = [ordered]@{ companion_sha256 = (Get-Sha ([IO.File]::ReadAllBytes($runtime.clock_evidence_file)))
            mutation_evidence_sha256 = (Get-Sha $mutationBytes); job_name = $runtime.job_name; job_execution = $runtime.job_execution }
        foreach ($key in $script:ClockReceipt.Keys) { $receipt[$key] = $script:ClockReceipt[$key] }
        return @{ ExitCode = $script:ClockExit; Output = @(New-Frame (Report $receipt)) }
    }
    $script:AnalyzeRuntime = $runtime
    $script:AnalyzeNative = [IO.File]::ReadAllText($script:AnalyzeRuntime.native_config) | ConvertFrom-Json -AsHashtable
    Assert ([Convert]::ToHexString([IO.File]::ReadAllBytes($script:AnalyzeNative.mutation_evidence_file)) -ceq $mutationHex) 'Analysis must receive the exact mutation evidence bytes'
    return @{ ExitCode = $script:AnalysisExit; Output = @($script:AnalysisLog) }
}
function Invoke-VLiteMeasAz {
    param([string[]]$Arguments, [switch]$Raw)
    $line = $Arguments -join ' '
    $script:Calls.Add($line)
    Assert ($Arguments[-2] -ceq '--subscription' -and $Arguments[-1] -ceq $subscription) "Missing subscription: $line"
    Assert ($line -notmatch '<[a-z0-9-]+(:[a-z0-9-]+)?>') "Unresolved placeholder: $line"
    if ($script:OnCall) { & $script:OnCall $line }
    function Value([string]$Flag) { $Arguments[[array]::IndexOf($Arguments, $Flag) + 1] }
    switch -Regex ($line) {
        '^group exists' { return $script:GroupExists }
        'check-name' { return @{ nameAvailable = $true } }
        '^group delete' { if ($script:DeleteFails) { throw 'delete failed' }; return }
        '^acr run' {
            return @{ status = $script:BuildStatus }
        }
        '^acr build' { return }
        '^acr repository show .*gateway-measure-fixture:' { return @{ digest = "sha256:$('c' * 64)" } }
        '^acr repository show' { return @{ digest = "sha256:$('b' * 64)" } }
        '^identity create' {
            $name = Value '--name'
            $role = $name.Split('-')[3]
            return @{ id = "$base/providers/Microsoft.ManagedIdentity/userAssignedIdentities/$name"
                principalId = "principal-$role-9f8e7d"; clientId = "client-$role-1a2b3c" }
        }
        '^containerapp env create' {
            return @{ id = "$base/providers/Microsoft.App/managedEnvironments/vlite-meas-t26-env"
                properties = @{ defaultDomain = 'calm-sea-1.westus2.azurecontainerapps.io'
                    appLogsConfiguration = @{ logAnalyticsConfiguration = @{ customerId = 'workspace-55aa66bb' } } } }
        }
        '^containerapp (job )?create' {
            $name = Value '--name'
            $script:Manifests[$name] = [IO.File]::ReadAllText((Value '--yaml')) | ConvertFrom-Json -AsHashtable
            return @{ id = "x/$name" }
        }
        '^containerapp job start' { return @{ name = "$(Value '--name')-abc12" } }
        '^containerapp job execution show' { return @{ properties = @{ status = $script:JobStatus[(Value '--name')] } } }
        '^containerapp job logs show' {
            if ($script:ConsoleDownPublish -and (Value '--name') -ceq 'vlite-meas-t26-pub') { throw 'console unavailable for completed execution' }
            if ((Value '--name') -ceq 'vlite-meas-t26-pub') { return $script:PublishLog }
            return $script:DriverLog
        }
        '^containerapp replica list' {
            $app = Value '--name'
            $count = if ($app -like '*-gw') { 2 } else { 1 }
            $before = @($script:Calls | Where-Object { $_ -like 'containerapp job start --name vlite-meas-t26-drv*' }).Count -eq 0
            $suffix = if ($before) { 'x' } else { $script:AfterReplicaSuffix }
            $replicas = Get-Replicas $app $count $suffix
            if ($script:TamperInventory) { $replicas = @(& $script:TamperInventory $replicas $before) }
            return ,$replicas
        }
        '^containerapp logs show' {
            $replica = Value '--replica'
            if ($replica -ceq $script:ObserverMissing) { return @() }
            $role = if ($replica -like '*-gw--*') { 'gateway' } else { 'backend' }
            $observed = [ordered]@{ schema = 'vlite-meas005-observer/1'; replica = $replica; role = $role
                app = (Value '--name'); revision = (Value '--revision')
                owns_serving_child = $true; terminated_early = $false; interrupted = $false; events = @() }
            if ($script:TamperObserver) { & $script:TamperObserver $observed }
            return New-Frame (Report $observed)
        }
        '^monitor log-analytics query' {
            $script:QueryCount++
            # Models Log Analytics ingestion lag: rows appear only after QueryReadyAfter empty polls.
            if ($script:QueryReadyAfter -gt 0 -and $script:QueryCount -gt $script:QueryReadyAfter) {
                return @($script:PublishLog | ForEach-Object { @{ Log_s = $_ } })
            }
            throw 'no rows'
        }
        default { return @{ id = 'ok' } }
    }
}

function Invoke-Live([string]$Measurement, [hashtable]$Extra = @{}) {
    $out = Join-Path $scratch ([guid]::NewGuid().ToString('N'))
    $options = @{ Id = 't26'; SourceRevision = $revision; OutDir = $out; SubscriptionId = $subscription
        Measurements = @($Measurement); FixtureServer = '/opt/vlite/bin/vlite-server' }
    if ($Measurement -ceq 'MEAS-005') { $options.ControlProgram = '/opt/vlite/bin/ha-meas005' }
    foreach ($key in $Extra.Keys) { $options[$key] = $Extra[$key] }
    $config = Test-VLiteMeasParameters $options
    $config.DirtyPaths = ($script:SourceDirty -join "`n")
    $result = @{ Error = $null; Out = $out; Summary = $null }
    try { $null = Invoke-VLiteMeasGateway $config 6>$null 3>$null } catch { $result.Error = $_.Exception.Message }
    $summaryFile = Join-Path $out 'summary.json'
    if (Test-Path -LiteralPath $summaryFile) {
        $result.SummaryText = [IO.File]::ReadAllText($summaryFile)
        $result.Summary = $result.SummaryText | ConvertFrom-Json -AsHashtable
    }
    return $result
}

function Assert-Cleaned($Result, [string]$Label) {
    Assert ($script:Calls -contains "group delete --name vlite-ha-meas-t26 --yes --no-wait --subscription $subscription") "$Label`: group not deleted"
    Assert ($Result.Summary.cleanupStatus -ceq 'Deleted') "$Label`: cleanup status"
    if ($script:LocalCalls.Count) {
        Assert (@($script:LocalCalls | Where-Object { $_.Arguments -contains 'cleanup' }).Count -ge 1) "$Label`: fixture cleanup missing"
    }
}

function Assert-FixtureRefusal([string]$Label, [scriptblock]$Tamper, [string]$Reason, [scriptblock]$Setup = $null) {
    Reset-Mock 'MEAS-004'
    $script:Tamper = $Tamper
    if ($Setup) { & $Setup }
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Error -ceq 'MEAS-004 failed at ValidateFixture; diagnostics withheld.') "$Label`: unexpected error $($result.Error)"
    Assert ($result.Summary.failureReason -ceq $Reason) "$Label`: reason $($result.Summary.failureReason)"
    Assert (-not ($script:Calls | Where-Object { $_ -like 'acr build*gateway-measure-fixture*' -or $_ -like 'containerapp create*' -or $_ -like 'containerapp job create*' })) "$Label`: continued after refusal"
    Assert ($result.Summary.fixtureCleanup -ceq 'Removed') "$Label`: fixture not cleaned"
    Assert-Cleaned $result $Label
    Assert (-not $result.SummaryText.Contains('SECRET-')) "$Label`: summary leaked fixture material"
}

try {
    # ---- Plan: offline, non-mutating, verifiable. ----
    function az { throw 'az must not be called' }
    function git { throw 'git must not be called' }
    $saved = ${function:Invoke-VLiteMeasAz}
    $savedLocal = ${function:Invoke-VLiteMeasLocal}
    function Invoke-VLiteMeasAz { throw 'Azure must not be called in Plan' }
    function Invoke-VLiteMeasLocal { throw 'Local processes must not run in Plan' }
    $script:VLiteMeasIsLinux = $false  # -Plan is host-independent
    foreach ($measurement in 'MEAS-004', 'MEAS-005') {
        $out = Join-Path $scratch "plan-$measurement"
        $steps = @(Invoke-VLiteMeas -Options @{ Id = 't26'; SourceRevision = $revision; OutDir = $out; Plan = $true
            Measurements = @($measurement) } 6>$null)
        Assert (-not (Test-Path -LiteralPath $out)) 'Plan must not create OutDir'
        $order = ($steps | ForEach-Object { $_.Step }) | Select-Object -Unique
        $expected = @('CheckGroup', 'CheckRegistryName', 'CheckStorageName', 'CreateGroup', 'CreateRegistry', 'BuildMeasureImage',
            'ShowMeasureImage', 'CreateIdentity', 'GrantPull', 'CreateStorage', 'CreateControlContainer', 'GrantControl',
            'CreateEnvironment', 'PrepareFixture', 'BuildFixtureImage', 'ShowFixtureImage', 'CreatePublishJob', 'StartPublish',
            'PollPublish', 'ReadPublish', 'QueryPublish', 'CreateApp', 'CreateDriverJob', 'CleanupFixture', 'InventoryBefore',
            'StartDriver', 'PollDriver', 'ReadDriver', 'QueryDriver', 'InventoryAfter')
        if ($measurement -ceq 'MEAS-005') { $expected += @('CheckDriverClock', 'ReadObserver', 'QueryObserver', 'Analyze') }
        $expected += @('DeleteGroup', 'WaitDeleted')
        Assert (($order -join ',') -ceq ($expected -join ',')) "$measurement plan order differs: $($order -join ',')"
        $apps = @($steps | Where-Object Step -ceq 'CreateApp' | ForEach-Object Role)
        $expectedApps = if ($measurement -ceq 'MEAS-005') { 'writer,reader,gateway' } else { 'writer,gateway' }
        Assert (($apps -join ',') -ceq $expectedApps) "$measurement app set differs"
        Assert (@($steps | Where-Object Step -ceq 'PrepareFixture').Count -eq 1) 'Exactly one bootstrap must be planned'
        $measureBuild = $steps | Where-Object Step -ceq 'BuildMeasureImage'
        $taskPath = $measureBuild.Arguments[[array]::IndexOf($measureBuild.Arguments, '--file') + 1]
        $context = $measureBuild.Arguments[-3]
        Assert (-not [IO.Path]::IsPathRooted($taskPath) -and (Test-Path -LiteralPath (Join-Path $context $taskPath) -PathType Leaf)) 'ACR must resolve the task file inside the uploaded source'
        $taskYaml = [IO.File]::ReadAllText((Join-Path $context $taskPath))
        Assert ($taskYaml.Contains('DOCKER_BUILDKIT=1') -and $taskYaml.Contains('--target gateway-measure') -and
            $taskYaml.Contains('gateway-measure:{{.Values.revision}}') -and $measureBuild.Arguments -ccontains "revision=$revision") 'Build and push must use the pinned source tag'
        $text = $steps | ConvertTo-Json -Depth 30
        foreach ($needle in '<subscription>', '<projections:driver>', '<client-id:gateway>', 'vlite-ha-meas-t26', 'vliteMeasurementOwner=ha-harness') {
            Assert ($text.Contains($needle)) "$measurement plan lacks $needle"
        }
        Assert (-not ($text -match 'SECRET-|principal-[a-z]+-9f8e|client-[a-z]+-1a2b')) 'Plan must contain placeholders only'
        foreach ($step in $steps | Where-Object Tool -ceq 'az') {
            Assert ($step.Arguments[-2] -ceq '--subscription') "$($step.Step) lacks subscription"
            Assert ($step.Arguments -notcontains 'containerapp' -or $step.Arguments -notcontains 'exec') 'exec is not used'
        }
        $build = ($steps | Where-Object Step -ceq 'BuildFixtureImage').Arguments
        Assert (($build[0..11] -join ' ') -ceq 'acr build --registry vlitemeast26 --platform linux/amd64 --no-logs --file /measurement/image/Dockerfile --image gateway-measure-fixture:t26-<fixture-sha12> /measurement/image') 'Fixture build must follow the helper plan'
        $fixturePlan = ($steps | Where-Object Step -ceq 'PrepareFixture').Manifest
        Assert ((@($fixturePlan.Keys) -join ',') -ceq 'server,writer_url,audience,control_target,control_object,generation,registry,base_image,fixture_repository,poll_interval_ms,revocations') 'Fixture plan keys differ'
        Assert ($fixturePlan.writer_url -ceq 'https://vlite-meas-t26-wr.internal.<default-domain>') 'Fixture writer URL differs'
        foreach ($create in $steps | Where-Object { $_.Step -cin 'CreateApp', 'CreatePublishJob', 'CreateDriverJob' }) {
            $manifest = $create.Manifest
            $names = @($manifest.properties.configuration.secrets | ForEach-Object { $_.name })
            Assert (@($names | Where-Object { $_ -like '<projections:*' -and $_ -cne "<projections:$($create.Role)>" }).Count -eq 0) "$($create.Step) mixes role projections"
            $identities = @($manifest.identity.userAssignedIdentities.Keys)
            Assert ($identities.Count -eq 1 -and $identities[0] -like "*/vlite-meas-t26-$($create.Role)-id") "$($create.Step) identity differs"
            Assert ($manifest.properties.configuration.registries[0].identity -ceq $identities[0]) 'Registry identity differs'
            $script = $manifest.properties.template.containers[0].args[0]
            Assert ($script.Contains('cp -L /run/vlite-meas/*') -and $script.Contains('umask 077')) 'Container script must copy private secrets'
            if ($create.Role -cin 'writer', 'reader') {
                Assert ($names -contains 'cfg-copy' -and $script.Contains('cfg-copy')) 'Backends copy the exact fixture'
                $run = ($manifest.properties.configuration.secrets | Where-Object name -ceq 'cfg-run').value | ConvertFrom-Json -AsHashtable
                Assert ($run.database_file -ceq "/measurement/$($create.Role)/database.vlite") 'Backend database path differs'
            } else { Assert ($names -notcontains 'cfg-copy') 'Only backends copy the fixture' }
            if ($create.Step -ceq 'CreateApp') {
                $scale = $manifest.properties.template.scale
                $count = if ($create.Role -ceq 'gateway') { 2 } else { 1 }
                Assert ($scale.minReplicas -eq $count -and $scale.maxReplicas -eq $count) 'Apps use fixed replica counts'
                Assert ($manifest.properties.configuration.ingress.external -eq $false) 'Apps use internal ingress only'
            } else {
                Assert ($manifest.properties.configuration.replicaRetryLimit -eq 0 -and
                    $manifest.properties.configuration.manualTriggerConfig.parallelism -eq 1) 'Jobs are never retried'
            }
        }
        $driver = ($steps | Where-Object Step -ceq 'CreateDriverJob').Manifest.properties.configuration.secrets
        $native = ($driver | Where-Object name -ceq 'cfg-native').value | ConvertFrom-Json -AsHashtable
        if ($measurement -ceq 'MEAS-004') {
            Assert ($fixturePlan.poll_interval_ms -eq 1000 -and $fixturePlan.revocations -eq 10) 'MEAS-004 fixture defaults changed'
            Assert (($native.rates -join ',') -ceq '5' -and $native.requested.trial -ceq 'fixed-two') 'MEAS-004 pilot settings differ'
            Assert ($native.direct_url -ceq 'https://vlite-meas-t26-wr.internal.<default-domain>') 'Direct URL differs'
        } else {
            Assert ($native.poll_interval_ms -eq 100 -and $native.observation_deadline_ms -eq 1000 -and
                $native.mutations -eq 3 -and $native.seed -eq 1 -and $fixturePlan.poll_interval_ms -eq 100 -and
                $fixturePlan.revocations -eq 1) 'MEAS-005 pilot settings differ'
            $observe = ((($steps | Where-Object { $_.Step -ceq 'CreateApp' -and $_.Role -ceq 'gateway' }).Manifest.properties.configuration.secrets |
                Where-Object name -ceq 'cfg-run').value | ConvertFrom-Json -AsHashtable)
            Assert ($observe.operation -ceq 'observe' -and $observe.observe_interval_ms -eq 50 -and $observe.duration_secs -eq 1200 -and
                -not $observe.Contains('database_file')) 'Gateway observer config differs'
        }
    }
    $full = @(Invoke-VLiteMeas -Options @{ Id = 't26'; SourceRevision = $revision; OutDir = (Join-Path $scratch 'plan-full'); Plan = $true
        Measurements = @('MEAS-004'); GatewayProfile = 'full' } 6>$null)
    $native = (($full | Where-Object Step -ceq 'CreateDriverJob').Manifest.properties.configuration.secrets | Where-Object name -ceq 'cfg-native').value | ConvertFrom-Json -AsHashtable
    Assert (($native.rates -join ',') -ceq '50,200,800,1600' -and $native.duration_secs -eq 180 -and $native.max_in_flight -eq 256) 'Full MEAS-004 settings differ from the contract'
    foreach ($poll in 1, 100, 250, 500, 1000, 2000, 5000) {
        $steps = @(Invoke-VLiteMeas -Options @{ Id = "sweep$poll"; SourceRevision = $revision; Plan = $true
            OutDir = (Join-Path $scratch "sweep-$poll"); Measurements = @('MEAS-005')
            GatewayProfile = 'full'; GatewayPollMs = "$poll" } 6>$null)
        $fixturePlan = ($steps | Where-Object Step -ceq 'PrepareFixture').Manifest
        $native = (($steps | Where-Object Step -ceq 'CreateDriverJob').Manifest.properties.configuration.secrets |
            Where-Object name -ceq 'cfg-native').value | ConvertFrom-Json -AsHashtable
        Assert ($fixturePlan.poll_interval_ms -eq $poll -and $fixturePlan.revocations -eq 10 -and
            $native.poll_interval_ms -eq $poll -and $native.observation_deadline_ms -eq 30000 -and
            $native.mutations -eq 20) "Sweep $poll does not share effective settings"
    }

    # ---- Option refusals. ----
    $opts = @{ Id = 't26'; SourceRevision = $revision; OutDir = (Join-Path $scratch 'opts'); Plan = $true }
    foreach ($bad in @(
        @{ Measurements = @('MEAS-004'); Tiers = @(1) }, @{ Measurements = @('MEAS-005'); Records = '5' },
        @{ Measurements = @('MEAS-004'); Image = 'x/y:1' }, @{ Measurements = @('MEAS-004'); KeepOnFailure = $true },
        @{ Measurements = @('MEAS-004'); ControlProgram = '/x/y' }, @{ Measurements = @('MEAS-005'); GatewayProfile = 'huge' },
        @{ Measurements = @('MEAS-005'); FixtureServer = 'relative/server' }, @{ Measurements = @('MEAS-005'); FixtureServer = '/a/../b' },
        @{ Measurements = @('MEAS-007'); FixtureServer = '/a/b' }, @{ Measurements = @('MEAS-002'); GatewayProfile = 'pilot' },
        @{ Measurements = @('MEAS-004', 'MEAS-005') }, @{ Measurements = @('MEAS-004'); Samples = '5' },
        @{ Measurements = @('MEAS-004'); GatewayPollMs = '100' }, @{ Measurements = @('MEAS-006'); GatewayPollMs = '100' },
        @{ Measurements = @('MEAS-002'); GatewayPollMs = '100' }, @{ Measurements = @('MEAS-007'); GatewayPollMs = '100' },
        @{ Measurements = @('MEAS-005'); GatewayPollMs = '' }, @{ Measurements = @('MEAS-005'); GatewayPollMs = '0' },
        @{ Measurements = @('MEAS-005'); GatewayPollMs = '5001' }, @{ Measurements = @('MEAS-005'); GatewayPollMs = '1.5' },
        @{ Measurements = @('MEAS-005'); GatewayPollMs = 'true' }
    )) {
        $candidate = $opts.Clone(); foreach ($key in $bad.Keys) { $candidate[$key] = $bad[$key] }
        $failed = $false
        try { $null = Test-VLiteMeasParameters $candidate } catch { $failed = $true }
        Assert $failed "Expected refusal for $($bad | ConvertTo-Json -Compress)"
    }
    $live = @{ Id = 't26'; SourceRevision = $revision; OutDir = (Join-Path $scratch 'opts'); SubscriptionId = $subscription }
    Assert-Throws { Test-VLiteMeasParameters ($live + @{ Measurements = @('MEAS-004'); FixtureServer = '/a/b' }) } 'Linux/WSL'
    $script:VLiteMeasIsLinux = $true
    Assert-Throws { Test-VLiteMeasParameters ($live + @{ Measurements = @('MEAS-004') }) } 'FixtureServer is required'
    Assert-Throws { Test-VLiteMeasParameters ($live + @{ Measurements = @('MEAS-005'); FixtureServer = '/a/b' }) } 'ControlProgram is required'
    Set-Item function:Invoke-VLiteMeasAz $saved
    Set-Item function:Invoke-VLiteMeasLocal $savedLocal

    # ---- Live MEAS-004 orchestration with mocks. ----
    Reset-Mock 'MEAS-004'
    $script:BuildStatus = 'Failed'
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Summary.failedStep -ceq 'BuildMeasureImage' -and $result.Summary.failureReason -ceq 'GatewayImageBuild') 'Unsuccessful task cannot supply an image'
    Assert (-not ($script:Calls | Where-Object { $_ -like 'acr repository show*' -or $_ -like 'containerapp *' })) 'Stop provisioning after failed task'
    Assert-Cleaned $result 'gateway-build-failure'

    Reset-Mock 'MEAS-004'
    $result = Invoke-Live 'MEAS-004'
    Assert (-not $result.Error) "MEAS-004 run failed: $($result.Error)"
    Assert ($result.Summary.status -ceq 'Succeeded' -and $result.Summary.measurementStatus -ceq 'Collected') 'MEAS-004 summary status'
    Assert ($result.Summary.qualification -ceq 'proposal_evidence_only' -and -not $result.SummaryText.Contains('passed')) 'No qualification claim'
    Assert ($result.Summary.fixtureCleanup -ceq 'RemovedAfterDistribution' -and $result.Summary.inventory.fixed) 'Fixture cleanup / inventory'
    Assert ($result.Summary.credentialsExpireNotBeforeUnixMs -eq 1800000000000 + 7200000) 'Expiry recorded'
    Assert (Test-Path (Join-Path $result.Out 'MEAS-004-load.json')) 'Load evidence saved'
    Assert-Cleaned $result 'MEAS-004'
    Assert (@($script:LocalCalls | Where-Object { $_.Arguments -contains 'prepare' }).Count -eq 1) 'Exactly one bootstrap'
    Assert ($script:FixturePlan.server -ceq '/opt/vlite/bin/vlite-server' -and
        $script:FixturePlan.writer_url -ceq 'https://vlite-meas-t26-wr.internal.calm-sea-1.westus2.azurecontainerapps.io' -and
        $script:FixturePlan.base_image -ceq "vlitemeast26.azurecr.io/gateway-measure@sha256:$('b' * 64)") 'Fixture plan uses live values'
    $prepareIndex = $script:Calls.IndexOf(($script:Calls | Where-Object { $_ -like 'containerapp env create*' }))
    $measureBuildIndex = $script:Calls.IndexOf(($script:Calls | Where-Object { $_ -like 'acr run*gateway-build.yaml*' }))
    $buildIndex = $script:Calls.IndexOf(($script:Calls | Where-Object { $_ -like 'acr build*gateway-measure-fixture*' }))
    $publishIndex = $script:Calls.IndexOf(($script:Calls | Where-Object { $_ -like 'containerapp job start --name vlite-meas-t26-pub*' }))
    $appIndex = $script:Calls.IndexOf(($script:Calls | Where-Object { $_ -like 'containerapp create --name vlite-meas-t26-wr*' }))
    Assert ($measureBuildIndex -ge 0 -and $measureBuildIndex -lt $prepareIndex -and
        $prepareIndex -lt $buildIndex -and $buildIndex -lt $publishIndex -and $publishIndex -lt $appIndex) 'Build after bootstrap, publish before apps'
    Assert ($script:Calls -contains "acr build --registry vlitemeast26 --platform linux/amd64 --no-logs --file /measurement/image/Dockerfile --image gateway-measure-fixture:t26-ffffffffffff /measurement/image --resource-group vlite-ha-meas-t26 --subscription $subscription") 'Exact fixture build'
    Assert (@($script:Calls | Where-Object { $_ -like 'containerapp job start --name vlite-meas-t26-pub*' }).Count -eq 1) 'Publish once'
    foreach ($name in 'vlite-meas-t26-pub', 'vlite-meas-t26-drv', 'vlite-meas-t26-gw', 'vlite-meas-t26-wr') {
        $manifest = $script:Manifests[$name]
        Assert $manifest "Missing manifest $name"
        $role = switch -Wildcard ($name) { '*-gw' { 'gateway' } '*-wr' { 'writer' } default { 'driver' } }
        $secrets = $manifest.properties.configuration.secrets
        $projectionNames = @($secrets | Where-Object { $_.name -like 'm-*' } | ForEach-Object name)
        Assert ($projectionNames.Count -ge 1 -and @($projectionNames | Where-Object { $_ -notlike "m-$role-t26-*" }).Count -eq 0) "$name has foreign projections"
        Assert ($manifest.properties.template.containers[0].image -ceq "vlitemeast26.azurecr.io/gateway-measure-fixture@sha256:$('c' * 64)") "$name image is not the exact fixture image"
        $target = ($secrets | Where-Object name -ceq 'control-target').value | ConvertFrom-Json -AsHashtable
        Assert ($target.client_id -ceq "client-$role-1a2b3c" -and $target.auth -ceq 'managed_identity' -and $target.prefix -ceq 'meas-t26/') "$name control target differs"
        $restore = ($secrets | Where-Object name -ceq 'cfg-restore').value | ConvertFrom-Json -AsHashtable
        Assert ($restore.sha256 -ceq $script:Fixture.exports[$role].sha256 -and $restore.generation -ceq 't26' -and
            ($restore.projection_files -join ',') -ceq (($projectionNames | ForEach-Object { "/measurement/in/$_" }) -join ',')) "$name restore config differs"
        # Decode delivered projections: backends never receive signing/admin/driver material.
        $payload = -join @($secrets | Where-Object { $_.name -like 'm-*' } | ForEach-Object value)
        $bundle = ConvertFrom-VLiteMeasJsonBytes (Expand-VLiteMeasGzip ([Convert]::FromBase64String($payload)) 2097152)
        Assert ($bundle.role -ceq $role) "$name delivered a foreign bundle"
        if ($role -ceq 'writer') { Assert (-not ($bundle.files.Keys | Where-Object { $_ -like 'admin/*' -or $_ -ceq 'signing.key' })) 'Writer received driver/signing files' }
        $env = $manifest.properties.template.containers[0].env
        Assert (($env | Where-Object name -ceq 'VLITE_RESOURCE_SOURCE_REVISION').value -ceq $revision) "$name provenance env"
    }
    Assert (-not ($script:Manifests.Values | ForEach-Object { $_ | ConvertTo-Json -Depth 30 } | Where-Object { $_ -match $subscription -and $_ -match 'principal-' })) 'Manifests do not carry principal IDs'
    Assert (-not $result.SummaryText.Contains('client-') -and -not $result.SummaryText.Contains($subscription) -and
        -not $result.SummaryText.Contains('workspace-55aa')) 'Summary leaks private identifiers'
    Assert (-not (Get-ChildItem -LiteralPath $result.Out -Recurse -File | Where-Object { [IO.File]::ReadAllText($_.FullName).Contains('SECRET-') })) 'OutDir contains fixture material'

    # ---- Live MEAS-005 orchestration with observers and offline analysis. ----
    Reset-Mock 'MEAS-005'
    $result = Invoke-Live 'MEAS-005'
    Assert (-not $result.Error) "MEAS-005 run failed: $($result.Error)"
    Assert ($result.Summary.measurementStatus -ceq 'Incomplete' -and $result.Summary.incompleteReasons -contains 'ClockCalibrationUnavailable') 'MEAS-005 timing stays incomplete without clocks'
    Assert (@($result.Summary.evidence | Where-Object kind -ceq 'observer').Count -eq 4) 'Four observer reports collected'
    Assert (Test-Path (Join-Path $result.Out 'MEAS-005-analysis.json')) 'Analysis saved'
    Assert ($script:AnalyzeRuntime.control_program -ceq '/opt/vlite/bin/ha-meas005' -and $script:AnalyzeRuntime.operation -ceq 'analyze' -and
        $script:AnalyzeNative.fixed_inventory_confirmed -eq $true -and $script:AnalyzeNative.replicas.Count -eq 4) 'Analyze config differs'
    Assert ($script:Now -ge 1800000000000 + 1260000) 'Observers waited for their bounded window'
    Assert ($script:ClockRuntime.operation -ceq 'check_driver_clock' -and $script:ClockRuntime.role -ceq 'driver' -and
        $script:ClockRuntime.job_name -ceq 'vlite-meas-t26-drv' -and $script:ClockRuntime.job_execution -ceq 'vlite-meas-t26-drv-abc12' -and
        $script:ClockRuntime.source_revision -ceq $revision -and -not $script:ClockRuntime.Contains('native_config')) 'Clock check must bind the started driver execution'
    Assert ($script:ClockCompanion -ceq $script:CompanionJson) 'Clock check must receive the exact VLITE-CLOCK payload'
    Assert ([IO.File]::ReadAllText((Join-Path $result.Out 'MEAS-005-driver-clock.json')) -ceq $script:CompanionJson -and
        (Test-Path (Join-Path $result.Out 'MEAS-005-driver-clock-check.json')) -and
        @($result.Summary.evidence | Where-Object { $_.kind -cin @('driver-clock', 'driver-clock-check') }).Count -eq 2) 'Driver clock evidence saved'
    Assert (-not ($result.Summary.incompleteReasons | Where-Object { $_ -like 'DriverClock*' })) 'A complete job-local check adds no clock reason'
    Assert ([IO.File]::ReadAllText((Join-Path $result.Out 'MEAS-005-mutations.json')) -ceq $script:MutationJson) 'Mutation evidence unchanged by the companion'
    $clockIndex = @($script:LocalCalls | ForEach-Object { $_.Operation }).IndexOf('check_driver_clock')
    Assert ($clockIndex -ge 0 -and $clockIndex -lt @($script:LocalCalls | ForEach-Object { $_.Operation }).IndexOf('analyze')) 'Clock check runs once before analysis'
    Assert (@($script:LocalCalls | Where-Object { $_.Operation -ceq 'check_driver_clock' }).Count -eq 1) 'Clock check runs once'
    $observerCalls = @($script:Calls | Where-Object { $_ -like 'containerapp logs show*' })
    Assert ($observerCalls.Count -eq 4 -and ($observerCalls | Where-Object { $_ -like '*--replica vlite-meas-t26-rd--r1-x1*--container reader*' })) 'Replica-specific observer collection'
    $reader = $script:Manifests['vlite-meas-t26-rd']
    Assert ((($reader.properties.configuration.secrets | Where-Object name -ceq 'cfg-run').value | ConvertFrom-Json -AsHashtable).operation -ceq 'observe') 'Reader runs observer'
    $mutate = (($script:Manifests['vlite-meas-t26-drv'].properties.configuration.secrets | Where-Object name -ceq 'cfg-native').value | ConvertFrom-Json -AsHashtable)
    Assert ($mutate.target_file -ceq '/measurement/control-target.json' -and $mutate.server -ceq '/usr/local/bin/vlite-server' -and $mutate.mode -ceq 'run') 'Mutate config differs'
    Assert-Cleaned $result 'MEAS-005'

    Reset-Mock 'MEAS-005'
    $script:MutationJson = Report (New-MutationReport 'full' 250)
    $script:DriverLog = New-Frame $script:MutationJson
    $result = Invoke-Live 'MEAS-005' @{ GatewayProfile = 'full'; GatewayPollMs = '250' }
    Assert (-not $result.Error -and $result.Summary.profile -ceq 'full' -and
        -not ($result.Summary.incompleteReasons -contains 'MutationChecksIncomplete')) 'Valid full-profile mutation checks must be accepted'
    Assert ([IO.File]::ReadAllText((Join-Path $result.Out 'MEAS-005-mutations.json')) -ceq $script:MutationJson) 'Full-profile evidence is preserved unchanged'
    Assert-Cleaned $result 'MEAS-005-full'

    Reset-Mock 'MEAS-005'
    $report = New-MutationReport
    $report.complete = $true
    $report.mutations[0].generation = 18446744073709551613uL
    $report.mutations[1].generation = 18446744073709551614uL
    $report.mutations[2].generation = [ulong]::MaxValue
    $script:MutationJson = Report $report
    $script:DriverLog = New-Frame $script:MutationJson
    $result = Invoke-Live 'MEAS-005'
    Assert (-not $result.Error) 'Valid complete checks and adjacent u64 generations must be accepted'
    Assert-Cleaned $result 'MEAS-005-complete'

    # Driver clock evidence is job-local and advisory: problems add incomplete reasons, never failures or retries.
    # Each case: reason, whether the offline check runs, and the setup.
    $clockCases = @(
        @('DriverClockContinuityUnavailable', $false, { $script:DriverLog = New-Frame $script:MutationJson }),
        @('DriverClockContinuityIncomplete', $true, {
            $script:ClockExit = 2; $script:ClockReceipt.complete = $false; $script:ClockReceipt.incomplete_reasons = @('monitor_unavailable') }),
        @('DriverClockEvidenceRejected', $true, { $script:ClockExit = 1 }),
        @('DriverClockEvidenceRejected', $true, { $script:ClockExit = 2 }),
        @('DriverClockEvidenceRejected', $true, { $script:ClockReceipt.complete = 'true' }),
        @('DriverClockEvidenceRejected', $true, { $script:ClockReceipt.schema = 'vlite-meas005-driver-clock/1' }),
        @('DriverClockEvidenceRejected', $true, { $script:ClockReceipt.operation = 'analyze' }),
        @('DriverClockEvidenceRejected', $true, { $script:ClockReceipt.job_execution = 'vlite-meas-t26-drv-other1' }),
        @('DriverClockEvidenceRejected', $true, { $script:ClockReceipt.mutation_evidence_sha256 = 'e' * 64 }),
        @('DriverClockEvidenceRejected', $true, { $script:ClockReceipt.companion_sha256 = 'e' * 64 }),
        @('DriverClockEvidenceRejected', $false, { $script:DriverLog = @($script:DriverLog | Select-Object -Skip 1) }),
        @('DriverClockEvidenceRejected', $false, { $script:DriverLog = @(New-Frame $script:CompanionJson 'VLITE-CLOCK') + @($script:DriverLog) }),
        @('DriverClockEvidenceRejected', $false, {
            $script:DriverLog = @(New-Frame (Report ([ordered]@{ schema = 'vlite-meas005-observer/1' })) 'VLITE-CLOCK') + @(New-Frame $script:MutationJson) }))
    foreach ($case in $clockCases) {
        $reason, $checked, $setup = $case
        Reset-Mock 'MEAS-005'
        & $setup
        $result = Invoke-Live 'MEAS-005'
        Assert (-not $result.Error -and $result.Summary.status -ceq 'Succeeded') "$reason`: clock evidence must not fail the run: $($result.Error)"
        Assert ($result.Summary.incompleteReasons -contains $reason -and
            $result.Summary.incompleteReasons -contains 'ClockCalibrationUnavailable') "$reason`: missing incomplete reason"
        Assert (@($result.Summary.incompleteReasons | Where-Object { $_ -like 'DriverClock*' }).Count -eq 1) "$reason`: one clock reason"
        Assert (Test-Path (Join-Path $result.Out 'MEAS-005-analysis.json')) "$reason`: analysis still runs"
        Assert ([IO.File]::ReadAllText((Join-Path $result.Out 'MEAS-005-mutations.json')) -ceq $script:MutationJson) "$reason`: mutation evidence changed"
        Assert (@($script:Calls | Where-Object { $_ -like 'containerapp job start --name vlite-meas-t26-drv*' }).Count -eq 1) "$reason`: driver must never be retried"
        $saved = @($result.Summary.evidence | Where-Object { $_.kind -like 'driver-clock*' }).Count
        Assert ($saved -eq $(if ($reason -ceq 'DriverClockContinuityIncomplete') { 2 } else { 0 })) "$reason`: unverified clock evidence saved"
        Assert (($null -ne $script:ClockRuntime) -eq $checked) "$reason`: clock check invocation differs"
    }

    foreach ($field in 'app', 'revision') {
        Reset-Mock 'MEAS-005'
        $script:TamperObserver = { param($report) $report[$field] = 'foreign' }.GetNewClosure()
        $result = Invoke-Live 'MEAS-005'
        Assert ($result.Summary.incompleteReasons -contains 'ObserverReportMissing' -and
            $result.Summary.incompleteReasons -contains 'AnalysisNotRun') "Foreign observer $field was accepted"
    }

    Reset-Mock 'MEAS-005'
    $script:TamperInventory = { param($replicas, $before)
        if (-not $before) {
            [array]::Reverse($replicas)
            foreach ($replica in $replicas) { $replica.properties.containers[0].ready = $false }
        }
        $replicas
    }
    $result = Invoke-Live 'MEAS-005'
    Assert ($result.Summary.inventory.fixed -and -not $result.Error) 'Reordered inventory and intentionally stopped observers must preserve fixed identity'

    foreach ($badCount in $null, '', '0', $false, -1, 0.5) {
        Reset-Mock 'MEAS-005'
        $script:TamperInventory = { param($replicas, $before)
            $replicas[0].properties.containers[0].restartCount = $badCount
            $replicas
        }.GetNewClosure()
        $result = Invoke-Live 'MEAS-005'
        Assert ($result.Summary.failureReason -ceq 'Inventory') 'Missing or malformed restart count was accepted'
        Assert (-not ($script:Calls | Where-Object { $_ -like 'containerapp job start --name vlite-meas-t26-drv*' })) 'Invalid inventory must stop before driver writes'
    }

    foreach ($inventoryTamper in
        { param($replicas, $before) $replicas[0].id = ''; $replicas },
        { param($replicas, $before) $replicas[0].id = $replicas[0].id.Replace('/containerApps/', '/foreignApps/'); $replicas },
        { param($replicas, $before) $replicas[0].properties.containers[0].name = 'foreign'; $replicas },
        { param($replicas, $before) $replicas[0]; $replicas[0] }) {
        Reset-Mock 'MEAS-005'
        $script:TamperInventory = $inventoryTamper
        $result = Invoke-Live 'MEAS-005'
        Assert ($result.Summary.failureReason -ceq 'Inventory') 'Missing, foreign or duplicate identity was accepted'
    }

    foreach ($mutationTamper in @(
        { param($r) $r.complete = $true; $r.mutations = @(); $r.Remove('requested') },
        { param($r) $r.complete = $true; $r.mutations = @() },
        { param($r) $r.mutation_and_ingress_checks_complete = 'false' },
        { param($r) $r.Remove('mutations') },
        { param($r) $r.mutations = @($r.mutations[0]) },
        { param($r) $r.requested.mutations = 20 },
        { param($r) $r.requested.seed = 2 },
        { param($r) $r.requested.poll_interval_ms = 1000 },
        { param($r) $r.requested.observation_deadline_ms = 30000 },
        { param($r) $r.requested.labels.routing = 'reader_preferred' },
        { param($r) $r.requested.labels.Remove('reader_cpu') },
        { param($r) $r.mutations[0].outcome = 'unknown_do_not_retry'; $r.mutations[0].generation = $null },
        { param($r) $r.mutations[0].generation = $null },
        { param($r) $r.mutations[0].generation = '2' },
        { param($r) $r.mutations[1].generation = 2 },
        { param($r) $r.mutations[1].index = 0 },
        { param($r) $r.mutations[1].kind = 'revocation' },
        { param($r) $r.mutations[0].behavior = $null },
        { param($r) $r.mutations[1].behavior.affected_status = 200 },
        { param($r) $r.mutations[1].behavior.unaffected_semantic_match = 'true' },
        { param($r) $r.mutations[1].behavior.unaffected_semantic_match = $false },
        { param($r) $r.mutations[1].behavior.old_generation_envelope = 'accepted' },
        { param($r) $r.mutations[1].behavior.scope = 'per_replica_coverage' },
        { param($r) $r.mutations[1].observation_deadline_ms = 30000 }
    )) {
        Reset-Mock 'MEAS-005'
        $report = New-MutationReport
        & $mutationTamper $report
        $script:DriverLog = New-Frame (Report $report)
        $result = Invoke-Live 'MEAS-005'
        Assert ($result.Summary.failedStep -ceq 'ReadDriver' -and $result.Summary.failureReason -ceq 'MutationEvidence') 'Invalid successful mutation evidence must fail closed'
        Assert (-not (Test-Path (Join-Path $result.Out 'MEAS-005-mutations.json')) -and
            @($result.Summary.evidence | Where-Object kind -ceq 'mutations').Count -eq 0) 'Rejected mutation evidence must not be saved'
        Assert (-not ($script:LocalCalls | Where-Object { $_.Operation -cin @('analyze', 'check_driver_clock') })) 'Rejected mutation evidence must not reach offline consumers'
        Assert-Cleaned $result 'invalid-mutations'
    }

    # A failed driver can preserve a valid prefix, including no attempted mutations, without claiming completion.
    foreach ($partial in @(
        @(),
        @([ordered]@{ index = 0; kind = 'revocation'; outcome = 'unknown_do_not_retry'; generation = $null }),
        @([ordered]@{ index = 0; kind = 'revocation'; outcome = 'not_attempted_signing_read_failed'; generation = $null }),
        @((New-MutationReport).mutations[0]),
        @([ordered]@{ index = 0; kind = 'revocation'; outcome = 'acknowledged'; generation = 2
            behavior = $null; observation_deadline_ms = 1000 })
    )) {
        Reset-Mock 'MEAS-005'
        $script:JobStatus['vlite-meas-t26-drv'] = 'Failed'
        $report = New-MutationReport
        $report.mutation_and_ingress_checks_complete = $false
        $report.mutations = @($partial)
        $script:MutationJson = Report $report
        $script:DriverLog = New-Frame $script:MutationJson
        if ($partial.Count) { $script:ObserverMissing = 'vlite-meas-t26-gw--r1-x2' }
        $result = Invoke-Live 'MEAS-005'
        Assert (-not $result.Error -and $result.Summary.measurementStatus -ceq 'Incomplete') 'Incomplete mutation evidence is recorded, not failed'
        $reasons = @('MutationChecksIncomplete', 'AnalysisNotRun')
        if ($partial.Count) { $reasons += 'ObserverReportMissing' }
        foreach ($reason in $reasons) {
            Assert ($result.Summary.incompleteReasons -contains $reason) "Missing incomplete reason $reason"
        }
        Assert ([IO.File]::ReadAllText((Join-Path $result.Out 'MEAS-005-mutations.json')) -ceq $script:MutationJson) 'Partial mutation evidence is preserved unchanged'
        Assert (-not ($script:LocalCalls | Where-Object { $_.Operation -ceq 'analyze' })) 'Analysis must not run without mutations or with missing observers'
    }

    Reset-Mock 'MEAS-005'
    $script:JobStatus['vlite-meas-t26-drv'] = 'Failed'  # status disagrees with complete mutation checks
    $result = Invoke-Live 'MEAS-005'
    Assert ($result.Error -ceq 'MEAS-005 failed at ReadDriver; diagnostics withheld.' -and $result.Summary.failureReason -ceq 'MutationEvidence') 'Inconsistent mutation evidence fails closed'
    Assert-Cleaned $result 'mutation-mismatch'

    Reset-Mock 'MEAS-004'
    $script:AfterReplicaSuffix = 'y'
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Summary.measurementStatus -ceq 'Incomplete' -and $result.Summary.incompleteReasons -contains 'InventoryChanged' -and
        -not $result.Summary.inventory.fixed) 'Replacement replicas make the trial incomplete'

    foreach ($lineageValue in 1uL, 9223372036854775807uL, 9223372036854775808uL, [ulong]::MaxValue) {
        Reset-Mock 'MEAS-004'
        $script:Tamper = {
            param($s, $p)
            if ($p -ceq 'pre') {
                foreach ($role in $s.metadata.Keys) { $s.metadata[$role].lineage = $lineageValue }
            }
        }.GetNewClosure()
        $result = Invoke-Live 'MEAS-004'
        Assert (-not $result.Error -and $result.Summary.measurementStatus -ceq 'Collected') "Valid u64 lineage $lineageValue was refused: $($result.Error)"
        Assert-Cleaned $result 'unsigned-lineage'
    }

    # ---- Fixture lineage/path/digest/role/expiry refusals. ----
    Assert-FixtureRefusal 'export-sha' { param($s, $p) if ($p -ceq 'post') { $s.summary.exports.gateway.sha256 = 'e' * 64 } } 'FixtureExport'
    Assert-FixtureRefusal 'export-path' { param($s, $p) if ($p -ceq 'post') { $s.summary.exports.writer.file = '/tmp/writer.json' } } 'FixtureSummary'
    Assert-FixtureRefusal 'bundle-role' { param($s, $p) if ($p -ceq 'pre') { $s.bundles.writer.role = 'reader' } } 'FixtureExport'
    Assert-FixtureRefusal 'role-file' { param($s, $p) if ($p -ceq 'pre') { $s.files.writer['admin/credentials.json'] = 'eA==' } } 'FixtureRole'
    Assert-FixtureRefusal 'database-file' { param($s, $p) if ($p -ceq 'pre') { $s.files.reader['database.vlite'] = 'eA==' } } 'FixtureRole'
    Assert-FixtureRefusal 'generation' { param($s, $p) if ($p -ceq 'pre') { $s.bundles.gateway.generation = 'other' } } 'FixtureExport'
    Assert-FixtureRefusal 'export-generation' { param($s, $p) if ($p -ceq 'post') { $s.exports.reader.generation = 'other' } } 'FixtureExport'
    Assert-FixtureRefusal 'projection-name' { param($s, $p) if ($p -ceq 'post') { $s.exports.driver.secrets[0].name = 'm-driver-t26-9' } } 'FixtureExport'
    Assert-FixtureRefusal 'source' { param($s, $p) if ($p -ceq 'pre') { $s.bundles.driver.source = [ordered]@{ revision = 'b' * 40; dirty_paths = @() } } } 'FixtureExport'
    Assert-FixtureRefusal 'fixture-digest' { param($s, $p) if ($p -ceq 'pre') { $s.metadata.writer.fixture_sha256 = 'd' * 64 } } 'FixtureLineage'
    Assert-FixtureRefusal 'lineage' { param($s, $p) if ($p -ceq 'pre') { $s.metadata.reader.lineage = 8 } } 'FixtureLineage'
    Assert-FixtureRefusal 'adjacent-unsigned-lineages' {
        param($s, $p)
        if ($p -ceq 'pre') {
            foreach ($role in $s.metadata.Keys) { $s.metadata[$role].lineage = [ulong]::MaxValue }
            $s.metadata.reader.lineage = 18446744073709551614uL
        }
    } 'FixtureLineage'
    foreach ($lineageValue in @($null, $true, '7', 1.5, 0, -1, [bigint]::Parse('18446744073709551616'))) {
        Assert-FixtureRefusal 'invalid-lineage' {
            param($s, $p)
            if ($p -ceq 'pre') {
                foreach ($role in $s.metadata.Keys) { $s.metadata[$role].lineage = $lineageValue }
            }
        }.GetNewClosure() 'FixtureLineage'
    }
    Assert-FixtureRefusal 'expiry-late' { param($s, $p) if ($p -ceq 'post') { $s.summary.credentials_expire_not_before_unix_ms += 1 } } 'FixtureExpiry'
    Assert-FixtureRefusal 'expiry-early' { param($s, $p) if ($p -ceq 'post') { $s.summary.credentials_expire_not_before_unix_ms -= 5000 } } 'FixtureExpiry'
    Assert-FixtureRefusal 'planned-build' { param($s, $p) if ($p -ceq 'post') { $s.summary.image.planned_build[4] = 'otherregistry' } } 'FixtureBuild'
    Assert-FixtureRefusal 'base-image' { param($s, $p) if ($p -ceq 'post') { $s.summary.image.base_image = 'vlitemeast26.azurecr.io/other@sha256:' + ('b' * 64) } } 'FixtureBuild'
    Assert-FixtureRefusal 'executed' { param($s, $p) if ($p -ceq 'post') { $s.summary.executed = @('az acr build') } } 'FixtureSummary'
    Assert-FixtureRefusal 'extra-key' { param($s, $p) if ($p -ceq 'post') { $s.summary.extra = 1 } } 'FixtureSummary'
    Assert-FixtureRefusal 'summary-source' { param($s, $p) if ($p -ceq 'post') { $s.summary.source.dirty_paths = @('M x') } } 'FixtureSummary'
    Assert-FixtureRefusal 'two-lines' $null 'FixtureSummary' { $script:SummaryLines = { param($f) @('{}', ($f.summary | ConvertTo-Json -Depth 20 -Compress)) } }

    # ---- Dirty provenance flows through the fixture verification exactly. ----
    Reset-Mock 'MEAS-004' @(' M deploy/x.ps1', '?? new file.txt')
    $result = Invoke-Live 'MEAS-004' @{ AllowDirty = $true }
    Assert (-not $result.Error -and $result.Summary.dirty) "Dirty provenance run failed: $($result.Error)"
    $env = $script:Manifests['vlite-meas-t26-gw'].properties.template.containers[0].env
    Assert (($env | Where-Object name -ceq 'VLITE_RESOURCE_SOURCE_DIRTY_PATHS').value -ceq " M deploy/x.ps1`n?? new file.txt") 'Dirty paths env differs'

    # ---- Credential window is a hard bound. ----
    foreach ($elapsed in 782700L, 782701L) {
        Reset-Mock 'MEAS-005'
        $script:OnCall = { param($line)
            if ($line -like 'containerapp job create --name vlite-meas-t26-drv*') { $script:Now += $elapsed }
        }
        $result = Invoke-Live 'MEAS-005'
        if ($elapsed -eq 782700L) {
            Assert (-not $result.Error) 'Exact conservative pilot observer window should pass'
        } else {
            Assert ($result.Summary.failureReason -ceq 'ObserverWindow' -and
                $result.Summary.failedStep -ceq 'StartDriver') 'Insufficient observer lifetime must stop before mutation'
            Assert (-not ($script:Calls | Where-Object { $_ -like 'containerapp job start --name vlite-meas-t26-drv*' })) 'Driver started after conservative observer window elapsed'
        }
        Assert-Cleaned $result 'observer-window'
    }

    Reset-Mock 'MEAS-004'
    $script:OnCall = { param($line) if ($line -like 'acr build*gateway-measure-fixture*') { $script:Now += 7100000 } }
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Error -ceq 'MEAS-004 failed at CreatePublishJob; diagnostics withheld.' -and $result.Summary.failureReason -ceq 'CredentialWindow') 'Expiry margin refuses publication'
    Assert (-not ($script:Calls | Where-Object { $_ -like 'containerapp job create*' })) 'No job created past the credential window'
    Assert-Cleaned $result 'credential-window'

    Reset-Mock 'MEAS-004'
    $script:OnCall = { param($line) if ($line -like 'containerapp replica list*') { $script:Now += 3500000 } }
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Summary.failureReason -ceq 'CredentialWindow' -and $result.Summary.failedStep -ceq 'StartDriver') 'Driver needs a full job timeout before expiry'
    Assert (-not ($script:Calls | Where-Object { $_ -like 'containerapp job start --name vlite-meas-t26-drv*' })) 'Driver not started past the window'

    # ---- Failure paths: static errors and cleanup always attempted. ----
    Reset-Mock 'MEAS-004'
    $script:PrepareExit = 1
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Error -ceq 'MEAS-004 failed at PrepareFixture; diagnostics withheld.' -and $result.Summary.failureReason -ceq 'FixturePrepare') 'Prepare failure static'
    Assert ($result.Summary.fixtureCleanup -ceq 'Removed') 'Prepare failure still cleans fixture'
    Assert-Cleaned $result 'prepare-failure'

    Reset-Mock 'MEAS-004'
    $script:RootEmpty = $false
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Error -match '/measurement fixture root must' -and -not $result.Summary) 'Non-empty root refused before any work'
    Assert (-not $script:LocalCalls.Count -and -not ($script:Calls | Where-Object { $_ -like 'group create*' -or $_ -like 'group delete*' })) 'Foreign root never touched or cleaned'

    Reset-Mock 'MEAS-004'
    $script:RootEmptySequence.AddRange([bool[]]@($true, $false))
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Error -ceq 'MEAS-004 failed at PrepareFixture; diagnostics withheld.' -and $result.Summary.failureReason -ceq 'FixtureRoot') 'Root populated during provisioning is refused before prepare'
    Assert (-not $script:LocalCalls.Count -and $result.Summary.fixtureCleanup -cne 'Removed') 'Late foreign root is neither prepared nor cleaned'
    Assert-Cleaned $result 'late-root'

    Reset-Mock 'MEAS-004'
    $script:GroupExists = $true
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Error -match 'already exists' -and -not ($script:Calls | Where-Object { $_ -like 'group delete*' })) 'Existing group is never reused or deleted'

    Reset-Mock 'MEAS-004'
    $script:JobStatus['vlite-meas-t26-pub'] = 'Failed'
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Summary.failureReason -ceq 'PublishReceipt' -and $result.Summary.failedStep -ceq 'ReadPublish') 'Publish failure detected'
    Assert (@($script:Calls | Where-Object { $_ -like 'containerapp job start*' }).Count -eq 1) 'Publish is never retried'
    Assert (-not ($script:Calls | Where-Object { $_ -like 'containerapp create*' })) 'No apps after failed publish'
    Assert-Cleaned $result 'publish-failure'

    # Evidence that appears in Log Analytics late is still collected, and a read that never succeeds
    # is bounded and leaves a static diagnostic (no raw errors or log text) in the failed summary.
    Assert (($script:VLiteMeasLogAttempts - 1) * $script:VLiteMeasLogIntervalSecs -ge 600) 'Log Analytics wait must cover documented ingestion delay'
    Reset-Mock 'MEAS-004'
    $script:ConsoleDownPublish = $true
    $script:QueryReadyAfter = 30
    $result = Invoke-Live 'MEAS-004'
    Assert (-not $result.Error -and $result.Summary.status -ceq 'Succeeded' -and $null -eq $result.Summary.failureDiagnostic) "Late Log Analytics evidence not collected: $($result.Error)"
    Assert (@($script:Calls | Where-Object { $_ -like 'monitor log-analytics query*' }).Count -eq 31) 'Collector must stop polling once evidence arrives'
    Assert-Cleaned $result 'late-evidence'

    Reset-Mock 'MEAS-004'
    $script:ConsoleDownPublish = $true
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Summary.failedStep -ceq 'ReadPublish' -and $result.Summary.failureReason -ceq 'PublishEvidence') 'Unavailable publish evidence fails the read step'
    $diagnostic = $result.Summary.failureDiagnostic
    Assert (($diagnostic.Keys | Sort-Object) -join ',' -ceq 'category,consoleEvidence,jobExitCode,logAnalyticsAttempts,logAnalyticsEvidence,reason') 'Failure diagnostic shape differs'
    Assert ($diagnostic.category -ceq 'EvidenceUnavailable' -and $diagnostic.reason -ceq 'RetrievalFailed' -and $null -eq $diagnostic.jobExitCode -and
        $diagnostic.consoleEvidence -ceq 'Unavailable' -and $diagnostic.logAnalyticsEvidence -ceq 'Unavailable') 'Failure diagnostic codes differ'
    Assert ($diagnostic.logAnalyticsAttempts -eq $script:VLiteMeasLogAttempts -and $script:QueryCount -eq $script:VLiteMeasLogAttempts) 'Log Analytics polling must be bounded by the attempt limit'
    Assert (-not ($script:Calls | Where-Object { $_ -like 'containerapp job start --name vlite-meas-t26-drv*' })) 'Driver must not start without publish evidence'
    Assert (-not $result.SummaryText.Contains('no rows') -and -not $result.SummaryText.Contains('console unavailable')) 'Diagnostic leaked raw error text'
    Assert-Cleaned $result 'evidence-unavailable'

    Reset-Mock 'MEAS-004'
    $script:PublishLog = @('VLITE-MEAS FAILED 7')
    $result = Invoke-Live 'MEAS-004'
    $diagnostic = $result.Summary.failureDiagnostic
    Assert ($result.Summary.failureReason -ceq 'PublishEvidence' -and $diagnostic.category -ceq 'JobFailed' -and $diagnostic.jobExitCode -eq 7 -and
        $diagnostic.consoleEvidence -ceq 'JobFailed' -and $diagnostic.logAnalyticsAttempts -eq 0) 'Probe failure marker is reported as JobFailed with its exit code'
    Assert-Cleaned $result 'publish-marker'

    Reset-Mock 'MEAS-004'
    $script:DriverLog = New-Frame (Report ([ordered]@{ schema = 'vlite-meas004/1'; order = 'ABBA'; windows = @(@{ note = 'client-driver-1a2b3c' }) }))
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Summary.failureReason -ceq 'PrivateIdentifier' -and -not (Test-Path (Join-Path $result.Out 'MEAS-004-load.json'))) 'Evidence with private identifiers is refused'

    Reset-Mock 'MEAS-004'
    $script:DeleteFails = $true
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Error -ceq 'MEASUREMENT CLEANUP FAILED: delete tagged resource group vlite-ha-meas-t26 manually.') 'Delete failure reported separately'
    Assert ($result.Summary.cleanupStatus -ceq 'Failed' -and $result.Summary.cleanupFailedStep -ceq 'DeleteGroup' -and
        $result.Summary.status -ceq 'Succeeded') 'Delete failure recorded separately from measurement'

    Reset-Mock 'MEAS-004'
    $script:FixtureCleanupExit = 1
    $result = Invoke-Live 'MEAS-004'
    Assert ($result.Error -match 'FIXTURE CLEANUP FAILED' -and $result.Summary.fixtureCleanup -ceq 'Failed') 'Fixture cleanup failure is loud'
    Assert ($script:Calls -contains "group delete --name vlite-ha-meas-t26 --yes --no-wait --subscription $subscription") 'Group still deleted'

    Write-Output 'Gateway measurement orchestration offline tests passed.'
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
}
