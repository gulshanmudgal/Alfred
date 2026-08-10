param(
  [Parameter(Mandatory=$true)][string]$Executable
)

$ErrorActionPreference = "Stop"
$resolved = (Resolve-Path $Executable).Path
$bytes = [IO.File]::ReadAllBytes($resolved)
if ($bytes.Length -lt 256) { throw "$resolved is not a valid Windows executable." }

$peOffset = [BitConverter]::ToInt32($bytes, 0x3c)
$optionalHeader = $peOffset + 24
$subsystem = [BitConverter]::ToUInt16($bytes, $optionalHeader + 68)
if ($subsystem -ne 2) {
  throw "Alfred.exe uses PE subsystem $subsystem instead of Windows GUI subsystem 2. Closing its console would terminate the app."
}

Write-Host "PASS Alfred.exe uses the Windows GUI subsystem and is terminal-independent"
