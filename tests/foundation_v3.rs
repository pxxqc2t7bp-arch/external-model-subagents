use ext_models_mcp::error::{record_worker_panic, ErrorKind, RichError};
use ext_models_mcp::harness::catalog::{resolve_route_rich, HarnessInventory};
use ext_models_mcp::harness::codex::CodexAdapter;
use ext_models_mcp::harness::{
    CapabilityMode, HarnessAdapter, ProjectRulesMode, RunRequest, WorkMode,
};
use ext_models_mcp::sandbox::WorktreeSession;
use ext_models_mcp::stream::cleanup_expired_artifacts_in;
#[cfg(unix)]
use ext_models_mcp::harness::execute_dispatch;
#[cfg(unix)]
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, FileTimes};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::Arc;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());
#[cfg(unix)]
use std::thread;
use std::time::{Duration, SystemTime};

#[test]
fn worktree_drop_cleans_up_automatically_on_panic() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("ext-wt-drop-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();

    Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap();
    Command::new("git")
        .args(["config", "user.email", "t@local"])
        .current_dir(&repo)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "t"])
        .current_dir(&repo)
        .status()
        .unwrap();
    fs::write(repo.join("main.rs"), "fn main() {}\n").unwrap();
    Command::new("git").args(["add", "."]).current_dir(&repo).status().unwrap();
    Command::new("git")
        .args(["commit", "-q", "-m", "init"])
        .current_dir(&repo)
        .status()
        .unwrap();

    let call_id = format!("drop_panic_{}", std::process::id());
    let mut captured_wt = PathBuf::new();
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ws = WorktreeSession::create(&repo, &call_id, &AtomicBool::new(false)).unwrap();
        captured_wt = ws.worktree_cwd.clone();
        assert!(captured_wt.exists());
        panic!("simulated worker panic before finalize");
    }));
    assert!(res.is_err());
    assert!(
        !captured_wt.exists(),
        "Drop must remove leaked worktree directory on panic: {}",
        captured_wt.display()
    );

    let wt_list = Command::new("git")
        .args(["worktree", "list"])
        .current_dir(&repo)
        .output()
        .unwrap();
    let list_txt = String::from_utf8_lossy(&wt_list.stdout);
    assert!(
        !list_txt.contains(&call_id),
        "git worktree metadata must be pruned on Drop, got: {list_txt}"
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn artifact_ttl_cleanup_removes_expired_files_only() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("ext-ttl-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let old_file = dir.join("old.md");
    let fresh_file = dir.join("fresh.md");
    let nested_dir = dir.join("wt-active");
    fs::write(&old_file, "expired").unwrap();
    fs::write(&fresh_file, "fresh").unwrap();
    fs::create_dir_all(&nested_dir).unwrap();

    let ten_days_ago = SystemTime::now() - Duration::from_secs(10 * 86_400);
    let f = fs::OpenOptions::new().write(true).open(&old_file).unwrap();
    f.set_times(FileTimes::new().set_modified(ten_days_ago)).unwrap();
    drop(f);

    let removed = cleanup_expired_artifacts_in(&dir, Duration::from_secs(7 * 86_400));
    assert_eq!(removed, 1);
    assert!(!old_file.exists(), "expired artifact must be removed");
    assert!(fresh_file.exists(), "fresh artifact must remain");
    assert!(nested_dir.exists(), "directories must not be touched by file TTL sweep");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn provider_scoped_version_mismatch_allows_openai_switch_and_codex_disables_plugins() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut invs = BTreeMap::new();
    let mut codex_inv = HarnessInventory {
        default_provider: Some("ferry".into()),
        default_model: Some("gpt-6.2-astra".into()),
        auth_mode: "oauth".into(),
        plugins: vec!["slack@openai-curated".into(), "canva@openai-bundled".into()],
        mcps: vec!["larkopen-doc-search".into()],
        ..Default::default()
    };
    codex_inv.providers.insert("openai".into(), Vec::new());
    codex_inv.providers.insert(
        "ferry".into(),
        vec![
            "ferry-gpt-5-6-sol".into(),
            "ferry-gpt-6-2-sol".into(),
            "ferry-gpt-6-astra".into(),
        ],
    );
    invs.insert("codex".into(), codex_inv.clone());

    let avail: BTreeSet<String> = ["codex"].into_iter().map(String::from).collect();

    // Switching explicitly to `provider: "openai"` with `gpt-6.2-sol` or `gpt-6.2-astra` MUST NOT
    // be blocked by `ferry`'s `ferry-gpt-5-6-sol` or `ferry-gpt-6-astra`!
    let r_sol = resolve_route_rich(
        Some("codex"),
        Some("openai"),
        Some("gpt-6.2-sol"),
        false,
        &invs,
        &avail,
    )
    .expect("explicit provider=openai must pass through cleanly without cross-provider mismatch");
    assert_eq!(r_sol.provider.as_deref(), Some("openai"));
    assert_eq!(r_sol.model.as_deref(), Some("gpt-6.2-sol"));

    // Matching `6.2-sol` on default `ferry` resolves `ferry-gpt-6-2-sol` via `ferry` channel prefix!
    let r_ferry =
        resolve_route_rich(Some("codex"), None, Some("gpt-6.2-sol"), false, &invs, &avail).unwrap();
    assert_eq!(r_ferry.model.as_deref(), Some("ferry-gpt-6-2-sol"));

    // Version mismatch WITHIN the same provider still blocks silent downgrades!
    let err = resolve_route_rich(
        Some("codex"),
        Some("ferry"),
        Some("gpt-6.4-sol"),
        false,
        &invs,
        &avail,
    )
    .expect_err("must reject 6.4 -> 6.2 downgrade inside ferry");
    assert_eq!(err.kind, ErrorKind::RoutingMismatch);
    assert!(err.suggestion.is_some());

    // Verify Codex `CapabilityMode::Code` disables plugins so remote OAuth MCP workers never hang!
    let codex = CodexAdapter;
    if codex.is_available() {
        let req = RunRequest {
            call_id: "c_plug".into(),
            prompt: "hi".into(),
            effective_prompt: "hi".into(),
            worker_system_patch: None,
            cwd: PathBuf::from("/tmp"),
            mode: WorkMode::Plan,
            project_rules: ProjectRulesMode::Smart,
            capabilities: CapabilityMode::Code,
            provider: Some("openai".into()),
            model: Some("gpt-6.2-sol".into()),
            effort: None,
            session_id: None,
            keep_session: false,
            review_base: None,
            worktree: false,
            timeout_ms: 30_000,
            idle_timeout_ms: 15_000,
            max_chars: 4000,
        };
        let mut spec = codex.build_base(&req).unwrap();
        codex.apply_headless_guard(&mut spec);
        codex.apply_capabilities(&CapabilityMode::Code, &codex_inv, &req, &mut spec).unwrap();
        assert!(
            spec.args
                .iter()
                .any(|a| a == "plugins.\"slack@openai-curated\".enabled=false"),
            "Code mode must disable configured Codex plugins: {:?}",
            spec.args
        );
    }
}

#[test]
fn rich_error_and_panic_artifact_recording_are_actionable() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let err = RichError::new(ErrorKind::InvalidInput, "bad field")
        .with_suggestion("Use `mode: \"plan\"`");
    let rendered = err.to_string();
    assert!(rendered.contains("[invalid_input] bad field"));
    assert!(rendered.contains("Suggestion: Use `mode: \"plan\"`"));

    let panic_payload: Box<dyn std::any::Any + Send> = Box::new("boom in test worker");
    let p_err = record_worker_panic("test_panic_99", panic_payload.as_ref());
    assert_eq!(p_err.kind, ErrorKind::ExecutionFailed);
    assert!(p_err.message.contains("boom in test worker"));
    let hint = p_err.suggestion.expect("must point to panic log file");
    let log_path = hint.strip_prefix("Inspect panic artifact at ").unwrap();
    let content = fs::read_to_string(log_path).unwrap();
    assert!(content.contains("boom in test worker"));
    let _ = fs::remove_file(log_path);
}

