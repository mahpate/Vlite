<#
.SYNOPSIS
  Run the VLite REST API and web dashboard on a Windows devbox using Docker inside WSL.

.DESCRIPTION
  Offers to install WSL 2, Ubuntu and Docker when missing. Windows installation
  may require administrator approval and a restart; rerun the same command afterward.
  Creates (or reuses) a deployment directory inside a WSL distro, writes compose.yaml,
  a .env with published GHCR images and host ports, and a random
  API token, then starts the stack with `docker compose --profile dashboard up -d --wait`.
  A hidden keepalive process keeps the WSL VM (and therefore Docker) running after the
  script exits. Data lives in Docker volumes and survives restarts; only -Reset deletes it.

.EXAMPLE
  .\scripts\devbox.ps1                 # start (or update) and print URLs
  .\scripts\devbox.ps1 -Token          # copy the API token to the clipboard
  .\scripts\devbox.ps1 -Status         # show container status
  .\scripts\devbox.ps1 -Down           # stop containers and the keepalive (data kept)
  .\scripts\devbox.ps1 -Reset          # stop and DELETE the database volumes
#>
[CmdletBinding(DefaultParameterSetName = 'Up')]
param(
    [ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9._-]*$')][string]$Version = 'v0.4.1',
    [ValidatePattern('^[a-zA-Z0-9][a-zA-Z0-9._-]*$')]
    [string]$Distro = 'Ubuntu-24.04',
    [ValidatePattern('^/[a-zA-Z0-9/_-]+$')]
    [string]$DeployDir = '/root/vlite-deploy',
    [ValidateRange(1, 65535)][int]$ApiPort = 18085,
    [ValidateRange(1, 65535)][int]$DashboardPort = 8082,
    [ValidateRange(1, 65535)][int]$Dimensions = 3,
    [switch]$Yes,
    [Parameter(ParameterSetName = 'Token')][switch]$Token,
    [Parameter(ParameterSetName = 'Status')][switch]$Status,
    [Parameter(ParameterSetName = 'Down')][switch]$Down,
    [Parameter(ParameterSetName = 'Reset')][switch]$Reset
)

$ErrorActionPreference = 'Stop'
$Action = $PSCmdlet.ParameterSetName
$DevboxScriptPath = $PSCommandPath

function Confirm-Install([string]$Message) {
    if ($Yes) { return }
    if ((Read-Host "$Message [y/N]") -notmatch '^(y|yes)$') {
        throw 'Installation declined. Nothing further was installed.'
    }
}

function Invoke-WslInstall([string[]]$Arguments) {
    Confirm-Install "Install Windows/WSL prerequisites ($($Arguments -join ' '))? Windows will ask for administrator approval"
    try {
        $process = Start-Process -FilePath 'wsl.exe' -ArgumentList $Arguments -Verb RunAs -Wait -PassThru
    } catch {
        throw "Windows installation could not start: $($_.Exception.Message). Run PowerShell as administrator and retry."
    }
    if ($process.ExitCode -in @(3010, 1641)) {
        throw 'Windows needs a restart to finish WSL installation. Save your work, restart Windows, then rerun this script. No containers were started.'
    }
    if ($process.ExitCode -ne 0) {
        throw "WSL installation failed (exit $($process.ExitCode)). Check Windows Update, hardware virtualization and administrator access. Restart Windows if requested, then rerun."
    }
}

