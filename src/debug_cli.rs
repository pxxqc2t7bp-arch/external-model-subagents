//! Bounded call diagnostics (`debug` / `trace`) and safe call `replay`.

use crate::error::{ErrorKind, RichError};
use crate::harness::execute_dispatch;
use crate::stream::{artifact_dir, read_file_capped, write_artifact_bytes};
use serde_json::{Map, Value};
use std::io::{ErrorKind as IoErrorKind, Read};
use std::path::Path;
use std::sync::{atomic::AtomicBool, Arc};

fn failure(message: impl Into<String>) -> RichError {
    RichError::new(ErrorKind::ExecutionFailed, message).with_suggestion(
        "Check artifact retention/permissions and harness configuration; rerun to regenerate missing artifacts.",
    )
}

fn redact_secret(value: &mut Value, secret: &str) -> bool {
    match value {
        Value::String(text) if text.contains(secret) => {
            *text = text.replace(secret, "[redacted]");
            true
        }
        Value::Array(items) => {
            let mut hit = false;
            for v in items {
                hit |= redact_secret(v, secret);
            }
            hit
        }
        Value::Object(fields) => {
            let mut hit = false;
            for v in fields.values_mut() {
                hit |= redact_secret(v, secret);
            }
            hit
        }
        _ => false,
    }
}

/// Persist call metadata to `<call_id>-meta.json`, redacting only actual secret env values (`>= 8` bytes).
pub fn save_meta(call_id: &str, mut meta: Value, key_env: Option<&str>) {
    let mut redacted = false;
    for (name, value) in std::env::vars_os() {
        let name_str = name.to_string_lossy();
        let upper = name_str.to_ascii_uppercase();
        let is_secret_var = key_env == Some(name_str.as_ref())
            || ["API_KEY", "_TOKEN", "_SECRET", "PASSWORD"]
                .iter()
                .any(|&tag| upper.contains(tag));
        if is_secret_var {
            if let Some(secret) = value.to_str().filter(|s| s.len() >= 8) {
                redacted |= redact_secret(&mut meta, secret);
            }
        }
    }
    meta["redacted"] = Value::Bool(redacted);
    match serde_json::to_vec(&meta)
        .map_err(|e| e.to_string())
        .and_then(|bytes| write_artifact_bytes(&format!("{call_id}-meta.json"), &bytes).map(|_| ()))
    {
        Ok(()) => {}
        Err(err) => eprintln!("debug_cli: failed to persist metadata for {call_id}: {err}"),
    }
}

/// Execute `debug`, `trace`, or `replay` for `call_id`.
pub fn run(command: &str, call_id: &str) -> (bool, String) {
    let result = (|| -> Result<(bool, String), RichError> {
        if call_id.is_empty()
            || call_id.len() > 128
            || !call_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(RichError::new(
                ErrorKind::InvalidInput,
                "call_id must be 1..128 ASCII letters, digits, underscores, or hyphens",
            ));
        }
        let dir = artifact_dir().map_err(|e| failure(e.to_string()))?;
        let raw =
            read_file_capped(&dir.join(format!("{call_id}-meta.json")), 768 * 1024).map_err(failure)?;
        let mut meta: Map<String, Value> = serde_json::from_str(&raw)
            .map_err(|e| failure(format!("Invalid metadata for '{call_id}': {e}")))?;

        if command == "replay" {
            if meta.get("redacted").and_then(Value::as_bool) == Some(true) {
                return Err(RichError::new(
                    ErrorKind::InvalidInput,
                    "Replay refused because prompt/arguments contained a live secret that was redacted.",
                )
                .with_suggestion("Re-run with credentials passed via environment variables instead of inline text."));
            }
            let mut args = Map::new();
            for key in [
                "prompt",
                "harness",
                "provider",
                "model",
                "effort",
                "cwd",
                "mode",
                "project_rules",
                "capabilities",
                "mcps",
                "context_files",
                "handoff_from",
                "review_base",
                "session_id",
                "keep_session",
                "worktree",
                "timeout_ms",
                "idle_timeout_ms",
                "max_chars",
            ] {
                if let Some(v) = meta.remove(key) {
                    if !v.is_null() {
                        args.insert(key.into(), v);
                    }
                }
            }
            return Ok(execute_dispatch(
                &Value::Object(args),
                Arc::new(AtomicBool::new(false)),
            ));
        }

        let status = meta.get("status").and_then(Value::as_str).unwrap_or("unknown");
        let dur = meta.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
        let mut report = format!(
            "Call ID: {call_id}\nStatus: {status}\nDuration: {dur}ms\nMetadata:\n{}\n",
            serde_json::to_string_pretty(&meta).map_err(|e| failure(e.to_string()))?
        );

        if let Some(path) = meta.get("artifact").and_then(Value::as_str) {
            report.push_str("\nOutput artifact (up to 32 KiB):\n");
            match read_file_capped(Path::new(path), 32 * 1024) {
                Ok(text) => report.push_str(&text),
                Err(e) => report.push_str(&format!("(unavailable: {e})")),
            }
        }

        match std::fs::File::open(dir.join(format!("{call_id}-panic.log"))) {
            Ok(file) => {
                let mut panic_buf = Vec::with_capacity(4096);
                let _ = file.take(32 * 1024).read_to_end(&mut panic_buf);
                report.push_str("\n\nPanic log:\n");
                report.push_str(&String::from_utf8_lossy(&panic_buf));
            }
            Err(e) if e.kind() == IoErrorKind::NotFound => {}
            Err(e) => report.push_str(&format!("\n\nPanic log error: {e}")),
        }
        Ok((false, report))
    })();
    result.unwrap_or_else(|e| (true, e.to_string()))
}
