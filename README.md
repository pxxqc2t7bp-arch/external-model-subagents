# External Model Subagents (v3.0 — Heap-Bounded Rust Swarm & Harness Hub)

Ultra-lightweight native **pure-`std` Rust MCP Server & CLI Hub** (`~650 KB` binary, `~2 MB` idle RSS, `0.0%` idle CPU) for orchestrating external AI coding CLIs (**OpenAI Codex**, **Claude Code**, **Trae**, **ZCode**, **DeepSeek**, **Kimi**, plus custom CLIs) and zero-exposure **HTTP SSE harnesses** (**Pi**, **OpenRouter**, custom gateways) from Antigravity.

## Key Capabilities in v3.0

- **Structured Multi-Model Swarm Modes (`swarm`)**:
  - **`panel`**: Parallel multi-model review/analysis with confidence-weighted `SWARM_VOTE` consensus and partial-failure tolerance.
  - **`pipeline`**: Multi-stage sequential workflows (`analyze -> refine -> verify`) with single-pass template variables (`{task}`, `{prev_output}`, `{context.<stage>}`) and output validation (`non_empty`, `json_valid`, `contains`, `min_chars`).
  - **`debate`**: Multi-round `proposer` vs `challenger` adversarial critique judged by an independent `judge` model.
- **Expanded CLI + Zero-Leakage HTTP Ecosystem**:
  - Built-in adapters for `codex`, `claude`, `trae`, `zcode`, `deepseek` (CLI + R1 reasoning HTTP fallback), `kimi` (CLI + Moonshot HTTP fallback), and `pi` (HTTP SSE).
  - Declarative `harnesses.json` (custom CLIs) and `http-harnesses.json` (`openai_sse` / `anthropic_sse` via `curl -K -` stdin config — zero API keys in process `argv` or child env).
- **Three 100% Orthogonal Control Axes**:
  1. **Headless Guard**: Strips GUI popups, TTY hooks, remote OAuth plugin MCP stalls, and recursive MCP loops.
  2. **`project_rules` (`smart` | `on` | `off`)**: Controls `AGENTS.md` / `CLAUDE.md` independently of MCPs and skills.
  3. **`capabilities` (`code` | `full`) & `mcps: [...]`**: Instant zero-MCP default, selective MCP slicing, or full plugin access.
- **Observability, Metrics & Time-Travel Debugging**:
  - `ext-models-mcp status --metrics` (or MCP `status` with `include_metrics: true`) reports `AtomicU64` dispatch/swarm counters, 5-bucket latency histograms, and live `#[global_allocator]` heap stats.
  - `ext-models-mcp debug <call_id>` & `ext-models-mcp replay <call_id>` inspect timelines/artifacts and safely replay calls.

## Quick Install

```bash
./scripts/install.sh
```

## Quick Examples

```bash
# 1. Inspect installed harnesses, models, heap stats, and cumulative metrics
ext-models-mcp status --metrics

# 2. Run a 3-agent Panel Swarm with weighted consensus
ext-models-mcp run --swarm '{"type":"panel","consensus_threshold":0.66,"agents":[{"harness":"codex","role":"security"},{"harness":"claude","role":"architecture"},{"harness":"deepseek","role":"reasoning"}]}' "Audit src/sandbox.rs for race conditions"

# 3. Inspect or replay a previous call
ext-models-mcp debug c12345_1
ext-models-mcp replay c12345_1
```

## Harness configuration and trust

With `capabilities="full"`, the worker can invoke configured MCP servers and plugins with their own credentials and permissions. Those tools can read, change, or transmit data outside a Git worktree. Enable full access only for trusted user and project configurations. Codex and Claude filter the hub's recursive and computer-use MCP names. On Trae, `project_rules="off"` supplies best-effort prompt guidance because `traecli` lacks a native rule-disable flag.

| File | Supported fields |
| --- | --- |
| `harnesses.json` | Each CLI entry requires `id` and `binary`. Optional fields are `strengths`, `args`, `plan_flags`, `write_flags`, `full_flags`, `prompt_via_stdin` and `json_result_field`. Argument templates expand `{cwd}`, `{model}` and `{prompt}` once. |
| `http-harnesses.json` | Each entry requires `id`, `endpoint`, `api_key_env` and `default_model`. Optional fields are `strengths`, `models`, `extra_headers` and `response_format` (alias `format`), which accepts `openai_sse` or `anthropic_sse`. |
| `models.json` | Maps each harness name to an object of provider names and their arrays of model ID strings. |

The two harness files accept an array of entries or an object containing a `harnesses` array. Both support `$schema` metadata. Set `EXT_MODELS_HARNESSES_FILE`, `EXT_MODELS_HTTP_HARNESSES_FILE` or `EXT_MODELS_CATALOG_FILE` to select the corresponding file.

## Environment Variables

| Variable | Default | Purpose |
| :--- | :--- | :--- |
| `EXT_MODELS_MAX_HEAP_MB` | `16` | Hard heap ceiling in MiB (`4..=256`) |
| `EXT_MODELS_MAX_JOBS` | `3` | Max concurrent child CLI/HTTP processes (`1..=12`) |
| `EXT_MODELS_ARTIFACT_TTL_DAYS` | `7` | Auto-clean expired `/tmp/ext-models` artifacts (`0` = disable) |
| `EXT_MODELS_DEFAULT_RULES` | `smart` | Default `project_rules` policy (`smart` \| `on` \| `off`) |
| `EXT_MODELS_DEFAULT_CAPS` | `code` | Default `capabilities` profile (`code` \| `full`) |
| `EXT_MODELS_LOG_FILE` | *(unset)* | Opt-in path for structured event logs |
| `EXT_MODELS_LOG_LEVEL` | `info` | Log verbosity (`error` \| `warn` \| `info` \| `debug`) |
| `EXT_MODELS_LOG_FORMAT` | `human` | Log encoding (`human` \| `json`) |

## Documentation
- [Architecture Guide](./ARCHITECTURE.md)
- [Engineering & Anti-Bloat Contract](./AGENTS.md)
- [Contributing](./CONTRIBUTING.md)
- [Changelog](./CHANGELOG.md)

## License

MIT
