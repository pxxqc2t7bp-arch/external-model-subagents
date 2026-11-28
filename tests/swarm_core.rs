use ext_models_mcp::stream::write_artifact;
use ext_models_mcp::swarm::consensus::{evaluate_consensus, extract_vote};
use ext_models_mcp::swarm::context::SwarmContext;
use ext_models_mcp::swarm::registry::{AgentRegistry, AgentStatus};
use ext_models_mcp::swarm::types::{
    Decision, Destination, Finding, MessagePayload, Severity, SwarmAgent, SwarmMessage, SwarmSpec,
};
use serde_json::json;

fn sample_agent(role: &str, cap: &str) -> SwarmAgent {
    SwarmAgent {
        harness: Some("codex".into()),
        provider: None,
        model: None,
        effort: None,
        role: role.into(),
        capabilities: vec![cap.into()],
        weight: 1.0,
    }
}

#[test]
fn registry_capability_lookup_and_message_routing() {
    let reg = AgentRegistry::new();
    let rx1 = reg.register("a1".into(), sample_agent("reviewer", "Security-Audit"));
    let rx2 = reg.register("a2".into(), sample_agent("proposer", "rust-core"));

    assert_eq!(reg.find_by_capability("security-audit"), vec!["a1".to_string()]);
    assert_eq!(reg.find_by_capability("PROPOSER"), vec!["a2".to_string()]);

    let bcast = SwarmMessage {
        from: "coord".into(),
        to: Destination::Broadcast,
        payload: MessagePayload::TaskProposal {
            task: "audit".into(),
            vote_id: "v1".into(),
        },
        correlation_id: None,
    };
    assert_eq!(reg.send(bcast), 2);
    assert!(rx1.try_recv().is_ok());
    assert!(rx2.try_recv().is_ok());

    reg.set_status("a2", AgentStatus::Completed);
    assert!(reg.find_by_capability("proposer").is_empty());

    let direct = SwarmMessage {
        from: "a1".into(),
        to: Destination::Agent("a2".into()),
        payload: MessagePayload::StageHandoff {
            stage: "s1".into(),
            call_id: "c1".into(),
        },
        correlation_id: None,
    };
    assert_eq!(reg.send(direct), 0, "completed agent must not receive messages");
    reg.unregister("a1");
    assert!(reg.find_by_capability("reviewer").is_empty());
}

#[test]
fn context_single_pass_template_note_truncation_and_handoff() {
    let ctx = SwarmContext::new();
    ctx.set_note("arch".into(), "pure-std".into());
    // Oversized multibyte UTF-8 note must truncate cleanly at <= 16 KiB boundary.
    ctx.set_note("big".into(), "中".repeat(8000));
    let got_big = ctx.get_note("big").unwrap();
    assert!(got_big.len() <= 16 * 1024);

    // Single-pass guarantee: `{prev_output}` containing `{context.arch}` must NOT double-expand!
    let rendered = ctx.render_template(
        "Task={task} | Arch={context.arch} | Prev={prev_output} | Unknown={keep}",
        "refactor",
        "literal {context.arch}",
    );
    assert_eq!(
        rendered,
        "Task=refactor | Arch=pure-std | Prev=literal {context.arch} | Unknown={keep}"
    );

    ctx.add_finding(
        "f1".into(),
        Finding {
            author: "a1".into(),
            severity: Severity::High,
            summary: "lock scope".into(),
        },
    );
    assert_eq!(ctx.findings_snapshot().len(), 1);

    let cid = format!("cswarm_{}", std::process::id());
    let saved = write_artifact(&format!("{cid}-codex.md"), "stage one result").unwrap();
    ctx.record_stage_handoff("analyze".into(), cid);
    assert_eq!(
        ctx.load_stage_output("analyze", 4096).unwrap(),
        "stage one result"
    );
    let _ = std::fs::remove_file(saved);
}

#[test]
fn consensus_vote_extraction_weighted_majority_tie_and_abstain() {
    let out1 = "Checked files.\nSWARM_VOTE: APPROVE | confidence=0.90 | summary=solid implementation\n";
    let out2 = "Found edge case.\nSWARM_VOTE: REJECT | confidence=0.80 | summary=missing cancel check\n";
    let out3 = "No explicit marker here\nFinal thought on module.\n";

    let v1 = extract_vote("a1", "lead", 2.0, out1);
    let v2 = extract_vote("a2", "critic", 1.0, out2);
    let v3 = extract_vote("a3", "observer", 1.0, out3);

    assert_eq!(v1.decision, Decision::Approve);
    assert!((v1.confidence - 0.90).abs() < 1e-5);
    assert_eq!(v1.summary, "solid implementation");
    assert_eq!(v3.decision, Decision::Abstain);
    assert_eq!(v3.summary, "Final thought on module.");

    // Weighted: Approve = 2.0 * 0.9 = 1.8, Reject = 1.0 * 0.8 = 0.8 => 1.8 / 2.6 = 0.692 >= 0.66
    let res = evaluate_consensus(vec![v1, v2, v3], 0.66);
    assert_eq!(res.decision, Decision::Approve);
    assert!(res.reached);

    // Exact tie must yield Abstain and reached=false
    let t1 = extract_vote("x1", "r1", 1.0, "SWARM_VOTE: APPROVE | confidence=0.8");
    let t2 = extract_vote("x2", "r2", 1.0, "SWARM_VOTE: REJECT | confidence=0.8");
    let tie = evaluate_consensus(vec![t1, t2], 0.5);
    assert_eq!(tie.decision, Decision::Abstain);
    assert!(!tie.reached);

    // Verify boundary parser for SwarmSpec
    let spec = SwarmSpec::from_args(&json!({
        "swarm": {
            "type": "debate",
            "proposer": {"harness": "codex", "role": "proposer"},
            "challenger": {"harness": "claude", "role": "challenger"},
            "judge": {"harness": "zcode", "role": "judge"},
            "rounds": 9
        }
    }))
    .unwrap()
    .unwrap();
    match spec {
        SwarmSpec::Debate(d) => assert_eq!(d.rounds, 4, "rounds must clamp to 1..=4 at boundary"),
        _ => panic!("expected Debate variant"),
    }
}
