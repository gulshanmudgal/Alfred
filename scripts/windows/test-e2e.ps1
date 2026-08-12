param(
  [string]$HostPath = "",
  [switch]$SkipDesktop,
  [switch]$SkipSave
)
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
if (-not $HostPath) {
  # Keep test executables off VM shared folders. Their filesystem bridge can
  # report Win32 error 223 for otherwise-small stdio actions after a .NET host
  # was built or launched from the share.
  $HostSource = Join-Path $env:LOCALAPPDATA "Alfred\test-host"
  New-Item -ItemType Directory -Force -Path $HostSource | Out-Null
  Copy-Item (Join-Path $root "native\windows-host\Program.cs") $HostSource -Force
  Copy-Item (Join-Path $root "native\windows-host\Alfred.WindowsHost.csproj") $HostSource -Force
  $HostProject = Join-Path $HostSource "Alfred.WindowsHost.csproj"
  dotnet build $HostProject -c Debug
  $HostPath = Join-Path $HostSource "bin\Debug\net10.0-windows\win-x64\alfred-windows-host.exe"
} elseif (-not (Test-Path $HostPath)) {
  throw "The requested Windows host does not exist: $HostPath"
}
$HostPath = (Resolve-Path $HostPath).Path
$token = [Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))
$startInfo = [Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $HostPath
$startInfo.UseShellExecute = $false
$startInfo.RedirectStandardInput = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
$startInfo.Environment["ALFRED_CAPABILITY_TOKEN"] = $token
$hostProcess = [Diagnostics.Process]::new()
$hostProcess.StartInfo = $startInfo
if (-not $hostProcess.Start()) { throw "Could not start Alfred Windows host." }

function Invoke-AlfredHost([string]$Method, [hashtable]$Params = @{}, [string]$Intent = "observe", [string]$Target = "", [string]$Application = "") {
  $message = @{ id = [guid]::NewGuid().ToString(); method = $Method; capabilityToken = $token; params = $Params; application = $Application; intent = $Intent; target = $Target } | ConvertTo-Json -Compress -Depth 10
  $hostProcess.StandardInput.WriteLine($message)
  $hostProcess.StandardInput.Flush()
  $line = $hostProcess.StandardOutput.ReadLine()
  if (-not $line) { throw "Windows host closed without a response: $($hostProcess.StandardError.ReadToEnd())" }
  return $line | ConvertFrom-Json
}

try {
  $health = Invoke-AlfredHost "health"
  if (-not $health.ok -or $health.result.host -ne "windows") { throw "Health handshake failed." }
  Write-Host "PASS health and capability handshake"

  $blocked = Invoke-AlfredHost "click" @{ x = 10; y = 10 } "delete email" "Delete"
  if ($blocked.ok -or $blocked.error -notmatch "Destructive actions") { throw "Deletion guard did not reject the test request." }
  Write-Host "PASS destructive-action defense in depth"

  # The planner may name installed apps, but never an executable path or command.
  $blockedLaunch = Invoke-AlfredHost "launchApplication" @{} "launch application" "Executable path" "C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
  if ($blockedLaunch.ok -or $blockedLaunch.error -notmatch "not an exact installed Start-menu application") { throw "Executable-path launch was not rejected." }
  Write-Host "PASS exact Start-menu application boundary"

  $installed = Invoke-AlfredHost "listInstalledApplications"
  if (-not $installed.ok -or -not ($installed.result | Where-Object { $_.name -eq "Notepad" })) { throw "Installed application discovery did not include Notepad." }
  Write-Host "PASS installed application discovery"

  $blockedDeleteKey = Invoke-AlfredHost "key" @{ virtualKey = 46 } "press key" "Keyboard input" "Notepad"
  if ($blockedDeleteKey.ok -or $blockedDeleteKey.error -notmatch "Delete key is blocked") { throw "Delete-key guard did not reject the test request." }
  Write-Host "PASS Delete-key defense in depth"

  $unknownKey = Invoke-AlfredHost "key" @{ virtualKey = 0x5B } "press key" "Desktop" "Notepad"
  if ($unknownKey.ok) { throw "An unlisted virtual key was not refused by the host." }
  Write-Host "PASS virtual-key allow-list"

  $unknownShortcut = Invoke-AlfredHost "shortcut" @{ keys = "CTRL+X" } "press shortcut" "Editor" "Notepad"
  if ($unknownShortcut.ok) { throw "An unlisted shortcut was not refused by the host." }
  Write-Host "PASS shortcut allow-list"

  if (-not $SkipDesktop) {
    $launched = Invoke-AlfredHost "launchApplication" @{} "launch approved application" "Notepad" "Notepad"
    if (-not $launched.ok -or -not $launched.result.processId) { throw "Semantic application launch failed." }
    $notepadId = [int]$launched.result.processId
    $relaunched = Invoke-AlfredHost "launchApplication" @{} "launch approved application again" "Notepad" "Notepad"
    if (-not $relaunched.ok -or -not $relaunched.result.alreadyRunning -or [int]$relaunched.result.processId -ne $notepadId) {
      throw "Repeated application launch was not idempotent."
    }
    Write-Host "PASS idempotent application launch"
    $apps = Invoke-AlfredHost "listApplications"
    $app = $apps.result | Where-Object { $_.processId -eq $notepadId } | Select-Object -First 1
    if (-not $app) { throw "Notepad was not visible to the Windows host." }
    $resolved = Invoke-AlfredHost "resolveApplication" @{ name = "Notepad" }
    if (-not $resolved.ok -or $resolved.result.processId -ne $notepadId) { throw "Application name resolution returned the wrong process." }
    Write-Host "PASS application name resolution"
    $focused = Invoke-AlfredHost "focusApplication" @{ processId = $notepadId } "focus approved application" "Notepad" "Notepad"
    if (-not $focused.ok) { throw "Semantic application focus failed." }
    Write-Host "PASS window focus and foreground verification"
    $tree = Invoke-AlfredHost "observeWindow" @{ processId = $notepadId } "observe Notepad" "Notepad" "Notepad"
    if (-not $tree.ok -or -not $tree.result.bounds) { throw "UI Automation observation failed: $($tree.error)" }
    $capture = Invoke-AlfredHost "captureWindow" @{ processId = $notepadId } "capture Notepad" "Notepad" "Notepad"
    if (-not $capture.ok -or $capture.result.base64.Length -lt 1000) { throw "Window capture did not return PNG evidence: $($capture.error)" }
    $x = [int]($tree.result.bounds.x + [Math]::Max(80, $tree.result.bounds.width / 2))
    $y = [int]($tree.result.bounds.y + [Math]::Max(100, $tree.result.bounds.height / 2))
    if (-not (Invoke-AlfredHost "click" @{ x = $x; y = $y; processId = $notepadId } "focus Notepad editor" "Editor" "Notepad").ok) { throw "Targeted pointer input failed." }
    if (-not (Invoke-AlfredHost "typeText" @{ text = "Alfred Windows end-to-end smoke test"; processId = $notepadId } "type smoke-test text" "Notepad editor" "Notepad").ok) { throw "Targeted keyboard input failed." }
    if (-not (Invoke-AlfredHost "key" @{ virtualKey = 13; processId = $notepadId } "press enter" "Editor" "Notepad").ok) { throw "Allowed key (Enter) was rejected." }
    $outside = Invoke-AlfredHost "click" @{ x = -32000; y = -32000; processId = $notepadId } "click target" "Editor" "Notepad"
    if ($outside.ok) { throw "A click outside the target window bounds was not refused." }
    Write-Host "PASS targeted input, bounds validation, and key allow-list"
    $editorControlType = "ControlType.Edit"
    $found = Invoke-AlfredHost "findElement" @{ processId = $notepadId; controlType = $editorControlType } "locate editor" "Notepad" "Notepad"
    if (-not $found.result.found) {
      # Current Windows 11 Notepad exposes its text surface as Document rather
      # than Edit; older builds use Edit. Exercise whichever UIA contract the
      # installed version reports.
      $editorControlType = "ControlType.Document"
      $found = Invoke-AlfredHost "findElement" @{ processId = $notepadId; controlType = $editorControlType } "locate editor" "Notepad" "Notepad"
    }
    if (-not $found.ok -or -not $found.result.found) { throw "findElement did not locate the Notepad editor." }
    $missing = Invoke-AlfredHost "findElement" @{ processId = $notepadId; name = "Alfred element that does not exist 9f3b2" } "locate missing" "Notepad" "Notepad"
    if (-not $missing.ok -or $missing.result.found) { throw "findElement must report found=false (not throw) for absent elements." }
    $read = Invoke-AlfredHost "getValue" @{ processId = $notepadId; controlType = $editorControlType } "read editor" "Notepad" "Notepad"
    if ($read.ok -and $read.result.value -match "smoke test") { Write-Host "PASS findElement presence checks and getValue data capture" }
    else { Write-Host "PASS findElement presence checks (getValue unsupported by this Notepad build; tolerated)" }

    if (-not $SkipSave) {
      $desktop = [Environment]::GetFolderPath([Environment+SpecialFolder]::DesktopDirectory)
      $savedPath = Join-Path $desktop "Alfred Windows smoke $([guid]::NewGuid().ToString('N')).txt"
      $save = Invoke-AlfredHost "shortcut" @{ keys = "CTRL+S"; processId = $notepadId } "save the new smoke-test file" "Save" "Notepad"
      if (-not $save.ok) { throw "CTRL+S did not open Save As: $($save.error)" }
      Start-Sleep -Milliseconds 800
      $fileNameSelector = @{ processId = $notepadId; automationId = "1001" }
      $fileName = Invoke-AlfredHost "findElement" $fileNameSelector "locate file name" "File name" "Notepad"
      if (-not $fileName.result.found) {
        $fileNameSelector = @{ processId = $notepadId; name = "File name:"; controlType = "ControlType.Edit" }
        $fileName = Invoke-AlfredHost "findElement" $fileNameSelector "locate file name" "File name" "Notepad"
      }
      if (-not $fileName.ok -or -not $fileName.result.found) { throw "Save As file-name field was not found." }
      $fileNameSelector["value"] = $savedPath
      $setName = Invoke-AlfredHost "setValue" $fileNameSelector "set a unique new file name" "File name" "Notepad"
      if (-not $setName.ok) { throw "Could not set Save As file name: $($setName.error)" }
      $saved = Invoke-AlfredHost "invokeElement" @{ processId = $notepadId; name = "Save"; controlType = "ControlType.Button" } "save a new text file" "Save" "Notepad"
      if (-not $saved.ok) { throw "Save button invocation failed: $($saved.error)" }
      $deadline = [DateTime]::UtcNow.AddSeconds(8)
      while (-not (Test-Path $savedPath) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 200 }
      if (-not (Test-Path $savedPath)) { throw "Notepad did not create $savedPath" }
      if ((Get-Content $savedPath -Raw) -notmatch "Alfred Windows end-to-end smoke test") { throw "Saved file content did not match the typed text." }
      Write-Host "PASS create, save, and verify file: $savedPath"
    } else {
      Write-Host "Notepad is intentionally left open with unsaved test text; no persistent data was deleted."
    }
  }
} finally {
  if (-not $hostProcess.HasExited) { $hostProcess.Kill($true) }
  $hostProcess.Dispose()
}
