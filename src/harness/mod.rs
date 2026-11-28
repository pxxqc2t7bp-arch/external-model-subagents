//! Core `HarnessAdapter` trait, three-axis orthogonal policy types, session tracker,
//! zero-copy handoff prompt assembler, and parallel `targets` dispatcher.

pub mod catalog;
pub mod claude;
pub mod codex;
pub mod custom;
pub mod deepseek;
pub mod discovery;
pub mod http_client;
pub mod kimi;
pub mod pi;
pub mod trae;
pub mod zcode;

use crate::alloc_cap::heap_stats_kb;
use crate::error::record_worker_panic;
use crate::harness::catalog::{
    canonical_harness_id, resolve_route_rich, scan_registered_inventories, HarnessInventory,
};
use crate::sandbox::{git_diff_stat, run_supervised_bounded, CommandSpec, WorktreeSession};
use crate::stream::{
    next_call_id, persist_and_budget, read_file_capped, read_handoff, TailBuffer, DEFAULT_MAX_CHARS,
};
use crate::swarm::types::SwarmSpec;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkMode {
    Plan,
    Write,
    Full,
}

impl WorkMode {
    pub fn parse(s: Option<&str>) -> Self {
        match s.unwrap_or("plan").trim().to_ascii_lowercase().as_str() {
            "write" | "edit" | "build" | "accept_edits" | "acceptedits" | "workspace-write" => {
                Self::Write
            }
            "full" | "yolo" | "bypass" | "auto" | "danger-full-access" => Self::Full,
            _ => Self::Plan,
        }
    }
}

/// Axis 2: Project rules (`AGENTS.md` / `CLAUDE.md`) control — 100% decoupled from MCP/plugin capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectRulesMode {
    Smart,
    On,
    Off,
}

impl ProjectRulesMode {
    pub fn parse(s: Option<&str>) -> Self {
        let fallback = std::env::var("EXT_MODELS_DEFAULT_RULES").ok();
        let raw = s.or(fallback.as_deref()).unwrap_or("smart");
        match raw.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "inherit" | "strict" => Self::On,
            "off" | "false" | "none" | "clean" => Self::Off,
            _ => Self::Smart,
        }
    }
}

/// Axis 3: External MCP & plugin capability mounting — 100% decoupled from `ProjectRulesMode`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityMode {
    Code,
    Selective(Vec<String>),
    Full,
}

