param(
  [Parameter(Mandatory = $true)][string]$FilePath
)
$ErrorActionPreference = "Stop"

# Alfred is signed only when a code-signing identity is configured. Until then
# this script is a deliberate no-op so CI and local packaging keep working.
#
# Option A (local certificate / HSM / token): set ALFRED_SIGN_THUMBPRINT to the
#   SHA-1 thumbprint of a certificate in the CurrentUser\My or LocalMachine\My store.
# Option B (Azure Trusted Signing): replace the signtool invocation below with the
#   Trusted Signing dlib flow (Endpoint/Account/Profile env vars) — see
#   https://learn.microsoft.com/azure/trusted-signing/how-to-signing-integrations

$thumbprint = $env:ALFRED_SIGN_THUMBPRINT
if ([string]::IsNullOrWhiteSpace($thumbprint)) {
  Write-Host "Alfred signing not configured; leaving $([IO.Path]::GetFileName($FilePath)) unsigned."
  exit 0
}

$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue |
  Sort-Object FullName -Descending | Select-Object -First 1
if (-not $signtool) { throw "signtool.exe not found; install the Windows SDK signing tools." }

& $signtool.FullName sign /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com /sha1 $thumbprint /v $FilePath
if ($LASTEXITCODE -ne 0) { throw "signtool failed for $FilePath (exit $LASTEXITCODE)." }
Write-Host "Signed $([IO.Path]::GetFileName($FilePath))."
