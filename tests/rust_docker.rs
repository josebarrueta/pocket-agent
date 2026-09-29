#![cfg(unix)]

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use pocket_agent::{
    capability::{CapabilityBroker, CapabilityLimits},
    docker::{DockerJobFactory, DockerJobFactoryConfig, DockerLimits},
    domain::{ApprovalRequest, JobSpec},
    model_proxy::{ModelDescriptor, ModelProxy, ModelProxyLimits},
    ports::{JobEventPort, JobFactory, WorkerAccessIssuer},
    workspace::{WorkspaceLimits, WorkspaceManager},
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Events;

#[async_trait]
impl JobEventPort for Events {
    async fn status(&self, _message: &str) -> Result<()> {
        Ok(())
    }
    async fn request_approval(&self, _request: ApprovalRequest) -> Result<String> {
        Ok("no".into())
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
