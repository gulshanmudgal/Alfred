# Chromium browser bridge

This unpacked Manifest V3 extension provides semantic DOM observation and actions for Chrome, Edge, Brave, and other Chromium browsers. It talks only to the `com.alfred.browser_bridge` Native Messaging host; webpages cannot access the bridge. Install this folder through the browser's **Load unpacked** control, then install the generated native-host manifest for that browser.

## Commands

| Method | Role |
| --- | --- |
| `status` | Extension health |
| `listTabs` | Open tabs |
| `navigate` | Load a URL |
| `captureVisible` | Visible-tab PNG |
| `observe` | Interactive elements + refs (shadow-DOM piercing); login/CAPTCHA signals |
| `read` | Structured page text (headings, tables, grids, main/article); chunked ~6000 chars with `offset` |
| `scroll` | Viewport page or scroll-to-text |
| `find` | Playwright-style getByText → refs |
| `wait` | Poll until text appears (SPA load) |
| `click` / `type` / `getText` | Act on a ref from observe/find |

Commands accept an optional `tabId`. Alfred Core pins each run to the tab it last used so a changed active tab cannot redirect actions mid-run. Element references expire when the page changes; content-script failures surface as real command failures. The extension independently blocks destructive language and typing into password fields.

## Analysis tasks

`observe` alone is not enough to analyse a dashboard. Goal runs automatically pair **observe + read** into the planner's desktop state, and the planner is given a browser skill (see `skills/browser.md`) that mirrors a short Playwright-style playbook: navigate → wait → read → scroll/find/click → summarize only grounded text.
