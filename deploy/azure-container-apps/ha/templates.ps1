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
    Assert-VLiteHaInteger $Config 'ReaderCapacityBytes' 1 ([long]::MaxValue)
    Assert-VLiteHaInteger $Config 'ReaderStalenessBoundMs' 1 ([long]::MaxValue)
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
        if ($Config.ContainsKey($key) -and $Config[$key] -in @('vlite-app', 'vlite-env', 'vlite-data')) {
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
        Assert-VLiteHaKeys $Config @("${role}Cpu", "${role}Memory")
        $cpu = [double]0
        if (-not [double]::TryParse([string]$Config["${role}Cpu"],
                [Globalization.NumberStyles]::Float, [Globalization.CultureInfo]::InvariantCulture, [ref]$cpu) -or
            $cpu -notin @(0.25, 0.5, 0.75, 1, 1.25, 1.5, 1.75, 2)) {
            throw "Config.${role}Cpu must be an explicit Consumption CPU allocation from 0.25 to 2 in 0.25 increments."
        }
        $memory = ($cpu * 2).ToString('0.0#', [Globalization.CultureInfo]::InvariantCulture) + 'Gi'
        $alternate = ($cpu * 2).ToString('0.##', [Globalization.CultureInfo]::InvariantCulture) + 'Gi'
        if ($Config["${role}Memory"] -cne $memory -and $Config["${role}Memory"] -cne $alternate) {
            throw "Config.${role}Memory must be explicitly specified as $memory (Consumption CPU/RAM pair)."
        }
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
        Assert-VLiteHaInteger $Config 'CreateFormat' 3 5
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
            $arguments = @('/data/vectors.vlite', '--serve-mode', 'writer', '--bind', '0.0.0.0:8080',
                '--allow-remote', '--gateway-trust-file', '/run/vlite/trust.json', '--durability', 'full')
            if ($Config['InitializeTenant']) {
                $arguments += @('--create-if-missing', [string]$Config.Dimensions, '--create-format', [string]$Config.CreateFormat)
            }
            $minimum = 1
            $maximum = 1
        } else {
            $files['archive-target'] = 'archive-target.json'
            $files['reader-config'] = 'reader.json'
            $arguments = @('/data/served/database.vlite', '--serve-mode', 'reader', '--bind', '0.0.0.0:8080',
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
        if ($Role -eq 'Reader') {
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
