# External Model Subagents — Engineering & Anti-Bloat Contract

## Core Architecture Invariants
- **Pure `std` Rust**: Keep `Cargo.toml` minimal (`serde_json` + `#[cfg(unix)] libc` only). Never add `tokio`, `reqwest`, `dashmap`, `regex`, or async runtimes.
- **Bounded Memory & Binary Size**: Respect `#[global_allocator]` (`src/alloc_cap.rs`, tested at `EXT_MODELS_MAX_HEAP_MB=4`) and `< 1 MB` stripped release binary. Use `256 KiB` worker thread stacks and `O(1)` streaming (`read_bounded_line`, `open_artifact_writer`).
- **Two MCP Tools Only**: Expose only `dispatch` and `status` in `src/protocol.rs`.

## Anti-Over-Defensive Coding Rules (Mandatory)
1. **Parse Once at Boundaries, Trust Internally**: Validate/canonicalize untrusted JSON, CLI flags, and config files **once** at ingress (`main.rs`, `protocol.rs`, `execute_dispatch`, config loaders). Internal helpers and `src/swarm/*` must trust typed Rust inputs—do NOT re-check empty strings, re-canonicalize paths, or re-sanitize IDs.
2. **Flat Error, Zero Silent Fallbacks**: Use `crate::error::RichError` (`<= 8` `ErrorKind` variants). Never invent per-module nested error enums or `From` boilerplate. Never mask operational failures with `.unwrap_or_default()` or `let _ = ...` (only best-effort teardown/Drop may ignore cleanup errors).
3. **Zero Speculative Guards or Dead Shims**: Write direct, minimal code for current requirements only. No unused traits, `#[allow(dead_code)]`, speculative retries, defensive `.clone()`s, or duplicate `O_NOFOLLOW`/permission checks (reuse `stream::open_artifact_writer`). Delete replaced code cleanly.
4. **Lock Discipline**: Never perform subprocess I/O or `sleep` while holding a `Mutex` or `RwLock`.

## Verification Gate
Run `cargo clippy --all-targets -- -D warnings && cargo test` before finishing any change.
