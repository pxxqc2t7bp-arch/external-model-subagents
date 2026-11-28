//! OpenAI Codex CLI adapter (`codex exec`, `codex exec resume`, `codex exec review`).

use crate::harness::catalog::HarnessInventory;
use crate::harness::discovery::bins;
use crate::harness::{
    CapabilityMode, HarnessAdapter, ParseState, ProjectRulesMode, RunRequest, WorkMode,
};
use crate::sandbox::CommandSpec;
use crate::stream::artifact_dir;
use serde_json::Value;
use std::path::Path;

pub struct CodexAdapter;

impl HarnessAdapter for CodexAdapter {
    fn id(&self) -> &str {
        "codex"
    }

    fn strengths(&self) -> &str {
        "Deep debugging, adversarial code review (`review_base`), root-cause analysis, complex algorithms"
    }

    fn is_available(&self) -> bool {
        bins().codex.is_some()
    }

    fn binary_display(&self) -> String {
        bins()
            .codex
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(not found)".into())
    }

    fn build_base(&self, req: &RunRequest) -> Result<CommandSpec, String> {
        let bin = bins()
            .codex
            .clone()
            .ok_or_else(|| "codex CLI not found".to_string())?;

        let out_file = artifact_dir()
            .ok()
            .map(|d| d.join(format!("{}-codex-last.txt", req.call_id)));

        let mut args = vec!["exec".to_string()];

        // Map unified mode to Codex sandbox flag (`review` subcommand runs read-only natively).
        if req.review_base.is_none() {
            let sb = match req.mode {
                WorkMode::Plan => "read-only",
                WorkMode::Write => "workspace-write",
                WorkMode::Full => "danger-full-access",
            };
            args.extend(["-s".into(), sb.into()]);
        }

        if let Some(ref prov) = req.provider {
            let escaped = escape_toml_str(prov);
            args.extend(["-c".into(), format!("model_provider=\"{escaped}\"")]);
        }
        if let Some(ref m) = req.model {
            args.extend(["-m".into(), m.clone()]);
        }
        if let Some(ref eff) = req.effort {
            let escaped = escape_toml_str(eff);
            args.extend(["-c".into(), format!("model_reasoning_effort=\"{escaped}\"")]);
        }

        args.push("--skip-git-repo-check".into());
        if !req.keep_session && req.session_id.is_none() {
            args.push("--ephemeral".into());
        }
        args.push("--json".into());
        if let Some(ref of) = out_file {
            args.extend(["-o".into(), of.to_string_lossy().into_owned()]);
        }

        // Compose stdin payload (including optional worker patch when `project_rules == Smart`).
        let mut stdin_text = String::new();
        if let Some(ref patch) = req.worker_system_patch {
            stdin_text.push_str(patch);
            stdin_text.push_str("\n\n");
        }
        stdin_text.push_str(&req.effective_prompt);

        if let Some(ref base) = req.review_base {
            let is_uncommitted =
                base.eq_ignore_ascii_case("uncommitted") || base.eq_ignore_ascii_case("working");
            if req.worktree && is_uncommitted {
                return Err(
                    "worktree=true cannot be combined with review_base='uncommitted' \
                     (worktree snapshots dirty state into a baseline commit; run read-only uncommitted review without worktree)"
                        .into(),
                );
            }
            args.push("review".into());
            let has_custom_input = !req.prompt.trim().is_empty()
                || req.effective_prompt.starts_with("<previous_agent_handoff ")
                || req.effective_prompt.starts_with("<context_file ");
            if has_custom_input {
                // `codex exec review` forbids combining `--base`/`--uncommitted` with `[PROMPT]` (`-`).
                // Embed the target diff scope into the stdin prompt and pass `-` only.
                let scope = if is_uncommitted {
                    "Review Scope: Perform an adversarial code review of all uncommitted changes (staged, unstaged, and untracked files — inspect with `git status` and `git diff HEAD`).\n\n".to_string()
                } else {
                    format!(
                        "Review Scope: Perform an adversarial code review of git changes against base '{base}' (inspect with `git diff {base}...HEAD` and `git log --oneline {base}..HEAD`).\n\n"
                    )
                };
                stdin_text.insert_str(0, &scope);
                args.push("-".into());
            } else {
                if is_uncommitted {
                    args.push("--uncommitted".into());
                } else {
                    args.extend(["--base".into(), base.clone()]);
                }
                stdin_text.clear();
            }
        } else if let Some(ref sid) = req.session_id {
            if sid.starts_with('-') {
                return Err(format!("invalid session_id '{sid}': must not start with '-'"));
            }
            args.extend(["-C".into(), req.cwd.to_string_lossy().into_owned()]);
            args.push("resume".into());
            if sid.eq_ignore_ascii_case("last") {
                args.push("--last".into());
            } else {
                args.push(sid.clone());
            }
            args.push("-".into());
        } else {
            args.extend(["-C".into(), req.cwd.to_string_lossy().into_owned()]);
            args.push("-".into());
        }

        Ok(CommandSpec {
            program: bin,
            args,
            envs: Vec::new(),
            env_remove: Vec::new(),
            stdin_data: (!stdin_text.is_empty()).then_some(stdin_text),
            cwd: req.cwd.clone(),
            output_file: out_file,
        })
    }

