param(
  # Rust target triple: x86_64-pc-windows-msvc (default) or aarch64-pc-windows-msvc.
  [string]$Triple = "x86_64-pc-windows-msvc"
)
$ErrorActionPreference = "Stop"
$hostRid = if ($Triple -like "aarch64*") { "win-arm64" } else { "win-x64" }
$arch = if ($Triple -like "aarch64*") { "arm64" } else { "x64" }
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $root
npm ci
dotnet publish native/windows-host/Alfred.WindowsHost.csproj -c Release -r $hostRid --self-contained true
# The sidecar ships inside the installer, so it must be signed before bundling;
# Tauri's signCommand only signs Tauri-produced binaries. No-op until a
# certificate is configured (see sign.ps1).
& (Join-Path $PSScriptRoot "sign.ps1") -FilePath "native\windows-host\bin\Release\net10.0-windows\$hostRid\publish\alfred-windows-host.exe"
$binaryDirectory = Join-Path $root "src-tauri\binaries"
New-Item -ItemType Directory -Force -Path $binaryDirectory | Out-Null
Copy-Item "native\windows-host\bin\Release\net10.0-windows\$hostRid\publish\alfred-windows-host.exe" (Join-Path $binaryDirectory "alfred-windows-host-$Triple.exe") -Force
npm run tauri build -- --target $Triple --config src-tauri/tauri.windows.conf.json
& "scripts\windows\verify-gui-subsystem.ps1" -Executable "src-tauri\target\$Triple\release\alfred.exe"

$distribution = Join-Path $root "dist\windows"
New-Item -ItemType Directory -Force -Path $distribution | Out-Null

# Copy Tauri's versioned bundles to stable, obvious artifact names. Keeping the
# installers under dist/windows also lets CI upload them at the artifact root
# instead of burying them under src-tauri/target/<triple>/release/bundle.
$bundleDirectory = Join-Path $root "src-tauri\target\$Triple\release\bundle"
$nsisBundles = @(Get-ChildItem -Path (Join-Path $bundleDirectory "nsis\*.exe") -File)
$msiBundles = @(Get-ChildItem -Path (Join-Path $bundleDirectory "msi\*.msi") -File)
if ($nsisBundles.Count -ne 1) {
  throw "Expected exactly one NSIS installer under $bundleDirectory, found $($nsisBundles.Count)."
}
if ($msiBundles.Count -ne 1) {
  throw "Expected exactly one MSI installer under $bundleDirectory, found $($msiBundles.Count)."
}

$nsisInstaller = Join-Path $distribution "Alfred-Windows-$arch-Setup.exe"
$msiInstaller = Join-Path $distribution "Alfred-Windows-$arch.msi"
Copy-Item $nsisBundles[0].FullName $nsisInstaller -Force
Copy-Item $msiBundles[0].FullName $msiInstaller -Force

$portable = Join-Path $distribution "portable-$arch"
New-Item -ItemType Directory -Force -Path $portable | Out-Null
Copy-Item "src-tauri\target\$Triple\release\alfred.exe" (Join-Path $portable "Alfred.exe") -Force
Copy-Item "native\windows-host\bin\Release\net10.0-windows\$hostRid\publish\alfred-windows-host.exe" (Join-Path $portable "alfred-windows-host.exe") -Force
New-Item -ItemType Directory -Force -Path (Join-Path $portable "browser") | Out-Null
Copy-Item "browser\chromium-extension" (Join-Path $portable "browser\chromium-extension") -Recurse -Force
New-Item -ItemType Directory -Force -Path (Join-Path $portable "scripts\windows") | Out-Null
Copy-Item "scripts\windows\install-browser-bridge.ps1" (Join-Path $portable "scripts\windows\install-browser-bridge.ps1") -Force
$archive = Join-Path $distribution "Alfred-Windows-$arch-portable.zip"
if (Test-Path $archive) { Remove-Item $archive -Force }
Compress-Archive -Path (Join-Path $portable "*") -DestinationPath $archive -CompressionLevel Optimal

foreach ($releaseFile in @($nsisInstaller, $msiInstaller, $archive)) {
  $hash = (Get-FileHash -Algorithm SHA256 $releaseFile).Hash.ToLowerInvariant()
  $fileName = Split-Path $releaseFile -Leaf
  Set-Content -Path "$releaseFile.sha256" -Value "$hash  $fileName" -Encoding ASCII
}

Write-Host "NSIS installer: $nsisInstaller"
Write-Host "MSI installer: $msiInstaller"
Write-Host "Portable release: $archive"
