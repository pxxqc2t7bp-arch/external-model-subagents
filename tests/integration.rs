use ext_models_mcp::alloc_cap::heap_stats_kb;
use ext_models_mcp::harness::catalog::{resolve_route, HarnessInventory};
use ext_models_mcp::harness::claude::ClaudeAdapter;
use ext_models_mcp::harness::codex::CodexAdapter;
use ext_models_mcp::harness::{
    execute_dispatch, CapabilityMode, HarnessAdapter, ProjectRulesMode, RunRequest, WorkMode,
};
use ext_models_mcp::sandbox::{run_supervised, CommandSpec};
use ext_models_mcp::stream::read_bounded_line;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[test]
fn heap_allocator_tracks_live_and_peak_bytes() {
    let (before_kb, _, limit_kb) = heap_stats_kb();
    assert!(limit_kb >= 4096);
    let big = vec![42u8; 512 * 1024];
    std::hint::black_box(&big);
    let (during_kb, peak_kb, _) = heap_stats_kb();
    assert!(during_kb >= before_kb + 400);
    assert!(peak_kb >= during_kb);
    drop(big);
    let (after_kb, _, _) = heap_stats_kb();
    assert!(after_kb < during_kb);
}

#[test]
fn utf8_multibyte_across_tiny_chunks_and_64kb_cap() {
    let repeated = "你好世界🚀Antigravity".repeat(400);
    let second_line = "第二行完整保留";
    let payload = format!("{repeated}\n{second_line}\n");

    // 17-byte BufReader splits 3-byte Chinese and 4-byte emoji sequences constantly.
    let mut reader = BufReader::with_capacity(17, payload.as_bytes());
    let mut scratch = Vec::new();

    let (line1, trunc1) = read_bounded_line(&mut reader, &mut scratch, 1024)
        .unwrap()
        .unwrap();
    assert!(trunc1);
    assert!(line1.len() <= 1024);
    assert!(!line1.contains('\u{FFFD}'), "must never corrupt split UTF-8");

    let (line2, trunc2) = read_bounded_line(&mut reader, &mut scratch, 1024)
        .unwrap()
        .unwrap();
    assert!(!trunc2);
    assert_eq!(line2, second_line);
}

#[test]
fn provider_isolated_and_version_preserving_routing() {
    let mut invs = BTreeMap::new();

    // Codex has both official subscription ("openai") and a custom gateway ("hub_a").
    let mut codex_inv = HarnessInventory {
        default_provider: Some("hub_a".into()),
        default_model: Some("gpt-5.4-pro".into()),
        auth_mode: "oauth".into(),
        ..Default::default()
    };
    codex_inv.providers.insert("openai".into(), vec!["gpt-5.4-mini".into()]);
    codex_inv.providers.insert(
        "hub_a".into(),
        vec![
            "hub_a-gpt-5-4-codex".into(),
            "gpt-5.4-codex".into(),
            "gpt-5.4-pro".into(),
            "gpt-5.2-pro".into(),
        ],
    );
    invs.insert("codex".into(), codex_inv);

    // Claude is in custom gateway mode (NO official subscription, so NO bare `opus`), only has v5.2 pro!
    let mut claude_inv = HarnessInventory {
        default_provider: Some("gateway".into()),
        default_model: Some("claude-gpt-5-3-codex".into()),
        auth_mode: "gateway".into(),
        ..Default::default()
    };
    claude_inv.providers.insert(
        "gateway".into(),
        vec!["claude-gpt-5-4-codex".into(), "claude-gpt-5-2-pro".into()],
    );
    invs.insert("claude".into(), claude_inv);

    // ZCode actually serves `claude-opus-4-6`.
    let mut zcode_inv = HarnessInventory {
        default_provider: Some("zcode".into()),
        auth_mode: "provider_config".into(),
        ..Default::default()
    };
    zcode_inv
        .providers
        .insert("zcode".into(), vec!["claude-opus-4-6".into()]);
    invs.insert("zcode".into(), zcode_inv);

    let avail: BTreeSet<String> = ["codex", "claude", "zcode"]
        .into_iter()
        .map(String::from)
        .collect();

    // 1. Asking for "opus-4-6" without harness must route to zcode, NEVER gateway-only claude!
    let r_opus = resolve_route(None, None, Some("opus-4-6"), false, &invs, &avail).unwrap();
    assert_eq!(r_opus.harness, "zcode");
    assert_eq!(r_opus.model.as_deref(), Some("claude-opus-4-6"));

    // 2. Shorthand "5.4codex" normalizes accurately per chosen harness!
    let r_c_codex =
        resolve_route(Some("claude"), None, Some("5.4codex"), false, &invs, &avail).unwrap();
    assert_eq!(r_c_codex.model.as_deref(), Some("claude-gpt-5-4-codex"));
    let r_x_codex =
        resolve_route(Some("codex"), None, Some("5.4codex"), false, &invs, &avail).unwrap();
    assert!(
        matches!(
            r_x_codex.model.as_deref(),
            Some("gpt-5.4-codex") | Some("hub_a-gpt-5-4-codex")
        )
    );

    // 3. Refuses silent version downgrade (`5.4-pro` on Claude which only has `5.2-pro` MUST error!).
    let err = resolve_route(Some("claude"), None, Some("5.4-pro"), false, &invs, &avail)
        .expect_err("must reject 5.4 -> 5.2 downgrade");
    assert!(err.contains("refusing silent version downgrade"));
    assert!(err.contains("codex"));

    // 4. `<provider>/<model>` shorthand switches Codex to official subscription ("openai").
    let r_sub =
        resolve_route(Some("codex"), None, Some("openai/gpt-5.4-mini"), false, &invs, &avail)
            .unwrap();
    assert_eq!(r_sub.provider.as_deref(), Some("openai"));
    assert_eq!(r_sub.model.as_deref(), Some("gpt-5.4-mini"));

    // 5. Uncached enterprise gateway model passes through cleanly when harness is specified.
    let r_pt = resolve_route(
        Some("codex"),
        Some("litellm"),
        Some("corp-qwen3-coder"),
        false,
        &invs,
        &avail,
    )
    .unwrap();
    assert_eq!(r_pt.provider.as_deref(), Some("litellm"));
    assert_eq!(r_pt.model.as_deref(), Some("corp-qwen3-coder"));
}

