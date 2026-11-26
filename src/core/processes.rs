//! Background process management.
//!
//! Owns long-running child processes spawned through
//! [`ProcessRegistry::start_with`] and lets the agent *follow* them across
//! turns with [`ProcessRegistry::poll`] / [`ProcessRegistry::output`], or
//! stop them with [`ProcessRegistry::kill`] / [`ProcessRegistry::clean`].
//!
//! Spawning is a purpose-built-tool API: in-process tools call
//! [`ProcessRegistry::start_with`] directly, while the LLM-facing `process`
//! tool is management-only, so the LLM never specifies arbitrary shell
//! commands.
//!
//! There is exactly one [`ProcessRegistry`] per `archon` invocation. Background
//! jobs live for the lifetime of the binary (one OS process = one job table), so
//! the registry is a process-wide singleton rather than state threaded through
//! the tool pipeline. [`CoreEngine::run`](super::CoreEngine::run) calls
//! [`ProcessRegistry::shutdown`] on exit so nothing is left running.

use anyhow::{Context, Result, bail};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::Mutex,
};
use tracing::{debug, info};

use crate::core::config::expand_tilde;
use crate::tools::utils::path::{ensure_within_root, validate_relative_path, workspace_root};

/// Maximum number of concurrently *running* jobs (running = not finished).
const MAX_JOBS: usize = 16;
/// Total bytes of recorded output per job (stdout + stderr). Beyond this we keep
/// draining (so a chatty child never blocks on a full pipe buffer) but discard
/// the excess.
const MAX_OUT_BYTES: usize = 512 * 1024;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn new_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("proc-{ts}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// A short-lived snapshot of a job for display in `list`.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub source: String,
    pub elapsed_secs: u64,
    pub finished: bool,
    /// Exit code if the process exited normally; `None` if killed by a signal.
    pub exit_code: Option<i32>,
    pub out_bytes: usize,
    /// Bytes of output not yet returned by a `poll` call.
    pub unserved_bytes: usize,
    pub truncated: bool,
}

/// Handle returned by [`ProcessRegistry::start_with`].
#[derive(Debug, Clone)]
pub struct SpawnedJob {
    pub id: String,
    pub pid: u32,
}

/// Parameters for spawning a background job via [`ProcessRegistry::start_with`].
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// Which tool/feature spawned this job (e.g. the tool's `NAME`).
    pub source: String,
}

#[cfg(unix)]
pub(crate) fn kill_process_group(pgid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    if let Ok(pgid) = i32::try_from(pgid)
        && pgid > 0
    {
        // SAFETY: sending SIGKILL to a negative process-group id (-pgid) is a
        // standard POSIX syscall with no memory-safety preconditions.
        unsafe {
            let _ = kill(-pgid, SIGKILL);
        }
    }
}

struct Job {
    id: String,
    pid: u32,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    source: String,
    started: Instant,
    /// Recorded (capped) combined output: stdout, then `\n`, then stderr.
    out: Vec<u8>,
    truncated: bool,
    /// Byte length of `out` already served to the client via `poll` (cursor).
    served: usize,
    exit: Option<ExitStatus>,
    finished: bool,
    /// Own process-group id (with `process_group(0)`, equals the child pid).
    #[cfg(unix)]
    pgid: Option<u32>,
}

impl Job {
    fn to_info(&self) -> JobInfo {
        let unserved = self.out.len().saturating_sub(self.served);
        JobInfo {
            id: self.id.clone(),
            pid: self.pid,
            command: self.command.clone(),
            args: self.args.clone(),
            cwd: self.cwd.clone(),
            source: self.source.clone(),
            elapsed_secs: self.started.elapsed().as_secs(),
            finished: self.finished,
            exit_code: self.exit.and_then(|s| s.code()),
            out_bytes: self.out.len(),
            unserved_bytes: unserved,
            truncated: self.truncated,
        }
    }

    fn render_output(&self, body: String) -> String {
        let status = if self.finished {
            format!(
                "finished (exit code {:?})",
                self.exit.and_then(|s| s.code())
            )
        } else {
            "running".to_string()
        };
        let trunc = if self.truncated {
            " [output truncated]"
        } else {
            ""
        };
        let body = if body.is_empty() {
            "(no output)".to_string()
        } else {
            body
        };
        format!("[job {}] {status}{trunc}\n{body}", self.id)
    }
}

fn terminate_running_jobs(map: &mut HashMap<String, Job>) -> usize {
    let mut terminated = 0usize;
    for job in map.values_mut().filter(|j| !j.finished) {
        terminated += 1;
        #[cfg(unix)]
        if let Some(pgid) = job.pgid.take() {
            kill_process_group(pgid);
        }
    }
    terminated
}

pub struct ProcessRegistry {
    jobs: Mutex<HashMap<String, Job>>,
}

