//! Process-group supervisor (total + idle timeout, cancellation, stderr drain, concurrency cap)
//! and dirty-aware isolated Git worktree manager.

use crate::stream::{
    artifact_dir, open_artifact_writer, read_bounded_line, TailBuffer, MAX_LINE_BYTES,
};
use std::ffi::OsStr;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
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
    on_line: F,
) -> Result<ExecOutcome, String>
where
    F: FnMut(&str, bool),
{
    run_supervised_bounded(
        spec,
        total_timeout,
        idle_timeout,
        MAX_LINE_BYTES,
        cancel,
        on_line,
    )
}

/// Like [`run_supervised`], with a configurable per-line stdout byte cap.
pub fn run_supervised_bounded<F>(
    spec: &CommandSpec,
    total_timeout: Duration,
    idle_timeout: Duration,
    max_stdout_line_bytes: usize,
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

    // Read stdout line-by-line on the current worker thread with bounded scratch.
    let line_cap = max_stdout_line_bytes.max(1024);
    let stdout_pipe = child.stdout.take().ok_or("missing child stdout")?;
    let mut stdout_reader = BufReader::with_capacity(8192, stdout_pipe);
    let mut scratch = Vec::with_capacity(4096);
    while let Ok(Some((line, trunc))) =
        read_bounded_line(&mut stdout_reader, &mut scratch, line_cap)
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
    baseline_tree: String,
    call_id: String,
    skipped_untracked: usize,
}

pub struct WorktreeSummary {
    pub diff_stat: String,
    pub patch_path: Option<PathBuf>,
    pub export_error: Option<String>,
    pub preserved_worktree: Option<PathBuf>,
    pub skipped_untracked: usize,
}

impl WorktreeSession {
    pub fn create(cwd: &Path, call_id: &str) -> Result<Self, String> {
        let canon_cwd = fs::canonicalize(cwd)
            .map_err(|e| format!("failed to canonicalize cwd {}: {e}", cwd.display()))?;
        let mut root_bytes = run_git_bytes(&canon_cwd, &["rev-parse", "--show-toplevel"])?;
        while matches!(root_bytes.last(), Some(b'\n' | b'\r')) {
            root_bytes.pop();
        }
        let raw_root = Path::new(OsStr::from_bytes(&root_bytes));
        let repo_root = fs::canonicalize(raw_root).map_err(|e| {
            format!(
                "failed to canonicalize git repository root {}: {e}",
                raw_root.display()
            )
        })?;
        let rel_sub = canon_cwd
            .strip_prefix(&repo_root)
            .map_err(|_| {
                format!(
                    "cwd {} is outside git repository {}",
                    canon_cwd.display(),
                    repo_root.display()
                )
            })?
            .to_path_buf();
        if rel_sub
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(format!(
                "invalid relative subdirectory '{}' for worktree",
                rel_sub.display()
            ));
        }

        // Snapshot tracked staged + unstaged modifications using a private temporary index copy so
        // the source `.git/index` is never mutated or locked (`0` byte-for-byte side effects).
        let base_ref = snapshot_tracked_without_touching_index(&repo_root, call_id)?;

        let raw_wt_root = artifact_dir()
            .map_err(|e| e.to_string())?
            .join(format!("wt-{call_id}"));
        if fs::symlink_metadata(&raw_wt_root).is_ok() {
            return Err(format!(
                "worktree target path already exists: {}",
                raw_wt_root.display()
            ));
        }
        let wt_str = raw_wt_root.to_string_lossy().into_owned();
        run_git(&repo_root, &["worktree", "add", "--detach", &wt_str, &base_ref])?;
        let wt_root = fs::canonicalize(&raw_wt_root).unwrap_or(raw_wt_root);

