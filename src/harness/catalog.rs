//! Provider-aware model catalog & capability scanner + strict version-preserving router.
//!
//! Designed for open-source portability across:
//! - Official subscriptions (OpenAI OAuth, Anthropic Max OAuth)
//! - Enterprise/custom Model Gateways (LiteLLM, OneAPI, OpenRouter, Bedrock, Vertex)
//! - Zero secret leakage: never reads or emits API keys, tokens, or private URLs.

use crate::harness::discovery::{bins, home_dir};
use crate::stream::{artifact_dir, write_artifact};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, Default)]
pub struct HarnessInventory {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub auth_mode: String,
    /// `provider_id -> list of known model IDs` (empty list means pass-through gateway).
    pub providers: BTreeMap<String, Vec<String>>,
    pub mcps: Vec<String>,
    pub plugins: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ResolvedRoute {
    pub harness: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub note: Option<String>,
}

pub fn scan_inventories(cwd: &Path, probe_trae: bool) -> BTreeMap<String, HarnessInventory> {
    let mut map = BTreeMap::new();
    map.insert("codex".into(), scan_codex());
    map.insert("claude".into(), scan_claude(cwd));
    map.insert("trae".into(), scan_trae(probe_trae));
    map.insert("zcode".into(), scan_zcode());
    apply_user_model_overrides(&mut map);
    map
}

/// Resolve `(harness, provider, model)` with strict version preservation (never silently
/// downgrading e.g. `5.4-pro` -> `5.2-pro`, and never routing `opus` to a gateway-only
/// Claude instance that doesn't serve Opus).
pub fn resolve_route(
    requested_harness: Option<&str>,
    requested_provider: Option<&str>,
    requested_model: Option<&str>,
    is_review: bool,
    inventories: &BTreeMap<String, HarnessInventory>,
    available_harnesses: &BTreeSet<String>,
) -> Result<ResolvedRoute, String> {
    let norm_harness = requested_harness
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("auto"))
        .map(canonical_harness_id);

    if let Some(ref h) = norm_harness {
        if !available_harnesses.contains(h) {
            return Err(format!(
                "harness '{h}' is not installed or ready on this machine (available: {:?})",
                available_harnesses
            ));
        }
    }

    // Support `<provider>/<model>` shorthand when `<provider>` matches a configured provider.
    let (mut prov_opt, mut model_opt) = (
        requested_provider
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
        requested_model
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
    );

    if prov_opt.is_none() {
        if let Some(ref raw_m) = model_opt {
            if let Some((lhs, rhs)) = raw_m.split_once('/') {
                let known_prov = inventories.values().any(|inv| inv.providers.contains_key(lhs))
                    || lhs.eq_ignore_ascii_case("openai");
                if known_prov && !rhs.is_empty() {
                    prov_opt = Some(lhs.to_string());
                    model_opt = Some(rhs.to_string());
                }
            }
        }
    }

    // Case 1: No specific model requested -> pick requested harness or best default harness.
    let Some(query_model) = model_opt else {
        let h = norm_harness.unwrap_or_else(|| {
            if is_review && available_harnesses.contains("codex") {
                "codex".into()
            } else {
                ["codex", "claude", "trae", "zcode"]
                    .into_iter()
                    .find(|id| available_harnesses.contains(*id))
                    .unwrap_or("codex")
                    .to_string()
            }
        });
        return Ok(ResolvedRoute {
            harness: h,
            provider: prov_opt,
            model: None,
            note: None,
        });
    };

    // Case 2: Harness explicitly specified -> resolve inside that harness or pass through safely.
    if let Some(h) = norm_harness {
        let Some(inv) = inventories.get(&h) else {
            return Ok(ResolvedRoute {
                harness: h,
                provider: prov_opt,
                model: Some(query_model),
                note: None,
            });
        };
        if let Some((matched_prov, matched_model)) =
            match_in_inventory(inv, prov_opt.as_deref(), &query_model)
        {
            let final_prov = prov_opt.or_else(|| {
                if inv.default_provider.as_deref() != Some(matched_prov.as_str())
                    && h == "codex"
                {
                    Some(matched_prov)
                } else {
                    None
                }
            });
            return Ok(ResolvedRoute {
                harness: h,
                provider: final_prov,
                model: Some(matched_model),
                note: None,
            });
        }

        // Check if user hit a dangerous cross-version mismatch (e.g. asked Claude for 5.4-pro when Claude only has 5.2-pro).
        if let Some(near_miss) = find_version_mismatch_hint(&query_model, inv) {
            let exact_elsewhere = find_exact_across_all(&query_model, inventories, available_harnesses);
            let hint = exact_elsewhere
                .map(|(eh, ep, em)| format!("; exact match is on harness='{eh}' (provider='{ep}', model='{em}')"))
                .unwrap_or_default();
            return Err(format!(
                "model '{query_model}' is not in '{h}' catalog (closest version mismatch: '{near_miss}' — refusing silent version downgrade{hint})"
            ));
        }

        // Pass through verbatim for uncached enterprise gateways (LiteLLM / OneAPI / new releases).
        return Ok(ResolvedRoute {
            harness: h,
            provider: prov_opt,
            model: Some(query_model),
            note: Some("pass-through (uncached model id)".into()),
        });
    }

