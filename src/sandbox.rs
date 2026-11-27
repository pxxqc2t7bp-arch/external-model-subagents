//! Process-group supervisor (total + idle timeout, cancellation, stderr drain, concurrency cap)
//! and dirty-aware isolated Git worktree manager.

use crate::stream::{artifact_dir, read_bounded_line, write_artifact, TailBuffer, MAX_LINE_BYTES};
use std::fs;
use std::io::{BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const SMALL_STACK: usize = 256 * 1024;
const STDERR_CAP: usize = 4 * 1024;

/// Global concurrency semaphore limiting simultaneous external CLI child processes.
pub struct JobPermit;

static JOB_STATE: OnceLock<(Mutex<usize>, Condvar, usize)> = OnceLock::new();

fn job_limiter() -> &'static (Mutex<usize>, Condvar, usize) {
    JOB_STATE.get_or_init(|| {
        let max_jobs = std::env::var("EXT_MODELS_MAX_JOBS")
            .ok()
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(3)
            .clamp(1, 8);
        (Mutex::new(0), Condvar::new(), max_jobs)
    })
}

pub fn acquire_job_permit() -> JobPermit {
    let (lock, cvar, max_jobs) = job_limiter();
    let mut active = lock.lock().unwrap_or_else(|e| e.into_inner());
    while *active >= *max_jobs {
        active = cvar.wait(active).unwrap_or_else(|e| e.into_inner());
    }
    *active += 1;
    JobPermit
}

impl Drop for JobPermit {
    fn drop(&mut self) {
        let (lock, cvar, _) = job_limiter();
        let mut active = lock.lock().unwrap_or_else(|e| e.into_inner());
        *active = active.saturating_sub(1);
        cvar.notify_one();
    }
}

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    pub stdin_data: Option<String>,
    pub cwd: PathBuf,
    pub output_file: Option<PathBuf>,
}

pub struct ExecOutcome {
    pub exit_code: i32,
    pub timed_out: bool,
    pub idle_timed_out: bool,
    pub cancelled: bool,
    pub stderr_tail: String,
    pub duration_ms: u64,
}

