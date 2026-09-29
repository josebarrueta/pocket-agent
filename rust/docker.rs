use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
    process::{Child, ChildStdin, ChildStdout, Command as AsyncCommand},
    sync::{Mutex, OnceCell},
    time::{Instant, timeout, timeout_at},
};
use uuid::Uuid;

use crate::{
    domain::{ApprovalRequest, JobSpec, TurnResult},
    ports::{JobEventPort, JobFactory, JobHandle, WorkerAccessIssuer, WorkerLease},
    workspace::{DisposableWorkspace, WorkspaceManager},
};

const MANAGED_LABEL: &str = "pocket-agent.managed=true";
const PROTOCOL_VERSION: u64 = 1;
const MAX_PROTOCOL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct DockerLimits {
    pub cpus: f64,
    pub memory_bytes: u64,
    pub pids: u32,
    pub temporary_storage_bytes: u64,
    pub workspace_storage_bytes: u64,
    pub open_files: u32,
    pub output_bytes: usize,
    pub job_timeout: Duration,
}

impl Default for DockerLimits {
    fn default() -> Self {
        Self {
            cpus: 1.0,
            memory_bytes: 1024 * 1024 * 1024,
            pids: 256,
            temporary_storage_bytes: 256 * 1024 * 1024,
            workspace_storage_bytes: 768 * 1024 * 1024,
            open_files: 1024,
            output_bytes: 1_000_000,
            job_timeout: Duration::from_secs(60 * 60),
        }
    }
}

#[derive(Clone)]
pub struct DockerJobFactory {
    docker: PathBuf,
    tar: PathBuf,
    image: String,
    model: String,
    thinking: String,
    permissions: String,
    limits: DockerLimits,
    workspaces: WorkspaceManager,
    access: Option<Arc<dyn WorkerAccessIssuer>>,
}

pub struct DockerJobFactoryConfig {
    pub docker: PathBuf,
    pub tar: PathBuf,
    pub image: String,
    pub model: String,
    pub thinking: String,
    pub permissions: String,
    pub limits: DockerLimits,
    pub workspaces: WorkspaceManager,
    pub access: Option<Arc<dyn WorkerAccessIssuer>>,
    pub allow_unpinned_image_for_tests: bool,
}

impl DockerJobFactory {
    pub fn new(config: DockerJobFactoryConfig) -> Result<Self> {
        ensure!(
            config.docker.is_absolute() && config.tar.is_absolute(),
            "Docker and tar paths must be absolute"
        );
        ensure!(
            config.allow_unpinned_image_for_tests || is_pinned_image(&config.image),
            "Worker image must be digest-pinned"
        );
        ensure!(
            config.limits.cpus.is_finite() && config.limits.cpus > 0.0,
            "CPU limit must be positive"
        );
        ensure!(
            config.limits.memory_bytes > 0
                && config.limits.pids > 0
                && config.limits.output_bytes > 0,
            "Docker limits must be positive"
        );
        Ok(Self {
            docker: config.docker,
            tar: config.tar,
            image: config.image,
            model: config.model,
            thinking: config.thinking,
            permissions: config.permissions,
            limits: config.limits,
            workspaces: config.workspaces,
            access: config.access,
        })
    }

    pub async fn reconcile(&self) -> Result<()> {
        let docker = self.docker.clone();
        tokio::task::spawn_blocking(move || {
            for kind in ["container", "volume"] {
                let list = if kind == "container" {
                    run(
                        &docker,
                        &[
                            "ps",
                            "--all",
                            "--quiet",
                            "--filter",
                            &format!("label={MANAGED_LABEL}"),
                        ],
                        4 * 1024 * 1024,
                    )?
                } else {
                    run(
                        &docker,
                        &[
                            "volume",
                            "ls",
                            "--quiet",
                            "--filter",
                            &format!("label={MANAGED_LABEL}"),
                        ],
                        4 * 1024 * 1024,
                    )?
                };
                let ids = list
                    .lines()
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>();
                if !ids.is_empty() {
                    let mut arguments = if kind == "container" {
                        vec!["rm", "--force"]
                    } else {
                        vec!["volume", "rm", "--force"]
                    };
                    arguments.extend(ids);
                    run(&docker, &arguments, 4 * 1024 * 1024)?;
                }
            }
            Ok(())
        })
        .await?
    }
}