impl ProcessRegistry {
    /// Returns the process-wide registry, creating it on first use.
    ///
    /// A single canonical instance is stored and handed out for the lifetime of
    /// the process, so every tool invocation in the same `archon` binary shares
    /// the same table of background jobs.
    pub fn global() -> Arc<Self> {
        // SAFETY: `Arc::new` cannot fail and the instance lives for the process.
        static INSTANCE: std::sync::OnceLock<Arc<ProcessRegistry>> = std::sync::OnceLock::new();
        INSTANCE
            .get_or_init(|| {
                Arc::new(ProcessRegistry {
                    jobs: Mutex::new(HashMap::new()),
                })
            })
            .clone()
    }

    /// Start a background job with the given options and return its handle.
    ///
    /// Reserved for purpose-built tools that need to manage long-running
    /// child processes. The LLM never calls this directly — the `process`
    /// tool is management-only (list, output, poll, kill, clean).
    ///
    /// The working directory defaults to the workspace root and is validated to
    /// stay within it. The command inherits the ambient environment (so `PATH`
    /// and `HOME` work) with any `env` overrides applied on top.
    pub async fn start_with(self: &Arc<Self>, opts: &SpawnOptions) -> Result<SpawnedJob> {
        let cmd = opts.command.trim();
        if cmd.is_empty() {
            bail!("'command' is required to spawn a background job");
        }

        // Resolve + validate the working directory (default to the workspace root).
        let root = workspace_root()?;
        let workdir: PathBuf = match &opts.cwd {
            Some(raw) if !raw.trim().is_empty() => {
                let raw = raw.trim();
                let expanded = expand_tilde(raw);
                let candidate = if expanded.is_absolute() {
                    expanded
                } else {
                    root.join(validate_relative_path(raw)?)
                };
                if !candidate.exists() {
                    bail!("'cwd' does not exist: '{raw}'");
                }
                ensure_within_root(&candidate, &root, "cwd")?
            }
            _ => root,
        };

        // Enforce the concurrent-job cap before spawning.
        {
            let map = self.jobs.lock().await;
            let running = map.values().filter(|j| !j.finished).count();
            if running >= MAX_JOBS {
                bail!(
                    "Cannot start: {running} background job(s) already running (max {MAX_JOBS}). \
                     Use action=\"clean\" to stop some first."
                );
            }
        }

        let mut c = Command::new(cmd);
        c.args(&opts.args)
            .current_dir(&workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        c.process_group(0);
        for (k, v) in &opts.env {
            c.env(k, v);
        }

        let mut child = c
            .spawn()
            .with_context(|| format!("failed to spawn background process '{cmd}'"))?;
        let id = new_id();
        let pid = child.id().unwrap_or(0);

        {
            let mut map = self.jobs.lock().await;
            map.insert(
                id.clone(),
                Job {
                    id: id.clone(),
                    pid,
                    command: cmd.to_string(),
                    args: opts.args.clone(),
                    cwd: Some(workdir.to_string_lossy().into_owned()),
                    source: opts.source.clone(),
                    started: Instant::now(),
                    out: Vec::new(),
                    truncated: false,
                    served: 0,
                    exit: None,
                    finished: false,
                    #[cfg(unix)]
                    pgid: Some(pid),
                },
            );
        }

        let out_pipe = child.stdout.take().context("failed to capture stdout")?;
        let err_pipe = child.stderr.take().context("failed to capture stderr")?;

        // Detached drain task: reads both pipes to EOF *concurrently* (so a
        // chatty child never blocks on a full pipe buffer), records capped
        // output, reaps the child, and reaps any process-group stragglers.
        let reg = Arc::clone(self);
        tokio::spawn(drain_task(reg, id.clone(), pid, child, out_pipe, err_pipe));

        debug!("Started background job {id} [{}]: {cmd} {:?} (pid {pid})", opts.source, opts.args);
        Ok(SpawnedJob { id, pid })
    }

    /// Snapshot all jobs (running and finished).
    pub async fn list(self: &Arc<Self>) -> Vec<JobInfo> {
        let map = self.jobs.lock().await;
        map.values().map(Job::to_info).collect()
    }

    /// Return the last `tail_lines` lines of a job's recorded output.
    /// A `tail_lines` of `0` returns the entire recorded output. This is a
    /// non-consuming peek: it does not advance the `poll` cursor.
    pub async fn output(self: &Arc<Self>, id: &str, tail_lines: usize) -> Result<String> {
        let map = self.jobs.lock().await;
        let job = job_by_id(&map, id)?;
        let text = String::from_utf8_lossy(&job.out);
        let lines: Vec<&str> = text.lines().collect();
        let start = if tail_lines == 0 {
            0
        } else {
            lines.len().saturating_sub(tail_lines)
        };
        Ok(job.render_output(lines[start..].join("\n")))
    }

    /// Wait up to `wait_ms` for the job to finish or emit new output, then
    /// return only the *new* output since the last `poll` (incremental).
    pub async fn poll(self: &Arc<Self>, id: &str, wait_ms: u64) -> Result<String> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            {
                let map = self.jobs.lock().await;
                let job = job_by_id(&map, id)?;
                if job.finished || Instant::now() >= deadline {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let mut map = self.jobs.lock().await;
        let job = map
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("Unknown job id '{id}'. Use action=\"list\"."))?;
        let text = String::from_utf8_lossy(&job.out);
        let start = text.floor_char_boundary(job.served.min(text.len()));
        let new_part = text[start..].trim_end().to_string();
        let new_len = text.len();
        drop(text);
        job.served = new_len;
        Ok(job.render_output(new_part))
    }

    /// Terminate a running job (SIGKILL its whole process group).
    pub async fn kill(self: &Arc<Self>, id: &str) -> Result<String> {
        let mut map = self.jobs.lock().await;
        let job = map.get_mut(id).ok_or_else(|| {
            anyhow::anyhow!("Unknown job id '{id}'. Use action=\"list\" to see active jobs.")
        })?;
        if job.finished {
            let exit_code = job.exit.and_then(|s| s.code());
            map.remove(id);
            return Ok(format!(
                "Job {id} already finished (exit code {exit_code:?}); removed from the table."
            ));
        }
        let pid = job.pid;
        #[cfg(unix)]
        if let Some(pgid) = job.pgid.take() {
            kill_process_group(pgid);
        }
        Ok(format!("Job {id} (pid {pid}) terminated."))
    }

    /// Terminate all running jobs and clear the job table.
    pub async fn clean(self: &Arc<Self>) -> Result<String> {
        let mut map = self.jobs.lock().await;
        let total = map.len();
        let terminated = terminate_running_jobs(&mut map);
        map.clear();
        Ok(format!(
            "Cleaned up {total} job(s) ({terminated} running were terminated)."
        ))
    }

    /// Kill every running job. Called by [`CoreEngine::run`](super::CoreEngine::run) on shutdown.
    pub async fn shutdown(self: &Arc<Self>) {
        let mut map = self.jobs.lock().await;
        let terminated = terminate_running_jobs(&mut map);
        if terminated > 0 {
            info!("Shutting down: terminated {terminated} running background job(s)");
        }
        map.retain(|_, j| j.finished);
        debug!(
            "Process registry shutdown complete ({} finished retained)",
            map.len()
        );
    }
}

/// Drain a pipe to EOF, streaming each chunk into the job's recorded output so
/// a running process's output is visible live (not only at exit). Keeps reading
/// (and discarding beyond the cap) so a chatty child never blocks on a full
/// pipe buffer.
async fn drain_into<R: AsyncRead + Unpin>(reg: &Arc<ProcessRegistry>, id: &str, mut reader: R) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => append_capped(reg, id, &buf[..n]).await,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

/// Append a chunk to the job's recorded output, enforcing the total cap.
async fn append_capped(reg: &Arc<ProcessRegistry>, id: &str, chunk: &[u8]) {
    let mut map = reg.jobs.lock().await;
    let Some(job) = map.get_mut(id) else {
        return; // Job removed (e.g. via `clean`) while draining.
    };
    if job.out.len() < MAX_OUT_BYTES {
        let room = MAX_OUT_BYTES - job.out.len();
        job.out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if job.out.len() >= MAX_OUT_BYTES {
            job.truncated = true;
        }
    }
}

/// Detached task that drains a job's pipes (streaming output live), reaps the
/// child, and marks the job finished.
async fn drain_task(
    reg: Arc<ProcessRegistry>,
    id: String,
    pid: u32,
    mut child: Child,
    out_pipe: tokio::process::ChildStdout,
    err_pipe: tokio::process::ChildStderr,
) {
    // Drain both pipes concurrently (so a chatty child never blocks on a full
    // pipe buffer), streaming output into the job as it arrives.
    let (o_id, e_id) = (id.clone(), id.clone());
    let (o_fut, e_fut) = (
        drain_into(&reg, &o_id, out_pipe),
        drain_into(&reg, &e_id, err_pipe),
    );
    tokio::join!(o_fut, e_fut);

    let status = child.wait().await;

    // Reap any stragglers left in the job's own process group.
    #[cfg(unix)]
    kill_process_group(pid);

    let mut map = reg.jobs.lock().await;
    if let Some(job) = map.get_mut(&id) {
        job.exit = status.as_ref().ok().copied();
        job.finished = true;
    }
    drop(map);
    debug!("Background job {id} finished (status: {status:?})");
}

fn job_by_id<'a>(map: &'a HashMap<String, Job>, id: &str) -> Result<&'a Job> {
    map.get(id).ok_or_else(|| {
        anyhow::anyhow!("Unknown job id '{id}'. Use action=\"list\" to see active jobs.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_id_is_a_helpful_error() {
        let reg = ProcessRegistry::global();
        let err = reg.output("does-not-exist", 10).await.unwrap_err();
        assert!(err.to_string().contains("Unknown job id"), "{err}");
    }
}
