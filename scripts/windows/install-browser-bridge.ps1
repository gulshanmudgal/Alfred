param(
  [Parameter(Mandatory=$true)][ValidatePattern('^[a-p]{32}$')][string]$ExtensionId,
  [string]$HostPath = ""
)
$ErrorActionPreference = "Stop"
if (-not $HostPath) {
  $candidate = Join-Path $PSScriptRoot "..\..\alfred-windows-host.exe"
  if (-not (Test-Path $candidate)) { $candidate = Join-Path $PSScriptRoot "alfred-windows-host.exe" }
  $HostPath = (Resolve-Path $candidate).Path
}
$root = Join-Path $env:LOCALAPPDATA "Alfred"
New-Item -ItemType Directory -Force -Path $root | Out-Null
$manifestPath = Join-Path $root "com.alfred.browser_bridge.json"
$manifest = [ordered]@{
  name = "com.alfred.browser_bridge"
  description = "Alfred installed-browser bridge"
  path = $HostPath
  type = "stdio"
  allowed_origins = @("chrome-extension://$ExtensionId/")
}
$manifest | ConvertTo-Json -Depth 4 | Set-Content -Encoding UTF8 $manifestPath
foreach ($key in @(
  "HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.alfred.browser_bridge",
  "HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.alfred.browser_bridge",
  "HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.alfred.browser_bridge"
)) {
  New-Item -Force -Path $key | Out-Null
  Set-Item -Path $key -Value $manifestPath
}
Write-Host "Alfred browser bridge registered for Chrome, Edge, and Brave. Restart the browser."
