#Requires -Version 7.2
# MEAS-004/005 gateway measurement orchestration, dot-sourced by measure.ps1.
# Contract: measure-gateway-contract.md. Exact fixture: measure-gateway-fixture.py.
# The harness builds/collects proposal evidence only. It never qualifies, never
# changes policy/thresholds/gates, and never retries publish or a driver job.
# One bootstrap on the Linux operator host produces the only fixture; every role
# receives only its own verified private projection from that bootstrap.

$script:VLiteMeasIsLinux = [bool]$IsLinux
$script:VLiteMeasGatewayRoles = @('driver', 'gateway', 'writer', 'reader')
$script:VLiteMeasGatewayEntry = '/usr/local/bin/measure-gateway-entrypoint.py'
$script:VLiteMeasGatewayBaseFiles = @{
    gateway = @('gateway.json', 'public.json', 'signing.key', 'measurement.json')
    writer = @('trust.json', 'public.json', 'backend.token', 'measurement.json')
    reader = @('trust.json', 'public.json', 'backend.token', 'measurement.json')
    driver = @('gateway.json', 'trust.json', 'public.json', 'signing.key', 'backend.token', 'directory.json',
        'manifest.json', 'load.expected.json', 'suspension.expected.json', 'measurement.json')
}
$script:VLiteMeasCredentialLifetimeMs = 7200000L
$script:VLiteMeasJobTimeoutSecs = 3600L
$script:VLiteMeasGatewayMarginMs = 120000L

function Test-VLiteMeasGatewayParameters([hashtable]$Config, [hashtable]$Options) {
    $meas005 = $Config.Measurements[0] -ceq 'MEAS-005'
    foreach ($name in 'Tiers', 'Records', 'Delta', 'Dimensions', 'Image') {
        if ($Options.ContainsKey($name)) { throw "$name is not supported for the selected measurement." }
    }
    if ($Config.KeepOnFailure) { throw 'KeepOnFailure is not supported for MEAS-004/005.' }
    if ($Options.ContainsKey('ControlProgram') -and -not $meas005) {
        throw 'ControlProgram is not supported for the selected measurement.'
    }
    if (-not $Config.GatewayProfile) { $Config.GatewayProfile = 'pilot' }
    if ($Config.GatewayProfile -cnotin @('pilot', 'full')) { throw 'GatewayProfile must be pilot or full.' }
    if ($Options.ContainsKey('GatewayPollMs')) {
        if (-not $meas005) { throw 'GatewayPollMs is not supported for the selected measurement.' }
        $poll = 0L
        if ([string]$Config.GatewayPollMs -cnotmatch '\A[0-9]+\z' -or
            -not [long]::TryParse([string]$Config.GatewayPollMs, [ref]$poll) -or
            $poll -lt 1 -or $poll -gt 5000) { throw 'GatewayPollMs is outside the probe limits.' }
        $Config.GatewayPollMs = $poll
    } else {
        $Config.GatewayPollMs = if ($meas005 -and $Config.GatewayProfile -ceq 'pilot') { 100L } else { 1000L }
    }
    foreach ($name in 'FixtureServer', 'ControlProgram') {
        $value = [string]$Config[$name]
        if ($value -and ($value -cnotmatch '\A(?:/[A-Za-z0-9._-]+)+\z' -or $value -cmatch '/\.\.?(?:/|\z)')) {
            throw "$name must be an absolute Linux path."
        }
    }
    if (-not $Config.Plan) {
        if (-not $script:VLiteMeasIsLinux) { throw 'MEAS-004/005 live runs require a Linux/WSL operator host.' }
        if (-not $Config.FixtureServer) { throw 'FixtureServer is required for a live MEAS-004/005 run.' }
        if ($meas005 -and -not $Config.ControlProgram) { throw 'ControlProgram is required for a live MEAS-005 run.' }
    }
    $id = $Config.Id
    $Config.Tiers = @(1)
    $Config.Apps = [ordered]@{ writer = "vlite-meas-$id-wr" }
    if ($meas005) { $Config.Apps.reader = "vlite-meas-$id-rd" }
    $Config.Apps.gateway = "vlite-meas-$id-gw"
    $Config.PublishJob = "vlite-meas-$id-pub"
    $Config.DriverJob = "vlite-meas-$id-drv"
    $Config.Identities = [ordered]@{}
    foreach ($role in $script:VLiteMeasGatewayRoles) { $Config.Identities[$role] = "vlite-meas-$id-$role-id" }
    foreach ($name in @($Config.Apps.Values) + @($Config.PublishJob, $Config.DriverJob) + @($Config.Identities.Values)) {
        Assert-VLiteMeasName $name
    }
}

function Get-VLiteMeasGatewaySettings([hashtable]$Config) {
    $full = $Config.GatewayProfile -ceq 'full'
    $controlPilot = $Config.Measurements[0] -ceq 'MEAS-005' -and -not $full
    $mutations = if ($full) { 20 } else { 3 }
    $deadline = if ($controlPilot) { 1000L } else { 30000L }
    $load = if ($full) {
        [ordered]@{ rates = @(50, 200, 800, 1600); duration_secs = 180; warmup_secs = 30; recovery_secs = 180
            max_in_flight = 256; timeout_ms = 5000 }
    } else {
        [ordered]@{ rates = @(5); duration_secs = 1; warmup_secs = 0; recovery_secs = 0; max_in_flight = 4; timeout_ms = 1000 }
    }
    return @{
        Load = $load
        PollIntervalMs = $Config.GatewayPollMs
        Revocations = if ($controlPilot) { 1 } else { 10 }
        ObservationDeadlineMs = $deadline
        Mutations = $mutations
        DriverBoundMs = $mutations * ($deadline + $Config.GatewayPollMs + 138000L)
        ObserverSecs = if ($full) { 3540L } else { 1200L }
    }
}

function Get-VLiteMeasGatewayScript([string]$Role, [string]$Kind) {
    $entry = "python3 $script:VLiteMeasGatewayEntry --config /measurement/in"
    $lines = @(
        'set -eu', 'umask 077',
        # An unset variable means the intended empty value; config equality still fails closed.
        'export VLITE_RESOURCE_SOURCE_DIRTY_PATHS="${VLITE_RESOURCE_SOURCE_DIRTY_PATHS-}"',
        'mkdir -m 700 /measurement/in', 'cp -L /run/vlite-meas/* /measurement/in/',
        'cp /measurement/in/control-target /measurement/control-target.json',
        "$entry/cfg-restore >/dev/null"
    )
    if ($Role -cin @('writer', 'reader')) {
        $lines += @("mkdir -m 700 /measurement/$Role", "$entry/cfg-copy >/dev/null")
    }
    if ($Kind -ceq 'observe') {
        # The observer report is printed once; the container then idles so no replacement boot resumes the trial.
        $lines += @("$entry/cfg-run || true", 'exec sleep 86400')
    } else {
        $lines += "exec $entry/cfg-run"
    }
    return $lines -join "`n"
}

function ConvertTo-VLiteMeasJson($Value) { return ($Value | ConvertTo-Json -Depth 20 -Compress) }

