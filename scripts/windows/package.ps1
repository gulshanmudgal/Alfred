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
