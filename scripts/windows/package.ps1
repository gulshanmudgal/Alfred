param(
  # Rust target triple: x86_64-pc-windows-msvc (default) or aarch64-pc-windows-msvc.
  [string]$Triple = "x86_64-pc-windows-msvc"
)
$ErrorActionPreference = "Stop"
$hostRid = if ($Triple -like "aarch64*") { "win-arm64" } else { "win-x64" }
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $root
npm ci
dotnet publish native/windows-host/Alfred.WindowsHost.csproj -c Release -r $hostRid --self-contained true
$binaryDirectory = Join-Path $root "src-tauri\binaries"
New-Item -ItemType Directory -Force -Path $binaryDirectory | Out-Null
Copy-Item "native\windows-host\bin\Release\net10.0-windows\$hostRid\publish\alfred-windows-host.exe" (Join-Path $binaryDirectory "alfred-windows-host-$Triple.exe") -Force
npm run tauri build -- --target $Triple --config src-tauri/tauri.windows.conf.json