        let setup_res = (|| -> Result<(String, PathBuf, usize), String> {
            // Copy up to 50 untracked non-ignored regular files (<= 2 MiB each) using bounded
            // NUL-delimited raw paths (`-z`) so huge untracked trees never spike heap memory.
            let (mut untracked_raw, extra_nuls, untracked_capped) = run_git_bytes_capped(
                &repo_root,
                &["ls-files", "-z", "--others", "--exclude-standard"],
                128 * 1024,
            )?;
            let mut skipped_untracked = if untracked_capped {
                if let Some(last_nul) = untracked_raw.iter().rposition(|&b| b == 0) {
                    let tail_has_partial = last_nul + 1 < untracked_raw.len();
                    untracked_raw.truncate(last_nul + 1);
                    extra_nuls.max(usize::from(tail_has_partial))
                } else {
                    untracked_raw.clear();
                    extra_nuls.max(1)
                }
            } else {
                0
            };
            let mut copied_count = 0usize;
            for entry in untracked_raw.split(|&b| b == 0).filter(|s| !s.is_empty()) {
                let rel = Path::new(OsStr::from_bytes(entry));
                if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
                    skipped_untracked += 1;
                    continue;
                }
                if copied_count >= 50 {
                    skipped_untracked += 1;
                    continue;
                }
                let src = repo_root.join(rel);
                let Ok(meta) = fs::symlink_metadata(&src) else {
                    skipped_untracked += 1;
                    continue;
                };
                if meta.file_type().is_symlink()
                    || !meta.file_type().is_file()
                    || meta.len() > 2 * 1024 * 1024
                {
                    skipped_untracked += 1;
                    continue;
                }
                let dst = wt_root.join(rel);
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        format!("failed to create dir {}: {e}", parent.display())
                    })?;
                }
                fs::copy(&src, &dst)
                    .map_err(|e| format!("failed to copy untracked {}: {e}", rel.display()))?;
                copied_count += 1;
            }

            // Snapshot pre-existing untracked files on detached HEAD so final patch contains ONLY worker edits.
            if copied_count > 0 {
                run_git(&wt_root, &["add", "-A"])?;
                run_git(
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
                )?;
            }

            // Capture immutable baseline tree OID so even worker `git commit`s are included in final patch.
            let baseline_tree = run_git(&wt_root, &["write-tree"])?.trim().to_string();
            if baseline_tree.is_empty() {
                return Err("failed to record worktree baseline tree OID".into());
            }

            let candidate_cwd = wt_root.join(&rel_sub);
            if !candidate_cwd.exists() {
                fs::create_dir_all(&candidate_cwd).map_err(|e| {
                    format!(
                        "failed to create worktree subdirectory {}: {e}",
                        candidate_cwd.display()
                    )
                })?;
            }
            let worktree_cwd = fs::canonicalize(&candidate_cwd).map_err(|e| {
                format!(
                    "failed to canonicalize worktree cwd {}: {e}",
                    candidate_cwd.display()
                )
            })?;
            if !worktree_cwd.starts_with(&wt_root) {
                return Err(format!(
                    "worktree cwd {} escaped worktree root {}",
                    worktree_cwd.display(),
                    wt_root.display()
                ));
            }
            Ok((baseline_tree, worktree_cwd, skipped_untracked))
        })();

        match setup_res {
            Ok((baseline_tree, worktree_cwd, skipped_untracked)) => Ok(Self {
                repo_root,
                worktree_cwd,
                wt_root,
                baseline_tree,
                call_id: call_id.to_string(),
                skipped_untracked,
            }),
            Err(e) => {
                cleanup_worktree(&repo_root, &wt_root);
                Err(e)
            }
        }
    }

    /// Tear down an unused worktree when request preparation fails before spawning the worker.
    pub fn abort(self) {
        cleanup_worktree(&self.repo_root, &self.wt_root);
    }

    /// Export lossless binary/commit-aware unified diff patch vs original baseline tree in O(1) RAM.
    /// If staging, diffing, or writing the patch fails, preserves `wt_root` on disk so zero work is lost.
    pub fn finalize(self) -> WorktreeSummary {
        let export_res = (|| -> Result<(String, Option<PathBuf>), String> {
            run_git(&self.wt_root, &["add", "-A"])?;
            let stat = run_git_capped(
                &self.wt_root,
                &[
                    "diff",
                    "--cached",
                    "--stat",
                    "--no-ext-diff",
                    &self.baseline_tree,
                ],
                64 * 1024,
            )?
            .trim()
            .to_string();
            let patch_path = stream_git_diff_to_artifact(
                &self.wt_root,
                &self.baseline_tree,
                &format!("{}.patch", self.call_id),
            )?;
            Ok((stat, patch_path))
        })();

        match export_res {
            Ok((diff_stat, patch_path)) => {
                cleanup_worktree(&self.repo_root, &self.wt_root);
                WorktreeSummary {
                    diff_stat,
                    patch_path,
                    export_error: None,
                    preserved_worktree: None,
                    skipped_untracked: self.skipped_untracked,
                }
            }
            Err(err) => WorktreeSummary {
                diff_stat: String::new(),
                patch_path: None,
                export_error: Some(err),
                preserved_worktree: Some(self.wt_root),
                skipped_untracked: self.skipped_untracked,
            },
        }
    }
}

