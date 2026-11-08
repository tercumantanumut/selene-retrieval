<#
.SYNOPSIS
  Zero-config first run of the installed Selene Retrieval with a network-endpoint watch (M2;
  Amendment 5.3 installer-smoke job). No admin.

.DESCRIPTION
  1. Preconditions: no default data dir, no \SR task, no uninstall key (refuses to run otherwise,
     so it can never touch someone's real index).
  2. Silent install to a temp per-user folder (path with a space).
  3. A background watch records every TCP/UDP endpoint owned by any sr-* process, polling every
     150 ms for the whole first run. Expected: none (local-only by default; no model download
     without a click; no TCP port by default).
  4. First run exactly as Claude would start it: sr-bridge.exe (stdio) -> sr-launch -> scheduled
     task -> sr-daemon. Before it: no settings file. After it: settings.toml created with defaults,
     the bundled model in use.
  5. A folder is added with the hidden support CLI (the wizard's path is checked by the app tests),
     indexed, and found through MCP `context_search` over the bridge.
  6. Silent uninstall with /DELETEAPPDATA: nothing left (install dir, task, \SR folder, sandbox
     profile, uninstall key, Start menu link, processes, index).
  Results: <Out>\firstrun.json, <Out>\netwatch.txt and a transcript. Exit 0 = PASS.
#>
param(
  [Parameter(Mandatory = $true)][string]$Installer,
  [string]$Out = (Join-Path $env:TEMP "sr-firstrun-verify"),
  # Optional: a client that drives the wizard's app-RPC path (estimate -> add -> pushed progress
  # -> search) like sr-app does; without it the folder is added with the hidden support CLI.
  [string]$WizardClient = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2
New-Item -ItemType Directory -Force $Out | Out-Null
Start-Transcript -Path (Join-Path $Out "firstrun.log") -Force | Out-Null

$results = [ordered]@{}
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [bool]$ok, [string]$detail = "") {
  $results[$name] = [ordered]@{ ok = $ok; detail = $detail }
  if ($ok) { Write-Host "PASS  $name $detail" } else { Write-Host "FAIL  $name $detail"; $failures.Add($name) }
}

$sys = Join-Path $env:SystemRoot "System32"
$dataDir = Join-Path $env:LOCALAPPDATA "SeleneRetrieval"
$uninstKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\SeleneRetrieval"
$lnk = Join-Path ([Environment]::GetFolderPath("Programs")) "Selene Retrieval.lnk"
$root = Join-Path $env:TEMP ("sr-fr-" + [Guid]::NewGuid().ToString("N").Substring(0, 8))
$installDir = Join-Path $root "Selene Retrieval"
$corpus = Join-Path $root "My Notes"
$stopFile = Join-Path $Out "netwatch.stop"
$netOut = Join-Path $Out "netwatch.txt"

function TaskFolderExists {
  $s = New-Object -ComObject Schedule.Service; $s.Connect()
  try { [void]$s.GetFolder("\SR"); return $true } catch { return $false }
}
function SrProcs {
  @(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($installDir, [StringComparison]::OrdinalIgnoreCase) })
}
function WaitUninstaller {
  $deadline = (Get-Date).AddSeconds(180)
  do {
    Start-Sleep -Milliseconds 500
    $u = @(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and $_.ExecutablePath -like "*\~nsu*.tmp\*" })
  } while ($u.Count -gt 0 -and (Get-Date) -lt $deadline)
  return ($u.Count -eq 0)
}

# ---- 1. Preconditions ---------------------------------------------------------------------------
if ((Test-Path $dataDir) -or (Test-Path $uninstKey) -or (TaskFolderExists)) {
  Write-Host "Refusing to run: a Selene Retrieval install, task or index already exists for this user."
  Stop-Transcript | Out-Null; exit 2
}
New-Item -ItemType Directory -Force $root, $corpus | Out-Null
Set-Content -Path (Join-Path $corpus "trip plan.md") -Value "# Trip plan`n`nThe quokkafest picnic is on the second Saturday, near the lighthouse.`n"
Set-Content -Path (Join-Path $corpus "todo.txt") -Value "Buy batteries for the lantern.`n"
if (Test-Path $stopFile) { Remove-Item $stopFile -Force }

