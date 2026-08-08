$ErrorActionPreference = "Stop"
$AlfredRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $AlfredRoot

if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    Write-Error "Alfred needs Node.js 20 or newer. Install Node.js, then run this command again."
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "Alfred needs the stable Rust toolchain. Install Rust from https://rustup.rs and try again."
}

if (-not (Get-Command dotnet -ErrorAction SilentlyContinue)) {
    Write-Error "Alfred needs the .NET 10 SDK for its Windows automation host. Install it, then run this command again."
}

if (-not (Test-Path "node_modules")) {
    Write-Host "Preparing Alfred for first launch…"
    npm install
}

dotnet build native/windows-host/Alfred.WindowsHost.csproj -c Debug
$env:ALFRED_WINDOWS_HOST_PATH = (Resolve-Path "native/windows-host/bin/Debug/net10.0-windows/win-x64/alfred-windows-host.exe").Path

npm run alfred
