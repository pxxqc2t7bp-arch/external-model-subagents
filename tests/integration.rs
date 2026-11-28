use ext_models_mcp::alloc_cap::heap_stats_kb;
use ext_models_mcp::harness::catalog::{resolve_route, HarnessInventory};
use ext_models_mcp::harness::claude::ClaudeAdapter;
use ext_models_mcp::harness::codex::CodexAdapter;
use ext_models_mcp::harness::{
    execute_dispatch, CapabilityMode, HarnessAdapter, ProjectRulesMode, RunRequest,
    WorkMode,
};
use ext_models_mcp::sandbox::WorktreeSession;
#[cfg(unix)]
use ext_models_mcp::harness::ParseState;
#[cfg(unix)]
use ext_models_mcp::sandbox::{run_supervised, CommandSpec};
use ext_models_mcp::stream::{next_call_id, read_bounded_line, TailBuffer};
#[cfg(unix)]
use ext_models_mcp::stream::artifact_dir;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::atomic::Ordering;
use std::sync::Arc;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
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

    // Exact-boundary regression: a line of EXACTLY `max_bytes` whose `\n` arrives in the next
    // BufRead chunk must NOT be falsely flagged as truncated!
    let exact_line = "X".repeat(1024);
    let exact_payload = format!("{exact_line}\nnext\n");
    let mut exact_reader = BufReader::with_capacity(512, exact_payload.as_bytes());
    let (exact_got, exact_trunc) = read_bounded_line(&mut exact_reader, &mut scratch, 1024)
        .unwrap()
        .unwrap();
    assert!(
        !exact_trunc,
        "line of exact max_bytes length must not be marked truncated when newline arrives separately"
    );
    assert_eq!(exact_got.len(), 1024);
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
        call_id: next_call_id(),
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
            &dummy_req,
            &mut spec,
        ).unwrap();
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
            &dummy_req,
            &mut spec,
        ).unwrap();
        assert!(spec.args.iter().any(|a| a == "project_doc_max_bytes=0"));
        assert!(
            !spec.args.iter().any(|a| a == "--ignore-rules"),
            "project_rules=off must not disable Codex execpolicy .rules files"
        );
        assert!(spec.args.iter().any(|a| a == "mcp_servers.mcp_drop.enabled=false"));
        assert!(!spec.args.iter().any(|a| a == "mcp_servers.mcp_keep.enabled=false"));
    }
}