fn snapshot_tracked_without_touching_index(repo_root: &Path, call_id: &str) -> Result<String, String> {
    let head_oid = run_git(repo_root, &["rev-parse", "--verify", "HEAD"])?
        .trim()
        .to_string();
    let head_tree = run_git(repo_root, &["rev-parse", "--verify", "HEAD^{tree}"])?
        .trim()
        .to_string();

    let mut idx_bytes = run_git_bytes(repo_root, &["rev-parse", "--git-path", "index"])?;
    while matches!(idx_bytes.last(), Some(b'\n' | b'\r')) {
        idx_bytes.pop();
    }
    let idx_rel = Path::new(OsStr::from_bytes(&idx_bytes));
    let src_index = if idx_rel.is_absolute() {
        idx_rel.to_path_buf()
    } else {
        repo_root.join(idx_rel)
    };

    let tmp_index = artifact_dir()
        .map_err(|e| e.to_string())?
        .join(format!("idx-{call_id}"));
    let _ = fs::remove_file(&tmp_index);
    if src_index.is_file() {
        fs::copy(&src_index, &tmp_index)
            .map_err(|e| format!("failed to copy git index for snapshot: {e}"))?;
    }

    let snap_res = (|| -> Result<String, String> {
        run_git_with_index(repo_root, &tmp_index, &["add", "-u"])?;
        let tree_oid = run_git_with_index(repo_root, &tmp_index, &["write-tree"])?
            .trim()
            .to_string();
        if tree_oid == head_tree {
            return Ok(head_oid);
        }
        let commit_oid = run_git(
            repo_root,
            &[
                "-c",
                "user.name=ext-models",
                "-c",
                "user.email=ext@localhost",
                "commit-tree",
                &tree_oid,
                "-p",
                &head_oid,
                "-m",
                "ext-models-tracked-snapshot",
            ],
        )?
        .trim()
        .to_string();
        Ok(commit_oid)
    })();

    let _ = fs::remove_file(&tmp_index);
    snap_res
}

