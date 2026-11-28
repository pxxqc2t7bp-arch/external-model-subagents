//! Minimal MCP JSON-RPC 2.0 schemas, synchronized stdout writer, and active request cancellation map.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Thread-safe JSON-RPC line emitter over `stdout`.
#[derive(Clone, Default)]
pub struct RpcWriter {
    lock: Arc<Mutex<()>>,
}

impl RpcWriter {
    pub fn send(&self, msg: &Value) {
        let Ok(mut line) = serde_json::to_string(msg) else {
            return;
        };
        line.push('\n');
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = io::stdout();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }

    pub fn send_result(&self, id: Value, result: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }));
    }

    pub fn send_error(&self, id: Value, code: i32, message: &str) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }));
    }
}

/// Tracks in-flight JSON-RPC `tools/call` requests so `notifications/cancelled` or EOF aborts them.
#[derive(Clone, Default)]
pub struct ActiveRequests {
    inner: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl ActiveRequests {
    pub fn register(&self, req_key: String) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        if let Ok(mut map) = self.inner.lock() {
            map.insert(req_key, Arc::clone(&flag));
        }
        flag
    }

    pub fn remove(&self, req_key: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(req_key);
        }
    }

    pub fn cancel_one(&self, req_key: &str) {
        if let Ok(map) = self.inner.lock() {
            if let Some(flag) = map.get(req_key) {
                flag.store(true, Ordering::Release);
            }
        }
    }

    pub fn cancel_all(&self) {
        if let Ok(map) = self.inner.lock() {
            for flag in map.values() {
                flag.store(true, Ordering::Release);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().map(|m| m.is_empty()).unwrap_or(true)
    }
}

pub fn id_to_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Ultra-compact MCP tool definitions (`dispatch` + `status`) minimizing schema context tokens.
pub fn tools_schema() -> Value {
    json!([
        {
            "name": "dispatch",
            "description": "Dispatch coding, review, or analysis work to external CLI harnesses (codex, claude, trae, zcode, or custom). Supports single run, parallel `targets`, isolated git `worktree`, zero-copy `handoff_from`, selective `mcps`, and decoupled `project_rules`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Goal, file paths, constraints, and verification commands." },
                    "cwd": { "type": "string", "description": "Workspace directory path." },
                    "harness": { "type": "string", "description": "Target CLI ('codex'|'claude'|'trae'|'zcode'|'auto')." },
                    "provider": { "type": "string", "description": "Optional provider override (e.g. 'openai' subscription vs 'litellm' gateway)." },
                    "model": { "type": "string", "description": "Model ID or shorthand (e.g. '5.4codex', 'sonnet-4.6', 'opus')." },
                    "effort": { "type": "string", "enum": ["low", "medium", "high", "xhigh", "max"] },
                    "mode": { "type": "string", "enum": ["plan", "write", "full"], "description": "Default 'plan' (read-only)." },
                    "project_rules": { "type": "string", "enum": ["smart", "on", "off"], "description": "AGENTS.md/CLAUDE.md handling (default 'smart')." },
                    "capabilities": { "type": "string", "enum": ["code", "full"], "description": "'code' (default: built-in+skills, 0 MCP) or 'full' (all plugins/MCPs)." },
                    "mcps": { "type": "array", "items": { "type": "string" }, "description": "Selective MCP server names to mount." },
                    "worktree": { "type": "boolean", "description": "Run in isolated dirty-aware git worktree and emit a `.patch`." },
                    "review_base": { "type": "string", "description": "Git branch/commit or 'uncommitted' for adversarial diff review." },
                    "handoff_from": { "type": "string", "description": "Previous `id` (`c...`) to pipe locally without re-copying text." },
                    "context_files": { "type": "array", "items": { "type": "string" }, "description": "File paths to attach locally." },
                    "session_id": { "type": "string", "description": "Session ID or 'last' to resume." },
                    "keep_session": { "type": "boolean", "description": "Persist session on disk for multi-turn reuse." },
                    "targets": {
                        "type": "array",
                        "description": "Up to 4 parallel targets: [{harness, provider, model, effort, prompt}].",
                        "items": { "type": "object" }
                    },
                    "swarm": {
                        "type": "object",
                        "description": "Structured multi-agent collaboration: {type: 'panel'|'pipeline'|'debate', agents/stages/proposer+challenger+judge, consensus_threshold, rounds}."
                    },
                    "timeout_ms": { "type": "integer", "description": "Total timeout ms (default 300000)." },
                    "max_chars": { "type": "integer", "description": "Inline output char budget (default 8000)." }
                }
            }
        },
        {
            "name": "status",
            "description": "Inspect installed harnesses, providers (subscription vs gateway), available models, MCPs/plugins, and Rust hub heap usage.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cwd": { "type": "string", "description": "Optional workspace path to include project-scoped MCPs." },
                    "probe_models": { "type": "boolean", "description": "Refresh uncached dynamic CLI model lists (e.g. Trae)." },
                    "include_metrics": { "type": "boolean", "description": "Include process-local dispatch, swarm, routing and latency metrics." }
                }
            }
        }
    ])
}
