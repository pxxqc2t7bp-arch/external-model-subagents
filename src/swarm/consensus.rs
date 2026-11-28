//! Vote extraction and weighted Swarm consensus.

use super::types::{ConsensusResult, Decision, Vote};

const VOTE_WINDOW: usize = 25;
const SUMMARY_CHARS: usize = 160;

/// Use the most recent vote marker in the output tail.
pub fn extract_vote(agent_id: &str, role: &str, weight: f32, output: &str) -> Vote {
    let marker = output
        .lines()
        .rev()
        .take(VOTE_WINDOW)
        .find_map(|line| line.trim_start().strip_prefix("SWARM_VOTE:"));
    let mut decision = Decision::Abstain;
    let mut confidence = 0.5;
    let mut summary = None;
    if let Some(marker) = marker {
        let (token, fields) = marker.split_once('|').unwrap_or((marker, ""));
        if let Some(parsed) = Decision::parse_token(token) {
            decision = parsed;
            for (key, value) in fields.split('|').filter_map(|field| field.split_once('=')) {
                let value = value.trim();
                match key.trim() {
                    "confidence" => {
                        if let Ok(value) = value.parse::<f32>() {
                            if value.is_finite() {
                                confidence = value.clamp(0.0, 1.0);
                            }
                        }
                    }
                    "summary" => summary = Some(value),
                    _ => {}
                }
            }
        }
    }
    let summary = summary.unwrap_or_else(|| {
        output.lines().rev().map(str::trim).find(|line| !line.is_empty()).unwrap_or("")
    }).chars().take(SUMMARY_CHARS).collect();
    Vote {
        agent_id: agent_id.into(),
        role: role.into(),
        decision,
        confidence,
        weight,
        summary,
    }
}

/// Compare the winner's share of effective weight to the threshold. A tie has no winner.
/// Aggregate confidence is the weight-adjusted mean over non-abstaining votes.
pub fn evaluate_consensus(votes: Vec<Vote>, threshold: f32) -> ConsensusResult {
    let (mut approve, mut reject, mut needs_changes, mut weight) = (0.0, 0.0, 0.0, 0.0);
    for vote in &votes {
        let score = match vote.decision {
            Decision::Approve => &mut approve,
            Decision::Reject => &mut reject,
            Decision::NeedsChanges => &mut needs_changes,
            Decision::Abstain => continue,
        };
        *score += vote.weight * vote.confidence;
        weight += vote.weight;
    }

    let total = approve + reject + needs_changes;
    let (decision, winning_score) = if approve > reject && approve > needs_changes {
        (Decision::Approve, approve)
    } else if reject > approve && reject > needs_changes {
        (Decision::Reject, reject)
    } else if needs_changes > approve && needs_changes > reject {
        (Decision::NeedsChanges, needs_changes)
    } else {
        (Decision::Abstain, 0.0)
    };
    let approval_ratio = if total > 0.0 { approve / total } else { 0.0 };
    let reached = total > 0.0 && decision != Decision::Abstain && winning_score / total >= threshold;
    let confidence = if weight > 0.0 { total / weight } else { 0.0 };
    let reasoning_summary = format!(
        "{}: effective weights approve={approve:.2}, reject={reject:.2}, needs_changes={needs_changes:.2}; consensus {}",
        decision.as_str(),
        if reached { "reached" } else { "not reached" },
    );
    ConsensusResult {
        decision,
        reached,
        approval_ratio,
        threshold,
        confidence,
        votes,
        reasoning_summary,
    }
}
