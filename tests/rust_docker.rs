#![cfg(unix)]

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use pocket_agent::{
    capability::{CapabilityBroker, CapabilityLimits},
    docker::{DockerJobFactory, DockerJobFactoryConfig, DockerLimits},
    domain::{ApprovalRequest, AuthorizationRequest, JobSpec},
    model_proxy::{ModelDescriptor, ModelProxy, ModelProxyLimits},
    ports::{JobEventPort, JobFactory, PrivateMount, WorkerAccessIssuer, WorkerLease},
    workspace::{WorkspaceLimits, WorkspaceManager},
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Events;

struct MissingBrokerIssuer(PathBuf);
struct MissingBrokerLease(PathBuf);

#[async_trait]
impl WorkerAccessIssuer for MissingBrokerIssuer {
    async fn issue(&self, _spec: &JobSpec) -> Result<Vec<Arc<dyn WorkerLease>>> {
        Ok(vec![Arc::new(MissingBrokerLease(self.0.clone()))])
    }
}

impl WorkerLease for MissingBrokerLease {
    fn environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "POCKET_AGENT_MCP_SOCKET".into(),
                "/run/pocket-agent-broker/missing.sock".into(),
            ),
            ("POCKET_AGENT_MCP_CREDENTIAL".into(), "unusable".into()),
        ])
    }

    fn mounts(&self) -> Vec<PrivateMount> {
        vec![PrivateMount {
            source: self.0.clone(),
            destination: PathBuf::from("/run/pocket-agent-broker"),
        }]
    }

    fn revoke(&self) {}
}

fn docker_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[async_trait]
impl JobEventPort for Events {
    async fn status(&self, _message: &str) -> Result<()> {
        Ok(())
    }
    async fn request_approval(&self, _request: ApprovalRequest) -> Result<String> {
        Ok("no".into())
    }
    async fn authorization_required(&self, _request: AuthorizationRequest) -> Result<()> {
        Ok(())
    }
}

fn git(directory: &std::path::Path, arguments: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .status()
            .unwrap()
            .success()
    );
}

