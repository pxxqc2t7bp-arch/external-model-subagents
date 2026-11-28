use super::catalog::HarnessInventory;
use super::discovery::find_binary;
use super::http_client::{HttpFormat, HttpHarnessAdapter};
use super::{local_cli_command, HarnessAdapter, ParseState, RunRequest, WorkMode};
use crate::sandbox::CommandSpec;
use std::{env, path::PathBuf};

pub struct KimiAdapter {
    bin: Option<PathBuf>,
    http: HttpHarnessAdapter,
}

impl Default for KimiAdapter {
    fn default() -> Self {
        Self {
            bin: find_binary("kimi", Some("KIMI_BIN")).or_else(|| find_binary("kimi-cli", None)),
            http: HttpHarnessAdapter {
                id: "kimi".into(),
                strengths: "Long-context repository comprehension and multi-file analysis".into(),
                endpoint: env::var("KIMI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.moonshot.cn/v1/chat/completions".into()),
                api_key_env: "KIMI_API_KEY".into(),
                default_model: "kimi-k2-5".into(),
                models: Vec::new(),
                format: HttpFormat::OpenAiSse,
                extra_headers: Vec::new(),
            },
        }
    }
}

impl HarnessAdapter for KimiAdapter {
    fn id(&self) -> &str {
        &self.http.id
    }
    fn strengths(&self) -> &str {
        &self.http.strengths
    }
    fn is_available(&self) -> bool {
        self.bin.is_some()
            || (self.response_key_env().is_some() && find_binary("curl", Some("CURL_BIN")).is_some())
    }
    fn binary_display(&self) -> String {
        self.bin.as_ref().map_or_else(|| self.http.binary_display(), |p| p.display().to_string())
    }
    fn model_inventory(&self) -> Option<HarnessInventory> {
        Some(self.http.inventory(if self.bin.is_some() { "cli" } else { "api_key" }))
    }
    fn response_key_env(&self) -> Option<&str> {
        [self.http.api_key_env.as_str(), "MOONSHOT_API_KEY"]
            .into_iter()
            .find(|name| env::var_os(name).is_some_and(|v| !v.is_empty()))
    }
    fn build_base(&self, req: &RunRequest) -> Result<CommandSpec, String> {
        match &self.bin {
            Some(bin) => {
                let mut args = vec!["--quiet".into()];
                if req.mode == WorkMode::Full {
                    args.push("--yolo".into());
                }
                Ok(local_cli_command(bin, args, req))
            }
            None => self.http.build_request(
                req,
                self.response_key_env().unwrap_or(&self.http.api_key_env),
            ),
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
