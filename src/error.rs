//! Flat, actionable error model (`RichError`) and worker panic artifact recorder.

use crate::stream::write_artifact;
use std::any::Any;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidInput,
    RoutingMismatch,
    HarnessUnavailable,
    WorktreeFailure,
    ExecutionTimeout,
    ExecutionFailed,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::RoutingMismatch => "routing_mismatch",
            Self::HarnessUnavailable => "harness_unavailable",
            Self::WorktreeFailure => "worktree_failure",
            Self::ExecutionTimeout => "execution_timeout",
            Self::ExecutionFailed => "execution_failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RichError {
    pub kind: ErrorKind,
    pub message: String,
    pub suggestion: Option<String>,
}

impl RichError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            suggestion: None,
        }
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        let s = suggestion.into();
        if !s.trim().is_empty() {
            self.suggestion = Some(s);
        }
        self
    }
}

impl fmt::Display for RichError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind.as_str(), self.message)?;
        if let Some(ref hint) = self.suggestion {
            write!(f, "\nSuggestion: {hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RichError {}

/// Extract panic message and persist a trace artifact under `/tmp/ext-models/<id>-panic.log`.
pub fn record_worker_panic(context_id: &str, payload: &(dyn Any + Send)) -> RichError {
    let detail = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    };

    let safe_id: String = context_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    let tag = if safe_id.is_empty() { "worker" } else { &safe_id };
    let body = format!("context_id: {context_id}\npanic: {detail}\n");
    let saved = write_artifact(&format!("{tag}-panic.log"), &body);

    let mut err = RichError::new(
        ErrorKind::ExecutionFailed,
        format!("internal worker panic: {detail}"),
    );
    if let Some(path) = saved {
        err = err.with_suggestion(format!("Inspect panic artifact at {}", path.display()));
    }
    err
}
