# Provider adapters

Provider sessions supervise one installed CLI without granting it direct computer-control capabilities. Core implements detection, OS-vault credential lookup, non-interactive invocation, separate stdout/stderr streaming, cancellation, and process cleanup.

Implemented invocations:

- Codex: `codex exec --json --ephemeral --sandbox read-only`
- GitHub Copilot: `copilot -p ... -s`
- Cursor: `cursor-agent -p --output-format stream-json`
- Grok: `grok -p --output-format json` (whole-message envelope; streaming-json token chunks are still reassembled if seen)

Providers propose plans. Saved semantic actions are independently policy-checked and executed by Alfred Core; provider output is never treated as a native capability.
