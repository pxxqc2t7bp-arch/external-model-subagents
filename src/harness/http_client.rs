//! Declarative HTTP harnesses using curl's stdin config and bounded SSE parsing.

use super::catalog::{canonical_harness_id, HarnessInventory};
use super::discovery::{find_binary, home_dir};
use super::{HarnessAdapter, ParseState, RunRequest};
use crate::error::{ErrorKind, RichError};
use crate::sandbox::CommandSpec;
use serde_json::{json, Value};
use std::{env, fs::File, io::{self, Read}, path::PathBuf};

const STATUS: &str = "__HTTP_STATUS__:";
const REASONING_OPEN: &str = "<reasoning>\n";
const REASONING_CLOSE: &str = "\n</reasoning>\n\n";
const REPLY_CAP: usize = 256 * 1024;

#[derive(Clone, Copy)]
pub enum HttpFormat {
    OpenAiSse,
    AnthropicSse,
}

pub struct HttpHarnessAdapter {
    pub id: String,
    pub strengths: String,
    pub endpoint: String,
    pub api_key_env: String,
    pub default_model: String,
    pub models: Vec<String>,
    pub format: HttpFormat,
    pub extra_headers: Vec<(String, String)>,
}

pub fn load_http_adapters() -> Vec<HttpHarnessAdapter> {
    let override_path = env::var_os("EXT_MODELS_HTTP_HARNESSES_FILE");
    let path = override_path.as_ref().map(PathBuf::from).unwrap_or_else(|| {
        home_dir().join(".gemini/config/plugins/external-model-subagents/http-harnesses.json")
    });
    let mut raw = String::new();
    match File::open(path).and_then(|file| file.take(128 * 1024 + 1).read_to_string(&mut raw)) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound && override_path.is_none() => return Vec::new(),
        _ => {
            eprintln!("{}", RichError::new(ErrorKind::InvalidInput, "cannot read HTTP harness configuration"));
            return Vec::new();
        }
    }
    let result = (raw.len() <= 128 * 1024).then(|| serde_json::from_str::<Value>(&raw).ok()).flatten();
    if let Some(items) = result.as_ref().and_then(|v| v["harnesses"].as_array().or_else(|| v.as_array())) {
        let adapters: Vec<_> = items.iter().filter_map(parse_adapter).collect();
        if adapters.len() != items.len() {
            eprintln!("{}", RichError::new(ErrorKind::InvalidInput, "skipped invalid HTTP harness entries"));
        }
        return adapters;
    }
    eprintln!("{}", RichError::new(ErrorKind::InvalidInput, "invalid or oversized HTTP harness configuration"));
    Vec::new()
}

fn parse_adapter(item: &Value) -> Option<HttpHarnessAdapter> {
    if !item.as_object()?.keys().all(|k| matches!(k.as_str(),
        "$schema" | "id" | "strengths" | "endpoint" | "api_key_env" | "default_model"
        | "models" | "format" | "response_format" | "extra_headers")) { return None; }
    let field = |name: &str| item[name].as_str().map(str::trim).filter(|s| !s.is_empty());
    let id = canonical_harness_id(item["id"].as_str()?);
    let api_key_env = field("api_key_env")?;
    if id.is_empty() || id == "auto" || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        || !api_key_env.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') { return None; }
    let format = match item.get("response_format").or_else(|| item.get("format")) {
        None => HttpFormat::OpenAiSse,
        Some(v) if v.as_str() == Some("openai_sse") => HttpFormat::OpenAiSse,
        Some(v) if v.as_str() == Some("anthropic_sse") => HttpFormat::AnthropicSse,
        _ => return None,
    };
    let mut extra_headers = Vec::new();
    if let Some(headers) = item.get("extra_headers") {
        for (name, value) in headers.as_object()? {
            let value = value.as_str()?;
            if name.is_empty() || name.contains([':', '\r', '\n', '\0']) || value.contains(['\r', '\n', '\0']) { return None; }
            extra_headers.push((name.clone(), value.into()));
        }
    }
    let mut models = Vec::new();
    if let Some(list) = item.get("models") {
        for model in list.as_array()? { models.push(model.as_str()?.into()); }
    }
    Some(HttpHarnessAdapter {
        id, strengths: field("strengths").unwrap_or("Custom HTTP harness").into(),
        endpoint: field("endpoint")?.into(), api_key_env: api_key_env.into(),
        default_model: field("default_model")?.into(), models, format, extra_headers,
    })
}

