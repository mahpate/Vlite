#Requires -Version 7.2
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'deploy.ps1')

function Assert($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Assert-Throws([scriptblock]$Action, [string]$Pattern) {
    $failure = $null
    try { & $Action | Out-Null } catch { $failure = $_.Exception.Message }
    Assert ($failure -and $failure -match $Pattern) "Expected refusal /$Pattern/; received: $failure"
}

function Assert-Keys($Value, [string[]]$Expected, [string]$Label) {
    Assert ((@($Value.Keys | Sort-Object -CaseSensitive) -join ',') -ceq
        (@($Expected | Sort-Object -CaseSensitive) -join ',')) "$Label field set differs."
}

function Get-RustFields([string]$File, [string]$Struct) {
    $source = Get-Content -Raw -LiteralPath $File
    $body = [regex]::Match($source, "(?s)struct $Struct \{(.*?)\n\}").Groups[1].Value
    Assert ($body.Length -gt 0) "Missing Rust schema $Struct."
    @([regex]::Matches($body, '(?m)^\s+([a-z_]+):') | ForEach-Object { $_.Groups[1].Value })
}

$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..' '..' '..')).Path
function Get-RustSource([string[]]$Parts) {
    $path = $repo
    foreach ($part in $Parts) { $path = Join-Path $path $part }
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Rust schema source not found: $($Parts -join '/'); run from a full checkout"
    }
    $path
}

$config = @{
    EnableHa = $true; AllowUnqualifiedWriterStorage = $true; EnableReaders = $true
    SubscriptionId = 'test-sub'; ResourceGroup = 'test-rg'; Location = 'test-region'
    DeploymentId = 'test-ha'; Environment = 'test-ha-env'
    SubnetId = '/subscriptions/test-sub/resourceGroups/test-rg/providers/Microsoft.Network/virtualNetworks/test-vnet/subnets/test-subnet'
    GatewayName = 'test-gateway'; WriterName = 'test-writer'; ReaderName = 'test-reader'
    StorageAccount = 'teststorage'; ShareName = 'test-ha-share'; WriterStorageName = 'test-ha-storage'
    Image = ('example.invalid/vlite@sha256:' + ('a' * 64)); WorkloadProfileName = 'Consumption'
    GatewayMinReplicas = 2; GatewayMaxReplicas = 4; GatewayHttpConcurrency = 5
    ReaderMinReplicas = 1; ReaderMaxReplicas = 3; ReaderHttpConcurrency = 5
    ReaderCapacityBytes = 1048576; ReaderStalenessBoundMs = 10000; ReaderPollIntervalMs = 100
    SCtlMs = 5000; PollIntervalMs = 100; EnvelopeLifetimeMs = 1000; MaxLifetimeMs = 1000
    SkewMs = 100; ReadinessPropagationMs = 1000; DrainTimeoutMs = 1000; SafetyMarginMs = 1000
    GatewayTerminationGracePeriodSeconds = 4; WriterTerminationGracePeriodSeconds = 4
    ReaderTerminationGracePeriodSeconds = 4
    GatewayStartupBudgetSeconds = 100; WriterStartupBudgetSeconds = 100; ReaderStartupBudgetSeconds = 100
    WriterStopBudgetMs = 1000; ReaderStopBudgetMs = 1000
    MaxRequestBytes = 1024; MaxInFlight = 10; UpstreamTimeoutMs = 1000; MaxBuckets = 10
    SecretGeneration = 7; ActiveKid = 'test-key'; WriterAudience = 'test-writer'; ReaderAudience = 'test-reader'
    TimeoutSeconds = 1
    ControlTargetFile = '/run/vlite/control-target.json'; ControlObject = 'control.json'
    SigningKeyFile = '/run/vlite/signing-key'; PublicKeyBundleFile = '/run/vlite/public-keys.json'
    ReaderTargetFile = '/run/vlite/archive-target.json'; FollowerDir = '/data'
}
$groupId = '/subscriptions/test-sub/resourceGroups/test-rg'
$environmentId = "$groupId/providers/Microsoft.App/managedEnvironments/test-ha-env"
foreach ($role in @('Gateway', 'Writer', 'Reader')) {
    $config["${role}IdentityId"] = "$groupId/providers/Microsoft.ManagedIdentity/userAssignedIdentities/test-$($role.ToLowerInvariant())"
    $config["${role}Cpu"] = 0.25
    $config["${role}Memory"] = '0.5Gi'
}
$config.ControlTarget = @{
    account = 'teststorage'; container = 'test-control'; coordination_container = 'test-control-coordination'
    prefix = 'test/control/'; auth = 'managed_identity'; client_id = 'test-gateway-client'
}
$config.ArchiveTarget = @{
    account = 'teststorage'; container = 'test-data'; coordination_container = 'test-coordination'
    prefix = 'test/tenant/'; auth = 'managed_identity'; client_id = 'test-writer-client'
}

$script:calls = [Collections.Generic.List[string]]::new()
$script:argv = [Collections.Generic.List[object]]::new()
$script:allArgv = [Collections.Generic.List[object]]::new()
$script:allSecretValues = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$script:handlers = @{}
$script:unexpected = [Collections.Generic.List[string]]::new()
function Add-Az([string[]]$Arguments, [scriptblock]$Handler) {
    $script:handlers[$Arguments -join [char]31] = $Handler
}
function Invoke-VLiteAz([string[]]$Arguments) {
    $script:calls.Add(($Arguments -join ' '))
    $script:argv.Add($Arguments.Clone())
    $script:allArgv.Add($Arguments.Clone())
    $lookup = $Arguments.Clone()
    if ($lookup.Count -eq 10 -and $lookup[0] -ceq 'containerapp' -and
        $lookup[1] -cin @('create', 'update') -and $lookup[8] -ceq '--yaml') {
        Assert (Test-Path -LiteralPath $lookup[9] -PathType Leaf) 'Manifest missing at apply.'
        $lookup[9] = '<manifest>'
    }
    $key = $lookup -join [char]31
    if (-not $script:handlers.ContainsKey($key)) {
        $script:unexpected.Add(($Arguments -join ' '))
        throw "Unexpected Azure operation: $($Arguments -join ' ')"
    }
    $result = & $script:handlers[$key] $Arguments
    if ($null -ne $result) { return ,$result }
}
# Any accidental direct native invocation is also blocked.
function az {
    $script:unexpected.Add('Direct native Azure CLI invocation')
    throw 'Real Azure CLI is forbidden in these tests.'
}