function Get-VLiteMeasGatewaySecrets([hashtable]$Config, [string]$Role, [string]$Kind, [hashtable]$Values) {
    # Kind: publish | driver | serve | observe. Only the role's own projection is included.
    $settings = Get-VLiteMeasGatewaySettings $Config
    $meas005 = $Config.Measurements[0] -ceq 'MEAS-005'
    $common = [ordered]@{ role = $Role; work_dir = '/measurement'; source_revision = $Config.SourceRevision
        source_dirty_paths = $Config.DirtyPaths }
    function Doc([string]$Operation, [Collections.IDictionary]$Extra) {
        $document = [ordered]@{ operation = $Operation }
        foreach ($key in $common.Keys) { $document[$key] = $common[$key] }
        foreach ($key in $Extra.Keys) { $document[$key] = $Extra[$key] }
        return (ConvertTo-VLiteMeasJson $document)
    }
    $projections = @($Values.Projections[$Role])
    $secrets = [Collections.Generic.List[object]]::new()
    foreach ($projection in $projections) { $secrets.Add([ordered]@{ name = $projection.name; value = $projection.value }) }
    $target = [ordered]@{ account = $Config.StorageAccount; container = 'control-data'
        coordination_container = 'control-coordination'; prefix = "meas-$($Config.Id)/"
        auth = 'managed_identity'; client_id = $Values.ClientIds[$Role] }
    $secrets.Add([ordered]@{ name = 'control-target'; value = (ConvertTo-VLiteMeasJson $target) })
    $secrets.Add([ordered]@{ name = 'cfg-restore'; value = (Doc 'restore' ([ordered]@{
        projection_files = @($projections | ForEach-Object { "/measurement/in/$($_.name)" })
        generation = $Config.Id; sha256 = $Values.ExportSha[$Role]; bootstrap_dir = '/measurement/bootstrap' })) })
    $backend = $Role -cin @('writer', 'reader')
    $database = "/measurement/$Role/database.vlite"
    if ($backend) {
        $secrets.Add([ordered]@{ name = 'cfg-copy'; value = (Doc 'copy_fixture' ([ordered]@{
            fixture_file = '/opt/vlite-measure/fixture/database.vlite'; fixture_sha256 = $Values.FixtureSha
            database_file = $database })) })
    }
    switch ($Kind) {
        'publish' { $run = Doc 'publish' ([ordered]@{ bootstrap_dir = '/measurement/bootstrap' }) }
        'driver' {
            if ($meas005) {
                $run = Doc 'mutate' ([ordered]@{ native_config = '/measurement/in/cfg-native' })
                $native = [ordered]@{ mode = 'run'; server = '/usr/local/bin/vlite-server'
                    bootstrap_dir = '/measurement/bootstrap'; target_file = '/measurement/control-target.json'
                    object = 'gateway/control.json'; gateway_url = $Values.GatewayUrl; poll_interval_ms = $settings.PollIntervalMs
                    observation_deadline_ms = $settings.ObservationDeadlineMs; mutations = $settings.Mutations; seed = 1
                    requested = [ordered]@{ gateway_replicas = '2'; writer_replicas = '1'; reader_replicas = '1'
                        gateway_cpu = '1'; gateway_memory = '2Gi'; writer_cpu = '1'; writer_memory = '2Gi'
                        reader_cpu = '0.5'; reader_memory = '1Gi'; driver_cpu = '1'; driver_memory = '2Gi'
                        routing = 'writer_only'; load = 'idle' } }
            } else {
                $run = Doc 'load' ([ordered]@{ native_config = '/measurement/in/cfg-native' })
                $native = [ordered]@{ gateway_config = '/measurement/bootstrap/gateway.json'
                    token_file = '/measurement/bootstrap/load-0.token'
                    expected_body_file = '/measurement/bootstrap/load.expected.json'
                    gateway_url = $Values.GatewayUrl; direct_url = $Values.WriterUrl }
                foreach ($key in $settings.Load.Keys) { $native[$key] = $settings.Load[$key] }
                $native.requested = [ordered]@{ gateway_cpu = '1'; gateway_memory = '2Gi'; runtime_max_replicas = '2'
                    routing = 'writer_only'; trial = 'fixed-two' }
            }
        }
        default {
            $extra = [ordered]@{ bootstrap_dir = '/measurement/bootstrap' }
            if ($backend) { $extra.database_file = $database; $extra.fixture_sha256 = $Values.FixtureSha }
            if ($Kind -ceq 'observe') {
                $extra.observe_interval_ms = 50; $extra.duration_secs = $settings.ObserverSecs
                $extra.ready_timeout_ms = 1000; $extra.timeout_secs = 3590
                $run = Doc 'observe' $extra
            } else {
                $extra.duration_secs = 7200
                $run = Doc 'serve' $extra
            }
        }
    }
    $secrets.Add([ordered]@{ name = 'cfg-run'; value = $run })
    if ($Kind -ceq 'driver') { $secrets.Add([ordered]@{ name = 'cfg-native'; value = (ConvertTo-VLiteMeasJson $native) }) }
    return $secrets.ToArray()
}

function New-VLiteMeasGatewayManifest([hashtable]$Config, [string]$Role, [string]$Kind, [hashtable]$Values) {
    $identity = $Values.IdentityIds[$Role]
    $resources = switch ($Role) {
        'reader' { [ordered]@{ cpu = 0.5; memory = '1Gi' } }
        default { [ordered]@{ cpu = 1.0; memory = '2Gi' } }
    }
    $secrets = @(Get-VLiteMeasGatewaySecrets $Config $Role $Kind $Values)
    $container = [ordered]@{
        name = $Role; image = $Values.Image; command = @('/bin/sh', '-c'); args = @(Get-VLiteMeasGatewayScript $Role $Kind)
        env = @([ordered]@{ name = 'VLITE_RESOURCE_SOURCE_REVISION'; value = $Config.SourceRevision },
            [ordered]@{ name = 'VLITE_RESOURCE_SOURCE_DIRTY_PATHS'; value = $Config.DirtyPaths })
        resources = $resources
        volumeMounts = @([ordered]@{ volumeName = 'meas'; mountPath = '/run/vlite-meas' })
    }
    $template = [ordered]@{
        containers = @($container)
        volumes = @([ordered]@{ name = 'meas'; storageType = 'Secret'
            secrets = @($secrets | ForEach-Object { [ordered]@{ secretRef = $_.name; path = $_.name } }) })
    }
    $configuration = [ordered]@{
        secrets = $secrets
        registries = @([ordered]@{ server = "$($Config.Registry).azurecr.io"; identity = $identity })
    }
    if ($Kind -cin @('serve', 'observe')) {
        # Explicit TCP probes; liveness tolerates the post-observer idle window so no replacement boot occurs.
        $container.probes = @(
            [ordered]@{ type = 'Startup'; tcpSocket = [ordered]@{ port = 8080 }; periodSeconds = 30; failureThreshold = 10 },
            [ordered]@{ type = 'Readiness'; tcpSocket = [ordered]@{ port = 8080 }; periodSeconds = 5; failureThreshold = 3 },
            [ordered]@{ type = 'Liveness'; tcpSocket = [ordered]@{ port = 8080 }; initialDelaySeconds = 60
                periodSeconds = 240; failureThreshold = 10 })
        $replicas = if ($Role -ceq 'gateway') { 2 } else { 1 }
        $configuration.activeRevisionsMode = 'Single'
        $configuration.ingress = [ordered]@{ external = $false; targetPort = 8080; transport = 'http'; allowInsecure = $false }
        $template.scale = [ordered]@{ minReplicas = $replicas; maxReplicas = $replicas }
    } else {
        $configuration.triggerType = 'Manual'
        $configuration.replicaTimeout = $script:VLiteMeasJobTimeoutSecs
        $configuration.replicaRetryLimit = 0
        $configuration.manualTriggerConfig = [ordered]@{ parallelism = 1; replicaCompletionCount = 1 }
    }
    return [ordered]@{
        location = $Config.Location
        identity = [ordered]@{ type = 'UserAssigned'; userAssignedIdentities = [ordered]@{ $identity = @{} } }
        properties = [ordered]@{ environmentId = $Values.EnvironmentId; workloadProfileName = 'Consumption'
            configuration = $configuration; template = $template }
    }
}

function Get-VLiteMeasGatewayValues([hashtable]$Config, [string]$Subscription, [string]$Domain) {
    $base = "/subscriptions/$Subscription/resourceGroups/$($Config.ResourceGroup)"
    $values = @{
        EnvironmentId = "$base/providers/Microsoft.App/managedEnvironments/$($Config.Environment)"
        IdentityIds = @{}; ClientIds = @{}; Projections = @{}; ExportSha = @{}
        GatewayUrl = "https://$($Config.Apps.gateway).internal.$Domain"
        WriterUrl = "https://$($Config.Apps.writer).internal.$Domain"
        FixtureSha = '<fixture-sha256>'
        Image = "$($Config.Registry).azurecr.io/gateway-measure-fixture@<fixture-image-digest>"
    }
    foreach ($role in $script:VLiteMeasGatewayRoles) {
        $values.IdentityIds[$role] = "$base/providers/Microsoft.ManagedIdentity/userAssignedIdentities/$($Config.Identities[$role])"
        $values.ClientIds[$role] = "<client-id:$role>"
        $values.ExportSha[$role] = "<export-sha256:$role>"
        $values.Projections[$role] = @([ordered]@{ name = "<projections:$role>"; value = '<redacted>' })
    }
    return $values
}

