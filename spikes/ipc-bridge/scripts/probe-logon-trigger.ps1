# M0-D: can a non-elevated user register a LogonTrigger scoped to themselves?
# Creates \SeleneRetrievalSpike\LogonTriggerProbe (disabled trigger), reports, deletes it.
$ErrorActionPreference = 'Continue'
$sid = ([System.Security.Principal.WindowsIdentity]::GetCurrent()).User.Value
$d = Join-Path $PSScriptRoot '..\target\release\sr-daemon.exe' | Resolve-Path
$xml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>false</Enabled><UserId>$sid</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>$sid</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries></Settings>
  <Actions Context="Author"><Exec><Command>"$d"</Command><Arguments>--launched-by task-logontrigger</Arguments></Exec></Actions>
</Task>
"@
$f = Join-Path $env:TEMP 'sr-m0d-logon-probe.xml'
[System.IO.File]::WriteAllText($f, $xml, [System.Text.Encoding]::Unicode)
schtasks /create /tn "\SeleneRetrievalSpike\LogonTriggerProbe" /xml $f /f
"create exit=$LASTEXITCODE"
schtasks /delete /tn "\SeleneRetrievalSpike\LogonTriggerProbe" /f 2>$null | Out-Null
[System.IO.File]::Delete($f)
