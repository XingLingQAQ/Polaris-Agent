# Uptime Polaris Agent Windows installer
# .\install.ps1 -Endpoint https://status.example.com -Token TOKEN

param(
  [string]$Endpoint = '',
  [string]$Token = "",
  [int]$SampleInterval = 0,
  [int]$ReportInterval = 0,
  [string]$Platform = '',
  [ValidateSet('system','user','manual')][string]$Mode = 'system',
  [string]$IncludeNics = '',
  [string]$ExcludeNics = '',
  [string]$IncludeMounts = '',
  [string]$ExcludeMounts = '',
  [string]$InstallDir = '',
  [string]$ServiceName = 'polaris-agent',
  [switch]$AutoUpdate,
  [switch]$DisableAutoUpdate,
  [switch]$NoRegisterCommand,
  [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

if (-not $Token -and $env:POLARIS_AGENT_TOKEN) { $Token = $env:POLARIS_AGENT_TOKEN }

function Say($m)  { Write-Host $m }
function Warn($m) { Write-Host $m -ForegroundColor Yellow }
function Die($m)  { throw $m }

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

$agentName = 'polaris-agent'

$destDir = if ($InstallDir) { $InstallDir } elseif ($Mode -eq 'system') { Join-Path $env:ProgramFiles $agentName } else { Join-Path $env:LOCALAPPDATA $agentName }
$dest = Join-Path $destDir ($agentName + '.exe')
$cfgDir = if ($Mode -eq 'system') { Join-Path $env:ProgramData $agentName } else { Join-Path $env:LOCALAPPDATA $agentName }
$cfg = Join-Path $cfgDir 'config.json'
$metadata = Join-Path $cfgDir 'install.json'
$logPath = Join-Path $cfgDir 'agent.log'
$marker = Join-Path $cfgDir ('.' + $agentName + '-managed')
if ($ServiceName -notmatch '^[A-Za-z0-9_.@-]+$') { Die 'invalid task name' }
$taskName = $ServiceName

if ($Uninstall) {
  $found = @()
  $owned = (Test-Path $marker) -and ((Get-Content $marker -Raw) -eq "$taskName|$dest")
  if ($owned) { $found += $dest,$cfg }
  $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
  if ($task -and $task.Actions.Execute -eq $dest) { $found += "task $taskName" }

  if ($found.Count -eq 0) {
    Say "nothing to remove - the probe is not installed on this machine"
    return
  }

  if ($owned -and (Test-Path -LiteralPath $metadata) -and (Test-Path -LiteralPath $dest)) {
    & $dest command unregister --config $cfg
    if ($LASTEXITCODE -ne 0) { Die "cannot remove the registered terminal command" }
  }
  Say "removing:"
  foreach ($f in $found) { Say "  $f" }
  if ($task -and $task.Actions.Execute -eq $dest) { Unregister-ScheduledTask -TaskName $taskName -Confirm:$false }
  if ($owned) {
    Remove-Item -LiteralPath $dest,$cfg,$marker,$metadata -Force -ErrorAction SilentlyContinue
    Remove-Item $destDir,$cfgDir -Force -ErrorAction SilentlyContinue
  }
  Say ""
  Say "done"
  return
}

Say "Uptime Polaris Agent installer"
Say "  host        : windows/$env:PROCESSOR_ARCHITECTURE"
Say "  elevated    : $(if ($isAdmin) { 'yes' } else { 'no' })"
Say ""

if (-not $isAdmin -and $Mode -eq 'system') {
  Die "needs an elevated PowerShell: installing to Program Files and registering
a scheduled task both require administrator rights.

Open PowerShell as Administrator and run the same command again."
}

$existingTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
if ($Mode -ne 'manual' -and $existingTask -and $existingTask.Actions.Execute -ne $dest) { Die "refusing to overwrite unrelated scheduled task $taskName" }

if (-not $Endpoint) { Die "an endpoint is required; use -Endpoint URL" }
if (-not $Token) { $Token = Read-Host "Agent token" }
if (-not $Token) { Die "an Agent token is required" }
$effectiveSample = if ($SampleInterval -gt 0) { $SampleInterval } else { 5 }
$effectiveReport = if ($ReportInterval -gt 0) { $ReportInterval } else { 60 }
if ($effectiveSample -lt 1 -or $effectiveReport -lt $effectiveSample -or $effectiveReport -gt 1800) { Die 'require 1 <= sample <= report <= 1800' }

# WOW64 reports the shell's architecture; prefer the native architecture.
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
switch ($arch) {
  'AMD64' { $archKey = 'x86_64' }
  'ARM64' { Die "unsupported platform windows/arm64; supported: linux/x86_64, linux/aarch64, windows/x86_64, darwin/x86_64, darwin/aarch64" }
  default { Die "unsupported architecture: $arch" }
}
$detectedPlatform = "windows-$archKey"
if ($Platform -and $Platform -ne $detectedPlatform) { Die "selected build $Platform does not match this host ($detectedPlatform)" }

New-Item -ItemType Directory -Path $destDir -Force | Out-Null
New-Item -ItemType Directory -Path $cfgDir -Force | Out-Null
if ((Test-Path $dest) -or (Test-Path $cfg)) {
  if (-not (Test-Path $marker) -or (Get-Content $marker -Raw) -ne "$taskName|$dest") {
    $found = @()
    if (Test-Path $dest) { $found += $dest }
    if (Test-Path $cfg)  { $found += $cfg }
    Die "refusing to overwrite files this installer does not own
  found  : $($found -join ' ')
  marker : $marker (absent or records another install)
  These are leftover files from an earlier install whose marker was removed. Delete them and re-run:
    Remove-Item -Force $($found -join ', ')"
  }
}

$releaseRepository = 'XingLingQAQ/Polaris-Agent'
$releaseBase = "https://github.com/$releaseRepository/releases"
$manifestUrl = "$releaseBase/latest/download/manifest.json"
Say "-> checking the latest stable release"
try {
  $manifest = Invoke-RestMethod -Uri $manifestUrl -UseBasicParsing -TimeoutSec 15
} catch {
  Die "cannot read the latest Agent release: $($_.Exception.Message). Check GitHub access from this machine and that a stable release includes manifest.json."
}
$releaseVersion = [string]$manifest.version
if ($manifest.repository -cne $releaseRepository -or $releaseVersion -notmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' -or $manifest.tag -cne "v$releaseVersion") {
  Die 'release manifest repository, version, or tag is invalid'
}
$entries = @($manifest.platforms | Where-Object { $_.key -ceq $detectedPlatform })
if ($entries.Count -ne 1) { Die "release manifest has no unique build for $detectedPlatform" }
$entry = $entries[0]
$releaseFile = "polaris-agent-$releaseVersion-$detectedPlatform.exe"
if ($entry.file -cne $releaseFile -or $entry.version -cne $releaseVersion -or $entry.sha256 -isnot [string] -or $entry.sha256 -notmatch '^[a-fA-F0-9]{64}$') {
  Die "release manifest has invalid metadata or SHA-256 for $detectedPlatform"
}
if (($entry.size -isnot [int] -and $entry.size -isnot [long] -and $entry.size -isnot [double]) -or $entry.size -le 0 -or $entry.size -gt 64MB -or [Math]::Floor($entry.size) -ne $entry.size) {
  Die "release manifest has no valid size for $detectedPlatform"
}
$expectedSha = $entry.sha256.ToLowerInvariant()
$expectedSize = [long]$entry.size
$url = "$releaseBase/download/$($manifest.tag)/$releaseFile"
Say "  version     : $releaseVersion"
Say "  download    : $url"
Say "-> downloading"

$stage = Join-Path $destDir ('.download-' + [Guid]::NewGuid() + '.exe')
try {
  Invoke-WebRequest -Uri $url -OutFile $stage -UseBasicParsing
} catch {
  Remove-Item -LiteralPath $stage -Force -ErrorAction SilentlyContinue
  Die "download failed: $($_.Exception.Message)

The Agent GitHub repository needs a published stable release with an asset for windows/$archKey.
Check GitHub access from this machine."
}

$magic = [System.IO.File]::ReadAllBytes($stage)[0..1]
if ($magic[0] -ne 0x4D -or $magic[1] -ne 0x5A) {
  Remove-Item -LiteralPath $stage -Force -ErrorAction SilentlyContinue
  Die "the downloaded file is not a Windows executable. Report this."
}

if ((Get-Item -LiteralPath $stage).Length -ne $expectedSize) {
  Remove-Item -LiteralPath $stage -Force -ErrorAction SilentlyContinue
  Die 'downloaded binary size does not match the release manifest'
}
# Hash through .NET: restricted or trimmed hosts may not load the
# Get-FileHash cmdlet from Microsoft.PowerShell.Utility.
# This file must stay ASCII: Windows PowerShell 5.1 reads a BOM-less script
# with the ANSI code page.
$sha256 = [System.Security.Cryptography.SHA256]::Create()
try {
  $stream = [System.IO.File]::OpenRead($stage)
  try {
    $actualSha = ([System.BitConverter]::ToString($sha256.ComputeHash($stream))).Replace('-', '').ToLowerInvariant()
  } finally {
    $stream.Dispose()
  }
} finally {
  $sha256.Dispose()
}
if ($actualSha -ne $expectedSha) {
  Remove-Item -LiteralPath $stage -Force -ErrorAction SilentlyContinue
  Die "sha256 mismatch for $detectedPlatform
  expected: $expectedSha
  actual:   $actualSha
The downloaded file does not match the release manifest; refusing to install it."
}
Say "  verified    : sha256 $actualSha"

# Keep credentials out of scheduled-task arguments.
$cfgValues = @{
  endpoint = $Endpoint
}
$cfgValues.token = $Token
if ($AutoUpdate -and $DisableAutoUpdate) { throw 'Choose AutoUpdate or DisableAutoUpdate, not both' }
if ($AutoUpdate) { $cfgValues.auto_update = $true }
if ($DisableAutoUpdate) { $cfgValues.auto_update = $false }
if ($SampleInterval -gt 0 -or $ReportInterval -gt 0) {
  if ($SampleInterval -gt 0) { $cfgValues.sample_interval = $SampleInterval }
  if ($ReportInterval -gt 0) { $cfgValues.report_interval = $ReportInterval }
} elseif (-not (Test-Path $cfg)) {
  $cfgValues.sample_interval = $effectiveSample
  $cfgValues.report_interval = $effectiveReport
}
foreach ($name in @('IncludeNics','ExcludeNics','IncludeMounts','ExcludeMounts')) {
  $value = (Get-Variable $name -ValueOnly)
  if ($value) {
    $key = [regex]::Replace($name, '[A-Z]', { param($m) '_' + $m.Value.ToLower() }).TrimStart('_').ToLower()
    $cfgValues[$key] = @($value.Split(',') | Where-Object { $_ })
  }
}
# Scheduled tasks use an explicit restart handoff.
$cfgValues.service_tier = if ($Mode -eq 'manual') { 'C' } else { 'A' }
$savedValues = @{}
if (Test-Path $cfg) {
  try {
    $saved = Get-Content -Path $cfg -Raw | ConvertFrom-Json
    $allowed = @('endpoint','token','sample_interval','report_interval','include_nics','exclude_nics','include_mounts','exclude_mounts','insecure','auto_update','log_file','runtime_revision','service_tier')
    if ($saved -isnot [PSCustomObject]) { throw 'config must be an object' }
    foreach ($property in $saved.PSObject.Properties) {
      if ($allowed -cnotcontains $property.Name) { throw 'configuration contains unsupported fields' }
      $savedValues[$property.Name] = $property.Value
    }
  } catch {
    Remove-Item -LiteralPath $stage -Force -ErrorAction SilentlyContinue
    Die 'existing config is invalid; refusing to overwrite it'
  }
}
foreach ($key in $cfgValues.Keys) { $savedValues[$key] = $cfgValues[$key] }
$savedSample = if ($savedValues.ContainsKey('sample_interval')) { $savedValues.sample_interval } else { 5 }
$savedReport = if ($savedValues.ContainsKey('report_interval')) { $savedValues.report_interval } else { 60 }
if ($savedSample -isnot [int] -and $savedSample -isnot [long] -or $savedReport -isnot [int] -and $savedReport -isnot [long] -or $savedSample -lt 1 -or $savedReport -lt $savedSample -or $savedReport -gt 1800) {
  Remove-Item -LiteralPath $stage -Force -ErrorAction SilentlyContinue
  Die 'existing config has invalid sample/report intervals'
}
$cfgJson = $savedValues | ConvertTo-Json
$binaryBackup = Join-Path $destDir ('.binary-backup-' + [guid]::NewGuid().ToString('N') + '.exe')
$cfgBackup = Join-Path $cfgDir ('.config-backup-' + [guid]::NewGuid().ToString('N') + '.tmp')
$metadataBackup = Join-Path $cfgDir ('.install-backup-' + [guid]::NewGuid().ToString('N') + '.tmp')
$binaryDiscard = Join-Path $destDir ('.binary-failed-' + [guid]::NewGuid().ToString('N') + '.exe')
$cfgDiscard = Join-Path $cfgDir ('.config-failed-' + [guid]::NewGuid().ToString('N') + '.tmp')
$cfgStage = Join-Path $cfgDir ('.config-' + [guid]::NewGuid().ToString('N') + '.tmp')
$hadBinary = Test-Path -LiteralPath $dest
$hadConfig = Test-Path -LiteralPath $cfg
$hadMetadata = Test-Path -LiteralPath $metadata
$hadMarker = Test-Path -LiteralPath $marker
$binaryInstalled = $false
$configInstalled = $false
$taskStopped = $false
$taskTouched = $false
$taskXml = $null
$rollbackFailed = $false
try {
  if ($hadMetadata) { Copy-Item -LiteralPath $metadata -Destination $metadataBackup }
  if ($Mode -ne 'manual' -and $existingTask) { $taskXml = Export-ScheduledTask -TaskName $taskName -ErrorAction Stop }
  if ($existingTask -and $existingTask.Actions.Execute -eq $dest -and $existingTask.State -eq 'Running') {
    Say "-> stopping existing Agent task"
    Stop-ScheduledTask -TaskName $taskName -ErrorAction Stop
    $taskStopped = $true
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while ((Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).State -eq 'Running') {
      if ([DateTime]::UtcNow -ge $deadline) { throw 'existing Agent task did not stop within 30 seconds' }
      Start-Sleep -Milliseconds 250
    }
  }
  $deadline = [DateTime]::UtcNow.AddSeconds(10)
  while (-not $binaryInstalled) {
    try {
      if ($hadBinary) { [IO.File]::Replace($stage, $dest, $binaryBackup) }
      else { [IO.File]::Move($stage, $dest) }
      $binaryInstalled = $true
    } catch {
      if ([DateTime]::UtcNow -ge $deadline) { throw "cannot replace Agent binary: $($_.Exception.Message). Check for another running Agent using $dest." }
      Start-Sleep -Milliseconds 250
    }
  }
  Say "  installed   : $dest"

try {
  $security = New-Object Security.AccessControl.FileSecurity
  $security.SetAccessRuleProtection($true, $false)
  $sids = if ($Mode -eq 'system') { @('S-1-5-18', 'S-1-5-32-544') } else { @([Security.Principal.WindowsIdentity]::GetCurrent().User.Value) }
  foreach ($sid in $sids) {
    $identity = New-Object Security.Principal.SecurityIdentifier($sid)
    $rule = New-Object Security.AccessControl.FileSystemAccessRule($identity, 'FullControl', 'Allow')
    $security.AddAccessRule($rule)
  }
  $stream = New-Object IO.FileStream($cfgStage, [IO.FileMode]::CreateNew, [Security.AccessControl.FileSystemRights]::Write, [IO.FileShare]::None, 4096, [IO.FileOptions]::None, $security)
  try {
    $actual = [IO.File]::GetAccessControl($cfgStage)
    if (-not $actual.AreAccessRulesProtected) { throw 'configuration ACL is inherited' }
    $rules = @($actual.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
    if ($rules.Count -ne $sids.Count -or @($rules | Where-Object { $_.IsInherited -or $_.AccessControlType -ne 'Allow' -or $sids -notcontains $_.IdentityReference.Value }).Count) { throw 'configuration ACL is not private' }
    $bytes = [Text.UTF8Encoding]::new($false).GetBytes($cfgJson)
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush($true)
  } finally { $stream.Dispose() }
  if (Test-Path -LiteralPath $cfg) {
    [IO.File]::SetAccessControl($cfg, $security)
    [IO.File]::Replace($cfgStage, $cfg, $cfgBackup)
  }
  else { [IO.File]::Move($cfgStage, $cfg) }
  $configInstalled = $true
} finally { Remove-Item -LiteralPath $cfgStage -Force -ErrorAction SilentlyContinue }
Set-Content -Path $marker -Value "$taskName|$dest" -NoNewline -Encoding ASCII
Say "  config      : $cfg (private ACL)"

$manager = if ($Mode -eq 'manual') { 'manual' } else { 'windows-task' }
$scope = if ($Mode -eq 'system') { 'system' } else { 'user' }
$recordArgs = @('record-installation', '--config', $cfg, '--manager', $manager, '--service-name', $ServiceName, '--scope', $scope, '--mode', $Mode, '--log-path', $logPath)
if ($InstallDir) { $recordArgs += @('--install-dir', $InstallDir) }
& $dest @recordArgs
if ($LASTEXITCODE -ne 0) { throw "cannot save installation information" }
if (-not $NoRegisterCommand) {
  & $dest command register --config $cfg --scope $scope
  if ($LASTEXITCODE -ne 0) { throw "cannot register the terminal command; use -NoRegisterCommand to skip registration" }
  $registered = (Get-Content -LiteralPath $metadata -Raw | ConvertFrom-Json).registration.directory
  if ($registered -and ($env:Path.Split(';') -notcontains $registered)) { $env:Path += ';' + $registered }
}
if ($Mode -eq 'manual') {
  if ($NoRegisterCommand) { Say "manual mode selected; run: & '$($dest.Replace("'", "''"))' run --config '$($cfg.Replace("'", "''"))'" }
  else { Say "manual mode selected; run: pa run" }
} else {
Say "-> registering scheduled task"
$action = New-ScheduledTaskAction -Execute $dest -Argument "run --config `"$cfg`" --log-file `"$logPath`""
$trigger = New-ScheduledTaskTrigger -AtStartup
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
if ($Mode -eq 'system') {
  $trigger = New-ScheduledTaskTrigger -AtStartup
  $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
} else {
  $trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
  $principal = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive -RunLevel Limited
}
$taskTouched = $true
Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force | Out-Null
Start-ScheduledTask -TaskName $taskName -ErrorAction Stop
}
} catch {
  $failure = $_
  if ($taskTouched) {
    try {
      $currentTask = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
      if ($currentTask -and $currentTask.State -eq 'Running') {
        Stop-ScheduledTask -TaskName $taskName -ErrorAction Stop
      }
      if ($taskXml) { Register-ScheduledTask -TaskName $taskName -Xml $taskXml -Force | Out-Null }
      elseif ($currentTask) { Unregister-ScheduledTask -TaskName $taskName -Confirm:$false }
    } catch { $rollbackFailed = $true; Warn "cannot restore scheduled task: $($_.Exception.Message)" }
  }
  try {
    if ($binaryInstalled) {
      if ($hadBinary) { [IO.File]::Replace($binaryBackup, $dest, $binaryDiscard) }
      else { Remove-Item -LiteralPath $dest -Force }
    }
    if ($configInstalled) {
      if ($hadConfig) { [IO.File]::Replace($cfgBackup, $cfg, $cfgDiscard) }
      else { Remove-Item -LiteralPath $cfg -Force }
    }
    if ($hadMetadata -and (Test-Path -LiteralPath $metadataBackup)) { Copy-Item -LiteralPath $metadataBackup -Destination $metadata -Force }
    elseif (-not $hadMetadata) { Remove-Item -LiteralPath $metadata -Force -ErrorAction SilentlyContinue }
    if (-not $hadMarker) { Remove-Item -LiteralPath $marker -Force -ErrorAction SilentlyContinue }
  } catch { $rollbackFailed = $true; Warn "cannot restore previous installation: $($_.Exception.Message)" }
  if ($taskStopped -and -not $rollbackFailed) {
    try { Start-ScheduledTask -TaskName $taskName -ErrorAction Stop }
    catch { $rollbackFailed = $true; Warn "cannot restart previous Agent task: $($_.Exception.Message)" }
  }
  if ($rollbackFailed) { Warn "recovery files retained in $destDir and $cfgDir" }
  Die "installation failed: $($failure.Exception.Message)"
} finally {
  Remove-Item -LiteralPath $stage,$cfgStage -Force -ErrorAction SilentlyContinue
  if (-not $rollbackFailed) { Remove-Item -LiteralPath $binaryBackup,$cfgBackup,$metadataBackup,$binaryDiscard,$cfgDiscard -Force -ErrorAction SilentlyContinue }
}
if ($Mode -eq 'manual') { return }

Start-Sleep -Seconds 3
$proc = Get-Process $agentName -ErrorAction SilentlyContinue
Say ""
if ($proc) {
  Say "process started (pid $($proc.Id)); awaiting first report from the server"
} else {
  Warn "! probe does not appear to be running"
}

Say ""
Say "persistence: tier A"
if ($Mode -eq 'system') { Say "  a SYSTEM scheduled task starts it at startup" }
else { Say "  a user scheduled task starts it when $env:USERNAME logs in" }
Say ""
$manage = if ($NoRegisterCommand) { "& '" + $dest.Replace("'", "''") + "' --config '" + $cfg.Replace("'", "''") + "'" } else { 'pa' }
Say "status:  $manage status"
Say "logs:    $manage logs -f"
Say "stop:    $manage stop"
Say "restart: $manage restart"
Say "remove:  the installer with -Uninstall"
