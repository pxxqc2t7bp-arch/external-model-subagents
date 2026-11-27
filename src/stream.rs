//! Structurally bounded, UTF-8 safe line reader, ring tail buffer, and zero-copy handoff store.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum bytes retained per single stdout line (prevents OOM on multi-MB JSONL tool dumps).
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// Default character budget returned inline to the Master Orchestrator.
pub const DEFAULT_MAX_CHARS: usize = 8_000;

static CALL_SEQ: AtomicU64 = AtomicU64::new(1);

/// Generate a short deterministic call ID (`c<pid>_<seq>`) for zero-copy handoff files.
pub fn next_call_id() -> String {
    let seq = CALL_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("c{}_{}", std::process::id(), seq)
}

/// Ensure `/tmp/ext-models` exists with owner-only `0o700` permissions.
pub fn artifact_dir() -> io::Result<PathBuf> {
    let base = std::env::temp_dir().join("ext-models");
    if !base.exists() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&base)?;
    }
    Ok(base)
}

/// Write an owner-only (`0o600`) artifact file under `/tmp/ext-models`.
pub fn write_artifact(filename: &str, content: &str) -> Option<PathBuf> {
    let dir = artifact_dir().ok()?;
    let path = dir.join(filename);
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    file.write_all(content.as_bytes()).ok()?;
    Some(path)
}

/// Resolve a `handoff_from` call ID (e.g. `"c1234_1"`) to its saved Markdown artifact content.
pub fn read_handoff(call_id: &str, max_bytes: usize) -> Result<String, String> {
    let safe_id: String = call_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if safe_id.is_empty() {
        return Err(format!("invalid handoff_from id: '{call_id}'"));
    }
    let dir = artifact_dir().map_err(|e| e.to_string())?;
    let exact = dir.join(format!("{safe_id}.md"));
    let target = if exact.exists() {
        exact
    } else {
        let prefix = format!("{safe_id}-");
        fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".md"))
            })
            .ok_or_else(|| format!("handoff artifact not found for '{safe_id}' in {}", dir.display()))?
    };
    read_file_capped(&target, max_bytes)
}

/// Read a local context file capped at `max_bytes` on a valid UTF-8 boundary.
pub fn read_file_capped(path: &Path, max_bytes: usize) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    if bytes.len() <= max_bytes {
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    let s = String::from_utf8_lossy(&bytes[..max_bytes]);
    let mut clean = s.into_owned();
    clean.push_str("\n...[truncated file context]...");
    Ok(clean)
}

/// Read one `\n`-terminated line from `reader` keeping at most `max_bytes` in RAM,
/// draining any excess bytes in `O(1)` memory without corrupting UTF-8 boundaries.
/// Returns `Ok(None)` on EOF, or `Ok(Some((line_without_crlf, was_truncated)))`.
pub fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    scratch: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<Option<(String, bool)>> {
    scratch.clear();
    let mut total_seen = 0usize;
    let mut truncated = false;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if total_seen == 0 {
                return Ok(None);
            }
            break;
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let slice = &available[..pos];
            append_capped(scratch, slice, max_bytes, &mut truncated);
            reader.consume(pos + 1);
            total_seen += pos + 1;
            break;
        } else {
            let len = available.len();
            append_capped(scratch, available, max_bytes, &mut truncated);
            reader.consume(len);
            total_seen += len;
        }
    }

    if scratch.last() == Some(&b'\r') {
        scratch.pop();
    }
    trim_incomplete_utf8_tail(scratch);
    let text = String::from_utf8_lossy(scratch).into_owned();
    let _ = total_seen;
    Ok(Some((text, truncated)))
}

fn append_capped(dst: &mut Vec<u8>, src: &[u8], max_bytes: usize, truncated: &mut bool) {
    if dst.len() < max_bytes {
        let remain = max_bytes - dst.len();
        if src.len() <= remain {
            dst.extend_from_slice(src);
        } else {
            dst.extend_from_slice(&src[..remain]);
            *truncated = true;
        }
    } else {
        *truncated = true;
    }
}

fn trim_incomplete_utf8_tail(buf: &mut Vec<u8>) {
    if let Err(err) = std::str::from_utf8(buf) {
        if err.error_len().is_none() {
            buf.truncate(err.valid_up_to());
        }
    }
}

/// Fixed-capacity UTF-8 tail buffer (used for `stderr` and raw fallback text).
pub struct TailBuffer {
    cap: usize,
    data: String,
}

impl TailBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(256),
            data: String::new(),
        }
    }

    pub fn push_line(&mut self, line: &str) {
        if crate::alloc_cap::is_near_soft_limit() && self.data.len() > 1024 {
            self.shrink_to(1024);
        }
        if !self.data.is_empty() {
            self.data.push('\n');
        }
        if line.len() >= self.cap {
            let start = floor_char_boundary_from_end(line, self.cap);
            self.data.clear();
            self.data.push_str(&line[start..]);
            return;
        }
        self.data.push_str(line);
        if self.data.len() > self.cap {
            self.shrink_to(self.cap);
        }
    }

    fn shrink_to(&mut self, target: usize) {
        if self.data.len() <= target {
            return;
        }
        let drop_to = self.data.len() - target;
        let idx = ceil_char_boundary(&self.data, drop_to);
        self.data.drain(..idx);
    }

    pub fn as_str(&self) -> &str {
        self.data.trim()
    }
}

/// Persist full output to `/tmp/ext-models/<call_id>-<harness>.md` and truncate inline text
/// cleanly within `budget_chars` (preserving head 75% + tail 25%).
pub fn persist_and_budget(
    call_id: &str,
    harness: &str,
    full_text: &str,
    budget_chars: usize,
) -> (String, Option<PathBuf>) {
    let saved = write_artifact(&format!("{call_id}-{harness}.md"), full_text);
    let char_count = full_text.chars().count();
    if char_count <= budget_chars || budget_chars == 0 {
        return (full_text.to_string(), saved);
    }
    let head_n = (budget_chars * 3) / 4;
    let tail_n = budget_chars.saturating_sub(head_n);
    let head_end = full_text
        .char_indices()
        .nth(head_n)
        .map(|(i, _)| i)
        .unwrap_or(full_text.len());
    let tail_start = full_text
        .char_indices()
        .nth(char_count.saturating_sub(tail_n))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let omitted = char_count.saturating_sub(head_n + tail_n);
    let saved_hint = saved
        .as_ref()
        .map(|p| format!("; full text at {}", p.display()))
        .unwrap_or_default();
    let truncated = format!(
        "{}\n\n[... truncated {omitted} chars{saved_hint} ...]\n\n{}",
        &full_text[..head_end],
        &full_text[tail_start..]
    );
    (truncated, saved)
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn floor_char_boundary_from_end(s: &str, max_bytes: usize) -> usize {
    if s.len() <= max_bytes {
        return 0;
    }
    ceil_char_boundary(s, s.len() - max_bytes)
}