#[test]
fn three_axes_are_strictly_orthogonal() {
    let dummy_req = RunRequest {
        call_id: "t1".into(),
        prompt: "hi".into(),
        effective_prompt: "hi".into(),
        worker_system_patch: Some("PATCH".into()),
        cwd: PathBuf::from("/tmp"),
        mode: WorkMode::Plan,
        project_rules: ProjectRulesMode::Off,
        capabilities: CapabilityMode::Full,
        provider: None,
        model: None,
        effort: None,
        session_id: None,
        keep_session: false,
        review_base: None,
        worktree: false,
        timeout_ms: 30_000,
        idle_timeout_ms: 15_000,
        max_chars: 4000,
    };

    // Verify Claude `project_rules = Off` + `capabilities = Full` uses surgical env var, NOT `--setting-sources user`.
    let claude = ClaudeAdapter;
    if claude.is_available() {
        let mut spec = claude.build_base(&dummy_req).unwrap();
        claude.apply_headless_guard(&mut spec);
        claude.apply_project_rules(ProjectRulesMode::Off, &dummy_req, &mut spec);
        claude.apply_capabilities(
            &CapabilityMode::Full,
            &HarnessInventory::default(),
            PathBuf::from("/tmp").as_path(),
            &mut spec,
        );
        assert!(spec
            .envs
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_DISABLE_CLAUDE_MDS" && v == "1"));
        assert!(!spec.args.iter().any(|a| a == "--setting-sources"));
        assert!(spec.args.iter().any(|a| a == "--mcp-config"));
    }

    // Verify Codex `project_rules = Off` + `capabilities = Selective(["mcp_keep"])` stay independent.
    let codex = CodexAdapter;
    if codex.is_available() {
        let inv = HarnessInventory {
            mcps: vec!["mcp_keep".into(), "mcp_drop".into()],
            ..Default::default()
        };
        let mut spec = codex.build_base(&dummy_req).unwrap();
        codex.apply_headless_guard(&mut spec);
        codex.apply_project_rules(ProjectRulesMode::Off, &dummy_req, &mut spec);
        codex.apply_capabilities(
            &CapabilityMode::Selective(vec!["mcp_keep".into()]),
            &inv,
            PathBuf::from("/tmp").as_path(),
            &mut spec,
        );
        assert!(spec.args.iter().any(|a| a == "project_doc_max_bytes=0"));
        assert!(spec.args.iter().any(|a| a == "mcp_servers.mcp_drop.enabled=false"));
        assert!(!spec.args.iter().any(|a| a == "mcp_servers.mcp_keep.enabled=false"));
    }
}

#[test]
fn cancellation_reaps_entire_process_group_and_drains_stderr() {
    let tmp = std::env::temp_dir().join(format!("ext-pgid-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    let pid_file = tmp.join("grandchild.pid");

    let script = format!(
        "sleep 60 &\necho $! > \"{}\"\nhead -c 65536 /dev/zero | tr '\\0' 'E' >&2\nsleep 60\n",
        pid_file.display()
    );
    let spec = CommandSpec {
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script],
        envs: Vec::new(),
        env_remove: Vec::new(),
        stdin_data: None,
        cwd: tmp.clone(),
        output_file: None,
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_trigger = Arc::clone(&cancel);
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        cancel_trigger.store(true, Ordering::Release);
    });

    let outcome = run_supervised(
        &spec,
        Duration::from_secs(30),
        Duration::from_secs(30),
        cancel,
        |_, _| {},
    )
    .unwrap();

    assert!(outcome.cancelled);
    assert!(!outcome.stderr_tail.is_empty());

    if let Ok(raw_pid) = fs::read_to_string(&pid_file) {
        if let Ok(gpid) = raw_pid.trim().parse::<i32>() {
            thread::sleep(Duration::from_millis(120));
            let alive = unsafe { libc::kill(gpid, 0) == 0 };
            assert!(!alive, "grandchild process {gpid} must be reaped with process group");
        }
    }
    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn custom_harness_dirty_worktree_patch_and_zero_copy_handoff() {
    let base = std::env::temp_dir().join(format!("ext-wt-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let repo = base.join("repo");
    let bin_dir = base.join("bin");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();

    // Initialize git repo with committed + uncommitted modified + untracked files.
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
    fs::write(repo.join("lib.rs"), "pub fn add() -> i32 { 1 }\n").unwrap();
    Command::new("git").args(["add", "."]).current_dir(&repo).status().unwrap();
    Command::new("git")
        .args(["commit", "-q", "-m", "init"])
        .current_dir(&repo)
        .status()
        .unwrap();

    // Dirty tracked edit + pre-existing untracked file in main workspace:
    fs::write(repo.join("lib.rs"), "pub fn add() -> i32 { 2 }\n").unwrap();
    fs::write(repo.join("notes.txt"), "untracked local note\n").unwrap();

    // Fake custom CLI harness that reads stdin and edits `lib.rs` when in write mode.
    let fake_cli = bin_dir.join("fake-agent");
    fs::write(
        &fake_cli,
        "#!/bin/sh\ninput=$(cat)\nif [ \"$1\" = \"--write\" ]; then\n  echo 'pub fn add() -> i32 { 42 }' > lib.rs\n  echo '{\"reply\":\"Refactored lib.rs to return 42\"}'\nelse\n  printf '{\"reply\":\"Received handoff len=%s\"}\\n' \"${#input}\"\nfi\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&fake_cli).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake_cli, perms).unwrap();

    let harnesses_cfg = base.join("harnesses.json");
    fs::write(
        &harnesses_cfg,
        serde_json::to_string(&json!({
            "harnesses": [{
                "id": "fake",
                "binary": "fake-agent",
                "strengths": "Test harness",
                "args": [],
                "plan_flags": ["--plan"],
                "write_flags": ["--write"],
                "full_flags": ["--write"],
                "prompt_via_stdin": true,
                "json_result_field": "reply"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let prev_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{prev_path}", bin_dir.display()));
    std::env::set_var("EXT_MODELS_HARNESSES_FILE", &harnesses_cfg);

    // Run 1: isolated worktree write -> main repo must stay `2`, patch must turn `2 -> 42` cleanly!
    let (err1, out1) = execute_dispatch(
        &json!({
            "harness": "fake",
            "cwd": repo,
            "mode": "write",
            "worktree": true,
            "prompt": "Update add() to 42"
        }),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(!err1, "dispatch 1 failed: {out1}");
    assert!(out1.contains("Refactored lib.rs to return 42"));
    assert!(out1.contains("patch ready:"));
    assert_eq!(
        fs::read_to_string(repo.join("lib.rs")).unwrap(),
        "pub fn add() -> i32 { 2 }\n",
        "main working tree must remain untouched during worktree run"
    );

    // Extract call_id (`id=c...`) and test zero-copy handoff into Run 2!
    let call_id = out1
        .split("id=")
        .nth(1)
        .unwrap()
        .split([' ', '|', ']'])
        .next()
        .unwrap()
        .to_string();

    let (err2, out2) = execute_dispatch(
        &json!({
            "harness": "fake",
            "cwd": repo,
            "mode": "plan",
            "handoff_from": call_id,
            "prompt": "Review previous change"
        }),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(!err2, "dispatch 2 failed: {out2}");
    assert!(out2.contains("Received handoff len="));

    let _ = fs::remove_dir_all(&base);
}
