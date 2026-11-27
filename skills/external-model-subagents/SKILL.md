---
name: external-model-subagents
description: Orchestrate external CLI harnesses (Codex, Claude Code, Trae, ZCode, custom CLIs) for debugging, adversarial code review, isolated worktree refactors, and multi-model verification.
---

# External Model Harnesses — Master Orchestrator Playbook

Use the `external-model-subagents_hub` MCP server (`dispatch` and `status`) to command external coding harnesses as specialized workers while keeping your own context clean.

## 1. When to Delegate vs. Execute Directly
- **Do directly yourself**: 1–2 file edits, quick searches, UI/browser checks, or routine workspace commands.
- **Delegate via `dispatch`**:
  - **Deep root-cause debugging / adversarial diff review**: `codex` (use `review_base: "uncommitted"` or `"main"`).
  - **Multi-file module refactoring / isolated implementation**: `claude` or `codex` with `mode: "write"`, `worktree: true` (emits a clean `.patch` you can inspect and `git apply`).
  - **Multi-model council / cross-verification**: pass `targets: [{harness: "codex", model: "..."}, {harness: "claude", model: "..."}]` (up to 4 concurrent runs).
  - **Harness-exclusive plugins/IM workflows**: pass `capabilities: "full"` or `mcps: ["<server_name>"]`.

## 2. Prevent Misrouting: Inspect Inventory First (`status`)
Users often run official subscriptions alongside custom Model Gateways (LiteLLM, OneAPI, OpenRouter, Bedrock, etc.), and a model alias (e.g. `opus`, `5.4codex`, `sonnet-4.6`) may exist only on specific providers or under gateway-prefixed IDs.
- Call `status` once per session before routing unfamiliar models or MCPs.
- Switch providers per call without mutating global configs via `provider` (e.g. `harness: "codex", provider: "openai", model: "gpt-5.4"` vs gateway models).
- Never guess downgraded versions: the hub enforces version-preserving matching and passes uncached corporate gateway IDs through when `harness` is explicit.

## 3. Three Orthogonal Axes (100% Decoupled — Combine Freely)
1. **`project_rules` (`"smart"` | `"on"` | `"off"`)** — *controls only `AGENTS.md` / `CLAUDE.md`*:
   - `"on"`: project rules are clean engineering conventions; load 100% natively.
   - `"smart"` *(default)*: keep repo coding/test conventions, suppress interactive human-prompt / `/plan` / git-push stalls.
   - `"off"`: repo rules belong to a conflicting framework or hurt the task; skip project rule files completely (`CLAUDE_CODE_DISABLE_CLAUDE_MDS=1` / `project_doc_max_bytes=0`) **without** disabling MCPs or skills.
   - Always honor explicit user instructions to enable or disable project rules.
2. **`capabilities` (`"code"` | `"full"`) & `mcps: [...]`** — *controls only external tools/plugins*:
   - `"code"` *(default)*: built-in tools + zero-cost local Skills, `0` external MCP processes.
   - `mcps: ["name"]`: dynamically slice config to launch **only** the named MCP server(s).
   - `"full"`: enable all installed plugins and non-GUI business MCPs.
3. **`mode` (`"plan"` | `"write"` | `"full"`) & `worktree: true`**:
   - Default `"plan"` is read-only. Use `worktree: true` for parallel edits or dirty-workspace safety.

## 4. Zero-Copy Inter-Agent Relay & Async Background Runs
- **Briefing rule**: Send **goal + relative file paths + constraints + verification command**—never paste hundreds of lines of source code that the worker can read itself.
- **Zero-copy handoff**: Every `dispatch` header returns `id=c...`. When chaining Agent A -> Agent B (e.g., implementer -> reviewer), pass `handoff_from: "c..."` plus a 1-sentence instruction so the Rust hub pipes the full artifact locally at **0 Master token copy cost**.
- **Long async tasks (> 5 min)**: Run `"$HOME/.gemini/config/plugins/external-model-subagents/bin/ext-models-mcp" run --cwd "<dir>" -H <harness> -m <model> - <<'EOF'` via `run_command` in the background (`WaitMsBeforeAsync: 1000`) and wait for native task completion notification.