function New-TestSecrets([string]$Role, [hashtable]$Candidate = $config) {
    $g = "g$($Candidate.SecretGeneration)"
    $secrets = @{ "verify-$g" = '{"test_only":"public-bundle"}'; "control-target-$g" = '{"test_only":"control"}' }
    if ($Role -eq 'Gateway') {
        $secrets["gw-seed-$g"] = 'TEST-ONLY-PRIVATE-SEED'
        $secrets["gateway-config-$g"] = New-VLiteHaGatewayConfig $Candidate | ConvertTo-Json -Depth 30 -Compress
    } else {
        $trust = $Candidate.Clone()
        $trust.Audience = $Candidate["${Role}Audience"]
        $secrets["backend-trust-$g"] = New-VLiteHaTrustConfig $trust | ConvertTo-Json -Depth 30 -Compress
        if ($Role -eq 'Writer') {
            $secrets["archive-target-$g"] = $Candidate.ArchiveTarget | ConvertTo-Json -Depth 30 -Compress
            $secrets["archive-policy-$g"] = New-VLiteHaWriterArchivePolicy $Candidate | ConvertTo-Json -Depth 30 -Compress
        } elseif ($Role -eq 'Reader') {
            $secrets["archive-target-$g"] = '{"test_only":"archive"}'
            $secrets["reader-config-$g"] = New-VLiteHaReaderApplyConfig $Candidate | ConvertTo-Json -Depth 30 -Compress
        }
    }
    return $secrets
}

Assert (Test-VLiteHaParameters $config) 'Valid fixture rejected.'
$gatewayConfig = New-VLiteHaGatewayConfig $config
$trustConfig = $config.Clone(); $trustConfig.Audience = $config.WriterAudience
$trust = New-VLiteHaTrustConfig $trustConfig
Assert-Keys $gatewayConfig (Get-RustFields (Get-RustSource @('src', 'server', 'gateway_mode.rs')) 'GatewayFile') 'Gateway Rust'
Assert-Keys $trust (Get-RustFields (Get-RustSource @('src', 'server', 'gateway_trust.rs')) 'TrustFile') 'Trust Rust'
Assert-Keys (New-VLiteHaReaderApplyConfig $config) @(
    'target_file', 'follower_dir', 'capacity_bytes', 'staleness_bound_ms', 'poll_interval_ms'
) 'Reader apply configuration'
$overCapacity = $config.Clone(); $overCapacity.ReaderCapacityBytes = 1073741825
Assert-Throws { New-VLiteHaReaderApplyConfig $overCapacity } 'ReaderCapacityBytes'
$overBound = $config.Clone(); $overBound.ReaderStalenessBoundMs = 3600001
Assert-Throws { New-VLiteHaReaderApplyConfig $overBound } 'ReaderStalenessBoundMs'

$templates = @{}
foreach ($role in @('Gateway', 'Writer', 'Reader')) {
    $templates[$role] = & "New-VLiteHa${role}Template" $config $environmentId (New-TestSecrets $role) 'g7-test'
}
$gateway = $templates.Gateway
$writer = $templates.Writer
$reader = $templates.Reader
Assert ($gateway.properties.template.scale.minReplicas -ge 2 -and
    $gateway.properties.template.scale.maxReplicas -eq $gatewayConfig.max_replicas) 'Gateway scale/config mismatch.'
Assert ($gateway.properties.template.volumes.Count -eq 1 -and
    $gateway.properties.template.volumes[0].storageType -ceq 'Secret') 'Gateway has a data volume.'
Assert ($gateway.properties.configuration.ingress.external -and
    -not $gateway.properties.configuration.ingress.allowInsecure) 'Gateway ingress is not HTTPS-only external.'
Assert (($gateway.properties.template.containers[0].args -join ' ') -ceq
    'gateway --config /run/vlite/gateway.json') 'Gateway invocation changed.'
Assert ($writer.properties.template.scale.minReplicas -eq 1 -and
    $writer.properties.template.scale.maxReplicas -eq 1 -and
    $writer.properties.template.scale.rules.Count -eq 0) 'Writer can scale out.'
Assert ($writer.properties.template.containers[0].args -notcontains '--create-if-missing') 'Restart initializes a database.'
Assert (($writer.properties.template.containers[0].args -join ' ') -match '--durability full') 'Writer lost full durability.'
foreach ($flag in @('--authority-target-file', '--archive-policy-file')) {
    Assert (@($writer.properties.template.containers[0].args | Where-Object { $_ -ceq $flag }).Count -eq 1) "Writer needs exactly one $flag."
}
Assert (($writer.properties.template.containers[0].args -join ' ') -match
    '--authority-target-file /run/vlite/archive-target.json --archive-policy-file /run/vlite/archive-policy.json') 'Writer archive arguments missing.'