    // Case 3: `harness` omitted / "auto" -> search across all ready harnesses.
    let mut candidates: Vec<(String, String, String, i32)> = Vec::new();
    for h in available_harnesses {
        if let Some(inv) = inventories.get(h) {
            if let Some((mp, mm)) = match_in_inventory(inv, prov_opt.as_deref(), &query_model) {
                let mut score = score_candidate(h, &mm, &query_model, is_review);
                if inv.default_provider.as_deref() == Some(mp.as_str()) {
                    score += 5;
                }
                candidates.push((h.clone(), mp, mm, score));
            }
        }
    }

    candidates.sort_by_key(|b| std::cmp::Reverse(b.3));
    if let Some((best_h, best_p, best_m, _)) = candidates.first() {
        let others: Vec<String> = candidates
            .iter()
            .skip(1)
            .map(|(h, _, m, _)| format!("{h}:{m}"))
            .collect();
        let inv = inventories.get(best_h);
        let need_prov_override = best_h == "codex"
            && inv.and_then(|i| i.default_provider.as_deref()) != Some(best_p.as_str());
        let final_prov = prov_opt.or_else(|| need_prov_override.then(|| best_p.clone()));
        let note = if others.is_empty() {
            Some(format!("auto-routed -> {best_h} ({best_m})"))
        } else {
            Some(format!(
                "auto-routed -> {best_h} ({best_m}); also in [{}]",
                others.join(", ")
            ))
        };
        return Ok(ResolvedRoute {
            harness: best_h.clone(),
            provider: final_prov,
            model: Some(best_m.clone()),
            note,
        });
    }

    Err(format!(
        "could not auto-route model '{query_model}' across local catalogs; specify `harness` explicitly to pass through to a custom gateway"
    ))
}

pub fn canonical_harness_id(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "codex" | "codex-gpt" | "gpt" | "openai" => "codex".into(),
        "claude" | "claude-code" | "claude_code" => "claude".into(),
        "trae" | "traecli" | "trae-agent" => "trae".into(),
        "zcode" | "zcode-agent" => "zcode".into(),
        other => other.to_string(),
    }
}

// --- Version-preserving token matcher ---

const CHANNEL_PREFIXES: &[&str] = &[
    "claude", "openai", "anthropic", "gateway", "proxy", "ark", "byteplus", "standard",
    "coding", "agent", "plan", "doubao",
];

fn split_version_and_words(s: &str) -> (Vec<String>, Vec<String>, bool) {
    let lower = s.to_ascii_lowercase();
    let wants_max = lower.contains("max") || lower.contains("[1m]") || lower.contains("1m");
    let clean = lower.replace("[1m]", " ");
    let mut nums = Vec::new();
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut cur_is_digit: Option<bool> = None;

    for ch in clean.chars() {
        if ch.is_ascii_alphanumeric() {
            let is_d = ch.is_ascii_digit();
            if cur_is_digit == Some(is_d) || cur_is_digit.is_none() {
                cur.push(ch);
                cur_is_digit = Some(is_d);
            } else {
                push_tok(&mut nums, &mut words, &cur, cur_is_digit.unwrap_or(false));
                cur.clear();
                cur.push(ch);
                cur_is_digit = Some(is_d);
            }
        } else if !cur.is_empty() {
            push_tok(&mut nums, &mut words, &cur, cur_is_digit.unwrap_or(false));
            cur.clear();
            cur_is_digit = None;
        }
    }
    if !cur.is_empty() {
        push_tok(&mut nums, &mut words, &cur, cur_is_digit.unwrap_or(false));
    }
    (nums, words, wants_max)
}

