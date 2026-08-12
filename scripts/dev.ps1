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

# WebDAV/VM shared folders are suitable for editing source but not for Windows
# Node/Rust/.NET artifacts. Stage source on the Windows disk when this checkout
# is on a mapped network drive; edits remain in the shared checkout and each
# launch refreshes the local staging copy.
$AlfredDrive = Get-PSDrive -Name $AlfredRoot.Drive.Name -ErrorAction SilentlyContinue
if ($AlfredDrive -and $AlfredDrive.DisplayRoot) {
    $AlfredWindowsRoot = Join-Path $env:LOCALAPPDATA "Alfred\windows-dev"
    New-Item -ItemType Directory -Force -Path $AlfredWindowsRoot | Out-Null
    Write-Host "Staging Alfred source on the Windows system drive..."
    & robocopy $AlfredRoot.Path $AlfredWindowsRoot /E /XD .git node_modules dist target bin obj /XF *.tmp /NFL /NDL /NJH /NJS /NP
    if ($LASTEXITCODE -gt 7) {
        throw "Could not stage Alfred from $AlfredRoot to $AlfredWindowsRoot (robocopy exit code $LASTEXITCODE)."
    }
    Set-Location $AlfredWindowsRoot
}

if (-not (Test-Path "node_modules/.bin/tauri.cmd")) {
    Write-Host "Preparing the Alfred development environment..."
    npm install
}

# UTM/Parallels shared folders can expose source files normally while failing
# large .NET build artifacts and executable I/O with Win32 error 223
# (ERROR_FILE_TOO_LARGE). Build and run the automation host from the Windows
# system drive so its stdio transport is backed by a normal local filesystem.
$AlfredHostSource = Join-Path $env:LOCALAPPDATA "Alfred\dev-host"
New-Item -ItemType Directory -Force -Path $AlfredHostSource | Out-Null
Copy-Item "native/windows-host/Program.cs" $AlfredHostSource -Force
Copy-Item "native/windows-host/Alfred.WindowsHost.csproj" $AlfredHostSource -Force
$AlfredHostProject = Join-Path $AlfredHostSource "Alfred.WindowsHost.csproj"
dotnet build $AlfredHostProject -c Debug
$AlfredHostExecutable = Join-Path $AlfredHostSource "bin\Debug\net10.0-windows\win-x64\alfred-windows-host.exe"
if (-not (Test-Path $AlfredHostExecutable)) {
    throw "The Alfred Windows automation host build completed without producing $AlfredHostExecutable."
}
$env:ALFRED_WINDOWS_HOST_PATH = (Resolve-Path $AlfredHostExecutable).Path
$RustHost = (rustc -vV | Where-Object { $_ -like "host: *" } | Select-Object -First 1) -replace '^host:\s*', ''
if (-not $RustHost) {
    throw "Could not determine the active Rust host target."
}
$SidecarDirectory = "src-tauri/binaries"
New-Item -ItemType Directory -Force -Path $SidecarDirectory | Out-Null
Copy-Item $AlfredHostExecutable (Join-Path $SidecarDirectory "alfred-windows-host-$RustHost.exe") -Force
npm run alfred
