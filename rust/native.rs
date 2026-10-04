use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{Mutex, OnceCell},
    time::{Instant, timeout, timeout_at},
};

use crate::{
    domain::{ApprovalRequest, JobSpec, TurnResult},
    ports::{JobEventPort, JobFactory, JobHandle, WorkerAccessIssuer, WorkerLease},
    workspace::{DisposableWorkspace, WorkspaceManager},
};

const PROTOCOL_VERSION: u64 = 1;
const MAX_PROTOCOL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct NativeLimits {
    pub open_files: u32,
    pub output_bytes: usize,
    pub job_timeout: Duration,
}

impl Default for NativeLimits {
    fn default() -> Self {
        Self {
            open_files: 1024,
            output_bytes: 1_000_000,
            job_timeout: Duration::from_secs(60 * 60),
        }
    }
}

#[derive(Clone)]
pub struct NativeJobFactory {
    sandbox_exec: PathBuf,
    node: PathBuf,
    worker: PathBuf,
    model: String,
    thinking: String,
    permissions: String,
    limits: NativeLimits,
    workspaces: WorkspaceManager,
    access: Option<Arc<dyn WorkerAccessIssuer>>,
}

pub struct NativeJobFactoryConfig {
    pub sandbox_exec: PathBuf,
    pub node: PathBuf,
    pub worker: PathBuf,
    pub model: String,
    pub thinking: String,
    pub permissions: String,
    pub limits: NativeLimits,
    pub workspaces: WorkspaceManager,
    pub access: Option<Arc<dyn WorkerAccessIssuer>>,
}

impl NativeJobFactory {
    pub fn new(config: NativeJobFactoryConfig) -> Result<Self> {
        ensure!(cfg!(target_os = "macos"), "native sandbox requires macOS");
        for (name, path) in [
            ("sandbox-exec", &config.sandbox_exec),
            ("Node", &config.node),
            ("worker", &config.worker),
        ] {
            ensure!(path.is_absolute(), "{name} path must be absolute");
            ensure!(
                path.is_file(),
                "{name} executable or file does not exist: {}",
                path.display()
            );
        }
        ensure!(
            config.limits.open_files > 0 && config.limits.output_bytes > 0,
            "native limits must be positive"
        );
        Ok(Self {
            sandbox_exec: config.sandbox_exec.canonicalize()?,
            node: config.node.canonicalize()?,
            worker: config.worker.canonicalize()?,
            model: config.model,
            thinking: config.thinking,
            permissions: config.permissions,
            limits: config.limits,
            workspaces: config.workspaces,
            access: config.access,
        })
    }
}

#[async_trait]
impl JobFactory for NativeJobFactory {
    async fn create(&self, spec: JobSpec) -> Result<Arc<dyn JobHandle>> {
        let workspaces = self.workspaces.clone();
        let job_id = spec.job_id.clone();
        let repository = spec.repository.clone();
        let workspace =
            tokio::task::spawn_blocking(move || workspaces.create(&job_id, &repository)).await??;
        let runtime = workspace
            .path
            .parent()
            .ok_or_else(|| anyhow!("workspace has no private job directory"))?
            .join("runtime");
        std::fs::create_dir(&runtime)?;
        let leases = match &self.access {
            Some(access) => match access.issue(&spec).await {
                Ok(leases) => leases,
                Err(error) => {
                    let mut workspace = workspace;
                    let _ = workspace.dispose();
                    return Err(error);
                }
            },
            None => Vec::new(),
        };
        Ok(Arc::new(NativeJob {
            id: spec.job_id,
            sandbox_exec: self.sandbox_exec.clone(),
            node: self.node.clone(),
            worker: self.worker.clone(),
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            permissions: self.permissions.clone(),
            limits: self.limits.clone(),
            runtime,
            workspace: Mutex::new(Some(workspace)),
            process: OnceCell::new(),
            leases,
            run_sequence: AtomicU64::new(0),
            running: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
        }))
    }

    fn repositories(&self) -> Vec<String> {
        self.workspaces.aliases()
    }
}

struct WorkerProcess {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    lines: Mutex<Lines<BufReader<ChildStdout>>>,
    stderr: Mutex<Option<ChildStderr>>,
    pid: u32,
}

