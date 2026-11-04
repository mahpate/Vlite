param(
  [Parameter(Mandatory = $true)][string]$Mutants,
  [Parameter(Mandatory = $true)][string]$Ids,
  [string]$Name = 'run',
  [string]$TargetDir = '',
  [int]$TimeoutMinutes = 25,
  [switch]$AnchorsOnly,
  [switch]$Keep
)
# Verification only. Runs hand-built mutants against a COPY of the current clean checkout, so the checkout
# itself is never modified (safe inside a pool report task). JSON: [{"id","desc","file","old","new","cmd"}];
# `old` must occur exactly once (LF-normalized match, CRLF-aware write). Prints a markdown summary, keeps it in
# $env:TEMP\vlite-mut-<Name>-summary.md and removes the temporary copy and build output.
$ErrorActionPreference = 'Continue'
$repo = (git rev-parse --show-toplevel).Trim()
Set-Location $repo
if (-not [string]::IsNullOrEmpty((git status --porcelain))) { throw 'checkout is not clean; stop and report' }
$sha = (git rev-parse HEAD).Trim()
$want = @($Ids -split '[,\s]+' | Where-Object { $_ })
$all = Get-Content -Raw $Mutants | ConvertFrom-Json
$sel = @($all | Where-Object { $want -contains $_.id })
if ($sel.Count -ne $want.Count) {
  throw ('mutant ids not found: ' + (@($want | Where-Object { $sel.id -notcontains $_ }) -join ','))
}

function Get-Anchor($m) {
  $p = Join-Path $repo (([string]$m.file) -replace '/', '\')
  $orig = [IO.File]::ReadAllText($p)
  $norm = $orig.Replace("`r`n", "`n")
  $old = ([string]$m.old).Replace("`r`n", "`n")
  [pscustomobject]@{
    Path = $p; Orig = $orig; Crlf = $orig.Contains("`r`n"); Norm = $norm; Old = $old
    New = ([string]$m.new).Replace("`r`n", "`n")
    Count = ([regex]::Matches($norm, [regex]::Escape($old))).Count
  }
}

$anchors = @{}
foreach ($m in $sel) { $anchors[$m.id] = Get-Anchor $m }
if ($AnchorsOnly) {
  foreach ($m in $sel) { '{0} anchor count={1}' -f $m.id, $anchors[$m.id].Count }
  return
}

$work = Join-Path $env:TEMP "vlite-mut-$Name"
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
$copy = Join-Path $work 'src'
New-Item -ItemType Directory -Force -Path $copy | Out-Null
robocopy $repo $copy /E /XD (Join-Path $repo '.git') (Join-Path $repo 'target') /NFL /NDL /NJH /NJS /NP | Out-Null
if ($LASTEXITCODE -ge 8) { throw "robocopy failed with $LASTEXITCODE" }
$env:CARGO_TARGET_DIR = if ($TargetDir) { $TargetDir } else { Join-Path $work 'target' }
$env:CARGO_INCREMENTAL = '0'
if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = [string][Math]::Min(4, [Environment]::ProcessorCount) }
$env:PYTHONUTF8 = '1'
$started = Get-Date

