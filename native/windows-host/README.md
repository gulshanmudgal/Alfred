# Windows automation host

This is a self-contained C#/.NET executable using Windows UI Automation, GDI screen capture, and `SendInput`. It accepts newline-delimited JSON only when `capabilityToken` matches the per-launch `ALFRED_CAPABILITY_TOKEN`. Destructive language is rejected again in this process as defense in depth. It never exposes PowerShell, a shell, or arbitrary process launch.

Methods: `health`, `listApplications`, `observeWindow`, `invokeElement`, `captureWindow`, `click`, `typeText`, and `key`.

Build on Windows with `dotnet publish -c Release -r win-x64`. Alfred Core must generate a cryptographically random capability token, place it only in the child process environment, and attach it to every request.
