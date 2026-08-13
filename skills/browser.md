# Browser skill (Playwright-style)

Alfred does not embed Playwright. It exposes a **similar control surface** through the Chromium extension + Native Messaging bridge, with every action gated by Alfred Core policy.

This skill is injected into the goal-run planner prompt whenever the goal or target apps involve the web.

## Methods

| Method | Purpose | Payload |
| --- | --- | --- |
| `browser.navigate` | Open a URL in the pinned/active tab | `{ "url": "https://..." }` |
| `browser.wait` | Wait until text appears (SPA load) | `{ "text": "Errors", "timeoutMs": 12000 }` |
| `browser.observe` | List interactive controls + refs | `{}` |
| `browser.read` | Extract page **content** (headings, tables, grids, main text) | `{ "offset": 0 }` |
| `browser.scroll` | Page viewport or scroll text into view | `{ "direction": "down" }` or `{ "text": "RUM" }` |
| `browser.find` | Playwright-like getByText → refs | `{ "text": "Errors" }` |
| `browser.click` | Click a ref from observe/find | `{ "ref": "e3" }` |
| `browser.dblclick` | Double-click a ref | `{ "ref": "e3" }` |
| `browser.hover` | Hover a ref (menus, tooltips) | `{ "ref": "e3" }` |
| `browser.type` | Type into an input ref | `{ "ref": "e5", "text": "..." }` |
| `browser.getText` | Single-element text (≤2000 chars) | `{ "ref": "e2" }` |

## Playbook for portal analysis (e.g. Datadog RUM)

1. User is already logged in on the browser profile that has the Alfred extension.
2. Goal includes the Datadog URL (or navigate from a bookmark the user has open).
3. `browser.navigate` → `browser.wait` for a known fragment → auto **observe + read** each planner turn.
4. Use `browser.find` + `browser.click` to open RUM / Errors.
5. `browser.read` / `browser.scroll` to gather error list text.
6. Summarize **only** text present in observations or action-history digests.
7. On `loginWall` / `captcha` signals: stop and ask the user — never invent metrics.

## What this is not

- Not a headless Playwright cluster
- Not a Datadog API client (UI only)
- Canvas-only UIs (Google Docs) still need native marks / probe; DOM refs miss the canvas
- Contenteditable composers, canvas editors, and custom SPA controls (`div[role=button]`, tabindex cards) ignore untrusted DOM events; Alfred then uses a trusted OS click/type/hover on the element's page box
- Not unattended public posting
- `browser.navigate` succeeds only on the requested path or a real committed URL change — staying on the same origin is not enough

## Extension reload

After updating `browser/chromium-extension`, open `edge://extensions` (or Chrome), reload **Alfred Browser Bridge**, and keep the Native Messaging host registered.
