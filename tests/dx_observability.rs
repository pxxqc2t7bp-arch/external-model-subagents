#![cfg(unix)]

use ext_models_mcp::debug_cli;
use ext_models_mcp::harness::{
    build_status_report_with_options, execute_dispatch_with_progress, ProgressState,
};
use ext_models_mcp::stream::next_call_id;
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[test]
fn observability_metrics_progress_and_debug_replay_work_end_to_end() {
    let dir = std::env::temp_dir().join(format!("ext-dx-{}", next_call_id()));
    fs::create_dir_all(&dir).unwrap();
    let log_file = dir.join("events.jsonl");

    let fake_cli = dir.join("dx-agent");
    fs::write(
        &fake_cli,
        "#!/bin/sh\nin=$(cat)\nprintf '{\"reply\":\"Echo: %s\"}\\n' \"$in\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_cli, fs::Permissions::from_mode(0o755)).unwrap();

    let cfg = dir.join("harnesses.json");
    fs::write(
        &cfg,
        serde_json::to_string(&json!({
            "harnesses": [{
                "id": "dxmock",
                "binary": fake_cli,
                "args": [],
                "prompt_via_stdin": true,
                "json_result_field": "reply"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    // Run via compiled CLI subprocess to test `EXT_MODELS_LOG_FILE`, `debug`, `replay`, and `status --metrics`.
    let bin = env!("CARGO_BIN_EXE_ext-models-mcp");
    // Prompt deliberately contains words like "token" and "secret_key" to prove non-over-defensive replay!
    let run_out = Command::new(bin)
        .args([
            "run",
            "-H",
            "dxmock",
            "-C",
            &dir.to_string_lossy(),
            "--rules",
            "off",
            "Fix auth token refresh and id_to_key helper",
        ])
        .env("EXT_MODELS_HARNESSES_FILE", &cfg)
        .env("EXT_MODELS_LOG_FILE", &log_file)
        .env("EXT_MODELS_LOG_FORMAT", "json")
        .env("EXT_MODELS_LOG_LEVEL", "info")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&run_out.stdout);
    assert!(run_out.status.success(), "CLI run failed: {stdout}");

    // Verify structured JSON log file was written.
    let logs = fs::read_to_string(&log_file).unwrap();
    assert!(logs.contains("\"event\":\"dispatch_start\""), "{logs}");
    assert!(logs.contains("\"event\":\"dispatch_end\""), "{logs}");

    // Extract call_id and verify `debug <call_id>` + `replay <call_id>` work cleanly!
    let call_id = stdout
        .split("id=")
        .nth(1)
        .unwrap()
        .split([' ', '|', ']'])
        .next()
        .unwrap()
        .trim()
        .to_string();

    let (dbg_err, dbg_txt) = debug_cli::run("debug", &call_id);
    assert!(!dbg_err, "debug failed: {dbg_txt}");
    assert!(dbg_txt.contains("Status: success"), "{dbg_txt}");
    assert!(dbg_txt.contains("Fix auth token refresh"), "{dbg_txt}");

    std::env::set_var("EXT_MODELS_HARNESSES_FILE", &cfg);
    let progress = Arc::new(ProgressState::default());
    let (rep_err, rep_txt) = execute_dispatch_with_progress(
        &json!({
            "harness": "dxmock",
            "cwd": dir,
            "project_rules": "off",
            "prompt": "Progress & metrics check"
        }),
        Arc::new(AtomicBool::new(false)),
        Some(Arc::clone(&progress)),
    );
    assert!(!rep_err, "{rep_txt}");
    let notif = progress.notification(&json!("tok-1"));
    assert_eq!(notif["progressToken"], json!("tok-1"));
    assert_eq!(notif["details"]["completed"], json!(1));

    let (replay_err, replay_out) = debug_cli::run("replay", &call_id);
    assert!(!replay_err, "replay must succeed on ordinary 'token'/'_key' prompt: {replay_out}");
    assert!(replay_out.contains("Fix auth token refresh"), "{replay_out}");

    let status_metrics = build_status_report_with_options(&dir, false, true);
    let parsed: Value = serde_json::from_str(&status_metrics).unwrap();
    assert!(parsed["metrics"]["dispatch"]["total"].as_u64().unwrap() >= 2);
    assert!(parsed["metrics"]["heap_stats_kb"]["limit"].as_u64().unwrap() >= 4096);

    std::env::remove_var("EXT_MODELS_HARNESSES_FILE");
    let _ = fs::remove_dir_all(&dir);
}
