# Opt-in faster local builds for this repository; nothing here is committed to Cargo configuration.
# Usage (this shell only; start a new shell, or pass -Off, to undo):
#   . .\scripts\dev-fast-env.ps1          enable what is available and print it
#   . .\scripts\dev-fast-env.ps1 -Off     unset what this script set
#   $env:VLITE_DEV_FAST_DEBUG = '2'       before running: debug info level (default line-tables-only; 2 for a debugger)
#   $env:VLITE_DEV_FAST_INCREMENTAL = '1' before running: 1 or 0, incremental builds on (release too) or off, even with sccache
# It sets, and never overrides a variable you already set (a value it set itself is replaced when you run it
# again): RUSTC_WRAPPER (sccache, if installed), CARGO_INCREMENTAL (off with sccache, which cannot cache
# incremental crates), CARGO_PROFILE_DEV_DEBUG, CARGO_PROFILE_TEST_DEBUG and the MSVC host linker (rust-lld or
# lld-link, if found). Changing these rebuilds the target directory once. Not used by CI.

# No param block: a dot-sourced script's parameters would become variables of the caller's shell.
& {
    param([string[]]$Words)
    $Off = $Words -contains '-Off'

    function Say([string]$text) { Write-Host "dev-fast: $text" }

    $listName = 'VLITE_DEV_FAST_SET'
    $before = @(([Environment]::GetEnvironmentVariable($listName) -split ' ') | Where-Object { $_ })

    if ($Off) {
        foreach ($name in $before) {
            Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
            Say "unset $name"
        }
        Remove-Item -LiteralPath "Env:$listName" -ErrorAction SilentlyContinue
        if (-not $before) { Say 'nothing to undo' }
        return
    }

    $owned = New-Object System.Collections.Generic.List[string]
    $owned.AddRange([string[]]$before)
    $applied = New-Object System.Collections.Generic.List[string]
    function Enable([string]$name, [string]$value, [string]$why) {
        $current = [Environment]::GetEnvironmentVariable($name)
        $ours = $owned.Contains($name)
        if ($null -ne $current -and -not $ours) { Say "kept your $name=$current"; return }
        [Environment]::SetEnvironmentVariable($name, $value)
        if (-not $ours) { $owned.Add($name) }
        $applied.Add($name)
        Say "$name=$value  ($why)"
    }

    $sccache = Get-Command sccache -ErrorAction SilentlyContinue
    if ($sccache) {
        Enable 'RUSTC_WRAPPER' 'sccache' "compile cache, $($sccache.Source)"
    } else {
        Say 'sccache not found: no compile cache (install with: cargo install sccache --locked)'
    }

    $forced = [Environment]::GetEnvironmentVariable('VLITE_DEV_FAST_INCREMENTAL')
    if ($forced -and $forced -notin '0', '1') {
        Say "VLITE_DEV_FAST_INCREMENTAL must be 0 or 1, not '$forced': ignored"
        $forced = $null
    }
    $incremental = $forced
    if (-not $incremental -and $env:RUSTC_WRAPPER -match 'sccache') { $incremental = '0' }
    if ($incremental) {
        $reason = if ($forced -eq '1') { 'from VLITE_DEV_FAST_INCREMENTAL; release builds become incremental too' }
                  elseif ($forced) { 'from VLITE_DEV_FAST_INCREMENTAL' }
                  else { 'sccache cannot cache incremental crates; VLITE_DEV_FAST_INCREMENTAL=1 keeps them' }
        Enable 'CARGO_INCREMENTAL' $incremental $reason
    } else {
        Say 'CARGO_INCREMENTAL left at the Cargo default (incremental dev builds): fastest for edit and rebuild loops'
    }

    $debug = [Environment]::GetEnvironmentVariable('VLITE_DEV_FAST_DEBUG')
    if (-not $debug) { $debug = 'line-tables-only' }
    $why = 'less debug info to write and link; panics still show file and line'
    Enable 'CARGO_PROFILE_DEV_DEBUG' $debug $why
    Enable 'CARGO_PROFILE_TEST_DEBUG' $debug $why

    if ($env:OS -ne 'Windows_NT') {
        Say 'linker unchanged (the override is for Windows)'
    } elseif (-not (Get-Command rustc -ErrorAction SilentlyContinue)) {
        Say 'linker unchanged (rustc not found on PATH)'
    } else {
        $hostLine = @(& rustc -vV) | Where-Object { $_ -like 'host: *' } | Select-Object -First 1
        $triple = ([string]$hostLine -replace '^host:\s*', '').Trim()
        if ($triple -notlike '*-pc-windows-msvc') {
            Say "linker unchanged (host $triple is not an MSVC target)"
        } else {
            $sysroot = ((& rustc --print sysroot) | Select-Object -First 1).Trim()
            $bundled = Join-Path $sysroot "lib\rustlib\$triple\bin\rust-lld.exe"
            $onPath = Get-Command lld-link -ErrorAction SilentlyContinue
            $linker = if (Test-Path -LiteralPath $bundled) { $bundled } elseif ($onPath) { $onPath.Source } else { $null }
            if ($linker) {
                $variable = 'CARGO_TARGET_' + ($triple.ToUpperInvariant() -replace '-', '_') + '_LINKER'
                Enable $variable $linker 'lld links much faster than link.exe'
            } else {
                Say 'linker unchanged (neither rust-lld nor lld-link found)'
            }
        }
    }

    if ($owned.Count) {
        [Environment]::SetEnvironmentVariable($listName, ($owned -join ' '))
    }
    Say "$($applied.Count) setting(s) applied; undo with: . .\scripts\dev-fast-env.ps1 -Off"
} (@($args | ForEach-Object { [string]$_ }))
