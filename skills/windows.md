# Windows skill

Alfred controls any installed Windows application through UI Automation marks and a mark-targeted virtual mouse and keyboard. The planner never receives raw pointer or keyboard APIs.

This skill is injected into every goal-run planner prompt.

## Methods

| Method | Purpose | Payload |
| --- | --- | --- |
| `listInstalledApplications` | Search installed Start / AppsFolder names | `{ "query": "store" }` |
| `listApplications` | List running windows | application `Alfred` |
| `launchApplication` | Open or focus an installed app | exact installed name |
| `wait` | Poll until text is visible | `{ "text": "Search", "timeoutMs": 8000 }` |
| `observeWindow` | Mark catalog + visible text | `{}` |
| `findElement` | Search the tree and mint marks | `{ "text": "Search" }` |
| `invokeElement` / `setValue` | Semantic invoke or value | `{ "mark": "n12" }` |
| `click` / `typeText` / `hover` / `drag` | Human-like mouse and keyboard on a mark | `{ "mark": "n12" }` |
| `probe` | Mint a mark from a screenshot point | `{ "nx": 0.42, "ny": 0.61 }` |

## Playbook

1. If the installed name is uncertain, `listInstalledApplications {"query":"..."}` and use one exact returned name.
2. `launchApplication` that name. Do not invent `.exe` paths or command lines.
3. `wait` for a label you expect, then `observeWindow` / `findElement`.
4. Prefer `invokeElement` / `setValue`. If the control ignores patterns, `click` / `typeText` move the real cursor and keyboard onto that mark.
5. UWP apps (Store, Settings, Calculator) use their Start name, never `ApplicationFrameHost`.
6. One small action per reply. After a failure, change approach instead of repeating.

## What this is not

- Not PowerShell or arbitrary process launch
- Not a Delete-key or uninstall channel
- Not raw screen-coordinate clicking