fn push_tok(nums: &mut Vec<String>, words: &mut Vec<String>, tok: &str, is_digit: bool) {
    if is_digit {
        nums.push(tok.to_string());
    } else if tok != "max" {
        words.push(tok.to_string());
    }
}

fn match_in_inventory(
    inv: &HarnessInventory,
    prov_filter: Option<&str>,
    query: &str,
) -> Option<(String, String)> {
    let q_lower = query.to_ascii_lowercase();
    let (q_nums, q_words, q_max) = split_version_and_words(query);
    let mut best: Option<(String, String, i32)> = None;

    for (prov, models) in &inv.providers {
        if let Some(pf) = prov_filter {
            if !prov.eq_ignore_ascii_case(pf) {
                continue;
            }
        }
        let (_, prov_words, _) = split_version_and_words(prov);
        for m in models {
            if m.eq_ignore_ascii_case(&q_lower) {
                return Some((prov.clone(), m.clone()));
            }
            let (m_nums, m_words, m_max) = split_version_and_words(m);
            // Strict version rule: if query specifies numbers (e.g. [5, 4]), candidate MUST match them exactly!
            if !q_nums.is_empty() && m_nums != q_nums {
                continue;
            }
            if q_words.is_empty() {
                continue;
            }
            let stripped_m_words: Vec<&str> = m_words
                .iter()
                .map(String::as_str)
                .filter(|w| {
                    !CHANNEL_PREFIXES.contains(w)
                        && *w != "gpt"
                        && !prov_words.iter().any(|pw| pw == *w)
                })
                .collect();
            let all_q_present = q_words.iter().all(|qw| m_words.iter().any(|mw| mw == qw));
            if !all_q_present {
                continue;
            }
            // Prevent bare "gpt" from matching random models unless variant matches.
            let extra_core = stripped_m_words
                .iter()
                .filter(|w| !q_words.iter().any(|qw| qw == **w))
                .count();
            if extra_core > 0 && q_nums.is_empty() {
                continue;
            }

            let mut score = 100 - (extra_core as i32 * 10);
            if q_max == m_max {
                score += 20;
            } else if m_max && !q_max {
                score -= 15;
            }
            // When query omitted version numbers (e.g. "sonnet"), prefer newer version numbers (4.6 > 4.5).
            if q_nums.is_empty() {
                let ver_bonus: i32 = m_nums
                    .iter()
                    .enumerate()
                    .map(|(i, n)| n.parse::<i32>().unwrap_or(0) * if i == 0 { 10 } else { 1 })
                    .sum();
                score += ver_bonus;
            }
            if best.as_ref().is_none_or(|b| score > b.2) {
                best = Some((prov.clone(), m.clone(), score));
            }
        }
    }
    best.map(|(p, m, _)| (p, m))
}

fn find_version_mismatch_hint(query: &str, inv: &HarnessInventory) -> Option<String> {
    let (q_nums, q_words, _) = split_version_and_words(query);
    if q_nums.is_empty() || q_words.is_empty() {
        return None;
    }
    let distinctive: Vec<&String> = q_words
        .iter()
        .filter(|w| *w != "gpt" && !CHANNEL_PREFIXES.contains(&w.as_str()))
        .collect();
    if distinctive.is_empty() {
        return None;
    }
    for models in inv.providers.values() {
        for m in models {
            let (m_nums, m_words, _) = split_version_and_words(m);
            if m_nums != q_nums && distinctive.iter().all(|dw| m_words.contains(dw)) {
                return Some(m.clone());
            }
        }
    }
    None
}

fn find_exact_across_all(
    query: &str,
    inventories: &BTreeMap<String, HarnessInventory>,
    available: &BTreeSet<String>,
) -> Option<(String, String, String)> {
    for h in available {
        if let Some(inv) = inventories.get(h) {
            if let Some((p, m)) = match_in_inventory(inv, None, query) {
                return Some((h.clone(), p, m));
            }
        }
    }
    None
}

fn score_candidate(harness: &str, model_id: &str, query: &str, is_review: bool) -> i32 {
    let mut s = 50;
    if model_id.eq_ignore_ascii_case(query) {
        s += 30;
    }
    let ml = model_id.to_ascii_lowercase();
    let (_, _, _) = split_version_and_words(model_id);
    if is_review && harness == "codex" {
        s += 25;
    }
    if (ml.starts_with("gpt-") || ml.contains("-gpt-")) && harness == "codex" {
        s += 15;
    } else if ml.starts_with("claude-opus") || ml.starts_with("claude-sonnet") || ml == "opus" {
        if harness == "claude" {
            s += 15;
        }
    } else if (ml.contains("seed") || ml.contains("kimi")) && harness == "trae" {
        s += 10;
    }
    let (m_nums, _, _) = split_version_and_words(model_id);
    let ver_bonus: i32 = m_nums
        .iter()
        .enumerate()
        .map(|(i, n)| n.parse::<i32>().unwrap_or(0) * if i == 0 { 5 } else { 1 })
        .sum();
    s + ver_bonus
}