fn config_escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"), '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"), '\r' => escaped.push_str("\\r"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

impl HttpHarnessAdapter {
    pub(super) fn inventory(&self, auth_mode: &str) -> HarnessInventory {
        let mut models = self.models.clone();
        if !models.contains(&self.default_model) { models.push(self.default_model.clone()); }
        HarnessInventory {
            auth_mode: auth_mode.into(), default_model: Some(self.default_model.clone()),
            default_provider: Some(self.id.clone()), providers: [(self.id.clone(), models)].into(),
            ..HarnessInventory::default()
        }
    }

    pub(super) fn build_request(&self, req: &RunRequest, key_env: &str) -> Result<CommandSpec, String> {
        let unavailable = |msg| RichError::new(ErrorKind::HarnessUnavailable, msg).to_string();
        let program = find_binary("curl", Some("CURL_BIN")).ok_or_else(|| unavailable("curl not found"))?;
        let key = env::var(key_env).ok().filter(|s| !s.is_empty() && !s.contains(['\r', '\n']))
            .ok_or_else(|| unavailable("HTTP harness API key is missing or invalid"))?;
        let model = req.model.as_deref().unwrap_or(&self.default_model);
        let user = json!({"role": "user", "content": req.effective_prompt});
        let (auth, body) = match self.format {
            HttpFormat::OpenAiSse => {
                let mut messages = Vec::with_capacity(2);
                if let Some(patch) = &req.worker_system_patch { messages.push(json!({"role": "system", "content": patch})); }
                messages.push(user);
                (format!("Authorization: Bearer {key}"), json!({"model": model, "messages": messages, "stream": true}))
            }
            HttpFormat::AnthropicSse => {
                let mut body = json!({"model": model, "messages": [user], "stream": true, "max_tokens": 4096});
                if let Some(patch) = &req.worker_system_patch { body["system"] = json!(patch); }
                (format!("x-api-key: {key}"), body)
            }
        };
        let mut config = format!("url = \"{}\"\nrequest = \"POST\"\nproto = \"=http,https\"\ngloboff\nheader = \"Content-Type: application/json\"\nheader = \"{}\"\n", config_escape(&self.endpoint), config_escape(&auth));
        if matches!(self.format, HttpFormat::AnthropicSse) {
            config.push_str("header = \"anthropic-version: 2023-06-01\"\n");
        }
        for (name, value) in &self.extra_headers {
            config.push_str(&format!("header = \"{}: {}\"\n", config_escape(name), config_escape(value)));
        }
        config.push_str(&format!("data = \"{}\"\n", config_escape(&body.to_string())));
        Ok(CommandSpec {
            program, args: ["-q", "-sS", "-N", "-w", "\n__HTTP_STATUS__:%{http_code}\n", "-K", "-"].map(String::from).into(),
            envs: Vec::new(), env_remove: vec![key_env.into()],
            stdin_data: Some(config), cwd: req.cwd.clone(), output_file: None,
            temp_files: Vec::new(),
        })
    }

}

impl HarnessAdapter for HttpHarnessAdapter {
    fn id(&self) -> &str { &self.id }
    fn strengths(&self) -> &str { &self.strengths }
    fn is_available(&self) -> bool {
        env::var_os(&self.api_key_env).is_some_and(|v| !v.is_empty())
            && find_binary("curl", Some("CURL_BIN")).is_some()
    }
    fn binary_display(&self) -> String {
        find_binary("curl", Some("CURL_BIN")).map_or_else(|| "curl (not found)".into(), |p| p.display().to_string())
    }
    fn model_inventory(&self) -> Option<HarnessInventory> { Some(self.inventory("api_key")) }
    fn response_key_env(&self) -> Option<&str> { Some(&self.api_key_env) }
    fn build_base(&self, req: &RunRequest) -> Result<CommandSpec, String> {
        self.build_request(req, &self.api_key_env)
    }

    fn on_stdout_line_bounded(&self, line: &str, truncated: bool, state: &mut ParseState) {
        if truncated && line.trim_start().starts_with("data:") {
            state.is_error = true;
            state.last_message = Some("HTTP SSE data line exceeded the line cap and was truncated".into());
            return;
        }
        self.on_stdout_line(line, state);
    }

    fn on_stdout_line(&self, line: &str, state: &mut ParseState) {
        let line = line.trim();
        if let Some(status) = line.strip_prefix(STATUS) {
            let code = status.parse::<u16>().unwrap_or(0);
            if !(200..=299).contains(&code) {
                if !state.is_error { state.last_message = Some(format!("HTTP status {code}")); }
                state.is_error = true;
            }
            return;
        }
        let payload = line.strip_prefix("data:").unwrap_or(line).trim_start();
        if !payload.starts_with('{') { return; }
        let Ok(v) = serde_json::from_str::<Value>(payload) else { return; };
        if let Some(error) = v.get("error").filter(|e| !e.is_null()) {
            state.is_error = true;
            state.last_message = Some(error["message"].as_str().or_else(|| error.as_str()).map_or_else(|| error.to_string(), String::from));
            return;
        }
        if state.is_error { return; }
        let (reasoning, content) = match self.format {
            HttpFormat::OpenAiSse => {
                let choice = &v["choices"][0];
                (choice["delta"]["reasoning_content"].as_str(), choice["delta"]["content"].as_str().or_else(|| choice["message"]["content"].as_str()))
            }
            HttpFormat::AnthropicSse if v["type"] == "content_block_delta" => (None, v["delta"]["text"].as_str()),
            HttpFormat::AnthropicSse => return,
        };
        if let Some(text) = reasoning.filter(|s| !s.is_empty()) {
            let message = state.last_message.get_or_insert_with(String::new);
            if state.http_reasoning_end.is_none() && message.len() + REASONING_OPEN.len() + REASONING_CLOSE.len() < REPLY_CAP {
                message.insert_str(0, concat!("<reasoning>\n", "\n</reasoning>\n\n"));
                state.http_reasoning_end = Some(REASONING_OPEN.len());
            }
            if let Some(end) = &mut state.http_reasoning_end {
                let remaining = (REPLY_CAP / 2 - *end - REASONING_CLOSE.len()).min(REPLY_CAP - message.len());
                let text = &text[..text.floor_char_boundary(remaining)];
                message.insert_str(*end, text);
                *end += text.len();
            }
        }
        if let Some(text) = content.filter(|s| !s.is_empty()) {
            let message = state.last_message.get_or_insert_with(String::new);
            message.push_str(&text[..text.floor_char_boundary(REPLY_CAP - message.len())]);
        }
    }
}
