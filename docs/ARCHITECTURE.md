# Alfred architecture

## Trust boundary

Provider CLIs are planners. Alfred never gives them its keyboard, pointer, accessibility, screen-capture, browser Native Messaging, or native-host capability. Each adapter also applies the strongest planner-only restrictions exposed by that CLI.

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
2. Core atomically persists a versioned `GoalRunMemory` ledger (goal, chosen provider, provider session id, evolving app set, observations, plan, history, pending action, failures, and completion evidence) before invoking the provider.
3. Core sends that complete durable state to a supervised provider process. When the CLI exposes session continuation, Alfred resumes the exact Codex thread, Copilot session, Cursor chat, or Grok session as a second source of conversational context. Core remains authoritative if a provider session disappears.
4. The provider replies with exactly one JSON action or a completion claim. Core parses provider-specific structured envelopes tolerantly.
5. The action is executed through the policy gate. Methods Alfred understands and classifies as non-destructive run automatically; destructive actions are hard-denied; genuinely unknown effects are the only actions that can park for an exception.
6. A completion claim never closes the run. Alfred takes a fresh observation and requires a separate evidence-review response with concrete visible facts or successful results. Only then does the checkpoint become `completed`.
7. Outcomes are committed to memory and the reusable run ledger before the loop repeats.

Guardrails: a machine-wide run lock, evidence-backed action postconditions, a consecutive-failure breaker, an optional human check-in cadence (the run pauses for review), and fail-closed exit for unattended scheduled runs that hit a `waiting` state. Live goals do not have an arbitrary global step limit; they continue until verified completion, a concrete repeated failure, user stop, or an unrecoverable provider/target error.

### Prompt-injection posture

Observations contain untrusted content (web page text, window titles), so planner output is treated as adversarial by construction: the declared `effect` is never trusted — Core derives a floor from the method and commit-verb targets (`observe` / `modify_reversible` / `external_write`), persistent data-loss actions and the Delete key are hard-denied, the host/extension re-check the resolved control’s live name rather than `targetLabel`, unknown effects park for human approval, live goals act by per-window mark rather than raw screen pixels, and application launch accepts only fixed aliases or exact installed Start-menu / AppsFolder names. The planner can therefore never authorize a native action on its own; it can only propose one.

### Hybrid visual grounding

Vision-only control is feasible, but it is not the reliability target: screenshots are essential for canvas-rendered and image-heavy content, while accessibility selectors are more deterministic for ordinary controls and survive display scaling better than coordinates. Alfred therefore uses both. Each native observe produces a **mark catalog** (`n12`) and each capture is set-of-mark annotated so the planner acts by id, not screen pixels. `findElement` / `probe` mint marks when the default catalog hid a control. New installations enable screenshot sharing by default and users can disable it because images leave the machine through the selected provider.

Each turn captures one image per target application (`PrintWindow` for native windows, visible-tab capture when the optional browser bridge is present) into a per-run folder under the app-data directory. The delivery pipe differs per CLI:

- **Codex**: attached with `-i/--image <FILE>...`.
- **Copilot**: attached with `--attachment <path>` (valid in the non-interactive `-p` mode Alfred uses).
- **Grok / Cursor**: no image flag exists, but their built-in file-reading tools hand image files to the multimodal model — verified live against the Grok CLI (a single `read_file` on a screenshot returned full visual understanding, including embedded text). Alfred lists the screenshot paths in the prompt for these providers.

Captures double as cockpit evidence. Retention follows the screenshot policy (`all` / `failures` / `none`), the folder prunes to the newest dozen files during a run, and stale folders are swept at startup.

## Processes

### Desktop shell

Tauri hosts the React interface. It owns window lifecycle, native dialogs, notifications, and the user-visible activity stream. It does not call OS automation APIs directly.

### Alfred Core

The Rust core owns workflow state, policy, local persistence, provider supervision, capability issuance, and event ordering. This is the trusted computing base.