function New-VLiteMeasFixturePlan([hashtable]$Config, [string]$Domain, [string]$Digest) {
    $server = if ($Config.FixtureServer) { $Config.FixtureServer } else { '<fixture-server>' }
    $settings = Get-VLiteMeasGatewaySettings $Config
    return [ordered]@{
        server = $server; writer_url = "https://$($Config.Apps.writer).internal.$Domain"; audience = 'measurement-writer'
        control_target = [ordered]@{ account = $Config.StorageAccount; container = 'control-data'
            coordination_container = 'control-coordination'; prefix = "meas-$($Config.Id)/" }
        control_object = 'gateway/control.json'; generation = $Config.Id; registry = $Config.Registry
        base_image = "$($Config.Registry).azurecr.io/gateway-measure@$Digest"; fixture_repository = 'gateway-measure-fixture'
        poll_interval_ms = $settings.PollIntervalMs; revocations = $settings.Revocations
    }
}

function New-VLiteMeasGatewayPlan([hashtable]$Config) {
    $sub = '<subscription>'
    $rg = $Config.ResourceGroup
    $base = "/subscriptions/$sub/resourceGroups/$rg"
    $registryId = "$base/providers/Microsoft.ContainerRegistry/registries/$($Config.Registry)"
    $storageId = "$base/providers/Microsoft.Storage/storageAccounts/$($Config.StorageAccount)"
    $expires = [DateTime]::UtcNow.AddDays(1).ToString('yyyy-MM-ddTHH:mm:ssZ')
    $meas005 = $Config.Measurements[0] -ceq 'MEAS-005'
    $here = Join-Path $script:VLiteMeasRepository 'deploy/azure-container-apps/ha'
    $fixtureScript = Join-Path $here 'measure-gateway-fixture.py'
    $values = Get-VLiteMeasGatewayValues $Config $sub '<default-domain>'
    $fixtureTag = "gateway-measure-fixture:$($Config.Id)-<fixture-sha12>"
    $steps = [Collections.Generic.List[object]]::new()
    function Add([string]$Name, [string[]]$Arguments, [string]$Role = '', [string]$Tool = 'az', $Manifest = $null) {
        $stepArguments = if ($Tool -ceq 'az') { @($Arguments + @('--subscription', $sub)) } else { @($Arguments) }
        $steps.Add([pscustomobject]@{ Step = $Name; Role = $Role; Tool = $Tool; Arguments = $stepArguments; Manifest = $Manifest })
    }
    function Logs([string]$Prefix, [string]$Job, [string]$Container) {
        Add "Read$Prefix" @('containerapp', 'job', 'logs', 'show', '--name', $Job, '--resource-group', $rg,
            '--execution', '<execution>', '--container', $Container, '--tail', '300', '--format', 'text')
        Add "Query$Prefix" @('monitor', 'log-analytics', 'query', '--workspace', '<workspace-id>', '--analytics-query',
            "ContainerAppConsoleLogs_CL | where tostring(column_ifexists('ContainerGroupName_s', column_ifexists('ContainerGroupName_g', ''))) startswith '<execution>' | order by TimeGenerated asc | project Log_s",
            '--timespan', 'PT4H')
    }
    Add 'CheckGroup' @('group', 'exists', '--name', $rg)
    Add 'CheckRegistryName' @('acr', 'check-name', '--name', $Config.Registry)
    Add 'CheckStorageName' @('storage', 'account', 'check-name', '--name', $Config.StorageAccount)
    Add 'CreateGroup' @('group', 'create', '--name', $rg, '--location', $Config.Location, '--tags',
        "vliteMeasurement=$($Config.Id)", 'vliteMeasurementOwner=ha-harness', "vliteMeasurementExpires=$expires")
    Add 'CreateRegistry' @('acr', 'create', '--name', $Config.Registry, '--resource-group', $rg, '--sku', 'Basic',
        '--admin-enabled', 'false')
    Add 'BuildMeasureImage' @('acr', 'run', '--registry', $Config.Registry, '--timeout', '7200', '--no-logs',
        '--file', 'deploy/azure-container-apps/ha/gateway-build.yaml', '--set', "revision=$($Config.SourceRevision)",
        $script:VLiteMeasRepository)
    Add 'ShowMeasureImage' @('acr', 'repository', 'show', '--name', $Config.Registry, '--image',
        "gateway-measure:$($Config.SourceRevision)")
    foreach ($role in $script:VLiteMeasGatewayRoles) {
        Add 'CreateIdentity' @('identity', 'create', '--name', $Config.Identities[$role], '--resource-group', $rg) $role
    }
    foreach ($role in $script:VLiteMeasGatewayRoles) {
        Add 'GrantPull' @('role', 'assignment', 'create', '--assignee-object-id', "<principal-id:$role>",
            '--assignee-principal-type', 'ServicePrincipal', '--role', 'AcrPull', '--scope', $registryId) $role
    }
    Add 'CreateStorage' @('storage', 'account', 'create', '--name', $Config.StorageAccount, '--resource-group', $rg,
        '--location', $Config.Location, '--sku', 'Standard_LRS', '--kind', 'StorageV2', '--allow-shared-key-access', 'false',
        '--allow-blob-public-access', 'false', '--min-tls-version', 'TLS1_2', '--https-only', 'true')
    foreach ($container in 'control-data', 'control-coordination') {
        Add 'CreateControlContainer' @('storage', 'container-rm', 'create', '--storage-account', $Config.StorageAccount,
            '--resource-group', $rg, '--name', $container) $container
    }
    foreach ($role in $script:VLiteMeasGatewayRoles) {
        # Only the driver may write control state; serving roles are read-only.
        $access = if ($role -ceq 'driver') { 'Storage Blob Data Contributor' } else { 'Storage Blob Data Reader' }
        Add 'GrantControl' @('role', 'assignment', 'create', '--assignee-object-id', "<principal-id:$role>",
            '--assignee-principal-type', 'ServicePrincipal', '--role', $access, '--scope', $storageId) $role
    }
    Add 'CreateEnvironment' @('containerapp', 'env', 'create', '--name', $Config.Environment, '--resource-group', $rg,
        '--location', $Config.Location, '--enable-workload-profiles', 'true')
    Add 'PrepareFixture' @($fixtureScript, 'prepare', '--plan', '<fixture-plan>') '' 'python3' `
        (New-VLiteMeasFixturePlan $Config '<default-domain>' '<measure-digest>')
    Add 'BuildFixtureImage' @('acr', 'build', '--registry', $Config.Registry, '--platform', 'linux/amd64', '--no-logs',
        '--file', '/measurement/image/Dockerfile', '--image', $fixtureTag, '/measurement/image', '--resource-group', $rg)
    Add 'ShowFixtureImage' @('acr', 'repository', 'show', '--name', $Config.Registry, '--image', $fixtureTag)
    Add 'CreatePublishJob' @('containerapp', 'job', 'create', '--name', $Config.PublishJob, '--resource-group', $rg,
        '--yaml', '<manifest:publish>') 'driver' 'az' (New-VLiteMeasGatewayManifest $Config 'driver' 'publish' $values)
    Add 'StartPublish' @('containerapp', 'job', 'start', '--name', $Config.PublishJob, '--resource-group', $rg)
    Add 'PollPublish' @('containerapp', 'job', 'execution', 'show', '--name', $Config.PublishJob, '--resource-group', $rg,
        '--job-execution-name', '<execution>')
    Logs 'Publish' $Config.PublishJob 'driver'
    $serveKind = if ($meas005) { 'observe' } else { 'serve' }
    foreach ($role in $Config.Apps.Keys) {
        Add 'CreateApp' @('containerapp', 'create', '--name', $Config.Apps[$role], '--resource-group', $rg,
            '--yaml', "<manifest:$role>") $role 'az' (New-VLiteMeasGatewayManifest $Config $role $serveKind $values)
    }
    Add 'CreateDriverJob' @('containerapp', 'job', 'create', '--name', $Config.DriverJob, '--resource-group', $rg,
        '--yaml', '<manifest:driver>') 'driver' 'az' (New-VLiteMeasGatewayManifest $Config 'driver' 'driver' $values)
    Add 'CleanupFixture' @($fixtureScript, 'cleanup') '' 'python3'
    foreach ($role in $Config.Apps.Keys) {
        Add 'InventoryBefore' @('containerapp', 'replica', 'list', '--name', $Config.Apps[$role], '--resource-group', $rg) $role
    }
    Add 'StartDriver' @('containerapp', 'job', 'start', '--name', $Config.DriverJob, '--resource-group', $rg)
    Add 'PollDriver' @('containerapp', 'job', 'execution', 'show', '--name', $Config.DriverJob, '--resource-group', $rg,
        '--job-execution-name', '<execution>')
    Logs 'Driver' $Config.DriverJob 'driver'
    foreach ($role in $Config.Apps.Keys) {
        Add 'InventoryAfter' @('containerapp', 'replica', 'list', '--name', $Config.Apps[$role], '--resource-group', $rg) $role
    }
    if ($meas005) {
        Add 'ReadObserver' @('containerapp', 'logs', 'show', '--name', '<app>', '--resource-group', $rg,
            '--revision', '<revision>', '--replica', '<replica>',
            '--container', '<container>', '--tail', '300', '--format', 'text', '--type', 'console')
        Add 'QueryObserver' @('monitor', 'log-analytics', 'query', '--workspace', '<workspace-id>', '--analytics-query',
            "ContainerAppConsoleLogs_CL | where ContainerAppName_s == '<app>' and tostring(column_ifexists('ContainerGroupName_s', '')) == '<replica>' | order by TimeGenerated asc | project Log_s",
            '--timespan', 'PT4H')
        Add 'Analyze' @((Join-Path $here 'measure-gateway-entrypoint.py'), '--config', '<analyze-config>') '' 'python3'
    }
    Add 'DeleteGroup' @('group', 'delete', '--name', $rg, '--yes', '--no-wait')
    Add 'WaitDeleted' @('group', 'exists', '--name', $rg)
    return $steps.ToArray()
}

# ---- Mockable host boundaries (tests replace these; none run in -Plan). ----

function Get-VLiteMeasUnixMs { return [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() }

function Invoke-VLiteMeasLocal([string]$FilePath, [string[]]$Arguments, [hashtable]$Environment) {
    # `env` preserves intentionally empty provenance values; stderr is never captured.
    $assignments = @($Environment.Keys | Sort-Object | ForEach-Object { "$_=$($Environment[$_])" })
    $output = @(& env @assignments $FilePath @Arguments 2>$null)
    return @{ ExitCode = $LASTEXITCODE; Output = $output }
}

function Test-VLiteMeasFixtureRootEmpty {
    $root = '/measurement'
    if (-not (Test-Path -LiteralPath $root -PathType Container)) { return $false }
    $item = Get-Item -LiteralPath $root -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { return $false }
    return @(Get-ChildItem -LiteralPath $root -Force).Count -eq 0
}

function Get-VLiteMeasFixtureFile([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint -or $item.Length -gt 1048576) {
        throw (New-VLiteMeasFailure 'FixtureExport' 'Fixture export file is unsafe.')
    }
    return [IO.File]::ReadAllText($Path, [Text.UTF8Encoding]::new($false, $true))
}

function New-VLiteMeasPrivateDirectory([string]$Parent) {
    if (-not $Parent) { return [IO.Directory]::CreateTempSubdirectory('vlite-meas-').FullName }
    $path = Join-Path $Parent ([guid]::NewGuid().ToString('N'))
    if ($IsWindows) { $null = [IO.Directory]::CreateDirectory($path) }
    else { $null = [IO.Directory]::CreateDirectory($path, [IO.UnixFileMode]'UserRead, UserWrite, UserExecute') }
    return $path
}

function Set-VLiteMeasPrivate([string]$Path, $Content) {
    $options = [IO.FileStreamOptions]::new()
    $options.Mode = [IO.FileMode]::CreateNew
    $options.Access = [IO.FileAccess]::Write
    if (-not $IsWindows) { $options.UnixCreateMode = [IO.UnixFileMode]'UserRead, UserWrite' }
    $bytes = if ($Content -is [byte[]]) { $Content } else { [Text.UTF8Encoding]::new($false).GetBytes([string]$Content) }
    $stream = [IO.FileStream]::new($Path, $options)
    try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
}

# ---- Strict fixture verification. ----

function Assert-VLiteMeasKeys($Object, [string[]]$Expected, [string]$Reason) {
    if ($Object -isnot [Collections.IDictionary] -or
        ((@($Object.Keys) | Sort-Object) -join ',') -cne (($Expected | Sort-Object) -join ',')) {
        throw (New-VLiteMeasFailure $Reason 'Fixture document has an invalid schema.')
    }
}

function Test-VLiteMeasInteger($Value, [long]$Minimum, [long]$Maximum) {
    return ($Value -is [long] -or $Value -is [int]) -and $Value -ge $Minimum -and $Value -le $Maximum
}

function Test-VLiteMeasSource($Source, [hashtable]$Config) {
    if ($Source -isnot [Collections.IDictionary] -or
        ((@($Source.Keys) | Sort-Object) -join ',') -cne 'dirty_paths,revision') { return $false }
    $expected = if ($Config.DirtyPaths) { @($Config.DirtyPaths -split "`n") } else { @() }
    $actual = $Source.dirty_paths
    if ($Source.revision -cne $Config.SourceRevision -or $actual -isnot [array] -or $actual.Count -ne $expected.Count) {
        return $false
    }
    for ($i = 0; $i -lt $expected.Count; $i++) {
        if ($actual[$i] -isnot [string] -or $actual[$i] -cne $expected[$i]) { return $false }
    }
    return $true
}

