# Provider adapters

Provider adapters supervise one installed CLI without granting it Alfred's native computer-control capability. Core implements detection, OS-vault credential lookup, non-interactive invocation, exact session continuation, separate stdout/stderr streaming, cancellation, and process cleanup. The complete run state is also persisted by Core, so CLI session history is helpful context rather than a single point of failure.

Implemented invocations:

- Codex: first turn `codex ... exec --json --sandbox read-only`; later turns `codex ... exec resume <thread-id>`. The session is intentionally not ephemeral. Optional Settings picks are passed as `-m <model>` and `-c model_reasoning_effort="<effort>"`. Model/effort lists come from `codex debug models`.
- GitHub Copilot: `copilot -p ... --output-format json --session-id <run-id>` with custom instructions, shell/write/URL/memory, and built-in MCPs disabled. Optional Settings picks are `--model` and `--effort`. Lists come from `copilot help config` and `copilot --help`.
- Cursor: `cursor-agent -p --output-format stream-json`, then `--resume=<session-id>`; Alfred never passes `--force`. Optional Settings model is `--model`; effort is applied as `model[effort=…]` when the CLI listed that form. Lists come from `cursor-agent --list-models` or `cursor-agent models`.
- Grok: `grok -p --output-format json --session-id <run-id> --tools read_file --no-subagents`, then `--resume <run-id>` (whole-message envelope; streaming-json token chunks are still reassembled if seen). Optional Settings picks are `-m` and `--reasoning-effort`. Models come from `grok models`; applicable efforts are read from the CLI's rejection text for that model.

Every provider proposes one action at a time. Saved goals are re-planned against current observations rather than replaying stale refs. Provider output is always reclassified by Core and is never treated as a native capability.
