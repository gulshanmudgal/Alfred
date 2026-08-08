# macOS automation host

This is the remaining native platform implementation. It will be a Swift executable using AXUIElement and ScreenCaptureKit, accepting only capability-authorized messages from Alfred Core and reporting Screen Recording and Accessibility permission status explicitly.

Initial methods: `health`, `listApplications`, `observeWindow`, `queryElements`, `invokeElement`, `captureWindow`, and `releaseControl`.

The shared Tauri shell, Rust policy/core, provider sessions, YAML workflows, credentials, and scheduler already run on macOS. Recorded native desktop actions deliberately fail closed until this host is implemented; browser actions remain portable.
