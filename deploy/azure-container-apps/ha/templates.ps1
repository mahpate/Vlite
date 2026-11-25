#Requires -Version 7.2

# These builders perform no I/O. Config keys are PascalCase; runtime JSON keys
# deliberately follow the Rust schemas. File paths are fixed projection contracts.
# All app builders require the complete deployment Config validated below:
# SubscriptionId, ResourceGroup, Location, DeploymentId, Environment, ShareName,
# WriterStorageName, Image, WorkloadProfileName, SecretGeneration, EnableHa,
# AllowUnqualifiedWriterStorage, GatewayMinReplicas, GatewayMaxReplicas,
# GatewayHttpConcurrency, SafetyMarginMs, WriterAudience, plus runtime inputs.
# Per enabled role: <Role>Name/Cpu/Memory/IdentityId/StartupBudgetSeconds/
# TerminationGracePeriodSeconds; backends also require <Role>StopBudgetMs.
# EnableReaders adds ReaderAudience/MinReplicas/MaxReplicas/HttpConcurrency and
# the reader-apply inputs. Trust builders take a role-specific Config.Audience.
# SecretGeneration is a positive integer; Secrets contains exactly the role's
# <prefix>-g<SecretGeneration> string values (see the explicit file map below).
# The offline control-worker Job (New-VLiteHaWorkerJobTemplate, never applied by
# deploy.ps1) also requires WorkerJobName/IdentityId/Cpu/Memory and ControlTarget,
# accepts an optional WorkerReplicaTimeoutSeconds, and its Secrets hold only
# control-target-g<SecretGeneration>.

# Fields of the Rust Blob target (cloud_backup.rs Target, minus the
# emulator-only endpoint); shared by every target check here and in deploy.ps1.
$script:VLiteHaBlobTargetFields = @('account', 'container', 'coordination_container', 'prefix', 'auth', 'client_id')
# Mirrors resource_id.rs SINGLETON; the harness checks parity.
$script:VLiteHaSingletonNames = @('vlite-app', 'vlite-env', 'vlite-data')

function Get-VLiteHaDeterministicGuid {
    param([Parameter(Mandatory)][string]$Text)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { $hash = $sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($Text)) } finally { $sha.Dispose() }
    ([guid]::new([byte[]]$hash[0..15])).ToString()
}

function Assert-VLiteHaKeys {
    param([hashtable]$Config, [string[]]$Keys)
    foreach ($key in $Keys) {
        if (-not $Config.ContainsKey($key) -or $null -eq $Config[$key] -or
            ($Config[$key] -is [string] -and [string]::IsNullOrWhiteSpace($Config[$key]))) {
            throw "Config.$key is required; no deployment default is supplied."
        }
        if ($Config[$key] -is [Collections.IEnumerable] -and $Config[$key] -isnot [string]) {
            throw "Config.$key must be a scalar value, not a collection."
        }
    }
}

function Assert-VLiteHaInteger {
    param([hashtable]$Config, [string]$Key, [decimal]$Minimum, [decimal]$Maximum)
    Assert-VLiteHaKeys $Config @($Key)
    $number = [decimal]0
    if (-not [decimal]::TryParse([string]$Config[$Key],
            [Globalization.NumberStyles]::None, [Globalization.CultureInfo]::InvariantCulture, [ref]$number) -or
        $number -lt $Minimum -or $number -gt $Maximum) {
        throw "Config.$Key must be an integer in [$Minimum, $Maximum]."
    }
}

function Assert-VLiteHaFlag {
    param([hashtable]$Config, [string]$Key, [switch]$Required)
    if (-not $Config.ContainsKey($Key)) {
        if ($Required) { throw "Config.$Key must be explicitly enabled." }
        return
    }
    if ($Config[$Key] -isnot [bool] -and $Config[$Key] -isnot [Management.Automation.SwitchParameter]) {
        throw "Config.$Key must be a Boolean or switch."
    }
    if ($Required -and -not $Config[$Key]) { throw "Config.$Key must be explicitly enabled." }
}

function Assert-VLiteHaControlConfig {
    param([hashtable]$Config)
    Assert-VLiteHaKeys $Config @('ControlTargetFile', 'ControlObject', 'PublicKeyBundleFile')
    if ($Config.ControlTargetFile -isnot [string] -or $Config.PublicKeyBundleFile -isnot [string] -or
        $Config.ControlTargetFile -cne '/run/vlite/control-target.json' -or
        $Config.PublicKeyBundleFile -cne '/run/vlite/public-keys.json') {
        throw 'Config.ControlTargetFile and Config.PublicKeyBundleFile must match /run/vlite/control-target.json and /run/vlite/public-keys.json projections.'
    }
    # Match control_state::valid_object_name, including the reserved set protocol.
    $object = $Config.ControlObject
    if ($object -isnot [string] -or $object.Length -gt 256 -or
        $object -cnotmatch '^[a-z0-9._/-]+$' -or $object.StartsWith('/') -or
        @($object.Split('/') | Where-Object { $_ -in @('', '.', '..') }).Count -gt 0 -or
        $object -in @('owner.json', 'catalog.json', 'authority.json') -or $object.StartsWith('sets/')) {
        throw 'Config.ControlObject must be a relative [a-z0-9._/-] object outside the reserved set protocol.'
    }
    Assert-VLiteHaInteger $Config 'SCtlMs' 1 3600000
    Assert-VLiteHaInteger $Config 'PollIntervalMs' 1 3600000
    Assert-VLiteHaInteger $Config 'ReadinessPropagationMs' 0 3600000
    if ([long]$Config.PollIntervalMs -ge [long]$Config.SCtlMs) {
        throw 'Config.PollIntervalMs must be positive and below Config.SCtlMs.'
    }
}