// --- Per-harness local non-secret configuration scanners ---

fn scan_codex() -> HarnessInventory {
    let mut inv = HarnessInventory {
        auth_mode: "none".into(),
        ..Default::default()
    };
    let codex_home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"));

    // Check auth.json presence & non-secret mode without exposing tokens/keys.
    let mut has_openai_auth = env::var_os("OPENAI_API_KEY").is_some();
    if let Ok(raw) = fs::read_to_string(codex_home.join("auth.json")) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            let has_tok = v.get("tokens").is_some_and(|t| !t.is_null());
            let has_key = v
                .get("OPENAI_API_KEY")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if has_tok || has_key {
                has_openai_auth = true;
                inv.auth_mode = if has_tok { "oauth".into() } else { "api_key".into() };
            }
        }
    }
    if has_openai_auth {
        inv.providers.entry("openai".into()).or_default();
    }

    let mut catalog_json: Option<PathBuf> = None;
    if let Ok(toml_text) = fs::read_to_string(codex_home.join("config.toml")) {
        parse_codex_toml(&toml_text, &mut inv, &mut catalog_json);
    }

    let active_prov = inv
        .default_provider
        .clone()
        .unwrap_or_else(|| "openai".into());
    if inv.default_provider.is_some() && inv.auth_mode == "none" {
        inv.auth_mode = "custom_provider".into();
    }

    let mut active_models = BTreeSet::new();
    if let Some(ref dm) = inv.default_model {
        active_models.insert(dm.clone());
    }
    if let Some(cat_path) = catalog_json {
        collect_json_model_slugs(&cat_path, &mut active_models);
    }
    collect_json_model_slugs(&codex_home.join("models_cache.json"), &mut active_models);

    inv.providers
        .entry(active_prov)
        .or_default()
        .extend(active_models);
    inv
}

fn parse_codex_toml(text: &str, inv: &mut HarnessInventory, catalog_json: &mut Option<PathBuf>) {
    let mut section = String::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            if let Some(rest) = section.strip_prefix("model_providers.") {
                let prov = unquote_toml_seg(rest);
                if !prov.contains('.') && !prov.is_empty() {
                    inv.providers.entry(prov).or_default();
                }
            } else if let Some(rest) = section.strip_prefix("mcp_servers.") {
                let mcp = unquote_toml_seg(rest);
                if !mcp.contains('.') && !is_poison_mcp(&mcp) && !inv.mcps.contains(&mcp) {
                    inv.mcps.push(mcp);
                }
            } else if let Some(rest) = section.strip_prefix("plugins.") {
                let plug = unquote_toml_seg(rest);
                if !inv.plugins.contains(&plug) {
                    inv.plugins.push(plug);
                }
            }
            continue;
        }
        if section.is_empty() {
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                let val = unquote_toml_val(v);
                match key {
                    "model_provider" if !val.is_empty() => inv.default_provider = Some(val),
                    "model" if !val.is_empty() => inv.default_model = Some(val),
                    "model_catalog_json" if !val.is_empty() => {
                        *catalog_json = Some(PathBuf::from(val))
                    }
                    _ => {}
                }
            }
        }
    }
}

