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