function New-VLiteHaGatewayConfig {
    [CmdletBinding()]
    param([Parameter(Mandatory)][hashtable]$Config)

    Assert-VLiteHaControlConfig $Config
    Assert-VLiteHaKeys $Config @('SigningKeyFile', 'ActiveKid')
    if ($Config.SigningKeyFile -isnot [string] -or $Config.SigningKeyFile -cne '/run/vlite/signing-key') {
        throw 'Config.SigningKeyFile must match the /run/vlite/signing-key projection.'
    }
    if ($Config.ActiveKid -isnot [string] -or $Config.ActiveKid -cnotmatch '^[a-z0-9_-]{1,64}$') {
        throw 'Config.ActiveKid must contain 1..64 lowercase ASCII letters, digits, underscores or hyphens.'
    }
    Assert-VLiteHaInteger $Config 'GatewayMaxReplicas' 2 1000
    foreach ($key in @('EnvelopeLifetimeMs', 'DrainTimeoutMs', 'UpstreamTimeoutMs')) {
        Assert-VLiteHaInteger $Config $key 1 3600000
    }
    Assert-VLiteHaInteger $Config 'MaxRequestBytes' 1 8388608
    Assert-VLiteHaInteger $Config 'MaxInFlight' 1 65536
    Assert-VLiteHaInteger $Config 'MaxBuckets' 1 1000000
    @{
        bind = '0.0.0.0:8080'
        allow_remote = $true
        control_state = @{ target_file = $Config.ControlTargetFile; object = $Config.ControlObject }
        s_ctl_ms = [long]$Config.SCtlMs
        poll_interval_ms = [long]$Config.PollIntervalMs
        max_replicas = [int]$Config.GatewayMaxReplicas
        envelope_lifetime_ms = [long]$Config.EnvelopeLifetimeMs
        public_key_bundle_file = $Config.PublicKeyBundleFile
        signing_key_file = $Config.SigningKeyFile
        active_kid = $Config.ActiveKid
        readiness_propagation_ms = [long]$Config.ReadinessPropagationMs
        drain_timeout_ms = [long]$Config.DrainTimeoutMs
        max_request_bytes = [long]$Config.MaxRequestBytes
        max_in_flight = [int]$Config.MaxInFlight
        upstream_timeout_ms = [long]$Config.UpstreamTimeoutMs
        max_buckets = [int]$Config.MaxBuckets
    }
}

function New-VLiteHaTrustConfig {
    [CmdletBinding()]
    param([Parameter(Mandatory)][hashtable]$Config)

    Assert-VLiteHaControlConfig $Config
    Assert-VLiteHaKeys $Config @('Audience')
    if ($Config.Audience -isnot [string] -or
        [Text.Encoding]::UTF8.GetByteCount($Config.Audience) -gt 2048 -or
        $Config.Audience -match '\p{Cc}') {
        throw 'Config.Audience must contain 1..2048 UTF-8 bytes without control characters.'
    }
    Assert-VLiteHaInteger $Config 'SkewMs' 0 3600000
    Assert-VLiteHaInteger $Config 'MaxLifetimeMs' 1 3600000
    @{
        audience = $Config.Audience
        public_key_bundle_file = $Config.PublicKeyBundleFile
        control_state = @{ target_file = $Config.ControlTargetFile; object = $Config.ControlObject }
        s_ctl_ms = [long]$Config.SCtlMs
        poll_interval_ms = [long]$Config.PollIntervalMs
        skew_ms = [long]$Config.SkewMs
        max_lifetime_ms = [long]$Config.MaxLifetimeMs
        readiness_propagation_ms = [long]$Config.ReadinessPropagationMs
    }
}

function New-VLiteHaReaderApplyConfig {
    [CmdletBinding()]
    param([Parameter(Mandatory)][hashtable]$Config)

    Assert-VLiteHaKeys $Config @('ReaderTargetFile', 'FollowerDir')
    if ($Config.ReaderTargetFile -isnot [string] -or $Config.FollowerDir -isnot [string] -or
        $Config.ReaderTargetFile -cne '/run/vlite/archive-target.json' -or
        $Config.FollowerDir -cne '/data') {
        throw 'Config.ReaderTargetFile and Config.FollowerDir must match /run/vlite/archive-target.json and private /data.'
    }
    Assert-VLiteHaInteger $Config 'ReaderCapacityBytes' 1 1073741824
    Assert-VLiteHaInteger $Config 'ReaderStalenessBoundMs' 1 3600000
    Assert-VLiteHaInteger $Config 'ReaderPollIntervalMs' 1 3600000
    Assert-VLiteHaInteger $Config 'SCtlMs' 1 3600000
    if ([long]$Config.ReaderPollIntervalMs -ge [long]$Config.SCtlMs) {
        throw 'Config.ReaderPollIntervalMs must be positive and below Config.SCtlMs.'
    }
    if ([long]$Config.ReaderPollIntervalMs -ge [long]$Config.ReaderStalenessBoundMs) {
        throw 'Config.ReaderPollIntervalMs must be below Config.ReaderStalenessBoundMs.'
    }
    if ($Config.ContainsKey('ReaderSharedKeyFile') -and $null -ne $Config.ReaderSharedKeyFile) {
        throw 'Config.ReaderSharedKeyFile is not supported for HA: use preprovisioned managed identity, not production shared-key authentication.'
    }
    @{
        target_file = $Config.ReaderTargetFile
        follower_dir = $Config.FollowerDir
        capacity_bytes = [long]$Config.ReaderCapacityBytes
        staleness_bound_ms = [long]$Config.ReaderStalenessBoundMs
        poll_interval_ms = [long]$Config.ReaderPollIntervalMs
    }
}

# One explicit Consumption CPU/RAM pair for a role (Gateway, Writer, Reader or Worker).
function Assert-VLiteHaResources {
    param([hashtable]$Config, [string]$Role)
    Assert-VLiteHaKeys $Config @("${Role}Cpu", "${Role}Memory")
    $cpu = [double]0
    if (-not [double]::TryParse([string]$Config["${Role}Cpu"],
            [Globalization.NumberStyles]::Float, [Globalization.CultureInfo]::InvariantCulture, [ref]$cpu) -or
        $cpu -notin @(0.25, 0.5, 0.75, 1, 1.25, 1.5, 1.75, 2)) {
        throw "Config.${Role}Cpu must be an explicit Consumption CPU allocation from 0.25 to 2 in 0.25 increments."
    }
    $memory = ($cpu * 2).ToString('0.0#', [Globalization.CultureInfo]::InvariantCulture) + 'Gi'
    $alternate = ($cpu * 2).ToString('0.##', [Globalization.CultureInfo]::InvariantCulture) + 'Gi'
    if ($Config["${Role}Memory"] -cne $memory -and $Config["${Role}Memory"] -cne $alternate) {
        throw "Config.${Role}Memory must be explicitly specified as $memory (Consumption CPU/RAM pair)."
    }
}

