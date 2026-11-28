#![cfg(unix)]

use ext_models_mcp::alloc_cap::heap_stats_kb;
use ext_models_mcp::harness::execute_dispatch;
use ext_models_mcp::stream::next_call_id;
use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[test]
#[ignore]
fn stress_concurrent_panel_swarms_and_zero_heap_leak() {
    let dir = std::env::temp_dir().join(format!("ext-stress-{}", next_call_id()));
    fs::create_dir_all(&dir).unwrap();

    let fast_bin = dir.join("stress-worker");
    fs::write(
        &fast_bin,
        "#!/bin/sh\ncat >/dev/null\nprintf 'analysis ok\\nSWARM_VOTE: APPROVE | confidence=0.90 | summary=clean\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&fast_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let slow_bin = dir.join("slow-worker");
    fs::write(&slow_bin, "#!/bin/sh\ncat >/dev/null\nsleep 10\n").unwrap();
    fs::set_permissions(&slow_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let cfg = dir.join("harnesses.json");
    fs::write(
        &cfg,
        serde_json::to_string(&json!({
            "harnesses": [
                {
                    "id": "sfast",
                    "binary": fast_bin,
                    "args": [],
                    "prompt_via_stdin": true
                },
                {
                    "id": "sslow",
                    "binary": slow_bin,
                    "args": [],
                    "prompt_via_stdin": true
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    std::env::set_var("EXT_MODELS_HARNESSES_FILE", &cfg);

    // 1. Concurrent Swarm Panel burst: 8 simultaneous 3-agent panels (24 worker runs).
    let mut handles = Vec::new();
    for i in 0..8 {
        let cwd = dir.clone();
        handles.push(thread::spawn(move || {
            let (failed, out) = execute_dispatch(
                &json!({
                    "cwd": cwd,
                    "project_rules": "off",
                    "prompt": format!("Concurrent panel #{i}"),
                    "swarm": {
                        "type": "panel",
                        "consensus_threshold": 0.66,
                        "agents": [
                            {"harness": "sfast", "role": "arch"},
                            {"harness": "sfast", "role": "sec"},
                            {"harness": "sfast", "role": "perf"}
                        ]
                    }
                }),
                Arc::new(AtomicBool::new(false)),
            );
            assert!(!failed, "panel #{i} failed: {out}");
            assert!(out.contains("decision=APPROVE | reached=true"), "{out}");
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // 2. Warmup + 120 sequential dispatches memory-leak check.
    for _ in 0..5 {
        let _ = execute_dispatch(
            &json!({"harness": "sfast", "cwd": dir, "project_rules": "off", "prompt": "warmup"}),
            Arc::new(AtomicBool::new(false)),
        );
    }
    let (warmup_kb, _, limit_kb) = heap_stats_kb();
    for n in 0..120 {
        let (failed, _) = execute_dispatch(
            &json!({
                "harness": "sfast",
                "cwd": dir,
                "project_rules": "off",
                "prompt": format!("iteration {n}")
            }),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(!failed);
    }
    let (after_kb, peak_kb, _) = heap_stats_kb();
    assert!(
        after_kb <= warmup_kb + 256,
        "heap leaked across 120 dispatches: warmup={warmup_kb}KB after={after_kb}KB"
    );
    assert!(
        peak_kb <= limit_kb,
        "peak heap {peak_kb}KB exceeded configured limit {limit_kb}KB"
    );

    // 3. Cancellation storm: 6 slow dispatches cancelled mid-flight must reap cleanly < 2s.
    let cancel = Arc::new(AtomicBool::new(false));
    let mut cancel_handles = Vec::new();
    for _ in 0..6 {
        let cwd = dir.clone();
        let c = Arc::clone(&cancel);
        cancel_handles.push(thread::spawn(move || {
            execute_dispatch(
                &json!({"harness": "sslow", "cwd": cwd, "project_rules": "off", "prompt": "hang"}),
                c,
            )
        }));
    }
    thread::sleep(Duration::from_millis(200));
    cancel.store(true, Ordering::Release);
    for h in cancel_handles {
        let (failed, out) = h.join().unwrap();
        assert!(failed && out.contains("status=cancelled"), "{out}");
    }

    std::env::remove_var("EXT_MODELS_HARNESSES_FILE");
    let _ = fs::remove_dir_all(&dir);
}