function Test-VLiteMeasRoleFile([string]$Name, [string]$Role) {
    if ($script:VLiteMeasGatewayBaseFiles[$Role] -ccontains $Name) { return $true }
    if ($Role -cne 'driver') { return $false }
    return ($Name -cmatch '\A(?:load-[0-9]{1,3}|suspension-0)\.token\z') -or
        ($Name -cmatch '\Aadmin/[A-Za-z0-9._-]{1,128}\z' -and $Name -cnotin @('admin/.', 'admin/..'))
}

function Expand-VLiteMeasGzip([byte[]]$Bytes, [int]$Limit) {
    $input = [IO.MemoryStream]::new($Bytes, $false)
    $output = [IO.MemoryStream]::new()
    $gzip = [IO.Compression.GZipStream]::new($input, [IO.Compression.CompressionMode]::Decompress)
    try {
        $buffer = [byte[]]::new(65536)
        while (($read = $gzip.Read($buffer, 0, $buffer.Length)) -gt 0) {
            $output.Write($buffer, 0, $read)
            if ($output.Length -gt $Limit) { throw 'limit' }
        }
        return ,$output.ToArray()
    } finally { $gzip.Dispose(); $input.Dispose(); $output.Dispose() }
}

function ConvertFrom-VLiteMeasJsonBytes([byte[]]$Bytes) {
    $text = [Text.UTF8Encoding]::new($false, $true).GetString($Bytes)
    $document = $text | ConvertFrom-Json -AsHashtable -NoEnumerate
    if ($document -isnot [Collections.IDictionary]) { throw 'Expected a JSON object.' }
    return $document
}

