# Windows release verification

Run these gates on a clean Windows 11 VM before calling a build production-ready.

## Automated host smoke test

```powershell
./scripts/windows/test-e2e.ps1
```

The script starts the capability-authenticated host, verifies the deletion and input allow-lists, lists exact Start-menu applications, opens Notepad idempotently, observes its UI Automation tree, captures the window, types test text, saves it under a unique new filename on the desktop, and verifies the file content. It prints the retained path; the unique name prevents overwrite. Pass `-SkipSave` to leave the text unsaved.

Use `-SkipDesktop` on a non-interactive CI runner to test only the handshake and safety boundary.

## Packaged-app matrix

- Clean Windows 11 x64 VM with 100%, 125%, and 150% display scaling.
- Single and dual monitor; primary monitor placed left and right.
- Edge, Chrome, and Brave through native screenshot + UI Automation control; repeat with the optional extension to verify the DOM accelerator.
- Notepad and Calculator semantic UI Automation; Excel and Outlook safe-action auto execution.
- Codex, Copilot, Cursor, and Grok CLIs individually: detection, existing sign-in, OS-vault token, structured output, exact session continuation, cancellation, and failed-process recovery.
- Pause, take over, stop, crash/relaunch, retry from checkpoint, in-app schedule, and Windows Task Scheduler unattended launch.
- Automatic safety: verify every known non-destructive method runs without a user prompt, an unknown method parks once for an exception, and destructive requests remain non-overridable.
- Goal runs (agent loop): run a two-app goal per provider CLI; verify observation→plan→action cycling, durable `goal-runs/<id>.json` updates after every turn, dynamic app discovery, session-id reuse, the step/failure breakers, check-in pauses, and that prompt injection cannot bypass the Delete-key block.
- Completion gate: make the planner claim completion before the expected state is visible and verify the run stays active; then make the state visible and verify a fresh evidence review is required before `completed`.
- Hybrid vision: verify Codex (`--image`) and Copilot (`--attachment`) receive captures, Grok/Cursor receive prompt-listed paths and read them, the per-run folder prunes to twelve files, and retention cleanup honors the setting.
- Saved workflow: save the successful run, launch it again, and verify it re-enters the live goal loop instead of replaying old browser refs or coordinates.
- Attempt delete, trash, purge, overwrite, password-field typing, disguised delete labels, and destructive payload text through every executor. Every attempt must fail.
- Install, upgrade, repair, and uninstall both MSI and NSIS builds. Verify code signatures and SmartScreen behavior.

The GitHub Actions workflow builds both Windows installers. A signed release still requires publisher certificates and a Windows VM run of this matrix.
