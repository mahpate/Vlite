#Requires -Version 7.2
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'deploy.ps1')

function Assert($Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

$config = @{
    SubscriptionId = 'test-subscription'; ResourceGroup = 'asendme-rg'
    StorageAccount = 'shared-storage'; Environment = 'vlite-env'; AppName = 'vlite-app'
    ShareName = 'vlite-data'; Location = 'westus2'; Cpu = 1; Dimensions = 1536
    Image = 'ghcr.io/mahpate/vlite-api:v0.5.1'; ProbeImage = 'python:3.12-slim-bookworm'
    ShareSizeGiB = 10; TimeoutSeconds = 0; AllowExperimentalAzureFiles = $true
}
$template = New-VLiteTemplate $config 'environment-id' 'test-only-token' 'revision-test'
Assert ($template.properties.template.scale.minReplicas -eq 1) 'Minimum replica count changed.'
Assert ($template.properties.template.scale.maxReplicas -eq 1) 'Maximum replica count changed.'
Assert (-not $template.properties.configuration.ingress.allowInsecure) 'Insecure ingress enabled.'
Assert ($template.properties.template.initContainers[0].command[2] -match 'os.fsync') 'Missing storage preflight.'
Assert ($template.properties.template.containers[0].env[0].secretRef -eq 'admin-token') 'Token not secret-backed.'
Assert ($template.properties.template.volumes[0].mountOptions -notmatch 'nobrl') 'Locks disabled.'
Assert ($template.properties.template.containers[0].args -contains '4') 'Not using format 4.'

$script:calls = [Collections.Generic.List[string]]::new()
$script:mode = 'stop'
function Invoke-VLiteAz([string[]]$Arguments) {
    $call = $Arguments -join ' '
    $script:calls.Add($call)
    if ($call -eq 'account show') { return @{ id = 'test-subscription' } }
    if ($call -like 'group show*') { return @{ location = 'westus2' } }
    if ($call -like 'storage account show*') { return @{ primaryLocation = 'westus2' } }
    if ($call -like 'containerapp env list*') { return @() }
    if ($call -like 'storage share-rm list*') { return @(@{name='vlite-data';metadata=@{vliteApp='another-app'}}) }
    if ($call -like 'storage share-rm show*') { return @{name='vlite-data';metadata=@{vliteApp='another-app'}} }
    if ($call -like 'containerapp list*') { return @() }
    if ($call -like 'containerapp revision list*') {
        return @(@{name='old-a';properties=@{active=$true}}, @{name='old-b';properties=@{active=$false}})
    }
    if ($call -like 'containerapp revision deactivate*') { return }
    if ($call -like 'containerapp replica list*') {
        if ($script:mode -eq 'busy') { return @{name='still-running'} }
        return @()
    }
    throw "Unexpected Azure operation: $call"
}
Stop-VLiteRevisions $config
Assert ($script:calls[1] -match 'revision deactivate.*old-a') 'Active writer not stopped first.'
Assert ($script:calls[-1] -match 'replica list.*old-b') 'Inactive revision replicas not checked.'
$script:mode = 'busy'
$failed = $false
try { Stop-VLiteRevisions $config } catch {
    $failed = $_.Exception.Message -match 'Refusing a second database owner'
}
Assert $failed 'Failed to block a still-running database owner.'

foreach ($case in @('subscription', 'experimental', 'share-owner')) {
    $candidate = $config.Clone()
    if ($case -eq 'subscription') { $candidate.SubscriptionId = 'wrong-subscription' }
    if ($case -eq 'experimental') { $candidate.AllowExperimentalAzureFiles = $false }
    $script:calls.Clear()
    $failed = $false
    try { Invoke-VLiteDeployment $candidate } catch {
        $failed = $_.Exception.Message -match 'does not match|unqualified|not owned'
    }
    Assert $failed "Missing guard for $case."
    Assert (-not ($script:calls | Where-Object { $_ -match ' create| update| set| deactivate|keys list' })) "Mutation before $case guard."
}
$script:flow = 'fresh'
$script:capturedTemplate = $null
function Invoke-VLiteAz([string[]]$Arguments) {
    $call = $Arguments -join ' '
    $script:calls.Add($call)
    if ($call -eq 'account show') { return @{id='test-subscription'} }
    if ($call -like 'group show*') { return @{location='westus2'} }
    if ($call -like 'storage account show*') { return @{primaryLocation='westus2'} }
    if ($call -like 'containerapp env storage show*') {
        if ($script:flow -eq 'binding-mismatch') { return @{account='shared-storage';share='different-share'} }
        return @{account='shared-storage';share='vlite-data'}
    }
    if ($call -like 'containerapp env list*') {
        if ($script:flow -ne 'fresh') { return @{name='vlite-env';id='environment-id';tags=@{vliteDeployment='v1'}} }
        return @()
    }
    if ($call -like 'containerapp list*') {
        if ($script:flow -ne 'fresh') {
            return @{name='vlite-app';tags=@{vliteDeployment='v1'};properties=@{
                managedEnvironmentId='environment-id';template=@{volumes=@(@{storageName='vlite-app-data'})}
            }}
        }
        return @()
    }
    if ($call -like 'storage share-rm list*') {
        if ($script:flow -ne 'fresh') { return @{name='vlite-data'} }
        return @()
    }
    if ($call -like 'storage share-rm show*') { return @{name='vlite-data';metadata=@{vliteApp='vlite-app'}} }
    if ($call -like 'storage share-rm create*') { return }
    if ($call -like 'containerapp env create*') { return @{id='environment-id'} }
    if ($call -like 'storage account keys list*') { return @{value='fake-account-key'} }
    if ($call -like 'containerapp revision list*') { return @{name='old';properties=@{active=$true}} }
    if ($call -like 'containerapp revision deactivate*') { return }
    if ($call -like 'containerapp replica list*') {
        if ($call -match '--revision old$') { return @() }
        return @{properties=@{containers=@(@{name='vlite';ready=($script:flow -ne 'unready');started=$true})}}
    }
    if ($call -like 'containerapp env storage set*') { return }
    if ($Arguments[0] -eq 'containerapp' -and $Arguments[1] -in @('create','update')) {
        $path = $Arguments[[Array]::IndexOf($Arguments, '--yaml') + 1]
        $script:capturedTemplate = Get-Content -Raw -LiteralPath $path | ConvertFrom-Json -AsHashtable
        if ($script:flow -eq 'failed-update') { throw 'Simulated Azure update failure' }
        return
    }
    if ($call -like 'containerapp show*') {
        $revision = "vlite-app--$($script:capturedTemplate.properties.template.revisionSuffix)"
        return @{properties=@{latestRevisionName=$revision;latestReadyRevisionName=$revision
            configuration=@{ingress=@{fqdn='vlite.example.test'}}}}
    }
    throw "Unexpected Azure operation: $call"
}
function Invoke-WebRequest {
    param($Uri, $Headers, [switch]$SkipHttpErrorCheck, $TimeoutSec)
    Assert ($Headers.Authorization -match '^Bearer [a-f0-9]{64}$') 'Missing admin authentication.'
    return @{StatusCode=200;Content='{"items":[]}'}
}

$temp = Join-Path ([IO.Path]::GetTempPath()) "vlite-aca-tests-$([Guid]::NewGuid().ToString('N'))"
try {
    $config.StateDirectory = $temp
    Invoke-VLiteDeployment $config
    $savedToken = @(Get-ChildItem -LiteralPath $temp -Recurse -Filter admin-token)[0]
    $original = Get-Content -Raw -LiteralPath $savedToken.FullName
    Assert ($script:capturedTemplate.properties.template.volumes[0].storageName -eq 'vlite-app-data') 'Wrong share binding.'
    Assert (-not @(Get-ChildItem -LiteralPath $temp -Recurse -Filter deployment.json).Count) 'Secret manifest was retained.'
    Assert (-not ($script:calls | Where-Object { $_ -match 'asendme-env|admin-operations| delete|keys renew' })) 'ASENDME mutation detected.'

    $script:calls.Clear()
    $script:flow = 'update'
    Invoke-VLiteDeployment $config
    Assert ((Get-Content -Raw -LiteralPath $savedToken.FullName) -ceq $original) 'Token rotated on rerun.'
    $deactivate = $script:calls.FindIndex([Predicate[string]]{param($call) $call -match 'revision deactivate'})
    $replicas = $script:calls.FindIndex([Predicate[string]]{param($call) $call -match 'replica list'})
    $update = $script:calls.FindIndex([Predicate[string]]{param($call) $call -match '^containerapp update'})
    Assert ($deactivate -ge 0 -and $replicas -gt $deactivate -and $update -gt $replicas) 'Unsafe update order.'

    $script:flow = 'failed-update'
    $failed = $false
    try { Invoke-VLiteDeployment $config } catch { $failed = $_.Exception.Message -eq 'Simulated Azure update failure' }
    Assert $failed 'Azure failure was swallowed.'
    Assert (-not @(Get-ChildItem -LiteralPath $temp -Recurse -Filter deployment.json).Count) 'Failed deployment retained a secret manifest.'

    $script:flow = 'unready'
    $revision = "vlite-app--$($script:capturedTemplate.properties.template.revisionSuffix)"
    $failed = $false
    try { Wait-VLiteReady $config $revision $original } catch {
        $failed = $_.Exception.Message -match 'container is not ready'
    }
    Assert $failed 'Control-plane ready status bypassed actual container readiness.'

    $script:flow = 'binding-mismatch'
    $script:calls.Clear()
    $failed = $false
    try { Invoke-VLiteDeployment $config } catch { $failed = $_.Exception.Message -match 'different account/share' }
    Assert $failed 'Existing database silently moved to a different share.'
    Assert (-not ($script:calls | Where-Object { $_ -match ' create| update| set| deactivate' })) 'Mutation before mount drift check.'
} finally {
    if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Recurse -Force }
}
Write-Host 'Container Apps deployment guard, create, update and cleanup tests passed.'