struct NativeJob {
    id: String,
    sandbox_exec: PathBuf,
    node: PathBuf,
    worker: PathBuf,
    model: String,
    thinking: String,
    permissions: String,
    limits: NativeLimits,
    runtime: PathBuf,
    workspace: Mutex<Option<DisposableWorkspace>>,
    process: OnceCell<WorkerProcess>,
    leases: Vec<Arc<dyn WorkerLease>>,
    run_sequence: AtomicU64,
    running: AtomicBool,
    disposed: AtomicBool,
}

#[async_trait]
impl JobHandle for NativeJob {
    async fn run_turn(&self, prompt: &str, events: Arc<dyn JobEventPort>) -> Result<TurnResult> {
        ensure!(
            self.running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "native job is already running"
        );
        for lease in &self.leases {
            lease.set_events(Some(events.clone()));
        }
        let result = async {
            ensure!(!self.disposed.load(Ordering::SeqCst), "native job is disposed");
            ensure!(!prompt.is_empty() && prompt.len() <= 1024 * 1024, "prompt is empty or too large");
            let process = self.process.get_or_try_init(|| self.start_process()).await?;
            let sequence = self.run_sequence.fetch_add(1, Ordering::SeqCst) + 1;
            let run_id = format!("{}:{sequence}", self.id);
            let deadline = Instant::now() + self.limits.job_timeout;
            self.send(json!({
                "protocolVersion": PROTOCOL_VERSION, "type": "start", "jobId": self.id,
                "runId": run_id, "prompt": prompt,
                "deadlineAt": deadline_timestamp(self.limits.job_timeout)?, "outputLimitBytes": self.limits.output_bytes,
            })).await?;
            let mut bytes = 0usize;
            let mut lines = process.lines.lock().await;
            loop {
                let line = timeout_at(deadline, lines.next_line()).await
                    .map_err(|_| anyhow!("sandbox job deadline exceeded"))??;
                let Some(line) = line else {
                    let mut diagnostics = Vec::new();
                    if let Some(stderr) = process.stderr.lock().await.as_mut() {
                        let _ = timeout(
                            Duration::from_secs(1),
                            stderr.take(64 * 1024).read_to_end(&mut diagnostics),
                        )
                        .await;
                    }
                    bail!("worker control channel closed: {}", String::from_utf8_lossy(&diagnostics).trim());
                };
                bytes = bytes.checked_add(line.len() + 1).ok_or_else(|| anyhow!("protocol output overflow"))?;
                ensure!(line.len() <= MAX_PROTOCOL_BYTES && bytes <= self.limits.output_bytes + 1024 * 1024, "worker protocol output exceeded its limit");
                let message: Value = serde_json::from_str(&line).context("worker sent malformed JSON")?;
                if message.get("protocolVersion").and_then(Value::as_u64) != Some(PROTOCOL_VERSION)
                    || message.get("jobId").and_then(Value::as_str) != Some(&self.id)
                    || message.get("runId").and_then(Value::as_str) != Some(&run_id) { continue; }
                match message.get("type").and_then(Value::as_str) {
                    Some("status") => events.status(required_string(&message, "message")?).await?,
                    Some("approval_request") => {
                        let choices = message.get("choices").and_then(Value::as_array)
                            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
                        let answer = events.request_approval(ApprovalRequest {
                            title: required_string(&message, "title")?.to_owned(),
                            detail: required_string(&message, "detail")?.to_owned(), choices,
                        }).await?;
                        self.send(json!({
                            "protocolVersion": PROTOCOL_VERSION, "type": "approval_response", "jobId": self.id,
                            "runId": run_id, "requestId": required_string(&message, "requestId")?, "answer": answer,
                        })).await?;
                    }
                    Some("completion") => {
                        let output = required_string(&message, "output")?;
                        ensure!(output.len() <= self.limits.output_bytes, "worker output exceeded its limit");
                        let output = output.to_owned();
                        drop(lines);
                        let changed_files = self.capture_patch().await?;
                        return Ok(TurnResult { output, changed_files });
                    }
                    Some("failure") => {
                        let failure = required_string(&message, "message")?.to_owned();
                        drop(lines); self.dispose().await; bail!(failure);
                    }
                    _ => { drop(lines); self.dispose().await; bail!("worker sent an invalid protocol message"); }
                }
            }
        }.await;
        for lease in &self.leases {
            lease.set_events(None);
        }
        self.running.store(false, Ordering::SeqCst);
        if result.is_err() {
            self.dispose().await;
        }
        result
    }

