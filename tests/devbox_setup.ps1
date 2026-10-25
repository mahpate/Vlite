$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\..\scripts\devbox.ps1" -Yes
$script:Calls = [Collections.Generic.List[string]]::new()
$script:Scenario = ''
$script:Installed = $false

function Assert([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Expect-Failure([scriptblock]$Body, [string]$Pattern) {
    try { & $Body } catch {
        Assert ($_.Exception.Message -match $Pattern) "Unexpected error: $($_.Exception.Message)"
        return
    }
    throw "Expected failure matching $Pattern"
}
function wsl.exe {
    $command = $args -join ' '
    $script:Calls.Add($command)
    $global:LASTEXITCODE = 0
    switch ($command) {
        '--status' {
            if ($script:Scenario -eq 'missing-wsl' -and -not $script:Installed) { $global:LASTEXITCODE = 1 }
        }
        '--list --quiet' {
            if ($script:Scenario -ne 'missing-distro' -or $script:Installed) { "Ubuntu-24.04`0" }
        }
        '--list --verbose' {
            if ($script:Scenario -eq 'wsl1') { '  * Ubuntu-24.04    Stopped    1' }
            else { '  * Ubuntu-24.04    Stopped    2' }
        }
        default {
            if ($script:Scenario -eq 'first-run') { $global:LASTEXITCODE = 1 }
        }
    }
}
# Match the real parameter shape; these mocks never start a process.
function Start-Process {
    param($FilePath, $ArgumentList, [string]$Verb, [switch]$Wait, [switch]$PassThru)
    $script:Calls.Add("elevated: $($ArgumentList -join ' ')")
    $script:Installed = $true
    if ($script:Scenario -eq 'reboot') { return [pscustomobject]@{ ExitCode = 3010 } }
    if ($script:Scenario -eq 'install-failed') { return [pscustomobject]@{ ExitCode = 1 } }
    [pscustomobject]@{ ExitCode = 0 }
}

foreach ($scenario in @('ready', 'missing-wsl', 'missing-distro', 'wsl1')) {
    $script:Scenario = $scenario
    $script:Installed = $false
    $script:Calls.Clear()
    Ensure-Wsl $true
    if ($scenario -eq 'ready') {
        Assert (-not ($script:Calls -match 'elevated:')) 'Working WSL must not be reinstalled.'
    } else {
        Assert ([bool]($script:Calls -match 'elevated:')) "$scenario must install/convert prerequisites."
    }
    if ($scenario -eq 'missing-distro') {
        Assert ([bool]($script:Calls -match '--distribution Ubuntu-24.04 --no-launch')) 'Install the selected Ubuntu without forcing interactive launch.'
    }
}
$script:Scenario = 'missing-distro'
$script:Installed = $false
$script:Calls.Clear()
Expect-Failure { Ensure-Wsl $false } 'without management switches'
Assert (-not ($script:Calls -match 'elevated:')) 'Management operations must never install packages.'
$script:Scenario = 'first-run'
Expect-Failure { Ensure-Wsl $true } 'finish first-run setup'
$script:Scenario = 'reboot'
Expect-Failure { Invoke-WslInstall @('--install', '--no-distribution') } 'restart Windows'
$script:Scenario = 'install-failed'
Expect-Failure { Invoke-WslInstall @('--install', '--no-distribution') } 'installation failed'
$Distro = 'Debian'
$script:Scenario = 'missing-distro'
$script:Installed = $false
Expect-Failure { Ensure-Wsl $true } 'Automatic distro installation supports Ubuntu'
$Distro = 'Ubuntu-24.04'
function Get-Command { param($Name, $ErrorAction) $null }
$script:Calls.Clear()
Expect-Failure { Ensure-Wsl $false } 'WSL is missing'
Assert ($script:Calls.Count -eq 0) 'Management must not enable Windows features.'
Expect-Failure { Ensure-Wsl $true } 'WSL features were enabled'
Assert ([bool]($script:Calls -match 'Enable-WindowsOptionalFeature')) 'Missing wsl.exe must offer Windows feature installation.'
Remove-Item Function:\Get-Command

# Every destructive/management branch is exercised without accessing WSL or Docker.
function Ensure-Wsl { param($AllowInstall) }
function Invoke-Wsl { param($Script) $script:Calls.Add($Script) }
function Stop-Keepalive { $script:Calls.Add('stop keepalive') }
function Read-Host { param($Prompt) 'no' }
$Action = 'Reset'
$script:Calls.Clear()
Expect-Failure { Start-Devbox } 'Reset cancelled'
Assert ($script:Calls.Count -eq 0) 'Declining reset must preserve all data.'
$Action = 'Down'
Start-Devbox
Assert ([bool]($script:Calls -match 'down$')) 'Down must stop containers.'
Assert (-not ($script:Calls -match '--volumes')) 'Down must preserve volumes.'
function Write-WslFile { param($Path, $Content) $script:Calls.Add("file: $Path $Content") }
function Get-Keepalive { [pscustomobject]@{ ProcessId = 123 } }
function Invoke-RestMethod { param($Uri, $TimeoutSec) @{ status = 'ok' } }
function Invoke-WebRequest { param($Uri, [switch]$UseBasicParsing, $TimeoutSec) @{ Content = 'test response' } }
$script:Scenario = 'ready'
$Action = 'Up'
$script:Calls.Clear()
Start-Devbox
Assert ([bool]($script:Calls -match '--prepare-only --yes')) 'Startup must run prerequisite preparation.'
Assert ([bool]($script:Calls -match 'VLITE_API_IMAGE=ghcr.io/mahpate/vlite-api:')) 'Pin published images.'
Assert ([bool]($script:Calls -match 'up -d --no-build --wait')) 'Never build images on a new devbox.'
Assert (-not ($script:Calls -match 'compose.override.yaml')) 'Do not require the newer !reset Compose extension.'
Write-Host 'Windows bootstrap: ready/missing WSL/distro, WSL1, restart, install errors, first-run, unsupported distro and safe management passed (mocked).'
