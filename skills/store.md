# Microsoft Store skill

Use this when the goal is to open Microsoft Store, search the catalog, or inspect an app listing. Installing is an `external_write` commit and must be verified on the Store page. Uninstall is always blocked.

## Playbook

1. `listInstalledApplications {"query":"microsoft store"}` if the exact name is uncertain, then `launchApplication` with that exact name (`Microsoft Store`).
2. `wait {"text":"Search","timeoutMs":12000}` until the Store shell is up.
3. `findElement {"text":"Search"}` then `typeText` the product name into that mark. If the first Search mark is a button, `typeText` clicks it and types into the box that appears. Do not type into the taskbar or a browser.
4. `findElement` the matching tile or the app title, `click` / `invokeElement`, then re-observe.
5. A Get / Install / Open button is a commit. After clicking it, re-observe and report only the visible Store state (`Get`, `Installing`, `Open`, `Owned`).
6. Never uninstall, never claim an install finished unless the page shows `Installed` or `Open`.
7. Store is WinUI: if Invoke/Value is ignored, use the virtual mouse and keyboard on the mark.

## What this is not

- Not `ms-windows-store:` from the planner (the host maps the installed name itself)
- Not a package-manager or winget session
- Not a license or payment flow — stop and ask the user if Store asks to sign in or pay
