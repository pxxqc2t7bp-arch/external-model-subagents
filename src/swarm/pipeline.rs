use std::borrow::Cow;
use std::sync::atomic::Ordering;

use serde_json::Value;

use super::agent_to_target;
use super::context::SwarmContext;
use super::types::{SwarmStage, ValidationRule};
use crate::error::{ErrorKind, RichError};
use crate::harness::DispatchEnv;

pub fn execute_pipeline(
    stages: &[SwarmStage],
    task: &str,
    handoff_from: Option<&str>,
    env: &DispatchEnv,
    total_max_chars: usize,
) -> (bool, String) {
    let ctx = SwarmContext::new();
    let per_budget = (total_max_chars / stages.len()).max(800);
    let mut report = String::from("[swarm=pipeline]");
    let mut prev_output = String::new();
    let mut all_failed = true;
    for (i, stage) in stages.iter().enumerate() {
        report.push_str(&format!("\n\n### Stage {}: {}\n", i + 1, stage.name));
        if env.cancel.load(Ordering::Acquire) {
            crate::observability::event("warn", "swarm_pipeline_cancelled", || {
                serde_json::json!({ "stage": i + 1 })
            });
            let err = RichError::new(ErrorKind::ExecutionFailed, "Pipeline cancelled");
            report.push_str(&err.to_string());
            return (true, report);
        }
        crate::observability::event("info", "swarm_pipeline_stage_started", || {
            serde_json::json!({ "stage": i + 1, "continue_on_error": stage.continue_on_error })
        });
        let prompt = match &stage.input_template {
            Some(tpl) => Cow::Owned(ctx.render_template(tpl, task, &prev_output)),
            None if i == 0 => Cow::Borrowed(task),
            None => Cow::Owned(format!("{task}\n\n### Previous stage output\n{prev_output}")),
        };
        let res = env.run_target(
            &agent_to_target(&stage.agent),
            &format!("You are {}.\n\n{prompt}", stage.agent.role),
            if i == 0 { handoff_from } else { None },
            None,
            false,
            per_budget,
        );
        report.push_str(&res.formatted);
        let failure = if res.failed {
            Some(RichError::new(
                ErrorKind::ExecutionFailed,
                "Stage execution failed",
            ))
        } else {
            stage
                .validations
                .iter()
                .find_map(|rule| validate(rule, &res.raw_body))
        };
        crate::observability::event(
            if failure.is_some() { "warn" } else { "info" },
            "swarm_pipeline_stage_finished",
            || serde_json::json!({
                "stage": i + 1,
                "failed": failure.is_some(),
                "execution_failed": res.failed,
                "continuing_on_error": failure.is_some() && stage.continue_on_error,
            }),
        );
        if let Some(err) = failure {
            if !stage.continue_on_error {
                report.push_str(&format!("\n\nError: {err}"));
                return (true, report);
            }
            report.push_str(&format!("\n\nWarning: {err}; continuing pipeline"));
        } else {
            all_failed = false;
            ctx.set_note(stage.name.clone(), res.raw_body.clone());
            ctx.record_stage_handoff(stage.name.clone(), res.call_id);
        }
        prev_output = res.raw_body;
    }
    (all_failed, report)
}

fn validate(rule: &ValidationRule, output: &str) -> Option<RichError> {
    let valid = match rule {
        ValidationRule::NonEmpty => !output.trim().is_empty(),
        ValidationRule::JsonValid => serde_json::from_str::<Value>(output).is_ok(),
        ValidationRule::Contains(text) => output.contains(text.as_str()),
        ValidationRule::MinChars(min) => output.chars().count() >= *min,
    };
    (!valid).then(|| {
        RichError::new(ErrorKind::ExecutionFailed, format!("Output validation failed: {rule:?}"))
    })
}
