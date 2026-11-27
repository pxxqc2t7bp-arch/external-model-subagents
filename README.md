# External Model Subagents (v2.0 — Rust Heap-Bounded Master–Harness Hub)

Ultra-lightweight native **Rust MCP Server & CLI Hub** for orchestrating external AI coding harnesses (**OpenAI Codex**, **Claude Code**, **Trae**, **ZCode**, plus declarative custom CLIs like **OpenCode** or **Aider**) from a Master Orchestrator such as Antigravity.

## Key Design Highlights

- **Tiny Runtime Footprint**: Replaces multiple idle Node.js MCP servers (`~58 MB` RSS) with **1 native pure-`std` Rust binary** (`~500 KB` binary, `~2 MB` idle RSS, `0.0%` idle CPU) guarded by a `#[global_allocator]` hard heap ceiling (`EXT_MODELS_MAX_HEAP_MB=16`) and structurally bounded `64 KiB` streaming line buffers.
- **Minimal Passive Context (~76% Fewer Tokens/Turn)**: Exposes just 2 unified MCP tools (`dispatch`, `status`) with zero redundant background subagent definitions.
- **Provider-Aware & Subscription/Gateway Safe Routing**:
  - Cleanly separates **Official Subscriptions (OAuth)** from **Custom Model Gateways** (LiteLLM, OneAPI, OpenRouter, Bedrock, Vertex, corporate hubs) per `(harness, provider)`.
  - Supports per-call `provider` switching (e.g., `provider: "openai"` vs `provider: "litellm"`) without editing global `~/.codex/config.toml`.
  - Enforces **version-preserving normalization** (e.g., `5.4codex` resolves to `gpt-5.4-codex` or gateway-prefixed equivalents, while refusing silent version downgrades like `5.4 -> 5.2`).
- **Three 100% Orthogonal Control Axes**:
  1. **Headless Guard (always active)**: Strips GUI popups, TTY-blocking hooks, and self-recursive MCP loops (`A -> B -> A`).
  2. **`project_rules` (`smart` | `on` | `off`)**: Controls `AGENTS.md` / `CLAUDE.md` loading (`CLAUDE_CODE_DISABLE_CLAUDE_MDS=1` / `project_doc_max_bytes=0`) with **zero coupling** to MCPs, plugins, or skills.
  3. **`capabilities` (`code` | `full`) & `mcps: [...]`**: Keep default runs instant (`0` child MCPs while preserving zero-cost local Skills), mount only named MCP servers via dynamic config slicing, or unlock full plugin ecosystems on demand.
- **Zero-Copy Inter-Agent Handoff & Dirty-Aware Git Worktrees**:
  - `handoff_from: "<call_id>"` pipes Agent A's full output directly into Agent B's stdin inside Rust—zero Master token copy tax.
  - `worktree: true` snapshots uncommitted tracked & untracked changes into an isolated temporary Git worktree and exports a clean `.patch` (`git apply <patch>`).

## Quick Install

```bash
./scripts/install.sh
```

Requires Rust (`cargo`, edition 2021) on macOS or Linux. No hardcoded paths—works out of the box across `$HOME`, Mise, NVM, FNM, Volta, and Homebrew installations.

## Extending Without Recompiling

- **Add Custom CLI Harnesses**: Copy `harnesses.example.json` to `~/.gemini/config/plugins/external-model-subagents/harnesses.json`.
- **Declare Uncached Gateway Models**: Copy `models.example.json` to `~/.gemini/config/plugins/external-model-subagents/models.json`.

## Environment Variables

| Variable | Default | Purpose |
| :--- | :--- | :--- |
| `EXT_MODELS_MAX_HEAP_MB` | `16` | Hard heap ceiling in MiB (`4..=256`) |
| `EXT_MODELS_MAX_JOBS` | `3` | Max concurrent external CLI processes (`1..=8`) |
| `EXT_MODELS_DEFAULT_RULES` | `smart` | Default `project_rules` policy (`smart` \| `on` \| `off`) |
| `EXT_MODELS_DEFAULT_CAPS` | `code` | Default `capabilities` profile (`code` \| `full`) |

## License

MIT
