# Alfred architecture

## Trust boundary

Provider CLIs are planners. They never receive direct access to keyboard, pointer, accessibility, screen-capture, browser Native Messaging, or filesystem mutation APIs.

Every proposed action follows this path:

1. A provider or saved workflow proposes a semantic `ActionRequest`.
2. Alfred Core validates the message against the versioned schema.
3. The policy engine returns `allow`, `request_user`, or `hard_deny`.
4. For an allowed action, Core issues a per-process execution capability.
5. The native host verifies the capability and performs exactly that action.
6. The native host returns an observation and evidence to Core.
7. Core checkpoints the result and emits a user-facing event.

Deletion and persistent-data-loss decisions are `hard_deny` and cannot be changed through settings.

## Agent loop (goal runs)

A goal run closes the observe → plan → act loop with the provider inside the trust boundary:

1. Core observes every target application (UIA control summaries; DOM element refs for the pinned browser tab).
2. Core sends the goal, the observation bundle, and the capped action history to a fresh, sandboxed provider process (one process per turn; state lives in Core, not the CLI; stop kills the turn via `kill_on_drop`).
3. The provider replies with exactly one JSON action or `done`. Core parses tolerantly (whole output, trailing JSON lines, widest brace span).
4. The action is executed through the same policy gate as recorded steps. `request_user` parks the run in a `waiting` checkpoint until the user approves (durable grant + one-step override) or stops; `hard_deny` is absolute.
5. Outcomes append to the history and the loop repeats.

Guardrails: a machine-wide run lock, a per-run step limit, a consecutive-failure breaker, an optional human check-in cadence (the run pauses for review), and fail-closed exit for unattended scheduled runs that hit a `waiting` state.

### Prompt-injection posture

Observations contain untrusted content (web page text, window titles), so planner output is treated as adversarial by construction: the declared `effect` is never trusted — Core derives a floor from the method (mutating methods can never run as `observe`, which would skip the permission grant), destructive language and the Delete key are hard-denied regardless of phrasing, unknown effects park for human approval, and every action is scoped to the user-granted applications. The planner can therefore never authorize anything on its own; it can only propose.

### Visual grounding (opt-in)

Text observations miss canvas-rendered and image-heavy content. When the user enables **Share screenshots with the planner**, each turn also captures one image per target application (`PrintWindow` for native windows, visible-tab capture for the browser) into a per-run folder under the app-data directory. The models behind every supported CLI are multimodal; only the delivery pipe differs per CLI, and each pipe is verified before Alfred uses it:

- **Codex**: attached with `-i/--image <FILE>...`.
- **Copilot**: attached with `--attachment <path>` (valid in the non-interactive `-p` mode Alfred uses).
- **Grok / Cursor**: no image flag exists, but their built-in file-reading tools hand image files to the multimodal model — verified live against the Grok CLI (a single `read_file` on a screenshot returned full visual understanding, including embedded text). Alfred lists the screenshot paths in the prompt for these providers.

Captures double as cockpit evidence. Because images leave the device to the provider's API, the setting defaults to off. Retention follows the existing screenshot policy (`all` / `failures` / `none`), the folder prunes to the newest dozen files during a run, and stale folders are swept at startup.

## Processes

### Desktop shell

Tauri hosts the React interface. It owns window lifecycle, native dialogs, notifications, and the user-visible activity stream. It does not call OS automation APIs directly.

### Alfred Core

The Rust core owns workflow state, policy, local persistence, provider supervision, capability issuance, and event ordering. This is the trusted computing base.

### Provider adapters

Core detects and supervises Codex, GitHub Copilot, Cursor, and Grok CLIs. It starts non-interactive planning sessions, streams stdout/stderr separately, supports cancellation, uses existing CLI sign-in or credentials retrieved from the OS vault, and never gives a provider the native capability token.

### Native automation hosts

The Windows .NET host implements application enumeration, UI Automation tree observation and semantic invocation, window screenshot capture, and narrowly-scoped `SendInput` click/type/key operations. It communicates over capability-authenticated JSON Lines and repeats the destructive-language block in-process. A Swift AXUIElement/ScreenCaptureKit host remains the macOS platform milestone; the shared shell and Core already build on macOS.

### Browser bridge

The Chromium Manifest V3 extension provides DOM-backed observation, navigation, screenshot capture, click, and type through an installed Native Messaging host. Webpages cannot address the host. Element references expire after page changes; password fields and destructive actions are blocked in the extension.

## Persistence

Workflow definitions live in the user-selected library as YAML files. Secrets, provider authentication, schedules, screenshots, and machine-specific run state are never embedded in shareable workflow files.

## Windows release gate

1. Run the GitHub Actions Windows job and retain MSI/NSIS artifacts.
2. Execute the packaged-app smoke suite against Calculator, Notepad, Edge, and Excel on a clean Windows 11 VM.
3. Validate UI Automation behavior at 100%, 125%, and 150% display scaling and with two monitors.
4. Sign the app, sidecar, MSI, and NSIS installer; verify SmartScreen reputation and upgrade behavior.
5. Add the Swift macOS native host before claiming native macOS execution parity.
