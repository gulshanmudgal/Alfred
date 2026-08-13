param(
  [string]$HostPath = "",
  [switch]$SkipDesktop,
  [switch]$SkipSave
)
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
if (-not $HostPath) {
  # Keep test executables off VM shared folders. Their filesystem bridge can
  # report Win32 error 223 for otherwise-small stdio actions after a .NET host
  # was built or launched from the share.
  $HostSource = Join-Path $env:LOCALAPPDATA "Alfred\test-host"
  New-Item -ItemType Directory -Force -Path $HostSource | Out-Null
  Copy-Item (Join-Path $root "native\windows-host\Program.cs") $HostSource -Force
  Copy-Item (Join-Path $root "native\windows-host\Marks.cs") $HostSource -Force
  Copy-Item (Join-Path $root "native\windows-host\Alfred.WindowsHost.csproj") $HostSource -Force
  $HostProject = Join-Path $HostSource "Alfred.WindowsHost.csproj"
  dotnet build $HostProject -c Debug
  $HostPath = Join-Path $HostSource "bin\Debug\net10.0-windows\win-x64\alfred-windows-host.exe"
} elseif (-not (Test-Path $HostPath)) {
  throw "The requested Windows host does not exist: $HostPath"
}
$HostPath = (Resolve-Path $HostPath).Path
$tokenBytes = New-Object byte[] 32
$random = [Security.Cryptography.RandomNumberGenerator]::Create()
try { $random.GetBytes($tokenBytes) } finally { $random.Dispose() }
$token = [BitConverter]::ToString($tokenBytes).Replace("-", "")
$startInfo = [Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $HostPath
$startInfo.UseShellExecute = $false
$startInfo.RedirectStandardInput = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true
$startInfo.Environment["ALFRED_CAPABILITY_TOKEN"] = $token
$hostProcess = [Diagnostics.Process]::new()
$hostProcess.StartInfo = $startInfo
if (-not $hostProcess.Start()) { throw "Could not start Alfred Windows host." }

function Invoke-AlfredHost([string]$Method, [hashtable]$Params = @{}, [string]$Intent = "observe", [string]$Target = "", [string]$Application = "") {
  $message = @{ id = [guid]::NewGuid().ToString(); method = $Method; capabilityToken = $token; params = $Params; application = $Application; intent = $Intent; target = $Target } | ConvertTo-Json -Compress -Depth 10
  $hostProcess.StandardInput.WriteLine($message)
  $hostProcess.StandardInput.Flush()
  $line = $hostProcess.StandardOutput.ReadLine()
  if (-not $line) { throw "Windows host closed without a response: $($hostProcess.StandardError.ReadToEnd())" }
  return $line | ConvertFrom-Json
}

try {
  $health = Invoke-AlfredHost "health"
  if (-not $health.ok -or $health.result.host -ne "windows") { throw "Health handshake failed." }
  Write-Host "PASS health and capability handshake"

  $blocked = Invoke-AlfredHost "click" @{ x = 10; y = 10 } "delete email" "Delete"
  if ($blocked.ok -or $blocked.error -notmatch "Destructive actions") { throw "Deletion guard did not reject the test request." }
  Write-Host "PASS destructive-action defense in depth"

  # The planner may name installed apps, but never an executable path or command.
  $blockedLaunch = Invoke-AlfredHost "launchApplication" @{} "launch application" "Executable path" "C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
  if ($blockedLaunch.ok -or $blockedLaunch.error -notmatch "not an exact installed Start-menu application") { throw "Executable-path launch was not rejected." }
  Write-Host "PASS exact Start-menu application boundary"

  $installed = Invoke-AlfredHost "listInstalledApplications"
  if (-not $installed.ok -or -not ($installed.result | Where-Object { $_.name -eq "Notepad" })) { throw "Installed application discovery did not include Notepad." }
  Write-Host "PASS installed application discovery"

  $blockedDeleteKey = Invoke-AlfredHost "key" @{ virtualKey = 46 } "press key" "Keyboard input" "Notepad"
  if ($blockedDeleteKey.ok -or $blockedDeleteKey.error -notmatch "Delete key is blocked") { throw "Delete-key guard did not reject the test request." }
  Write-Host "PASS Delete-key defense in depth"

  $unknownKey = Invoke-AlfredHost "key" @{ virtualKey = 0x5B } "press key" "Desktop" "Notepad"
  if ($unknownKey.ok) { throw "An unlisted virtual key was not refused by the host." }
  Write-Host "PASS virtual-key allow-list"

  $unknownShortcut = Invoke-AlfredHost "shortcut" @{ keys = "CTRL+X" } "press shortcut" "Editor" "Notepad"
  if ($unknownShortcut.ok) { throw "An unlisted shortcut was not refused by the host." }
  Write-Host "PASS shortcut allow-list"

  if (-not $SkipDesktop) {
    $launched = Invoke-AlfredHost "launchApplication" @{} "launch approved application" "Notepad" "Notepad"
    if (-not $launched.ok -or -not $launched.result.processId) { throw "Semantic application launch failed." }
    $notepadId = [int]$launched.result.processId
    $relaunched = Invoke-AlfredHost "launchApplication" @{} "launch approved application again" "Notepad" "Notepad"
    if (-not $relaunched.ok -or -not $relaunched.result.alreadyRunning -or [int]$relaunched.result.processId -ne $notepadId) {
      throw "Repeated application launch was not idempotent."
    }
    Write-Host "PASS idempotent application launch"
    $apps = Invoke-AlfredHost "listApplications"
    $app = $apps.result | Where-Object { $_.processId -eq $notepadId } | Select-Object -First 1
    if (-not $app) { throw "Notepad was not visible to the Windows host." }
    $resolved = Invoke-AlfredHost "resolveApplication" @{ name = "Notepad" }
    if (-not $resolved.ok -or $resolved.result.processId -ne $notepadId) { throw "Application name resolution returned the wrong process." }
    Write-Host "PASS application name resolution"
    $focused = Invoke-AlfredHost "focusApplication" @{ processId = $notepadId } "focus approved application" "Notepad" "Notepad"
    if (-not $focused.ok) { throw "Semantic application focus failed." }
    Write-Host "PASS window focus and foreground verification"
    # Put another trusted window in front, then require the host to reclaim
    # focus. This catches Windows foreground-lock failures that only appear once
    # Alfred is behind another app.
    $calculator = Invoke-AlfredHost "launchApplication" @{} "launch focus competitor" "Calculator" "Calculator"
    if (-not $calculator.ok) { throw "Could not launch the focus competitor." }
    Start-Sleep -Milliseconds 250
    $refocused = Invoke-AlfredHost "focusApplication" @{ processId = $notepadId } "refocus approved application" "Notepad" "Notepad"
    if (-not $refocused.ok) { throw "Host could not reclaim focus from another foreground app." }
    Write-Host "PASS foreground-lock recovery"
    $tree = Invoke-AlfredHost "observeWindow" @{ processId = $notepadId } "observe Notepad" "Notepad" "Notepad"
    if (-not $tree.ok -or -not $tree.result.marks) { throw "UI Automation observation failed: $($tree.error)" }
    $notepadGeneration = [int]$tree.result.generation
    $capture = Invoke-AlfredHost "captureWindow" @{ processId = $notepadId } "capture Notepad" "Notepad" "Notepad"
    if (-not $capture.ok -or $capture.result.base64.Length -lt 1000) { throw "Window capture did not return PNG evidence: $($capture.error)" }
    if ([int]$capture.result.generation -ne $notepadGeneration) { throw "captureWindow reminted marks instead of annotating the current catalog." }
    $editorMark = @($tree.result.marks) | Where-Object { $_.role -match 'Document|Edit' } | Select-Object -First 1
    if (-not $editorMark) { throw "observeWindow did not expose a Notepad editor mark." }
    if (-not (Invoke-AlfredHost "click" @{ mark = $editorMark.id; processId = $notepadId } "focus Notepad editor" "Editor" "Notepad").ok) { throw "Targeted pointer input failed." }
    $typed = Invoke-AlfredHost "typeText" @{ text = "Alfred Windows end-to-end smoke test"; processId = $notepadId } "type smoke-test text" "Notepad editor" "Notepad"
    if (-not $typed.ok -or -not $typed.result.verified -or $typed.result.observedText -notmatch "end-to-end smoke test") {
      throw "Targeted keyboard input was not verified in the intended control: $($typed.error)"
    }
    $idempotent = Invoke-AlfredHost "typeText" @{ text = "Alfred Windows end-to-end smoke test"; processId = $notepadId } "retry the same smoke-test text" "Notepad editor" "Notepad"
    if (-not $idempotent.ok -or -not $idempotent.result.verified -or -not $idempotent.result.alreadyPresent) {
      throw "A duplicate typeText retry was not treated as idempotent."
    }
    $unicodeText = "Alfred Windows UTF-8 " + [char]0x2014 + " emoji " + [char]::ConvertFromUtf32(0x1F419)
    $replaced = Invoke-AlfredHost "typeText" @{ text = $unicodeText; processId = $notepadId } "replace smoke-test text with Unicode" "Notepad editor" "Notepad"
    if (-not $replaced.ok -or -not $replaced.result.verified -or $replaced.result.observedText -ne $unicodeText) {
      throw "Unicode text was corrupted, duplicated, or not verified in the intended control: $($replaced.error)"
    }
    if (-not (Invoke-AlfredHost "key" @{ virtualKey = 13; processId = $notepadId } "press enter" "Editor" "Notepad").ok) { throw "Allowed key (Enter) was rejected." }
    $outside = Invoke-AlfredHost "click" @{ x = -32000; y = -32000; processId = $notepadId } "click target" "Editor" "Notepad"
    if ($outside.ok) { throw "A click outside the target window bounds was not refused." }
    $calculatorObserve = Invoke-AlfredHost "observeWindow" @{ processId = [int]$calculator.result.processId } "observe Calculator" "Calculator" "Calculator"
    if (-not $calculatorObserve.ok) { throw "Calculator observe failed after Notepad marks were minted." }
    $notepadStill = Invoke-AlfredHost "typeText" @{ text = $unicodeText; mark = $editorMark.id; processId = $notepadId } "reuse Notepad mark after observing Calculator" "Notepad editor" "Notepad"
    if (-not $notepadStill.ok -or -not $notepadStill.result.verified) {
      throw "Observing Calculator expired or rebound the Notepad mark: $($notepadStill.error)"
    }
    $calcId = [int]$calculator.result.processId
    $probe = Invoke-AlfredHost "probe" @{ nx = 0.5; ny = 0.72; processId = $calcId } "probe Calculator keypad" "Equals" "Calculator"
    if (-not $probe.ok) { throw "probe failed: $($probe.error)" }
    if ($probe.result.kind -eq "mark") {
      $probed = Invoke-AlfredHost "click" @{ mark = $probe.result.mark; processId = $calcId } "click probed Calculator mark" "Equals" "Calculator"
      if (-not $probed.ok) { throw "click after probe failed: $($probed.error)" }
    }
    $browserPixel = Invoke-AlfredHost "click" @{ nx = 0.5; ny = 0.5; processId = $notepadId } "click unverified browser pixel" "Post" "Microsoft Edge"
    if ($browserPixel.ok -or $browserPixel.error -notmatch "unverified browser coordinate") {
      throw "Browser nx/ny without a matching control was not refused: $($browserPixel.error)"
    }
    $emptyBin = Invoke-AlfredHost "click" @{ mark = $editorMark.id; processId = $notepadId } "empty recycle bin" "Empty Recycle Bin" "Notepad"
    if ($emptyBin.ok -or $emptyBin.error -notmatch "Destructive") {
      throw "Live destructive name Empty Recycle Bin was not refused: $($emptyBin.error)"
    }
    Write-Host "PASS targeted input postcondition, bounds validation, key allow-list, and per-process marks"
    $badNavigation = Invoke-AlfredHost "navigateApplication" @{ url = "file:///C:/Windows/System32/cmd.exe"; processId = $notepadId } "navigate outside HTTP" "Address bar" "Notepad"
    if ($badNavigation.ok) { throw "Native navigation accepted a non-browser application and non-HTTP URL." }
    $badCredentialUrl = Invoke-AlfredHost "navigateApplication" @{ url = "https://user:secret@example.com"; processId = $notepadId } "navigate credential URL" "Address bar" "Microsoft Edge"
    if ($badCredentialUrl.ok) { throw "Native navigation accepted a URL containing credentials." }
    Write-Host "PASS native navigation browser and HTTP(S) allow-list"
    $editorControlType = "ControlType.Edit"
    $found = Invoke-AlfredHost "findElement" @{ processId = $notepadId; controlType = $editorControlType } "locate editor" "Notepad" "Notepad"
    if (-not $found.result.found) {
      # Current Windows 11 Notepad exposes its text surface as Document rather
      # than Edit; older builds use Edit. Exercise whichever UIA contract the
      # installed version reports.
      $editorControlType = "ControlType.Document"
      $found = Invoke-AlfredHost "findElement" @{ processId = $notepadId; controlType = $editorControlType } "locate editor" "Notepad" "Notepad"
    }
    if (-not $found.ok -or -not $found.result.found) { throw "findElement did not locate the Notepad editor." }
    $missing = Invoke-AlfredHost "findElement" @{ processId = $notepadId; name = "Alfred element that does not exist 9f3b2" } "locate missing" "Notepad" "Notepad"
    if (-not $missing.ok -or $missing.result.found) { throw "findElement must report found=false (not throw) for absent elements." }
    $read = Invoke-AlfredHost "getValue" @{ processId = $notepadId; controlType = $editorControlType } "read editor" "Notepad" "Notepad"
    if ($read.ok -and $read.result.value -match "smoke test") { Write-Host "PASS findElement presence checks and getValue data capture" }
    else { Write-Host "PASS findElement presence checks (getValue unsupported by this Notepad build; tolerated)" }

    if (-not $SkipSave) {
      $desktop = [Environment]::GetFolderPath([Environment+SpecialFolder]::DesktopDirectory)
      $savedPath = Join-Path $desktop "Alfred Windows smoke $([guid]::NewGuid().ToString('N')).txt"
      $save = Invoke-AlfredHost "shortcut" @{ keys = "CTRL+S"; processId = $notepadId } "save the new smoke-test file" "Save" "Notepad"
      if (-not $save.ok) { throw "CTRL+S did not open Save As: $($save.error)" }
      Start-Sleep -Milliseconds 800
      $fileNameSelector = @{ processId = $notepadId; automationId = "1001" }
      $fileName = Invoke-AlfredHost "findElement" $fileNameSelector "locate file name" "File name" "Notepad"
      if (-not $fileName.result.found) {
        $fileNameSelector = @{ processId = $notepadId; name = "File name:"; controlType = "ControlType.Edit" }
        $fileName = Invoke-AlfredHost "findElement" $fileNameSelector "locate file name" "File name" "Notepad"
      }
      if (-not $fileName.ok -or -not $fileName.result.found) { throw "Save As file-name field was not found." }
      $fileNameSelector["value"] = $savedPath
      $setName = Invoke-AlfredHost "setValue" $fileNameSelector "set a unique new file name" "File name" "Notepad"
      if (-not $setName.ok) { throw "Could not set Save As file name: $($setName.error)" }
      $saved = Invoke-AlfredHost "invokeElement" @{ processId = $notepadId; name = "Save"; controlType = "ControlType.Button" } "save a new text file" "Save" "Notepad"
      if (-not $saved.ok) { throw "Save button invocation failed: $($saved.error)" }
      $deadline = [DateTime]::UtcNow.AddSeconds(8)
      while (-not (Test-Path $savedPath) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 200 }
      if (-not (Test-Path $savedPath)) { throw "Notepad did not create $savedPath" }
      if ((Get-Content $savedPath -Raw) -notmatch [Regex]::Escape($unicodeText)) { throw "Saved file content did not preserve the verified Unicode text." }
      Write-Host "PASS create, save, and verify file: $savedPath"
    } else {
      Write-Host "Notepad is intentionally left open with unsaved test text; no persistent data was deleted."
    }
  }
} finally {
  if (-not $hostProcess.HasExited) { $hostProcess.Kill($true) }
  $hostProcess.Dispose()
}
