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
    let env_file = dir.join("curl_env.txt");
    let fake_curl = dir.join("fake-curl");

    // Fake curl captures argv, environment, and stdin (-K -), then emits DeepSeek-style SSE + HTTP 200.
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nenv > \"{}\"\ncat > \"{}\"\n\
         printf 'data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"step1 \"}}}}]}}\\n\\n'\n\
         printf 'data: {{\"choices\":[{{\"delta\":{{\"reasoning_content\":\"step2\"}}}}]}}\\n\\n'\n\
         printf 'data: {{\"choices\":[{{\"delta\":{{\"content\":\"Final \"}}}}]}}\\n\\n'\n\
         printf 'data: {{\"choices\":[{{\"delta\":{{\"content\":\"Answer\"}}}}]}}\\n\\n'\n\
         printf 'data: [DONE]\\n\\n'\n\
         printf '\\n__HTTP_STATUS__:200\\n'\n",
        argv_file.display(),
        env_file.display(),
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
    let env_dump = fs::read_to_string(&env_file).unwrap();
    let stdin_dump = fs::read_to_string(&stdin_file).unwrap();
    assert!(!argv_dump.contains(secret), "API key leaked into curl argv!");
    assert!(!env_dump.contains(secret), "API key leaked into curl child env!");
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