#[cfg(unix)]
#[test]
fn concurrent_dispatch_session_rwlock_has_zero_deadlocks() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("ext-conc-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let bin_dir = base.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();

    let fake_cli = bin_dir.join("fast-mock");
    fs::write(
        &fake_cli,
        "#!/bin/sh\ncat >/dev/null\necho '{\"reply\":\"OK\",\"session_id\":\"mock-session\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&fake_cli, fs::Permissions::from_mode(0o755)).unwrap();

    let cfg = base.join("harnesses.json");
    fs::write(
        &cfg,
        serde_json::to_string(&json!({
            "harnesses": [{
                "id": "fastmock",
                "binary": fake_cli,
                "args": [],
                "plan_flags": [],
                "write_flags": [],
                "full_flags": [],
                "prompt_via_stdin": true,
                "json_result_field": "reply"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let previous = std::env::var_os("EXT_MODELS_HARNESSES_FILE");
    std::env::set_var("EXT_MODELS_HARNESSES_FILE", &cfg);

    let mut handles = Vec::new();
    for i in 0..24 {
        let cwd = base.clone();
        handles.push(thread::spawn(move || {
            let (is_err, out) = execute_dispatch(
                &json!({
                    "harness": "fastmock",
                    "cwd": cwd,
                    "keep_session": true,
                    "session_id": if i % 2 == 0 { "last" } else { "" },
                    "prompt": format!("task {i}")
                }),
                Arc::new(AtomicBool::new(false)),
            );
            assert!(out.contains("session_id=mock-session"), "{out}");
            assert!(!is_err, "concurrent dispatch {i} failed: {out}");
        }));
    }
    let results: Vec<_> = handles.into_iter().map(|h| h.join()).collect();
    match previous {
        Some(value) => std::env::set_var("EXT_MODELS_HARNESSES_FILE", value),
        None => std::env::remove_var("EXT_MODELS_HARNESSES_FILE"),
    }
    for result in results { result.unwrap(); }

    let _ = fs::remove_dir_all(&base);
}