function Assert-VLiteMeasFixtureExport([string]$Text, [string]$Role, [hashtable]$Config, $Expected) {
    $document = $Text | ConvertFrom-Json -AsHashtable -NoEnumerate
    Assert-VLiteMeasKeys $document @('schema', 'role', 'generation', 'bootstrap_dir', 'sha256', 'compressed_bytes', 'secrets') 'FixtureExport'
    $secrets = $document.secrets
    if ($document.schema -cne 'vlite-meas-secrets/1' -or $document.role -cne $Role -or
        $document.generation -cne $Config.Id -or $document.bootstrap_dir -cne '/measurement/bootstrap' -or
        $document.sha256 -cne $Expected.sha256 -or $document.compressed_bytes -ne $Expected.compressed_bytes -or
        $secrets -isnot [array] -or $secrets.Count -ne $Expected.projections) {
        throw (New-VLiteMeasFailure 'FixtureExport' 'Fixture export identity differs from the bootstrap receipt.')
    }
    $projections = [Collections.Generic.List[object]]::new()
    $payload = [Text.StringBuilder]::new()
    for ($i = 0; $i -lt $secrets.Count; $i++) {
        $item = $secrets[$i]
        Assert-VLiteMeasKeys $item @('name', 'value') 'FixtureExport'
        if ($item.name -cne "m-$Role-$($Config.Id)-$($i + 1)" -or $item.value -isnot [string] -or
            $item.value -cnotmatch '\A[A-Za-z0-9+/=]{1,3000}\z') {
            throw (New-VLiteMeasFailure 'FixtureExport' 'Fixture export projection is invalid.')
        }
        $null = $payload.Append($item.value)
        $projections.Add([ordered]@{ name = $item.name; value = $item.value })
    }
    try {
        $compressed = [Convert]::FromBase64String($payload.ToString())
        $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($compressed)).ToLowerInvariant()
        if ($hash -cne $document.sha256 -or $compressed.Length -ne $document.compressed_bytes) { throw 'digest' }
        $bundle = ConvertFrom-VLiteMeasJsonBytes (Expand-VLiteMeasGzip $compressed 2097152)
    } catch { throw (New-VLiteMeasFailure 'FixtureExport' 'Fixture export digest or encoding mismatch.') }
    Assert-VLiteMeasKeys $bundle @('schema', 'role', 'generation', 'bootstrap_dir', 'source', 'files') 'FixtureExport'
    $files = $bundle.files
    if ($bundle.schema -cne 'vlite-meas-bundle/1' -or $bundle.role -cne $Role -or $bundle.generation -cne $Config.Id -or
        $bundle.bootstrap_dir -cne '/measurement/bootstrap' -or -not (Test-VLiteMeasSource $bundle.source $Config) -or
        $files -isnot [Collections.IDictionary] -or $files.Count -gt 256) {
        throw (New-VLiteMeasFailure 'FixtureExport' 'Fixture bundle identity differs.')
    }
    foreach ($name in $script:VLiteMeasGatewayBaseFiles[$Role]) {
        if (-not $files.Contains($name)) { throw (New-VLiteMeasFailure 'FixtureRole' 'Fixture bundle lacks a role file.') }
    }
    foreach ($name in @($files.Keys)) {
        if (-not (Test-VLiteMeasRoleFile $name $Role) -or $files[$name] -isnot [string]) {
            throw (New-VLiteMeasFailure 'FixtureRole' 'Fixture bundle contains a file outside the role allowance.')
        }
    }
    try { $metadata = ConvertFrom-VLiteMeasJsonBytes ([Convert]::FromBase64String($files['measurement.json'])) }
    catch { throw (New-VLiteMeasFailure 'FixtureExport' 'Fixture bundle metadata is invalid.') }
    if ($metadata.schema -cne 'vlite-meas-artifacts/1' -or -not (Test-VLiteMeasSource $metadata.source $Config) -or
        $metadata.fixture_sha256 -cne $Expected.fixture_sha256 -or $metadata.lineage -isnot [long] -and $metadata.lineage -isnot [int]) {
        throw (New-VLiteMeasFailure 'FixtureLineage' 'Fixture bundle lineage differs from the bootstrap.')
    }
    return @{ Projections = $projections.ToArray(); Lineage = [long]$metadata.lineage }
}

function Assert-VLiteMeasFixtureSummary([string[]]$Output, [hashtable]$Config, [hashtable]$Expected) {
    $lines = @($Output | Where-Object { $_ -ne '' })
    if ($lines.Count -ne 1) { throw (New-VLiteMeasFailure 'FixtureSummary' 'Fixture preparation output is not one summary.') }
    try { $summary = $lines[0] | ConvertFrom-Json -AsHashtable -NoEnumerate }
    catch { throw (New-VLiteMeasFailure 'FixtureSummary' 'Fixture preparation output is not JSON.') }
    Assert-VLiteMeasKeys $summary @('schema', 'qualification', 'source', 'bootstrap_dir', 'control_target_file',
        'fixture_sha256', 'generation', 'credentials_expire_not_before_unix_ms', 'exports', 'image', 'executed') 'FixtureSummary'
    $sha = $summary.fixture_sha256
    if ($summary.schema -cne 'vlite-meas-fixture-prep/1' -or $summary.qualification -cne 'proposal_evidence_only' -or
        -not (Test-VLiteMeasSource $summary.source $Config) -or $summary.bootstrap_dir -cne '/measurement/bootstrap' -or
        $summary.control_target_file -cne '/measurement/control-target.json' -or $sha -isnot [string] -or
        $sha -cnotmatch '\A[a-f0-9]{64}\z' -or $summary.generation -cne $Config.Id -or
        $summary.executed -isnot [array] -or $summary.executed.Count -ne 0) {
        throw (New-VLiteMeasFailure 'FixtureSummary' 'Fixture summary identity differs.')
    }
    $deadline = $summary.credentials_expire_not_before_unix_ms
    $earliest = $Expected.Start + $script:VLiteMeasCredentialLifetimeMs - 1000
    $latest = $Expected.End + $script:VLiteMeasCredentialLifetimeMs
    if (-not (Test-VLiteMeasInteger $deadline $earliest $latest)) {
        throw (New-VLiteMeasFailure 'FixtureExpiry' 'Fixture credential deadline is outside this bootstrap window.')
    }
    $image = $summary.image
    Assert-VLiteMeasKeys $image @('context_dir', 'fixture_file', 'base_image', 'image', 'planned_build') 'FixtureSummary'
    $sha12 = $sha.Substring(0, 12)
    $tag = "gateway-measure-fixture:$($Config.Id)-$sha12"
    $planned = @($Expected.PlannedBuild | ForEach-Object { $_.Replace('<fixture-sha12>', $sha12) })
    if ($image.context_dir -cne '/measurement/image' -or $image.fixture_file -cne '/opt/vlite-measure/fixture/database.vlite' -or
        $image.base_image -cne $Expected.BaseImage -or $image.image -cne "$($Config.Registry).azurecr.io/$tag" -or
        $image.planned_build -isnot [array] -or ($image.planned_build -join "`n") -cne ($planned -join "`n") -or
        $image.planned_build.Count -ne $planned.Count) {
        throw (New-VLiteMeasFailure 'FixtureBuild' 'Fixture image plan differs from the harness plan.')
    }
    Assert-VLiteMeasKeys $summary.exports $script:VLiteMeasGatewayRoles 'FixtureSummary'
    $result = @{ FixtureSha = $sha; Sha12 = $sha12; Deadline = [long]$deadline; Projections = @{}; ExportSha = @{} }
    $lineage = $null
    foreach ($role in $script:VLiteMeasGatewayRoles) {
        $export = $summary.exports[$role]
        Assert-VLiteMeasKeys $export @('file', 'sha256', 'compressed_bytes', 'projections') 'FixtureSummary'
        if ($export.file -cne "/measurement/exports/$role.json" -or $export.sha256 -isnot [string] -or
            $export.sha256 -cnotmatch '\A[a-f0-9]{64}\z' -or -not (Test-VLiteMeasInteger $export.compressed_bytes 1 144000) -or
            -not (Test-VLiteMeasInteger $export.projections 1 64)) {
            throw (New-VLiteMeasFailure 'FixtureSummary' 'Fixture export receipt is invalid.')
        }
        $verified = Assert-VLiteMeasFixtureExport (Get-VLiteMeasFixtureFile $export.file) $role $Config @{
            sha256 = $export.sha256; compressed_bytes = $export.compressed_bytes; projections = $export.projections
            fixture_sha256 = $sha }
        if ($null -ne $lineage -and $verified.Lineage -ne $lineage) {
            throw (New-VLiteMeasFailure 'FixtureLineage' 'Fixture exports come from different bootstraps.')
        }
        $lineage = $verified.Lineage
        $result.Projections[$role] = $verified.Projections
        $result.ExportSha[$role] = $export.sha256
    }
    return $result
}

function Assert-VLiteMeasGatewayReport([byte[]]$Bytes, [string]$Schema, [hashtable]$Config, [string[]]$PrivateValues) {
    $text = [Text.Encoding]::UTF8.GetString($Bytes)
    foreach ($value in $PrivateValues) {
        if ($value -and $text.Contains($value, [StringComparison]::OrdinalIgnoreCase)) {
            throw (New-VLiteMeasFailure 'PrivateIdentifier' 'Evidence contains private identifiers.')
        }
    }
    $document = ConvertFrom-VLiteMeasJsonBytes $Bytes
    if ($document.schema -cne $Schema -or $document.qualification -cne 'proposal_evidence_only' -or
        -not (Test-VLiteMeasSource $document.source $Config)) {
        throw (New-VLiteMeasFailure 'EvidenceIdentity' 'Evidence schema, qualification or provenance differs.')
    }
    return $document
}

function Assert-VLiteMeasGatewayTime([long]$Deadline, [long]$NeededMs) {
    if ((Get-VLiteMeasUnixMs) + $NeededMs -gt $Deadline) {
        throw (New-VLiteMeasFailure 'CredentialWindow' 'The fixture credential window is insufficient; refusing to continue.')
    }
}

# ---- Live orchestration. ----

