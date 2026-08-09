$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $root
npm ci
dotnet publish native/windows-host/Alfred.WindowsHost.csproj -c Release -r win-x64 --self-contained true
$triple = "x86_64-pc-windows-msvc"
$binaryDirectory = Join-Path $root "src-tauri\binaries"
New-Item -ItemType Directory -Force -Path $binaryDirectory | Out-Null
Copy-Item "native\windows-host\bin\Release\net10.0-windows\win-x64\publish\alfred-windows-host.exe" (Join-Path $binaryDirectory "alfred-windows-host-$triple.exe") -Force
npm run tauri build -- --config src-tauri/tauri.windows.conf.json

$distribution = Join-Path $root "dist\windows"
$portable = Join-Path $distribution "portable"
New-Item -ItemType Directory -Force -Path $portable | Out-Null
Copy-Item "src-tauri\target\release\alfred.exe" (Join-Path $portable "Alfred.exe") -Force
Copy-Item "native\windows-host\bin\Release\net10.0-windows\win-x64\publish\alfred-windows-host.exe" (Join-Path $portable "alfred-windows-host.exe") -Force
New-Item -ItemType Directory -Force -Path (Join-Path $portable "browser") | Out-Null
Copy-Item "browser\chromium-extension" (Join-Path $portable "browser\chromium-extension") -Recurse -Force
New-Item -ItemType Directory -Force -Path (Join-Path $portable "scripts\windows") | Out-Null
Copy-Item "scripts\windows\install-browser-bridge.ps1" (Join-Path $portable "scripts\windows\install-browser-bridge.ps1") -Force
$archive = Join-Path $distribution "Alfred-Windows-x64-portable.zip"
if (Test-Path $archive) { Remove-Item $archive -Force }
Compress-Archive -Path (Join-Path $portable "*") -DestinationPath $archive -CompressionLevel Optimal
$hash = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
Set-Content -Path "$archive.sha256" -Value "$hash  Alfred-Windows-x64-portable.zip" -Encoding ASCII
Write-Host "Portable release: $archive"
