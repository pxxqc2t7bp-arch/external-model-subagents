//! Anthropic Claude Code CLI adapter (`claude -p --output-format json`).
//!
//! Uses `CLAUDE_CODE_DISABLE_CLAUDE_MDS=1` for `project_rules: "off"` so disabling `CLAUDE.md`
//! remains 100% decoupled from `.mcp.json`, skills, and plugins.

use crate::harness::catalog::{build_claude_mcp_config, HarnessInventory};
use crate::harness::discovery::bins;
use crate::harness::{
    CapabilityMode, HarnessAdapter, ParseState, ProjectRulesMode, RunRequest, WorkMode,
};
use crate::sandbox::CommandSpec;
use serde_json::Value;
use std::path::Path;

pub struct ClaudeAdapter;

impl HarnessAdapter for ClaudeAdapter {
    fn id(&self) -> &str {
        "claude"
    }

    fn strengths(&self) -> &str {
        "Multi-file architectural refactoring, clean engineering implementation, frontend/UX polish"
    }

    fn is_available(&self) -> bool {
        bins().claude.is_some()
    }

    fn binary_display(&self) -> String {
        bins()
            .claude
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(not found)".into())
    }

    fn build_base(&self, req: &RunRequest) -> Result<CommandSpec, String> {
        let bin = bins()
            .claude
            .clone()
            .ok_or_else(|| "claude CLI not found".to_string())?;

        let perm = match req.mode {
            WorkMode::Plan => "plan",
            WorkMode::Write => "acceptEdits",
            WorkMode::Full => "bypassPermissions",
        };

        let mut args = vec![
            "-p".to_string(),
            "--output-format".into(),
            "json".into(),
            "--permission-mode".into(),
            perm.into(),
        ];

        if let Some(ref m) = req.model {
            args.extend(["--model".into(), m.clone()]);
        }
        if let Some(ref eff) = req.effort {
            args.extend(["--effort".into(), eff.clone()]);
        }
        if !req.keep_session && req.session_id.is_none() {
            args.push("--no-session-persistence".into());
        } else {
            args.push("--exclude-dynamic-system-prompt-sections".into());
        }

        if let Some(ref sid) = req.session_id {
            if sid.eq_ignore_ascii_case("last") {
                args.push("-c".into());
            } else {
                args.extend(["--resume".into(), sid.clone()]);
            }
        }

        let prompt_body = if let Some(ref base) = req.review_base {
            format!(
                "Perform a thorough adversarial code review of git changes against '{base}'.\n\n{}",
                req.effective_prompt
            )
        } else {
            req.effective_prompt.clone()
        };

        Ok(CommandSpec {
            program: bin,
            args,
            envs: Vec::new(),
            // Strip `CLAUDECODE` so Claude never refuses nested invocations.
            env_remove: vec!["CLAUDECODE".into()],
            stdin_data: Some(prompt_body),
            cwd: req.cwd.clone(),
            output_file: None,
        })
    }

    fn apply_headless_guard(&self, spec: &mut CommandSpec) {
        spec.args.extend([
            "--no-chrome".into(),
            "--settings".into(),
            "{\"disableAllHooks\":true}".into(),
        ]);
        spec.envs.push(("CLAUDE_CODE_DISABLE_AUTO_MEMORY".into(), "1".into()));
        spec.envs
            .push(("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into()));
    }

    fn apply_project_rules(
        &self,
        rules: ProjectRulesMode,
        req: &RunRequest,
        spec: &mut CommandSpec,
    ) {
        match rules {
            ProjectRulesMode::On => {}
            ProjectRulesMode::Smart => {
                if let Some(ref patch) = req.worker_system_patch {
                    spec.args
                        .extend(["--append-system-prompt".into(), patch.clone()]);
                }
            }
            ProjectRulesMode::Off => {
                // Surgical env flag: disables ONLY CLAUDE.md discovery without blocking MCPs or skills!
                spec.envs
                    .push(("CLAUDE_CODE_DISABLE_CLAUDE_MDS".into(), "1".into()));
            }
        }
    }

    fn apply_capabilities(
        &self,
        caps: &CapabilityMode,
        _inv: &HarnessInventory,
        cwd: &Path,
        spec: &mut CommandSpec,
    ) {
        match caps {
            CapabilityMode::Code => {
                spec.args.push("--strict-mcp-config".into());
                spec.envs
                    .push(("CLAUDE_CODE_SKIP_PLUGIN_MCP_SERVERS".into(), "1".into()));
            }
            CapabilityMode::Selective(allowed) => {
                let inline = build_claude_mcp_config(cwd, Some(allowed));
                spec.args.extend([
                    "--strict-mcp-config".into(),
                    "--mcp-config".into(),
                    inline,
                ]);
            }
            CapabilityMode::Full => {
                let inline = build_claude_mcp_config(cwd, None);
                spec.args.extend([
                    "--strict-mcp-config".into(),
                    "--mcp-config".into(),
                    inline,
                ]);
            }
        }
    }

    fn on_stdout_line(&self, line: &str, state: &mut ParseState) {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            state.raw_tail.push_line(trimmed);
            return;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
                state.session_id = Some(sid.to_string());
            }
            if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                state.is_error = true;
            }
            if let Some(res) = v.get("result").and_then(Value::as_str) {
                state.last_message = Some(res.to_string());
                return;
            }
        }
        state.raw_tail.push_line(trimmed);
    }
}
