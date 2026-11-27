//! Zero-recompile declarative CLI adapter loaded from `harnesses.json`
//! (supports plugging in tools like `opencode`, `aider`, `qwen-code`, etc.).

use crate::harness::discovery::{find_binary, home_dir};
use crate::harness::{HarnessAdapter, ParseState, RunRequest, WorkMode};
use crate::sandbox::CommandSpec;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;

pub struct CustomCliAdapter {
    id: String,
    binary_name: String,
    strengths: String,
    args_template: Vec<String>,
    plan_flags: Vec<String>,
    write_flags: Vec<String>,
    full_flags: Vec<String>,
    prompt_via_stdin: bool,
    json_result_field: Option<String>,
}

pub fn load_custom_adapters() -> Vec<CustomCliAdapter> {
    let path = env::var_os("EXT_MODELS_HARNESSES_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home_dir().join(".gemini/config/plugins/external-model-subagents/harnesses.json")
        });
    load_from_path(&path)
}

pub fn load_from_path(path: &PathBuf) -> Vec<CustomCliAdapter> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(val) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(arr) = val
        .get("harnesses")
        .and_then(Value::as_array)
        .or_else(|| val.as_array())
    else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for item in arr {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(bin) = item.get("binary").and_then(Value::as_str) else {
            continue;
        };
        out.push(CustomCliAdapter {
            id: id.trim().to_ascii_lowercase(),
            binary_name: bin.trim().to_string(),
            strengths: item
                .get("strengths")
                .and_then(Value::as_str)
                .unwrap_or("Custom external CLI harness")
                .to_string(),
            args_template: str_vec(item.get("args")),
            plan_flags: str_vec(item.get("plan_flags")),
            write_flags: str_vec(item.get("write_flags")),
            full_flags: str_vec(item.get("full_flags")),
            prompt_via_stdin: item
                .get("prompt_via_stdin")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            json_result_field: item
                .get("json_result_field")
                .and_then(Value::as_str)
                .map(String::from),
        });
    }
    out
}

impl HarnessAdapter for CustomCliAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn strengths(&self) -> &str {
        &self.strengths
    }

    fn is_available(&self) -> bool {
        find_binary(&self.binary_name, None).is_some()
    }

    fn binary_display(&self) -> String {
        find_binary(&self.binary_name, None)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("{} (not found)", self.binary_name))
    }

    fn build_base(&self, req: &RunRequest) -> Result<CommandSpec, String> {
        let bin = find_binary(&self.binary_name, None)
            .ok_or_else(|| format!("custom harness binary '{}' not found", self.binary_name))?;

        let mode_flags = match req.mode {
            WorkMode::Plan => &self.plan_flags,
            WorkMode::Write => &self.write_flags,
            WorkMode::Full => &self.full_flags,
        };

        let mut prompt = String::new();
        if let Some(ref patch) = req.worker_system_patch {
            prompt.push_str(patch);
            prompt.push_str("\n\n");
        }
        prompt.push_str(&req.effective_prompt);

        let mut args = Vec::new();
        args.extend(mode_flags.iter().cloned());

        let mut used_prompt_placeholder = false;
        for tpl in &self.args_template {
            if tpl == "{model}" {
                if let Some(ref m) = req.model {
                    args.push(m.clone());
                }
                continue;
            }
            if tpl.contains("{prompt}") {
                used_prompt_placeholder = true;
            }
            let expanded = tpl
                .replace("{cwd}", &req.cwd.to_string_lossy())
                .replace("{model}", req.model.as_deref().unwrap_or(""))
                .replace("{prompt}", &prompt);
            if !expanded.is_empty() {
                args.push(expanded);
            }
        }

        let stdin_data = if self.prompt_via_stdin && !used_prompt_placeholder {
            Some(prompt)
        } else {
            if !used_prompt_placeholder {
                args.push(prompt);
            }
            None
        };

        Ok(CommandSpec {
            program: bin,
            args,
            envs: Vec::new(),
            env_remove: Vec::new(),
            stdin_data,
            cwd: req.cwd.clone(),
            output_file: None,
        })
    }

    fn on_stdout_line(&self, line: &str, state: &mut ParseState) {
        if let Some(ref field) = self.json_result_field {
            let trimmed = line.trim();
            if trimmed.starts_with('{') {
                if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                    if let Some(txt) = v.get(field).and_then(Value::as_str) {
                        state.last_message = Some(txt.to_string());
                        return;
                    }
                }
            }
        }
        state.raw_tail.push_line(line);
    }
}

fn str_vec(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}
