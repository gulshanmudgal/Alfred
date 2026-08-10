param([switch]$Update)

$ErrorActionPreference = "Stop"
$Repository = "gulshanmudgal/Alfred"
$InstallRoot = Join-Path $env:LOCALAPPDATA "Alfred\versions"
$CurrentPointer = Join-Path $env:LOCALAPPDATA "Alfred\current.txt"

function Start-Alfred([string]$Executable) {
    if (-not (Test-Path $Executable)) { return $false }
    Start-Process -FilePath $Executable
    return $true
}

if (-not $Update -and (Test-Path $CurrentPointer)) {
    $CurrentExecutable = (Get-Content $CurrentPointer -Raw).Trim()
    if (Start-Alfred $CurrentExecutable) { exit 0 }
}

Write-Host "Finding the latest Alfred Windows release..."
$Headers = @{ "User-Agent" = "Alfred-Windows-Launcher"; "Accept" = "application/vnd.github+json" }
$Release = Invoke-RestMethod -Headers $Headers -Uri "https://api.github.com/repos/$Repository/releases/latest"
$AssetName = "Alfred-Windows-x64-portable.zip"
$Asset = $Release.assets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
if (-not $Asset) {
    throw "The latest Alfred release does not contain $AssetName. Ask the project owner to publish a Windows portable build."
}

$Version = $Release.tag_name -replace '[^0-9A-Za-z._-]', '-'
$VersionRoot = Join-Path $InstallRoot $Version
$Executable = Join-Path $VersionRoot "Alfred.exe"
if (-not (Test-Path $Executable)) {
    New-Item -ItemType Directory -Force -Path $VersionRoot | Out-Null
    $Archive = Join-Path $env:TEMP "Alfred-$Version-x64.zip"
    Write-Host "Downloading Alfred $($Release.tag_name)..."
    Invoke-WebRequest -Headers $Headers -Uri $Asset.browser_download_url -OutFile $Archive

    $ChecksumAsset = $Release.assets | Where-Object { $_.name -eq "$AssetName.sha256" } | Select-Object -First 1
    if ($ChecksumAsset) {
        $ChecksumFile = "$Archive.sha256"
        Invoke-WebRequest -Headers $Headers -Uri $ChecksumAsset.browser_download_url -OutFile $ChecksumFile
        $Expected = ((Get-Content $ChecksumFile -Raw).Trim() -split '\s+')[0]
        $Actual = (Get-FileHash -Algorithm SHA256 $Archive).Hash
        if ($Expected -ne $Actual) { throw "The Alfred download failed its SHA-256 verification." }
    }

    Expand-Archive -Path $Archive -DestinationPath $VersionRoot -Force
}

if (-not (Test-Path $Executable)) {
    throw "The portable release is invalid because Alfred.exe is missing."
}
New-Item -ItemType Directory -Force -Path (Split-Path $CurrentPointer -Parent) | Out-Null
Set-Content -Path $CurrentPointer -Value $Executable -Encoding UTF8
Write-Host "Launching Alfred $($Release.tag_name). No developer toolchain is required."
Start-Alfred $Executable | Out-Null
