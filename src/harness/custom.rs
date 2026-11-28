//! Zero-recompile declarative CLI adapter loaded from `harnesses.json`
//! (supports plugging in tools like `opencode`, `aider`, `qwen-code`, etc.).

use crate::harness::discovery::{find_binary, home_dir};
use crate::harness::{HarnessAdapter, ParseState, RunRequest, WorkMode};
use crate::sandbox::CommandSpec;
use serde_json::Value;
use std::env;
use crate::error::{ErrorKind, RichError};
use crate::stream::read_config;
use std::path::{Path, PathBuf};

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

pub fn load_from_path(path: &Path) -> Vec<CustomCliAdapter> {
    let warn = |reason: &str| {
        eprintln!(
            "{}",
            RichError::new(ErrorKind::InvalidInput, format!("{}: {reason}", path.display()))
        )
    };
    let raw = match read_config(path) {
        Ok(raw) => raw,
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && env::var_os("EXT_MODELS_HARNESSES_FILE").is_none() =>
        {
            return Vec::new()
        }
        Err(e) => {
            warn(&e.to_string());
            return Vec::new();
        }
    };
    let val: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn(&e.to_string());
            return Vec::new();
        }
    };
    let Some(arr) = val.as_array().or_else(|| {
        let obj = val.as_object()?;
        obj.keys()
            .all(|k| matches!(k.as_str(), "harnesses" | "$schema"))
            .then(|| val["harnesses"].as_array())
            .flatten()
    }) else {
        warn("expected a harnesses array");
        return Vec::new();
    };
    arr.iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let adapter = parse_adapter(item);
            if adapter.is_none() {
                warn(&format!("invalid harnesses[{i}] fields or id"));
            }
            adapter
        })
        .collect()
}

fn parse_adapter(item: &Value) -> Option<CustomCliAdapter> {
    if !item.as_object()?.keys().all(|k| {
        matches!(
            k.as_str(),
            "$schema"
                | "id"
                | "binary"
                | "strengths"
                | "args"
                | "plan_flags"
                | "write_flags"
                | "full_flags"
                | "prompt_via_stdin"
                | "json_result_field"
        )
    }) {
        return None;
    }
    let id = item["id"].as_str()?;
    let binary = item["binary"].as_str()?.trim();
    if id.is_empty()
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        || binary.is_empty()
    {
        return None;
    }
    let strings = |key: &str| match item.get(key) {
        None => Some(Vec::new()),
        Some(value) => value.as_array()?.iter().map(|v| v.as_str().map(String::from)).collect(),
    };
    Some(CustomCliAdapter {
        id: super::catalog::canonical_harness_id(id),
        binary_name: binary.into(),
        strengths: item
            .get("strengths")
            .map_or(Some("Custom external CLI harness"), Value::as_str)?
            .into(),
        args_template: strings("args")?,
        plan_flags: strings("plan_flags")?,
        write_flags: strings("write_flags")?,
        full_flags: strings("full_flags")?,
        prompt_via_stdin: item.get("prompt_via_stdin").map_or(Some(true), Value::as_bool)?,
        json_result_field: match item.get("json_result_field") {
            Some(v) => Some(v.as_str()?.into()),
            None => None,
        },
    })
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
        let cwd = req.cwd.to_string_lossy();
        let mut templates = self.args_template.iter().peekable();
        while let Some(tpl) = templates.next() {
            if req.model.is_none()
                && matches!(tpl.as_str(), "--model" | "-m")
                && templates.next_if(|next| next.as_str() == "{model}").is_some()
            {
                continue;
            }
            let mut expanded = String::new();
            for chunk in tpl.split_inclusive('}') {
                let Some((prefix, key)) = chunk.strip_suffix('}').and_then(|s| s.rsplit_once('{'))
                else {
                    expanded.push_str(chunk);
                    continue;
                };
                let value = match key {
                    "cwd" => cwd.as_ref(),
                    "model" => req.model.as_deref().unwrap_or(""),
                    "prompt" => {
                        used_prompt_placeholder = true;
                        &prompt
                    }
                    _ => {
                        expanded.push_str(chunk);
                        continue;
                    }
                };
                expanded.push_str(prefix);
                expanded.push_str(value);
            }
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
            temp_files: Vec::new(),
        })
    }

    fn on_stdout_line(&self, line: &str, state: &mut ParseState) {
        if let Some(ref field) = self.json_result_field {
            let trimmed = line.trim();
            if trimmed.starts_with('{') {
                if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                    if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
                        state.session_id = Some(sid.to_string());
                    }
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
