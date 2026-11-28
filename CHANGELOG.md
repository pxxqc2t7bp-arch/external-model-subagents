# Changelog

## [3.0.0] - 2026-11-28

### Added
- **In-Process Swarm Modes (`dispatch.swarm` / `--swarm`)**:
  - `panel`: `256 KiB`-stack scoped parallel execution with weighted `SWARM_VOTE` consensus (`Approve` / `Reject` / `NeedsChanges` / `Abstain`) and partial-failure resilience.
  - `pipeline`: Sequential multi-stage execution with single-pass `{task}`, `{prev_output}`, and `{context.<stage>}` template expansion plus built-in output validation (`non_empty`, `json_valid`, `contains`, `min_chars`).
  - `debate`: Multi-round `proposer` vs `challenger` rebuttal loop adjudicated by an independent `judge` (`DEBATE_VERDICT`).
- **Declarative HTTP Framework & New Harnesses**:
  - `HttpHarnessAdapter` (`http-harnesses.json`) streaming `openai_sse` (including `<reasoning>` extraction) and `anthropic_sse` via `curl -K -` stdin config with zero `argv`/env credential exposure and automatic output secret redaction.
  - Built-in `deepseek`, `kimi`, and `pi` adapters with seamless CLI-first / HTTP-fallback discovery.
- **Observability, Metrics & Diagnostics**:
  - Opt-in structured event logging (`EXT_MODELS_LOG_FILE`, `EXT_MODELS_LOG_LEVEL`, `EXT_MODELS_LOG_FORMAT`).
  - Zero-allocation `AtomicU64` metrics and 5-bucket latency histogram (`status --metrics` / `include_metrics: true`).
  - Semantic 5-second MCP `notifications/progress` updates (`ProgressState`).
  - `ext-models-mcp debug|trace|replay <call_id>` CLI commands backed by secret-scrubbed `<call_id>-meta.json`.
- **Foundation & Cross-Platform Hardening**:
  - Conditional `#[cfg(unix)]` / `#[cfg(not(unix))]` platform layer + GitHub Actions Linux/macOS/Windows CI matrix.
  - RAII `Drop` cleanup guard on `WorktreeSession` preventing leaked worktrees on worker panic, plus lazy `EXT_MODELS_ARTIFACT_TTL_DAYS` artifact sweep.
  - `RichError` actionable diagnostics and `<call_id>-panic.log` persistence.

### Fixed
- Scoped version-mismatch checks to the requested `provider` and separated OpenAI `models_cache.json` from custom gateway providers in `catalog.rs`.
- Suppressed Codex `[plugins.*]` remote OAuth MCP workers in headless `Code` and `Selective` capability modes, and removed empty `model_catalog_json=""` override that triggered `os error 21`.
- Switched Claude adapter to `--output-format stream-json --verbose` to prevent `idle_timeout` during long multi-turn tool executions.

## [2.0.0] - 2026-11-27
- Initial heap-bounded pure-`std` Rust MCP server & CLI hub (`codex`, `claude`, `trae`, `zcode`, custom CLIs, 3-axis orthogonal controls, dirty-aware Git worktrees, zero-copy handoff).
