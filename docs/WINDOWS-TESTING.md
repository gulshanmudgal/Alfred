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
- Phase 2 state conditions: record a step with a `waitFor` label and one with `expect`; verify the run waits, verifies, retries, and that an already-satisfied `expect` skips the action on resume (idempotent replay).
- Cross-app data flow: capture a value with `saveAs` in one app and substitute `${name}` into another app's step.
- Mid-run approval: trigger an action in an un-granted app; verify the run parks in "waiting", the cockpit prompt approves (durable grant) or denies (clean stop), and an unattended scheduled run exits non-zero instead of waiting forever.
- Goal runs (agent loop): run a two-app goal per provider CLI; verify observation→plan→action cycling, the step-limit and consecutive-failure breakers, check-in pauses, and that a prompt-injection attempt (a page telling the planner to type or delete) cannot bypass grants or the Delete-key block.
- Planner vision: enable "Share screenshots with the planner"; verify Codex (`--image`) and Copilot (`--attachment`) receive captures, Grok/Cursor receive prompt-listed paths and read them, the per-run folder prunes to twelve files, and retention cleanup honors the setting.
- Attempt delete, trash, purge, overwrite, password-field typing, disguised delete labels, and destructive payload text through every executor. Every attempt must fail.
- Install, upgrade, repair, and uninstall both MSI and NSIS builds. Verify code signatures and SmartScreen behavior.

The GitHub Actions workflow builds both Windows installers. A signed release still requires publisher certificates and a Windows VM run of this matrix.