/// Spawn `spec` in its own Unix process group (`setpgid(0, 0)`), feed stdin, drain stderr on a
/// bounded thread, stream stdout line-by-line into `on_line`, and guarantee `-SIGTERM -> -SIGKILL`
/// cleanup on total timeout, idle timeout, or cancellation.
pub fn run_supervised<F>(
    spec: &CommandSpec,
    total_timeout: Duration,
    idle_timeout: Duration,
    cancel: Arc<AtomicBool>,
    mut on_line: F,
) -> Result<ExecOutcome, String>
where
    F: FnMut(&str, bool),
{
    let _permit = acquire_job_permit();
    let start = Instant::now();

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .stdin(if spec.stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    for k in &spec.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in &spec.envs {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {e}", spec.program.display()))?;
    let pgid = child.id() as i32;

    if let (Some(mut sin), Some(payload)) = (child.stdin.take(), spec.stdin_data.clone()) {
        let _ = thread::Builder::new()
            .name("ext-stdin".into())
            .stack_size(SMALL_STACK)
            .spawn(move || {
                let _ = sin.write_all(payload.as_bytes());
                let _ = sin.flush();
                // Dropping `sin` immediately closes pipe EOF so CLI starts working.
            });
    }

    let finished = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    let idle_timed_out = Arc::new(AtomicBool::new(false));
    let was_cancelled = Arc::new(AtomicBool::new(false));
    let last_activity_ms = Arc::new(AtomicU64::new(0));

    // Drain stderr continuously to prevent pipe buffer deadlock.
    let stderr_pipe = child.stderr.take().ok_or("missing child stderr")?;
    let act_err = Arc::clone(&last_activity_ms);
    let stderr_handle = thread::Builder::new()
        .name("ext-stderr".into())
        .stack_size(SMALL_STACK)
        .spawn(move || {
            let mut reader = BufReader::with_capacity(4096, stderr_pipe);
            let mut scratch = Vec::with_capacity(1024);
            let mut tail = TailBuffer::new(STDERR_CAP);
            while let Ok(Some((line, _))) = read_bounded_line(&mut reader, &mut scratch, 8192) {
                act_err.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                tail.push_line(&line);
            }
            tail.as_str().to_string()
        })
        .map_err(|e| format!("stderr thread error: {e}"))?;

    // Watchdog thread enforcing cancel, total_timeout, and idle_timeout.
    let fin_wd = Arc::clone(&finished);
    let to_wd = Arc::clone(&timed_out);
    let idle_wd = Arc::clone(&idle_timed_out);
    let can_wd = Arc::clone(&was_cancelled);
    let act_wd = Arc::clone(&last_activity_ms);
    let watchdog = thread::Builder::new()
        .name("ext-watchdog".into())
        .stack_size(SMALL_STACK)
        .spawn(move || {
            let total_ms = total_timeout.as_millis() as u64;
            let idle_ms = idle_timeout.as_millis() as u64;
            while !fin_wd.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(80));
                if fin_wd.load(Ordering::Acquire) {
                    break;
                }
                let elapsed = start.elapsed().as_millis() as u64;
                let last_act = act_wd.load(Ordering::Relaxed);
                if cancel.load(Ordering::Acquire) {
                    can_wd.store(true, Ordering::Release);
                    terminate_process_group(pgid, &fin_wd);
                    break;
                }
                if elapsed >= total_ms {
                    to_wd.store(true, Ordering::Release);
                    terminate_process_group(pgid, &fin_wd);
                    break;
                }
                if idle_ms > 0 && elapsed.saturating_sub(last_act) >= idle_ms {
                    idle_wd.store(true, Ordering::Release);
                    terminate_process_group(pgid, &fin_wd);
                    break;
                }
            }
        })
        .map_err(|e| format!("watchdog thread error: {e}"))?;

    // Read stdout line-by-line on the current worker thread with bounded 64 KiB scratch.
    let stdout_pipe = child.stdout.take().ok_or("missing child stdout")?;
    let mut stdout_reader = BufReader::with_capacity(8192, stdout_pipe);
    let mut scratch = Vec::with_capacity(4096);
    while let Ok(Some((line, trunc))) =
        read_bounded_line(&mut stdout_reader, &mut scratch, MAX_LINE_BYTES)
    {
        last_activity_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        on_line(&line, trunc);
    }

    let status = child.wait().map_err(|e| format!("wait error: {e}"))?;
    finished.store(true, Ordering::Release);
    // Ensure any stray background grandchildren in the process group are reaped.
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }

    let _ = watchdog.join();
    let stderr_tail = stderr_handle.join().unwrap_or_default();

    Ok(ExecOutcome {
        exit_code: status.code().unwrap_or(-1),
        timed_out: timed_out.load(Ordering::Acquire),
        idle_timed_out: idle_timed_out.load(Ordering::Acquire),
        cancelled: was_cancelled.load(Ordering::Acquire),
        stderr_tail,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

fn terminate_process_group(pgid: i32, finished: &AtomicBool) {
    if pgid <= 0 {
        return;
    }
    // SAFETY: `pgid` was created with `process_group(0)` so `-pgid` targets only our child group.
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    for _ in 0..25 {
        if finished.load(Ordering::Acquire) {
            return;
        }
        thread::sleep(Duration::from_millis(80));
    }
    if !finished.load(Ordering::Acquire) {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

/// Isolated Git worktree carrying uncommitted tracked & untracked changes from the main repo.
pub struct WorktreeSession {
    pub repo_root: PathBuf,
    pub worktree_cwd: PathBuf,
    wt_root: PathBuf,
    call_id: String,
}

pub struct WorktreeSummary {
    pub diff_stat: String,
    pub patch_path: Option<PathBuf>,
}

impl WorktreeSession {
    pub fn create(cwd: &Path, call_id: &str) -> Result<Self, String> {
        let root_out = run_git(cwd, &["rev-parse", "--show-toplevel"])?;
        let repo_root = PathBuf::from(root_out.trim());
        let rel_sub = cwd.strip_prefix(&repo_root).unwrap_or(Path::new(""));

        // Snapshot tracked uncommitted modifications without mutating main index or stash stack.
        let stash_oid = run_git(&repo_root, &["stash", "create"]).unwrap_or_default();
        let base_ref = if stash_oid.trim().is_empty() {
            "HEAD".to_string()
        } else {
            stash_oid.trim().to_string()
        };

        let wt_root = artifact_dir()
            .map_err(|e| e.to_string())?
            .join(format!("wt-{call_id}"));
        if wt_root.exists() {
            let _ = fs::remove_dir_all(&wt_root);
        }
        let wt_str = wt_root.to_string_lossy().into_owned();
        run_git(&repo_root, &["worktree", "add", "--detach", &wt_str, &base_ref])?;

        // Copy up to 50 untracked non-ignored files (< 2 MiB each) so newly added files exist too.
        let mut copied_untracked = false;
        if let Ok(untracked) = run_git(&repo_root, &["ls-files", "--others", "--exclude-standard"])
        {
            for rel in untracked.lines().take(50) {
                let src = repo_root.join(rel);
                if let Ok(meta) = fs::metadata(&src) {
                    if meta.is_file() && meta.len() <= 2 * 1024 * 1024 {
                        let dst = wt_root.join(rel);
                        if let Some(parent) = dst.parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        if fs::copy(&src, &dst).is_ok() {
                            copied_untracked = true;
                        }
                    }
                }
            }
        }
        // Snapshot pre-existing untracked files on detached HEAD so final patch contains ONLY worker edits.
        if copied_untracked {
            let _ = run_git(&wt_root, &["add", "-A"]);
            let _ = run_git(
                &wt_root,
                &[
                    "-c",
                    "user.name=ext-models",
                    "-c",
                    "user.email=ext@localhost",
                    "commit",
                    "-q",
                    "--no-verify",
                    "--no-gpg-sign",
                    "-m",
                    "ext-models-baseline",
                ],
            );
        }

        let worktree_cwd = wt_root.join(rel_sub);
        Ok(Self {
            repo_root,
            worktree_cwd,
            wt_root,
            call_id: call_id.to_string(),
        })
    }

    /// Export unified diff patch vs original base and remove temporary worktree directory.
    pub fn finalize(self) -> WorktreeSummary {
        let _ = run_git(&self.wt_root, &["add", "-A"]);
        let stat = run_git(&self.wt_root, &["diff", "--cached", "--stat"])
            .unwrap_or_default()
            .trim()
            .to_string();
        let patch = run_git(&self.wt_root, &["diff", "--cached"]).unwrap_or_default();
        let patch_path = if !patch.trim().is_empty() {
            write_artifact(&format!("{}.patch", self.call_id), &patch)
        } else {
            None
        };
        let wt_str = self.wt_root.to_string_lossy().into_owned();
        let _ = run_git(&self.repo_root, &["worktree", "remove", "--force", &wt_str]);
        let _ = fs::remove_dir_all(&self.wt_root);
        WorktreeSummary {
            diff_stat: stat,
            patch_path,
        }
    }
}

pub fn git_diff_stat(cwd: &Path) -> Option<String> {
    let out = run_git(cwd, &["status", "--short"]).ok()?;
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        let lines: Vec<&str> = trimmed.lines().take(25).collect();
        Some(lines.join("\n"))
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git {:?} failed to start: {e}", args))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {:?} exited {}: {}",
            args,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}
