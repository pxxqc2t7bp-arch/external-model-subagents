//! `ext-models-mcp` entrypoint:
//! - `ext-models-mcp [mcp]`         : Run heap-bounded stdio MCP server (default)
//! - `ext-models-mcp status ...`    : Print discovered harnesses, providers, models, MCPs, and heap usage
//! - `ext-models-mcp run ...`       : Direct headless CLI runner (ideal for async background jobs)

use ext_models_mcp::alloc_cap;
use ext_models_mcp::debug_cli;
use ext_models_mcp::harness::{build_status_report_with_options, execute_dispatch};
use ext_models_mcp::mcp_server::serve_stdio;
use serde_json::{json, Map, Value};
use std::env;
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn main() {
    alloc_cap::init_from_env();
    ext_models_mcp::metrics::init();
    let args: Vec<String> = env::args().skip(1).collect();

    let code = match args.first().map(String::as_str) {
        None | Some("mcp") => serve_stdio(),
        Some("status") => cmd_status(&args[1..]),
        Some("run") => cmd_run(&args[1..]),
        Some(cmd @ ("debug" | "trace" | "replay")) => cmd_debug_or_replay(cmd, &args[1..]),
        Some("-h") | Some("--help") | Some("help") => {
            print_help();
            0
        }
        Some("-V") | Some("--version") | Some("version") => {
            println!("ext-models-mcp {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Some(other) => {
            eprintln!("Unknown subcommand '{other}'. Run `ext-models-mcp --help`.");
            2
        }
    };
    process::exit(code);
}

fn cmd_debug_or_replay(cmd: &str, args: &[String]) -> i32 {
    let Some(call_id) = args.first() else {
        eprintln!("Usage: ext-models-mcp {cmd} <call_id>");
        return 2;
    };
    let (is_err, out) = debug_cli::run(cmd, call_id);
    println!("{out}");
    if is_err { 1 } else { 0 }
}

fn cmd_status(args: &[String]) -> i32 {
    let mut probe = false;
    let mut metrics = false;
    let mut cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--probe" | "--models" => probe = true,
            "--metrics" => metrics = true,
            "--cwd" if i + 1 < args.len() => {
                cwd = PathBuf::from(&args[i + 1]);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    println!("{}", build_status_report_with_options(&cwd, probe, metrics));
    0
}

fn cmd_run(args: &[String]) -> i32 {
    let mut obj = Map::new();
    let mut mcps: Vec<Value> = Vec::new();
    let mut context_files: Vec<Value> = Vec::new();
    let mut positional_prompt: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let next = args.get(i + 1);
        match flag {
            "--harness" | "-H" if next.is_some() => {
                obj.insert("harness".into(), json!(next.unwrap()));
                i += 1;
            }
            "--provider" | "-P" if next.is_some() => {
                obj.insert("provider".into(), json!(next.unwrap()));
                i += 1;
            }
            "--model" | "-m" if next.is_some() => {
                obj.insert("model".into(), json!(next.unwrap()));
                i += 1;
            }
            "--effort" if next.is_some() => {
                obj.insert("effort".into(), json!(next.unwrap()));
                i += 1;
            }
            "--cwd" | "-C" if next.is_some() => {
                obj.insert("cwd".into(), json!(next.unwrap()));
                i += 1;
            }
            "--mode" if next.is_some() => {
                obj.insert("mode".into(), json!(next.unwrap()));
                i += 1;
            }
            "--rules" | "--project-rules" if next.is_some() => {
                obj.insert("project_rules".into(), json!(next.unwrap()));
                i += 1;
            }
            "--caps" | "--capabilities" if next.is_some() => {
                obj.insert("capabilities".into(), json!(next.unwrap()));
                i += 1;
            }
            "--mcp" if next.is_some() => {
                mcps.push(json!(next.unwrap()));
                i += 1;
            }
            "--file" if next.is_some() => {
                context_files.push(json!(next.unwrap()));
                i += 1;
            }
            "--handoff" if next.is_some() => {
                obj.insert("handoff_from".into(), json!(next.unwrap()));
                i += 1;
            }
            "--review" if next.is_some() => {
                obj.insert("review_base".into(), json!(next.unwrap()));
                i += 1;
            }
            "--session" | "--resume" if next.is_some() => {
                obj.insert("session_id".into(), json!(next.unwrap()));
                i += 1;
            }
            "--keep-session" => {
                obj.insert("keep_session".into(), json!(true));
            }
            "--worktree" | "-w" => {
                obj.insert("worktree".into(), json!(true));
            }
            "--swarm" if next.is_some() => {
                let raw = next.unwrap();
                let content = if raw.trim_start().starts_with('{') {
                    raw.clone()
                } else {
                    match std::fs::read_to_string(raw) {
                        Ok(txt) => txt,
                        Err(e) => {
                            eprintln!("Failed to read --swarm file '{raw}': {e}");
                            return 2;
                        }
                    }
                };
                match serde_json::from_str::<Value>(&content) {
                    Ok(v) => {
                        obj.insert("swarm".into(), v);
                    }
                    Err(e) => {
                        eprintln!("Invalid --swarm JSON: {e}");
                        return 2;
                    }
                }
                i += 1;
            }
            "--timeout-ms" if next.is_some() => {
                if let Ok(n) = next.unwrap().parse::<u64>() {
                    obj.insert("timeout_ms".into(), json!(n));
                }
                i += 1;
            }
            "--max-chars" if next.is_some() => {
                if let Ok(n) = next.unwrap().parse::<u64>() {
                    obj.insert("max_chars".into(), json!(n));
                }
                i += 1;
            }
            "--" => {
                positional_prompt.extend_from_slice(&args[i + 1..]);
                break;
            }
            "-h" | "--help" => {
                print_help();
                return 0;
            }
            other => positional_prompt.push(other.to_string()),
        }
        i += 1;
    }

    if !mcps.is_empty() {
        obj.insert("mcps".into(), Value::Array(mcps));
    }
    if !context_files.is_empty() {
        obj.insert("context_files".into(), Value::Array(context_files));
    }

    let joined = positional_prompt.join(" ");
    let prompt = if (joined.is_empty() || joined == "-") && !io::stdin().is_terminal() {
        let mut buf = String::new();
        let _ = io::stdin().take(256 * 1024).read_to_string(&mut buf);
        buf
    } else {
        joined
    };
    obj.insert("prompt".into(), json!(prompt));

    let cancel = Arc::new(AtomicBool::new(false));
    let (is_err, out) = execute_dispatch(&Value::Object(obj), cancel);
    println!("{out}");
    if is_err { 1 } else { 0 }
}

fn print_help() {
    println!(
        "ext-models-mcp {} — Heap-bounded Rust MCP hub for external CLI coding harnesses\n\n\
         USAGE:\n  \
           ext-models-mcp [mcp]                                  Start stdio MCP server\n  \
           ext-models-mcp status [--probe] [--metrics] [--cwd D] Inspect harnesses, models, metrics & heap\n  \
           ext-models-mcp run [OPTIONS] [PROMPT|-]               Run a task directly (reads stdin if '-')\n  \
           ext-models-mcp debug|trace|replay <call_id>           Inspect or replay a previous dispatch\n\n\
         RUN OPTIONS:\n  \
           -H, --harness <codex|claude|trae|zcode|auto>\n  \
           -P, --provider <id>       Provider override (e.g. 'openai' vs 'litellm')\n  \
           -m, --model <id>          Model ID or shorthand\n  \
           -C, --cwd <dir>           Target project directory\n  \
           --mode <plan|write|full>  Permission mode (default: plan)\n  \
           --rules <smart|on|off>    Project AGENTS.md/CLAUDE.md policy (default: smart)\n  \
           --caps <code|full>        External capability profile (default: code)\n  \
           --mcp <name>              Mount specific MCP server (repeatable)\n  \
           -w, --worktree            Execute inside isolated dirty-aware git worktree\n  \
           --swarm <json|file>       Execute structured Swarm ('panel'|'pipeline'|'debate')\n  \
           --review <base|uncommitted>\n  \
           --handoff <call_id>       Zero-copy pipe previous call artifact\n  \
           --session <id|last>       Resume session\n  \
           --keep-session            Persist session on disk",
        env!("CARGO_PKG_VERSION")
    );
}
