//! ZCode CLI adapter (`zcode -p` standalone binary or `node zcode.cjs -p`).

use crate::harness::catalog::HarnessInventory;
use crate::harness::discovery::{bins, home_dir};
use crate::harness::{
    CapabilityMode, HarnessAdapter, ParseState, ProjectRulesMode, RunRequest, WorkMode,
};
use crate::sandbox::CommandSpec;
use serde_json::Value;
use std::env;
use std::path::{Path, PathBuf};

pub struct ZCodeAdapter;

impl HarnessAdapter for ZCodeAdapter {
    fn id(&self) -> &str {
        "zcode"
    }

    fn strengths(&self) -> &str {
        "Multi-provider GLM / Claude / Ark / DeepSeek headless coding & analysis"
    }

    fn is_available(&self) -> bool {
        let b = bins();
        b.zcode_standalone.is_some() || (b.zcode_script.is_some() && b.node.is_some())
    }

    fn binary_display(&self) -> String {
        let b = bins();
        if let Some(ref p) = b.zcode_standalone {
            p.display().to_string()
        } else if let Some(ref s) = b.zcode_script {
            s.display().to_string()
        } else {
            "(not found)".into()
        }
    }

    fn build_base(&self, req: &RunRequest) -> Result<CommandSpec, String> {
        let b = bins();
        let (program, mut args) = if let Some(ref bin) = b.zcode_standalone {
            (bin.clone(), Vec::new())
        } else if let (Some(ref node), Some(ref script)) = (&b.node, &b.zcode_script) {
            (node.clone(), vec![script.to_string_lossy().into_owned()])
        } else {
            return Err("zcode CLI not found".into());
        };

        let mode = match req.mode {
            WorkMode::Plan => "plan",
            WorkMode::Write => "edit",
            WorkMode::Full => "yolo",
        };

        let mut prompt = String::new();
        if req.project_rules == ProjectRulesMode::Smart {
            if let Some(ref patch) = req.worker_system_patch {
                prompt.push_str(patch);
                prompt.push_str("\n\n");
            }
        }
        if let Some(ref base) = req.review_base {
            prompt.push_str(&format!("Review git changes against '{base}'.\n\n"));
        }
        prompt.push_str(&req.effective_prompt);

        args.extend([
            "-p".into(),
            prompt,
            "--mode".into(),
            mode.into(),
            "--cwd".into(),
            req.cwd.to_string_lossy().into_owned(),
            "--no-color".into(),
            "--json".into(),
        ]);

        if let Some(ref m) = req.model {
            args.extend(["--model".into(), m.clone()]);
        }
        if !req.keep_session && req.session_id.is_none() {
            args.push("--ephemeral".into());
        }
        if let Some(ref sid) = req.session_id {
            if sid.eq_ignore_ascii_case("last") {
                args.push("-c".into());
            } else {
                args.extend(["--resume".into(), sid.clone()]);
            }
        }

        let mut envs = Vec::new();
        let builtin = env::var_os("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(
                    "/Applications/ZCode.app/Contents/Resources/config/provider/zcode-builtin.json",
                )
            });
        if builtin.is_file() {
            envs.push((
                "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE".into(),
                builtin.to_string_lossy().into_owned(),
            ));
        }
        let personal = env::var_os("ZCODE_PERSONAL_PROVIDER_CONFIG_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join(".zcode/v2/provider_config.json"));
        if personal.is_file() {
            envs.push((
                "ZCODE_PERSONAL_PROVIDER_CONFIG_FILE".into(),
                personal.to_string_lossy().into_owned(),
            ));
        }

        Ok(CommandSpec {
            program,
            args,
            envs,
            env_remove: Vec::new(),
            stdin_data: None,
            cwd: req.cwd.clone(),
            output_file: None,
        })
    }

    fn apply_headless_guard(&self, spec: &mut CommandSpec) {
        spec.args.extend([
            "--no-browser-state".into(),
            "--workflow-mode".into(),
            "disabled".into(),
            "--agent-always-background".into(),
            "off".into(),
        ]);
    }

    fn apply_project_rules(
        &self,
        rules: ProjectRulesMode,
        _req: &RunRequest,
        spec: &mut CommandSpec,
    ) {
        if rules == ProjectRulesMode::Off {
            spec.envs
                .push(("CLAUDE_CODE_DISABLE_CLAUDE_MDS".into(), "1".into()));
        }
    }

    fn apply_capabilities(
        &self,
        caps: &CapabilityMode,
        _inv: &HarnessInventory,
        _cwd: &Path,
        spec: &mut CommandSpec,
    ) {
        if matches!(caps, CapabilityMode::Code) {
            spec.args.extend(["--toolsearch-mode".into(), "off".into()]);
        }
    }

    fn on_stdout_line(&self, line: &str, state: &mut ParseState) {
        let trimmed = line.trim();
        if trimmed.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                if let Some(sid) = v
                    .get("sessionId")
                    .or_else(|| v.get("session_id"))
                    .and_then(Value::as_str)
                {
                    state.session_id = Some(sid.to_string());
                }
                if let Some(resp) = v
                    .get("response")
                    .or_else(|| v.get("result"))
                    .and_then(Value::as_str)
                {
                    state.last_message = Some(resp.to_string());
                    return;
                }
            }
        }
        state.raw_tail.push_line(trimmed);
    }
}
