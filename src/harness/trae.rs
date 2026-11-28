//! Trae CLI adapter (`traecli -p --output-format stream-json`).

use crate::harness::catalog::HarnessInventory;
use crate::harness::discovery::bins;
use crate::harness::{
    CapabilityMode, HarnessAdapter, ParseState, ProjectRulesMode, RunRequest, WorkMode,
};
use crate::sandbox::CommandSpec;
use serde_json::Value;

pub struct TraeAdapter;

impl HarnessAdapter for TraeAdapter {
    fn id(&self) -> &str {
        "trae"
    }

    fn strengths(&self) -> &str {
        "Multi-model exploration, fast parallel verification, and integrated workspace/IM plugins"
    }

    fn is_available(&self) -> bool {
        bins().trae.is_some()
    }

    fn binary_display(&self) -> String {
        bins()
            .trae
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(not found)".into())
    }

    fn build_base(&self, req: &RunRequest) -> Result<CommandSpec, String> {
        let bin = bins()
            .trae
            .clone()
            .ok_or_else(|| "traecli not found".to_string())?;

        let perm = match req.mode {
            WorkMode::Plan => "plan",
            WorkMode::Write => "default",
            WorkMode::Full => "bypass_permissions",
        };
        let query_sec = (req.timeout_ms / 1000).max(15);

        let mut args = vec![
            "-p".to_string(),
            "--output-format".into(),
            "stream-json".into(),
            "--permission-mode".into(),
            perm.into(),
            "--query-timeout".into(),
            format!("{query_sec}s"),
        ];

        if let Some(ref m) = req.model {
            args.extend(["-c".into(), format!("model.name={m}")]);
        }
        if let Some(ref sid) = req.session_id {
            if sid.eq_ignore_ascii_case("last") {
                args.push("--resume=AUTO".into());
            } else {
                args.extend(["--resume".into(), sid.clone()]);
            }
        }

        let mut full_prompt = String::new();
        if req.project_rules == ProjectRulesMode::Smart {
            if let Some(ref patch) = req.worker_system_patch {
                full_prompt.push_str(patch);
                full_prompt.push_str("\n\n");
            }
        } else if req.project_rules == ProjectRulesMode::Off {
            // traecli has no native rule-disable flag; Off is best-effort prompt guidance.
            full_prompt.push_str(
                "Instruction Override: Disregard repository-level AGENTS.md / workflow rules for this isolated run.\n\n",
            );
        }
        if let Some(ref base) = req.review_base {
            full_prompt.push_str(&format!("Review git changes against '{base}'.\n\n"));
        }
        full_prompt.push_str(&req.effective_prompt);

        args.extend(["--".into(), full_prompt]);

        Ok(CommandSpec {
            program: bin,
            args,
            envs: Vec::new(),
            env_remove: Vec::new(),
            stdin_data: None,
            cwd: req.cwd.clone(),
            output_file: None,
            temp_files: Vec::new(),
        })
    }

    fn apply_capabilities(
        &self,
        caps: &CapabilityMode,
        _inv: &HarnessInventory,
        _req: &RunRequest,
        spec: &mut CommandSpec,
    ) -> Result<(), String> {
        let wait_secs = match caps {
            CapabilityMode::Code => "0",
            CapabilityMode::Selective(_) | CapabilityMode::Full => "8",
        };
        let pos = spec
            .args
            .iter()
            .position(|a| a == "--")
            .unwrap_or(spec.args.len());
        spec.args.insert(pos, "--mcp-init-wait-seconds".into());
        spec.args.insert(pos + 1, wait_secs.into());
        Ok(())
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
        if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
            state.session_id = Some(sid.to_string());
        }
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        if ty == "system" {
            if let Some(m) = v.get("model").and_then(Value::as_str) {
                state.used_model = Some(m.to_string());
            }
        } else if ty == "assistant" {
            if let Some(content) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
            {
                state.last_message = Some(content.to_string());
            }
        } else if ty == "result" {
            if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                state.is_error = true;
            }
            if let Some(res) = v.get("result").and_then(Value::as_str) {
                state.last_message = Some(res.to_string());
            }
        }
    }
}