# ---- 3. Network watch (started before anything of ours runs) -------------------------------------
$watch = Start-Job -ArgumentList $netOut, $stopFile -ScriptBlock {
  param([string]$OutFile, [string]$Stop)
  $seen = @{}; $polls = 0; $procs = @{}
  $sw = [Diagnostics.Stopwatch]::StartNew()
  while (-not (Test-Path $Stop) -and $sw.Elapsed.TotalSeconds -lt 3600) {
    $ps = @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $_.ProcessName -like "sr-*" })
    $pids = @($ps | ForEach-Object { $_.Id })
    foreach ($p in $ps) { $procs["$($p.ProcessName)"] = 1 }
    if ($pids.Count -gt 0) {
      Get-NetTCPConnection -ErrorAction SilentlyContinue | Where-Object { $pids -contains $_.OwningProcess } | ForEach-Object {
        $seen["TCP $($_.LocalAddress):$($_.LocalPort) -> $($_.RemoteAddress):$($_.RemotePort) $($_.State)"] = 1 }
      Get-NetUDPEndpoint -ErrorAction SilentlyContinue | Where-Object { $pids -contains $_.OwningProcess } | ForEach-Object {
        $seen["UDP $($_.LocalAddress):$($_.LocalPort)"] = 1 }
    }
    $polls++; Start-Sleep -Milliseconds 150
  }
  $lines = @("polls=$polls", "endpoints=$($seen.Count)", "--- processes seen:") + ($procs.Keys | Sort-Object) + @("--- endpoints:") + ($seen.Keys | Sort-Object)
  $lines | Out-File -Encoding utf8 $OutFile
}