function Get-WslDistros {
    $output = & wsl.exe --list --quiet
    if ($LASTEXITCODE -ne 0) { throw 'Cannot list WSL distributions. Restart Windows after installing WSL, then rerun. See https://learn.microsoft.com/windows/wsl/install.' }
    @($output -replace "`0", '' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
}

function Ensure-Wsl([bool]$AllowInstall) {
    $wsl = Get-Command wsl.exe -ErrorAction SilentlyContinue
    if (-not $wsl) {
        if (-not $AllowInstall) { throw 'WSL is missing. Run this script without management switches to set it up.' }
        Confirm-Install 'Enable Windows Subsystem for Linux and Virtual Machine Platform? Windows will ask for administrator approval; a restart may be required'
        $enable = "Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Windows-Subsystem-Linux,VirtualMachinePlatform -All -NoRestart -ErrorAction Stop | Out-Host"
        $process = Start-Process -FilePath 'powershell.exe' -Verb RunAs -Wait -PassThru -ArgumentList @(
            '-NoProfile', '-Command', "`"& { try { $enable; exit 0 } catch { Write-Error `$_; exit 1 } }`""
        )
        if ($process.ExitCode -ne 0) { throw 'Windows could not enable WSL. Use Windows 10 build 19041+ or Windows 11, check administrator access and hardware virtualization, then retry.' }
        throw 'Windows WSL features were enabled. Save your work and restart Windows, then rerun this script to install Ubuntu. If wsl.exe is still unavailable, install WSL from https://learn.microsoft.com/windows/wsl/install.'
    }
    $null = & wsl.exe --status 2>&1
    if ($LASTEXITCODE -ne 0) {
        if (-not $AllowInstall) { throw 'WSL is not ready. Run this script without management switches to set it up.' }
        Invoke-WslInstall -Arguments @('--install', '--no-distribution')
        $null = & wsl.exe --status 2>&1
        if ($LASTEXITCODE -ne 0) { throw 'WSL is not ready yet. Restart Windows, then rerun this script. Enable hardware virtualization in firmware if Windows requests it.' }
    }
    $distros = @(Get-WslDistros)
    if ($distros -notcontains $Distro) {
        if (-not $AllowInstall) { throw "WSL distro '$Distro' is not installed. Run this script without management switches first." }
        if ($Distro -notin @('Ubuntu-22.04', 'Ubuntu-24.04')) {
            throw "Automatic distro installation supports Ubuntu-22.04 or Ubuntu-24.04. Use the default -Distro Ubuntu-24.04, or install '$Distro' yourself."
        }
        Invoke-WslInstall -Arguments @('--install', '--distribution', $Distro, '--no-launch')
        if (@(Get-WslDistros) -notcontains $Distro) {
            throw "Ubuntu installation is not complete. Restart Windows, run 'wsl -d $Distro' to finish setup, then rerun this script."
        }
    }
    $listing = & wsl.exe --list --verbose
    if ($LASTEXITCODE -ne 0) { throw 'Unable to check WSL version. Update WSL with wsl --update, restart Windows and retry.' }
    $row = @($listing -replace "`0", '' | Where-Object { $_ -match "^\s*\*?\s*$([regex]::Escape($Distro))\s+.+\s+[12]\s*$" })
    if ($row.Count -ne 1) { throw "Cannot determine WSL version for '$Distro'. Run 'wsl --list --verbose' and confirm this distro uses version 2." }
    if ($row[0] -notmatch '\s+2\s*$') {
        if (-not $AllowInstall) { throw "This deployment requires WSL 2. Run 'wsl --set-version $Distro 2' and retry." }
        Invoke-WslInstall -Arguments @('--set-version', $Distro, '2')
    }
    & wsl.exe -d $Distro -u root -e sh -c 'test -r /etc/os-release'
    if ($LASTEXITCODE -ne 0) { throw "Ubuntu could not start. Run 'wsl -d $Distro' to finish first-run setup. If prompted, create a Linux username/password, then rerun this script." }
}

function Invoke-Wsl([string]$Script) {
    & wsl.exe -d $Distro -u root -e sh -c $Script
    if ($LASTEXITCODE -ne 0) { throw "WSL command failed (exit $LASTEXITCODE): $Script" }
}

function Write-WslFile([string]$Path, [string]$Content) {
    $bytes = [Text.Encoding]::UTF8.GetBytes(($Content -replace "`r`n", "`n"))
    $b64 = [Convert]::ToBase64String($bytes)
    Invoke-Wsl "echo '$b64' | base64 -d > '$Path'"
}

function Get-Keepalive {
    Get-CimInstance Win32_Process -Filter "Name = 'wsl.exe'" |
        Where-Object { $_.CommandLine -like "*$Distro*vlite-keepalive*" }
}

function Stop-Keepalive {
    Get-Keepalive | ForEach-Object { Stop-Process -Id $_.ProcessId -ErrorAction SilentlyContinue }
}

function Start-Devbox {
if ($ApiPort -eq $DashboardPort) { throw 'API and dashboard ports must differ.' }
$compose = "cd '$DeployDir' && docker compose -f compose.yaml --profile dashboard"
Ensure-Wsl ($Action -eq 'Up')

switch ($Action) {
    'Token' {
        $value = "$(& wsl.exe -d $Distro -u root -e cat "$DeployDir/.secrets/api-token")".Trim()
        if ($LASTEXITCODE -ne 0) { throw 'Cannot read the token. Start the deployment first; the existing token was not changed.' }
        if (-not $value) { throw "No token found at $DeployDir/.secrets/api-token. Run the script without switches first." }
        Set-Clipboard -Value $value
        Write-Host 'API token copied to the clipboard. Paste it into the dashboard "API token" field.'
        return
    }
    'Status' { Invoke-Wsl "$compose ps"; return }
    'Down' {
        Invoke-Wsl "$compose down"
        Stop-Keepalive
        Write-Host 'Stopped. Data volumes were kept; run the script again to restart.'
        return
    }
    'Reset' {
        if ((Read-Host "Delete the VLite database and admin volumes in '$DeployDir'? Type DELETE to confirm") -cne 'DELETE') {
            throw 'Reset cancelled; data was kept.'
        }
        Invoke-Wsl "$compose down --volumes"
        Stop-Keepalive
        Write-Host 'Stopped and deleted the database volumes. The API token was kept.'
        return
    }
}

Write-Host "==> Checking Docker in WSL ($Distro)"
$installer = Join-Path $PSScriptRoot 'docker-dashboard.sh'
if (Test-Path $installer) {
    $installerContent = Get-Content $installer -Raw
} else {
    $installerContent = (Invoke-WebRequest 'https://raw.githubusercontent.com/mahpate/Vlite/main/scripts/docker-dashboard.sh' -UseBasicParsing).Content
}
$null = & wsl.exe -d $Distro -u root -e sh -c 'command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1'
if ($LASTEXITCODE -ne 0) { Confirm-Install "Install missing Docker Engine/Compose packages inside $Distro from Docker's official Ubuntu repository" }
# stdin avoids a world-writable installer path; preserve the script's own exit status.
$installerContent -replace "`r`n", "`n" | & wsl.exe -d $Distro -u root -e sh -s -- --prepare-only --yes
if ($LASTEXITCODE -ne 0) { throw 'Docker prerequisite setup failed. Follow the error above, then rerun. No VLite containers were started.' }

Write-Host "==> Preparing $DeployDir"
Invoke-Wsl "mkdir -p '$DeployDir/.secrets' && chmod 700 '$DeployDir/.secrets'"

$repoCompose = Join-Path $PSScriptRoot '..\compose.yaml'
if (Test-Path $repoCompose) {
    Write-WslFile "$DeployDir/compose.yaml" (Get-Content $repoCompose -Raw)
} else {
    $content = (Invoke-WebRequest "https://raw.githubusercontent.com/mahpate/Vlite/$Version/compose.yaml" -UseBasicParsing).Content
    Write-WslFile "$DeployDir/compose.yaml" $content
}

Write-WslFile "$DeployDir/.env" @"
VLITE_PORT=$ApiPort
VLITE_DASHBOARD_PORT=$DashboardPort
VLITE_DIMENSIONS=$Dimensions
VLITE_API_IMAGE=ghcr.io/mahpate/vlite-api:$Version
VLITE_DASHBOARD_IMAGE=ghcr.io/mahpate/vlite-dashboard:$Version
"@

# The token file must be readable by the container user (UID 10001); its directory stays private.
Invoke-Wsl "f='$DeployDir/.secrets/api-token'; [ -s `"`$f`" ] || head -c 32 /dev/urandom | base64 | tr -d '\n=+/' > `"`$f`"; chmod 644 `"`$f`""

Write-Host "==> Pulling $Version images and starting containers"
Invoke-Wsl "$compose pull --quiet api dashboard && $compose up -d --no-build --wait api dashboard"

if (-not (Get-Keepalive)) {
    Write-Host '==> Starting hidden WSL keepalive so Docker keeps running after this window closes'
    # Created through WMI so it is not tied to this console or its process tree.
    $startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{ ShowWindow = [uint16]0 }
    $result = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
        CommandLine = "wsl.exe -d $Distro -e sh -c `"exec sleep infinity`" vlite-keepalive"
        ProcessStartupInformation = $startup
    }
    if ($result.ReturnValue -ne 0) { throw "WSL keepalive could not start (Windows error $($result.ReturnValue)). Containers may stop when WSL exits." }
}

try {
    $health = Invoke-RestMethod "http://127.0.0.1:$ApiPort/health" -TimeoutSec 10
    $null = Invoke-WebRequest "http://127.0.0.1:$DashboardPort/" -UseBasicParsing -TimeoutSec 10
} catch {
    throw "Containers started, but Windows could not reach the API/dashboard on 127.0.0.1 ports $ApiPort/$DashboardPort. Check 'wsl --list --verbose', port conflicts and your firewall. Run this script with -Status to inspect containers. $($_.Exception.Message)"
}
Write-Host "==> API health: $($health | ConvertTo-Json -Compress)"

Write-Host ''
Write-Host "Dashboard : http://127.0.0.1:$DashboardPort"
Write-Host "REST API  : http://127.0.0.1:$ApiPort"
Write-Host "API token : powershell -NoProfile -ExecutionPolicy Bypass -File `"$DevboxScriptPath`" -Distro $Distro -DeployDir $DeployDir -Token"
Write-Host "Stop      : powershell -NoProfile -ExecutionPolicy Bypass -File `"$DevboxScriptPath`" -Distro $Distro -DeployDir $DeployDir -Down (data is kept)"
}

# Dot-sourcing loads functions for prerequisite tests without touching the host.
if ($MyInvocation.InvocationName -ne '.') { Start-Devbox }