fn scan_claude(cwd: &Path) -> HarnessInventory {
    let mut inv = HarnessInventory {
        auth_mode: "subscription_or_env".into(),
        ..Default::default()
    };
    let claude_dir = env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".claude"));

    let mut custom_gateway = env::var_os("ANTHROPIC_BASE_URL").is_some();
    let mut models = BTreeSet::new();

    if let Ok(raw) = fs::read_to_string(claude_dir.join("settings.json")) {
        if let Ok(v) = serde_json::from_str::<Value>(&raw) {
            if let Some(m) = v.get("model").and_then(Value::as_str) {
                inv.default_model = Some(m.to_string());
                models.insert(m.to_string());
            }
            if let Some(env_obj) = v.get("env").and_then(Value::as_object) {
                if env_obj
                    .get("ANTHROPIC_BASE_URL")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
                {
                    custom_gateway = true;
                }
                for k in [
                    "ANTHROPIC_MODEL",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL",
                    "ANTHROPIC_DEFAULT_SONNET_MODEL",
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
                    "ANTHROPIC_SMALL_FAST_MODEL",
                ] {
                    if let Some(m) = env_obj.get(k).and_then(Value::as_str) {
                        if !m.is_empty() {
                            models.insert(m.to_string());
                        }
                    }
                }
            }
        }
    }

    if custom_gateway {
        inv.auth_mode = "gateway".into();
        inv.default_provider = Some("gateway".into());
        collect_json_model_slugs(&claude_dir.join("cache/gateway-models.json"), &mut models);
        inv.providers
            .insert("gateway".into(), models.into_iter().collect());
    } else {
        inv.default_provider = Some("anthropic".into());
        for alias in ["opus", "sonnet", "haiku"] {
            models.insert(alias.to_string());
        }
        inv.providers
            .insert("anthropic".into(), models.into_iter().collect());
    }

    // Collect MCP server names from ~/.claude.json and <cwd>/.mcp.json.
    for path in [home_dir().join(".claude.json"), cwd.join(".mcp.json")] {
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(servers) = v.get("mcpServers").and_then(Value::as_object) {
                    for name in servers.keys() {
                        if !is_poison_mcp(name) && !inv.mcps.contains(name) {
                            inv.mcps.push(name.clone());
                        }
                    }
                }
            }
        }
    }
    inv
}

/// Build an inline `--mcp-config` JSON string for Claude containing ONLY allowed MCP definitions.
pub fn build_claude_mcp_config(cwd: &Path, filter: Option<&[String]>) -> String {
    let mut merged = serde_json::Map::new();
    for path in [home_dir().join(".claude.json"), cwd.join(".mcp.json")] {
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(servers) = v.get("mcpServers").and_then(Value::as_object) {
                    for (name, cfg) in servers {
                        if is_poison_mcp(name) {
                            continue;
                        }
                        if let Some(allowed) = filter {
                            if !allowed.iter().any(|a| a.eq_ignore_ascii_case(name)) {
                                continue;
                            }
                        }
                        merged.insert(name.clone(), cfg.clone());
                    }
                }
            }
        }
    }
    serde_json::json!({ "mcpServers": merged }).to_string()
}

fn scan_zcode() -> HarnessInventory {
    let mut inv = HarnessInventory {
        auth_mode: "provider_config".into(),
        default_provider: Some("zcode".into()),
        ..Default::default()
    };
    let personal = env::var_os("ZCODE_PERSONAL_PROVIDER_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".zcode/v2/provider_config.json"));
    let builtin = env::var_os("ZCODE_BUILTIN_PROVIDER_CONFIG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/Applications/ZCode.app/Contents/Resources/config/provider/zcode-builtin.json")
        });

    let mut models = BTreeSet::new();
    collect_json_model_slugs(&personal, &mut models);
    collect_json_model_slugs(&builtin, &mut models);
    inv.providers
        .insert("zcode".into(), models.into_iter().collect());
    inv
}

fn scan_trae(probe_cli: bool) -> HarnessInventory {
    let mut inv = HarnessInventory {
        auth_mode: "cli".into(),
        default_provider: Some("trae".into()),
        ..Default::default()
    };
    let home = home_dir();
    let mut dirs = vec![home.join(".trae"), home.join(".trae-cn")];
    if let Some(extra) = env::var_os("TRAE_CONFIG_DIR") {
        dirs.insert(0, PathBuf::from(extra));
    }
    for d in dirs {
        if let Ok(raw) = fs::read_to_string(d.join("mcp.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(obj) = v.get("mcpServers").and_then(Value::as_object) {
                    for k in obj.keys() {
                        if !is_poison_mcp(k) && !inv.mcps.contains(k) {
                            inv.mcps.push(k.clone());
                        }
                    }
                }
            }
        }
        if let Ok(raw) = fs::read_to_string(d.join("plugin-config.json")) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                if let Some(obj) = v.get("plugins").and_then(Value::as_object) {
                    for k in obj.keys() {
                        if !inv.plugins.contains(k) {
                            inv.plugins.push(k.clone());
                        }
                    }
                }
            }
        }
    }

    let mut models = BTreeSet::new();
    let cache_file = artifact_dir().ok().map(|d| d.join("trae-models-cache.json"));
    let cache_fresh = cache_file
        .as_ref()
        .and_then(|p| fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < Duration::from_secs(3600));

    if cache_fresh {
        if let Some(ref p) = cache_file {
            collect_json_model_slugs(p, &mut models);
        }
    } else if probe_cli {
        if let Some(ref trae_bin) = bins().trae {
            if let Ok(out) = Command::new(trae_bin)
                .args(["models", "--json"])
                .stdin(Stdio::null())
                .output()
            {
                if out.status.success() {
                    let txt = String::from_utf8_lossy(&out.stdout);
                    let _ = write_artifact("trae-models-cache.json", &txt);
                    if let Some(ref p) = cache_file {
                        collect_json_model_slugs(p, &mut models);
                    }
                }
            }
        }
    } else if let Some(ref p) = cache_file {
        collect_json_model_slugs(p, &mut models);
    }

    inv.default_model = models.iter().find(|m| m.contains("Code")).cloned().or_else(|| models.iter().next().cloned());
    inv.providers
        .insert("trae".into(), models.into_iter().collect());
    inv
}

