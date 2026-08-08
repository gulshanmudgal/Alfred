# Windows release verification

Run these gates on a clean Windows 11 VM before calling a build production-ready.

## Automated host smoke test

```powershell
./scripts/windows/test-e2e.ps1
```

The script starts the capability-authenticated host, verifies the deletion guard, opens Notepad, observes its UI Automation tree, captures the window, clicks the editor, and types non-destructive test text. It deliberately leaves the text unsaved so the test itself does not delete or overwrite persistent data.

Use `-SkipDesktop` on a non-interactive CI runner to test only the handshake and safety boundary.

## Packaged-app matrix

- Clean Windows 11 x64 VM with 100%, 125%, and 150% display scaling.
- Single and dual monitor; primary monitor placed left and right.
- Edge, Chrome, and Brave with the unpacked extension and generated Native Messaging manifest.
- Notepad and Calculator semantic UI Automation; Excel and Outlook with explicit reversible-action grants.
- Codex, Copilot, Cursor, and Grok CLIs individually: detection, existing sign-in, OS-vault token, streaming, cancel, and failed-process recovery.
- Pause, take over, stop, crash/relaunch, retry from checkpoint, in-app schedule, and Windows Task Scheduler unattended launch.
- Attempt delete, trash, purge, overwrite, password-field typing, disguised delete labels, and destructive payload text through every executor. Every attempt must fail.
- Install, upgrade, repair, and uninstall both MSI and NSIS builds. Verify code signatures and SmartScreen behavior.

The GitHub Actions workflow builds both Windows installers. A signed release still requires publisher certificates and a Windows VM run of this matrix.
