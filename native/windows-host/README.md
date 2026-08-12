# Windows automation host

This is a self-contained C#/.NET executable using Windows UI Automation, GDI screen capture, and `SendInput`. It accepts newline-delimited JSON only when `capabilityToken` matches the per-launch `ALFRED_CAPABILITY_TOKEN`. Destructive language is rejected again in this process as defense in depth. It never exposes PowerShell, a shell, or arbitrary process launch.

Methods: `health`, `listApplications`, `listInstalledApplications`, `resolveApplication`, `launchApplication`, `focusApplication`, `activate`, `observeWindow`, `captureWindow`, `findElement`, `getValue`, `invokeElement`, `setValue`, `click`, `typeText`, `key`, and `shortcut`.

Targeting rules (phase 1 hardening):

- `resolveApplication` maps a planner-selected application name to the live process that owns its window; Alfred Core re-resolves every action so stale or reused PIDs cannot redirect input.
- `launchApplication` focuses an already-running match instead of launching a duplicate. Known Windows/browser aliases use fixed executable names; every other app must exactly match an installed Start-menu shortcut. Executable paths and command lines are never accepted from the planner.
- `click`, `typeText`, and `key` accept an optional `processId`. When present, the host brings that process's window to the foreground (restoring it if minimized) and verifies foreground ownership before sending input; `click` also verifies the point lies inside the target window bounds. Without a `processId`, input falls back to legacy focus-based behavior.
- `key` only sends an allow-listed set (Backspace, Tab, Enter, Escape, Space, PageUp/Down, End, Home, arrows, F1-F12). The Delete key and all unlisted virtual-key codes are refused, closing the raw-key bypass around the semantic destructive-action filter.
- `shortcut` exposes only `CTRL+L` (browser/Explorer address bar) and `CTRL+S` (Save/Save As). Arbitrary modifier combinations are refused.
- `invokeElement`/`setValue` locate elements by `automationId` with a `name` + `controlType` fallback. `setValue` uses the UIA `ValuePattern`, which does not depend on keyboard focus.
- `captureWindow` uses `PrintWindow` so occluding windows from other apps cannot leak into evidence, and fails clearly for minimized windows.
- `findElement` reports presence (`found: true/false`) without throwing so Alfred Core can poll preconditions and postconditions; `getValue` reads an element's value for cross-application data flow (`saveAs` step variables).

Build on Windows with `dotnet publish -c Release -r win-x64`. Alfred Core must generate a cryptographically random capability token, place it only in the child process environment, and attach it to every request.
