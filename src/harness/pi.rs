use super::http_client::{HttpFormat, HttpHarnessAdapter};
use std::env;

pub type PiAdapter = HttpHarnessAdapter;

pub fn pi_adapter() -> PiAdapter {
    PiAdapter {
        id: "pi".into(),
        strengths: "Conversational synthesis, product critique, and technical writing".into(),
        endpoint: env::var("PI_BASE_URL")
            .unwrap_or_else(|_| "https://api.inflection.ai/v1/chat/completions".into()),
        api_key_env: "PI_API_KEY".into(),
        default_model: "inflection_3_pi".into(),
        models: Vec::new(),
        format: HttpFormat::OpenAiSse,
        extra_headers: Vec::new(),
    }
}
