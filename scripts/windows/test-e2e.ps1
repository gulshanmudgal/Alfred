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

  $blockedLaunch = Invoke-AlfredHost "launchApplication" @{} "launch application" "PowerShell" "PowerShell"
  if ($blockedLaunch.ok -or $blockedLaunch.error -notmatch "not allowed") { throw "Application launch allowlist did not reject the test request." }
  Write-Host "PASS application launch allowlist"

  $blockedDeleteKey = Invoke-AlfredHost "key" @{ virtualKey = 46 } "press key" "Delete" "Notepad"
  if ($blockedDeleteKey.ok -or $blockedDeleteKey.error -notmatch "Delete key is blocked") { throw "Delete-key guard did not reject the test request." }
  Write-Host "PASS Delete-key defense in depth"

  if (-not $SkipDesktop) {
    $launched = Invoke-AlfredHost "launchApplication" @{} "launch approved application" "Notepad" "Notepad"
    if (-not $launched.ok -or -not $launched.result.processId) { throw "Semantic application launch failed." }
    $notepadId = [int]$launched.result.processId
    $apps = Invoke-AlfredHost "listApplications"
    $app = $apps.result | Where-Object { $_.processId -eq $notepadId } | Select-Object -First 1
    if (-not $app) { throw "Notepad was not visible to the Windows host." }
    $focused = Invoke-AlfredHost "focusApplication" @{ processId = $notepadId } "focus approved application" "Notepad" "Notepad"
    if (-not $focused.ok) { throw "Semantic application focus failed." }
    $tree = Invoke-AlfredHost "observeWindow" @{ processId = $notepadId } "observe Notepad" "Notepad" "Notepad"
    if (-not $tree.ok -or -not $tree.result.bounds) { throw "UI Automation observation failed." }
    $capture = Invoke-AlfredHost "captureWindow" @{ processId = $notepadId } "capture Notepad" "Notepad" "Notepad"
    if (-not $capture.ok -or $capture.result.base64.Length -lt 1000) { throw "Window capture did not return PNG evidence." }
    $x = [int]($tree.result.bounds.x + [Math]::Max(80, $tree.result.bounds.width / 2))
    $y = [int]($tree.result.bounds.y + [Math]::Max(100, $tree.result.bounds.height / 2))
    if (-not (Invoke-AlfredHost "click" @{ x = $x; y = $y } "focus Notepad editor" "Editor" "Notepad").ok) { throw "Pointer input failed." }
    if (-not (Invoke-AlfredHost "typeText" @{ text = "Alfred Windows end-to-end smoke test" } "type smoke-test text" "Notepad editor" "Notepad").ok) { throw "Keyboard input failed." }
    Write-Host "PASS application launch, focus, discovery, UI Automation, capture, pointer, and keyboard"
    Write-Host "Notepad is intentionally left open with unsaved test text; no persistent data was deleted."
  }
} finally {
  if (-not $hostProcess.HasExited) { $hostProcess.Kill($true) }
  $hostProcess.Dispose()
}
