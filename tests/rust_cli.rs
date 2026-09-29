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
    let _ = fs::remove_dir_all(root);
}
