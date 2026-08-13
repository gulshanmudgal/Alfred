# Windows automation host

This is a self-contained C#/.NET executable using Windows UI Automation, GDI screen capture, and `SendInput`. It accepts newline-delimited JSON only when `capabilityToken` matches the per-launch `ALFRED_CAPABILITY_TOKEN`. Destructive language is rejected again in this process as defense in depth. It never exposes PowerShell, a shell, or arbitrary process launch.

Methods: `health`, `listApplications`, `listInstalledApplications`, `resolveApplication`, `launchApplication`, `focusApplication`, `navigateApplication`, `activate`, `observeWindow`, `captureWindow`, `findElement`, `getValue`, `invokeElement`, `setValue`, `click`, `typeText`, `key`, `shortcut`, `probe`, `scroll`, `rightClick`, `doubleClick`, `hover`, and `drag`.

`observeWindow` returns a per-process mark catalog (`n1`…`n24`) plus a generation, not a raw UIA dump. Query marks from `findElement`/`probe` survive the next observe of that window. `captureWindow` annotates the current catalog without reminting. Actions resolve `{"mark":"n12"}` as `(processId, id)`. After resolution the live UIA name is re-checked for persistent data loss. `probe` (`nx`,`ny` in window bitmap space 0–1) mints a mark from a visual hit or reports `visualOnly`. Pixel `x,y` remains only for recorded YAML. Live browser `click`/`rightClick`/`doubleClick`/`hover` with `nx`/`ny` still refuse when no matching page control sits under the point.

Targeting rules (phase 1 hardening):

- `resolveApplication` maps a planner-selected application name to the live process that owns its window; Alfred Core re-resolves every action so stale or reused PIDs cannot redirect input.
- `launchApplication` focuses an already-running match instead of launching a duplicate. Known Windows/browser aliases use fixed executable names; every other app must exactly match an installed Start-menu shortcut. Executable paths and command lines are never accepted from the planner.
- `navigateApplication` performs an atomic Ctrl+L/type/Enter navigation and accepts only an allow-listed Edge/Chrome/Brave target plus an absolute HTTP(S) URL.
- `click`, `typeText`, and `key` accept an optional `processId`. When present, the host brings that process's window to the foreground (restoring it if minimized) and verifies foreground ownership before sending input. `click` resolves a matching visible target label when possible, rejects disabled controls, and verifies the final point is inside the target window. `typeText` focuses an explicit selector or the best matching visible editable control, refuses accidental browser-address-bar input, and succeeds only when the entered text can be read back from that target. Without a `processId`, input falls back to application-name resolution.
- `key` only sends an allow-listed set (Backspace, Tab, Enter, Escape, Space, PageUp/Down, End, Home, arrows, F1-F12). The Delete key and all unlisted virtual-key codes are refused, closing the raw-key bypass around the semantic destructive-action filter.
- `shortcut` exposes only `CTRL+L` (browser/Explorer address bar) and `CTRL+S` (Save/Save As). Arbitrary modifier combinations are refused.
- `invokeElement`/`setValue` locate elements by `automationId` with a `name` + `controlType` fallback. `setValue` uses the UIA `ValuePattern`, which does not depend on keyboard focus.
- `captureWindow` uses `PrintWindow` so occluding windows from other apps cannot leak into evidence, and fails clearly for minimized windows.
- `findElement` reports presence (`found: true/false`) without throwing so Alfred Core can poll preconditions and postconditions; `getValue` reads an element's value for cross-application data flow (`saveAs` step variables).

Build on Windows with `dotnet publish -c Release -r win-x64`. Alfred Core must generate a cryptographically random capability token, place it only in the child process environment, and attach it to every request.
