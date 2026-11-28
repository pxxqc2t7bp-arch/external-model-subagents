//! Structurally bounded, UTF-8 safe line reader, ring tail buffer, and zero-copy handoff store.

use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

/// Maximum bytes retained per single stdout line (prevents OOM on multi-MB JSONL tool dumps).
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// Default character budget returned inline to the Master Orchestrator.
pub const DEFAULT_MAX_CHARS: usize = 8_000;
const DEFAULT_ARTIFACT_TTL_DAYS: u64 = 7;

static CALL_SEQ: AtomicU64 = AtomicU64::new(1);
static TTL_SWEEP_ONCE: OnceLock<()> = OnceLock::new();

#[cfg(unix)]
mod sys {
    use std::fs;
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub fn open_secure_writer(path: &Path) -> io::Result<fs::File> {
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
}

#[cfg(not(unix))]
mod sys {
    use std::fs;
    use std::io;
    use std::path::Path;

    pub fn open_secure_writer(path: &Path) -> io::Result<fs::File> {
        if let Ok(meta) = fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to write through symlink",
                ));
            }
        }
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
    }
}

fn ensure_secure_dir(base: &Path) -> io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    let meta = match fs::symlink_metadata(base) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(base) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            fs::symlink_metadata(base)?
        }
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("artifact path {} is not a real directory", base.display()),
        ));
    }
    #[cfg(unix)]
    {
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("artifact directory {} is not owned by current user", base.display()),
            ));
        }
        if meta.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(base, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// Generate a short deterministic call ID (`c<pid>_<seq>`) for zero-copy handoff files.
pub fn next_call_id() -> String {
    let seq = CALL_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("c{}_{}", std::process::id(), seq)
}

/// Ensure `<temp_dir>/ext-models` exists as an owner-only non-symlink directory and run lazy TTL sweep once.
pub fn artifact_dir() -> io::Result<PathBuf> {
    let base = std::env::temp_dir().join("ext-models");
    ensure_secure_dir(&base)?;
    TTL_SWEEP_ONCE.get_or_init(|| {
        let days = std::env::var("EXT_MODELS_ARTIFACT_TTL_DAYS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_ARTIFACT_TTL_DAYS);
        if days > 0 {
            let _ = cleanup_expired_artifacts_in(&base, Duration::from_secs(days.saturating_mul(86_400)));
        }
    });
    Ok(base)
}

/// Remove regular artifact files in `dir` whose `modified` timestamp is older than `max_age`.
pub fn cleanup_expired_artifacts_in(dir: &Path, max_age: Duration) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let expired = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= max_age);
        if expired && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Remove regular artifact files under `<temp_dir>/ext-models` older than `max_age`.
pub fn cleanup_expired_artifacts(max_age: Duration) -> usize {
    let Ok(dir) = artifact_dir() else {
        return 0;
    };
    cleanup_expired_artifacts_in(&dir, max_age)
}

/// Open an owner-only (`0o600`, `O_NOFOLLOW` on Unix) artifact writer under `<temp_dir>/ext-models`.
pub fn open_artifact_writer(filename: &str) -> Result<(PathBuf, fs::File), String> {
    let dir = artifact_dir().map_err(|e| format!("artifact_dir error: {e}"))?;
    let path = dir.join(filename);
    let file = sys::open_secure_writer(&path)
        .map_err(|e| format!("failed to open artifact {}: {e}", path.display()))?;
    Ok((path, file))
}

/// Write raw bytes to an owner-only (`0o600`, `O_NOFOLLOW`) artifact file under `/tmp/ext-models`.
pub fn write_artifact_bytes(filename: &str, content: &[u8]) -> Result<PathBuf, String> {
    let (path, mut file) = open_artifact_writer(filename)?;
    if let Err(e) = file.write_all(content).and_then(|()| file.flush()) {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(format!("failed to write artifact {}: {e}", path.display()));
    }
    Ok(path)
}

/// Write an owner-only (`0o600`) UTF-8 artifact file under `/tmp/ext-models`.
pub fn write_artifact(filename: &str, content: &str) -> Option<PathBuf> {
    write_artifact_bytes(filename, content.as_bytes()).ok()
}

/// Resolve a `handoff_from` call ID (e.g. `"c1234_1"`) to its saved Markdown artifact content.
pub fn read_handoff(call_id: &str, max_bytes: usize) -> Result<String, String> {
    if call_id.is_empty() || call_id.len() > 128
        || !call_id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err(format!("invalid handoff_from id: '{call_id}'"));
    }
    let dir = artifact_dir().map_err(|e| e.to_string())?;
    let exact = dir.join(format!("{call_id}.md"));
    let target = if exact.exists() {
        exact
    } else {
        let prefix = format!("{call_id}-");
        fs::read_dir(&dir)
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".md"))
            })
            .ok_or_else(|| format!("handoff artifact not found for '{call_id}' in {}", dir.display()))?
    };
    read_file_capped(&target, max_bytes)
}

/// Read a local context file capped at `max_bytes` on a valid UTF-8 boundary without loading
/// the entire file into memory first.
pub fn read_file_capped(path: &Path, max_bytes: usize) -> Result<String, String> {
    let file =
        fs::File::open(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    file.take((max_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let truncated = bytes.len() > max_bytes;
    if truncated {
        bytes.truncate(max_bytes);
        trim_incomplete_utf8_tail(&mut bytes);
    }
    let mut clean = String::from_utf8(bytes)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned());
    if truncated {
        clean.push_str("\n...[truncated file context]...");
    }
    Ok(clean)
}

/// Read a complete UTF-8 configuration or prompt, rejecting inputs over 256 KiB.
pub fn read_input(reader: impl Read) -> io::Result<String> {
    const LIMIT: usize = 256 * 1024;
    let mut bytes = Vec::new();
    reader.take(LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "input exceeds 256 KiB"));
    }
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn read_config(path: &Path) -> io::Result<String> {
    read_input(fs::File::open(path)?)
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
    if src.is_empty() {
        return;
    }
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
        let line_cap = if self.data.is_empty() {
            self.cap
        } else {
            self.data.push('\n');
            self.cap / 2
        };
        self.data.push_str(&line[floor_char_boundary_from_end(line, line_cap)..]);
        self.shrink_to(self.cap);
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
    let saved = write_artifact_bytes(&format!("{call_id}-{harness}.md"), full_text.as_bytes());
    let char_count = full_text.chars().count();
    if char_count <= budget_chars || budget_chars == 0 {
        return (full_text.to_string(), saved.ok());
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
        .unwrap_or_else(|e| format!("; failed to save full text: {e}"));
    let truncated = format!(
        "{}\n\n[... truncated {omitted} chars{saved_hint} ...]\n\n{}",
        &full_text[..head_end],
        &full_text[tail_start..]
    );
    (truncated, saved.ok())
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