fn docker(path: &std::path::Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new(path)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[tokio::test]
async fn rust_docker_job_runs_the_worker_and_exports_a_patch() -> Result<()> {
    let _guard = docker_test_lock().lock().await;
    let Some(image) = std::env::var_os("POCKET_AGENT_DOCKER_FIXTURE_IMAGE") else {
        return Ok(());
    };
    let root =
        std::env::temp_dir().join(format!("pocket-agent-rust-docker-{}", uuid::Uuid::new_v4()));
    let source = root.join("source");
    std::fs::create_dir_all(&source)?;
    git(&source, &["init", "--quiet"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "user.email", "test@example.com"]);
    std::fs::write(source.join("input.txt"), "baseline\n")?;
    git(&source, &["add", "."]);
    git(&source, &["commit", "--quiet", "-m", "baseline"]);

    let workspaces = WorkspaceManager::new(
        root.join("workspaces"),
        BTreeMap::from([("app".into(), source)]),
        WorkspaceLimits::default(),
    )?;
    let docker_path = PathBuf::from(
        std::env::var_os("POCKET_AGENT_DOCKER_PATH").unwrap_or_else(|| "/usr/bin/docker".into()),
    );
    let factory = DockerJobFactory::new(DockerJobFactoryConfig {
        docker: docker_path.clone(),
        tar: PathBuf::from(
            std::env::var_os("POCKET_AGENT_TAR_PATH").unwrap_or_else(|| "/usr/bin/tar".into()),
        ),
        image: image.to_string_lossy().into_owned(),
        model: "fake/fake".into(),
        thinking: "off".into(),
        permissions: r#"{"read":"allow","write":"allow","bash":"allow"}"#.into(),
        limits: DockerLimits {
            job_timeout: Duration::from_secs(30),
            output_bytes: 1024,
            ..DockerLimits::default()
        },
        workspaces,
        access: None,
        allow_unpinned_image_for_tests: true,
    })?;
    factory.reconcile().await?;
    let job = factory
        .create(JobSpec {
            job_id: format!("rust-{}", std::process::id()),
            ingress_id: "test".into(),
            principal_id: "test".into(),
            conversation_id: "test".into(),
            repository: "app".into(),
        })
        .await?;

    let label = format!("label=pocket-agent.job-id=rust-{}", std::process::id());
    let id = docker(
        &docker_path,
        &["ps", "--all", "--quiet", "--filter", &label],
    );
    let inspected: serde_json::Value =
        serde_json::from_str(&docker(&docker_path, &["inspect", &id]))?;
    let inspected = &inspected[0];
    assert_eq!(inspected["Config"]["User"], "65532:65532");
    assert_eq!(inspected["HostConfig"]["ReadonlyRootfs"], true);
    assert_eq!(inspected["HostConfig"]["NetworkMode"], "none");
    assert_eq!(
        inspected["HostConfig"]["CapDrop"],
        serde_json::json!(["ALL"])
    );
    assert_eq!(inspected["HostConfig"]["LogConfig"]["Type"], "none");
    assert_eq!(inspected["Mounts"].as_array().unwrap().len(), 1);
    assert_eq!(inspected["Mounts"][0]["Type"], "volume");
    assert_eq!(inspected["Mounts"][0]["Destination"], "/workspace");

    let result = job.run_turn("first", Arc::new(Events)).await?;
    assert_eq!(result.output, "completed first");
    assert!(
        result
            .changed_files
            .iter()
            .any(|file| file.path == "worker.txt")
    );
    assert!(job.run_turn("huge-output", Arc::new(Events)).await.is_err());
    assert!(
        docker(
            &docker_path,
            &["ps", "--all", "--quiet", "--filter", &label]
        )
        .is_empty()
    );
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[tokio::test]
async fn rust_worker_reaches_only_its_scoped_capability_broker() -> Result<()> {
    let _guard = docker_test_lock().lock().await;
    if cfg!(target_os = "macos") {
        return Ok(());
    }
    let Some(image) = std::env::var_os("POCKET_AGENT_DOCKER_FIXTURE_IMAGE") else {
        return Ok(());
    };
    let root = PathBuf::from("/tmp").join(format!("pa-rust-broker-{}", uuid::Uuid::new_v4()));
    let source = root.join("source");
    std::fs::create_dir_all(&source)?;
    git(&source, &["init", "--quiet"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "user.email", "test@example.com"]);
    std::fs::write(source.join("input.txt"), "baseline\n")?;
    git(&source, &["add", "."]);
    git(&source, &["commit", "--quiet", "-m", "baseline"]);
    let workspaces = WorkspaceManager::new(
        root.join("workspaces"),
        BTreeMap::from([("app".into(), source)]),
        WorkspaceLimits::default(),
    )?;
    let broker = CapabilityBroker::new(
        root.join("broker/broker.sock"),
        root.join("audit/capabilities.ndjson"),
        workspaces.clone(),
        CapabilityLimits::default(),
    )?;
    broker.start().await?;
    let access: Arc<dyn WorkerAccessIssuer> = broker.clone();
    let docker_path = PathBuf::from(
        std::env::var_os("POCKET_AGENT_DOCKER_PATH").unwrap_or_else(|| "/usr/bin/docker".into()),
    );
    let factory = DockerJobFactory::new(DockerJobFactoryConfig {
        docker: docker_path,
        tar: PathBuf::from(
            std::env::var_os("POCKET_AGENT_TAR_PATH").unwrap_or_else(|| "/usr/bin/tar".into()),
        ),
        image: image.to_string_lossy().into_owned(),
        model: "fake/fake".into(),
        thinking: "off".into(),
        permissions: r#"{"read":"allow","write":"allow","bash":"allow"}"#.into(),
        limits: DockerLimits {
            job_timeout: Duration::from_secs(30),
            ..DockerLimits::default()
        },
        workspaces,
        access: Some(access),
        allow_unpinned_image_for_tests: true,
    })?;
    let job = factory
        .create(JobSpec {
            job_id: format!("rust-broker-{}", std::process::id()),
            ingress_id: "test".into(),
            principal_id: "test".into(),
            conversation_id: "test".into(),
            repository: "app".into(),
        })
        .await?;
    let result = job.run_turn("broker-list", Arc::new(Events)).await?;
    assert!(
        result
            .changed_files
            .iter()
            .any(|file| file.path == "broker.json")
    );
    job.cancel().await?;
    broker.close().await;
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[tokio::test]
async fn rust_worker_reaches_the_host_model_proxy_without_provider_credentials() -> Result<()> {
    let _guard = docker_test_lock().lock().await;
    if cfg!(target_os = "macos") {
        return Ok(());
    }
    let Some(image) = std::env::var_os("POCKET_AGENT_DOCKER_TEST_IMAGE") else {
        return Ok(());
    };
    let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider_address = provider.local_addr()?;
    tokio::spawn(async move {
        let (mut stream, _) = provider.accept().await.unwrap();
        let mut request = Vec::new();
        let mut expected = None;
        loop {
            let mut chunk = [0u8; 8192];
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if expected.is_none()
                && let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let header = String::from_utf8_lossy(&request[..end]);
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                expected = Some(end + 4 + length);
            }
            if expected.is_some_and(|length| request.len() >= length) {
                break;
            }
        }
        let body = json!({
            "choices": [{ "message": { "content": "proxy works" }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 2, "completion_tokens": 2 }
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });

    let root = std::env::temp_dir().join(format!(
        "pocket-agent-rust-model-docker-{}",
        uuid::Uuid::new_v4()
    ));
    let source = root.join("source");
    std::fs::create_dir_all(&source)?;
    git(&source, &["init", "--quiet"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "user.email", "test@example.com"]);
    std::fs::write(source.join("input.txt"), "baseline\n")?;
    git(&source, &["add", "."]);
    git(&source, &["commit", "--quiet", "-m", "baseline"]);
    let proxy = ModelProxy::new(
        root.join("model/model.sock"),
        root.join("audit/models.ndjson"),
        ModelDescriptor {
            provider: "openai".into(),
            model: "fake".into(),
            name: "Fake".into(),
            context_window: 4096,
            max_tokens: 256,
            images: false,
        },
        "provider-key-never-enters-worker".into(),
        Some(format!("http://{provider_address}/v1")),
        ModelProxyLimits {
            lease_lifetime: Duration::from_secs(60),
            request_timeout: Duration::from_secs(10),
            requests_per_minute: 10,
            tokens_per_request: 256,
            tokens_per_job: 1024,
        },
    )?;
    proxy.start().await?;
    let access: Arc<dyn WorkerAccessIssuer> = proxy.clone();
    let workspaces = WorkspaceManager::new(
        root.join("workspaces"),
        BTreeMap::from([("app".into(), source)]),
        WorkspaceLimits::default(),
    )?;
    let docker_path = PathBuf::from(
        std::env::var_os("POCKET_AGENT_DOCKER_PATH").unwrap_or_else(|| "/usr/bin/docker".into()),
    );
    let factory = DockerJobFactory::new(DockerJobFactoryConfig {
        docker: docker_path.clone(),
        tar: PathBuf::from(
            std::env::var_os("POCKET_AGENT_TAR_PATH").unwrap_or_else(|| "/usr/bin/tar".into()),
        ),
        image: image.to_string_lossy().into_owned(),
        model: "openai/fake".into(),
        thinking: "off".into(),
        permissions: r#"{"read":"allow","write":"deny","bash":"deny"}"#.into(),
        limits: DockerLimits {
            job_timeout: Duration::from_secs(30),
            ..DockerLimits::default()
        },
        workspaces,
        access: Some(access),
        allow_unpinned_image_for_tests: true,
    })?;
    factory.reconcile().await?;
    let job_id = format!("rust-model-{}", std::process::id());
    let job = factory
        .create(JobSpec {
            job_id: job_id.clone(),
            ingress_id: "test".into(),
            principal_id: "test".into(),
            conversation_id: "test".into(),
            repository: "app".into(),
        })
        .await?;
    let container = docker(
        &docker_path,
        &[
            "ps",
            "--all",
            "--quiet",
            "--filter",
            &format!("label=pocket-agent.job-id={job_id}"),
        ],
    );
    let inspected = docker(&docker_path, &["inspect", &container]);
    assert!(!inspected.contains("provider-key-never-enters-worker"));
    let result = job.run_turn("reply", Arc::new(Events)).await?;
    assert_eq!(result.output, "proxy works");
    job.cancel().await?;
    proxy.close().await;
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

fn fixture_factory(
    root: &std::path::Path,
    image: &std::ffi::OsStr,
    limits: DockerLimits,
) -> Result<(DockerJobFactory, PathBuf)> {
    fixture_factory_with_access(root, image, limits, None)
}

fn fixture_factory_with_access(
    root: &std::path::Path,
    image: &std::ffi::OsStr,
    limits: DockerLimits,
    access: Option<Arc<dyn WorkerAccessIssuer>>,
) -> Result<(DockerJobFactory, PathBuf)> {
    let source = root.join("source");
    std::fs::create_dir_all(&source)?;
    git(&source, &["init", "--quiet"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "user.email", "test@example.com"]);
    std::fs::write(source.join("input.txt"), "baseline\n")?;
    git(&source, &["add", "."]);
    git(&source, &["commit", "--quiet", "-m", "baseline"]);
    let workspaces = WorkspaceManager::new(
        root.join("workspaces"),
        BTreeMap::from([("app".into(), source)]),
        WorkspaceLimits::default(),
    )?;
    let docker_path = PathBuf::from(
        std::env::var_os("POCKET_AGENT_DOCKER_PATH").unwrap_or_else(|| "/usr/bin/docker".into()),
    );
    Ok((
        DockerJobFactory::new(DockerJobFactoryConfig {
            docker: docker_path.clone(),
            tar: PathBuf::from(
                std::env::var_os("POCKET_AGENT_TAR_PATH").unwrap_or_else(|| "/usr/bin/tar".into()),
            ),
            image: image.to_string_lossy().into_owned(),
            model: "fake/fake".into(),
            thinking: "off".into(),
            permissions: r#"{"read":"allow","write":"allow","bash":"allow"}"#.into(),
            limits,
            workspaces,
            access,
            allow_unpinned_image_for_tests: true,
        })?,
        docker_path,
    ))
}

async fn fixture_job(
    factory: &DockerJobFactory,
    prefix: &str,
) -> Result<Arc<dyn pocket_agent::ports::JobHandle>> {
    factory
        .create(JobSpec {
            job_id: format!("{prefix}-{}", uuid::Uuid::new_v4().simple()),
            ingress_id: "test".into(),
            principal_id: "test".into(),
            conversation_id: "test".into(),
            repository: "app".into(),
        })
        .await
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let value = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[((value >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            output.push(TABLE[(value & 63) as usize] as char);
        }
    }
    output
}

async fn wait_for_missing(docker_path: &std::path::Path, kind: &str, id: &str) {
    for _ in 0..50 {
        if !std::process::Command::new(docker_path)
            .args([kind, "inspect", id])
            .output()
            .unwrap()
            .status
            .success()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("Docker object {id} was not reclaimed");
}

#[tokio::test]
async fn real_worker_fails_closed_when_private_broker_is_unavailable() -> Result<()> {
    let _guard = docker_test_lock().lock().await;
    if cfg!(target_os = "macos") {
        return Ok(());
    }
    let Some(image) = std::env::var_os("POCKET_AGENT_DOCKER_TEST_IMAGE") else {
        return Ok(());
    };
    let root = PathBuf::from("/tmp").join(format!("pa-missing-broker-{}", uuid::Uuid::new_v4()));
    let socket_directory = root.join("broker");
    std::fs::create_dir_all(&socket_directory)?;
    let access: Arc<dyn WorkerAccessIssuer> = Arc::new(MissingBrokerIssuer(socket_directory));
    let (factory, _) = fixture_factory_with_access(
        &root,
        &image,
        DockerLimits {
            job_timeout: Duration::from_secs(30),
            ..DockerLimits::default()
        },
        Some(access),
    )?;
    let job = fixture_job(&factory, "missing-broker").await?;
    let error = job
        .run_turn("initialize capabilities", Arc::new(Events))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Worker extension initialization failed")
    );
    assert!(error.to_string().contains("missing.sock"));
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[tokio::test]
async fn rust_worker_cannot_reach_host_files_ports_internet_secrets_or_docker() -> Result<()> {
    let _guard = docker_test_lock().lock().await;
    let Some(image) = std::env::var_os("POCKET_AGENT_DOCKER_FIXTURE_IMAGE") else {
        return Ok(());
    };
    let root = PathBuf::from("/tmp").join(format!("pa-boundary-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root)?;
    let sibling = root.join("sibling-secret");
    let signal_key = root.join("signal-key");
    let credential = root.join("credential-store");
    std::fs::write(&sibling, "sibling-only")?;
    std::fs::write(&signal_key, "signal-only")?;
    std::fs::write(&credential, "credential-only")?;
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await?;
    let port = listener.local_addr()?.port();
    let listener_task = tokio::spawn(async move {
        let _ = listener.accept().await;
    });
    let (factory, _) = fixture_factory(&root, &image, DockerLimits::default())?;
    let job = fixture_job(&factory, "boundary").await?;
    let payload = serde_json::to_vec(&json!({
        "paths": [sibling, signal_key, credential, "/var/run/docker.sock"],
        "hostPort": port,
    }))?;
    let result = job
        .run_turn(
            &format!("probe-boundary:{}", base64url(&payload)),
            Arc::new(Events),
        )
        .await?;
    let boundary: serde_json::Value = serde_json::from_str(&result.output)?;
    assert_eq!(boundary["readablePaths"], json!([]));
    assert_eq!(boundary["inheritedSecrets"], json!([]));
    assert_eq!(boundary["internetReachable"], false);
    assert_eq!(boundary["hostPortReachable"], false);
    job.cancel().await?;
    listener_task.abort();
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[tokio::test]
async fn rust_docker_bounds_crashes_deadlines_and_ignored_cancellation() -> Result<()> {
    let _guard = docker_test_lock().lock().await;
    let Some(image) = std::env::var_os("POCKET_AGENT_DOCKER_FIXTURE_IMAGE") else {
        return Ok(());
    };
    let root = PathBuf::from("/tmp").join(format!("pa-lifecycle-{}", uuid::Uuid::new_v4()));
    let (factory, docker_path) = fixture_factory(
        &root,
        &image,
        DockerLimits {
            job_timeout: Duration::from_secs(1),
            ..DockerLimits::default()
        },
    )?;
    let crashing = fixture_job(&factory, "crash").await?;
    assert!(crashing.run_turn("crash", Arc::new(Events)).await.is_err());

    let hanging = fixture_job(&factory, "timeout").await?;
    assert!(hanging.run_turn("hang", Arc::new(Events)).await.is_err());

    let cancelled = fixture_job(&factory, "cancel").await?;
    let running = cancelled.clone();
    let turn =
        tokio::spawn(async move { running.run_turn("ignore-cancel", Arc::new(Events)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancelled.cancel().await?;
    assert!(turn.await?.is_err());
    assert!(
        docker(
            &docker_path,
            &[
                "ps",
                "--all",
                "--quiet",
                "--filter",
                "label=pocket-agent.managed=true"
            ]
        )
        .is_empty()
    );
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[tokio::test]
async fn rust_docker_contains_storage_process_and_memory_pressure() -> Result<()> {
    let _guard = docker_test_lock().lock().await;
    let Some(image) = std::env::var_os("POCKET_AGENT_DOCKER_FIXTURE_IMAGE") else {
        return Ok(());
    };
    let root = PathBuf::from("/tmp").join(format!("pa-pressure-{}", uuid::Uuid::new_v4()));
    let (storage_factory, _) = fixture_factory(
        &root.join("storage"),
        &image,
        DockerLimits {
            temporary_storage_bytes: 8 * 1024 * 1024,
            workspace_storage_bytes: 16 * 1024 * 1024,
            job_timeout: Duration::from_secs(30),
            ..DockerLimits::default()
        },
    )?;
    let disk = fixture_job(&storage_factory, "disk").await?;
    let output = disk.run_turn("disk-pressure", Arc::new(Events)).await?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output.output)?["bounded"],
        true
    );
    disk.cancel().await?;

    let (pressure_factory, _) = fixture_factory(
        &root.join("kernel"),
        &image,
        DockerLimits {
            pids: 32,
            memory_bytes: 128 * 1024 * 1024,
            job_timeout: Duration::from_secs(30),
            ..DockerLimits::default()
        },
    )?;
    let forked = fixture_job(&pressure_factory, "fork").await?;
    assert!(
        forked
            .run_turn("fork-pressure", Arc::new(Events))
            .await
            .is_err()
    );
    let memory = fixture_job(&pressure_factory, "memory").await?;
    assert!(
        memory
            .run_turn("memory-pressure", Arc::new(Events))
            .await
            .is_err()
    );
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[tokio::test]
async fn rust_docker_reconciles_labeled_orphans() -> Result<()> {
    let _guard = docker_test_lock().lock().await;
    let Some(image) = std::env::var_os("POCKET_AGENT_DOCKER_TEST_IMAGE") else {
        return Ok(());
    };
    let root = PathBuf::from("/tmp").join(format!("pa-reconcile-{}", uuid::Uuid::new_v4()));
    let (factory, docker_path) = fixture_factory(&root, &image, DockerLimits::default())?;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let volume = format!("pocket-agent-rust-orphan-{suffix}");
    let container = format!("pocket-agent-rust-orphan-{suffix}");
    docker(
        &docker_path,
        &[
            "volume",
            "create",
            "--label",
            "pocket-agent.managed=true",
            &volume,
        ],
    );
    docker(
        &docker_path,
        &[
            "create",
            "--name",
            &container,
            "--label",
            "pocket-agent.managed=true",
            "--mount",
            &format!("type=volume,src={volume},dst=/workspace"),
            &image.to_string_lossy(),
            "--smoke-test",
        ],
    );
    factory.reconcile().await?;
    wait_for_missing(&docker_path, "container", &container).await;
    wait_for_missing(&docker_path, "volume", &volume).await;
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}