$writerPolicy = New-VLiteHaWriterArchivePolicy $config
Assert ($writerPolicy.target.client_id -ceq $config.ArchiveTarget.client_id) 'Writer policy lost identity.'
Assert-VLiteHaConfigDocument -Json @($writer.properties.configuration.secrets |
    Where-Object { $_.name -ceq 'archive-policy-g7' })[0].value `
    -Expected $writerPolicy -Label 'Mounted writer policy'
$wrongPolicySecrets = New-TestSecrets 'Writer'
$wrongPolicy = New-VLiteHaWriterArchivePolicy $config
$wrongPolicy.target.client_id = 'wrong-writer-client'
$wrongPolicySecrets['archive-policy-g7'] = $wrongPolicy | ConvertTo-Json -Depth 30 -Compress
Assert-Throws {
    New-VLiteHaWriterTemplate $config $environmentId $wrongPolicySecrets 'g7-test'
} 'must match the validated Config value'
foreach ($case in @(
    @('WriterBackupIntervalSecs', 299), @('WriterBackupKeepSets', 2),
    @('WriterBackupKeepDays', 29), @('WriterBackupOrphanGraceHours', 0),
    @('WriterBackupDrillEvery', 101), @('WriterArchiveIntervalSecs', 59),
    @('WriterBackupIntervalSecs', 604801), @('WriterBackupKeepSets', 1001),
    @('WriterBackupKeepDays', 3651), @('WriterBackupOrphanGraceHours', 721),
    @('WriterBackupDrillEvery', -1), @('WriterArchiveIntervalSecs', 86401)
)) {
    $invalidPolicy = $config.Clone(); $invalidPolicy[$case[0]] = $case[1]
    Assert-Throws { New-VLiteHaWriterArchivePolicy $invalidPolicy } 'must be an integer'
}
Assert (@($writer.properties.template.volumes | Where-Object {
    $_.storageType -ceq 'AzureFile' -and $_.storageName -ceq $config.WriterStorageName
}).Count -eq 1) 'Writer share binding missing.'
$init = $config.Clone(); $init.InitializeTenant = $true; $init.Dimensions = 8; $init.CreateFormat = 4
$initWriter = New-VLiteHaWriterTemplate $init $environmentId (New-TestSecrets 'Writer' $init) 'g7-init'
Assert (($initWriter.properties.template.containers[0].args -join ' ') -match
    '--create-if-missing 8 --create-format 4$') 'Explicit initialization flags missing.'
$format3 = $init.Clone(); $format3.CreateFormat = 3
Assert-Throws { New-VLiteHaWriterTemplate $format3 $environmentId (New-TestSecrets 'Writer' $format3) 'g7-init' } 'CreateFormat'
Assert (@($reader.properties.template.volumes | Where-Object { $_.storageType -ceq 'EmptyDir' }).Count -eq 1) 'Reader scratch is not private EmptyDir.'
Assert (($reader | ConvertTo-Json -Depth 30) -notmatch 'AzureFile|test-ha-storage|test-ha-share') 'Reader can mount writer storage.'
$readerArgs = $reader.properties.template.containers[0].args -join ' '
Assert ($reader.properties.template.containers[0].args[0] -ceq '/data/reader-a/database.vlite') 'Reader positional path is not slot A.'
Assert ($readerArgs -match '--reader-apply-file /run/vlite/reader.json$') 'Reader apply contract missing.'
Assert ($readerArgs -notmatch '--(create|durability|group-commit|admin|grpc|ann-snapshot)') 'Reader has prohibited flags.'
foreach ($role in @('Gateway', 'Writer', 'Reader')) {
    $template = $templates[$role]
    $props = $template.properties
    Assert ($props.configuration.activeRevisionsMode -ceq 'Single') "$role is not Single revision mode."
    Assert ($props.configuration.ingress.stickySessions.affinity -ceq 'none') "$role uses sticky sessions."
    Assert ($props.configuration.ingress.additionalPortMappings.Count -eq 0) "$role exposes extra ports."
    Assert ($props.configuration.ingress.transport -ceq 'http' -and
        $props.configuration.ingress.targetPort -eq 8080) "$role ingress protocol differs."
    if ($role -ne 'Gateway') { Assert (-not $props.configuration.ingress.external) 'Backend is public.' }
    Assert ($template.tags.vliteHaRole -ceq $role.ToLowerInvariant() -and
        $template.tags.vliteHaDeployment -ceq $config.DeploymentId -and
        $template.tags.vliteHaProduction -ceq 'false') "$role ownership/evaluation tags missing."
    Assert ($props.template.terminationGracePeriodSeconds -eq $config["${role}TerminationGracePeriodSeconds"]) "$role grace ignored."
    Assert ($props.template.containers[0].env.Count -eq 0) "$role has credential/admin environment variables."
    Assert (($template | ConvertTo-Json -Depth 30) -notmatch 'admin-token|VLITE_ADMIN_DIR|VLITE_AUTH_TOKEN') "$role contains admin credentials."
    $projection = @($props.template.volumes | Where-Object { $_.storageType -ceq 'Secret' })[0]
    $expectedPaths = @{
        'verify-g7' = 'public-keys.json'; 'control-target-g7' = 'control-target.json'
    }
    if ($role -eq 'Gateway') {
        $expectedPaths['gw-seed-g7'] = 'signing-key'
        $expectedPaths['gateway-config-g7'] = 'gateway.json'
    } else {
        $expectedPaths['backend-trust-g7'] = 'trust.json'
        if ($role -eq 'Writer') {
            $expectedPaths['archive-target-g7'] = 'archive-target.json'
            $expectedPaths['archive-policy-g7'] = 'archive-policy.json'
        } elseif ($role -eq 'Reader') {
            $expectedPaths['archive-target-g7'] = 'archive-target.json'
            $expectedPaths['reader-config-g7'] = 'reader.json'
        }
    }
    Assert-Keys (@{} + $expectedPaths) @($props.configuration.secrets.name) "$role role-specific secret allowlist"
    Assert ($projection.secrets.Count -eq $props.configuration.secrets.Count) "$role projects all/unmatched secrets."
    foreach ($item in $projection.secrets) {
        Assert-Keys $item @('secretRef', 'path') "$role secret projection"
        Assert (@($props.configuration.secrets | Where-Object { $_.name -ceq $item.secretRef }).Count -eq 1) 'Unknown secretRef.'
        Assert ($item.secretRef -match '-g7$' -and $item.path -notmatch '[/\\]') 'Projection generation/path changed.'
        Assert ($item.path -ceq $expectedPaths[$item.secretRef]) "$role secret maps to the wrong file."
    }
    $probes = $props.template.containers[0].probes
    foreach ($type in @('Startup', 'Readiness', 'Liveness')) {
        $probe = @($probes | Where-Object { $_.type -ceq $type })
        $path = if ($type -eq 'Liveness') { '/health' } else { '/ready' }
        Assert ($probe.Count -eq 1 -and $probe[0].httpGet.path -ceq $path -and
            $probe[0].httpGet.port -eq 8080 -and $probe[0].successThreshold -eq 1) "$role $type probe differs."
    }
}
Assert ($gateway.properties.template.scale.rules[0].http.metadata.concurrentRequests -ceq '5') 'Gateway HTTP scaling missing.'
Assert ($reader.properties.template.scale.maxReplicas -eq 3 -and
    $reader.properties.template.scale.rules[0].http.metadata.concurrentRequests -ceq '5') 'Reader scaling missing.'

$refusals = @(
    @{ Key = 'EnableHa'; Value = $false; Error = 'opt-in' },
    @{ Key = 'Image'; Value = 'example.invalid/vlite:latest'; Error = 'sha256' },
    @{ Key = 'GatewayMinReplicas'; Value = 1; Error = 'replicas' },
    @{ Key = 'GatewayMaxReplicas'; Value = 1; Error = 'replicas' },
    @{ Key = 'GatewayMaxReplicas'; Value = 1001; Error = 'replicas' },
    @{ Key = 'GatewayTerminationGracePeriodSeconds'; Value = 3; Error = 'must exceed' },
    @{ Key = 'WriterTerminationGracePeriodSeconds'; Value = 3; Error = 'must exceed' },
    @{ Key = 'ReaderTerminationGracePeriodSeconds'; Value = 3; Error = 'must exceed' },
    @{ Key = 'WriterStopBudgetMs'; Value = 3000; Error = 'also cover' },
    @{ Key = 'PollIntervalMs'; Value = 5000; Error = 'below SCtlMs' },
    @{ Key = 'PollIntervalMs'; Value = 0; Error = 'positive integer' },
    @{ Key = 'ReaderPollIntervalMs'; Value = 5000; Error = 'below Config.SCtlMs' },
    @{ Key = 'AllowUnqualifiedWriterStorage'; Value = $false; Error = 'unqualified' },
    @{ Key = 'Rotate'; Value = $true; Error = 'Automatic rotation is unsupported' },
    @{ Key = 'ReadRouting'; Value = 'readers'; Error = 'writer_only' },
    @{ Key = 'Dimensions'; Value = 8; Error = 'require explicit' },
    @{ Key = 'ReaderSharedKeyFile'; Value = 'secret'; Error = 'managed identity' }
)
foreach ($key in @('Environment', 'GatewayName', 'WriterName', 'ReaderName', 'ShareName', 'WriterStorageName')) {
    foreach ($name in @('vlite-app', 'vlite-env', 'vlite-data')) {
        $refusals += @{ Key = $key; Value = $name; Error = 'singleton' }
    }
}
foreach ($key in @('GatewayMaxReplicas', 'ReaderMaxReplicas', 'SafetyMarginMs', 'SkewMs',
    'GatewayStartupBudgetSeconds', 'WriterStopBudgetMs')) {
    $refusals += @{ Key = $key; Remove = $true; Error = 'required|Required' }
}
foreach ($case in $refusals) {
    $candidate = $config.Clone()
    if ($case.ContainsKey('Remove')) { $candidate.Remove($case.Key) } else { $candidate[$case.Key] = $case.Value }
    $script:calls.Clear()
    Assert-Throws { Invoke-VLiteHaDeployment $candidate } $case.Error
    Assert ($script:calls.Count -eq 0) "Azure contacted before parameter refusal: $($case.Key)"
}
$format3 = $config.Clone(); $format3.InitializeTenant = $true; $format3.Dimensions = 8; $format3.CreateFormat = 3
$script:calls.Clear()
Assert-Throws { Invoke-VLiteHaDeployment $format3 } 'CreateFormat'
Assert ($script:calls.Count -eq 0) 'Azure contacted before create-format refusal.'
Assert-Throws { & (Join-Path $PSScriptRoot 'deploy.ps1') -Config $config } 'opt-in'
$badSecrets = New-TestSecrets 'Gateway'
$badGateway = $gatewayConfig.Clone(); $badGateway.max_replicas = 5
$badSecrets['gateway-config-g7'] = $badGateway | ConvertTo-Json -Depth 30
Assert-Throws { New-VLiteHaGatewayTemplate $config $environmentId $badSecrets 'g7-test' } 'must match'
$badSecrets = New-TestSecrets 'Gateway'; $badSecrets['admin-token'] = 'TEST-ONLY-ADMIN'
Assert-Throws { New-VLiteHaGatewayTemplate $config $environmentId $badSecrets 'g7-test' } 'extra secrets'
$disabled = $config.Clone(); $disabled.EnableReaders = $false
Assert-Throws { New-VLiteHaReaderTemplate $disabled $environmentId (New-TestSecrets 'Reader') 'g7-test' } 'EnableReaders'

function Add-WriterRevision([string]$Revision, [bool]$Active) {
    $script:writerRevisions.Add($Revision)
    $script:active[$Revision] = $Active
    Add-Az @('containerapp', 'revision', 'deactivate', '--name', 'test-writer', '--subscription', 'test-sub',
        '--resource-group', 'test-rg', '--revision', $Revision) {
        param($a)
        $script:active[$a[10]] = $false
    }
    Add-Az @('containerapp', 'replica', 'list', '--name', 'test-writer', '--subscription', 'test-sub',
        '--resource-group', 'test-rg', '--revision', $Revision) {
        param($a)
        $script:stopChecks[$a[10]] = $true
        if ($script:busy -or $script:active[$a[10]]) { return ,@(@{ name = 'still-running' }) }
        ,@()
    }
}

function Reset-Mock([switch]$FreshWriter) {
    $script:calls.Clear(); $script:argv.Clear(); $script:handlers = @{}
    $script:apps = @{}; $script:captured = [Collections.Generic.List[object]]::new()
    $script:paths = [Collections.Generic.List[string]]::new()
    $script:busy = $false; $script:failApply = $false; $script:unready = $false; $script:applyBudget = [int]::MaxValue
    $script:routes = @(); $script:stopChecks = @{}; $script:active = @{}
    $script:writerRevisions = [Collections.Generic.List[string]]::new()
    $script:qualificationCalls = 0; $script:postStopBlocked = $false
    Add-Az @('account', 'show') { @{ id = 'test-sub' } }
    Add-Az @('containerapp', 'env', 'show', '--subscription', 'test-sub', '--resource-group', 'test-rg', '--name', 'test-ha-env') {
        @{ id = $environmentId; location = 'test-region'; tags = @{ vliteHaDeployment = 'test-ha' }; properties = @{
            zoneRedundant = $true; provisioningState = 'Succeeded'
            vnetConfiguration = @{ infrastructureSubnetId = $config.SubnetId }
            workloadProfiles = @(@{ name = 'Consumption'; workloadProfileType = 'Consumption' })
        } }
    }
    Add-Az @('network', 'vnet', 'subnet', 'show', '--ids', $config.SubnetId, '--subscription', 'test-sub') {
        @{ id = $config.SubnetId; delegations = @(@{ serviceName = 'Microsoft.App/environments' }) }
    }
    Add-Az @('storage', 'account', 'show', '--subscription', 'test-sub', '--resource-group', 'test-rg', '--name', 'teststorage') {
        @{ id = "$groupId/providers/Microsoft.Storage/storageAccounts/teststorage"
            primaryLocation = 'test-region'; sku = @{ name = 'Standard_ZRS' }; enableHttpsTrafficOnly = $true }
    }
    Add-Az @('storage', 'share-rm', 'show', '--subscription', 'test-sub', '--resource-group', 'test-rg',
        '--storage-account', 'teststorage', '--name', 'test-ha-share') { @{ metadata = @{ vliteHaDeployment = 'test-ha' } } }
    Add-Az @('containerapp', 'env', 'storage', 'show', '--subscription', 'test-sub', '--resource-group', 'test-rg',
        '--name', 'test-ha-env', '--storage-name', 'test-ha-storage') {
        @{ properties = @{ azureFile = @{ accountName = 'teststorage'; shareName = 'test-ha-share'; accessMode = 'ReadWrite' } } }
    }
    foreach ($role in @('Gateway', 'Writer', 'Reader')) {
        Add-Az @('identity', 'show', '--ids', $config["${role}IdentityId"], '--subscription', 'test-sub') {
            param($a)
            $role = ($a[3] -split '-')[-1]
            @{ id = $a[3]; clientId = "test-$role-client"; principalId = "test-$role-principal" }
        }
    }
    foreach ($container in @('test-control', 'test-control-coordination', 'test-data', 'test-coordination')) {
        Add-Az @('storage', 'container', 'show', '--account-name', 'teststorage', '--name', $container,
            '--auth-mode', 'login', '--subscription', 'test-sub') { param($a) @{ name = $a[6] } }
    }
    Add-Az @('containerapp', 'list', '--subscription', 'test-sub', '--resource-group', 'test-rg') { ,@($script:apps.Values) }
    Add-Az @('containerapp', 'env', 'http-route-config', 'list', '--subscription', 'test-sub',
        '--resource-group', 'test-rg', '--name', 'test-ha-env') { ,$script:routes }
    foreach ($role in @('Gateway', 'Writer', 'Reader')) {
        foreach ($verb in @('create', 'update')) {
            Add-Az @('containerapp', $verb, '--subscription', 'test-sub', '--resource-group', 'test-rg',
                '--name', $config["${role}Name"], '--yaml', '<manifest>') {
                param($a)
                $path = $a[9]
                $script:paths.Add($path)
                $manifest = Get-Content -Raw -LiteralPath $path | ConvertFrom-Json -AsHashtable
                $script:captured.Add($manifest)
                if ($script:failApply -or $script:captured.Count -gt $script:applyBudget) { throw 'Simulated apply failure' }
                $script:apps[$a[7]] = $manifest
                @{}
            }
        }
        Add-Az @('containerapp', 'show', '--subscription', 'test-sub', '--resource-group', 'test-rg',
            '--name', $config["${role}Name"]) {
            param($a)
            $manifest = $script:apps[$a[7]]
            $revision = "$($a[7])--$($manifest.properties.template.revisionSuffix)"
            @{ properties = @{ latestRevisionName = $revision; latestReadyRevisionName = $revision } }
        }
    }
    # Readiness revision suffixes are generated dynamically; register exact argv
    # only after the manifest has supplied the suffix, never a wildcard handler.
    $script:applyHandlers = $script:handlers.Clone()
    foreach ($key in @($script:handlers.Keys)) {
        if ($key -match ('^containerapp' + [char]31 + '(create|update)' + [char]31)) {
            $script:handlers[$key] = {
                param($a)
                $lookup = $a.Clone(); $lookup[9] = '<manifest>'
                & $script:applyHandlers[$lookup -join [char]31] $a | Out-Null
                $manifest = $script:apps[$a[7]]
                foreach ($secret in $manifest.properties.configuration.secrets) {
                    $script:allSecretValues.Add($secret.value) | Out-Null
                }
                $revision = "$($a[7])--$($manifest.properties.template.revisionSuffix)"
                Add-Az @('containerapp', 'replica', 'list', '--subscription', 'test-sub', '--resource-group', 'test-rg',
                    '--name', $a[7], '--revision', $revision) {
                    param($argsForReplica)
                    $manifest = $script:apps[$argsForReplica[7]]
                    ,@(1..$manifest.properties.template.scale.minReplicas | ForEach-Object {
                        @{ properties = @{ containers = @(@{ name = 'vlite'; ready = (-not $script:unready); started = $true }) } }
                    })
                }
                if ($a[7] -ceq 'test-writer') { Add-WriterRevision $revision $true }
                @{}
            }
        }
    }
    Add-Az @('containerapp', 'revision', 'list', '--name', 'test-writer', '--subscription', 'test-sub', '--resource-group', 'test-rg') {
        ,@($script:writerRevisions | ForEach-Object {
            @{ name = $_; properties = @{ active = $script:active[$_] } }
        })
    }
    if (-not $FreshWriter) {
        foreach ($revision in @('old-a', 'old-b', 'old-inactive')) {
            Add-WriterRevision $revision ($revision -ne 'old-inactive')
        }
    }
}

function Assert-NoMutation {
    Assert (-not @($script:calls | Where-Object { $_ -match ' (create|update|set|deactivate|activate|delete|upload) ' }).Count) 'Mutation before refusal.'
}
function Assert-NoManifests {
    Assert (-not @(Get-ChildItem -LiteralPath $script:testRoot -Recurse -Filter 'manifest-*.json').Count) 'Private manifest retained.'
    foreach ($path in $script:paths) { Assert (-not (Test-Path -LiteralPath $path)) 'Submitted manifest retained.' }
}

$script:testRoot = Join-Path ([IO.Path]::GetTempPath()) "vlite-ha-tests-$([Guid]::NewGuid().ToString('N'))"
$realQualification = ${function:Assert-VLiteHaQualification}
function Get-VLiteHaStateRoot { Join-Path $script:testRoot 'state' }
try {
    Set-VLitePrivateDirectory $script:testRoot
    $script:seed = Join-Path $script:testRoot 'test-only-seed'
    $script:bundle = Join-Path $script:testRoot 'test-only-bundle'
    [IO.File]::WriteAllText($script:seed, 'TEST-ONLY-PRIVATE-SEED')
    [IO.File]::WriteAllText($script:bundle, '{"test_only":"public-bundle"}')
    $config.SigningKeySourceFile = $script:seed; $config.PublicKeyBundleSourceFile = $script:bundle

    Reset-Mock
    Add-Az @('account', 'show') { @{ id = 'wrong-subscription' } }
    Assert-Throws { Invoke-VLiteHaDeployment $config } 'does not match'
    Assert ($script:calls.Count -eq 1) 'Subscription mismatch queried more resources.'
    Assert-NoMutation
    Reset-Mock
    Add-Az @('containerapp', 'env', 'show', '--subscription', 'test-sub', '--resource-group', 'test-rg', '--name', 'test-ha-env') {
        @{ tags = @{ vliteHaDeployment = 'someone-else' } }
    }
    Assert-Throws { Invoke-VLiteHaDeployment $config } 'unowned'
    Assert-NoMutation
    Reset-Mock
    $script:apps['test-writer'] = @{ name = 'test-writer'; tags = @{ vliteHaDeployment = 'someone-else' } }
    Assert-Throws { Invoke-VLiteHaDeployment $config } 'unowned'
    Assert-NoMutation
    Reset-Mock
    Assert-Throws { Invoke-VLiteHaDeployment $config } 'HA deployment is blocked'
    Assert-NoMutation
    Assert (-not (Test-Path -LiteralPath (Get-VLiteHaStateRoot))) 'Blocked capabilities wrote local state.'
    Reset-Mock
    $writerOnly = $config.Clone(); $writerOnly.EnableReaders = $false
    foreach ($key in @($writerOnly.Keys | Where-Object { $_ -match '^Reader' })) { $writerOnly.Remove($key) }
    Assert-Throws { Invoke-VLiteHaDeployment $writerOnly } 'HA deployment is blocked'
    Assert-NoMutation
    Assert (-not @($script:calls | Where-Object { $_ -match 'test-reader' }).Count) 'Readers were enabled implicitly.'

    foreach ($case in @('public', 'insecure', 'ports', 'missing-ingress', 'wrong-environment', 'routes', 'null-routes', 'unknown-backend')) {
        Reset-Mock
        $script:apps['test-writer'] = New-VLiteHaWriterTemplate $config $environmentId (New-TestSecrets 'Writer') 'g7-test'
        $app = $script:apps['test-writer']
        switch ($case) {
            'public' { $app.properties.configuration.ingress.external = $true }
            'insecure' { $app.properties.configuration.ingress.allowInsecure = $true }
            'ports' { $app.properties.configuration.ingress.additionalPortMappings = @(@{ external = $true }) }
            'missing-ingress' { $app.properties.configuration.ingress = $null }
            'wrong-environment' { $app.properties.managedEnvironmentId = 'wrong' }
            'routes' { $script:routes = @(@{ name = 'route-to-old-writer-label' }) }
            'null-routes' { $script:routes = $null }
            'unknown-backend' {
                $script:apps['unknown'] = @{ name = 'unknown'; tags = @{ vliteHaDeployment = 'test-ha'; vliteHaRole = 'reader' } }
            }
        }
        Assert-Throws { Assert-VLiteHaIsolation $config $environmentId @('test-writer') } 'ingress|environment|routes|inventory|Unknown owned backend'
        Assert-NoMutation
    }

    # Only the synthetic orchestration cases bypass the deliberately unwired
    # operational verifier. This is not a production parameter or attestation.
    function Assert-VLiteHaQualification([hashtable]$Config) {
        $script:qualificationCalls++
        if ($script:postStopBlocked -and $script:qualificationCalls -gt 1) { throw 'Post-stop authority CAS refused' }
    }
    function Get-CallIndex([string]$Pattern) {
        for ($i = 0; $i -lt $script:calls.Count; $i++) { if ($script:calls[$i] -match $Pattern) { return $i } }
        -1
    }
    function Test-CreateFlags($Manifest) {
        $writerArgs = @($Manifest.properties.template.containers[0].args)
        ($writerArgs -contains '--create-if-missing') -or ($writerArgs -contains '--create-format')
    }
    $initial = $config.Clone(); $initial.InitializeTenant = $true; $initial.Dimensions = 8; $initial.CreateFormat = 4

    # If the flagless replacement cannot be applied, the run fails closed before
    # readers/gateway, and a rerun without InitializeTenant replaces the init writer.
    foreach ($case in @('post-stop', 'apply-failure')) {
        Reset-Mock -FreshWriter
        switch ($case) {
            'post-stop' { $script:postStopBlocked = $true }
            'apply-failure' { $script:applyBudget = 1 }
        }
        Assert-Throws { Invoke-VLiteHaDeployment $initial } 'Initialization writer revision .* still carries --create-if-missing'
        Assert (Test-CreateFlags $script:captured[0]) "$case did not apply the init writer first."
        $initRevision = "test-writer--$($script:captured[0].properties.template.revisionSuffix)"
        Assert ($script:active[$initRevision] -eq $false) "$case left the create-flag writer revision active."
        Assert (-not @($script:calls | Where-Object { $_ -match '^containerapp (create|update) .*--name test-(reader|gateway) ' }).Count) "$case deployed readers/gateway."
        Assert (-not @($script:calls | Where-Object { $_ -match 'revision activate|rollback' }).Count) "$case reactivated a writer."
        Assert-NoManifests
        $failedApps = $script:apps.Clone()
        Assert (Test-CreateFlags $failedApps['test-writer']) "$case did not model the retained init writer."

        Reset-Mock -FreshWriter; $script:apps = $failedApps
        Add-WriterRevision $initRevision $false
        Invoke-VLiteHaDeployment $config -WarningAction SilentlyContinue
        Assert (($script:captured.tags.vliteHaRole -join ',') -ceq 'writer,reader,gateway') "$case recovery order changed."
        Assert (-not (Test-CreateFlags $script:captured[0]) -and -not (Test-CreateFlags $script:apps['test-writer'])) "$case recovery retained create flags."
        Assert ($script:stopChecks[$initRevision] -and (Get-CallIndex "replica list --name test-writer .*--revision $initRevision$") -lt (Get-CallIndex '^containerapp update .*--name test-writer ')) "$case recovery did not drain the init writer first."
        Assert (-not @($script:calls | Where-Object { $_ -match 'revision activate|rollback' }).Count) "$case recovery reactivated a writer."
        Assert-NoManifests
        Remove-Item -LiteralPath (Get-VLiteHaStateRoot) -Recurse -Force
    }

    Reset-Mock -FreshWriter
    Invoke-VLiteHaDeployment $initial -WarningAction SilentlyContinue
    Assert (($script:captured.tags.vliteHaRole -join ',') -ceq 'writer,writer,reader,gateway') 'Apply order is not init writer/flagless writer/readers/gateway.'
    $initArgs = @($script:captured[0].properties.template.containers[0].args)
    Assert ($initArgs -contains '--create-if-missing' -and $initArgs -contains '--create-format') 'Initial deployment did not initialize explicitly.'
    Assert (-not (Test-CreateFlags $script:captured[1])) 'Replacement writer revision retained create flags.'
    $firstSecrets = $script:captured[0].properties.configuration.secrets | ConvertTo-Json -Depth 30 -Compress
    $replacementSecrets = $script:captured[1].properties.configuration.secrets | ConvertTo-Json -Depth 30 -Compress
    Assert ($firstSecrets -ceq $replacementSecrets) 'Replacement writer changed immutable archive secrets.'
    Assert (-not (Test-CreateFlags $script:apps['test-writer'])) 'Live writer manifest retained create flags.'
    $initRevision = "test-writer--$($script:captured[0].properties.template.revisionSuffix)"
    $flaglessRevision = "test-writer--$($script:apps['test-writer'].properties.template.revisionSuffix)"
    Assert ($flaglessRevision -cne $initRevision -and $script:active[$flaglessRevision]) 'Flagless writer revision is not live.'
    Assert ($script:active[$initRevision] -eq $false -and $script:stopChecks[$initRevision]) 'Create-flag writer revision was not deactivated and drained.'
    Assert (@($script:calls | Where-Object { $_ -match '^containerapp create ' }).Count -eq 3) 'New apps did not use create.'
    Assert (@($script:calls | Where-Object { $_ -match '^containerapp update ' }).Count -eq 1) 'Init writer was not replaced exactly once.'
    $order = @(
        '^containerapp create .*--name test-writer ',
        "^containerapp replica list --subscription .*--revision $initRevision$",
        "^containerapp revision deactivate .*--revision $initRevision$",
        "^containerapp replica list --name test-writer .*--revision $initRevision$",
        '^containerapp update .*--name test-writer ',
        "^containerapp replica list --subscription .*--revision $flaglessRevision$",
        '^containerapp create .*--name test-reader ',
        '^containerapp create .*--name test-gateway '
    ) | ForEach-Object { Get-CallIndex $_ }
    for ($i = 0; $i -lt $order.Count; $i++) {
        Assert ($order[$i] -ge 0 -and ($i -eq 0 -or $order[$i] -gt $order[$i - 1])) "Init ordering step $i is missing or out of order."
    }
    Assert ($script:qualificationCalls -eq 2) 'Missing post-stop authority gate before replacing the init writer.'
    Assert-NoManifests
    $private = Join-Path (Get-VLiteHaStateRoot) 'azure-ha' 'test-sub' 'test-rg' 'test-ha'
    $generationPath = Join-Path $private 'generation-g7.json'
    $statePath = Join-Path $private 'deployment.json'
    $originalMaterial = [IO.File]::ReadAllText($generationPath)
    $originalState = [IO.File]::ReadAllText($statePath)
    $ownedApps = $script:apps.Clone()

    Reset-Mock; $script:apps = $ownedApps.Clone()
    Invoke-VLiteHaDeployment $config -WarningAction SilentlyContinue
    Assert (($script:captured.tags.vliteHaRole -join ',') -ceq 'writer,reader,gateway') 'Update order changed.'
    Assert (@($script:calls | Where-Object { $_ -match '^containerapp update ' }).Count -eq 3) 'Existing apps did not use update.'
    Assert ($script:captured[0].properties.template.containers[0].args -notcontains '--create-if-missing') 'Restart retained initialization.'
    Assert ([IO.File]::ReadAllText($generationPath) -ceq $originalMaterial) 'Rerun rotated immutable material.'
    $updateIndex = $script:calls.FindIndex([Predicate[string]]{ param($s) $s -match '^containerapp update ' })
    foreach ($revision in @('old-a', 'old-b')) {
        $index = $script:calls.FindIndex([Predicate[string]]{ param($s) $s -match "revision deactivate .*--revision $revision$" })
        Assert ($index -ge 0 -and $index -lt $updateIndex) 'Not all active revisions stopped before update.'
    }
    foreach ($revision in @('old-a', 'old-b', 'old-inactive')) {
        $index = $script:calls.FindIndex([Predicate[string]]{ param($s) $s -match "replica list .*--revision $revision$" })
        Assert ($index -ge 0 -and $index -lt $updateIndex -and $script:stopChecks[$revision]) 'Every revision must have zero replicas before update.'
    }
    $lastDeactivate = $script:calls.FindLastIndex([Predicate[string]]{ param($s) $s -match 'revision deactivate ' })
    $firstReplica = $script:calls.FindIndex([Predicate[string]]{ param($s) $s -match 'replica list ' })
    Assert ($lastDeactivate -lt $firstReplica) 'Replica drain began before deactivating every active revision.'
    Assert ($script:qualificationCalls -eq 2) 'Missing post-stop authority gate.'
    Assert-NoManifests
    foreach ($case in @('busy', 'apply-failure', 'unready', 'post-stop', 'missing-state', 'missing-generation', 'changed-generation', 'reinitialize')) {
        Reset-Mock; $script:apps = $ownedApps.Clone()
        $candidate = $config.Clone()
        $pattern = ''
        switch ($case) {
            'busy' { $script:busy = $true; $pattern = 'Refusing a second database owner' }
            'apply-failure' { $script:failApply = $true; $pattern = 'Simulated apply failure' }
            'unready' { $script:unready = $true; $pattern = 'Exact HA revision replicas did not become ready' }
            'post-stop' { $script:postStopBlocked = $true; $pattern = 'authority CAS refused' }
            'missing-state' { Remove-Item -LiteralPath $statePath; $pattern = 'Original deployment state is missing' }
            'missing-generation' { Remove-Item -LiteralPath $generationPath; $pattern = 'Original generation material is missing' }
            'changed-generation' { $candidate.SecretGeneration = 8; $pattern = 'binding/generation differs' }
            'reinitialize' {
                $candidate.InitializeTenant = $true; $candidate.Dimensions = 8; $candidate.CreateFormat = 4
                $pattern = 'brand-new deployment'
            }
        }
        try {
            Assert-Throws { Invoke-VLiteHaDeployment $candidate } $pattern
            Assert-NoManifests
            if ($case -notin @('apply-failure', 'unready')) {
                Assert (-not @($script:calls | Where-Object { $_ -match '^containerapp (create|update) ' }).Count) "$case updated an app."
            } else {
                Assert ($script:captured.Count -eq 1 -and $script:captured[0].tags.vliteHaRole -ceq 'writer') 'Failure did not stop subsequent app rollout.'
            }
            foreach ($a in $script:allArgv) {
                $text = $a -join ' '
                foreach ($value in $script:allSecretValues) {
                    Assert (-not $text.Contains($value)) 'Secret/config value leaked into argv.'
                }
                Assert ($text -notmatch '--secrets|keys list|account set|--overwrite|revision activate') 'Forbidden operation or credential argv.'
            }
            Assert (-not @($script:calls | Where-Object { $_ -match 'revision activate|rollback' }).Count) 'Failure reactivated old writer.'
        } finally {
            if (-not (Test-Path -LiteralPath $statePath)) { [IO.File]::WriteAllText($statePath, $originalState) }
            if (-not (Test-Path -LiteralPath $generationPath)) { [IO.File]::WriteAllText($generationPath, $originalMaterial) }
        }
    }
    Assert ($script:unexpected.Count -eq 0) 'An unexpected Azure operation was attempted.'
} finally {
    ${function:Assert-VLiteHaQualification} = $realQualification
    if (Test-Path -LiteralPath $script:testRoot) { Remove-Item -LiteralPath $script:testRoot -Recurse -Force }
}
Write-Host 'HA native assertions and recording-mock deployment safety tests passed; no Azure access.'
