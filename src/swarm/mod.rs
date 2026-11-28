//! Heap-bounded in-process Swarm subsystem (`types`, `registry`, `context`, `consensus`,
//! `panel`, `pipeline`, `debate`).

pub mod consensus;
pub mod context;
pub mod debate;
pub mod panel;
pub mod pipeline;
pub mod registry;
pub mod types;

use crate::harness::{DispatchEnv, DispatchTarget};
use crate::swarm::types::{SwarmAgent, SwarmSpec};

pub fn agent_to_target(agent: &SwarmAgent) -> DispatchTarget {
    DispatchTarget {
        harness: agent.harness.clone(),
        provider: agent.provider.clone(),
        model: agent.model.clone(),
        effort: agent.effort.clone(),
        prompt_override: None,
    }
}

pub fn execute_swarm(
    spec: SwarmSpec,
    task: &str,
    handoff_from: Option<&str>,
    env: &DispatchEnv,
    total_max_chars: usize,
) -> (bool, String) {
    let (kind, stage, total) = match &spec {
        SwarmSpec::Panel { agents, .. } => {
            (crate::metrics::SwarmKind::Panel, "swarm_panel", agents.len() as u64)
        }
        SwarmSpec::Pipeline { stages } => {
            (crate::metrics::SwarmKind::Pipeline, "swarm_pipeline", stages.len() as u64)
        }
        // Opening proposal, challenge/response pairs, then the judge.
        SwarmSpec::Debate(spec) => {
            (crate::metrics::SwarmKind::Debate, "swarm_debate", (2 * spec.rounds + 2) as u64)
        }
    };
    if let Some(progress) = &env.progress {
        progress.set_stage(stage, total);
    }
    crate::metrics::swarm_started(kind);
    crate::observability::event("info", "swarm_started", || {
        serde_json::json!({ "kind": kind.as_str(), "total": total })
    });
    let result = match spec {
        SwarmSpec::Panel {
            agents,
            consensus_threshold,
            parallel,
        } => panel::execute_panel(&agents, consensus_threshold, parallel, task, handoff_from, env, total_max_chars),
        SwarmSpec::Pipeline { stages } => {
            pipeline::execute_pipeline(&stages, task, handoff_from, env, total_max_chars)
        }
        SwarmSpec::Debate(debate_spec) => {
            debate::execute_debate(&debate_spec, task, handoff_from, env, total_max_chars)
        }
    };
    crate::metrics::swarm_finished(kind, result.0);
    crate::observability::event(if result.0 { "warn" } else { "info" }, "swarm_finished", || {
        serde_json::json!({ "kind": kind.as_str(), "failed": result.0 })
    });
    result
}