try {
  # ---- 2. Install --------------------------------------------------------------------------------
  $p = Start-Process -FilePath $Installer -ArgumentList @("/S", "/D=$installDir") -Wait -PassThru
  Check "install.exit_code" ($p.ExitCode -eq 0) "exit $($p.ExitCode)"
  Check "first_run.no_settings_before" (-not (Test-Path (Join-Path $dataDir "settings.toml"))) ""

  # ---- 4. First run through the bridge (as an MCP client starts it) ------------------------------
  $psi = New-Object Diagnostics.ProcessStartInfo
  $psi.FileName = Join-Path $installDir "sr-bridge.exe"
  $psi.Arguments = "--client claude"
  $psi.UseShellExecute = $false; $psi.RedirectStandardInput = $true; $psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true
  $psi.CreateNoWindow = $true
  # A stripped environment: no dev hooks, no PATH beyond System32.
  foreach ($k in @($psi.EnvironmentVariables.Keys)) { if ($k -like "SR_*") { $psi.EnvironmentVariables.Remove($k) } }
  $psi.EnvironmentVariables["PATH"] = $sys
  $b = [Diagnostics.Process]::Start($psi)
  $errTask = $b.StandardError.ReadToEndAsync()
  $script:pendingRead = $null
  function Rpc([string]$line, [int]$id, [int]$timeoutSec) {
    try {
      $b.StandardInput.WriteLine($line); $b.StandardInput.Flush()
      if ($id -lt 0) { return $null }
      $deadline = (Get-Date).AddSeconds($timeoutSec)
      while ((Get-Date) -lt $deadline) {
        if ($null -eq $script:pendingRead) { $script:pendingRead = $b.StandardOutput.ReadLineAsync() }
        $left = [int][math]::Max(1, ($deadline - (Get-Date)).TotalMilliseconds)
        if (-not $script:pendingRead.Wait($left)) { return $null }
        $text = $script:pendingRead.Result; $script:pendingRead = $null
        if ($null -eq $text) { return $null }
        $m = $text | ConvertFrom-Json
        if (($m.PSObject.Properties.Name -contains "id") -and $m.id -eq $id) { return $m }
      }
    } catch { Write-Host "rpc error: $($_.Exception.Message)" }
    return $null
  }
  function Field($o, [string[]]$path) {
    foreach ($k in $path) {
      if ($null -eq $o -or -not ($o.PSObject.Properties.Name -contains $k)) { return $null }
      $o = $o.$k
    }
    return $o
  }
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $init = Rpc '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"firstrun","version":"1"}}}' 1 120
  $results["first_run.initialize_ms_cold"] = $sw.ElapsedMilliseconds
  Check "first_run.initialize" ($null -ne (Field $init @("result", "serverInfo", "name"))) ("ms=" + $sw.ElapsedMilliseconds)
  [void](Rpc '{"jsonrpc":"2.0","method":"notifications/initialized"}' -1 0)
  $st = Rpc '{"jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"sr://status"}}' 2 60
  Check "first_run.status" ($null -ne (Field $st @("result", "contents"))) ""
  $d = @(SrProcs) | Where-Object { $_.Name -eq "sr-daemon.exe" } | Select-Object -First 1
  # Amendment 3.9: sr-launch starts the daemon outside the client's Job (breakaway) and falls back
  # to the per-user scheduled task; never in-job on a normal desktop.
  $how = $(if ($d) { ([regex]::Match($d.CommandLine, "--launched-by (\S+)")).Groups[1].Value } else { "" })
  $results["first_run.launched_by"] = $how
  Check "first_run.daemon_started_detached" ($how -eq "breakaway" -or $how -eq "task") ("launched-by " + $how)

  # Settings created with defaults, bundled model in use.
  $daemon = Join-Path $installDir "sr-daemon.exe"
  $deadline = (Get-Date).AddSeconds(60)
  while (-not (Test-Path (Join-Path $dataDir "settings.toml")) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
  Check "first_run.settings_created" (Test-Path (Join-Path $dataDir "settings.toml")) ""
  $diag = & $daemon admin diagnostics 2>&1 | Out-String
  Check "first_run.bundled_model" ($diag -match "bge-small-en-v1.5") ""

  # ---- 5. A folder, indexed, found from MCP ------------------------------------------------------
  if ($WizardClient -ne "") {
    $o = & $WizardClient $installDir $corpus "when is the quokkafest picnic" "trip plan.md" 2>&1 | Out-String
    $o | Set-Content (Join-Path $Out "wizard.json")
    $results["wizard"] = $o.Trim()
    Check "first_run.wizard_app_rpc" ($LASTEXITCODE -eq 0) $o.Trim()
  } else {
    $o = & $daemon admin folder add $corpus --wait 2>&1 | Out-String
    Check "first_run.folder_added" ($LASTEXITCODE -eq 0 -and $o -match '"synced"') ""
  }
  $s = Rpc '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"when is the quokkafest picnic"}}}' 3 120
  $text = (Field $s @("result", "content")) | ForEach-Object { $_.text } | Out-String
  Check "first_run.search_finds_the_file" ($text -match "trip plan\.md") ""
  $b.StandardInput.Close()
  if (-not $b.WaitForExit(10000)) { $b.Kill() }
  $errTask.Result | Set-Content (Join-Path $Out "bridge-stderr.txt")

  # ---- 6. Uninstall, delete everything -----------------------------------------------------------
  $p = Start-Process -FilePath (Join-Path $installDir "uninstall.exe") -ArgumentList @("/S", "/DELETEAPPDATA") -Wait -PassThru
  Check "uninstall.exit_code" ($p.ExitCode -eq 0) "exit $($p.ExitCode)"
  Check "uninstall.finished" (WaitUninstaller) ""
  Check "uninstall.install_dir_gone" (-not (Test-Path $installDir)) ""
  Check "uninstall.task_folder_gone" (-not (TaskFolderExists)) ""
  Check "uninstall.uninstall_key_gone" (-not (Test-Path $uninstKey)) ""
  Check "uninstall.start_menu_gone" (-not (Test-Path $lnk)) ""
  Check "uninstall.no_processes" (@(SrProcs).Count -eq 0) ""
  Check "uninstall.index_deleted" (-not (Test-Path $dataDir)) ""
}
finally {
  New-Item -ItemType File -Force $stopFile | Out-Null
  [void](Wait-Job $watch -Timeout 30); Remove-Job $watch -Force -ErrorAction SilentlyContinue
  foreach ($x in @(SrProcs)) { Stop-Process -Id $x.ProcessId -Force -ErrorAction SilentlyContinue }
  if (Test-Path (Join-Path $installDir "uninstall.exe")) {
    Start-Process -FilePath (Join-Path $installDir "uninstall.exe") -ArgumentList @("/S", "/DELETEAPPDATA") -Wait | Out-Null
    [void](WaitUninstaller)
  }
  if (Test-Path $root) { Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue }
  if (Test-Path $stopFile) { Remove-Item $stopFile -Force -ErrorAction SilentlyContinue }
}
$net = if (Test-Path $netOut) { Get-Content $netOut } else { @() }
$endpoints = ($net | Where-Object { $_ -like "endpoints=*" }) -replace "endpoints=", ""
$results["netwatch"] = $net
Check "network.no_endpoints" ($endpoints -eq "0") ("endpoints=" + $endpoints)
$results["failures"] = @($failures)
$results | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $Out "firstrun.json")
Stop-Transcript | Out-Null
if ($failures.Count -gt 0) { Write-Host "FIRSTRUN FAILED: $($failures -join ', ')"; exit 1 }
Write-Host "FIRSTRUN PASSED"
exit 0
