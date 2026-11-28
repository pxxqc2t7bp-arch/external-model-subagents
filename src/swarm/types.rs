//! Boundary-validated Swarm types (`SwarmSpec`, `SwarmAgent`, `Decision`, `Vote`, `SwarmMessage`).
//!
//! All untrusted JSON validation and clamping happens once in [`SwarmSpec::from_json`];
//! downstream `swarm::*` modules trust these structs without redundant checks.

use crate::error::{ErrorKind, RichError};
use crate::harness::catalog::canonical_harness_id;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct SwarmAgent {
    pub harness: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub role: String,
    pub capabilities: Vec<String>,
    pub weight: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationRule {
    NonEmpty,
    JsonValid,
    Contains(String),
    MinChars(usize),
}

#[derive(Clone, Debug)]
pub struct SwarmStage {
    pub name: String,
    pub agent: SwarmAgent,
    pub input_template: Option<String>,
    pub validations: Vec<ValidationRule>,
    pub continue_on_error: bool,
}

#[derive(Clone, Debug)]
pub struct DebateSpec {
    pub proposer: SwarmAgent,
    pub challenger: SwarmAgent,
    pub judge: SwarmAgent,
    pub rounds: usize,
}

#[derive(Clone, Debug)]
pub enum SwarmSpec {
    Panel {
        agents: Vec<SwarmAgent>,
        consensus_threshold: f32,
        parallel: bool,
    },
    Pipeline {
        stages: Vec<SwarmStage>,
    },
    Debate(Box<DebateSpec>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject,
    NeedsChanges,
    Abstain,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "APPROVE",
            Self::Reject => "REJECT",
            Self::NeedsChanges => "NEEDS_CHANGES",
            Self::Abstain => "ABSTAIN",
        }
    }

    pub fn parse_token(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "APPROVE" | "APPROVED" | "PASS" | "LGTM" => Some(Self::Approve),
            "REJECT" | "REJECTED" | "FAIL" | "BLOCK" => Some(Self::Reject),
            "NEEDS_CHANGES" | "NEED_CHANGES" | "CHANGES_REQUESTED" | "REVISE" => {
                Some(Self::NeedsChanges)
            }
            "ABSTAIN" | "NEUTRAL" => Some(Self::Abstain),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Vote {
    pub agent_id: String,
    pub role: String,
    pub decision: Decision,
    pub confidence: f32,
    pub weight: f32,
    pub summary: String,
}

#[derive(Clone, Debug)]
pub struct ConsensusResult {
    pub decision: Decision,
    pub reached: bool,
    pub approval_ratio: f32,
    pub threshold: f32,
    pub confidence: f32,
    pub votes: Vec<Vote>,
    pub reasoning_summary: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "P0",
            Self::High => "P1",
            Self::Medium => "P2",
            Self::Low => "P3",
            Self::Info => "INFO",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub author: String,
    pub severity: Severity,
    pub summary: String,
}

#[derive(Clone, Debug)]
pub enum Destination {
    Agent(String),
    Broadcast,
}

#[derive(Clone, Debug)]
pub enum MessagePayload {
    TaskProposal { task: String, vote_id: String },
    VoteCast(Vote),
    ShareFinding(Finding),
    StageHandoff { stage: String, call_id: String },
}

#[derive(Clone, Debug)]
pub struct SwarmMessage {
    pub from: String,
    pub to: Destination,
    pub payload: MessagePayload,
    pub correlation_id: Option<String>,
}

impl SwarmSpec {
    /// Parse optional `swarm` or `swarm_mode` field at the request boundary.
    pub fn from_args(args: &Value) -> Result<Option<Self>, RichError> {
        let Some(raw) = args.get("swarm").or_else(|| args.get("swarm_mode")) else {
            return Ok(None);
        };
        if raw.is_null() {
            return Ok(None);
        }
        let ty = raw
            .get("type")
            .or_else(|| raw.get("mode"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        match ty.as_str() {
            "" | "single" => Ok(None),
            "panel" | "collaborative" => {
                let arr = raw
                    .get("agents")
                    .or_else(|| raw.get("panel"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        RichError::new(
                            ErrorKind::InvalidInput,
                            "swarm panel requires an `agents` array",
                        )
                        .with_suggestion(
                            "Pass `\"agents\": [{\"harness\": \"codex\", \"role\": \"reviewer\"}, ...]`",
                        )
                    })?;
                if arr.is_empty() || arr.len() > 6 {
                    return Err(RichError::new(
                        ErrorKind::InvalidInput,
                        format!("swarm panel supports 1..=6 agents (got {})", arr.len()),
                    ));
                }
                let mut agents = Vec::with_capacity(arr.len());
                for (i, item) in arr.iter().enumerate() {
                    agents.push(parse_agent(item, &format!("panel_{}", i + 1))?);
                }
                let threshold = raw
                    .get("consensus_threshold")
                    .or_else(|| raw.get("threshold"))
                    .and_then(Value::as_f64)
                    .map(|v| (v as f32).clamp(0.1, 1.0))
                    .unwrap_or(0.66);
                let parallel = raw
                    .get("parallel_execution")
                    .or_else(|| raw.get("parallel"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                Ok(Some(Self::Panel {
                    agents,
                    consensus_threshold: threshold,
                    parallel,
                }))
            }
            "pipeline" => {
                let arr = raw
                    .get("stages")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        RichError::new(
                            ErrorKind::InvalidInput,
                            "swarm pipeline requires a `stages` array",
                        )
                    })?;
                if arr.is_empty() || arr.len() > 6 {
                    return Err(RichError::new(
                        ErrorKind::InvalidInput,
                        format!("swarm pipeline supports 1..=6 stages (got {})", arr.len()),
                    ));
                }
                let mut stages = Vec::with_capacity(arr.len());
                for (i, st) in arr.iter().enumerate() {
                    let default_name = format!("stage_{}", i + 1);
                    let name = st
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&default_name)
                        .to_string();
                    let agent_val = st.get("agent").unwrap_or(st);
                    let agent = parse_agent(agent_val, &name)?;
                    let input_template = st
                        .get("input_template")
                        .or_else(|| st.get("prompt"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    let validations = parse_validations(st.get("output_validation"))?;
                    let continue_on_error = st
                        .get("continue_on_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    stages.push(SwarmStage {
                        name,
                        agent,
                        input_template,
                        validations,
                        continue_on_error,
                    });
                }
                Ok(Some(Self::Pipeline { stages }))
            }
            "debate" => {
                let proposer = parse_agent(
                    raw.get("proposer").ok_or_else(|| {
                        RichError::new(ErrorKind::InvalidInput, "swarm debate requires `proposer`")
                    })?,
                    "proposer",
                )?;
                let challenger = parse_agent(
                    raw.get("challenger").ok_or_else(|| {
                        RichError::new(
                            ErrorKind::InvalidInput,
                            "swarm debate requires `challenger`",
                        )
                    })?,
                    "challenger",
                )?;
                let judge = parse_agent(
                    raw.get("judge").ok_or_else(|| {
                        RichError::new(ErrorKind::InvalidInput, "swarm debate requires `judge`")
                    })?,
                    "judge",
                )?;
                let rounds = raw
                    .get("rounds")
                    .and_then(Value::as_u64)
                    .map(|n| (n as usize).clamp(1, 4))
                    .unwrap_or(2);
                Ok(Some(Self::Debate(Box::new(DebateSpec {
                    proposer,
                    challenger,
                    judge,
                    rounds,
                }))))
            }
            other => Err(RichError::new(
                ErrorKind::InvalidInput,
                format!("unknown swarm mode '{other}'"),
            )
            .with_suggestion("Supported swarm modes: 'panel', 'pipeline', 'debate'")),
        }
    }
}

fn parse_agent(v: &Value, default_role: &str) -> Result<SwarmAgent, RichError> {
    if !v.is_object() {
        return Err(RichError::new(
            ErrorKind::InvalidInput,
            format!("swarm agent '{default_role}' must be a JSON object"),
        ));
    }
    let harness = v
        .get("harness")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("auto"))
        .map(canonical_harness_id);
    let provider = v
        .get("provider")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let model = v
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let effort = v
        .get("effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let role = v
        .get("role")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_role)
        .to_string();
    let mut capabilities = Vec::new();
    if let Some(spec) = v.get("specialization").and_then(Value::as_str) {
        if !spec.trim().is_empty() {
            capabilities.push(spec.trim().to_string());
        }
    }
    if let Some(arr) = v.get("capabilities").and_then(Value::as_array) {
        for c in arr.iter().filter_map(Value::as_str) {
            let t = c.trim();
            if !t.is_empty() && !capabilities.iter().any(|x| x == t) {
                capabilities.push(t.to_string());
            }
        }
    }
    let weight = v
        .get("weight")
        .and_then(Value::as_f64)
        .map(|w| (w as f32).clamp(0.1, 10.0))
        .unwrap_or(1.0);

    Ok(SwarmAgent {
        harness,
        provider,
        model,
        effort,
        role,
        capabilities,
        weight,
    })
}

fn parse_validations(raw: Option<&Value>) -> Result<Vec<ValidationRule>, RichError> {
    let Some(arr) = raw.and_then(Value::as_array) else {
        return Ok(vec![ValidationRule::NonEmpty]);
    };
    let mut rules = Vec::with_capacity(arr.len());
    for item in arr {
        if let Some(s) = item.as_str() {
            rules.push(parse_rule_name(s, None)?);
            continue;
        }
        let ty = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("non_empty");
        rules.push(parse_rule_name(ty, Some(item))?);
    }
    if rules.is_empty() {
        rules.push(ValidationRule::NonEmpty);
    }
    Ok(rules)
}

fn parse_rule_name(ty: &str, obj: Option<&Value>) -> Result<ValidationRule, RichError> {
    match ty.trim().to_ascii_lowercase().as_str() {
        "non_empty" | "nonempty" => Ok(ValidationRule::NonEmpty),
        "json_valid" | "json" => Ok(ValidationRule::JsonValid),
        "contains" | "contains_keyword" => {
            let kw = obj
                .and_then(|o| o.get("value").or_else(|| o.get("keyword")))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    RichError::new(
                        ErrorKind::InvalidInput,
                        "validation rule 'contains' requires non-empty `value`",
                    )
                })?;
            Ok(ValidationRule::Contains(kw.to_string()))
        }
        "min_length" | "min_chars" => {
            let n = obj
                .and_then(|o| o.get("value").or_else(|| o.get("min")))
                .and_then(Value::as_u64)
                .unwrap_or(1) as usize;
            Ok(ValidationRule::MinChars(n))
        }
        other => Err(RichError::new(
            ErrorKind::InvalidInput,
            format!("unsupported validation rule '{other}'"),
        )
        .with_suggestion("Supported rules: non_empty, json_valid, contains, min_length")),
    }
}