### Provider adapters

Core detects and supervises Codex, GitHub Copilot, Cursor, and Grok CLIs. It starts non-interactive planning turns, extracts the documented session/thread id from structured output, resumes that exact conversation, streams stdout/stderr separately, supports cancellation, uses existing CLI sign-in or credentials retrieved from the OS vault, and never gives a provider the native capability token. Codex runs in its read-only sandbox; Copilot is denied shell/write/URL/memory tools; Grok receives only `read_file` (needed for screenshot-path delivery) and no subagents.

### Native automation hosts

The Windows .NET host implements running/installed application enumeration (Start menu plus `shell:AppsFolder`), UI Automation mark catalogs and semantic invocation (Invoke/Value/Toggle/Scroll patterns first), set-of-mark `PrintWindow` capture, PerMonitorV2 coordinate mapping, native `wait` for slow shells, and a mark-targeted virtual mouse/keyboard (`SendInput` with a short human-like cursor path and cadenced keystrokes) when a pattern is missing, `SetValue` is ignored, or a browser composer needs a trusted OS gesture. Known inbox apps use fixed executable or protocol aliases (`ms-windows-store:`, `ms-settings:`); any other app can launch only through an installed Start-menu shortcut or AppsFolder AUMID (exact or unique high-confidence match), never an arbitrary executable path or command line. It communicates over capability-authenticated JSON Lines and repeats the destructive-language block in-process. A Swift AXUIElement/ScreenCaptureKit host remains the macOS platform milestone; the shared shell and Core already build on macOS.

### Browser bridge

The Chromium Manifest V3 extension is an optional accelerator for DOM-backed observation, navigation, screenshot capture, click, and type. It is not required for browser control: without it, Edge/Chrome/Brave are ordinary native targets observed through screenshots and UI Automation. When a page ignores untrusted DOM events (contenteditable composers, some SPAs), the extension returns a page-space box and Core retries with the host's trusted pointer/keyboard after the live label has already been safety-checked. Core computes bridge availability before every planner turn; extension-only `browser.*` methods and the `Installed browser` pseudo-target are omitted and rejected when disconnected. Native URL changes use `navigateApplication`, an atomic Ctrl+L/type/Enter operation restricted to allow-listed browsers and absolute HTTP(S) URLs. Webpages cannot address the bridge. Element references expire after page changes; password fields and destructive actions are blocked in the extension.

## Persistence

Workflow definitions live in the user-selected library as YAML files. A saved workflow is a goal definition with the brain that learned it, target-app hints, and a successful action audit trail. Running it re-enters the live planner instead of blindly replaying stale browser refs or coordinates. Secrets, provider authentication, screenshots, provider session ids, and machine-specific run state are never embedded in shareable workflow files.

Machine-specific goal memory lives under app data in `goal-runs/<run-id>.json`; successful reusable steps live in the separate run-step ledger. Both use atomic replacement.

## Windows-Use evaluation

[Windows-Use](https://github.com/Jeomon/Windows-Use) validates Alfred's accessibility-first, optional-vision direction and provides useful patterns for bounded observations, event subscribers, and persistent memory. It is MIT licensed, but Alfred does not embed it in the trusted path today: it is Python-based, exposes PowerShell/filesystem tools, and explicitly provides no sandbox or isolation. Selective adaptation remains possible after Alfred's native protocol and safety invariants have equivalent tests.

## Windows release gate

1. Run the GitHub Actions Windows job and retain MSI/NSIS artifacts.
2. Execute the packaged-app smoke suite against Calculator, Notepad, Edge, and Excel on a clean Windows 11 VM.
3. Validate UI Automation behavior at 100%, 125%, and 150% display scaling and with two monitors.
4. Sign the app, sidecar, MSI, and NSIS installer; verify SmartScreen reputation and upgrade behavior.
5. Add the Swift macOS native host before claiming native macOS execution parity.