impl CapabilityMode {
    pub fn parse(caps: Option<&str>, mcps: &[String]) -> Self {
        if !mcps.is_empty() {
            return Self::Selective(mcps.to_vec());
        }
        let fallback = std::env::var("EXT_MODELS_DEFAULT_CAPS").ok();
        let raw = caps.or(fallback.as_deref()).unwrap_or("code");
        match raw.trim().to_ascii_lowercase().as_str() {
            "full" | "all" | "plugins" => Self::Full,
            _ => Self::Code,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DispatchTarget {
    pub harness: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub prompt_override: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RunRequest {
    pub call_id: String,
    pub prompt: String,
    pub effective_prompt: String,
    pub worker_system_patch: Option<String>,
    pub cwd: PathBuf,
    pub mode: WorkMode,
    pub project_rules: ProjectRulesMode,
    pub capabilities: CapabilityMode,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub session_id: Option<String>,
    pub keep_session: bool,
    pub review_base: Option<String>,
    pub worktree: bool,
    pub timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_chars: usize,
}

pub struct ParseState {
    pub session_id: Option<String>,
    pub used_model: Option<String>,
    pub last_message: Option<String>,
    pub is_error: bool,
    pub raw_tail: TailBuffer,
    http_reasoning_end: Option<usize>,
}

impl Default for ParseState {
    fn default() -> Self {
        Self::new()
    }
}

impl ParseState {
    pub fn new() -> Self {
        Self {
            session_id: None,
            used_model: None,
            last_message: None,
            is_error: false,
            raw_tail: TailBuffer::new(32 * 1024),
            http_reasoning_end: None,
        }
    }
}

/// Clean orthogonal adapter contract implemented by each built-in or declarative harness.
pub trait HarnessAdapter: Send + Sync {
    fn id(&self) -> &str;
    fn strengths(&self) -> &str;
    fn is_available(&self) -> bool;
    fn binary_display(&self) -> String;

    fn model_inventory(&self) -> Option<HarnessInventory> {
        None
    }

    /// Environment variable to redact from provider output before persisting it.
    fn response_key_env(&self) -> Option<&str> {
        None
    }

    /// Build base invocation (`program`, core flags, prompt transport, session/review flags).
    fn build_base(&self, req: &RunRequest) -> Result<CommandSpec, String>;

    /// Axis 1: Always strip GUI popups, TTY hooks, and self-recursive MCP loops.
    fn apply_headless_guard(&self, _spec: &mut CommandSpec) {}

    /// Axis 2: Apply `project_rules` (`On` | `Smart` | `Off`) without touching MCP/plugin flags.
    fn apply_project_rules(&self, _rules: ProjectRulesMode, _req: &RunRequest, _spec: &mut CommandSpec) {}

    /// Axis 3: Apply `capabilities` (`Code` | `Selective` | `Full`) without touching rule flags.
    fn apply_capabilities(
        &self,
        _caps: &CapabilityMode,
        _inv: &HarnessInventory,
        _cwd: &Path,
        _spec: &mut CommandSpec,
    ) {
    }

    /// Maximum bytes retained per single stdout line for this harness.
    fn max_stdout_line_bytes(&self) -> usize {
        crate::stream::MAX_LINE_BYTES
    }

    /// Consume a single bounded stdout line in `O(1)` memory.
    fn on_stdout_line(&self, line: &str, state: &mut ParseState);

    /// Consume a single bounded stdout line along with its line-truncation status.
    fn on_stdout_line_bounded(&self, line: &str, _truncated: bool, state: &mut ParseState) {
        self.on_stdout_line(line, state);
    }
}

static LAST_SESSIONS: OnceLock<RwLock<HashMap<(String, PathBuf), String>>> = OnceLock::new();
type InvCacheEntry = Option<(Instant, PathBuf, Arc<BTreeMap<String, HarnessInventory>>)>;
static INV_CACHE: OnceLock<Mutex<InvCacheEntry>> = OnceLock::new();

fn session_table() -> &'static RwLock<HashMap<(String, PathBuf), String>> {
    LAST_SESSIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Serializes and caches `scan_inventories` briefly (5s) so concurrent `tools/call` bursts
/// never parse catalog JSON files simultaneously on multiple threads under tight heap caps.
fn cached_inventories(
    cwd: &Path,
    adapters: &[Box<dyn HarnessAdapter>],
) -> Arc<BTreeMap<String, HarnessInventory>> {
    let lock = INV_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((ts, ref cached_cwd, ref arc_map)) = *guard {
        if cached_cwd == cwd && ts.elapsed() < Duration::from_secs(5) {
            return Arc::clone(arc_map);
        }
    }
    let fresh = Arc::new(scan_registered_inventories(cwd, false, adapters));
    *guard = Some((Instant::now(), cwd.to_path_buf(), Arc::clone(&fresh)));
    fresh
}

pub fn registry() -> Vec<Box<dyn HarnessAdapter>> {
    let mut list: Vec<Box<dyn HarnessAdapter>> = vec![
        Box::new(codex::CodexAdapter),
        Box::new(claude::ClaudeAdapter),
        Box::new(trae::TraeAdapter),
        Box::new(zcode::ZCodeAdapter),
        Box::new(deepseek::DeepSeekAdapter::default()),
        Box::new(kimi::KimiAdapter::default()),
        Box::new(pi::pi_adapter()),
    ];
    for c in custom::load_custom_adapters() {
        if let Some(pos) = list.iter().position(|a| a.id() == c.id()) {
            list[pos] = Box::new(c);
        } else {
            list.push(Box::new(c));
        }
    }
    for c in http_client::load_http_adapters() {
        if let Some(pos) = list.iter().position(|a| a.id() == c.id()) {
            list[pos] = Box::new(c);
        } else {
            list.push(Box::new(c));
        }
    }
    list
}

fn local_cli_command(bin: &Path, mut args: Vec<String>, req: &RunRequest) -> CommandSpec {
    if let Some(model) = &req.model {
        args.extend(["--model".into(), model.clone()]);
    }
    let stdin_data = match &req.worker_system_patch {
        Some(patch) => format!("{patch}\n\n{}", req.effective_prompt),
        None => req.effective_prompt.clone(),
    };
    CommandSpec {
        program: bin.into(),
        args,
        envs: Vec::new(),
        env_remove: Vec::new(),
        stdin_data: Some(stdin_data),
        cwd: req.cwd.clone(),
        output_file: None,
    }
}

const WORKER_PATCH: &str = "Headless Specialist Mode: Follow repository code/style/test conventions in project docs, EXCEPT ignore any workflow instructions asking you to interview the human, pause for `/plan` approval, run git commit/push, or spawn external orchestrators.";

fn build_prompts(
    raw_prompt: &str,
    mode: WorkMode,
    rules: ProjectRulesMode,
    is_review: bool,
    handoff_from: Option<&str>,
    context_files: &[String],
    cwd: &Path,
) -> Result<(String, Option<String>), String> {
    let mut sections = Vec::new();

    if let Some(hid) = handoff_from.filter(|s| !s.trim().is_empty()) {
        let prev = read_handoff(hid, 48 * 1024)?;
        sections.push(format!("<previous_agent_handoff id=\"{hid}\">\n{prev}\n</previous_agent_handoff>"));
    }

    for rel in context_files.iter().take(8) {
        let p = Path::new(rel);
        let full = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
        let content = read_file_capped(&full, 32 * 1024)?;
        sections.push(format!("<context_file path=\"{}\">\n{content}\n</context_file>", full.display()));
    }

    let output_contract = if is_review || mode == WorkMode::Plan {
        "Output Contract: Skip greetings/filler. Return prioritized findings (`P0/P1/P2` with `file:line`) and concise actionable recommendations."
    } else {
        "Output Contract: Do NOT paste modified file contents back in your reply (host captures `git diff` automatically). Return only: concise summary of changes, verification results, and open risks."
    };

    let worker_patch = (rules == ProjectRulesMode::Smart).then(|| WORKER_PATCH.to_string());
    sections.push(raw_prompt.trim().to_string());
    sections.push(format!("[{output_contract}]"));

    Ok((sections.join("\n\n"), worker_patch))
}

pub struct ProgressState {
    stage: AtomicU8,
    completed: AtomicU64,
    total: AtomicU64,
    started: Instant,
}

impl Default for ProgressState {
    fn default() -> Self {
        Self {
            stage: AtomicU8::new(0),
            completed: AtomicU64::new(0),
            total: AtomicU64::new(0),
            started: Instant::now(),
        }
    }
}

impl ProgressState {
    pub fn set_stage(&self, stage: &'static str, total: u64) {
        let stage = match stage {
            "swarm_panel" => 2,
            "swarm_pipeline" => 3,
            "swarm_debate" => 4,
            "running" => 1,
            _ => 0,
        };
        self.total.store(total, Ordering::Relaxed);
        self.stage.store(stage, Ordering::Release);
    }

    pub fn inc_completed(&self) {
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn notification(&self, token: &Value) -> Value {
        let stage = ["routing", "running", "swarm_panel", "swarm_pipeline", "swarm_debate"]
            [self.stage.load(Ordering::Acquire) as usize];
        let completed = self.completed.load(Ordering::Relaxed);
        json!({
            "progressToken": token, "progress": completed, "progressStatus": stage,
            "details": {"stage": stage, "elapsed_ms": self.started.elapsed().as_millis(),
                "completed": completed, "total": self.total.load(Ordering::Relaxed)}
        })
    }
}

/// Execute a single or multi-target (`targets`) dispatch call and format a compact report.
pub fn execute_dispatch(args: &Value, cancel: Arc<AtomicBool>) -> (bool, String) {
    execute_dispatch_with_progress(args, cancel, None)
}

pub fn execute_dispatch_with_progress(
    args: &Value,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<ProgressState>>,
) -> (bool, String) {
    let started = Instant::now();
    let timed_out = Arc::new(AtomicBool::new(false));
    crate::metrics::dispatch_started();
    crate::observability::event("info", "dispatch_start", || json!({}));
    let result = dispatch_inner(args, cancel, progress, Arc::clone(&timed_out));
    let elapsed = started.elapsed();
    crate::metrics::dispatch_finished(elapsed, result.0, timed_out.load(Ordering::Relaxed));
    crate::observability::event(if result.0 { "error" } else { "info" }, "dispatch_end", || {
        json!({"failed": result.0, "duration_ms": elapsed.as_millis(),
            "timed_out": timed_out.load(Ordering::Relaxed)})
    });
    result
}

fn dispatch_inner(
    args: &Value,
    cancel: Arc<AtomicBool>,
    progress: Option<Arc<ProgressState>>,
    timed_out: Arc<AtomicBool>,
) -> (bool, String) {
    let raw_prompt = args.get("prompt").and_then(Value::as_str).unwrap_or("").trim();
    let review_base = args
        .get("review_base")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    if raw_prompt.is_empty() && review_base.is_none() {
        return (true, "`prompt` (or `review_base`) is required.".into());
    }

    let raw_cwd = args
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    if !raw_cwd.is_dir() {
        return (
            true,
            format!("working directory does not exist: {}", raw_cwd.display()),
        );
    }
    let cwd = fs::canonicalize(&raw_cwd).unwrap_or(raw_cwd);

    let mode = WorkMode::parse(args.get("mode").and_then(Value::as_str));
    let project_rules = ProjectRulesMode::parse(args.get("project_rules").and_then(Value::as_str));
    let mcps: Vec<String> = args
        .get("mcps")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();
    let capabilities = CapabilityMode::parse(args.get("capabilities").and_then(Value::as_str), &mcps);
    let handoff_from = args.get("handoff_from").and_then(Value::as_str);
    let context_files: Vec<String> = args
        .get("context_files")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default();

    let session_req = args
        .get("session_id")
        .or_else(|| args.get("resume"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let keep_session = args
        .get("keep_session")
        .and_then(Value::as_bool)
        .unwrap_or(session_req.is_some());
    let worktree = args.get("worktree").and_then(Value::as_bool).unwrap_or(false);
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(300_000)
        .clamp(5_000, 3_600_000);
    let idle_floor = 10_000_u64.min(timeout_ms);
    let idle_timeout_ms = args
        .get("idle_timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(180_000)
        .clamp(idle_floor, timeout_ms);
    let total_max_chars = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MAX_CHARS)
        .clamp(800, 40_000);

    let adapters = Arc::new(registry());
    let available: BTreeSet<String> = adapters
        .iter()
        .filter(|a| a.is_available())
        .map(|a| a.id().to_string())
        .collect();
    let inventories = cached_inventories(&cwd, &adapters);

    let env = DispatchEnv {
        cwd: cwd.clone(),
        mode,
        project_rules,
        capabilities,
        context_files,
        review_base,
        worktree,
        timeout_ms,
        idle_timeout_ms,
        adapters,
        inventories,
        available,
        cancel,
        progress,
        timed_out,
    };

    match SwarmSpec::from_args(args) {
        Ok(Some(swarm_spec)) => {
            return crate::swarm::execute_swarm(
                swarm_spec,
                raw_prompt,
                handoff_from,
                &env,
                total_max_chars,
            );
        }
        Err(e) => return (true, e.to_string()),
        Ok(None) => {}
    }

    let targets = parse_targets(args);
    if targets.len() > 4 {
        return (true, "at most 4 parallel `targets` are supported per dispatch.".into());
    }

    if let Some(progress) = &env.progress {
        progress.set_stage("routing", targets.len() as u64);
    }
    let per_target_budget = (total_max_chars / targets.len().max(1)).max(800);
    if targets.len() == 1 {
        let res = env.run_target(
            &targets[0],
            raw_prompt,
            handoff_from,
            session_req,
            keep_session,
            per_target_budget,
        );
        return (res.failed, res.formatted);
    }

    // Multi-target parallel execution on compact 256 KiB-stack worker threads.
    let mut handles = Vec::with_capacity(targets.len());
    for t in targets {
        let env_c = env.clone();
        let prompt_base = raw_prompt.to_string();
        let hf_c = handoff_from.map(String::from);

        let handle = thread::Builder::new()
            .name("ext-target".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let res = env_c.run_target(
                    &t,
                    &prompt_base,
                    hf_c.as_deref(),
                    None,
                    keep_session,
                    per_target_budget,
                );
                (res.failed, res.formatted)
            });
        if let Ok(h) = handle {
            handles.push(h);
        }
    }

    let mut all_failed = true;
    let mut blocks = Vec::with_capacity(handles.len());
    for (idx, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok((is_err, txt)) => {
                if !is_err {
                    all_failed = false;
                }
                blocks.push(format!("### Target #{}\n{}", idx + 1, txt));
            }
            Err(payload) => {
                let err = record_worker_panic(&format!("target_{}", idx + 1), payload.as_ref());
                blocks.push(format!("### Target #{}\n{err}", idx + 1));
            }
        }
    }
    (all_failed, blocks.join("\n\n---\n\n"))
}

#[derive(Clone)]
pub struct DispatchEnv {
    pub cwd: PathBuf,
    pub mode: WorkMode,
    pub project_rules: ProjectRulesMode,
    pub capabilities: CapabilityMode,
    pub context_files: Vec<String>,
    pub review_base: Option<String>,
    pub worktree: bool,
    pub timeout_ms: u64,
    pub idle_timeout_ms: u64,
    pub adapters: Arc<Vec<Box<dyn HarnessAdapter>>>,
    pub inventories: Arc<BTreeMap<String, HarnessInventory>>,
    pub available: BTreeSet<String>,
    pub cancel: Arc<AtomicBool>,
    pub progress: Option<Arc<ProgressState>>,
    timed_out: Arc<AtomicBool>,
}

pub struct TargetRunResult {
    pub call_id: String,
    pub failed: bool,
    pub raw_body: String,
    pub formatted: String,
}

impl DispatchEnv {
    pub fn run_target(
        &self,
        target: &DispatchTarget,
        base_prompt: &str,
        handoff_from: Option<&str>,
        session_req: Option<String>,
        keep_session: bool,
        budget_chars: usize,
    ) -> TargetRunResult {
        let call_id = next_call_id();
        let mut metadata = json!({
            "call_id": call_id, "harness": target.harness, "provider": target.provider,
            "model": target.model, "cwd": self.cwd,
            "mode": match self.mode { WorkMode::Plan => "plan", WorkMode::Write => "write", WorkMode::Full => "full" },
            "project_rules": match self.project_rules { ProjectRulesMode::Smart => "smart", ProjectRulesMode::On => "on", ProjectRulesMode::Off => "off" },
            "prompt": target.prompt_override.as_deref().unwrap_or(base_prompt),
            "status": "failed", "artifact": null, "patch": null,
            "effort": target.effort, "review_base": self.review_base, "worktree": self.worktree,
            "context_files": self.context_files, "handoff_from": handoff_from,
            "session_id": session_req, "keep_session": keep_session,
            "timeout_ms": self.timeout_ms, "idle_timeout_ms": self.idle_timeout_ms,
            "max_chars": budget_chars,
            "capabilities": if self.capabilities == CapabilityMode::Full { "full" } else { "code" },
            "mcps": match &self.capabilities { CapabilityMode::Selective(names) => json!(names), _ => json!([]) }
        });
        let route = match resolve_route_rich(
            target.harness.as_deref(),
            target.provider.as_deref(),
            target.model.as_deref(),
            self.review_base.is_some(),
            &self.inventories,
            &self.available,
        ) {
            Ok(r) => r,
            Err(e) => {
                crate::observability::event("warn", "routing_failed", || json!({"call_id": call_id}));
                let msg = format!("Routing error: {e}");
                metadata["status"] = json!("routing_error");
                metadata["error"] = json!(msg);
                crate::debug_cli::save_meta(&call_id, metadata, None);
                return TargetRunResult {
                    call_id,
                    failed: true,
                    raw_body: msg.clone(),
                    formatted: msg,
                };
            }
        };

        let automatic = target.harness.as_deref().is_none_or(|h| h == "auto");
        crate::metrics::routing(automatic);
        crate::observability::event("info", "routing_result", || {
            json!({"call_id": call_id, "harness": route.harness, "automatic": automatic})
        });
        metadata["harness"] = json!(route.harness);
        metadata["provider"] = json!(route.provider);
        metadata["model"] = json!(route.model);
        let Some(adapter) = self.adapters.iter().find(|a| a.id() == route.harness) else {
            let msg = format!("unknown harness '{}'", route.harness);
            return TargetRunResult {
                call_id,
                failed: true,
                raw_body: msg.clone(),
                formatted: msg,
            };
        };

        if self.worktree
            && self.review_base.as_deref().is_some_and(|b| {
                b.eq_ignore_ascii_case("uncommitted") || b.eq_ignore_ascii_case("working")
            })
        {
            let msg = "worktree=true cannot be combined with review_base='uncommitted' \
                (worktree snapshots dirty state into a baseline commit; run read-only uncommitted review without worktree)"
                .to_string();
            return TargetRunResult {
                call_id,
                failed: true,
                raw_body: msg.clone(),
                formatted: msg,
            };
        }

        if let Some(progress) = &self.progress {
            if progress.stage.load(Ordering::Acquire) == 0 {
                progress.set_stage("running", progress.total.load(Ordering::Relaxed).max(1));
            }
        }

        let wt_session = if self.worktree {
            match WorktreeSession::create(&self.cwd, &call_id) {
                Ok(ws) => Some(ws),
                Err(e) => {
                    let msg = format!("Failed to create isolated git worktree: {e}");
                    return TargetRunResult {
                        call_id,
                        failed: true,
                        raw_body: msg.clone(),
                        formatted: msg,
                    };
                }
            }
        } else {
            None
        };
        let exec_cwd = wt_session
            .as_ref()
            .map(|w| w.worktree_cwd.clone())
            .unwrap_or_else(|| self.cwd.clone());

        let prompt_text = target.prompt_override.as_deref().unwrap_or(base_prompt);
        let (effective_prompt, worker_system_patch) = match build_prompts(
            prompt_text,
            self.mode,
            self.project_rules,
            self.review_base.is_some(),
            handoff_from,
            &self.context_files,
            &exec_cwd,
        ) {
            Ok(pair) => pair,
            Err(e) => {
                if let Some(ws) = wt_session {
                    ws.abort();
                }
                return TargetRunResult {
                    call_id,
                    failed: true,
                    raw_body: e.clone(),
                    formatted: e,
                };
            }
        };

        let resolved_sid = session_req.and_then(|s| {
            if s.eq_ignore_ascii_case("last") {
                session_table()
                    .read()
                    .ok()
                    .and_then(|m| m.get(&(route.harness.clone(), self.cwd.clone())).cloned())
                    .or_else(|| Some("last".into()))
            } else {
                Some(s)
            }
        });

        let req = RunRequest {
            call_id: call_id.clone(),
            prompt: prompt_text.to_string(),
            effective_prompt,
            worker_system_patch,
            cwd: exec_cwd.clone(),
            mode: self.mode,
            project_rules: self.project_rules,
            capabilities: self.capabilities.clone(),
            provider: route.provider.clone(),
            model: route.model.clone(),
            effort: target.effort.clone(),
            session_id: resolved_sid,
            keep_session,
            review_base: self.review_base.clone(),
            worktree: self.worktree,
            timeout_ms: self.timeout_ms,
            idle_timeout_ms: self.idle_timeout_ms,
            max_chars: budget_chars,
        };

        // Orthogonal 4-stage CommandSpec pipeline: base -> headless_guard -> project_rules -> capabilities
        let mut spec = match adapter.build_base(&req) {
            Ok(s) => s,
            Err(e) => {
                if let Some(ws) = wt_session {
                    ws.abort();
                }
                return TargetRunResult {
                    call_id,
                    failed: true,
                    raw_body: e.clone(),
                    formatted: e,
                };
            }
        };
        adapter.apply_headless_guard(&mut spec);
        adapter.apply_project_rules(self.project_rules, &req, &mut spec);
        let empty_inv = HarnessInventory::default();
        let inv = self.inventories.get(&route.harness).unwrap_or(&empty_inv);
        adapter.apply_capabilities(&self.capabilities, inv, &exec_cwd, &mut spec);

        let mut state = ParseState::new();
        let mut outcome = match run_supervised_bounded(
            &spec,
            Duration::from_millis(self.timeout_ms),
            Duration::from_millis(self.idle_timeout_ms),
            adapter.max_stdout_line_bytes(),
            Arc::clone(&self.cancel),
            |line, trunc| adapter.on_stdout_line_bounded(line, trunc, &mut state),
        ) {
            Ok(o) => o,
            Err(e) => {
                if let Some(ws) = wt_session {
                    ws.abort();
                }
                return TargetRunResult {
                    call_id,
                    failed: true,
                    raw_body: e.clone(),
                    formatted: e,
                };
            }
        };

        // Prefer dedicated output file (e.g. Codex `-o <file>`) if written and non-empty.
        let file_msg = spec
            .output_file
            .as_ref()
            .and_then(|p| {
                let txt = read_file_capped(p, 2 * 1024 * 1024).ok();
                let _ = fs::remove_file(p);
                txt
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let mut raw_reply = file_msg
            .or_else(|| state.last_message.take())
            .unwrap_or_else(|| state.raw_tail.as_str().to_string());
        if let Some(key) = adapter
            .response_key_env()
            .and_then(|name| std::env::var(name).ok())
            .filter(|key| !key.is_empty())
        {
            for output in [&mut raw_reply, &mut outcome.stderr_tail] {
                if output.contains(key.as_str()) {
                    *output = output.replace(key.as_str(), "[redacted]");
                }
            }
        }

        let wt_summary = wt_session.map(|ws| ws.finalize());
        let direct_diff = if wt_summary.is_none() && self.mode != WorkMode::Plan {
            git_diff_stat(&exec_cwd)
        } else {
            None
        };

        let export_failed = wt_summary
            .as_ref()
            .is_some_and(|w| w.export_error.is_some());
        let failed = outcome.cancelled
            || outcome.timed_out
            || outcome.idle_timed_out
            || state.is_error
            || outcome.exit_code != 0
            || export_failed;

        if keep_session && !failed {
            if let Some(ref sid) = state.session_id {
                if let Ok(mut tbl) = session_table().write() {
                    tbl.insert((route.harness.clone(), self.cwd.clone()), sid.clone());
                }
            }
        }

        let no_stdout = raw_reply.trim().is_empty();
        let mut full_body = if failed && no_stdout {
            format!("(no stdout; stderr: {})", outcome.stderr_tail)
        } else {
            raw_reply
        };

        if failed && !outcome.stderr_tail.is_empty() && !full_body.contains(&outcome.stderr_tail) {
            full_body.push_str("\n\n[stderr tail]\n");
            full_body.push_str(&outcome.stderr_tail);
        }

        let display_body = if !failed && no_stdout {
            "(completed with empty stdout)"
        } else {
            &full_body
        };
        let (budgeted_body, saved_path) =
            persist_and_budget(&call_id, &route.harness, display_body, budget_chars);

        if outcome.timed_out || outcome.idle_timed_out {
            self.timed_out.store(true, Ordering::Relaxed);
        }

        let mut meta = vec![format!("id={call_id}"), format!("harness={}", route.harness)];
        if let Some(ref p) = route.provider {
            meta.push(format!("provider={p}"));
        }
        if let Some(m) = state.used_model.or(route.model) {
            metadata["model"] = json!(m);
            meta.push(format!("model={m}"));
        }
        if let Some(sid) = state.session_id {
            meta.push(format!("session_id={sid}"));
        }
        meta.push(format!("{}ms", outcome.duration_ms));
        let status_label = if outcome.cancelled {
            meta.push("status=cancelled".into());
            "cancelled"
        } else if outcome.timed_out {
            meta.push("status=timeout".into());
            "timeout"
        } else if outcome.idle_timed_out {
            meta.push("status=idle_timeout".into());
            "idle_timeout"
        } else if failed {
            meta.push(format!("exit={}", outcome.exit_code));
            "failed"
        } else {
            "success"
        };
        if let Some(ref p) = saved_path {
            meta.push(format!("artifact={}", p.display()));
            metadata["artifact"] = json!(p);
        }
        metadata["duration_ms"] = json!(outcome.duration_ms);
        metadata["exit_code"] = json!(outcome.exit_code);
        metadata["failed"] = json!(failed);
        metadata["status"] = json!(status_label);

        let mut out = format!("[{}]\n", meta.join(" | "));
        if let Some(note) = route.note {
            out.push_str(&format!("[route: {note}]\n"));
        }
        out.push('\n');
        out.push_str(&budgeted_body);

        if let Some(wt) = wt_summary {
            if wt.skipped_untracked > 0 {
                out.push_str(&format!(
                    "\n\n[worktree note: skipped {} untracked item(s) (symlinks, >2 MiB, or >50 file cap)]",
                    wt.skipped_untracked
                ));
            }
            if let Some(err) = wt.export_error {
                let kept = wt
                    .preserved_worktree
                    .map(|p| format!("; checkout preserved at {}", p.display()))
                    .unwrap_or_default();
                out.push_str(&format!("\n\n[worktree export error: {err}{kept}]"));
            } else {
                if !wt.diff_stat.is_empty() {
                    out.push_str(&format!("\n\n[worktree diff]\n{}", wt.diff_stat));
                }
                if let Some(pp) = wt.patch_path {
                    metadata["patch"] = json!(pp);
                    out.push_str(&format!(
                        "\n[patch ready: `git apply {}`]",
                        pp.display()
                    ));
                }
            }
        } else if let Some(st) = direct_diff {
            out.push_str(&format!("\n\n[workspace git status]\n{st}"));
        }

        crate::debug_cli::save_meta(&call_id, metadata, adapter.response_key_env());
        if let Some(progress) = &self.progress {
            progress.inc_completed();
        }

        TargetRunResult {
            call_id,
            failed,
            raw_body: full_body,
            formatted: out,
        }
    }
}

fn parse_targets(args: &Value) -> Vec<DispatchTarget> {
    if let Some(arr) = args.get("targets").and_then(Value::as_array) {
        if !arr.is_empty() {
            return arr
                .iter()
                .map(|t| DispatchTarget {
                    harness: t.get("harness").and_then(Value::as_str).map(canonical_harness_id),
                    provider: t.get("provider").and_then(Value::as_str).map(String::from),
                    model: t.get("model").and_then(Value::as_str).map(String::from),
                    effort: t.get("effort").and_then(Value::as_str).map(String::from),
                    prompt_override: t.get("prompt").and_then(Value::as_str).map(String::from),
                })
                .collect();
        }
    }
    vec![DispatchTarget {
        harness: args
            .get("harness")
            .and_then(Value::as_str)
            .map(canonical_harness_id),
        provider: args.get("provider").and_then(Value::as_str).map(String::from),
        model: args.get("model").and_then(Value::as_str).map(String::from),
        effort: args.get("effort").and_then(Value::as_str).map(String::from),
        prompt_override: None,
    }]
}

/// Build the `status` JSON report (capabilities, models, providers, and live Rust heap stats).
pub fn build_status_report(cwd: &Path, probe_models: bool) -> String {
    build_status_report_with_options(cwd, probe_models, false)
}

/// Build the `status` JSON report with optional cumulative process metrics.
pub fn build_status_report_with_options(
    cwd: &Path,
    probe_models: bool,
    include_metrics: bool,
) -> String {
    let adapters = registry();
    let inventories = scan_registered_inventories(cwd, probe_models, &adapters);
    let (cur_kb, peak_kb, limit_kb) = heap_stats_kb();

    let mut harnesses_json = Vec::with_capacity(adapters.len());
    for a in &adapters {
        let inv = inventories.get(a.id()).cloned().unwrap_or_default();
        harnesses_json.push(json!({
            "harness": a.id(),
            "ready": a.is_available(),
            "binary": a.binary_display(),
            "strengths": a.strengths(),
            "auth_mode": inv.auth_mode,
            "default_provider": inv.default_provider,
            "default_model": inv.default_model,
            "providers": inv.providers,
            "mcps": inv.mcps,
            "plugins": inv.plugins,
        }));
    }

    let mut report = json!({
        "hub_version": env!("CARGO_PKG_VERSION"),
        "heap_kb": {
            "current": cur_kb,
            "peak": peak_kb,
            "hard_limit": limit_kb,
        },
        "harnesses": harnesses_json,
    });
    if include_metrics {
        report["metrics"] = crate::metrics::snapshot();
    }
    serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
}