function Invoke-VLiteMeasGateway([hashtable]$Config) {
    $steps = @(New-VLiteMeasGatewayPlan $Config)
    if ($Config.Plan) {
        foreach ($step in $steps) {
            $label = if ($step.Role) { "$($step.Step)[$($step.Role)]" } else { $step.Step }
            Write-Information ("{0}: {1} {2}" -f $label, $step.Tool, (ConvertTo-Json -InputObject $step.Arguments -Compress)) -InformationAction Continue
        }
        return $steps
    }
    if ([Text.Encoding]::UTF8.GetByteCount([string]$Config.DirtyPaths) -gt 16384) {
        throw 'Dirty path provenance exceeds 16384 bytes; refusing an unverifiable run.'
    }
    $measurement = $Config.Measurements[0]
    $meas005 = $measurement -ceq 'MEAS-005'
    $settings = Get-VLiteMeasGatewaySettings $Config
    $provenance = @{ VLITE_RESOURCE_SOURCE_REVISION = $Config.SourceRevision; VLITE_RESOURCE_SOURCE_DIRTY_PATHS = $Config.DirtyPaths }
    $replacements = @{ '<subscription>' = $Config.SubscriptionId }
    function Find([string]$Name, [string]$Role = '') {
        $found = @($steps | Where-Object { $_.Step -ceq $Name -and $_.Role -ceq $Role })
        if ($found.Count -ne 1) { throw 'Internal measurement plan error.' }
        return $found[0]
    }
    function Expand([string]$Name, [string]$Role = '', [hashtable]$Extra = @{}) {
        $map = $replacements + $Extra
        @((Find $Name $Role).Arguments | ForEach-Object {
            $value = $_
            foreach ($key in $map.Keys) { $value = $value.Replace($key, [string]$map[$key]) }
            if ($value -cmatch '<[a-z0-9-]+(?::[a-z0-9-]+)?>') { throw 'Unresolved measurement plan placeholder.' }
            $value
        })
    }
    function InvokeGatewayStep([string]$Name, [string]$Role = '', [hashtable]$Extra = @{}) { Invoke-VLiteMeasAz (Expand $Name $Role $Extra) }
    function Fixture([string]$Name, [hashtable]$Extra = @{}) {
        Invoke-VLiteMeasLocal 'python3' (Expand $Name '' $Extra) $provenance
    }
    $exists = InvokeGatewayStep 'CheckGroup'
    if ($exists -isnot [bool]) { throw 'Group existence check did not return a boolean.' }
    if ($exists) { throw 'Measurement resource group already exists; refusing to reuse or delete it.' }
    foreach ($check in 'CheckRegistryName', 'CheckStorageName') {
        $availability = InvokeGatewayStep $check
        if ($availability.nameAvailable -isnot [bool] -or -not $availability.nameAvailable) {
            throw 'Measurement resource name is unavailable; refusing preexisting or foreign resources.'
        }
    }
    # Never clean a fixture root this run did not prove empty.
    if (-not (Test-VLiteMeasFixtureRootEmpty)) { throw 'The /measurement fixture root must exist, be private and be empty.' }
    $null = [IO.Directory]::CreateDirectory($Config.OutDir)
    $summary = [ordered]@{
        measurement = $measurement; profile = $Config.GatewayProfile; qualification = 'proposal_evidence_only'
        sourceRevision = $Config.SourceRevision; dirty = [bool]$Config.DirtyPaths; resourceGroup = $Config.ResourceGroup
        status = 'Running'; measurementStatus = 'Running'; incompleteReasons = [Collections.Generic.List[string]]::new()
        failedStep = $null; failureReason = $null; fixtureSha256 = $null; credentialsExpireNotBeforeUnixMs = $null
        publishExecution = $null; driverExecution = $null; driverStatus = $null
        inventory = [ordered]@{ before = $null; after = $null; fixed = $false }
        evidence = [Collections.Generic.List[object]]::new()
        fixtureCleanup = 'NotStarted'; cleanupStatus = 'Pending'; cleanupFailedStep = $null
    }
    $privateValues = [Collections.Generic.List[string]]::new()
    $privateValues.Add($Config.SubscriptionId)
    $work = $null; $prepared = $false; $stage = 'Start'; $workspace = ''
    function Save-Evidence([string]$Name, [byte[]]$Bytes, [string]$Kind) {
        [IO.File]::WriteAllBytes((Join-Path $Config.OutDir $Name), $Bytes)
        $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($Bytes)).ToLowerInvariant()
        $summary.evidence.Add([ordered]@{ kind = $Kind; file = $Name; sha256 = $hash })
    }
    function Read-JobEvidence([string]$Prefix, [string]$Execution) {
        $diagnostic = [ordered]@{}
        try {
            return (Get-VLiteMeasEvidence (Expand "Read$Prefix" '' @{ '<execution>' = $Execution }) `
                (Expand "Query$Prefix" '' @{ '<execution>' = $Execution; '<workspace-id>' = $workspace }) $diagnostic).Bytes
        } catch { throw (New-VLiteMeasFailure "${Prefix}Evidence" 'Job evidence was unavailable or failed verification.') }
    }
    function Start-Job([string]$Name, [string]$Job) {
        # No retry: publish and driver writes are unknown_do_not_retry once attempted.
        $started = InvokeGatewayStep $Name
        $execution = [string]$started.name
        if ($execution -cnotmatch "\A$([regex]::Escape($Job))-[a-z0-9]{1,32}\z") {
            throw (New-VLiteMeasFailure 'JobStart' 'Job start returned a foreign or invalid execution name.')
        }
        return $execution
    }
    function Read-Inventory([string]$Name) {
        $inventory = [ordered]@{}; $ready = $true
        foreach ($role in @($Config.Apps.Keys | Sort-Object -CaseSensitive)) {
            $listed = InvokeGatewayStep $Name $role
            if ($listed -isnot [array]) { throw (New-VLiteMeasFailure 'Inventory' 'Replica inventory is not a list.') }
            $app = $Config.Apps[$role]
            $prefix = "/subscriptions/$($Config.SubscriptionId)/resourceGroups/$($Config.ResourceGroup)/providers/Microsoft.App/containerApps/$app/revisions/"
            $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
            $replicas = @(foreach ($replica in $listed) {
                $replicaName = [string]$replica.name
                $identity = [regex]::Match([string]$replica.id,
                    "\A$([regex]::Escape($prefix))(?<revision>[a-z0-9][a-z0-9-]{0,252})/replicas/$([regex]::Escape($replicaName))\z",
                    [Text.RegularExpressions.RegexOptions]::IgnoreCase)
                if ($replicaName -cnotmatch '\A[a-z0-9][a-z0-9-]{0,252}\z' -or
                    -not $identity.Success -or -not $seen.Add($replicaName)) {
                    throw (New-VLiteMeasFailure 'Inventory' 'Replica inventory contains a missing, duplicate or foreign identity.')
                }
                $revision = $identity.Groups['revision'].Value
                $restarts = 0L
                if (@($replica.properties.containers).Count -ne 1) {
                    throw (New-VLiteMeasFailure 'Inventory' 'Replica inventory lacks its single measurement container.')
                }
                foreach ($container in @($replica.properties.containers)) {
                    if ([string]$container.name -cne $role -or
                        ($container.restartCount -isnot [int] -and $container.restartCount -isnot [long]) -or
                        $container.restartCount -lt 0 -or $container.restartCount -gt [int]::MaxValue) {
                        throw (New-VLiteMeasFailure 'Inventory' 'Replica inventory lacks its container identity or restart count.')
                    }
                    $restarts += [long]$container.restartCount
                    if ($container.ready -ne $true -or [string]$container.runningState -cne 'Running') { $ready = $false }
                }
                [ordered]@{ app = $app; revision = $revision; name = $replicaName; container = $role
                    created = [string]$replica.properties.createdTime; restarts = $restarts }
            })
            $expected = if ($role -ceq 'gateway') { 2 } else { 1 }
            if ($replicas.Count -ne $expected) { $ready = $false }
            $inventory[$role] = @($replicas | Sort-Object -Property { $_['name'] } -CaseSensitive)
        }
        return @{ Inventory = $inventory; Ready = $ready }
    }
    function Add-Incomplete([string]$Reason) { if (-not $summary.incompleteReasons.Contains($Reason)) { $summary.incompleteReasons.Add($Reason) } }
    try {
        $work = New-VLiteMeasPrivateDirectory ''
        foreach ($name in 'CreateGroup', 'CreateRegistry') { $stage = $name; $null = InvokeGatewayStep $name }
        $stage = 'BuildMeasureImage'
        $build = InvokeGatewayStep 'BuildMeasureImage'
        if ($build.status -cne 'Succeeded') { throw (New-VLiteMeasFailure 'GatewayImageBuild' 'Gateway image build did not succeed.') }
        $stage = 'ShowMeasureImage'
        $digest = [string](InvokeGatewayStep 'ShowMeasureImage').digest
        if ($digest -cnotmatch '\Asha256:[a-f0-9]{64}\z') { throw 'Measurement image digest is invalid.' }
        $values = Get-VLiteMeasGatewayValues $Config $Config.SubscriptionId '<pending>'
        $principals = @{}
        foreach ($role in $script:VLiteMeasGatewayRoles) {
            $stage = 'CreateIdentity'
            $identity = InvokeGatewayStep 'CreateIdentity' $role
            if ($identity -isnot [Collections.IDictionary] -or [string]$identity.id -ine $values.IdentityIds[$role] -or
                -not $identity.principalId -or -not $identity.clientId) {
                throw 'Created identity lacks its expected ID, principal or client.'
            }
            $principals[$role] = [string]$identity.principalId
            $values.ClientIds[$role] = [string]$identity.clientId
            $replacements["<principal-id:$role>"] = $principals[$role]
            $privateValues.Add($principals[$role]); $privateValues.Add($values.ClientIds[$role])
        }
        foreach ($role in $script:VLiteMeasGatewayRoles) { $stage = 'GrantPull'; $null = InvokeGatewayStep 'GrantPull' $role }
        $stage = 'CreateStorage'; $null = InvokeGatewayStep 'CreateStorage'
        foreach ($container in 'control-data', 'control-coordination') { $stage = 'CreateControlContainer'; $null = InvokeGatewayStep 'CreateControlContainer' $container }
        foreach ($role in $script:VLiteMeasGatewayRoles) { $stage = 'GrantControl'; $null = InvokeGatewayStep 'GrantControl' $role }
        $stage = 'CreateEnvironment'
        $environment = InvokeGatewayStep 'CreateEnvironment'
        $domain = [string]$environment.properties.defaultDomain
        $workspace = [string]$environment.properties.appLogsConfiguration.logAnalyticsConfiguration.customerId
        if ([string]$environment.id -ine $values.EnvironmentId -or $domain -cnotmatch '\A[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+\z' -or
            -not $workspace) { throw 'Created environment lacks its expected ID, domain or workspace.' }
        $privateValues.Add($workspace)
        $live = Get-VLiteMeasGatewayValues $Config $Config.SubscriptionId $domain
        $live.ClientIds = $values.ClientIds

        $stage = 'PrepareFixture'
        # Re-prove the shared root is empty right before `prepare`: provisioning takes minutes,
        # and finally-cleanup must never clear a root another run populated meanwhile.
        if (-not (Test-VLiteMeasFixtureRootEmpty)) {
            throw (New-VLiteMeasFailure 'FixtureRoot' 'The /measurement fixture root is no longer empty.')
        }
        $planFile = Join-Path $work 'fixture-plan.json'
        Set-VLiteMeasPrivate $planFile (ConvertTo-VLiteMeasJson (New-VLiteMeasFixturePlan $Config $domain $digest))
        $prepared = $true
        $started = Get-VLiteMeasUnixMs
        $result = Fixture 'PrepareFixture' @{ '<fixture-plan>' = $planFile }
        $ended = Get-VLiteMeasUnixMs
        if ($result.ExitCode -ne 0) { throw (New-VLiteMeasFailure 'FixturePrepare' 'Fixture preparation failed; diagnostics withheld.') }
        $stage = 'ValidateFixture'
        $buildStep = Find 'BuildFixtureImage'
        $plannedBuild = @('az') + @($buildStep.Arguments[0..($buildStep.Arguments.Count - 5)])
        $fixture = Assert-VLiteMeasFixtureSummary $result.Output $Config @{ Start = $started; End = $ended
            BaseImage = "$($Config.Registry).azurecr.io/gateway-measure@$digest"; PlannedBuild = $plannedBuild }
        $summary.fixtureSha256 = $fixture.FixtureSha
        $summary.credentialsExpireNotBeforeUnixMs = $fixture.Deadline
        $deadline = $fixture.Deadline
        $replacements['<fixture-sha12>'] = $fixture.Sha12
        $live.FixtureSha = $fixture.FixtureSha; $live.Projections = $fixture.Projections; $live.ExportSha = $fixture.ExportSha
        Assert-VLiteMeasGatewayTime $deadline $script:VLiteMeasGatewayMarginMs

        $stage = 'BuildFixtureImage'; $null = InvokeGatewayStep 'BuildFixtureImage'
        $stage = 'ShowFixtureImage'
        $fixtureDigest = [string](InvokeGatewayStep 'ShowFixtureImage').digest
        if ($fixtureDigest -cnotmatch '\Asha256:[a-f0-9]{64}\z') { throw 'Fixture image digest is invalid.' }
        $live.Image = "$($Config.Registry).azurecr.io/gateway-measure-fixture@$fixtureDigest"
        function Create([string]$Name, [string]$Role, [string]$Kind, [string]$Label) {
            Assert-VLiteMeasGatewayTime $deadline $script:VLiteMeasGatewayMarginMs
            $file = Join-Path $work "$Label.json"
            Set-VLiteMeasPrivate $file ((New-VLiteMeasGatewayManifest $Config $Role $Kind $live) | ConvertTo-Json -Depth 20)
            try { $null = InvokeGatewayStep $Name $Role @{ "<manifest:$Label>" = $file } }
            finally { Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue }
        }
        $stage = 'CreatePublishJob'; Create 'CreatePublishJob' 'driver' 'publish' 'publish'
        $stage = 'StartPublish'
        Assert-VLiteMeasGatewayTime $deadline $script:VLiteMeasGatewayMarginMs
        $execution = Start-Job 'StartPublish' $Config.PublishJob
        $summary.publishExecution = $execution
        $stage = 'PollPublish'
        $status = Wait-VLiteMeasExecution (Expand 'PollPublish' '' @{ '<execution>' = $execution })
        $stage = 'ReadPublish'
        $receipt = ConvertFrom-VLiteMeasJsonBytes (Read-JobEvidence 'Publish' $execution)
        Assert-VLiteMeasKeys $receipt @('schema', 'qualification', 'operation', 'source', 'generation',
            'control_state_published', 'authority_mode', 'read_routing') 'PublishReceipt'
        if ($status -cne 'Succeeded' -or $receipt.schema -cne 'vlite-meas-runtime/1' -or
            $receipt.qualification -cne 'proposal_evidence_only' -or $receipt.operation -cne 'publish' -or
            -not (Test-VLiteMeasSource $receipt.source $Config) -or $receipt.generation -ne 1 -or
            $receipt.control_state_published -ne $true -or $receipt.authority_mode -cne 'off' -or
            $receipt.read_routing -cne 'writer_only') {
            throw (New-VLiteMeasFailure 'PublishReceipt' 'Control publication was not verified; it is never retried.')
        }
        $serveKind = if ($meas005) { 'observe' } else { 'serve' }
        if ($meas005) {
            # Observers start with the apps and must finish, with slack, before credential expiry.
            Assert-VLiteMeasGatewayTime $deadline (($settings.ObserverSecs * 1000L) + 900000L)
        }
        $observersStartedNotBefore = Get-VLiteMeasUnixMs
        foreach ($role in $Config.Apps.Keys) { $stage = 'CreateApp'; Create 'CreateApp' $role $serveKind $role }
        $stage = 'CreateDriverJob'; Create 'CreateDriverJob' 'driver' 'driver' 'driver'
        $stage = 'CleanupFixture'
        $cleanup = Fixture 'CleanupFixture'
        if ($cleanup.ExitCode -ne 0 -or (@($cleanup.Output | Where-Object { $_ }) -join '') -cne
            '{"schema":"vlite-meas-fixture-cleanup/1","removed":true}') {
            throw (New-VLiteMeasFailure 'FixtureCleanup' 'Fixture cleanup failed; remove /measurement contents manually.')
        }
        $summary.fixtureCleanup = 'RemovedAfterDistribution'

        $stage = 'InventoryBefore'
        $readyBy = (Get-VLiteMeasUnixMs) + 900000L
        while ($true) {
            $state = Read-Inventory 'InventoryBefore'
            foreach ($replicas in $state.Inventory.Values) {
                if (@($replicas | Where-Object { $_.restarts -ne 0 }).Count) {
                    throw (New-VLiteMeasFailure 'ReplicaRestarted' 'A measurement replica restarted before the trial.')
                }
            }
            if ($state.Ready) { break }
            if ((Get-VLiteMeasUnixMs) -ge $readyBy) { throw (New-VLiteMeasFailure 'ReadyTimeout' 'Measurement apps did not become ready.') }
            Start-Sleep -Seconds 10
        }
        $readyAt = Get-VLiteMeasUnixMs
        $summary.inventory.before = $state.Inventory

        $stage = 'StartDriver'
        Assert-VLiteMeasGatewayTime $deadline (($script:VLiteMeasJobTimeoutSecs * 1000L) + $script:VLiteMeasGatewayMarginMs)
        if ($meas005) {
            $now = Get-VLiteMeasUnixMs
            if ($now -lt $observersStartedNotBefore -or
                $now + $settings.DriverBoundMs -gt $observersStartedNotBefore + ($settings.ObserverSecs * 1000L)) {
                throw (New-VLiteMeasFailure 'ObserverWindow' 'Insufficient conservative observer lifetime for the driver.')
            }
        }
        $execution = Start-Job 'StartDriver' $Config.DriverJob
        $summary.driverExecution = $execution
        $stage = 'PollDriver'
        $status = Wait-VLiteMeasExecution (Expand 'PollDriver' '' @{ '<execution>' = $execution })
        $summary.driverStatus = $status
        $stage = 'ReadDriver'
        $bytes = Read-JobEvidence 'Driver' $execution
        if ($meas005) {
            $mutation = Assert-VLiteMeasGatewayReport $bytes 'vlite-meas005/1' $Config $privateValues
            $checks = $mutation.mutation_and_ingress_checks_complete
            if ($mutation.stage -cne 'mutations' -or $checks -isnot [bool] -or
                ($status -ceq 'Succeeded') -ne $checks -or $status -cnotin @('Succeeded', 'Failed')) {
                throw (New-VLiteMeasFailure 'MutationEvidence' 'Mutation evidence is inconsistent with its job result.')
            }
            if (-not $checks) { Add-Incomplete 'MutationChecksIncomplete' }
            Save-Evidence 'MEAS-005-mutations.json' $bytes 'mutations'
        } else {
            if ($status -cne 'Succeeded') { throw (New-VLiteMeasFailure 'LoadJob' 'Load job did not succeed.') }
            $null = Assert-VLiteMeasGatewayReport $bytes 'vlite-meas004/1' $Config $privateValues
            Save-Evidence 'MEAS-004-load.json' $bytes 'load'
        }

        if ($meas005) {
            $stage = 'WaitObservers'
            $observersDone = $readyAt + ($settings.ObserverSecs * 1000L) + 60000L
            while ((Get-VLiteMeasUnixMs) -lt $observersDone) { Start-Sleep -Seconds 30 }
        }
        $stage = 'InventoryAfter'
        $after = (Read-Inventory 'InventoryAfter').Inventory
        $summary.inventory.after = $after
        $summary.inventory.fixed = (ConvertTo-VLiteMeasJson $summary.inventory.before) -ceq (ConvertTo-VLiteMeasJson $after)
        if (-not $summary.inventory.fixed) { Add-Incomplete 'InventoryChanged' }

        if ($meas005) {
            $stage = 'ReadObserver'
            $analysis = New-VLiteMeasPrivateDirectory $work
            $replicaEntries = [Collections.Generic.List[object]]::new()
            $index = 0
            foreach ($role in $Config.Apps.Keys) {
                $observerRole = if ($role -ceq 'gateway') { 'gateway' } else { 'backend' }
                foreach ($replica in $summary.inventory.before[$role]) {
                    $index++
                    $map = @{ '<app>' = $replica.app; '<revision>' = $replica.revision
                        '<replica>' = $replica.name; '<container>' = $replica.container
                        '<workspace-id>' = $workspace }
                    try {
                        $observed = (Get-VLiteMeasEvidence (Expand 'ReadObserver' '' $map) (Expand 'QueryObserver' '' $map) ([ordered]@{})).Bytes
                        $report = Assert-VLiteMeasGatewayReport $observed 'vlite-meas005-observer/1' $Config $privateValues
                        if ($report.replica -cne $replica.name -or $report.role -cne $observerRole -or
                            $report.app -cne $replica.app -or $report.revision -cne $replica.revision) { throw 'identity' }
                    } catch { Add-Incomplete 'ObserverReportMissing'; continue }
                    $name = "MEAS-005-observer-$index.json"
                    Save-Evidence $name $observed 'observer'
                    $file = Join-Path $analysis $name
                    Set-VLiteMeasPrivate $file $observed
                    $replicaEntries.Add([ordered]@{ replica = $replica.name; role = $observerRole; report_file = $file })
                }
            }
            # Replica-specific exec is not implemented, so clock fields are omitted and timing remains incomplete.
            Add-Incomplete 'ClockCalibrationUnavailable'
            if ($summary.incompleteReasons.Contains('ObserverReportMissing')) {
                Add-Incomplete 'AnalysisNotRun'
            } else {
                $stage = 'Analyze'
                $mutationFile = Join-Path $analysis 'mutations.json'
                Set-VLiteMeasPrivate $mutationFile $bytes
                $native = Join-Path $analysis 'analyze.json'
                Set-VLiteMeasPrivate $native (ConvertTo-VLiteMeasJson ([ordered]@{ mode = 'analyze'
                    mutation_evidence_file = $mutationFile; fixed_inventory_confirmed = [bool]$summary.inventory.fixed
                    replicas = $replicaEntries.ToArray() }))
                $runtime = Join-Path $analysis 'runtime.json'
                Set-VLiteMeasPrivate $runtime (ConvertTo-VLiteMeasJson ([ordered]@{ operation = 'analyze'; role = 'driver'
                    work_dir = $analysis; source_revision = $Config.SourceRevision; source_dirty_paths = $Config.DirtyPaths
                    control_program = $Config.ControlProgram; native_config = $native }))
                $analyzed = Invoke-VLiteMeasLocal 'python3' (Expand 'Analyze' '' @{ '<analyze-config>' = $runtime }) $provenance
                try {
                    if ($analyzed.ExitCode -notin @(0, 2)) { throw 'exit' }
                    $analysisBytes = ConvertFrom-VLiteMeasLog @($analyzed.Output)
                    $report = Assert-VLiteMeasGatewayReport $analysisBytes 'vlite-meas005/1' $Config $privateValues
                    if ($report.stage -cne 'analysis' -or $report.complete -isnot [bool]) { throw 'stage' }
                } catch { throw (New-VLiteMeasFailure 'AnalysisEvidence' 'Offline analysis did not produce verified evidence.') }
                Save-Evidence 'MEAS-005-analysis.json' $analysisBytes 'analysis'
                if (-not $report.complete) { Add-Incomplete 'AnalysisIncomplete' }
            }
        }
        $summary.measurementStatus = if ($summary.incompleteReasons.Count) { 'Incomplete' } else { 'Collected' }
        $summary.status = 'Succeeded'
    } catch {
        $summary.status = 'Failed'
        $summary.measurementStatus = 'Failed'
        $summary.failedStep = $stage
        $reason = [string]$_.Exception.Data['VLiteMeasReason']
        $summary.failureReason = if ($reason) { $reason } else { 'Unclassified' }
        $failure = "$measurement failed at $stage; diagnostics withheld."
    } finally {
        $cleanupFailures = [Collections.Generic.List[string]]::new()
        try {
            if ($prepared) {
                try {
                    $cleanup = Fixture 'CleanupFixture'
                    if ($cleanup.ExitCode -ne 0) { throw 'cleanup' }
                    if ($summary.fixtureCleanup -ceq 'NotStarted') { $summary.fixtureCleanup = 'Removed' }
                } catch {
                    $summary.fixtureCleanup = 'Failed'
                    $cleanupFailures.Add('FIXTURE CLEANUP FAILED: remove /measurement contents manually.')
                }
            }
            if ($work) { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
            try {
                $cleanupStep = 'DeleteGroup'
                $null = Invoke-VLiteMeasAz (Expand 'DeleteGroup')
                $cleanupStep = 'WaitDeleted'
                Wait-VLiteMeasDeleted (Expand 'WaitDeleted')
                $summary.cleanupStatus = 'Deleted'
            } catch {
                $summary.cleanupStatus = 'Failed'
                $summary.cleanupFailedStep = $cleanupStep
                $cleanupFailures.Insert(0, "MEASUREMENT CLEANUP FAILED: delete tagged resource group $($Config.ResourceGroup) manually.")
            }
        } finally {
            $summary | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath (Join-Path $Config.OutDir 'summary.json') -Encoding utf8
        }
        if ($cleanupFailures.Count) { throw $cleanupFailures[0] }
    }
    if ($summary.status -cne 'Succeeded') { throw $failure }
    return $summary
}
