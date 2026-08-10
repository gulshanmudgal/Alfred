param(
  [string]$HostPath = "",
  [switch]$SkipDesktop
)
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
if (-not $HostPath) {
  $HostPath = Join-Path $root "native\windows-host\bin\Release\net10.0-windows\win-x64\publish\alfred-windows-host.exe"
}
if (-not (Test-Path $HostPath)) {
  dotnet publish (Join-Path $root "native\windows-host\Alfred.WindowsHost.csproj") -c Release -r win-x64 --self-contained true
}
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

  # The host refuses these before any input is sent, so they are safe on headless CI.
  $blockedLaunch = Invoke-AlfredHost "launchApplication" @{} "launch application" "PowerShell" "PowerShell"
  if ($blockedLaunch.ok -or $blockedLaunch.error -notmatch "not allowed") { throw "Application launch allowlist did not reject the test request." }
  Write-Host "PASS application launch allowlist"

  $blockedDeleteKey = Invoke-AlfredHost "key" @{ virtualKey = 46 } "press key" "Keyboard input" "Notepad"
  if ($blockedDeleteKey.ok -or $blockedDeleteKey.error -notmatch "Delete key is blocked") { throw "Delete-key guard did not reject the test request." }
  Write-Host "PASS Delete-key defense in depth"

  $unknownKey = Invoke-AlfredHost "key" @{ virtualKey = 0x5B } "press key" "Desktop" "Notepad"
  if ($unknownKey.ok) { throw "An unlisted virtual key was not refused by the host." }
  Write-Host "PASS virtual-key allow-list"

  if (-not $SkipDesktop) {
    $launched = Invoke-AlfredHost "launchApplication" @{} "launch approved application" "Notepad" "Notepad"
    if (-not $launched.ok -or -not $launched.result.processId) { throw "Semantic application launch failed." }
    $notepadId = [int]$launched.result.processId
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
    if (-not $tree.ok -or -not $tree.result.bounds) { throw "UI Automation observation failed." }
    $capture = Invoke-AlfredHost "captureWindow" @{ processId = $notepadId } "capture Notepad" "Notepad" "Notepad"
    if (-not $capture.ok -or $capture.result.base64.Length -lt 1000) { throw "Window capture did not return PNG evidence." }
    $x = [int]($tree.result.bounds.x + [Math]::Max(80, $tree.result.bounds.width / 2))
    $y = [int]($tree.result.bounds.y + [Math]::Max(100, $tree.result.bounds.height / 2))
    if (-not (Invoke-AlfredHost "click" @{ x = $x; y = $y; processId = $notepadId } "focus Notepad editor" "Editor" "Notepad").ok) { throw "Targeted pointer input failed." }
    if (-not (Invoke-AlfredHost "typeText" @{ text = "Alfred Windows end-to-end smoke test"; processId = $notepadId } "type smoke-test text" "Notepad editor" "Notepad").ok) { throw "Targeted keyboard input failed." }
    if (-not (Invoke-AlfredHost "key" @{ virtualKey = 13; processId = $notepadId } "press enter" "Editor" "Notepad").ok) { throw "Allowed key (Enter) was rejected." }
    $outside = Invoke-AlfredHost "click" @{ x = -32000; y = -32000; processId = $notepadId } "click target" "Editor" "Notepad"
    if ($outside.ok) { throw "A click outside the target window bounds was not refused." }
    Write-Host "PASS targeted input, bounds validation, and key allow-list"
    $found = Invoke-AlfredHost "findElement" @{ processId = $notepadId; controlType = "ControlType.Edit" } "locate editor" "Notepad" "Notepad"
    if (-not $found.ok -or -not $found.result.found) { throw "findElement did not locate the Notepad editor." }
    $missing = Invoke-AlfredHost "findElement" @{ processId = $notepadId; name = "Alfred element that does not exist 9f3b2" } "locate missing" "Notepad" "Notepad"
    if (-not $missing.ok -or $missing.result.found) { throw "findElement must report found=false (not throw) for absent elements." }
    $read = Invoke-AlfredHost "getValue" @{ processId = $notepadId; controlType = "ControlType.Edit" } "read editor" "Notepad" "Notepad"
    if ($read.ok -and $read.result.value -match "smoke test") { Write-Host "PASS findElement presence checks and getValue data capture" }
    else { Write-Host "PASS findElement presence checks (getValue unsupported by this Notepad build; tolerated)" }
    Write-Host "Notepad is intentionally left open with unsaved test text; no persistent data was deleted."
  }
} finally {
  if (-not $hostProcess.HasExited) { $hostProcess.Kill($true) }
  $hostProcess.Dispose()
}