function Test-VLiteHaParameters {
    [CmdletBinding()]
    param([Parameter(Mandatory)][hashtable]$Config)

    Assert-VLiteHaFlag $Config 'EnableHa' -Required
    Assert-VLiteHaFlag $Config 'AllowUnqualifiedWriterStorage' -Required
    Assert-VLiteHaFlag $Config 'EnableReaders'
    Assert-VLiteHaFlag $Config 'InitializeTenant'
    Assert-VLiteHaKeys $Config @(
        'SubscriptionId', 'ResourceGroup', 'Location', 'DeploymentId', 'Environment',
        'GatewayName', 'WriterName', 'ShareName', 'WriterStorageName', 'Image',
        'WorkloadProfileName', 'GatewayIdentityId', 'WriterIdentityId', 'WriterAudience'
    )
    if ($Config.Image -isnot [string] -or $Config.Image -cnotmatch '^[^\s@]+@sha256:[a-fA-F0-9]{64}$') {
        throw 'Config.Image must be an explicit image digest reference (...@sha256:<64 hex>); tags and default images are prohibited.'
    }
    if ($Config.DeploymentId -isnot [string] -or $Config.DeploymentId -cnotmatch '^[a-z0-9][a-z0-9-]{0,62}$') {
        throw 'Config.DeploymentId must be a short opaque lowercase identifier (1..63 characters).'
    }
    # Check aliases too, so no incidental singleton name can be adopted.
    foreach ($key in @('Environment', 'EnvironmentName', 'AppName', 'GatewayName', 'WriterName',
            'ReaderName', 'ShareName', 'WriterShareName', 'WriterStorageName')) {
        if ($Config.ContainsKey($key) -and $Config[$key] -in $script:VLiteHaSingletonNames) {
            throw "Config.$key references a protected singleton name. The HA deployment cannot adopt or modify singleton resources."
        }
    }
    $appKeys = @('GatewayName', 'WriterName')
    if ($Config['EnableReaders']) {
        Assert-VLiteHaKeys $Config @('ReaderName', 'ReaderIdentityId', 'ReaderAudience')
        $appKeys += 'ReaderName'
    }
    foreach ($key in ($appKeys + @('Environment'))) {
        if ($Config[$key] -isnot [string] -or $Config[$key].Length -gt 32 -or
            $Config[$key] -cnotmatch '^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$' -or $Config[$key].Length -lt 3) {
            throw "Config.$key must be a lowercase ACA name of 3..32 characters, without consecutive or trailing hyphens."
        }
    }
    if (@($appKeys | ForEach-Object { $Config[$_] } | Select-Object -Unique).Count -ne $appKeys.Count) {
        throw 'GatewayName, WriterName and enabled ReaderName must be distinct.'
    }
    if ($Config.ShareName -isnot [string] -or $Config.ShareName.Length -lt 3 -or
        $Config.ShareName.Length -gt 63 -or $Config.ShareName -cnotmatch '^[a-z0-9]+(?:-[a-z0-9]+)*$') {
        throw 'Config.ShareName must be a new Azure Files share name (3..63 lowercase letters, digits and nonconsecutive hyphens).'
    }
    if ($Config.WriterStorageName -isnot [string] -or
        $Config.WriterStorageName -cnotmatch '^[a-z][a-z0-9-]{0,62}$') {
        throw 'Config.WriterStorageName must explicitly name the HA environment Azure Files storage binding.'
    }
    if ($Config.WorkloadProfileName -cne 'Consumption') {
        throw 'Config.WorkloadProfileName must be Consumption; Dedicated requires separate measured capacity approval.'
    }
    Assert-VLiteHaInteger $Config 'SecretGeneration' 1 2147483647
    Assert-VLiteHaInteger $Config 'GatewayMinReplicas' 2 1000
    Assert-VLiteHaInteger $Config 'GatewayMaxReplicas' 2 1000
    Assert-VLiteHaInteger $Config 'GatewayHttpConcurrency' 1 2147483647
    if ([int]$Config.GatewayMaxReplicas -lt [int]$Config.GatewayMinReplicas) {
        throw 'Config.GatewayMaxReplicas must be at least Config.GatewayMinReplicas.'
    }
    Assert-VLiteHaInteger $Config 'SafetyMarginMs' 0 3600000
    $roles = @('Gateway', 'Writer')
    if ($Config['EnableReaders']) {
        $roles += 'Reader'
        Assert-VLiteHaInteger $Config 'ReaderMinReplicas' 1 1000
        Assert-VLiteHaInteger $Config 'ReaderMaxReplicas' 1 1000
        Assert-VLiteHaInteger $Config 'ReaderHttpConcurrency' 1 2147483647
        if ([int]$Config.ReaderMaxReplicas -lt [int]$Config.ReaderMinReplicas) {
            throw 'Config.ReaderMaxReplicas must be at least Config.ReaderMinReplicas.'
        }
        New-VLiteHaReaderApplyConfig -Config $Config | Out-Null
    }
    New-VLiteHaGatewayConfig -Config $Config | Out-Null
    Assert-VLiteHaInteger $Config 'MaxLifetimeMs' 1 3600000
    if ([long]$Config.EnvelopeLifetimeMs -gt [long]$Config.MaxLifetimeMs) {
        throw 'Config.EnvelopeLifetimeMs must not exceed the backend Config.MaxLifetimeMs.'
    }
    foreach ($role in $roles) {
        $identity = $Config["${role}IdentityId"]
        if ($identity -isnot [string] -or
            $identity -inotmatch '^/subscriptions/[^/]+/resourceGroups/[^/]+/providers/Microsoft\.ManagedIdentity/userAssignedIdentities/[^/]+$') {
            throw "Config.${role}IdentityId must be an existing user-assigned managed identity resource ID."
        }
        Assert-VLiteHaResources $Config $role
        Assert-VLiteHaInteger $Config "${role}TerminationGracePeriodSeconds" 1 3600
        Assert-VLiteHaInteger $Config "${role}StartupBudgetSeconds" 10 100
        if ([int]$Config["${role}StartupBudgetSeconds"] % 10 -ne 0) {
            throw "Config.${role}StartupBudgetSeconds must be a multiple of the explicit 10-second startup probe period."
        }
        $requiredMs = [long]$Config.ReadinessPropagationMs + [long]$Config.DrainTimeoutMs + [long]$Config.SafetyMarginMs
        if ([long]$Config["${role}TerminationGracePeriodSeconds"] * 1000 -le $requiredMs) {
            throw "Config.${role}TerminationGracePeriodSeconds * 1000 must exceed ReadinessPropagationMs + DrainTimeoutMs + SafetyMarginMs."
        }
        if ($role -ne 'Gateway') {
            Assert-VLiteHaInteger $Config "${role}StopBudgetMs" 1 3600000
            if ([long]$Config["${role}TerminationGracePeriodSeconds"] * 1000 -le
                ([long]$Config.ReadinessPropagationMs + [long]$Config["${role}StopBudgetMs"] + [long]$Config.SafetyMarginMs)) {
                throw "Config.${role}TerminationGracePeriodSeconds must also cover readiness propagation, measured ${role}StopBudgetMs and SafetyMarginMs."
            }
            $trustConfig = $Config.Clone()
            $trustConfig.Audience = $Config["${role}Audience"]
            New-VLiteHaTrustConfig -Config $trustConfig | Out-Null
        }
    }
    if ($Config['InitializeTenant']) {
        Assert-VLiteHaInteger $Config 'Dimensions' 1 65536
        Assert-VLiteHaInteger $Config 'CreateFormat' 4 5
    } elseif ($Config.ContainsKey('Dimensions') -or $Config.ContainsKey('CreateFormat')) {
        throw 'Config.Dimensions and Config.CreateFormat require explicit Config.InitializeTenant; omit initialization inputs on restarts.'
    }
    if ($Config.ContainsKey('ReadRouting') -and $Config.ReadRouting -cne 'writer_only') {
        throw 'Config.ReadRouting must remain writer_only until independent reader replication qualification.'
    }
    $true
}