function Invoke-Cargo([string]$cmdline) {
  $argText = (($cmdline -replace '\s--offline', '') -split '\s+' | Where-Object { $_ }) -join ' '
  $out = Join-Path $work 'cargo.out'
  $err = Join-Path $work 'cargo.err'
  $p = Start-Process -FilePath 'cargo' -ArgumentList $argText -WorkingDirectory $copy -NoNewWindow -PassThru `
    -RedirectStandardOutput $out -RedirectStandardError $err
  $null = $p.Handle
  if (-not $p.WaitForExit($TimeoutMinutes * 60000)) {
    & taskkill /T /F /PID $p.Id | Out-Null
    return [pscustomobject]@{ Code = -999; Timeout = $true; Text = '' }
  }
  $p.WaitForExit()
  $text = ''
  foreach ($f in $out, $err) { if (Test-Path $f) { $text += [IO.File]::ReadAllText($f) } }
  [pscustomobject]@{ Code = $p.ExitCode; Timeout = $false; Text = $text }
}

$rows = New-Object System.Collections.ArrayList
$cmds = @($sel | ForEach-Object { [string]$_.cmd } | Select-Object -Unique)
$baselineBad = @{}
$baselineLines = New-Object System.Collections.ArrayList
foreach ($c in $cmds) {
  $t0 = Get-Date
  $r = Invoke-Cargo $c
  $line = [regex]::Match($r.Text, '(?m)^test result: .*$').Value
  [void]$baselineLines.Add(('`{0}` -> exit {1} in {2:N0}s {3}' -f $c, $r.Code, ((Get-Date) - $t0).TotalSeconds, $line))
  if ($r.Code -ne 0) { $baselineBad[$c] = $true }
}

foreach ($m in $sel) {
  $a = $anchors[$m.id]
  if ($baselineBad.ContainsKey([string]$m.cmd)) { [void]$rows.Add(@($m.id, 'NOT-RUN', 'baseline failed', $m.desc)); continue }
  if ($a.Count -ne 1 -or $a.Old -eq $a.New) { [void]$rows.Add(@($m.id, 'SKIP', "anchor count=$($a.Count)", $m.desc)); continue }
  $cp = Join-Path $copy (([string]$m.file) -replace '/', '\')
  $mut = $a.Norm.Replace($a.Old, $a.New)
  if ($a.Crlf) { $mut = $mut.Replace("`n", "`r`n") }
  [IO.File]::WriteAllText($cp, $mut)
  $r = Invoke-Cargo ([string]$m.cmd)
  $failed = @([regex]::Matches($r.Text, '(?m)^test (\S+) \.\.\. FAILED') | Select-Object -First 3 | ForEach-Object { $_.Groups[1].Value })
  $compile = [regex]::IsMatch($r.Text, '(?m)^error(\[E\d+\]:|: could not compile)')
  $verdict = if ($r.Timeout) { 'TIMEOUT' } elseif ($r.Code -ne 0 -and $failed.Count -gt 0) { 'KILLED' } `
    elseif ($compile) { 'COMPILE-ERROR' } elseif ($r.Code -eq 0) { 'SURVIVED' } else { "EXIT-$($r.Code)" }
  [IO.File]::WriteAllText($cp, $a.Orig)
  $restored = ((Get-FileHash $cp -Algorithm SHA256).Hash -eq (Get-FileHash $a.Path -Algorithm SHA256).Hash)
  $detail = if ($failed.Count -gt 0) { $failed -join ', ' } else { '' }
  if (-not $restored) { $detail += ' RESTORE-HASH-MISMATCH' }
  [void]$rows.Add(@($m.id, $verdict, $detail, $m.desc))
}

$count = { param($v) @($rows | Where-Object { $_[1] -eq $v }).Count }
$sb = New-Object System.Text.StringBuilder
[void]$sb.AppendLine("# Mutation run $Name")
[void]$sb.AppendLine('')
[void]$sb.AppendLine("- Candidate commit: ``$sha`` (clean checkout, mutants applied only to a temporary copy)")
[void]$sb.AppendLine("- Machine: $env:COMPUTERNAME; $(rustc --version); $(cargo --version)")
[void]$sb.AppendLine("- Started $($started.ToString('s')), finished $((Get-Date).ToString('s'))")
foreach ($l in $baselineLines) { [void]$sb.AppendLine("- Baseline: $l") }
[void]$sb.AppendLine('')
[void]$sb.AppendLine('| id | verdict | failing tests | mutant |')
[void]$sb.AppendLine('|---|---|---|---|')
foreach ($r in $rows) { [void]$sb.AppendLine(('| {0} | {1} | {2} | {3} |' -f $r[0], $r[1], $r[2], ([string]$r[3]).Replace('|', '/'))) }
[void]$sb.AppendLine('')
[void]$sb.AppendLine(('Counts: killed={0} survived={1} compile-error={2} skipped={3} timeout={4} not-run={5}' -f `
  (& $count 'KILLED'), (& $count 'SURVIVED'), (& $count 'COMPILE-ERROR'), (& $count 'SKIP'), (& $count 'TIMEOUT'), (& $count 'NOT-RUN')))
$summary = $sb.ToString()
$keepPath = Join-Path $env:TEMP "vlite-mut-$Name-summary.md"
[IO.File]::WriteAllText($keepPath, $summary)
if (-not $Keep) { Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue }
$summary
"SUMMARY-FILE $keepPath"