#[cfg(unix)]
#[test]
fn cancellation_reaps_entire_process_group_and_drains_stderr() {
    let tmp = std::env::temp_dir().join(format!("ext-pgid-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    let pid_file = tmp.join("grandchild.pid");

    let script = format!(
        "(trap '' TERM; exec sleep 10) >/dev/null &\necho $! > \"{}\"\nhead -c 65536 /dev/zero | tr '\\0' 'E' >&2\nsleep 60\n",
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
        temp_files: Vec::new(),
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
    assert!(outcome.duration_ms < 8_000, "watchdog failed to close inherited stderr");
    assert!(!outcome.stderr_tail.is_empty());

    let raw_pid = fs::read_to_string(&pid_file).expect("grandchild pid file must be written");
    let gpid: i32 = raw_pid
        .trim()
        .parse()
        .expect("grandchild pid must parse");
    thread::sleep(Duration::from_millis(120));
    let alive = unsafe { libc::kill(gpid, 0) == 0 };
    assert!(
        !alive,
        "grandchild process {gpid} must be reaped with process group"
    );
    let _ = fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
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
    assert!(Command::new("git").args(["config", "color.ui", "always"])
        .current_dir(&repo).status().unwrap().success());
    // Ensure patch export remains `git apply`-compatible even when user sets `diff.noprefix=true`!
    Command::new("git")
        .args(["config", "diff.noprefix", "true"])
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
                "binary": fake_cli,
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

    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_ext-models-mcp"))
            .args(["run", "-H", "fake"]).args(args).current_dir(&repo)
            .env("EXT_MODELS_HARNESSES_FILE", &harnesses_cfg).output().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{stdout}");
        stdout
    };
    let out1 = run(&["--mode", "write", "--worktree", "Update add() to 42"]);
    assert!(out1.contains("Refactored lib.rs to return 42"));
    assert!(out1.contains("patch ready:"));
    assert_eq!(
        fs::read_to_string(repo.join("lib.rs")).unwrap(),
        "pub fn add() -> i32 { 2 }\n",
        "main working tree must remain untouched during worktree run"
    );

    // Extract patch path and verify `git apply` cleanly updates the dirty main workspace!
    let patch_path = out1
        .split("`git apply ")
        .nth(1)
        .unwrap()
        .split('`')
        .next()
        .unwrap();
    let apply_status = Command::new("git")
        .args(["apply", patch_path])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(apply_status.success(), "git apply must succeed cleanly");
    assert_eq!(
        fs::read_to_string(repo.join("lib.rs")).unwrap(),
        "pub fn add() -> i32 { 42 }\n"
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

    let out2 = run(&["--mode", "plan", "--handoff", &call_id, "Review previous change"]);
    let len_str = out2
        .split("Received handoff len=")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim();
    let handoff_len: usize = len_str.parse().expect("handoff length must be numeric");
    assert!(
        handoff_len >= 60,
        "expected full handoff payload + prompt, got len={handoff_len}"
    );

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn short_timeout_ms_does_not_panic_and_mcp_server_survives() {
    let bin = env!("CARGO_BIN_EXE_ext-models-mcp");
    let mut child = Command::new(bin)
        .arg("mcp")
        .env("EXT_MODELS_MAX_HEAP_MB", "4")
        .env("EXT_MODELS_MAX_JOBS", "4")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    {
        let sin = child.stdin.as_mut().unwrap();
        for (req_id, timeout_ms) in [(1, 5_000u64), (2, 6_000), (3, 9_999), (4, 10_000)] {
            let msg = json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "tools/call",
                "params": {
                    "name": "dispatch",
                    "arguments": {
                        "prompt": "short timeout check",
                        "harness": "nonexistent_harness",
                        "timeout_ms": timeout_ms
                    }
                }
            });
            writeln!(sin, "{msg}").unwrap();
        }
        writeln!(
            sin,
            "{}",
            json!({ "jsonrpc": "2.0", "id": 99, "method": "ping" })
        )
        .unwrap();
    }

    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "MCP server crashed on short timeout_ms: status={}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let frames: Vec<Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    assert!(
        frames.iter().any(|f| f.get("id") == Some(&json!(99)) && f.get("result").is_some()),
        "MCP server must still respond to ping after short-timeout calls; got: {stdout}"
    );
    for id in 1..=4 {
        assert!(
            frames.iter().any(|f| f.get("id") == Some(&json!(id))),
            "missing RPC response for id={id} in {stdout}"
        );
    }
}

#[cfg(unix)]
#[test]
fn codex_review_avoids_flag_conflict_and_rules_off_preserves_execpolicy() {
    let tmp = std::env::temp_dir().join(format!("ext-codex-rev-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    Command::new("git").args(["init", "-q"]).current_dir(&tmp).status().unwrap();

    let argv_log = tmp.join("argv.txt");
    let stdin_log = tmp.join("stdin.txt");
    let fake_codex = tmp.join("fake-codex");
    // Size > 0 is fine; unwrap_native_codex falls back to original path when not inside node_modules.
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\ncat > \"{}\"\necho '{{\"type\":\"thread.started\",\"thread_id\":\"t-rev\"}}'\necho '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"REVIEW_OK\"}}}}'\n",
        argv_log.display(),
        stdin_log.display()
    );
    fs::write(&fake_codex, script).unwrap();
    fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o755)).unwrap();

    let bin = env!("CARGO_BIN_EXE_ext-models-mcp");

    // 1. Custom prompt + `--review main --rules off`: must pass `review -` (NO `--base`, NO `--ignore-rules`).
    let out1 = Command::new(bin)
        .args([
            "run",
            "-H",
            "codex",
            "-P",
            "gw\"\u{0008}\\x",
            "-C",
            &tmp.to_string_lossy(),
            "--review",
            "main",
            "--rules",
            "off",
            "Focus on concurrency bugs",
        ])
        .env("CODEX_BIN", &fake_codex)
        .output()
        .unwrap();
    assert!(
        out1.status.success(),
        "codex review with prompt failed: {}",
        String::from_utf8_lossy(&out1.stdout)
    );
    let args1: Vec<String> = fs::read_to_string(&argv_log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert!(args1.contains(&"review".to_string()));
    assert!(args1.contains(&"-".to_string()));
    assert!(
        args1.contains(&"model_provider=\"gw\\\"\\b\\\\x\"".to_string()),
        "TOML basic string must escape quotes, backspaces, and backslashes: {args1:?}"
    );
    assert!(
        !args1.contains(&"--base".to_string()),
        "must not combine --base with stdin '-' prompt: {args1:?}"
    );
    assert!(args1.contains(&"project_doc_max_bytes=0".to_string()));
    assert!(
        !args1.contains(&"--ignore-rules".to_string()),
        "must not pass --ignore-rules when project_rules=off: {args1:?}"
    );
    let sin1 = fs::read_to_string(&stdin_log).unwrap();
    assert!(sin1.contains("Review Scope:") && sin1.contains("main"));
    assert!(sin1.contains("Focus on concurrency bugs"));

    // 2. Empty prompt + `--review uncommitted` (with stdin closed via /dev/null): must pass `review --uncommitted` and NO `-`.
    let out2 = Command::new(bin)
        .args([
            "run",
            "-H",
            "codex",
            "-C",
            &tmp.to_string_lossy(),
            "--review",
            "uncommitted",
        ])
        .env("CODEX_BIN", &fake_codex)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // Note: when stdin is `/dev/null` (not a terminal), `cmd_run` reads 0 bytes -> empty prompt!
    assert!(
        out2.status.success(),
        "codex native uncommitted review failed: {}",
        String::from_utf8_lossy(&out2.stdout)
    );
    let args2: Vec<String> = fs::read_to_string(&argv_log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert!(args2.contains(&"review".to_string()));
    assert!(args2.contains(&"--uncommitted".to_string()));
    assert!(
        !args2.contains(&"-".to_string()),
        "native --uncommitted review without custom prompt must not pass '-': {args2:?}"
    );

    // 3. `worktree: true` + `review_base: "uncommitted"` must be rejected explicitly.
    let out3 = Command::new(bin)
        .args([
            "run",
            "-H",
            "codex",
            "-C",
            &tmp.to_string_lossy(),
            "-w",
            "--review",
            "uncommitted",
            "check",
        ])
        .env("CODEX_BIN", &fake_codex)
        .output()
        .unwrap();
    assert!(!out3.status.success());
    assert!(String::from_utf8_lossy(&out3.stdout).contains("cannot be combined"));

    let _ = fs::remove_dir_all(&tmp);
}

#[cfg(unix)]
#[test]
fn worktree_subdir_canonicalization_commits_binary_unicode_and_export_failure_preserves() {
    let base = std::env::temp_dir().join(format!("ext-wt-p0-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let repo = base.join("repo");
    let sub = repo.join("crates").join("sub");
    let bin_dir = base.join("bin");
    fs::create_dir_all(&sub).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    let outside_secret = base.join("outside-secret.txt");
    fs::write(&outside_secret, "TOP_SECRET_OUTSIDE_REPO").unwrap();

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
    assert!(Command::new("git").args(["config", "color.ui", "always"])
        .current_dir(&repo).status().unwrap().success());
    // Ensure patch export remains `git apply`-compatible even when user sets `diff.noprefix=true`!
    Command::new("git")
        .args(["config", "diff.noprefix", "true"])
        .current_dir(&repo)
        .status()
        .unwrap();
    fs::write(sub.join("lib.rs"), "pub fn v() -> i32 { 1 }\n").unwrap();
    Command::new("git").args(["add", "."]).current_dir(&repo).status().unwrap();
    Command::new("git")
        .args(["commit", "-q", "-m", "init"])
        .current_dir(&repo)
        .status()
        .unwrap();

    // Install a repository post-checkout hook that MUST NOT execute during hub worktree setup.
    let hooks_dir = repo.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let hook_marker = base.join("hook-ran.marker");
    let post_checkout = hooks_dir.join("post-checkout");
    fs::write(
        &post_checkout,
        format!("#!/bin/sh\necho RAN > \"{}\"\n", hook_marker.display()),
    )
    .unwrap();
    fs::set_permissions(&post_checkout, fs::Permissions::from_mode(0o755)).unwrap();

    // Dirty tracked edit + non-ASCII untracked file + external symlink:
    fs::write(sub.join("lib.rs"), "pub fn v() -> i32 { 2 }\n").unwrap();
    fs::write(repo.join("新文件.txt"), "中文内容\n").unwrap();
    symlink(&outside_secret, repo.join("ext-link.txt")).unwrap();
    let index_before = fs::read(repo.join(".git").join("index")).unwrap();

    let pwd_record = base.join("worker-pwd.txt");
    let checks_record = base.join("worker-checks.txt");
    let fake_worker = bin_dir.join("wt-worker");
    let worker_script = format!(
        "#!/bin/sh\ncat >/dev/null\npwd > \"{}\"\nif [ -f ../../新文件.txt ] && [ ! -e ../../ext-link.txt ]; then\n  echo CHECKS_OK > \"{}\"\nfi\nprintf 'pub fn v() -> i32 {{ 99 }}\\n' > lib.rs\nprintf '\\000\\001\\376\\377' > blob.bin\ngit add -A\ngit -c user.name=w -c user.email=w@local commit -q --no-verify -m 'worker commit'\nprintf 'after commit\\n' > extra.txt\necho '{{\"reply\":\"WORKTREE_DONE\"}}'\n",
        pwd_record.display(),
        checks_record.display()
    );
    fs::write(&fake_worker, worker_script).unwrap();
    fs::set_permissions(&fake_worker, fs::Permissions::from_mode(0o755)).unwrap();

    let harnesses_cfg = base.join("harnesses.json");
    fs::write(
        &harnesses_cfg,
        serde_json::to_string(&json!({
            "harnesses": [{
                "id": "wtw",
                "binary": "wt-worker",
                "args": [],
                "plan_flags": [],
                "write_flags": ["--write"],
                "full_flags": ["--write"],
                "prompt_via_stdin": true,
                "json_result_field": "reply"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    // Pass non-canonical cwd containing `..` (and `/tmp` symlink on macOS):
    let non_canon_cwd = repo.join("crates").join("sub").join("..").join("sub");
    let bin = env!("CARGO_BIN_EXE_ext-models-mcp");
    let prev_path = std::env::var("PATH").unwrap_or_default();
    let run_out = Command::new(bin)
        .args([
            "run",
            "-H",
            "wtw",
            "-C",
            &non_canon_cwd.to_string_lossy(),
            "--mode",
            "write",
            "-w",
            "Execute subdir commit + binary test",
        ])
        .env("PATH", format!("{}:{prev_path}", bin_dir.display()))
        .env("EXT_MODELS_HARNESSES_FILE", &harnesses_cfg)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&run_out.stdout);
    assert!(run_out.status.success(), "worktree run failed: {stdout}");
    let index_after = fs::read(repo.join(".git").join("index")).unwrap();
    assert_eq!(
        index_before, index_after,
        "source .git/index must remain 100% byte-for-byte untouched during worktree run"
    );
    assert!(
        !hook_marker.exists(),
        "hub git worktree setup must disable repository hooks"
    );
    let recorded_pwd = fs::read_to_string(&pwd_record).unwrap();
    assert!(
        recorded_pwd.trim().ends_with("/crates/sub"),
        "worker must run inside mapped subdir crates/sub, got: {recorded_pwd}"
    );
    assert_eq!(
        fs::read_to_string(&checks_record).unwrap_or_default().trim(),
        "CHECKS_OK",
        "non-ASCII untracked file must be copied and external symlink must be excluded"
    );
    assert!(
        stdout.contains("skipped 1 untracked"),
        "expected skipped symlink note in output: {stdout}"
    );
    assert!(
        stdout.contains("patch ready:"),
        "worker committed + unstaged changes must produce patch: {stdout}"
    );

    let patch_path = stdout
        .split("`git apply ")
        .nth(1)
        .unwrap()
        .split('`')
        .next()
        .unwrap();
    let apply = Command::new("git")
        .args(["apply", patch_path])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "git apply failed: {}",
        String::from_utf8_lossy(&apply.stderr)
    );
    assert_eq!(
        fs::read_to_string(sub.join("lib.rs")).unwrap(),
        "pub fn v() -> i32 { 99 }\n"
    );
    assert_eq!(
        fs::read(sub.join("blob.bin")).unwrap(),
        vec![0x00, 0x01, 0xfe, 0xff]
    );
    assert_eq!(
        fs::read_to_string(sub.join("extra.txt")).unwrap(),
        "after commit\n"
    );

    // Verify patch export failure (e.g. O_NOFOLLOW rejecting a symlink at `<call_id>.patch`)
    // preserves the worktree checkout on disk instead of deleting worker edits!
    let fail_id = format!("wt_preserve_{}", std::process::id());
    let ws = WorktreeSession::create(&repo, &fail_id, &AtomicBool::new(false)).unwrap();
    fs::write(ws.worktree_cwd.join("precious.txt"), "DO_NOT_LOSE_ME\n").unwrap();
    let poison_patch = artifact_dir().unwrap().join(format!("{fail_id}.patch"));
    let _ = fs::remove_file(&poison_patch);
    symlink(&outside_secret, &poison_patch).unwrap();

    let summary = ws.finalize();
    let _ = fs::remove_file(&poison_patch);
    assert!(summary.export_error.is_some(), "expected O_NOFOLLOW export failure");
    let preserved = summary
        .preserved_worktree
        .expect("must preserve worktree path on export failure");
    assert_eq!(
        fs::read_to_string(preserved.join("precious.txt")).unwrap(),
        "DO_NOT_LOSE_ME\n"
    );
    // Clean up preserved test worktree
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force", &preserved.to_string_lossy()])
        .current_dir(&repo)
        .status();
    let _ = fs::remove_dir_all(&preserved);

    // Trailing-space repo root + 4 MiB heap + 7 MiB worker diff streaming test (zero OOM abort):
    let space_repo = base.join("repo with trailing space ");
    fs::create_dir_all(&space_repo).unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(&space_repo)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "t@local"])
        .current_dir(&space_repo)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "t"])
        .current_dir(&space_repo)
        .status()
        .unwrap();
    fs::write(space_repo.join("seed.txt"), "seed\n").unwrap();
    Command::new("git")
        .args(["add", "."])
        .current_dir(&space_repo)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-q", "-m", "init"])
        .current_dir(&space_repo)
        .status()
        .unwrap();

    let big_worker = bin_dir.join("big-wt-worker");
    fs::write(
        &big_worker,
        "#!/bin/sh\ncat >/dev/null\nhead -c 7000000 /dev/zero | tr '\\0' 'Z' > huge.txt\necho '{\"reply\":\"BIG_PATCH_OK\"}'\n",
    )
    .unwrap();
    fs::set_permissions(&big_worker, fs::Permissions::from_mode(0o755)).unwrap();
    let big_cfg = base.join("big-harnesses.json");
    fs::write(
        &big_cfg,
        serde_json::to_string(&json!({
            "harnesses": [{
                "id": "bigw",
                "binary": "big-wt-worker",
                "args": [],
                "plan_flags": [],
                "write_flags": ["--write"],
                "full_flags": ["--write"],
                "prompt_via_stdin": true,
                "json_result_field": "reply"
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let big_out = Command::new(bin)
        .args([
            "run",
            "-H",
            "bigw",
            "-C",
            &space_repo.to_string_lossy(),
            "--mode",
            "write",
            "-w",
            "Generate 7MB diff under 4MB heap ceiling",
        ])
        .env("PATH", format!("{}:{prev_path}", bin_dir.display()))
        .env("EXT_MODELS_HARNESSES_FILE", &big_cfg)
        .env("EXT_MODELS_MAX_HEAP_MB", "4")
        .output()
        .unwrap();
    let big_stdout = String::from_utf8_lossy(&big_out.stdout);
    assert!(
        big_out.status.success(),
        "7MB patch export under 4MB heap cap must stream without OOM abort: status={}, out={big_stdout}",
        big_out.status
    );
    let big_patch = big_stdout
        .split("`git apply ")
        .nth(1)
        .unwrap()
        .split('`')
        .next()
        .unwrap();
    assert!(fs::metadata(big_patch).unwrap().len() >= 7_000_000);
    let _ = fs::remove_file(big_patch);

    let _ = fs::remove_dir_all(&base);
}

#[cfg(unix)]
#[test]
fn claude_large_75kb_single_line_json_and_truncation_guard() {
    let tmp = std::env::temp_dir().join(format!("ext-claude-75k-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let payload_text = "R".repeat(75_000);
    let json_line = serde_json::to_string(&json!({
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "session_id": "sess-75k-ok",
        "result": payload_text,
    }))
    .unwrap();
    let resp_file = tmp.join("resp.json");
    fs::write(&resp_file, format!("{json_line}\n")).unwrap();

    let fake_claude = tmp.join("fake-claude");
    fs::write(
        &fake_claude,
        format!("#!/bin/sh\ncat >/dev/null\ncat \"{}\"\n", resp_file.display()),
    )
    .unwrap();
    fs::set_permissions(&fake_claude, fs::Permissions::from_mode(0o755)).unwrap();

    let bin = env!("CARGO_BIN_EXE_ext-models-mcp");
    let out = Command::new(bin)
        .args([
            "run",
            "-H",
            "claude",
            "-C",
            &tmp.to_string_lossy(),
            "--keep-session",
            "Give 75KB response",
        ])
        .env("CLAUDE_BIN", &fake_claude)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "75KB claude run failed: {stdout}");
    assert!(
        stdout.contains("session_id=sess-75k-ok"),
        "must preserve session_id on >64KB claude JSON output: {stdout}"
    );
    let artifact_path = stdout
        .split("artifact=")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    let saved_artifact = fs::read_to_string(artifact_path).unwrap();
    assert_eq!(
        saved_artifact.len(),
        75_000,
        "full 75,000-byte result must be saved intact without 32KB TailBuffer corruption"
    );

    // Also verify truncation guard marks truncated JSON lines as error instead of silent success.
    let claude = ClaudeAdapter;
    let mut state = ParseState::new();
    claude.on_stdout_line_bounded("{\"type\":\"result\",\"result\":\"cut", true, &mut state);
    assert!(
        state.is_error,
        "truncated claude JSON line must set is_error=true"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn oversized_line_keeps_previous_diagnostics() {
    let mut tail = TailBuffer::new(257);
    tail.push_line("first diagnostic");
    tail.push_line(&"界".repeat(200));
    assert_eq!(tail.as_str(), format!("first diagnostic\n{}", "界".repeat(42)));
}

#[test]
fn worktree_requires_initial_commit() {
    let repo = std::env::temp_dir().join(next_call_id());
    assert!(Command::new("git").args(["init", "-q"]).arg(&repo).status().unwrap().success());
    let error =
        WorktreeSession::create(&repo, &next_call_id(), &AtomicBool::new(false)).err().unwrap();
    fs::remove_dir_all(repo).unwrap();
    assert!(error.contains("HEAD is unborn") && error.contains("initial commit"), "{error}");
}

#[test]
fn slash_model_ids_take_precedence_over_provider_shorthand() {
    let mut inv = HarnessInventory::default();
    inv.providers.insert("deepseek".into(), vec!["deepseek-r1".into()]);
    inv.providers
        .insert("gateway".into(), vec!["openai/gpt-5.4".into(), "deepseek/deepseek-r1".into()]);
    let inventories = [("codex".into(), inv)].into();
    let available = ["codex".into()].into();
    for harness in [None, Some("codex")] {
        for model in ["openai/gpt-5.4", "deepseek/deepseek-r1"] {
            let route =
                resolve_route(harness, None, Some(model), false, &inventories, &available).unwrap();
            assert_eq!(route.model.as_deref(), Some(model));
            assert_eq!(route.provider.as_deref(), Some("gateway"));
        }
    }
}

#[test]
fn dispatch_rejects_invalid_policy_and_target_fields() {
    for (field, value, message) in [
        ("worktree", json!("true"), "boolean"),
        ("swarm", json!([]), "object"),
        ("targets", json!(["codex"]), "objects"),
        ("targets", json!([{}, {}]), "single-target"),
    ] {
        let mut args = json!({"prompt": "test", "session_id": "last"});
        args[field] = value;
        let (failed, out) = execute_dispatch(&args, Arc::new(AtomicBool::new(false)));
        assert!(failed && out.contains("[invalid_input]") && out.contains(message), "{out}");
    }
}

#[test]
fn binary_search_returns_absolute_paths_and_prefers_override() {
    use ext_models_mcp::harness::discovery::find_binary_in;
    let executable = std::env::current_exe().unwrap();
    let dir = executable.parent().unwrap().to_owned();
    let name = executable.file_name().unwrap().to_str().unwrap();
    assert_eq!(find_binary_in(name, None, &[dir]), Some(executable.clone()));
    assert_eq!(find_binary_in("missing", Some(&executable), &[]), Some(executable));
    let relative = PathBuf::from("scripts/install.sh");
    let found = find_binary_in("missing", Some(&relative), &[]);
    assert_eq!(found, Some(std::env::current_dir().unwrap().join(relative)));
    assert_eq!(find_binary_in("missing", None, &[]), None);
}

#[test]
fn cli_help_and_invalid_flags() {
    let bin = env!("CARGO_BIN_EXE_ext-models-mcp");
    let help = Command::new(bin).arg("--help").output().unwrap();
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    for flag in ["--effort", "--file", "--timeout-ms", "--max-chars"] {
        assert!(text.contains(flag));
    }
    for args in [&["--timeout-ms", "bad"][..], &["--max-chars", "bad"], &["--file"], &["--unknown"]]
    {
        let result = Command::new(bin).arg("run").args(args).output().unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8(result.stderr).unwrap().contains(args[0]));
    }
}

#[cfg(unix)]
#[test]
fn claude_project_mcp_credentials_use_a_temporary_private_file() {
    let dir = std::env::temp_dir().join(next_call_id());
    fs::create_dir_all(dir.join(".claude")).unwrap();
    let settings = json!({"mcpServers":{"project-mcp":{"headers":{"key":"fixture-secret"}}}});
    fs::write(dir.join(".claude/settings.json"), settings.to_string()).unwrap();
    let fake = dir.join("fake-claude");
    fs::write(
        &fake,
        r#"#!/bin/sh
set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        *fixture-secret*) exit 9 ;;
        --mcp-config) shift; cfg=$1 ;;
    esac
    shift
done
[ -n "$(find "$cfg" -type f -perm 600 -print)" ]
grep -q fixture-secret "$cfg"
printf '%s' "$cfg" > config-path
cat >/dev/null
echo '{"result":"ok"}'
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_ext-models-mcp"))
            .args(args)
            .current_dir(&dir)
            .env("HOME", &dir)
            .env("CLAUDE_CONFIG_DIR", dir.join("config"))
            .env("CLAUDE_BIN", &fake)
            .output()
            .unwrap()
    };
    let status = run(&["status"]);
    let report: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(report["harnesses"].as_array().unwrap().iter().any(|h| h["harness"] == "claude"
        && h["mcps"].as_array().unwrap().contains(&json!("project-mcp"))));
    for args in [["--mcp", "project-mcp"], ["--caps", "full"]] {
        let result = run(&["run", "-H", "claude", args[0], args[1], "test"]);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stdout));
        let path = fs::read_to_string(dir.join("config-path")).unwrap();
        assert!(!PathBuf::from(path).exists());
    }
    fs::remove_dir_all(dir).unwrap();
}
