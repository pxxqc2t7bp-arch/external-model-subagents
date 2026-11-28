---
name: external-model-subagents
description: Orchestrate external CLI & HTTP harnesses (Codex, Claude Code, Trae, ZCode, DeepSeek, Kimi, Pi, custom) for debugging, Panel/Pipeline/Debate swarms, isolated worktree refactors, and multi-model verification.
---

# External Model Harnesses — Master Orchestrator Playbook (v3.0)

Use the `external-model-subagents_hub` MCP server (`dispatch` and `status`) to command external coding harnesses and multi-model swarms while keeping your own context clean.

## 1. Choosing Execution Mode
- **Direct single run**: `harness: "codex" | "claude" | "trae" | "zcode" | "deepseek" | "kimi" | "pi"`, optional `mode: "write", worktree: true` (emits a clean `git apply`-ready `.patch`).
- **Parallel `targets`**: Up to 4 independent targets `[{harness, provider, model, effort, prompt}]`.
- **Structured `swarm`** (pass `swarm: { ... }` on `dispatch`):
  - **Panel (`type: "panel"`)**: `{type: "panel", consensus_threshold: 0.66, agents: [{harness, model, role, weight}]}` — confidence-weighted `SWARM_VOTE` consensus across specialists.
  - **Pipeline (`type: "pipeline"`)**: `{type: "pipeline", stages: [{name, harness, model, input_template, output_validation, continue_on_error}]}` — sequential chain with `{task}`, `{prev_output}`, `{context.<stage>}` templates.
  - **Debate (`type: "debate"`)**: `{type: "debate", rounds: 2, proposer: {...}, challenger: {...}, judge: {...}}` — multi-round adversarial critique and independent verdict.

## 2. Prevent Misrouting & Anti-Bloat Prompting
- Call `status` (with `include_metrics: true` when diagnosing health) once per session before routing unfamiliar models or gateways.
- **Anti-over-defensive prompting**: Send **goal + target files + hard constraints (LOC budget, validate once at boundaries, no speculative shims) + verification command**.
- **Single-proxy gateway note**: If two models share a single-stream local proxy (e.g. `127.0.0.1:8790`), serialize calls via `pipeline` or sequential `handoff_from` rather than simultaneous heavy streams.

## 3. Three Orthogonal Axes (Combine Freely)
1. **`project_rules` (`"smart"` | `"on"` | `"off"`)**: Controls only `AGENTS.md` / `CLAUDE.md`.
2. **`capabilities` (`"code"` | `"full"`) & `mcps: [...]`**: Controls only external MCPs and plugins (`0` child MCPs in `"code"` mode).
3. **`mode` (`"plan"` | `"write"` | `"full"`) & `worktree: true`**: Read-only default vs isolated dirty-aware Git worktree patch generation.

## 4. Zero-Copy Handoff & Diagnostics
- Chain workers at **0 Master token copy cost** via `handoff_from: "c..."`.
- Inspect or replay runs from CLI via `ext-models-mcp debug <call_id>` and `ext-models-mcp replay <call_id>`.
