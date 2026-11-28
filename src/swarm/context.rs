//! Shared findings, notes, and artifact references for a Swarm run.

use std::collections::BTreeMap;
use std::sync::RwLock;

use super::types::Finding;
use crate::stream::read_handoff;

const NOTE_MAX_BYTES: usize = 16 * 1024;

#[derive(Default)]
pub struct SwarmContext {
    findings: RwLock<BTreeMap<String, Finding>>,
    notes: RwLock<BTreeMap<String, String>>,
    stage_handoffs: RwLock<BTreeMap<String, String>>,
}

impl SwarmContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_finding(&self, id: String, finding: Finding) {
        self.findings.write().expect("swarm findings poisoned").insert(id, finding);
    }

    pub fn findings_snapshot(&self) -> Vec<(String, Finding)> {
        self.findings
            .read()
            .expect("swarm findings poisoned")
            .iter()
            .map(|(id, finding)| (id.clone(), finding.clone()))
            .collect()
    }

    pub fn set_note(&self, key: String, mut note: String) {
        if note.len() > NOTE_MAX_BYTES {
            let mut end = NOTE_MAX_BYTES;
            while !note.is_char_boundary(end) {
                end -= 1;
            }
            note = note[..end].to_owned();
        }
        self.notes.write().expect("swarm notes poisoned").insert(key, note);
    }

    pub fn get_note(&self, key: &str) -> Option<String> {
        self.notes.read().expect("swarm notes poisoned").get(key).cloned()
    }

    pub fn record_stage_handoff(&self, stage: String, call_id: String) {
        self.stage_handoffs
            .write()
            .expect("swarm handoffs poisoned")
            .insert(stage, call_id);
    }

    pub fn load_stage_output(&self, stage: &str, max_bytes: usize) -> Result<String, String> {
        let call_id = self
            .stage_handoffs
            .read()
            .expect("swarm handoffs poisoned")
            .get(stage)
            .cloned()
            .ok_or_else(|| format!("no handoff recorded for swarm stage '{stage}'"))?;
        read_handoff(&call_id, max_bytes)
    }

    /// Expand known placeholders once, leaving unknown ones verbatim.
    pub fn render_template(&self, tpl: &str, task: &str, prev_output: &str) -> String {
        let notes = self.notes.read().expect("swarm notes poisoned");
        let mut rendered = String::with_capacity(tpl.len());
        for chunk in tpl.split_inclusive('}') {
            let Some((prefix, key)) = chunk.strip_suffix('}').and_then(|s| s.rsplit_once('{')) else {
                rendered.push_str(chunk);
                continue;
            };
            let value = match key {
                "task" => Some(task),
                "prev_output" => Some(prev_output),
                _ => key.strip_prefix("context.").and_then(|key| notes.get(key)).map(String::as_str),
            };
            if let Some(value) = value {
                rendered.push_str(prefix);
                rendered.push_str(value);
            } else {
                rendered.push_str(chunk);
            }
        }
        rendered
    }
}