    async fn steer(&self, message: &str) -> Result<()> {
        let sequence = self.run_sequence.load(Ordering::SeqCst);
        ensure!(
            sequence > 0 && self.running.load(Ordering::SeqCst),
            "sandbox job is not running"
        );
        self.send(json!({"protocolVersion": PROTOCOL_VERSION, "type": "steer", "jobId": self.id, "runId": format!("{}:{sequence}", self.id), "message": message})).await
    }

    async fn cancel(&self) -> Result<()> {
        let sequence = self.run_sequence.load(Ordering::SeqCst);
        if sequence > 0 && self.process.get().is_some() {
            let _ = self.send(json!({"protocolVersion": PROTOCOL_VERSION, "type": "cancel", "jobId": self.id, "runId": format!("{}:{sequence}", self.id), "reason": "operator"})).await;
        }
        self.dispose().await;
        Ok(())
    }
}

impl NativeJob {
    async fn start_process(&self) -> Result<WorkerProcess> {
        let workspace = self
            .workspace
            .lock()
            .await
            .as_ref()
            .ok_or_else(|| anyhow!("workspace is unavailable"))?
            .path
            .canonicalize()?;
        let runtime = self.runtime.canonicalize()?;
        let (environment, sockets) = native_access(&self.leases)?;
        let profile = sandbox_profile(&self.node, &self.worker, &workspace, &runtime, &sockets)?;
        let mut command = Command::new(&self.sandbox_exec);
        command
            .args(["-p", &profile])
            .arg(&self.node)
            .arg(&self.worker)
            .current_dir(&workspace)
            .env_clear()
            .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", &runtime)
            .env("TMPDIR", &runtime)
            .env("POCKET_AGENT_WORKSPACE", &workspace)
            .env("POCKET_AGENT_AGENT_DIR", runtime.join("agent"))
            .env("POCKET_AGENT_MODEL", &self.model)
            .env("POCKET_AGENT_THINKING", &self.thinking)
            .env("POCKET_AGENT_PERMISSIONS", &self.permissions)
            .env("POCKET_AGENT_JOB_ID", &self.id)
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let open_files = self.limits.open_files as libc::rlim_t;
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let nofile = libc::rlimit {
                    rlim_cur: open_files,
                    rlim_max: open_files,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &nofile) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let core = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::setrlimit(libc::RLIMIT_CORE, &core) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("start native sandbox worker")?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow!("native worker PID unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("worker stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("worker stdout unavailable"))?;
        let mut stderr = child.stderr.take();
        let mut lines = BufReader::new(stdout).lines();
        let hello = match timeout(Duration::from_secs(5), lines.next_line()).await {
            Err(_) => bail!("worker protocol handshake timed out"),
            Ok(Err(error)) => return Err(error.into()),
            Ok(Ok(None)) => {
                let mut diagnostics = Vec::new();
                if let Some(stderr) = stderr.as_mut() {
                    let _ = timeout(
                        Duration::from_secs(1),
                        stderr.take(64 * 1024).read_to_end(&mut diagnostics),
                    )
                    .await;
                }
                bail!(
                    "worker exited before protocol handshake: {}",
                    String::from_utf8_lossy(&diagnostics).trim()
                )
            }
            Ok(Ok(Some(hello))) => hello,
        };
        let value: Value = serde_json::from_str(&hello).context("worker hello is malformed")?;
        ensure!(
            value.get("type").and_then(Value::as_str) == Some("hello")
                && value
                    .get("supportedVersions")
                    .and_then(Value::as_array)
                    .is_some_and(|versions| versions
                        .iter()
                        .any(|version| version.as_u64() == Some(PROTOCOL_VERSION))),
            "worker protocol version is incompatible"
        );
        let process = WorkerProcess {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            lines: Mutex::new(lines),
            stderr: Mutex::new(stderr),
            pid,
        };
        write_message(
            &process.stdin,
            &json!({"type": "hello", "supportedVersions": [PROTOCOL_VERSION]}),
        )
        .await?;
        Ok(process)
    }

    async fn send(&self, message: Value) -> Result<()> {
        let process = self
            .process
            .get()
            .ok_or_else(|| anyhow!("worker is not connected"))?;
        write_message(&process.stdin, &message).await
    }

    async fn capture_patch(&self) -> Result<Vec<crate::domain::ChangedFile>> {
        let mut workspace = self.workspace.lock().await;
        let workspace = workspace
            .as_mut()
            .ok_or_else(|| anyhow!("workspace is unavailable"))?;
        Ok(workspace.export_patch()?.files)
    }

