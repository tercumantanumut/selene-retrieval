<#
.SYNOPSIS
  Install / uninstall round trip of the Selene Retrieval per-user installer (M2-I), no admin.

.DESCRIPTION
  1. Preconditions: no default data dir, no \SR task, no uninstall key, no sr-* process from the
     test folder (refuses to run otherwise, so it can never delete someone's real index).
  2. Silent install to a temp per-user folder whose path contains a space.
  3. Installed files, Apps & features key, Start menu shortcut, install-dir ACE, per-user task.
  4. check-crt / check-delayload against the INSTALLED exes (xtask, --no-build --exe).
  5. Daemon started through the scheduled task (schtasks /run), proven by its command line.
  6. sr-bridge MCP round trip: initialize -> notifications/initialized -> tools/list.
  7. Silent uninstall keeping the index -> nothing left except the data dir.
  8. Reinstall over the kept index, silent uninstall with /DELETEAPPDATA -> nothing left at all.
  9. Restores the extractor AppContainer profile if (and only if) it existed before the run.
  Results: <Out>\roundtrip.json and a transcript.

  Also used unchanged on the clean VM / Windows Sandbox (installer\verify\clean-vm.md); pass
  -SkipGuards there when no xtask build is available.

  -Msi (M5 tester): the same checks for the per-user MSI (`msiexec /i <msi> /qn INSTALLFOLDER=...`,
  `msiexec /x <msi> /qn [DELETEAPPDATA=1]`). The MSI has no uninstall.exe and its Apps & features
  entry is keyed by its product code (found by DisplayName "Selene Retrieval").
#>
param(
  [Parameter(Mandatory = $true)][string]$Installer,
  [string]$Xtask = "",
  [string]$Out = (Join-Path $env:TEMP "sr-m2i-verify"),
  [switch]$SkipGuards,
  [switch]$Msi
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2
New-Item -ItemType Directory -Force $Out | Out-Null
Start-Transcript -Path (Join-Path $Out "roundtrip.log") -Force | Out-Null

$results = [ordered]@{}
$failures = New-Object System.Collections.Generic.List[string]
function Check([string]$name, [bool]$ok, [string]$detail = "") {
  $results[$name] = [ordered]@{ ok = $ok; detail = $detail }
  if ($ok) { Write-Host "PASS  $name $detail" } else { Write-Host "FAIL  $name $detail"; $failures.Add($name) }
}

$sys = Join-Path $env:SystemRoot "System32"
$schtasks = Join-Path $sys "schtasks.exe"
$dataDir = Join-Path $env:LOCALAPPDATA "SeleneRetrieval"
$uninstKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\SeleneRetrieval"
$msiexec = Join-Path $env:SystemRoot "System32\msiexec.exe"
# MSI: the Apps & features entry (product code key) for "Selene Retrieval", or $null.
function MsiArpKey {
  Get-ChildItem "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall" -ErrorAction SilentlyContinue |
    Where-Object { (Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue).DisplayName -eq "Selene Retrieval" } |
    Select-Object -First 1
}
function OurArpExists { (Test-Path $uninstKey) -or ($null -ne (MsiArpKey)) }
function InstallPkg([string]$tag) {
  if ($Msi) {
    $log = Join-Path $Out "msi-$tag.log"
    return Start-Process -FilePath $msiexec -ArgumentList @("/i", "`"$Installer`"", "/qn", "INSTALLFOLDER=`"$installDir`"", "/l*v", "`"$log`"") -Wait -PassThru
  }
  return Start-Process -FilePath $Installer -ArgumentList @("/S", "/D=$installDir") -Wait -PassThru
}
function UninstallPkg([bool]$deleteData, [string]$tag) {
  if ($Msi) {
    $log = Join-Path $Out "msi-$tag.log"
    $a = @("/x", "`"$Installer`"", "/qn", "/l*v", "`"$log`"")
    if ($deleteData) { $a += "DELETEAPPDATA=1" }
    return Start-Process -FilePath $msiexec -ArgumentList $a -Wait -PassThru
  }
  $a = @("/S"); if ($deleteData) { $a += "/DELETEAPPDATA" }
  return Start-Process -FilePath (Join-Path $installDir "uninstall.exe") -ArgumentList $a -Wait -PassThru
}
$lnk = Join-Path ([Environment]::GetFolderPath("Programs")) "Selene Retrieval.lnk"
$profileName = "SeleneRetrieval.Extract"
$profileDir = Join-Path $env:LOCALAPPDATA ("Packages\" + $profileName.ToLower())
$mappings = "HKCU:\Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Mappings"
$root = Join-Path $env:TEMP ("sr-m2i-" + [Guid]::NewGuid().ToString("N").Substring(0, 8))
$installDir = Join-Path $root "Selene Retrieval"

Add-Type -Namespace SrVerify -Name Native -MemberDefinition @'
[DllImport("userenv.dll", CharSet = CharSet.Unicode)]
public static extern int CreateAppContainerProfile(string name, string display, string desc, System.IntPtr caps, int count, out System.IntPtr sid);
[DllImport("userenv.dll", CharSet = CharSet.Unicode)]
public static extern int DeriveAppContainerSidFromAppContainerName(string name, out System.IntPtr sid);
[DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
public static extern bool ConvertSidToStringSid(System.IntPtr sid, out string s);
[DllImport("advapi32.dll")]
public static extern System.IntPtr FreeSid(System.IntPtr sid);
'@

function ProfileSid {
  $p = [IntPtr]::Zero
  if ([SrVerify.Native]::DeriveAppContainerSidFromAppContainerName($profileName, [ref]$p) -ne 0) { return "" }
  $s = ""; [void][SrVerify.Native]::ConvertSidToStringSid($p, [ref]$s); [void][SrVerify.Native]::FreeSid($p); return $s
}
function ProfileExists {
  $sid = ProfileSid
  (Test-Path $profileDir) -or ($sid -ne "" -and (Test-Path (Join-Path $mappings $sid)))
}
function TaskFolderExists {
  $s = New-Object -ComObject Schedule.Service; $s.Connect()
  try { [void]$s.GetFolder("\SR"); return $true } catch { return $false }
}
function SrProcs {
  @(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($installDir, [StringComparison]::OrdinalIgnoreCase) })
}
function Setup([string]$exeDir, [string[]]$argv) {
  $o = & (Join-Path $exeDir "sr-setup.exe") @argv 2>&1 | Out-String
  return $o
}
function WaitUninstaller {
  # The NSIS uninstaller copies itself to %TEMP%\~nsuN.tmp and exits at once; wait for the copy.
  $deadline = (Get-Date).AddSeconds(180)
  do {
    Start-Sleep -Milliseconds 500
    $u = @(Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and $_.ExecutablePath -like "*\~nsu*.tmp\*" })
  } while ($u.Count -gt 0 -and (Get-Date) -lt $deadline)
  return ($u.Count -eq 0)
}
# Claude Desktop configs (Squirrel + MSIX). Metadata only (size + last write time): the file is
# never opened by this script, and its content is never read or printed (it can hold other MCP
# servers' tokens). M2 tester: was a content hash.
function ClientConfigHashes {
  $c = @((Join-Path $env:APPDATA "Claude\claude_desktop_config.json"))
  $c += @(Get-ChildItem (Join-Path $env:LOCALAPPDATA "Packages") -Directory -Filter "Claude_*" -ErrorAction SilentlyContinue |
    ForEach-Object { Join-Path $_.FullName "LocalCache\Roaming\Claude\claude_desktop_config.json" })
  # M4: Cursor and VS Code (+ Insiders) configs, same metadata-only rule.
  $c += @((Join-Path $env:USERPROFILE ".cursor\mcp.json"))
  $c += @((Join-Path $env:APPDATA "Code\User\mcp.json"), (Join-Path $env:APPDATA "Code - Insiders\User\mcp.json"))
  $h = [ordered]@{}
  foreach ($p in $c) {
    if (Test-Path $p) { $i = Get-Item -LiteralPath $p; $h[$p] = "$($i.Length)@$($i.LastWriteTimeUtc.Ticks)" }
    if (Test-Path ($p + ".selene-retrieval.bak")) { $h[$p + ".bak"] = "present" }
  }
  return ($h | ConvertTo-Json -Compress)
}
# M3: saved AI provider keys (generic credentials "SeleneRetrieval/*"). Only target names are
# looked at (cmdkey never prints secrets); the seeded test entry holds a dummy, not a real key.
$cmdkey = Join-Path $env:SystemRoot "System32\cmdkey.exe"
$testCred = "SeleneRetrieval/roundtrip-test"
function OurCredCount {
  @(& $cmdkey /list 2>$null | Where-Object { $_ -match "SeleneRetrieval/" }).Count
}
function SeedLeftovers {
  # A provider key and both autostart values, so the checks below prove the uninstaller removes
  # them (U22, R-UX25) instead of passing because they never existed.
  & $cmdkey "/generic:$testCred" "/user:roundtrip" "/pass:not-a-real-key" | Out-Null
  $approved = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
  if (-not (Test-Path $approved)) { New-Item -Path $approved | Out-Null }   # never -Force on a key
  New-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "Selene Retrieval" -Value "`"$installDir\sr-app.exe`" --background" -Force | Out-Null
  New-ItemProperty $approved -Name "Selene Retrieval" -PropertyType Binary -Value ([byte[]](3,0,0,0,0,0,0,0,0,0,0,0)) -Force | Out-Null
  $results["$phase.seeded_provider_key"] = ((OurCredCount) -ge 1)
}
function Leftovers([bool]$expectData) {
  Check "$phase.provider_keys_gone" ((OurCredCount) -eq 0) "SeleneRetrieval/* credentials"
  Check "$phase.install_dir_gone" (-not (Test-Path $installDir)) $installDir
  $task = (Get-ScheduledTask -TaskPath "\SR\" -ErrorAction SilentlyContinue | Measure-Object).Count
  Check "$phase.task_gone" ($task -eq 0) "tasks in \SR\: $task"
  Check "$phase.task_folder_gone" (-not (TaskFolderExists)) "\SR folder"
  Check "$phase.appcontainer_profile_gone" (-not (ProfileExists)) $profileDir
  Check "$phase.uninstall_key_gone" (-not (OurArpExists)) $uninstKey
  Check "$phase.start_menu_gone" (-not (Test-Path $lnk)) $lnk
  Check "$phase.no_processes" (@(SrProcs).Count -eq 0) ""
  Check "$phase.client_configs_untouched" ((ClientConfigHashes) -eq $clientsBefore) "Claude, Cursor and VS Code config size + mtime"
  Check "$phase.startup_approved_gone" ($null -eq (Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" -Name "Selene Retrieval" -ErrorAction SilentlyContinue)) ""
  Check "$phase.webview_data_gone" (-not (Test-Path (Join-Path $env:LOCALAPPDATA "ai.selene.retrieval"))) ""
  Check "$phase.run_value_gone" ($null -eq (Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "Selene Retrieval" -ErrorAction SilentlyContinue)) ""
  if ($expectData) { Check "$phase.index_kept" (Test-Path $dataDir) $dataDir }
  else { Check "$phase.index_deleted" (-not (Test-Path $dataDir)) $dataDir }
}

# ---- 1. Preconditions -------------------------------------------------------------------------
$phase = "pre"
$profileBefore = ProfileExists
# No Selene entry may exist in a client config before the run, so "untouched" is the expectation.
$clientsBefore = ClientConfigHashes
$results["pre.profile_existed"] = $profileBefore
if ((Test-Path $dataDir) -or (OurArpExists) -or (TaskFolderExists)) {
  Write-Host "Refusing to run: a Selene Retrieval install, task or index already exists for this user."
  Stop-Transcript | Out-Null; exit 2
}
New-Item -ItemType Directory -Force $root | Out-Null

try {
  # ---- 2. Install ------------------------------------------------------------------------------
  $phase = "install1"
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $p = InstallPkg "install1"
  $results["install1.seconds"] = [math]::Round($sw.Elapsed.TotalSeconds, 1)
  Check "install1.exit_code" ($p.ExitCode -eq 0) "exit $($p.ExitCode)"

  # ---- 3. What the install left ----------------------------------------------------------------
  $want = @("sr-app.exe", "sr-daemon.exe", "sr-bridge.exe", "sr-launch.exe", "sr-extract.exe", "sr-setup.exe",
    "pdfium.dll", "DirectML.dll", "vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll", "msvcp140_1.dll",
    "models\bge-small-en-v1.5\model_optimized.onnx", "models\bge-small-en-v1.5\tokenizer.json")
  if (-not $Msi) { $want += "uninstall.exe" }
  $missing = @($want | Where-Object { -not (Test-Path (Join-Path $installDir $_)) })
  Check "install1.files" ($missing.Count -eq 0) ("missing: " + ($missing -join ", "))
  $bytes = (Get-ChildItem $installDir -Recurse -File | Measure-Object Length -Sum).Sum
  $results["installed_bytes"] = $bytes
  $results["installed_mib"] = [math]::Round($bytes / 1MB, 1)
  $results["installer_bytes"] = (Get-Item $Installer).Length
  $k = $(if ($Msi) { $m = MsiArpKey; if ($m) { Get-ItemProperty $m.PSPath } } else { Get-ItemProperty $uninstKey -ErrorAction SilentlyContinue })
  Check "install1.uninstall_key" ($null -ne $k -and "$($k.InstallLocation)".TrimEnd('\') -eq $installDir) "$(if ($k) { $k.PSPath })"
  Check "install1.start_menu" (Test-Path $lnk) $lnk
  $sddl = (Get-Acl $installDir).Sddl
  # ALL APPLICATION PACKAGES (AC) read+execute, inherited by files and folders: (A;OICI;0x1200a9;;;AC)
  Check "install1.install_dir_ace" ($sddl -match "\(A;OICI(;ID)?;0x1200a9;;;AC\)") $sddl
  $status = Setup $installDir @("status", "--install-dir", $installDir)
  $taskName = ([regex]::Match($status, "task=(\S+)")).Groups[1].Value
  $results["task_name"] = $taskName
  Check "install1.task_registered" ($status -match "task_exists=true") $taskName
  $xml = & $schtasks /query /tn $taskName /xml | Out-String
  Check "install1.task_points_at_install" ($xml -match [regex]::Escape("$installDir\sr-daemon.exe")) ""
  # Task Scheduler omits <RunLevel> on export when it is the default (LeastPrivilege).
  Check "install1.task_not_elevated" ($xml -notmatch "HighestAvailable") ""
  Check "install1.task_interactive_token" ($xml -match "<LogonType>InteractiveToken</LogonType>") ""

  # ---- 4. Binary guards on the installed exes ---------------------------------------------------
  if (-not $SkipGuards -and $Xtask -ne "") {
    $crtArgs = @("check-crt", "--no-build")
    foreach ($e in @("sr-app", "sr-bridge", "sr-launch", "sr-extract", "sr-setup")) { $crtArgs += @("--exe", (Join-Path $installDir "$e.exe")) }
    $o = & $Xtask @crtArgs 2>&1 | Out-String; $ok = ($LASTEXITCODE -eq 0)
    $o | Set-Content (Join-Path $Out "check-crt.txt")
    Check "guards.check_crt_installed" $ok ""
    $o = & $Xtask check-delayload --no-build --exe (Join-Path $installDir "sr-daemon.exe") 2>&1 | Out-String; $ok = ($LASTEXITCODE -eq 0)
    $o | Set-Content (Join-Path $Out "check-delayload.txt")
    Check "guards.check_delayload_installed" $ok ""
  }

  # ---- 5. Daemon through the task ---------------------------------------------------------------
  $phase = "daemon"
  & $schtasks /run /tn $taskName | Out-Null
  Check "daemon.task_run" ($LASTEXITCODE -eq 0) ""
  $d = $null; $deadline = (Get-Date).AddSeconds(30)
  while ($null -eq $d -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 300
    $d = @(SrProcs) | Where-Object { $_.Name -eq "sr-daemon.exe" } | Select-Object -First 1
  }
  Check "daemon.started_by_task" ($null -ne $d -and $d.CommandLine -match "--launched-by task") ($(if ($d) { $d.CommandLine } else { "no daemon" }))
  $daemonPid = $(if ($d) { $d.ProcessId } else { 0 })

  # ---- 6. Bridge MCP round trip -----------------------------------------------------------------
  $psi = New-Object Diagnostics.ProcessStartInfo
  $psi.FileName = Join-Path $installDir "sr-bridge.exe"
  $psi.Arguments = "--client m2i-verify"
  $psi.UseShellExecute = $false; $psi.RedirectStandardInput = $true; $psi.RedirectStandardOutput = $true; $psi.RedirectStandardError = $true
  $psi.CreateNoWindow = $true
  $b = [Diagnostics.Process]::Start($psi)
  $errTask = $b.StandardError.ReadToEndAsync()
  # One pending ReadLineAsync at a time (a timed-out read is reused by the next call).
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
  $init = Rpc '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"m2i-verify","version":"1"}}}' 1 60
  $results["bridge.initialize_ms"] = $sw.ElapsedMilliseconds
  $server = Field $init @("result", "serverInfo", "name")
  Check "bridge.initialize" ($null -ne $server) ("server=" + $server + " protocol=" + (Field $init @("result", "protocolVersion")))
  [void](Rpc '{"jsonrpc":"2.0","method":"notifications/initialized"}' -1 0)
  $tools = Rpc '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' 2 30
  $list = Field $tools @("result", "tools")
  $names = @($(if ($null -ne $list) { $list | ForEach-Object { $_.name } }))
  $results["bridge.tools"] = $names
  Check "bridge.tools_list" ($names.Count -gt 0) ($names -join ", ")
  $d2 = @(SrProcs) | Where-Object { $_.Name -eq "sr-daemon.exe" } | Select-Object -First 1
  Check "bridge.same_daemon_as_task" ($null -ne $d2 -and $d2.ProcessId -eq $daemonPid) "pid $daemonPid"
  # The daemon's extract pool creates the sandbox profile; the uninstall must remove it (3.17).
  $results["daemon.profile_exists_while_running"] = (ProfileExists)
  $b.StandardInput.Close()
  if (-not $b.WaitForExit(10000)) { $b.Kill() }
  $errTask.Result | Set-Content (Join-Path $Out "bridge-stderr.txt")

  # ---- 7. Uninstall, keep the index -------------------------------------------------------------
  $phase = "uninstall1"
  SeedLeftovers
  $p = UninstallPkg $false "uninstall1"
  Check "uninstall1.exit_code" ($p.ExitCode -eq 0) "exit $($p.ExitCode)"
  Check "uninstall1.finished" (WaitUninstaller) ""
  Leftovers $true

  # ---- 8. Reinstall over the kept index, uninstall with /DELETEAPPDATA --------------------------
  $phase = "install2"
  $p = InstallPkg "install2"
  Check "install2.exit_code" ($p.ExitCode -eq 0) "exit $($p.ExitCode)"
  $status = Setup $installDir @("status")
  Check "install2.task_registered" ($status -match "task_exists=true") ""
  Check "install2.index_still_there" (Test-Path (Join-Path $dataDir "catalog.db")) $dataDir
  $phase = "uninstall2"
  SeedLeftovers
  $p = UninstallPkg $true "uninstall2"
  Check "uninstall2.exit_code" ($p.ExitCode -eq 0) "exit $($p.ExitCode)"
  Check "uninstall2.finished" (WaitUninstaller) ""
  Leftovers $false
}
finally {
  # ---- 9. Leave the box as found ----------------------------------------------------------------
  foreach ($x in @(SrProcs)) { Stop-Process -Id $x.ProcessId -Force -ErrorAction SilentlyContinue }
  if ($Msi -and $null -ne (MsiArpKey)) {
    [void](UninstallPkg $true "cleanup")
  } elseif (Test-Path (Join-Path $installDir "uninstall.exe")) {
    Start-Process -FilePath (Join-Path $installDir "uninstall.exe") -ArgumentList @("/S", "/DELETEAPPDATA") -Wait | Out-Null
    [void](WaitUninstaller)
  }
  & $cmdkey "/delete:$testCred" 2>$null | Out-Null   # only our seeded test entry, if it survived
  if ($profileBefore -and -not (ProfileExists)) {
    $sid = [IntPtr]::Zero
    $hr = [SrVerify.Native]::CreateAppContainerProfile($profileName, "Selene Retrieval file reader", "Selene Retrieval file reader", [IntPtr]::Zero, 0, [ref]$sid)
    if ($hr -eq 0) { [void][SrVerify.Native]::FreeSid($sid) }
    $results["post.profile_restored"] = ($hr -eq 0)
  }
  if (Test-Path $root) { Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue }
  $results["failures"] = @($failures)
  $results | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $Out "roundtrip.json")
  Stop-Transcript | Out-Null
}
if ($failures.Count -gt 0) { Write-Host "ROUNDTRIP FAILED: $($failures -join ', ')"; exit 1 }
Write-Host "ROUNDTRIP PASSED"
exit 0