#[async_trait]
impl JobFactory for DockerJobFactory {
    async fn create(&self, spec: JobSpec) -> Result<Arc<dyn JobHandle>> {
        let workspaces = self.workspaces.clone();
        let job_id = spec.job_id.clone();
        let repository = spec.repository.clone();
        let workspace =
            tokio::task::spawn_blocking(move || workspaces.create(&job_id, &repository)).await??;
        let suffix = &Uuid::new_v4().simple().to_string()[..8];
        let container = format!("pocket-agent-job-{}-{suffix}", spec.job_id);
        let volume = format!("pocket-agent-workspace-{}-{suffix}", spec.job_id);
        let leases = if let Some(access) = &self.access {
            access.issue(&spec).await?
        } else {
            Vec::new()
        };
        let docker = self.docker.clone();
        let tar = self.tar.clone();
        let image = self.image.clone();
        let limits = self.limits.clone();
        let model = self.model.clone();
        let thinking = self.thinking.clone();
        let permissions = self.permissions.clone();
        let workspace_path = workspace.path.clone();
        let create_result = tokio::task::spawn_blocking({
            let container = container.clone();
            let volume = volume.clone();
            let leases = leases.clone();
            let create_job_id = spec.job_id.clone();
            move || {
                create_container(CreateContainer {
                    docker: &docker,
                    tar: &tar,
                    image: &image,
                    container: &container,
                    volume: &volume,
                    job_id: &create_job_id,
                    workspace: &workspace_path,
                    model: &model,
                    thinking: &thinking,
                    permissions: &permissions,
                    limits: &limits,
                    leases: &leases,
                })
            }
        })
        .await?;
        if let Err(error) = create_result {
            revoke(&leases);
            let _ = cleanup_sync(&self.docker, &container, &volume);
            let mut workspace = workspace;
            let _ = workspace.dispose();
            return Err(error);
        }
        Ok(Arc::new(DockerJob {
            id: spec.job_id,
            docker: self.docker.clone(),
            tar: self.tar.clone(),
            image: self.image.clone(),
            container,
            volume,
            limits: self.limits.clone(),
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
}

struct DockerJob {
    id: String,
    docker: PathBuf,
    tar: PathBuf,
    image: String,
    container: String,
    volume: String,
    limits: DockerLimits,
    workspace: Mutex<Option<DisposableWorkspace>>,
    process: OnceCell<WorkerProcess>,
    leases: Vec<Arc<dyn WorkerLease>>,
    run_sequence: AtomicU64,
    running: AtomicBool,
    disposed: AtomicBool,
}

#[async_trait]
impl JobHandle for DockerJob {
    async fn run_turn(&self, prompt: &str, events: Arc<dyn JobEventPort>) -> Result<TurnResult> {
        ensure!(
            self.running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok(),
            "Docker job is already running"
        );
        for lease in &self.leases {
            lease.set_events(Some(events.clone()));
        }
        let result = async {
        ensure!(
            !self.disposed.load(Ordering::SeqCst),
            "Docker job is disposed"
        );
        ensure!(
            !prompt.is_empty() && prompt.len() <= 1024 * 1024,
            "Prompt is empty or too large"
        );
        let process = self
            .process
            .get_or_try_init(|| self.start_process())
            .await?;
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
            let line = timeout_at(deadline, lines.next_line())
                .await
                .map_err(|_| anyhow!("Sandbox job deadline exceeded"))??
                .ok_or_else(|| anyhow!("Worker control channel closed"))?;
            bytes = bytes
                .checked_add(line.len() + 1)
                .ok_or_else(|| anyhow!("Protocol output overflow"))?;
            ensure!(
                line.len() <= MAX_PROTOCOL_BYTES && bytes <= self.limits.output_bytes + 1024 * 1024,
                "Worker protocol output exceeded its limit"
            );
            let message: Value =
                serde_json::from_str(&line).context("Worker sent malformed JSON")?;
            if message.get("protocolVersion").and_then(Value::as_u64) != Some(PROTOCOL_VERSION)
                || message.get("jobId").and_then(Value::as_str) != Some(&self.id)
                || message.get("runId").and_then(Value::as_str) != Some(&run_id)
            {
                continue;
            }
            match message.get("type").and_then(Value::as_str) {
                Some("status") => events.status(required_string(&message, "message")?).await?,
                Some("approval_request") => {
                    let choices = message
                        .get("choices")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    let answer = events
                        .request_approval(ApprovalRequest {
                            title: required_string(&message, "title")?.to_owned(),
                            detail: required_string(&message, "detail")?.to_owned(),
                            choices,
                        })
                        .await?;
                    self.send(json!({
                        "protocolVersion": PROTOCOL_VERSION, "type": "approval_response", "jobId": self.id,
                        "runId": run_id, "requestId": required_string(&message, "requestId")?, "answer": answer,
                    })).await?;
                }
                Some("completion") => {
                    let output = required_string(&message, "output")?;
                    ensure!(
                        output.len() <= self.limits.output_bytes,
                        "Worker output exceeded its limit"
                    );
                    drop(lines);
                    let changed_files = self.capture_patch().await?;
                    return Ok(TurnResult {
                        output: output.to_owned(),
                        changed_files,
                    });
                }
                Some("failure") => {
                    let failure = required_string(&message, "message")?.to_owned();
                    drop(lines);
                    self.dispose().await;
                    bail!(failure);
                }
                _ => {
                    drop(lines);
                    self.dispose().await;
                    bail!("Worker sent an invalid protocol message");
                }
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
            "Sandbox job is not running"
        );
        self.send(json!({
            "protocolVersion": PROTOCOL_VERSION, "type": "steer", "jobId": self.id,
            "runId": format!("{}:{sequence}", self.id), "message": message,
        }))
        .await
    }

    async fn cancel(&self) -> Result<()> {
        let sequence = self.run_sequence.load(Ordering::SeqCst);
        if sequence > 0 && self.process.get().is_some() {
            let _ = self
                .send(json!({
                    "protocolVersion": PROTOCOL_VERSION, "type": "cancel", "jobId": self.id,
                    "runId": format!("{}:{sequence}", self.id), "reason": "operator",
                }))
                .await;
        }
        self.dispose().await;
        Ok(())
    }
}

impl DockerJob {
    async fn start_process(&self) -> Result<WorkerProcess> {
        let mut child = AsyncCommand::new(&self.docker)
            .args(["start", "--attach", "--interactive", &self.container])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Docker stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Docker stdout unavailable"))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut diagnostics = Vec::new();
                let _ = stderr.take(64 * 1024).read_to_end(&mut diagnostics).await;
            });
        }
        let mut lines = BufReader::new(stdout).lines();
        let hello = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .map_err(|_| anyhow!("Worker protocol handshake timed out"))??
            .ok_or_else(|| anyhow!("Worker exited before protocol handshake"))?;
        let value: Value = serde_json::from_str(&hello).context("Worker hello is malformed")?;
        ensure!(
            value.get("type").and_then(Value::as_str) == Some("hello")
                && value
                    .get("supportedVersions")
                    .and_then(Value::as_array)
                    .is_some_and(|versions| versions
                        .iter()
                        .any(|version| version.as_u64() == Some(PROTOCOL_VERSION))),
            "Worker protocol version is incompatible"
        );
        let process = WorkerProcess {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            lines: Mutex::new(lines),
        };
        write_message(
            &process.stdin,
            &json!({ "type": "hello", "supportedVersions": [PROTOCOL_VERSION] }),
        )
        .await?;
        Ok(process)
    }

    async fn send(&self, message: Value) -> Result<()> {
        let process = self
            .process
            .get()
            .ok_or_else(|| anyhow!("Worker is not connected"))?;
        write_message(&process.stdin, &message).await
    }

    async fn capture_patch(&self) -> Result<Vec<crate::domain::ChangedFile>> {
        let docker = self.docker.clone();
        let tar = self.tar.clone();
        let image = self.image.clone();
        let container = self.container.clone();
        let volume = self.volume.clone();
        let mut workspace = self.workspace.lock().await;
        let workspace = workspace
            .as_mut()
            .ok_or_else(|| anyhow!("Workspace is unavailable"))?;
        let destination = workspace.path.clone();
        tokio::task::spawn_blocking(move || {
            export_workspace(&docker, &tar, &image, &container, &volume, &destination)
        })
        .await??;
        Ok(workspace.export_patch()?.files)
    }

    async fn dispose(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        revoke(&self.leases);
        let docker = self.docker.clone();
        let container = self.container.clone();
        let volume = self.volume.clone();
        let _ =
            tokio::task::spawn_blocking(move || cleanup_sync(&docker, &container, &volume)).await;
        if let Some(process) = self.process.get() {
            let _ = process.child.lock().await.kill().await;
        }
        if let Some(mut workspace) = self.workspace.lock().await.take() {
            let _ = workspace.dispose();
        }
    }
}

impl Drop for DockerJob {
    fn drop(&mut self) {
        revoke(&self.leases);
    }
}

async fn write_message(stdin: &Mutex<ChildStdin>, message: &Value) -> Result<()> {
    let mut stdin = stdin.lock().await;
    let mut payload = serde_json::to_vec(message)?;
    payload.push(b'\n');
    stdin.write_all(&payload).await?;
    stdin.flush().await?;
    Ok(())
}

struct CreateContainer<'a> {
    docker: &'a Path,
    tar: &'a Path,
    image: &'a str,
    container: &'a str,
    volume: &'a str,
    job_id: &'a str,
    workspace: &'a Path,
    model: &'a str,
    thinking: &'a str,
    permissions: &'a str,
    limits: &'a DockerLimits,
    leases: &'a [Arc<dyn WorkerLease>],
}

fn create_container(input: CreateContainer<'_>) -> Result<()> {
    run(
        input.docker,
        &[
            "volume",
            "create",
            "--label",
            MANAGED_LABEL,
            "--label",
            &format!("pocket-agent.job-id={}", input.job_id),
            "--driver",
            "local",
            "--opt",
            "type=tmpfs",
            "--opt",
            "device=tmpfs",
            "--opt",
            &format!(
                "o=size={},uid=65532,gid=65532,mode=0700",
                input.limits.workspace_storage_bytes
            ),
            input.volume,
        ],
        1024 * 1024,
    )?;
    import_workspace(
        input.docker,
        input.tar,
        input.image,
        input.workspace,
        input.volume,
        &format!("{}-import", input.container),
    )?;
    let mut owned = vec![
        "create".into(),
        "--interactive".into(),
        "--name".into(),
        input.container.into(),
        "--hostname".into(),
        "pocket-agent-worker".into(),
        "--label".into(),
        MANAGED_LABEL.into(),
        "--label".into(),
        format!("pocket-agent.job-id={}", input.job_id),
        "--user".into(),
        "65532:65532".into(),
        "--read-only".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges=true".into(),
        "--network".into(),
        "none".into(),
        "--ipc".into(),
        "none".into(),
        "--pids-limit".into(),
        input.limits.pids.to_string(),
        "--memory".into(),
        input.limits.memory_bytes.to_string(),
        "--cpus".into(),
        input.limits.cpus.to_string(),
        "--ulimit".into(),
        format!("nofile={0}:{0}", input.limits.open_files),
        "--ulimit".into(),
        "core=0:0".into(),
        "--tmpfs".into(),
        format!(
            "/tmp:rw,nosuid,nodev,noexec,size={},mode=1777",
            input.limits.temporary_storage_bytes
        ),
        "--mount".into(),
        format!("type=volume,src={},dst=/workspace", input.volume),
        "--stop-timeout".into(),
        "1".into(),
        "--log-driver".into(),
        "none".into(),
        "--env".into(),
        format!("POCKET_AGENT_MODEL={}", input.model),
        "--env".into(),
        format!("POCKET_AGENT_THINKING={}", input.thinking),
        "--env".into(),
        format!("POCKET_AGENT_PERMISSIONS={}", input.permissions),
        "--env".into(),
        format!("POCKET_AGENT_JOB_ID={}", input.job_id),
    ];
    let mut destinations = BTreeMap::new();
    let mut environment = BTreeMap::new();
    for lease in input.leases {
        for mount in lease.mounts() {
            ensure!(
                mount.source.is_absolute() && mount.destination.is_absolute(),
                "Private mount paths must be absolute"
            );
            ensure!(
                destinations
                    .insert(mount.destination.clone(), mount.source.clone())
                    .is_none(),
                "Duplicate private mount destination"
            );
            owned.extend([
                "--mount".into(),
                format!(
                    "type=bind,src={},dst={},readonly",
                    mount.source.display(),
                    mount.destination.display()
                ),
            ]);
        }
        for (name, value) in lease.environment() {
            ensure!(
                environment.insert(name.clone(), value.clone()).is_none(),
                "Duplicate worker environment variable"
            );
            owned.extend(["--env".into(), format!("{name}={value}")]);
        }
    }
    owned.push(input.image.into());
    let references = owned.iter().map(String::as_str).collect::<Vec<_>>();
    run(input.docker, &references, 1024 * 1024)?;
    Ok(())
}

fn import_workspace(
    docker: &Path,
    tar: &Path,
    image: &str,
    workspace: &Path,
    volume: &str,
    name: &str,
) -> Result<()> {
    let mut archive = Command::new(tar)
        .args([
            "-C",
            workspace
                .to_str()
                .ok_or_else(|| anyhow!("Workspace path is not UTF-8"))?,
            "-c",
            "-f",
            "-",
            ".",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let importer = Command::new(docker)
        .args([
            "run",
            "--rm",
            "--interactive",
            "--name",
            name,
            "--user",
            "65532:65532",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges=true",
            "--network",
            "none",
            "--pids-limit",
            "64",
            "--memory",
            "134217728",
            "--cpus",
            "0.5",
            "--mount",
            &format!("type=volume,src={volume},dst=/workspace"),
            "--entrypoint",
            "tar",
            image,
            "-x",
            "-f",
            "-",
            "-C",
            "/workspace",
        ])
        .stdin(
            archive
                .stdout
                .take()
                .ok_or_else(|| anyhow!("Archive stdout unavailable"))?,
        )
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let archive_output = archive.wait_with_output()?;
    let importer_output = importer.wait_with_output()?;
    ensure!(
        archive_output.status.success(),
        "Workspace archive failed: {}",
        String::from_utf8_lossy(&archive_output.stderr)
    );
    ensure!(
        importer_output.status.success(),
        "Workspace import failed: {}",
        String::from_utf8_lossy(&importer_output.stderr)
    );
    Ok(())
}

fn export_workspace(
    docker: &Path,
    tar: &Path,
    image: &str,
    container: &str,
    volume: &str,
    destination: &Path,
) -> Result<()> {
    run(docker, &["pause", container], 1024 * 1024)?;
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("Workspace has no parent"))?;
    let staging = parent.join(format!(".rust-export-{}", Uuid::new_v4()));
    std::fs::create_dir(&staging)?;
    let result = (|| {
        let mut exporter = Command::new(docker)
            .args([
                "run",
                "--rm",
                "--user",
                "65532:65532",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges=true",
                "--network",
                "none",
                "--pids-limit",
                "64",
                "--memory",
                "134217728",
                "--cpus",
                "0.5",
                "--mount",
                &format!("type=volume,src={volume},dst=/workspace,readonly"),
                "--entrypoint",
                "tar",
                image,
                "-c",
                "-f",
                "-",
                "-C",
                "/workspace",
                ".",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let extractor = Command::new(tar)
            .args([
                "-x",
                "-f",
                "-",
                "-C",
                staging
                    .to_str()
                    .ok_or_else(|| anyhow!("Staging path is not UTF-8"))?,
                "--no-same-owner",
                "--no-same-permissions",
            ])
            .stdin(
                exporter
                    .stdout
                    .take()
                    .ok_or_else(|| anyhow!("Exporter stdout unavailable"))?,
            )
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let exporter_output = exporter.wait_with_output()?;
        let extractor_output = extractor.wait_with_output()?;
        ensure!(
            exporter_output.status.success(),
            "Workspace export failed: {}",
            String::from_utf8_lossy(&exporter_output.stderr)
        );
        ensure!(
            extractor_output.status.success(),
            "Workspace extraction failed: {}",
            String::from_utf8_lossy(&extractor_output.stderr)
        );
        let backup = parent.join(format!(".rust-export-old-{}", Uuid::new_v4()));
        std::fs::rename(destination, &backup)?;
        if let Err(error) = std::fs::rename(&staging, destination) {
            let _ = std::fs::rename(&backup, destination);
            return Err(error.into());
        }
        std::fs::remove_dir_all(backup)?;
        Ok(())
    })();
    let _ = run(docker, &["unpause", container], 1024 * 1024);
    if staging.exists() {
        let _ = std::fs::remove_dir_all(staging);
    }
    result
}

fn cleanup_sync(docker: &Path, container: &str, volume: &str) -> Result<()> {
    let _ = run(docker, &["rm", "--force", container], 1024 * 1024);
    let _ = run(docker, &["volume", "rm", "--force", volume], 1024 * 1024);
    Ok(())
}

fn run(program: &Path, arguments: &[&str], max_bytes: usize) -> Result<String> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {}", program.display()))?;
    let stderr = child.stderr.take().expect("piped stderr");
    let stderr_reader = std::thread::spawn(move || drain_bounded(stderr, 64 * 1024));
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut stdout)?;
    if stdout.len() > max_bytes {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("Command stderr reader failed"))?;
    ensure!(
        stdout.len() <= max_bytes,
        "Command output exceeded its limit"
    );
    if !status.success() {
        bail!(
            "{} {} failed: {}",
            program.display(),
            arguments.join(" "),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok(String::from_utf8(stdout)?.trim().to_owned())
}

fn drain_bounded(mut reader: impl Read, limit: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(read) = reader.read(&mut chunk) {
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(remaining)]);
    }
    kept
}

fn required_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Worker message is missing {key}"))
}
fn revoke(leases: &[Arc<dyn WorkerLease>]) {
    for lease in leases {
        lease.revoke();
    }
}
fn is_pinned_image(image: &str) -> bool {
    let digest = image
        .strip_prefix("sha256:")
        .or_else(|| image.rsplit_once("@sha256:").map(|(_, digest)| digest));
    digest.is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn deadline_timestamp(duration: Duration) -> Result<String> {
    let duration =
        time::Duration::try_from(duration).map_err(|_| anyhow!("Job timeout is too large"))?;
    Ok((time::OffsetDateTime::now_utc() + duration)
        .format(&time::format_description::well_known::Rfc3339)?)
}
