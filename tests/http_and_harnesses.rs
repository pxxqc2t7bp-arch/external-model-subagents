#![cfg(unix)]

use ext_models_mcp::harness::{build_status_report, execute_dispatch};
use ext_models_mcp::stream::next_call_id;
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn http_adapter_streams_openai_reasoning_and_never_leaks_api_key_in_argv() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("ext-http-{}", next_call_id()));
    fs::create_dir_all(&dir).unwrap();

    let argv_file = dir.join("curl_argv.txt");
    let stdin_file = dir.join("curl_stdin.txt");
    let fake_curl = dir.join("fake-curl");

    // Fake curl checks the key is absent from its environment and captures argv and stdin (-K -), then emits DeepSeek-style SSE + HTTP 200.
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n[ -z \"${{DEEPSEEK_API_KEY:-}}\" ] || exit 9\ncat > \"{}\"\n\
         printf 'data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"step1 \"}}}}]}}\\n\\n'\n\
         printf 'data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"step2\"}}}}]}}\\n\\n'\n\
         printf 'data: {{\"choices\":[{{\"delta\":{{\"content\":\"Final \"}}}}]}}\\n\\n'\n\
         printf 'data: {{\"choices\":[{{\"delta\":{{\"content\":\"Answer\"}}}}]}}\\n\\n'\n\
         printf 'data: [DONE]\\n\\n'\n\
         printf '\\n__HTTP_STATUS__:200\\n'\n",
        argv_file.display(),
        stdin_file.display()
    );
    fs::write(&fake_curl, script).unwrap();
    fs::set_permissions(&fake_curl, fs::Permissions::from_mode(0o755)).unwrap();

    let secret = "sk-super-secret-deepseek-998877";
    std::env::set_var("CURL_BIN", &fake_curl);
    std::env::set_var("DEEPSEEK_API_KEY", secret);
    std::env::remove_var("DEEPSEEK_BIN");

    let (failed, out) = execute_dispatch(
        &json!({
            "harness": "deepseek",
            "cwd": dir,
            "project_rules": "off",
            "prompt": "Solve 2+2"
        }),
        Arc::new(AtomicBool::new(false)),
    );

    assert!(!failed, "deepseek HTTP dispatch failed: {out}");
    assert!(out.contains("<reasoning>\nstep1 step2\n</reasoning>\n\nFinal Answer"), "{out}");

    // Security invariant: secret MUST NOT appear in process argv, child env, or status output!
    let argv_dump = fs::read_to_string(&argv_file).unwrap();
    let stdin_dump = fs::read_to_string(&stdin_file).unwrap();
    assert!(!argv_dump.contains(secret), "API key leaked into curl argv!");
    assert!(stdin_dump.contains(secret), "API key must be passed via curl -K stdin");

    let status_txt = build_status_report(&dir, false);
    assert!(!status_txt.contains(secret), "API key must never appear in status report");
    let status_json: Value = serde_json::from_str(&status_txt).unwrap();
    let ds = status_json["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["harness"] == "deepseek")
        .expect("deepseek must appear in status report");
    assert_eq!(ds["ready"], json!(true));

    std::env::remove_var("DEEPSEEK_API_KEY");
    std::env::remove_var("CURL_BIN");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn custom_http_anthropic_sse_and_http_error_status_and_redaction() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("ext-http-anth-{}", next_call_id()));
    fs::create_dir_all(&dir).unwrap();

    let fake_curl = dir.join("fake-curl");
    let mode_file = dir.join("mode.txt");
    let script = format!(
        "#!/bin/sh\ncat >/dev/null\nmode=$(cat \"{}\")\nif [ \"$mode\" = \"ok\" ]; then\n\
         printf 'event: content_block_delta\\ndata: {{\"type\":\"content_block_delta\",\"delta\":{{\"text\":\"Hello \"}}}}\\n\\n'\n\
         printf 'data: {{\"type\":\"content_block_delta\",\"delta\":{{\"text\":\"Anthropic\"}}}}\\n\\n'\n\
         printf '\\n__HTTP_STATUS__:200\\n'\n\
         else\n\
         printf 'data: {{\"error\":{{\"message\":\"bad key sk-anth-leak-123\"}}}}\\n'\n\
         printf '\\n__HTTP_STATUS__:401\\n'\n\
         fi\n",
        mode_file.display()
    );
    fs::write(&fake_curl, script).unwrap();
    fs::set_permissions(&fake_curl, fs::Permissions::from_mode(0o755)).unwrap();

    let http_cfg = dir.join("http-harnesses.json");
    fs::write(
        &http_cfg,
        serde_json::to_string(&json!({
            "harnesses": [{
                "id": "anth_gw",
                "endpoint": "https://example.com/v1/messages",
                "api_key_env": "TEST_ANTH_KEY",
                "default_model": "claude-custom-1",
                "models": ["claude-custom-1"],
                "response_format": "anthropic_sse"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    std::env::set_var("CURL_BIN", &fake_curl);
    std::env::set_var("EXT_MODELS_HTTP_HARNESSES_FILE", &http_cfg);
    std::env::set_var("TEST_ANTH_KEY", "sk-anth-leak-123");

    fs::write(&mode_file, "ok").unwrap();
    let (failed_ok, out_ok) = execute_dispatch(
        &json!({
            "model": "claude-custom-1",
            "cwd": dir,
            "project_rules": "off",
            "prompt": "hi"
        }),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(!failed_ok, "auto-routed custom anthropic_sse failed: {out_ok}");
    assert!(out_ok.contains("Hello Anthropic"), "{out_ok}");

    // Test 401 failure + automatic secret redaction in error output!
    let dir2 = dir.join("sub");
    fs::create_dir_all(&dir2).unwrap();
    fs::write(&mode_file, "err").unwrap();
    let (failed_err, out_err) = execute_dispatch(
        &json!({
            "harness": "anth_gw",
            "cwd": dir2,
            "project_rules": "off",
            "prompt": "hi"
        }),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(failed_err, "HTTP 401 must fail: {out_err}");
    assert!(!out_err.contains("sk-anth-leak-123"), "secret in error body must be redacted: {out_err}");
    assert!(out_err.contains("[redacted]"), "{out_err}");

    std::env::remove_var("TEST_ANTH_KEY");
    std::env::remove_var("EXT_MODELS_HTTP_HARNESSES_FILE");
    std::env::remove_var("CURL_BIN");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn http_stream_reports_truncation_and_reserves_space_for_content() {
    use ext_models_mcp::harness::{pi::pi_adapter, HarnessAdapter, ParseState};
    let adapter = pi_adapter();
    let mut state = ParseState::new();
    adapter.on_stdout_line_bounded("data: {\"choices\":", true, &mut state);
    assert!(state.is_error && state.last_message.unwrap().contains("truncated"));

    let mut state = ParseState::new();
    let line = format!(
        "data: {}",
        json!({"choices": [{"delta": {"reasoning_content": "界".repeat(3000)}}]})
    );
    for _ in 0..40 {
        adapter.on_stdout_line(&line, &mut state);
    }
    adapter
        .on_stdout_line(r#"data: {"choices":[{"delta":{"content":"final answer"}}]}"#, &mut state);
    let message = state.last_message.unwrap();
    assert!(message.ends_with("\n</reasoning>\n\nfinal answer"));
    assert!(message.len() <= 128 * 1024 + "final answer".len());
}