    async fn dispose(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        revoke(&self.leases);
        if let Some(process) = self.process.get() {
            unsafe {
                libc::killpg(process.pid as libc::pid_t, libc::SIGKILL);
            }
            let _ = process.child.lock().await.wait().await;
        }
        if let Some(mut workspace) = self.workspace.lock().await.take() {
            let _ = workspace.dispose();
        }
    }
}

impl Drop for NativeJob {
    fn drop(&mut self) {
        revoke(&self.leases);
    }
}

fn native_access(
    leases: &[Arc<dyn WorkerLease>],
) -> Result<(BTreeMap<String, String>, Vec<PathBuf>)> {
    let mut environment = BTreeMap::new();
    let mut mappings = Vec::new();
    let mut sockets = Vec::new();
    for lease in leases {
        for mount in lease.mounts() {
            ensure!(
                mount.source.is_absolute() && mount.destination.is_absolute(),
                "private mount paths must be absolute"
            );
            ensure!(
                !mappings
                    .iter()
                    .any(|(_, destination): &(PathBuf, PathBuf)| destination == &mount.destination),
                "duplicate private mount destination"
            );
            let source = mount
                .source
                .canonicalize()
                .with_context(|| format!("resolve private mount {}", mount.source.display()))?;
            sockets.push(source.clone());
            mappings.push((source, mount.destination));
        }
        for (name, mut value) in lease.environment() {
            for (source, destination) in &mappings {
                if let Ok(relative) = Path::new(&value).strip_prefix(destination) {
                    value = source.join(relative).to_string_lossy().into_owned();
                }
            }
            ensure!(
                environment.insert(name, value).is_none(),
                "duplicate worker environment variable"
            );
        }
    }
    Ok((environment, sockets))
}

fn sandbox_profile(
    node: &Path,
    worker: &Path,
    workspace: &Path,
    runtime: &Path,
    sockets: &[PathBuf],
) -> Result<String> {
    let quote = |path: &Path| -> Result<String> {
        let value = path
            .to_str()
            .ok_or_else(|| anyhow!("sandbox path is not UTF-8"))?;
        Ok(format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    };
    let worker_modules = quote(
        &worker
            .parent()
            .unwrap_or(Path::new("/"))
            .join("node_modules"),
    )?;
    let node_root = quote(
        node.parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new("/")),
    )?;
    let node = quote(node)?;
    let worker = quote(worker)?;
    let workspace = quote(workspace)?;
    let runtime = quote(runtime)?;
    let mut profile = format!(
        r#"(version 1)
(deny default)
(import "system.sb")
(allow process-fork)
(allow process-exec (literal {node}) (subpath "/bin") (subpath "/usr/bin") (subpath "/opt/homebrew/bin") (subpath "/opt/homebrew/Cellar") (subpath "/Applications/Xcode.app/Contents/Developer") (subpath "/Library/Developer/CommandLineTools") (subpath {workspace}) (subpath {runtime}))
(allow signal (target self))
(allow file-read-metadata)
(allow file-read* file-map-executable (subpath {node_root}) (subpath "/opt/homebrew/Cellar") (subpath "/opt/homebrew/opt") (subpath "/opt/homebrew/lib") (subpath "/Applications/Xcode.app/Contents/Developer") (subpath "/Library/Developer/CommandLineTools"))
(allow file-read* (subpath "/opt/homebrew/etc/openssl@3"))
(allow file-read* (literal {worker}) (subpath {worker_modules}) (subpath {workspace}) (subpath {runtime}))
(allow file-write* (subpath {workspace}) (subpath {runtime}))
"#
    );
    for socket_dir in sockets {
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath {}))\n",
            quote(socket_dir)?
        ));
        profile.push_str(&format!(
            "(allow network-outbound (remote unix-socket (subpath {})))\n",
            quote(socket_dir)?
        ));
    }
    Ok(profile)
}

async fn write_message(stdin: &Mutex<ChildStdin>, message: &Value) -> Result<()> {
    let mut stdin = stdin.lock().await;
    let mut payload = serde_json::to_vec(message)?;
    payload.push(b'\n');
    stdin.write_all(&payload).await?;
    stdin.flush().await?;
    Ok(())
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("worker message is missing {key}"))
}

fn revoke(leases: &[Arc<dyn WorkerLease>]) {
    for lease in leases {
        lease.revoke();
    }
}

fn deadline_timestamp(duration: Duration) -> Result<String> {
    let deadline = time::OffsetDateTime::now_utc() + time::Duration::try_from(duration)?;
    Ok(deadline.format(&time::format_description::well_known::Rfc3339)?)
}
