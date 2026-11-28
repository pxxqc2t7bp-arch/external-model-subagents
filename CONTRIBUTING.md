# Contributing to External Model Subagents

## Development Setup
Requires Rust stable (`edition = "2021"`) and `git`.

```bash
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --release -- --ignored
```

## Engineering & Anti-Bloat Rules
Please read [AGENTS.md](./AGENTS.md) before opening a pull request:
1. **Keep `Cargo.toml` Pure `std`**: Do not add async runtimes (`tokio`), HTTP stacks (`reqwest`), or utility crates (`dashmap`, `regex`).
2. **Avoid Over-Defensive Coding**: Validate inputs once at ingress (`protocol.rs`, `main.rs`, `SwarmSpec::from_args`, config loaders). Do not duplicate validation, invent per-module error enums, or leave dead shims.
3. **Respect Heap & Binary Budgets**: Every streaming path must stay `O(1)` in RAM under `EXT_MODELS_MAX_HEAP_MB=4`, and stripped release binaries must remain `< 1 MiB`.