fn apply_user_model_overrides(map: &mut BTreeMap<String, HarnessInventory>) {
    let custom_path = env::var_os("EXT_MODELS_CATALOG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home_dir().join(".gemini/config/plugins/external-model-subagents/models.json")
        });
    let Ok(raw) = fs::read_to_string(custom_path) else {
        return;
    };
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    for (harness, prov_val) in root {
        let inv = map.entry(canonical_harness_id(&harness)).or_default();
        if let Value::Object(provs) = prov_val {
            for (prov, arr) in provs {
                if let Value::Array(items) = arr {
                    let entry = inv.providers.entry(prov).or_default();
                    for item in items.iter().filter_map(Value::as_str) {
                        if !entry.iter().any(|x| x == item) {
                            entry.push(item.to_string());
                        }
                    }
                }
            }
        }
    }
}

pub fn is_poison_mcp(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("external-model-subagents")
        || lower == "ext-models-mcp"
        || lower == "computer-use"
        || lower == "unified-computer-use"
}

fn collect_json_model_slugs(path: &Path, out: &mut BTreeSet<String>) {
    let Ok(raw) = fs::read_to_string(path) else {
        return;
    };
    let Ok(val) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    extract_model_ids_recursive(&val, out, 0);
}

fn extract_model_ids_recursive(v: &Value, out: &mut BTreeSet<String>, depth: usize) {
    if depth > 6 {
        return;
    }
    match v {
        Value::Array(arr) => {
            for item in arr {
                extract_model_ids_recursive(item, out, depth + 1);
            }
        }
        Value::Object(obj) => {
            // Heuristic: objects representing a model entry have slug / modelId / id alongside display or context fields,
            // or live directly inside a `models` array.
            for key in ["slug", "modelId"] {
                if let Some(s) = obj.get(key).and_then(Value::as_str) {
                    if looks_like_model_id(s) {
                        out.insert(s.to_string());
                    }
                }
            }
            if depth >= 1 {
                for key in ["id", "model", "name"] {
                    if let Some(s) = obj.get(key).and_then(Value::as_str) {
                        if looks_like_model_id(s)
                            && (obj.contains_key("contextWindow")
                                || obj.contains_key("maxTokens")
                                || obj.contains_key("provider")
                                || obj.contains_key("owned_by")
                                || obj.contains_key("display_name")
                                || obj.contains_key("description")
                                || obj.get("object").and_then(Value::as_str) == Some("model")
                                || obj.get("type").and_then(Value::as_str) == Some("model"))
                        {
                            out.insert(s.to_string());
                        }
                    }
                }
            }
            for (k, child) in obj {
                if matches!(k.as_str(), "models" | "data") {
                    if let Value::Array(arr) = child {
                        for item in arr {
                            if let Value::String(s) = item {
                                if looks_like_model_id(s) {
                                    out.insert(s.clone());
                                }
                            }
                        }
                    }
                }
                if matches!(k.as_str(), "models" | "data") || depth < 3 {
                    extract_model_ids_recursive(child, out, depth + 1);
                }
            }
        }
        _ => {}
    }
}

fn looks_like_model_id(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t.len() <= 80
        && !t.contains(' ')
        && !t.starts_with("http")
        && !t.starts_with('/')
}

fn unquote_toml_seg(s: &str) -> String {
    let first = s.split('.').next().unwrap_or(s).trim();
    first.trim_matches('"').trim_matches('\'').to_string()
}

fn unquote_toml_val(s: &str) -> String {
    let before_comment = s.split('#').next().unwrap_or(s).trim();
    before_comment
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}