    fn apply_headless_guard(&self, spec: &mut CommandSpec) {
        insert_before_subcmd(
            &mut spec.args,
            &[
                "--disable",
                "hooks",
                "--disable",
                "memories",
                "-c",
                "notify=[]",
                "-c",
                "mcp_servers.computer-use.enabled=false",
            ],
        );
    }

    fn apply_project_rules(
        &self,
        rules: ProjectRulesMode,
        _req: &RunRequest,
        spec: &mut CommandSpec,
    ) {
        if rules == ProjectRulesMode::Off {
            // Disable only `AGENTS.md` project docs (`project_doc_max_bytes=0`).
            // Do NOT pass `--ignore-rules`, which disables Codex execpolicy `.rules` security controls.
            insert_before_subcmd(&mut spec.args, &["-c", "project_doc_max_bytes=0"]);
        }
    }

    fn apply_capabilities(
        &self,
        caps: &CapabilityMode,
        inv: &HarnessInventory,
        _cwd: &Path,
        spec: &mut CommandSpec,
    ) {
        match caps {
            CapabilityMode::Code => {
                // Disable both `[mcp_servers]` and `[plugins.*]` remote OAuth MCP workers in headless code runs.
                let mut flags = vec!["-c".to_string(), "mcp_servers={}".to_string()];
                if inv.plugins.is_empty() {
                    flags.push("-c".into());
                    flags.push("plugins={}".into());
                } else {
                    for plug in &inv.plugins {
                        let escaped = escape_toml_str(plug);
                        flags.push("-c".into());
                        flags.push(format!("plugins.\"{escaped}\".enabled=false"));
                    }
                }
                let refs: Vec<&str> = flags.iter().map(String::as_str).collect();
                insert_before_subcmd(&mut spec.args, &refs);
            }
            CapabilityMode::Selective(allowed) => {
                let mut flags = Vec::new();
                for name in &inv.mcps {
                    if !allowed.iter().any(|a| a.eq_ignore_ascii_case(name)) {
                        flags.push("-c".to_string());
                        flags.push(format!("mcp_servers.{name}.enabled=false"));
                    }
                }
                for plug in &inv.plugins {
                    if !allowed.iter().any(|a| a.eq_ignore_ascii_case(plug)) {
                        let escaped = escape_toml_str(plug);
                        flags.push("-c".into());
                        flags.push(format!("plugins.\"{escaped}\".enabled=false"));
                    }
                }
                let refs: Vec<&str> = flags.iter().map(String::as_str).collect();
                insert_before_subcmd(&mut spec.args, &refs);
            }
            CapabilityMode::Full => {
                // Keep all non-poison MCPs and plugins enabled.
            }
        }
    }

    fn on_stdout_line(&self, line: &str, state: &mut ParseState) {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            state.raw_tail.push_line(trimmed);
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            return;
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        if ty == "thread.started" {
            if let Some(tid) = v.get("thread_id").and_then(Value::as_str) {
                state.session_id = Some(tid.to_string());
            }
        } else if ty == "item.completed" {
            if let Some(item) = v.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                    if let Some(txt) = item.get("text").and_then(Value::as_str) {
                        state.last_message = Some(txt.to_string());
                    }
                }
            }
        } else if ty == "turn.failed" || ty == "error" {
            state.is_error = true;
            if let Some(msg) = v
                .get("message")
                .or_else(|| v.get("error").and_then(|e| e.get("message")))
                .and_then(Value::as_str)
            {
                state.raw_tail.push_line(msg);
            }
        }
    }
}

/// Insert global `codex exec [OPTIONS]` before any `review` / `resume` / `-` positional token.
fn insert_before_subcmd(args: &mut Vec<String>, extra: &[&str]) {
    let pos = args
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, a)| matches!(a.as_str(), "review" | "resume" | "-"))
        .map(|(i, _)| i)
        .unwrap_or(args.len());
    for (offset, item) in extra.iter().enumerate() {
        args.insert(pos + offset, (*item).to_string());
    }
}

fn escape_toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{0008}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\u{007F}' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}
