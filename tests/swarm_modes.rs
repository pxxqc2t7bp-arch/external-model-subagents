#![cfg(unix)]

use ext_models_mcp::harness::execute_dispatch;
use ext_models_mcp::stream::{next_call_id, write_artifact_bytes};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, Arc, Mutex};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Harnesses {
    dir: PathBuf,
    old_config: Option<OsString>,
}

impl Harnesses {
    fn new(scripts: &[(&str, &str)]) -> Self {
        let dir = std::env::temp_dir().join(format!("swarm-modes-{}", next_call_id()));
        fs::create_dir(&dir).unwrap();
        let harnesses: Vec<_> = scripts.iter().map(|(id, body)| {
            let path = dir.join(format!("{id}.sh"));
            fs::write(&path, format!("#!/bin/sh\nset -eu\nprompt=$(cat)\n{body}\n")).unwrap();
            json!({"id": id, "binary": "/bin/sh", "args": [path], "prompt_via_stdin": true})
        }).collect();
        let config = dir.join("harnesses.json");
        fs::write(&config, json!({"harnesses": harnesses}).to_string()).unwrap();
        let old_config = std::env::var_os("EXT_MODELS_HARNESSES_FILE");
        std::env::set_var("EXT_MODELS_HARNESSES_FILE", config);
        Self { dir, old_config }
    }

    fn run(&self, swarm: Value) -> (bool, String) {
        execute_dispatch(&json!({"cwd": self.dir, "prompt": "build", "swarm": swarm,
            "project_rules": "off", "max_chars": 16000}), Arc::new(AtomicBool::new(false)))
    }
}

impl Drop for Harnesses {
    fn drop(&mut self) {
        match &self.old_config {
            Some(value) => std::env::set_var("EXT_MODELS_HARNESSES_FILE", value),
            None => std::env::remove_var("EXT_MODELS_HARNESSES_FILE"),
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn panel_votes_and_single_node_failure() {
    let _lock = ENV_LOCK.lock().unwrap();
    let h = Harnesses::new(&[
        ("yes", "printf 'SWARM_VOTE: APPROVE | confidence=0.90 | summary=ready\\n'"),
        ("no", "printf 'SWARM_VOTE: REJECT | confidence=0.90 | summary=needs work\\n'"),
        ("broken", "printf 'node failed\\n'; exit 7"),
    ]);
    for parallel in [false, true] {
        let (failed, report) = h.run(json!({"type": "panel", "parallel": parallel, "agents": [
            {"harness": "yes", "weight": 2}, {"harness": "no"}, {"harness": "broken"}]}));
        assert!(!failed, "{report}");
        assert!(report.contains("decision=APPROVE | reached=true | approval_ratio=0.67"), "{report}");
        assert!(report.contains("panel_3 (panel_3): ABSTAIN"), "{report}");
        assert!(report.contains("node failed"), "{report}");
    }
}

#[test]
fn pipeline_templates_and_validation_rules() {
    let _lock = ENV_LOCK.lock().unwrap();
    let h = Harnesses::new(&[
        ("source", "printf '%s\\n' '{\"value\":\"{task}\"}'"),
        ("sink", r#"case "$prompt" in
*T=build\|P=\{\"value\":\"\{task\}\"\}\|C=\{\"value\":\"\{task\}\"\}\|U=\{unknown\}*) ;;
*) printf 'template mismatch\n'; exit 9;; esac
printf 'validated\n'"#),
    ]);
    let source = json!({"name": "source", "harness": "source", "output_validation": [
        "non_empty", "json_valid", {"type": "contains", "value": "value"},
        {"type": "min_chars", "value": 10}]});
    let mut sink = json!({"name": "sink", "harness": "sink",
        "input_template": "T={task}|P={prev_output}|C={context.source}|U={unknown}",
        "output_validation": [{"type": "contains", "value": "validated"}]});
    let (failed, report) = h.run(json!({"type": "pipeline", "stages": [source, sink]}));
    assert!(!failed && report.contains("validated"), "{report}");
    for rule in [json!({"type": "contains", "value": "missing"}),
        json!({"type": "min_chars", "value": 100})] {
        let (failed, report) = h.run(json!({"type": "pipeline", "stages": [
            {"harness": "source", "output_validation": [rule]}, {"harness": "sink"}]}));
        assert!(failed && !report.contains("### Stage 2"), "{report}");
    }
    sink["output_validation"] = json!(["json_valid"]);
    let (failed, report) = h.run(json!({"type": "pipeline", "stages": [source, sink]}));
    assert!(failed && report.contains("### Stage 2"), "{report}");
}

#[test]
fn debate_rounds_handoff_and_judge_verdict() {
    let _lock = ENV_LOCK.lock().unwrap();
    let h = Harnesses::new(&[
        ("proposer", r#"printf 'P\n' >> trace
case "$prompt" in
*objection-2*) printf 'response-2\n';;
*objection-1*) printf 'response-1\n';;
*handoff-seed*) printf 'initial-proposal\n';;
*) exit 8;; esac"#),
        ("challenger", r#"printf 'C\n' >> trace
case "$prompt" in
*response-1*) printf 'objection-2\n';;
*initial-proposal*) printf 'objection-1\n';;
*) exit 8;; esac"#),
        ("judge", r#"printf 'J\n' >> trace
for word in initial-proposal objection-1 response-1 objection-2 response-2; do
case "$prompt" in *"$word"*) ;; *) exit 8;; esac
done
printf 'Reasoned judgment\nDEBATE_VERDICT: CHALLENGER | confidence=0.85 | summary=stronger evidence\n'"#),
    ]);
    let id = next_call_id();
    let artifact = write_artifact_bytes(&format!("{id}.md"), b"handoff-seed").unwrap();
    let args = json!({"cwd": h.dir, "prompt": "build", "handoff_from": id,
        "project_rules": "off", "max_chars": 16000, "swarm": {"type": "debate", "rounds": 2,
        "proposer": {"harness": "proposer"}, "challenger": {"harness": "challenger"},
        "judge": {"harness": "judge"}}});
    let (failed, report) = execute_dispatch(&args, Arc::new(AtomicBool::new(false)));
    fs::remove_file(artifact).unwrap();
    assert!(!failed, "{report}");
    assert!(report.starts_with("[swarm=debate | winner=CHALLENGER | rounds=2 | confidence=0.85]"), "{report}");
    assert!(report.contains("Summary: stronger evidence"), "{report}");
    assert!(report.contains("Round 2: proposer"), "{report}");
    assert_eq!(fs::read_to_string(h.dir.join("trace")).unwrap(), "P\nC\nP\nC\nP\nJ\n");
    let (failed, report) = execute_dispatch(&args, Arc::new(AtomicBool::new(true)));
    assert!(failed && report.contains("cancelled"), "{report}");
    assert_eq!(fs::read_to_string(h.dir.join("trace")).unwrap(), "P\nC\nP\nC\nP\nJ\n");
}