fn run_git_with_index(cwd: &Path, index_file: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_INDEX_FILE", index_file)
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

fn cleanup_worktree(repo_root: &Path, wt_root: &Path) {
    let wt_str = wt_root.to_string_lossy().into_owned();
    let _ = run_git(repo_root, &["worktree", "remove", "--force", &wt_str]);
    let _ = fs::remove_dir_all(wt_root);
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

fn stream_git_diff_to_artifact(
    cwd: &Path,
    baseline_tree: &str,
    filename: &str,
) -> Result<Option<PathBuf>, String> {
    let mut child = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args([
            "diff",
            "--cached",
            "--binary",
            "--full-index",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            baseline_tree,
        ])
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git diff failed to start: {e}"))?;

    let stderr_pipe = child.stderr.take().ok_or("missing git diff stderr")?;
    let err_handle = thread::Builder::new()
        .name("ext-git-err".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut err_buf = Vec::with_capacity(1024);
            let _ = stderr_pipe.take(4096).read_to_end(&mut err_buf);
            String::from_utf8_lossy(&err_buf).trim().to_string()
        })
        .map_err(|e| format!("git stderr thread failed: {e}"))?;

    let mut stdout_pipe = child.stdout.take().ok_or("missing git diff stdout")?;
    let mut chunk = [0u8; 8192];
    let mut out_file: Option<(PathBuf, fs::File)> = None;
    let mut stream_err: Option<String> = None;

    loop {
        match stdout_pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if out_file.is_none() {
                    match open_artifact_writer(filename) {
                        Ok(pair) => out_file = Some(pair),
                        Err(e) => {
                            stream_err = Some(e);
                            let _ = child.kill();
                            break;
                        }
                    }
                }
                if let Some((ref path, ref mut f)) = out_file {
                    if let Err(e) = f.write_all(&chunk[..n]) {
                        stream_err =
                            Some(format!("failed to write artifact {}: {e}", path.display()));
                        let _ = child.kill();
                        break;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                stream_err = Some(format!("failed reading git diff stdout: {e}"));
                let _ = child.kill();
                break;
            }
        }
    }
    drop(stdout_pipe);

    let status = child
        .wait()
        .map_err(|e| format!("git diff wait failed: {e}"))?;
    let stderr_msg = err_handle.join().unwrap_or_default();

    if let Some(err) = stream_err {
        if let Some((ref p, _)) = out_file {
            let _ = fs::remove_file(p);
        }
        return Err(err);
    }
    if !status.success() {
        if let Some((ref p, _)) = out_file {
            let _ = fs::remove_file(p);
        }
        return Err(format!("git diff exited {status}: {stderr_msg}"));
    }
    if let Some((path, mut f)) = out_file {
        f.flush()
            .map_err(|e| format!("failed to flush artifact {}: {e}", path.display()))?;
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

fn run_git_capped(cwd: &Path, args: &[&str], max_bytes: usize) -> Result<String, String> {
    let (bytes, _, _) = run_git_bytes_capped(cwd, args, max_bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn run_git_bytes_capped(
    cwd: &Path,
    args: &[&str],
    max_bytes: usize,
) -> Result<(Vec<u8>, usize, bool), String> {
    let mut child = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git {:?} failed to start: {e}", args))?;

    let stderr_pipe = child.stderr.take().ok_or("missing git stderr")?;
    let err_handle = thread::Builder::new()
        .name("ext-git-err".into())
        .stack_size(128 * 1024)
        .spawn(move || {
            let mut err_buf = Vec::with_capacity(1024);
            let _ = stderr_pipe.take(4096).read_to_end(&mut err_buf);
            String::from_utf8_lossy(&err_buf).trim().to_string()
        })
        .map_err(|e| format!("git stderr thread failed: {e}"))?;

    let stdout_pipe = child.stdout.take().ok_or("missing git stdout")?;
    let mut buf = Vec::with_capacity(max_bytes.min(8192));
    let mut limited = stdout_pipe.take((max_bytes as u64).saturating_add(1));
    let _ = limited.read_to_end(&mut buf);
    let was_capped = buf.len() > max_bytes;
    let mut drained_nuls = 0usize;
    if was_capped {
        if buf[max_bytes] == 0 {
            drained_nuls += 1;
        }
        buf.truncate(max_bytes);
    }
    // Drain any remainder in O(1) memory (counting NUL terminators) so git doesn't SIGPIPE.
    let mut rest = limited.into_inner();
    let mut sink = [0u8; 4096];
    while let Ok(n) = rest.read(&mut sink) {
        if n == 0 {
            break;
        }
        drained_nuls += sink[..n].iter().filter(|&&b| b == 0).count();
    }
    drop(rest);

    let status = child
        .wait()
        .map_err(|e| format!("git {:?} wait failed: {e}", args))?;
    let stderr_msg = err_handle.join().unwrap_or_default();
    if status.success() {
        Ok((buf, drained_nuls, was_capped))
    } else {
        Err(format!("git {:?} exited {status}: {stderr_msg}", args))
    }
}

fn run_git_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(cwd)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git {:?} failed to start: {e}", args))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "git {:?} exited {}: {}",
            args,
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let bytes = run_git_bytes(cwd, args)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
