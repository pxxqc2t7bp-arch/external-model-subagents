use std::sync::atomic::Ordering;
use std::thread;

use super::agent_to_target;
use super::consensus::{evaluate_consensus, extract_vote};
use super::types::{Decision, SwarmAgent, Vote};
use crate::error::{record_worker_panic, ErrorKind, RichError};
use crate::harness::DispatchEnv;

pub fn execute_panel(
    agents: &[SwarmAgent],
    threshold: f32,
    parallel: bool,
    task: &str,
    handoff_from: Option<&str>,
    env: &DispatchEnv,
    total_max_chars: usize,
) -> (bool, String) {
    let prompt = format!(
        "{task}\nChoose one decision and end your response with a line in this format: SWARM_VOTE: APPROVE|REJECT|NEEDS_CHANGES|ABSTAIN | confidence=0.85 | summary=..."
    );
    let per_budget = (total_max_chars / agents.len()).max(800);
    let run = |id: &str, agent: &SwarmAgent| {
        crate::observability::event("info", "swarm_panel_agent_started", || {
            serde_json::json!({ "agent_id": id, "parallel": parallel })
        });
        let res = env.run_target(
            &agent_to_target(agent),
            &prompt,
            handoff_from,
            None,
            false,
            per_budget,
        );
        let vote = (!res.failed).then(|| extract_vote(id, &agent.role, agent.weight, &res.raw_body));
        (vote, res.formatted)
    };
    let mut all_failed = true;
    let mut votes = Vec::with_capacity(agents.len());
    let mut outputs = String::new();
    thread::scope(|scope| {
        let nodes: Vec<_> = agents
            .iter()
            .enumerate()
            .map(|(i, agent)| {
                let id = format!("panel_{}", i + 1);
                let worker = parallel.then(|| {
                    let worker_id = id.clone();
                    thread::Builder::new()
                        .stack_size(256 * 1024)
                        .spawn_scoped(scope, move || run(&worker_id, agent))
                });
                (id, agent, worker)
            })
            .collect();
        for (id, agent, worker) in nodes {
            let result = match worker {
                Some(Ok(worker)) => worker
                    .join()
                    .map_err(|payload| record_worker_panic(&id, payload.as_ref())),
                Some(Err(err)) => Err(RichError::new(ErrorKind::ExecutionFailed, err.to_string())),
                None if env.cancel.load(Ordering::Acquire) => {
                    Err(RichError::new(ErrorKind::ExecutionFailed, "Panel agent cancelled"))
                }
                None => Ok(run(&id, agent)),
            };
            let (vote, output) = match result {
                Ok(result) => result,
                Err(err) => (None, err.to_string()),
            };
            crate::observability::event(
                if vote.is_none() { "warn" } else { "info" },
                "swarm_panel_vote",
                || serde_json::json!({
                    "agent_id": id,
                    "failed": vote.is_none(),
                    "decision": vote.as_ref().map_or(Decision::Abstain, |v| v.decision).as_str(),
                    "confidence": vote.as_ref().map_or(0.0, |v| v.confidence),
                    "weight": agent.weight,
                }),
            );
            outputs.push_str(&format!("\n\n### Agent {id} ({})\n", agent.role));
            outputs.push_str(&output);
            votes.push(match vote {
                Some(vote) => {
                    all_failed = false;
                    vote
                }
                None => Vote {
                    agent_id: id,
                    role: agent.role.clone(),
                    decision: Decision::Abstain,
                    confidence: 0.0,
                    weight: agent.weight,
                    summary: "Execution failed".into(),
                }
            });
        }
    });

    let result = evaluate_consensus(votes, threshold);
    crate::observability::event("info", "swarm_panel_consensus", || {
        serde_json::json!({
            "decision": result.decision.as_str(),
            "reached": result.reached,
            "approval_ratio": result.approval_ratio,
            "confidence": result.confidence,
            "threshold": threshold,
        })
    });
    let mut report = format!(
        "[swarm=panel | decision={} | reached={} | approval_ratio={:.2} | confidence={:.2}]\n\n### Votes",
        result.decision.as_str(), result.reached, result.approval_ratio, result.confidence,
    );
    for vote in result.votes {
        report.push_str(&format!(
            "\n- {} ({}): {} | confidence={:.2} | weight={:.2} | summary={}",
            vote.agent_id, vote.role, vote.decision.as_str(), vote.confidence, vote.weight, vote.summary,
        ));
    }
    report.push_str(&outputs);
    (all_failed, report)
}
