use std::{fs, process::Command};

fn config() -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("pocket-agent-rust-cli-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    fs::write(
        &path,
        format!(
            r#"{{"repositories":{{"app":"{}"}},"stateDir":"{}","sandbox":{{"runner":"docker","dockerPath":"/usr/bin/docker","image":"worker@sha256:{}"}},"agent":{{"model":"anthropic/model","apiKeyEnv":"POCKET_AGENT_TEST_DEFINITELY_MISSING_KEY"}}}}"#,
            root.display(),
            root.join("state").display(),
            "a".repeat(64)
        ),
    )
    .unwrap();
    (root, path)
}

#[cfg(target_os = "macos")]
#[test]
fn native_runner_is_rejected_for_remote_ingress() {
    let Some(node) = [
        "/opt/homebrew/bin/node",
        "/usr/local/bin/node",
        "/usr/bin/node",
    ]
    .into_iter()
    .find(|path| std::path::Path::new(path).is_file()) else {
        return;
    };
    let root =
        std::env::temp_dir().join(format!("pocket-agent-native-cli-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    let worker = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/docker-worker/worker.mjs");
    fs::write(
        &path,
        format!(
            r#"{{"signal":{{"account":"+1555","allowedSenders":["+1556"]}},"repositories":{{"app":"{}"}},"stateDir":"{}","sandbox":{{"runner":"native","nodePath":"{node}","workerPath":"{}"}},"agent":{{"model":"anthropic/model","apiKeyEnv":"POCKET_AGENT_TEST_DEFINITELY_MISSING_KEY"}}}}"#,
            root.display(),
            root.join("state").display(),
            worker.display()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pocket-agent"))
        .args(["--config", path.to_str().unwrap(), "serve", "signal"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("local-interactive-only"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn signal_configuration_is_required_only_for_signal_ingress() {
    let (root, config) = config();
    let binary = env!("CARGO_BIN_EXE_pocket-agent");
    let signal = Command::new(binary)
        .args(["--config", config.to_str().unwrap(), "serve", "signal"])
        .output()
        .unwrap();
    let signal_error = String::from_utf8_lossy(&signal.stderr);
    assert!(!signal.status.success());
    assert!(signal_error.contains("Signal configuration is required"));
    assert!(!signal_error.contains("credential"));

    let cli = Command::new(binary)
        .args([
            "--config",
            config.to_str().unwrap(),
            "run",
            "--repo",
            "app",
            "--prompt",
            "test",
        ])
        .output()
        .unwrap();
    let cli_error = String::from_utf8_lossy(&cli.stderr);
    assert!(!cli.status.success());
    assert!(cli_error.contains("Configured model credential"));
    assert!(!cli_error.contains("Signal configuration is required"));

    let cwd_cli = Command::new(binary)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "run",
            "--prompt",
            "test",
        ])
        .output()
        .unwrap();
    let cwd_error = String::from_utf8_lossy(&cwd_cli.stderr);
    assert!(!cwd_cli.status.success());
    assert!(cwd_error.contains("Configured model credential"));
    assert!(!cwd_error.contains("--repo"));
    let _ = fs::remove_dir_all(root);
}