function Assert-VLiteHaConfigFields {
    param([hashtable]$Actual, [hashtable]$Expected, [string]$Label)
    if ($Actual.Count -ne $Expected.Count) { throw "$Label must contain exactly the generated runtime fields." }
    foreach ($key in $Expected.Keys) {
        if (@($Actual.Keys) -cnotcontains $key) { throw "$Label is missing the exact runtime field $key." }
        $value = $Actual[$key]
        if ($Expected[$key] -is [hashtable]) {
            if ($value -isnot [hashtable]) { throw "$Label.$key must be an object." }
            Assert-VLiteHaConfigFields -Actual $value -Expected $Expected[$key] -Label "$Label.$key"
        } else {
            $typeMatches = if ($Expected[$key] -is [string]) { $value -is [string] }
                elseif ($Expected[$key] -is [bool]) { $value -is [bool] }
                else { $value -is [long] -or $value -is [int] }
            if (-not $typeMatches -or $value -cne $Expected[$key]) {
                throw "$Label.$key must match the validated Config value."
            }
        }
    }
}

function Assert-VLiteHaConfigDocument {
    param([string]$Json, [hashtable]$Expected, [string]$Label)
    if ([Text.Encoding]::UTF8.GetByteCount($Json) -gt 65536) {
        throw "$Label exceeds the runtime's 65536-byte configuration limit."
    }
    try {
        $actual = ConvertFrom-Json -InputObject $Json -AsHashtable -NoEnumerate -Depth 30 -ErrorAction Stop
    } catch [ArgumentException] {
        # Parser diagnostics can quote secret content; only report its purpose.
        throw "$Label must be valid JSON; configuration contents are not logged."
    }
    if ($actual -isnot [hashtable]) { throw "$Label must be a JSON object." }
    Assert-VLiteHaConfigFields -Actual $actual -Expected $Expected -Label $Label
}

function New-VLiteHaWriterArchivePolicy {
    [CmdletBinding()]
    param([Parameter(Mandatory)][hashtable]$Config)

    if ($Config.ArchiveTarget -isnot [hashtable]) { throw 'Config.ArchiveTarget must be an object.' }
    $values = $Config.Clone()
    foreach ($entry in @(
        @('WriterBackupIntervalSecs', 3600, 300, 604800),
        @('WriterBackupKeepSets', 3, 3, 1000),
        @('WriterBackupKeepDays', 30, 30, 3650),
        @('WriterBackupOrphanGraceHours', 24, 1, 720),
        @('WriterBackupDrillEvery', 24, 0, 100),
        @('WriterArchiveIntervalSecs', 300, 60, 86400)
    )) {
        if (-not $values.ContainsKey($entry[0])) { $values[$entry[0]] = $entry[1] }
        Assert-VLiteHaInteger $values $entry[0] $entry[2] $entry[3]
    }
    Assert-VLiteHaFlag $Config 'WriterBackupRetentionEnabled'
    @{
        target = $Config.ArchiveTarget.Clone()
        enabled = $true
        interval_secs = [long]$values.WriterBackupIntervalSecs
        retention_enabled = [bool]$Config['WriterBackupRetentionEnabled']
        keep_sets = [int]$values.WriterBackupKeepSets
        keep_days = [long]$values.WriterBackupKeepDays
        orphan_grace_hours = [long]$values.WriterBackupOrphanGraceHours
        drill_every = [long]$values.WriterBackupDrillEvery
        archive = @{ enabled = $true; interval_secs = [long]$values.WriterArchiveIntervalSecs }
    }
}

