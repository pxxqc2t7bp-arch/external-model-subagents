use super::catalog::HarnessInventory;
use super::discovery::find_binary;
use super::http_client::{HttpFormat, HttpHarnessAdapter};
use super::{local_cli_command, HarnessAdapter, ParseState, RunRequest};
use crate::sandbox::CommandSpec;
use std::{env, path::PathBuf};

pub struct DeepSeekAdapter {
    bin: Option<PathBuf>,
    http: HttpHarnessAdapter,
}

impl Default for DeepSeekAdapter {
    fn default() -> Self {
        Self {
            bin: find_binary("deepseek", Some("DEEPSEEK_BIN"))
                .or_else(|| find_binary("deepseek-cli", None)),
            http: HttpHarnessAdapter {
                id: "deepseek".into(),
                strengths: "DeepSeek reasoning, math, and algorithm synthesis".into(),
                endpoint: env::var("DEEPSEEK_BASE_URL")
                    .unwrap_or_else(|_| "https://api.deepseek.com/chat/completions".into()),
                api_key_env: "DEEPSEEK_API_KEY".into(),
                default_model: "deepseek-reasoner".into(),
                models: vec!["deepseek-chat".into()],
                format: HttpFormat::OpenAiSse,
                extra_headers: Vec::new(),
            },
        }
    }
}

impl HarnessAdapter for DeepSeekAdapter {
    fn id(&self) -> &str {
        &self.http.id
    }
    fn strengths(&self) -> &str {
        &self.http.strengths
    }
    fn is_available(&self) -> bool {
        self.bin.is_some() || self.http.is_available()
    }
    fn binary_display(&self) -> String {
        self.bin.as_ref().map_or_else(|| self.http.binary_display(), |p| p.display().to_string())
    }
    fn model_inventory(&self) -> Option<HarnessInventory> {
        Some(self.http.inventory(if self.bin.is_some() { "cli" } else { "api_key" }))
    }
    fn response_key_env(&self) -> Option<&str> {
        self.http.response_key_env()
    }
    fn build_base(&self, req: &RunRequest) -> Result<CommandSpec, String> {
        match &self.bin {
            Some(bin) => Ok(local_cli_command(bin, vec!["chat".into()], req)),
            None => self.http.build_base(req),
        }
    }
    fn on_stdout_line(&self, line: &str, state: &mut ParseState) {
        if self.bin.is_some() {
            state.raw_tail.push_line(line);
        } else {
            self.http.on_stdout_line(line, state);
        }
    }
}
