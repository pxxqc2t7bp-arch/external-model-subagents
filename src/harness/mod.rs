//! Core `HarnessAdapter` trait, three-axis orthogonal policy types, session tracker,
//! zero-copy handoff prompt assembler, and parallel `targets` dispatcher.

pub mod catalog;
pub mod claude;
pub mod codex;
pub mod custom;
pub mod discovery;
pub mod trae;
pub mod zcode;

use crate::alloc_cap::heap_stats_kb;
use crate::harness::catalog::{
    canonical_harness_id, resolve_route, scan_inventories, HarnessInventory,
};
use crate::sandbox::{git_diff_stat, run_supervised_bounded, CommandSpec, WorktreeSession};
use crate::stream::{
    next_call_id, persist_and_budget, read_file_capped, read_handoff, TailBuffer, DEFAULT_MAX_CHARS,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
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
        }
    }
}

/// Clean orthogonal adapter contract implemented by each built-in or declarative CLI harness.
pub trait HarnessAdapter: Send + Sync {
    fn id(&self) -> &str;
    fn strengths(&self) -> &str;
    fn is_available(&self) -> bool;
    fn binary_display(&self) -> String;

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

static LAST_SESSIONS: OnceLock<Mutex<HashMap<(String, PathBuf), String>>> = OnceLock::new();
type InvCacheEntry = Option<(Instant, PathBuf, Arc<BTreeMap<String, HarnessInventory>>)>;
static INV_CACHE: OnceLock<Mutex<InvCacheEntry>> = OnceLock::new();

fn session_table() -> &'static Mutex<HashMap<(String, PathBuf), String>> {
    LAST_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Serializes and caches `scan_inventories` briefly (5s) so concurrent `tools/call` bursts
/// never parse catalog JSON files simultaneously on multiple threads under tight heap caps.
fn cached_inventories(cwd: &Path) -> Arc<BTreeMap<String, HarnessInventory>> {
    let lock = INV_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((ts, ref cached_cwd, ref arc_map)) = *guard {
        if cached_cwd == cwd && ts.elapsed() < Duration::from_secs(5) {
            return Arc::clone(arc_map);
        }
    }
    let fresh = Arc::new(scan_inventories(cwd, false));
    *guard = Some((Instant::now(), cwd.to_path_buf(), Arc::clone(&fresh)));
    fresh
}

pub fn registry() -> Vec<Box<dyn HarnessAdapter>> {
    let mut list: Vec<Box<dyn HarnessAdapter>> = vec![
        Box::new(codex::CodexAdapter),
        Box::new(claude::ClaudeAdapter),
        Box::new(trae::TraeAdapter),
        Box::new(zcode::ZCodeAdapter),
    ];
    for c in custom::load_custom_adapters() {
        list.push(Box::new(c));
    }
    list
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

/// Execute a single or multi-target (`targets`) dispatch call and format a compact report.
pub fn execute_dispatch(args: &Value, cancel: Arc<AtomicBool>) -> (bool, String) {
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

    let targets = parse_targets(args);
    if targets.len() > 4 {
        return (true, "at most 4 parallel `targets` are supported per dispatch.".into());
    }

    let adapters = Arc::new(registry());
    let available: BTreeSet<String> = adapters
        .iter()
        .filter(|a| a.is_available())
        .map(|a| a.id().to_string())
        .collect();
    let inventories = cached_inventories(&cwd);

    let per_target_budget = (total_max_chars / targets.len().max(1)).max(800);
    let multi = targets.len() > 1;

    if !multi {
        let t = &targets[0];
        return run_single_target(
            t,
            raw_prompt,
            &cwd,
            mode,
            project_rules,
            &capabilities,
            handoff_from,
            &context_files,
            session_req,
            keep_session,
            review_base,
            worktree,
            timeout_ms,
            idle_timeout_ms,
            per_target_budget,
            &adapters,
            &inventories,
            &available,
            cancel,
        );
    }

    // Multi-target parallel execution on compact 256 KiB-stack worker threads.
    let mut handles = Vec::with_capacity(targets.len());
    for t in targets {
        let prompt_base = raw_prompt.to_string();
        let cwd_c = cwd.clone();
        let caps_c = capabilities.clone();
        let hf_c = handoff_from.map(String::from);
        let cf_c = context_files.clone();
        let rev_c = review_base.clone();
        let ad_c = Arc::clone(&adapters);
        let inv_c = Arc::clone(&inventories);
        let av_c = available.clone();
        let can_c = Arc::clone(&cancel);
        // Forward requested worktree isolation flag to each parallel target worker.
        let wt_c = worktree;

        let handle = thread::Builder::new()
            .name("ext-target".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                run_single_target(
                    &t,
                    &prompt_base,
                    &cwd_c,
                    mode,
                    project_rules,
                    &caps_c,
                    hf_c.as_deref(),
                    &cf_c,
                    None,
                    keep_session,
                    rev_c,
                    wt_c,
                    timeout_ms,
                    idle_timeout_ms,
                    per_target_budget,
                    &ad_c,
                    &inv_c,
                    &av_c,
                    can_c,
                )
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
            Err(_) => blocks.push(format!("### Target #{}\n[worker thread panicked]", idx + 1)),
        }
    }
    (all_failed, blocks.join("\n\n---\n\n"))
}

#[allow(clippy::too_many_arguments)]
fn run_single_target(
    target: &DispatchTarget,
    base_prompt: &str,
    cwd: &Path,
    mode: WorkMode,
    project_rules: ProjectRulesMode,
    capabilities: &CapabilityMode,
    handoff_from: Option<&str>,
    context_files: &[String],
    session_req: Option<String>,
    keep_session: bool,
    review_base: Option<String>,
    worktree: bool,
    timeout_ms: u64,
    idle_timeout_ms: u64,
    budget_chars: usize,
    adapters: &[Box<dyn HarnessAdapter>],
    inventories: &BTreeMap<String, HarnessInventory>,
    available: &BTreeSet<String>,
    cancel: Arc<AtomicBool>,
) -> (bool, String) {
    let route = match resolve_route(
        target.harness.as_deref(),
        target.provider.as_deref(),
        target.model.as_deref(),
        review_base.is_some(),
        inventories,
        available,
    ) {
        Ok(r) => r,
        Err(e) => return (true, format!("Routing error: {e}")),
    };

    let Some(adapter) = adapters.iter().find(|a| a.id() == route.harness) else {
        return (true, format!("unknown harness '{}'", route.harness));
    };

    if worktree
        && review_base.as_deref().is_some_and(|b| {
            b.eq_ignore_ascii_case("uncommitted") || b.eq_ignore_ascii_case("working")
        })
    {
        return (
            true,
            "worktree=true cannot be combined with review_base='uncommitted' \
             (worktree snapshots dirty state into a baseline commit; run read-only uncommitted review without worktree)"
                .into(),
        );
    }

    let call_id = next_call_id();
    let wt_session = if worktree {
        match WorktreeSession::create(cwd, &call_id) {
            Ok(ws) => Some(ws),
            Err(e) => return (true, format!("Failed to create isolated git worktree: {e}")),
        }
    } else {
        None
    };
    let exec_cwd = wt_session
        .as_ref()
        .map(|w| w.worktree_cwd.clone())
        .unwrap_or_else(|| cwd.to_path_buf());

    let prompt_text = target.prompt_override.as_deref().unwrap_or(base_prompt);
    let (effective_prompt, worker_system_patch) = match build_prompts(
        prompt_text,
        mode,
        project_rules,
        review_base.is_some(),
        handoff_from,
        context_files,
        &exec_cwd,
    ) {
        Ok(pair) => pair,
        Err(e) => {
            if let Some(ws) = wt_session {
                ws.abort();
            }
            return (true, e);
        }
    };

    let resolved_sid = session_req.and_then(|s| {
        if s.eq_ignore_ascii_case("last") {
            session_table()
                .lock()
                .ok()
                .and_then(|m| m.get(&(route.harness.clone(), cwd.to_path_buf())).cloned())
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
        mode,
        project_rules,
        capabilities: capabilities.clone(),
        provider: route.provider.clone(),
        model: route.model.clone(),
        effort: target.effort.clone(),
        session_id: resolved_sid,
        keep_session,
        review_base,
        worktree,
        timeout_ms,
        idle_timeout_ms,
        max_chars: budget_chars,
    };

    // Orthogonal 4-stage CommandSpec pipeline: base -> headless_guard -> project_rules -> capabilities
    let mut spec = match adapter.build_base(&req) {
        Ok(s) => s,
        Err(e) => {
            if let Some(ws) = wt_session {
                ws.abort();
            }
            return (true, e);
        }
    };
    adapter.apply_headless_guard(&mut spec);
    adapter.apply_project_rules(project_rules, &req, &mut spec);
    let empty_inv = HarnessInventory::default();
    let inv = inventories.get(&route.harness).unwrap_or(&empty_inv);
    adapter.apply_capabilities(capabilities, inv, &exec_cwd, &mut spec);

    let mut state = ParseState::new();
    let outcome = match run_supervised_bounded(
        &spec,
        Duration::from_millis(timeout_ms),
        Duration::from_millis(idle_timeout_ms),
        adapter.max_stdout_line_bytes(),
        cancel,
        |line, trunc| adapter.on_stdout_line_bounded(line, trunc, &mut state),
    ) {
        Ok(o) => o,
        Err(e) => {
            if let Some(ws) = wt_session {
                ws.abort();
            }
            return (true, e);
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

    let raw_reply = file_msg
        .or_else(|| state.last_message.take())
        .unwrap_or_else(|| state.raw_tail.as_str().to_string());

    let wt_summary = wt_session.map(|ws| ws.finalize());
    let direct_diff = if wt_summary.is_none() && mode != WorkMode::Plan {
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
            if let Ok(mut tbl) = session_table().lock() {
                tbl.insert((route.harness.clone(), cwd.to_path_buf()), sid.clone());
            }
        }
    }

    let mut full_body = if raw_reply.trim().is_empty() {
        if failed {
            format!("(no stdout; stderr: {})", outcome.stderr_tail)
        } else {
            "(completed with empty stdout)".to_string()
        }
    } else {
        raw_reply
    };

    if failed && !outcome.stderr_tail.is_empty() && !full_body.contains(&outcome.stderr_tail) {
        full_body.push_str("\n\n[stderr tail]\n");
        full_body.push_str(&outcome.stderr_tail);
    }

    let (budgeted_body, saved_path) =
        persist_and_budget(&call_id, &route.harness, &full_body, budget_chars);

    let mut meta = vec![format!("id={call_id}"), format!("harness={}", route.harness)];
    if let Some(ref p) = route.provider {
        meta.push(format!("provider={p}"));
    }
    if let Some(m) = state.used_model.or(route.model) {
        meta.push(format!("model={m}"));
    }
    if let Some(sid) = state.session_id {
        meta.push(format!("session_id={sid}"));
    }
    meta.push(format!("{}ms", outcome.duration_ms));
    if outcome.cancelled {
        meta.push("status=cancelled".into());
    } else if outcome.timed_out {
        meta.push("status=timeout".into());
    } else if outcome.idle_timed_out {
        meta.push("status=idle_timeout".into());
    } else if failed {
        meta.push(format!("exit={}", outcome.exit_code));
    }
    if let Some(ref p) = saved_path {
        meta.push(format!("artifact={}", p.display()));
    }

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
                out.push_str(&format!(
                    "\n[patch ready: `git apply {}`]",
                    pp.display()
                ));
            }
        }
    } else if let Some(st) = direct_diff {
        out.push_str(&format!("\n\n[workspace git status]\n{st}"));
    }

    (failed, out)
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
    let adapters = registry();
    let inventories = scan_inventories(cwd, probe_models);
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

    let report = json!({
        "hub_version": env!("CARGO_PKG_VERSION"),
        "heap_kb": {
            "current": cur_kb,
            "peak": peak_kb,
            "hard_limit": limit_kb,
        },
        "harnesses": harnesses_json,
    });
    serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".into())
}