function New-VLiteHaAppTemplate {
    param(
        [hashtable]$Config, [string]$EnvironmentId, [hashtable]$Secrets,
        [string]$Revision, [ValidateSet('Gateway', 'Writer', 'Reader')][string]$Role
    )
    Test-VLiteHaParameters -Config $Config | Out-Null
    if ($Role -eq 'Reader' -and -not $Config['EnableReaders']) {
        throw 'Config.EnableReaders must be explicitly enabled to build a reader template.'
    }
    $expectedEnvironmentId = "/subscriptions/$($Config.SubscriptionId)/resourceGroups/$($Config.ResourceGroup)/providers/Microsoft.App/managedEnvironments/$($Config.Environment)"
    if (-not [string]::Equals($EnvironmentId, $expectedEnvironmentId, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'EnvironmentId must be the full resource ID matching Config.SubscriptionId, ResourceGroup and the new HA Environment.'
    }
    if ($Revision -cnotmatch '^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$' -or
        ("$($Config["${Role}Name"])--$Revision").Length -gt 64) {
        throw 'Revision must be a lowercase ACA revision suffix with no consecutive hyphens; app--suffix must fit 64 characters.'
    }
    $files = @{
        'verify' = 'public-keys.json'
        'control-target' = 'control-target.json'
    }
    if ($Role -eq 'Gateway') {
        $files['gw-seed'] = 'signing-key'
        $files['gateway-config'] = 'gateway.json'
        $arguments = @('gateway', '--config', '/run/vlite/gateway.json')
        $minimum = [int]$Config.GatewayMinReplicas
        $maximum = [int]$Config.GatewayMaxReplicas
    } else {
        $files['backend-trust'] = 'trust.json'
        if ($Role -eq 'Writer') {
            $files['archive-target'] = 'archive-target.json'
            $files['archive-policy'] = 'archive-policy.json'
            $arguments = @('/data/vectors.vlite', '--serve-mode', 'writer', '--bind', '0.0.0.0:8080',
                '--allow-remote', '--gateway-trust-file', '/run/vlite/trust.json', '--durability', 'full',
                '--authority-target-file', '/run/vlite/archive-target.json',
                '--archive-policy-file', '/run/vlite/archive-policy.json')
            if ($Config['InitializeTenant']) {
                $arguments += @('--create-if-missing', [string]$Config.Dimensions, '--create-format', [string]$Config.CreateFormat)
            }
            $minimum = 1
            $maximum = 1
        } else {
            $files['archive-target'] = 'archive-target.json'
            $files['reader-config'] = 'reader.json'
            $arguments = @('/data/reader-a/database.vlite', '--serve-mode', 'reader', '--bind', '0.0.0.0:8080',
                '--allow-remote', '--gateway-trust-file', '/run/vlite/trust.json',
                '--reader-apply-file', '/run/vlite/reader.json')
            $minimum = [int]$Config.ReaderMinReplicas
            $maximum = [int]$Config.ReaderMaxReplicas
        }
    }
    $projections = @()
    $appSecrets = @()
    foreach ($prefix in @($files.Keys | Sort-Object)) {
        $name = "$prefix-g$([int]$Config.SecretGeneration)"
        if (-not $Secrets.ContainsKey($name) -or $Secrets[$name] -isnot [string] -or
            [string]::IsNullOrWhiteSpace($Secrets[$name])) {
            throw "Secrets must provide a nonempty string for the required immutable $name projection."
        }
        $projections += @{ secretRef = $name; path = $files[$prefix] }
        $appSecrets += @{ name = $name; value = $Secrets[$name] }
    }
    if ($Secrets.Count -ne $files.Count) {
        throw "$Role Secrets must contain only that role's explicit generation-suffixed projections; extra secrets (including admin credentials) are prohibited."
    }
    $generation = [int]$Config.SecretGeneration
    if ($Role -eq 'Gateway') {
        Assert-VLiteHaConfigDocument -Json $Secrets["gateway-config-g$generation"] `
            -Expected (New-VLiteHaGatewayConfig -Config $Config) -Label 'Gateway configuration projection'
        if ([Text.Encoding]::UTF8.GetByteCount($Secrets["gw-seed-g$generation"]) -gt 1024) {
            throw 'Gateway signing seed exceeds the runtime 1024-byte seed-file limit.'
        }
    } else {
        $trustConfig = $Config.Clone()
        $trustConfig.Audience = $Config["${Role}Audience"]
        Assert-VLiteHaConfigDocument -Json $Secrets["backend-trust-g$generation"] `
            -Expected (New-VLiteHaTrustConfig -Config $trustConfig) -Label "$Role trust projection"
        if ($Role -eq 'Writer') {
            Assert-VLiteHaConfigDocument -Json $Secrets["archive-target-g$generation"] `
                -Expected $Config.ArchiveTarget -Label 'Writer archive target projection'
            Assert-VLiteHaConfigDocument -Json $Secrets["archive-policy-g$generation"] `
                -Expected (New-VLiteHaWriterArchivePolicy -Config $Config) -Label 'Writer archive policy projection'
        } elseif ($Role -eq 'Reader') {
            Assert-VLiteHaConfigDocument -Json $Secrets["reader-config-g$generation"] `
                -Expected (New-VLiteHaReaderApplyConfig -Config $Config) -Label 'Reader apply projection'
        }
    }
    if ([Text.Encoding]::UTF8.GetByteCount($Secrets["verify-g$generation"]) -gt 65536) {
        throw 'Public verification bundle exceeds the runtime 65536-byte configuration limit.'
    }
    $rules = @()
    if ($Role -ne 'Writer') {
        $rules = @(@{
            name = "$($Role.ToLowerInvariant())-http"
            http = @{ metadata = @{ concurrentRequests = [string]$Config["${Role}HttpConcurrency"] } }
        })
    }
    $volumes = @(@{ name = 'vlite-config'; storageType = 'Secret'; secrets = $projections })
    $mounts = @(@{ volumeName = 'vlite-config'; mountPath = '/run/vlite' })
    if ($Role -eq 'Writer') {
        $volumes += @{
            name = 'vlite-data'; storageType = 'AzureFile'; storageName = $Config.WriterStorageName
            mountOptions = 'uid=10001,gid=10001,file_mode=0600,dir_mode=0700'
        }
        $mounts += @{ volumeName = 'vlite-data'; mountPath = '/data' }
    } elseif ($Role -eq 'Reader') {
        $volumes += @{ name = 'vlite-data'; storageType = 'EmptyDir' }
        $mounts += @{ volumeName = 'vlite-data'; mountPath = '/data' }
    }
    $identityAssignments = @{}
    $identityAssignments[$Config["${Role}IdentityId"]] = @{}
    @{
        name = $Config["${Role}Name"]
        location = $Config.Location
        tags = @{
            vliteHaDeployment = $Config.DeploymentId
            vliteHaRole = $Role.ToLowerInvariant()
            vliteHaProduction = 'false'
            vliteHaStorage = 'unqualified-azure-files'
        }
        identity = @{ type = 'UserAssigned'; userAssignedIdentities = $identityAssignments }
        properties = @{
            managedEnvironmentId = $EnvironmentId
            workloadProfileName = $Config.WorkloadProfileName
            configuration = @{
                activeRevisionsMode = 'Single'
                secrets = $appSecrets
                ingress = @{
                    external = ($Role -eq 'Gateway')
                    targetPort = 8080
                    transport = 'http'
                    allowInsecure = $false
                    additionalPortMappings = @()
                    stickySessions = @{ affinity = 'none' }
                    traffic = @(@{ latestRevision = $true; weight = 100 })
                }
            }
            template = @{
                revisionSuffix = $Revision
                terminationGracePeriodSeconds = [int]$Config["${Role}TerminationGracePeriodSeconds"]
                scale = @{ minReplicas = $minimum; maxReplicas = $maximum; rules = $rules }
                volumes = $volumes
                containers = @(@{
                    name = 'vlite'
                    image = $Config.Image
                    command = @('/usr/local/bin/vlite-server')
                    args = $arguments
                    env = @()
                    resources = @{ cpu = [double]$Config["${Role}Cpu"]; memory = $Config["${Role}Memory"] }
                    volumeMounts = $mounts
                    probes = @(
                        @{
                            type = 'Startup'; httpGet = @{ path = '/ready'; port = 8080 }
                            periodSeconds = 10; timeoutSeconds = 3; successThreshold = 1
                            failureThreshold = [int]([int]$Config["${Role}StartupBudgetSeconds"] / 10)
                        },
                        @{
                            type = 'Readiness'; httpGet = @{ path = '/ready'; port = 8080 }
                            periodSeconds = 5; timeoutSeconds = 3; successThreshold = 1; failureThreshold = 3
                        },
                        @{
                            type = 'Liveness'; httpGet = @{ path = '/health'; port = 8080 }
                            periodSeconds = 30; timeoutSeconds = 3; successThreshold = 1; failureThreshold = 3
                        }
                    )
                })
            }
        }
    }
}

function New-VLiteHaGatewayTemplate {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][hashtable]$Config,
        [Parameter(Mandatory)][string]$EnvironmentId,
        [Parameter(Mandatory)][hashtable]$Secrets,
        [Parameter(Mandatory)][string]$Revision
    )
    New-VLiteHaAppTemplate -Config $Config -EnvironmentId $EnvironmentId -Secrets $Secrets -Revision $Revision -Role Gateway
}

function New-VLiteHaWriterTemplate {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][hashtable]$Config,
        [Parameter(Mandatory)][string]$EnvironmentId,
        [Parameter(Mandatory)][hashtable]$Secrets,
        [Parameter(Mandatory)][string]$Revision
    )
    New-VLiteHaAppTemplate -Config $Config -EnvironmentId $EnvironmentId -Secrets $Secrets -Revision $Revision -Role Writer
}

function New-VLiteHaReaderTemplate {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][hashtable]$Config,
        [Parameter(Mandatory)][string]$EnvironmentId,
        [Parameter(Mandatory)][hashtable]$Secrets,
        [Parameter(Mandatory)][string]$Revision
    )
    New-VLiteHaAppTemplate -Config $Config -EnvironmentId $EnvironmentId -Secrets $Secrets -Revision $Revision -Role Reader
}

# Tenant-scoped reader storage grants (design-tenant-provisioning ITEM-008).
# Pure builder: no I/O and no Azure call. It returns a custom data-plane-only
# pin-writer role (no control-plane actions, so no container delete/write)
# plus two condition-version-2.0 container-scoped assignments that confine one
# reader identity to its own Blob prefix on the EXISTING storage account:
# pin writes only under <prefix>reader-pins/ in the coordination container
# (custom role), reads and prefixed lists only under <prefix> in both
# containers (built-in Storage Blob Data Reader on the data container). User
# delegation keys are a storage-account-level action, so container-scoped
# assignments cannot mint them. Shared keys and SAS are refused. Prefixes are
# a security boundary here ONLY because the conditions say so; Azure
# enforcement of these exact conditions (and that the reader always lists with
# a prefix) is NOT qualified offline. Readers are cooperative inside their own
# tenant: any reader of a tenant can create or delete that tenant's pins.
# SiblingPrefixes is the operator's asserted-complete inventory of other
# tenant prefixes on the account (not discovered); nested/equal prefixes are
# refused in either direction because StringStartsWith on 'a/' would also
# match 'a/b/'. Target.client_id is accepted but unused: assignments bind the
# principal object ID.
function Assert-VLiteHaBlobPrefix {
    param([string]$Prefix, [string]$Label)
    # Same rule as cloud_backup.rs valid_prefix; also excludes quotes, so a
    # prefix cannot alter the generated condition.
    if ($Prefix -cnotmatch '^[a-z0-9._/-]{1,200}\z' -or -not $Prefix.EndsWith('/') -or
        @($Prefix.TrimEnd('/').Split('/') | Where-Object { $_ -in @('', '.', '..') }).Count -or
        $Prefix -eq '/') {
        throw "$Label prefix must be 1..200 chars of [a-z0-9._/-], end with '/', with no empty, '.' or '..' segments."
    }
}

function New-VLiteHaReaderStorageGrants {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$SubscriptionId,
        [Parameter(Mandatory)][string]$ResourceGroup,
        [Parameter(Mandatory)][hashtable]$Target,
        [Parameter(Mandatory)][string]$PrincipalId,
        [Parameter(Mandatory)][AllowEmptyCollection()][string[]]$SiblingPrefixes
    )
    # \z, not $: .NET '$' also matches before a trailing newline.
    $guid = '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\z'
    if ($SubscriptionId -cnotmatch $guid) { throw 'SubscriptionId must be a lowercase GUID.' }
    if ($PrincipalId -cnotmatch $guid) { throw 'PrincipalId must be a lowercase GUID (the reader identity principal).' }
    if ($ResourceGroup -cnotmatch '^[A-Za-z0-9._-]{1,90}\z' -or $ResourceGroup.EndsWith('.')) {
        throw 'ResourceGroup must be 1..90 chars of [A-Za-z0-9._-], not ending with a period.'
    }
    $allowed = $script:VLiteHaBlobTargetFields
    if ($Target.ContainsKey('shared_key_file')) { throw 'Reader grants refuse shared_key_file; shared account keys are never used.' }
    foreach ($key in $Target.Keys) {
        if ($key -cnotin $allowed) { throw "Target.$key is not a reader target field." }
    }
    if ($Target.auth -cne 'managed_identity') { throw 'Target.auth must be managed_identity.' }
    if ($Target.account -isnot [string] -or $Target.account -cnotmatch '^[a-z0-9]{3,24}\z') {
        throw 'Target.account must be a storage account name (3..24 lowercase letters/digits).'
    }
    foreach ($field in @('container', 'coordination_container')) {
        if ($Target[$field] -isnot [string] -or $Target[$field] -cnotmatch '^[a-z0-9](?!.*--)[a-z0-9-]{1,61}[a-z0-9]\z') {
            throw "Target.$field must be a Blob container name."
        }
    }
    if ($Target.container -ceq $Target.coordination_container) {
        throw 'Target.container and Target.coordination_container must be distinct.'
    }
    $prefix = [string]$Target.prefix
    Assert-VLiteHaBlobPrefix $prefix 'Target'
    foreach ($sibling in $SiblingPrefixes) {
        Assert-VLiteHaBlobPrefix $sibling 'Sibling'
        if ($sibling.StartsWith($prefix, [StringComparison]::Ordinal) -or
            $prefix.StartsWith($sibling, [StringComparison]::Ordinal)) {
            throw "Sibling prefix '$sibling' and '$prefix' nest or are equal; a prefix condition cannot separate them."
        }
    }

    $group = "/subscriptions/$SubscriptionId/resourceGroups/$ResourceGroup"
    $containers = "$group/providers/Microsoft.Storage/storageAccounts/$($Target.account)/blobServices/default/containers"
    $blobs = 'Microsoft.Storage/storageAccounts/blobServices/containers/blobs'
    $name = '@Resource[Microsoft.Storage/storageAccounts/blobServices/containers:name] StringEquals'
    $path = "@Resource[$($blobs):path] StringStartsWith"
    $list = "@Request[$($blobs):prefix] StringStartsWith"
    $read = { param($container)
        "((!(ActionMatches{'$blobs/read'} AND NOT SubOperationMatches{'Blob.List'})) OR " +
        "($name '$container' AND $path '$prefix')) AND " +
        "((!(ActionMatches{'$blobs/read'} AND SubOperationMatches{'Blob.List'})) OR " +
        "($name '$container' AND $list '$prefix'))"
    }
    $coordination = $Target.coordination_container
    $writes = "!(ActionMatches{'$blobs/write'}) AND !(ActionMatches{'$blobs/add/action'}) AND !(ActionMatches{'$blobs/delete'})"
    $pinCondition = "(($writes) OR " +
        "($name '$coordination' AND $path '$($prefix)reader-pins/')) AND " + (& $read $coordination)
    # Custom role names must be unique per Entra tenant and role IDs are GUIDs,
    # so both derive deterministically from the (case-insensitive) group scope.
    $roleGuid = Get-VLiteHaDeterministicGuid "vlite-ha-reader-pin-writer|$($group.ToLowerInvariant())"
    $roleIds = "/subscriptions/$SubscriptionId/providers/Microsoft.Authorization/roleDefinitions"
    # Assignment names are GUIDs too; deriving them from (scope, principal,
    # role) makes a re-render address the same assignment instead of a new one.
    $assignmentName = { param($scope, $role)
        Get-VLiteHaDeterministicGuid "vlite-ha-reader-grant|$("$scope|$PrincipalId|$role".ToLowerInvariant())"
    }
    # Built-in Storage Blob Data Reader; its delegation-key action is
    # account-level and unusable from this container scope.
    $blobDataReader = '2a2b9908-6ea1-4ae2-8e65-a410df84e7d1'
    [ordered]@{
        schema = 'vlite-ha-reader-storage-grants/1'
        qualification = 'offline_template_effective_access_unqualified'
        sibling_inventory = 'operator_asserted_complete'
        roleDefinition = [ordered]@{
            name = $roleGuid
            roleName = "VLite HA Reader Pin Writer $($roleGuid.Substring(0, 8))"
            description = 'Data-plane only: blob read/write/add/delete, narrowed per assignment by ABAC conditions.'
            actions = @(); notActions = @(); notDataActions = @()
            dataActions = @("$blobs/read", "$blobs/write", "$blobs/add/action", "$blobs/delete")
            assignableScopes = @($group)
        }
        assignments = @(
            [ordered]@{
                name = (& $assignmentName "$containers/$coordination" "$roleIds/$roleGuid")
                roleDefinitionId = "$roleIds/$roleGuid"; scope = "$containers/$coordination"
                principalId = $PrincipalId; principalType = 'ServicePrincipal'
                conditionVersion = '2.0'; condition = $pinCondition
            },
            [ordered]@{
                # Read-only role: the data container never accepts a reader write.
                name = (& $assignmentName "$containers/$($Target.container)" "$roleIds/$blobDataReader")
                roleDefinitionId = "$roleIds/$blobDataReader"; scope = "$containers/$($Target.container)"
                principalId = $PrincipalId; principalType = 'ServicePrincipal'
                conditionVersion = '2.0'; condition = (& $read $Target.container)
            }
        )
    }
}

# Control worker ACA Job (design-tenant-provisioning ITEM-022, TEST-014).
# Offline renderer only: no I/O and no Azure call; deploy.ps1 never applies it
# and Assert-VLiteHaQualification still refuses every HA deployment. A manual
# execution starts one replica (parallelism 1, one completion) that runs
# `vlite-server tenant-deployment work` once: one claim, one attempt.
# replicaRetryLimit is 0 because a retried replica would start a second
# attempt; an unknown outcome stays recorded in the journal for an operator.
# The Job runs as its own user-assigned identity, which no serving role may
# hold, and mounts only its control-target projection (no seed, trust, archive
# or admin material). The admin directory is the replica's EmptyDir, so the
# audit log lasts one execution; the Blob journal is the durable record.
function New-VLiteHaWorkerJobTemplate {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][hashtable]$Config,
        [Parameter(Mandatory)][string]$EnvironmentId,
        [Parameter(Mandatory)][hashtable]$Secrets
    )
    Test-VLiteHaParameters -Config $Config | Out-Null
    Assert-VLiteHaControlConfig -Config $Config
    $expectedEnvironmentId = "/subscriptions/$($Config.SubscriptionId)/resourceGroups/$($Config.ResourceGroup)/providers/Microsoft.App/managedEnvironments/$($Config.Environment)"
    if (-not [string]::Equals($EnvironmentId, $expectedEnvironmentId, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'EnvironmentId must be the full resource ID matching Config.SubscriptionId, ResourceGroup and the new HA Environment.'
    }
    Assert-VLiteHaKeys $Config @('WorkerJobName', 'WorkerIdentityId')
    $name = $Config.WorkerJobName
    if ($name -isnot [string] -or $name.Length -lt 3 -or $name.Length -gt 32 -or
        $name -cnotmatch '^[a-z][a-z0-9]*(?:-[a-z0-9]+)*\z' -or $name -cin $script:VLiteHaSingletonNames) {
        throw 'Config.WorkerJobName must be a new lowercase ACA name of 3..32 characters, without consecutive or trailing hyphens.'
    }
    foreach ($key in @('Environment', 'GatewayName', 'WriterName', 'ReaderName')) {
        if ($name -ceq $Config[$key]) { throw 'Config.WorkerJobName must differ from the environment and every serving app name.' }
    }
    $identity = $Config.WorkerIdentityId
    if ($identity -isnot [string] -or
        $identity -inotmatch '^/subscriptions/[^/\s]+/resourceGroups/[^/\s]+/providers/Microsoft\.ManagedIdentity/userAssignedIdentities/[^/\s]+\z') {
        throw 'Config.WorkerIdentityId must be an existing user-assigned managed identity resource ID.'
    }
    foreach ($role in @('Gateway', 'Writer', 'Reader')) {
        if ($Config["${role}IdentityId"] -is [string] -and
            [string]::Equals($identity, $Config["${role}IdentityId"], [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Config.WorkerIdentityId must be a dedicated identity that no serving role (gateway, writer or reader) holds.'
        }
    }
    Assert-VLiteHaResources $Config 'Worker'
    $values = $Config.Clone()
    if (-not $values.ContainsKey('WorkerReplicaTimeoutSeconds')) { $values.WorkerReplicaTimeoutSeconds = 1800 }
    Assert-VLiteHaInteger $values 'WorkerReplicaTimeoutSeconds' 300 14400

    $control = $Config['ControlTarget']
    if ($control -isnot [hashtable]) { throw 'Config.ControlTarget must be the reviewed managed-identity control-state Blob target.' }
    $secretName = "control-target-g$([int]$Config.SecretGeneration)"
    if ($Secrets.Count -ne 1 -or -not $Secrets.ContainsKey($secretName) -or $Secrets[$secretName] -isnot [string] -or
        [Text.Encoding]::UTF8.GetByteCount($Secrets[$secretName]) -gt 65536) {
        throw "Worker Secrets must contain only the $secretName control-target projection; extra secrets (including admin credentials) are prohibited."
    }
    try {
        $target = ConvertFrom-Json -InputObject $Secrets[$secretName] -AsHashtable -NoEnumerate -Depth 30 -ErrorAction Stop
    } catch {
        # Parser diagnostics can quote secret content; only report its purpose.
        throw 'Worker control-target projection must be valid JSON; its contents are not logged.'
    }
    $fields = $script:VLiteHaBlobTargetFields
    # Only the control and archive target client IDs are in the config; the
    # worker client ID must differ from both (Entra GUIDs ignore case).
    $knownClients = @($control['client_id'])
    if ($Config['ArchiveTarget'] -is [hashtable]) { $knownClients += $Config.ArchiveTarget['client_id'] }
    if ($target -isnot [hashtable] -or @($target.Keys | Where-Object { $_ -cnotin $fields }).Count -or
        @($fields | Where-Object { $target[$_] -isnot [string] -or [string]::IsNullOrWhiteSpace($target[$_]) }).Count -or
        $target.auth -cne 'managed_identity' -or
        @($fields | Where-Object { $_ -cne 'client_id' -and $target[$_] -cne $control[$_] }).Count -or
        $target.client_id -cnotmatch '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\z' -or
        @($knownClients | Where-Object {
                $_ -is [string] -and [string]::Equals($target.client_id, $_, [StringComparison]::OrdinalIgnoreCase)
            }).Count) {
        throw 'Worker control-target projection must be Config.ControlTarget bound to the worker identity client_id (a lowercase GUID); shared keys, connection strings and the control or archive target client IDs are prohibited.'
    }
    # Emit the validated object, not the input text, so duplicate JSON keys
    # cannot carry a value the checks above never saw.
    $projection = $target | ConvertTo-Json -Compress -Depth 5

    $identityAssignments = @{}
    $identityAssignments[$identity] = @{}
    @{
        name = $name
        location = $Config.Location
        tags = @{
            vliteHaDeployment = $Config.DeploymentId
            vliteHaRole = 'worker'
            vliteHaProduction = 'false'
        }
        identity = @{ type = 'UserAssigned'; userAssignedIdentities = $identityAssignments }
        properties = @{
            environmentId = $EnvironmentId
            workloadProfileName = $Config.WorkloadProfileName
            configuration = @{
                triggerType = 'Manual'
                replicaTimeout = [int]$values.WorkerReplicaTimeoutSeconds
                replicaRetryLimit = 0
                manualTriggerConfig = @{ parallelism = 1; replicaCompletionCount = 1 }
                secrets = @(@{ name = $secretName; value = $projection })
            }
            template = @{
                volumes = @(
                    @{
                        name = 'vlite-config'; storageType = 'Secret'
                        secrets = @(@{ secretRef = $secretName; path = 'control-target.json' })
                    },
                    @{ name = 'vlite-data'; storageType = 'EmptyDir' }
                )
                containers = @(@{
                    name = 'vlite'
                    image = $Config.Image
                    command = @('/usr/local/bin/vlite-server')
                    args = @('tenant-deployment', 'work', '--target-file', $Config.ControlTargetFile,
                        '--object', $Config.ControlObject, '--admin-dir', '/data')
                    env = @()
                    resources = @{ cpu = [double]$Config.WorkerCpu; memory = $Config.WorkerMemory }
                    volumeMounts = @(
                        @{ volumeName = 'vlite-config'; mountPath = '/run/vlite' },
                        @{ volumeName = 'vlite-data'; mountPath = '/data' }
                    )
                })
            }
        }
    }
}
