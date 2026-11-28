use crate::harness::DispatchEnv;
use crate::swarm::agent_to_target;
use crate::swarm::types::DebateSpec;
use std::sync::atomic::Ordering;

pub fn execute_debate(
    spec: &DebateSpec,
    task: &str,
    handoff_from: Option<&str>,
    env: &DispatchEnv,
    total_max_chars: usize,
) -> (bool, String) {
    let budget = (total_max_chars / (2 * spec.rounds + 2)).max(800);
    let mut history = String::new();
    let mut report = String::new();
    // Turn zero is the opening proposal; each round then challenges and responds.
    for turn in 0..=2 * spec.rounds {
        if env.cancel.load(Ordering::Relaxed) {
            crate::observability::event("warn", "swarm_debate_cancelled", || {
                serde_json::json!({ "phase": "turn", "turn": turn })
            });
            report.push_str("\n[swarm=debate | cancelled]");
            return (true, report);
        }
        let round = turn.div_ceil(2);
        let (agent, title, instruction) = if turn == 0 {
            (&spec.proposer, "Initial proposal".to_string(), "Present your initial proposal.")
        } else if turn % 2 == 1 {
            (&spec.challenger, format!("Round {round}: challenger"),
                "Challenge the proposal, taking the debate history into account.")
        } else {
            (&spec.proposer, format!("Round {round}: proposer"),
                "Respond to the latest challenge, taking the debate history into account.")
        };
        crate::observability::event("info", "swarm_debate_turn_started", || {
            serde_json::json!({
                "turn": turn,
                "round": round,
                "speaker": if turn % 2 == 0 { "proposer" } else { "challenger" },
            })
        });
        let prompt = format!(
            "You are {}. {instruction}\n\nTask:\n{task}\n\nDebate history:\n{history}",
            agent.role
        );
        let res = env.run_target(
            &agent_to_target(agent),
            &prompt,
            if turn == 0 { handoff_from } else { None },
            None,
            false,
            budget,
        );
        crate::observability::event(
            if res.failed { "warn" } else { "info" },
            "swarm_debate_turn_finished",
            || serde_json::json!({ "turn": turn, "round": round, "failed": res.failed }),
        );
        report.push_str(&format!("\n\n### {title}\n{}", res.formatted));
        if res.failed {
            return (true, report);
        }
        let end = res.raw_body.char_indices().nth(4000).map_or(res.raw_body.len(), |(i, _)| i);
        history.push_str(&format!("\n\n### {title}\n{}", &res.raw_body[..end]));
    }
    if env.cancel.load(Ordering::Relaxed) {
        crate::observability::event("warn", "swarm_debate_cancelled", || {
            serde_json::json!({ "phase": "judge" })
        });
        report.push_str("\n[swarm=debate | cancelled]");
        return (true, report);
    }
    crate::observability::event("info", "swarm_debate_judge_started", || {
        serde_json::json!({ "rounds": spec.rounds })
    });
    let prompt = format!(
        "You are {}. Judge the complete debate below for the task. \
         Choose PROPOSER, CHALLENGER, or TIE and finish with one line in this format:\n\
         DEBATE_VERDICT: PROPOSER|CHALLENGER|TIE | confidence=0.85 | summary=...\n\
         Replace PROPOSER|CHALLENGER|TIE with exactly one winner.\n\n\
         Task:\n{task}\n\nDebate history:\n{history}",
        spec.judge.role
    );
    let judge_res = env.run_target(
        &agent_to_target(&spec.judge), &prompt, None, None, false, budget,
    );
    let mut winner = "INVALID";
    let mut confidence = 0.5_f32;
    let mut summary = "";
    if let Some(verdict) = judge_res.raw_body.lines().rev().take(25)
        .find_map(|line| line.trim().strip_prefix("DEBATE_VERDICT:"))
    {
        let mut parts = verdict.split('|').map(str::trim);
        winner = match parts.next() {
            Some("PROPOSER") => "PROPOSER",
            Some("CHALLENGER") => "CHALLENGER",
            Some("TIE") => "TIE",
            _ => "INVALID",
        };
        for part in parts {
            if let Some(value) = part.strip_prefix("confidence=") {
                confidence = value.trim().parse::<f32>().ok()
                    .filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 1.0)).unwrap_or(0.5);
            } else if let Some(value) = part.strip_prefix("summary=") {
                summary = value.trim();
            }
        }
    }
    let failed = judge_res.failed || winner == "INVALID";
    crate::observability::event(
        if failed { "warn" } else { "info" },
        "swarm_debate_judge_finished",
        || serde_json::json!({
            "failed": failed,
            "winner": winner,
            "confidence": confidence,
        }),
    );
    (failed, format!(
        "[swarm=debate | winner={winner} | rounds={} | confidence={confidence:.2}]\n\n\
         ### Judge\n{}\n\nSummary: {summary}{report}",
        spec.rounds, judge_res.formatted
    ))
}
