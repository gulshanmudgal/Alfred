$ErrorActionPreference = "Stop"
$AlfredRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $AlfredRoot

if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    Write-Error "Alfred development needs Node.js 20 or newer."
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "Alfred development needs the stable Rust toolchain."
}
if (-not (Get-Command dotnet -ErrorAction SilentlyContinue)) {
    Write-Error "Alfred development needs the .NET 10 SDK."
}

if (-not (Test-Path "node_modules")) {
    Write-Host "Preparing the Alfred development environment..."
    npm install
}

dotnet build native/windows-host/Alfred.WindowsHost.csproj -c Debug
$env:ALFRED_WINDOWS_HOST_PATH = (Resolve-Path "native/windows-host/bin/Debug/net10.0-windows/win-x64/alfred-windows-host.exe").Path
npm run alfred
