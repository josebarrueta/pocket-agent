#![cfg(target_os = "macos")]

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use pocket_agent::{
    capability::{CapabilityBroker, CapabilityLimits},
    domain::{ApprovalRequest, JobSpec},
    native::{NativeJobFactory, NativeJobFactoryConfig, NativeLimits},
    ports::{JobEventPort, JobFactory},
    workspace::{WorkspaceLimits, WorkspaceManager},
};

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
    let output = std::process::Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn node_path() -> Option<PathBuf> {
    [
        "/opt/homebrew/bin/node",
        "/usr/local/bin/node",
        "/usr/bin/node",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

#[tokio::test]
async fn native_macos_job_runs_in_seatbelt_and_exports_a_patch() -> Result<()> {
    let Some(node) = node_path() else {
        return Ok(());
    };
    let root = PathBuf::from("/tmp").join(format!(
        "pa-native-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    let source = root.join("source");
    std::fs::create_dir_all(&source)?;
    git(&source, &["init", "--quiet"]);
    git(&source, &["config", "user.name", "Test"]);
    git(&source, &["config", "user.email", "test@example.com"]);
    std::fs::write(source.join("input.txt"), "baseline\n")?;
    git(&source, &["add", "."]);
    git(&source, &["commit", "--quiet", "-m", "baseline"]);

    let secret = root.join("host-secret.txt");
    std::fs::write(&secret, "must not be readable")?;
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
    let factory = NativeJobFactory::new(NativeJobFactoryConfig {
        sandbox_exec: PathBuf::from("/usr/bin/sandbox-exec"),
        node,
        worker: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/docker-worker/worker.mjs"),
        model: "fake/fake".into(),
        thinking: "off".into(),
        permissions: r#"{"read":"allow","write":"allow","bash":"allow"}"#.into(),
        limits: NativeLimits {
            job_timeout: Duration::from_secs(30),
            output_bytes: 4096,
            ..NativeLimits::default()
        },
        workspaces,
        access: Some(broker.clone()),
    })?;
    let job = factory
        .create(JobSpec {
            job_id: format!("native-{}", std::process::id()),
            ingress_id: "test".into(),
            principal_id: "test".into(),
            conversation_id: "test".into(),
            repository: "app".into(),
        })
        .await?;

    let result = job.run_turn("first", Arc::new(Events)).await?;
    assert_eq!(result.output, "completed first");
    assert!(
        result
            .changed_files
            .iter()
            .any(|file| file.path == "worker.txt")
    );

    let result = job.run_turn("native-shell", Arc::new(Events)).await?;
    assert_eq!(result.output, "shell:0");
    assert!(
        result
            .changed_files
            .iter()
            .any(|file| file.path == "shell.txt")
    );

    let result = job
        .run_turn(
            &format!("probe-isolation:{}", secret.display()),
            Arc::new(Events),
        )
        .await?;
    let isolation: serde_json::Value = serde_json::from_str(&result.output)?;
    assert_eq!(isolation["hostFileAccessible"], false);
    assert_eq!(isolation["inheritedSecret"], serde_json::Value::Null);
    assert!(
        result
            .changed_files
            .iter()
            .any(|file| file.path == "isolation.json")
    );

    let probe = serde_json::json!({"paths": [secret], "hostPort": 9});
    use std::io::Write as _;
    let mut encoder = Vec::new();
    write!(&mut encoder, "{probe}")?;
    let encoded = base64_url(&encoder);
    let result = job
        .run_turn(&format!("probe-boundary:{encoded}"), Arc::new(Events))
        .await?;
    let boundary: serde_json::Value = serde_json::from_str(&result.output)?;
    assert_eq!(boundary["readablePaths"], serde_json::json!([]));
    assert_eq!(boundary["internetReachable"], false);

    let result = job.run_turn("broker-list", Arc::new(Events)).await?;
    assert_eq!(result.output, "completed broker-list");
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

fn base64_url(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    for chunk in input.chunks(3) {
        let value = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        output.push(ALPHABET[((value >> 18) & 63) as usize] as char);
        output.push(ALPHABET[((value >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[((value >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[(value & 63) as usize] as char);
        }
    }
    output
}
