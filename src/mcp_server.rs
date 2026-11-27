//! Stdio JSON-RPC 2.0 loop (`0.0%` idle CPU, compact 256 KiB worker threads, progress heartbeats,
//! and automatic child process-group cleanup on cancellation or `stdin` EOF).

use crate::harness::{build_status_report, execute_dispatch};
use crate::protocol::{id_to_key, tools_schema, ActiveRequests, RpcWriter};
use crate::stream::read_bounded_line;
use serde_json::{json, Value};
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const MAX_RPC_LINE_BYTES: usize = 512 * 1024;
const WORKER_STACK: usize = 256 * 1024;

pub fn serve_stdio() -> i32 {
    let writer = RpcWriter::default();
    let active = ActiveRequests::default();

    let stdin = io::stdin();
    let mut reader = BufReader::with_capacity(8192, stdin.lock());
    let mut scratch = Vec::with_capacity(4096);

    while let Ok(Some((line, _))) = read_bounded_line(&mut reader, &mut scratch, MAX_RPC_LINE_BYTES)
    {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        handle_message(msg, &writer, &active);
    }

    // Host closed stdin: abort all running child process groups so zero orphans remain.
    active.cancel_all();
    for _ in 0..15 {
        if active.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(40));
    }
    0
}

fn handle_message(msg: Value, writer: &RpcWriter, active: &ActiveRequests) {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id_opt = msg.get("id").cloned();

    match method {
        "initialize" => {
            if let Some(id) = id_opt {
                let proto = msg
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .unwrap_or("2024-11-05");
                writer.send_result(
                    id,
                    json!({
                        "protocolVersion": proto,
                        "capabilities": { "tools": {} },
                        "serverInfo": {
                            "name": "ext-models-mcp",
                            "version": env!("CARGO_PKG_VERSION")
                        }
                    }),
                );
            }
        }
        "notifications/initialized" => {}
        "notifications/cancelled" => {
            if let Some(req_id) = msg.get("params").and_then(|p| p.get("requestId")) {
                active.cancel_one(&id_to_key(req_id));
            }
        }
        "ping" => {
            if let Some(id) = id_opt {
                writer.send_result(id, json!({}));
            }
        }
        "tools/list" => {
            if let Some(id) = id_opt {
                writer.send_result(id, json!({ "tools": tools_schema() }));
            }
        }
        "tools/call" => {
            let Some(id) = id_opt else {
                return;
            };
            spawn_tool_call(id, msg.get("params").cloned().unwrap_or(Value::Null), writer, active);
        }
        _ => {
            if let Some(id) = id_opt {
                writer.send_error(id, -32601, &format!("Method not found: {method}"));
            }
        }
    }
}

fn spawn_tool_call(id: Value, params: Value, writer: &RpcWriter, active: &ActiveRequests) {
    let tool_name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let progress_token = params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .cloned();

    if tool_name == "status" {
        let probe = args
            .get("probe_models")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        if !probe {
            let text = build_status_report(&cwd, false);
            writer.send_result(
                id,
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false
                }),
            );
            return;
        }
    }

    let req_key = id_to_key(&id);
    let req_key_err = req_key.clone();
    let id_worker = id.clone();
    let cancel_flag = active.register(req_key.clone());
    let writer_c = writer.clone();
    let active_c = active.clone();

    let res = thread::Builder::new()
        .name("ext-rpc-worker".into())
        .stack_size(WORKER_STACK)
        .spawn(move || {
            let hb_done = Arc::new(AtomicBool::new(false));
            if let Some(tok) = progress_token {
                let w_hb = writer_c.clone();
                let done_hb = Arc::clone(&hb_done);
                let _ = thread::Builder::new()
                    .name("ext-progress".into())
                    .stack_size(128 * 1024)
                    .spawn(move || {
                        let mut step = 1u64;
                        while !done_hb.load(Ordering::Acquire) {
                            thread::sleep(Duration::from_secs(15));
                            if done_hb.load(Ordering::Acquire) {
                                break;
                            }
                            w_hb.send(&json!({
                                "jsonrpc": "2.0",
                                "method": "notifications/progress",
                                "params": {
                                    "progressToken": tok,
                                    "progress": step,
                                }
                            }));
                            step += 1;
                        }
                    });
            }

            let (is_err, text) = match tool_name.as_str() {
                "dispatch" => execute_dispatch(&args, cancel_flag),
                "status" => {
                    let cwd = args
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(PathBuf::from)
                        .or_else(|| std::env::current_dir().ok())
                        .unwrap_or_else(|| PathBuf::from("."));
                    (false, build_status_report(&cwd, true))
                }
                other => (true, format!("Unknown tool: {other}")),
            };

            hb_done.store(true, Ordering::Release);
            active_c.remove(&req_key);
            writer_c.send_result(
                id_worker,
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_err
                }),
            );
        });

    if let Err(e) = res {
        active.remove(&req_key_err);
        writer.send_error(id, -32603, &format!("failed to spawn worker thread: {e}"));
    }
}
