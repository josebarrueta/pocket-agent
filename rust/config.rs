use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub signal: Option<SignalConfig>,
    #[serde(default)]
    pub repositories: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub connectors: ConnectorsConfig,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    pub sandbox: SandboxConfig,
    pub agent: AgentConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignalConfig {
    #[serde(default = "default_signal_url")]
    pub daemon_url: String,
    pub account: String,
    pub allowed_senders: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectorsConfig {
    #[serde(default)]
    pub arcade: Option<ArcadeConnectorConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArcadeConnectorConfig {
    pub gateway_slug: String,
    #[serde(default = "default_arcade_timeout")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_arcade_calls")]
    pub max_calls_per_job: u32,
    #[serde(default = "default_arcade_request")]
    pub max_request_bytes: usize,
    #[serde(default = "default_arcade_response")]
    pub max_response_bytes: usize,
    pub tools: Vec<ArcadeToolConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArcadeToolConfig {
    pub name: String,
    pub upstream_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(default = "default_allow")]
    pub policy: Decision,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxConfig {
    pub runner: SandboxKind,
    #[serde(default = "default_docker_path")]
    pub docker_path: PathBuf,
    pub image: Option<String>,
    pub node_path: Option<PathBuf>,
    pub worker_path: Option<PathBuf>,
    #[serde(default = "default_cpus")]
    pub cpus: f64,
    #[serde(default = "default_memory")]
    pub memory_bytes: u64,
    #[serde(default = "default_pids")]
    pub pids: u32,
    #[serde(default = "default_temporary_storage")]
    pub temporary_storage_bytes: u64,
    #[serde(default = "default_workspace_storage")]
    pub workspace_storage_bytes: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SandboxKind {
    Docker,
    Native,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentConfig {
    pub model: String,
    pub api_key_env: String,
    pub base_url: Option<String>,
    #[serde(default = "default_model_timeout")]
    pub model_request_timeout_ms: u64,
    #[serde(default = "default_model_rate")]
    pub model_max_requests_per_minute: u32,
    #[serde(default = "default_request_tokens")]
    pub model_max_tokens_per_request: u32,
    #[serde(default = "default_job_tokens")]
    pub model_max_tokens_per_job: u64,
    #[serde(default)]
    pub thinking: Thinking,
    #[serde(default)]
    pub permissions: Permissions,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    Off,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Permissions {
    pub read: Decision,
    pub write: Decision,
    pub bash: Decision,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            read: Decision::Allow,
            write: Decision::Ask,
            bash: Decision::Ask,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("read configuration {}", path.display()))?;
        let mut config: Self = serde_json::from_str(&contents).context("parse configuration")?;
        config.state_dir = expand_home(&config.state_dir)?;
        match config.sandbox.runner {
            SandboxKind::Docker => {
                ensure!(
                    config.sandbox.docker_path.is_absolute(),
                    "sandbox.dockerPath must be absolute"
                );
                ensure!(
                    config
                        .sandbox
                        .image
                        .as_deref()
                        .is_some_and(is_digest_pinned),
                    "sandbox.image must be pinned by a complete sha256 digest"
                );
            }
            SandboxKind::Native => {
                ensure!(cfg!(target_os = "macos"), "native sandbox requires macOS");
                ensure!(
                    config
                        .sandbox
                        .node_path
                        .as_ref()
                        .is_some_and(|path| path.is_absolute()),
                    "sandbox.nodePath must be absolute"
                );
                ensure!(
                    config
                        .sandbox
                        .worker_path
                        .as_ref()
                        .is_some_and(|path| path.is_absolute()),
                    "sandbox.workerPath must be absolute"
                );
            }
        }
        ensure!(
            config.sandbox.cpus.is_finite()
                && config.sandbox.cpus > 0.0
                && config.sandbox.cpus <= 64.0,
            "sandbox.cpus must be between 0 and 64"
        );
        ensure!(
            config.sandbox.pids > 0 && config.sandbox.pids <= 4096,
            "sandbox.pids must be between 1 and 4096"
        );
        for repository in config.repositories.values_mut() {
            *repository = expand_home(repository)?;
        }
        let (provider, model) = config
            .agent
            .model
            .split_once('/')
            .ok_or_else(|| anyhow!("agent.model must be provider/model-id"))?;
        ensure!(
            !provider.is_empty() && !model.is_empty(),
            "agent.model must be provider/model-id"
        );
        ensure!(
            valid_environment_name(&config.agent.api_key_env),
            "agent.apiKeyEnv must name an environment variable"
        );
        ensure!(
            (1_000..=30 * 60_000).contains(&config.agent.model_request_timeout_ms),
            "agent.modelRequestTimeoutMs is out of range"
        );
        ensure!(
            (1..=1_000).contains(&config.agent.model_max_requests_per_minute),
            "agent.modelMaxRequestsPerMinute is out of range"
        );
        if let Some(arcade) = &config.connectors.arcade {
            validate_arcade(arcade)?;
        }
        if let Some(signal) = &config.signal {
            ensure!(
                !signal.account.is_empty()
                    && !signal.allowed_senders.is_empty()
                    && signal
                        .allowed_senders
                        .iter()
                        .all(|sender| !sender.is_empty()),
                "Signal requires an account and allowed senders"
            );
            ensure!(
                signal.daemon_url.starts_with("http://")
                    || signal.daemon_url.starts_with("https://"),
                "signal.daemonUrl must be HTTP(S)"
            );
        }
        if let Some(url) = &config.agent.base_url {
            ensure!(
                url.starts_with("https://")
                    || url.starts_with("http://127.0.0.1")
                    || url.starts_with("http://localhost"),
                "agent.baseUrl must use HTTPS or loopback HTTP"
            );
        }
        Ok(config)
    }

    pub fn model_parts(&self) -> (&str, &str) {
        self.agent.model.split_once('/').expect("validated model")
    }
}

fn expand_home(path: &Path) -> Result<PathBuf> {
    let value = path.to_string_lossy();
    if value == "~" || value.starts_with("~/") {
        let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
        return Ok(PathBuf::from(home).join(value.trim_start_matches("~/")));
    }
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn is_digest_pinned(image: &str) -> bool {
    let digest = image
        .strip_prefix("sha256:")
        .or_else(|| image.rsplit_once("@sha256:").map(|(_, digest)| digest));
    digest.is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn validate_arcade(arcade: &ArcadeConnectorConfig) -> Result<()> {
    ensure!(
        !arcade.gateway_slug.is_empty()
            && arcade.gateway_slug.len() <= 128
            && arcade.gateway_slug.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
            }),
        "connectors.arcade.gatewaySlug is invalid"
    );
    ensure!(
        (1_000..=120_000).contains(&arcade.request_timeout_ms),
        "connectors.arcade.requestTimeoutMs is out of range"
    );
    ensure!(
        (1..=1_000).contains(&arcade.max_calls_per_job),
        "connectors.arcade.maxCallsPerJob is out of range"
    );
    ensure!(
        (1_024..=1024 * 1024).contains(&arcade.max_request_bytes),
        "connectors.arcade.maxRequestBytes is out of range"
    );
    ensure!(
        (1_024..=8 * 1024 * 1024).contains(&arcade.max_response_bytes),
        "connectors.arcade.maxResponseBytes is out of range"
    );
    ensure!(!arcade.tools.is_empty(), "connectors.arcade.tools is empty");
    let mut local = std::collections::BTreeSet::new();
    let mut upstream = std::collections::BTreeSet::new();
    for tool in &arcade.tools {
        ensure!(
            tool.name.starts_with("arcade.")
                && tool.name.len() <= 128
                && tool.name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_' || byte == b'-'
                }),
            "Arcade local tool name is invalid"
        );
        ensure!(
            !tool.upstream_name.is_empty() && tool.upstream_name.len() <= 256,
            "Arcade upstream tool name is invalid"
        );
        ensure!(
            !tool.description.is_empty() && tool.description.len() <= 2_048,
            "Arcade tool description is invalid"
        );
        ensure!(
            tool.input_schema
                .get("type")
                .and_then(|value| value.as_str())
                == Some("object")
                && tool
                    .input_schema
                    .get("additionalProperties")
                    .and_then(|value| value.as_bool())
                    == Some(false),
            "Arcade input schemas must be closed object schemas"
        );
        ensure!(
            local.insert(tool.name.clone()) && upstream.insert(tool.upstream_name.clone()),
            "Arcade tool names must be unique"
        );
    }
    Ok(())
}

fn default_state_dir() -> PathBuf {
    PathBuf::from("~/.local/share/pocket-agent")
}
fn default_signal_url() -> String {
    "http://127.0.0.1:8080".into()
}
fn default_docker_path() -> PathBuf {
    PathBuf::from("/usr/local/bin/docker")
}
fn default_cpus() -> f64 {
    1.0
}
fn default_memory() -> u64 {
    1_073_741_824
}
fn default_pids() -> u32 {
    256
}
fn default_temporary_storage() -> u64 {
    268_435_456
}
fn default_workspace_storage() -> u64 {
    805_306_368
}
fn default_model_timeout() -> u64 {
    120_000
}
fn default_model_rate() -> u32 {
    10
}
fn default_request_tokens() -> u32 {
    32_000
}
fn default_job_tokens() -> u64 {
    200_000
}
fn default_arcade_timeout() -> u64 {
    30_000
}
fn default_arcade_calls() -> u32 {
    30
}
fn default_arcade_request() -> usize {
    64 * 1024
}
fn default_arcade_response() -> usize {
    256 * 1024
}
fn default_allow() -> Decision {
    Decision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(signal: &str) -> String {
        format!(
            r#"{{{signal}"repositories":{{"app":"/tmp/app"}},"sandbox":{{"runner":"docker","dockerPath":"/usr/bin/docker","image":"worker@sha256:{}"}},"agent":{{"model":"anthropic/claude","apiKeyEnv":"ANTHROPIC_API_KEY"}}}}"#,
            "a".repeat(64)
        )
    }

    fn load(contents: &str) -> Result<Config> {
        let path =
            env::temp_dir().join(format!("pocket-agent-config-{}.json", uuid::Uuid::new_v4()));
        fs::write(&path, contents)?;
        let result = Config::load(&path);
        let _ = fs::remove_file(path);
        result
    }

    #[test]
    fn signal_is_optional_for_cli_ingress() {
        let config = load(&valid("")).unwrap();
        assert!(config.signal.is_none());
        assert_eq!(config.model_parts(), ("anthropic", "claude"));
    }

    #[test]
    fn signal_configuration_remains_supported() {
        let config = load(&valid(
            r#""signal":{"account":"+1555","allowedSenders":["+1556"]},"#,
        ))
        .unwrap();
        assert_eq!(config.signal.unwrap().account, "+1555");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_macos_configuration_does_not_require_docker_or_an_image() {
        let config = load(
            r#"{"repositories":{"app":"/tmp/app"},"sandbox":{"runner":"native","nodePath":"/opt/homebrew/bin/node","workerPath":"/opt/pocket-agent/worker.mjs"},"agent":{"model":"anthropic/claude","apiKeyEnv":"ANTHROPIC_API_KEY"}}"#,
        )
        .unwrap();
        assert_eq!(config.sandbox.runner, SandboxKind::Native);
        assert!(config.sandbox.image.is_none());
        assert!(
            load(
                r#"{"repositories":{"app":"/tmp/app"},"sandbox":{"runner":"native"},"agent":{"model":"anthropic/claude","apiKeyEnv":"ANTHROPIC_API_KEY"}}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn validates_slug_only_curated_arcade_configuration() {
        let arcade = r#""connectors":{"arcade":{"gatewaySlug":"dev-gateway","tools":[{"name":"arcade.github_get_issue","upstreamName":"GitHub.GetIssue","description":"Read one issue.","inputSchema":{"type":"object","properties":{"number":{"type":"integer","minimum":1}},"required":["number"],"additionalProperties":false},"policy":"allow"}]}},"#;
        let configured =
            valid("").replace("\"repositories\":", &format!("{arcade}\"repositories\":"));
        let config = load(&configured).unwrap();
        let arcade = config.connectors.arcade.unwrap();
        assert_eq!(arcade.gateway_slug, "dev-gateway");
        assert_eq!(arcade.tools[0].name, "arcade.github_get_issue");

        assert!(load(&configured.replace("dev-gateway", "https://evil.test/mcp")).is_err());
        assert!(
            load(&configured.replace(
                "\"additionalProperties\":false",
                "\"additionalProperties\":true"
            ))
            .is_err()
        );
    }

    #[test]
    fn rejects_unpinned_images_and_unsafe_model_configuration() {
        assert!(
            load(&valid("").replace(
                &format!("worker@sha256:{}", "a".repeat(64)),
                "worker:latest"
            ))
            .is_err()
        );
        assert!(load(&valid("").replace("anthropic/claude", "claude")).is_err());
        assert!(
            load(&valid("").replace(
                &format!("worker@sha256:{}", "a".repeat(64)),
                &format!("sha256:{}", "b".repeat(64))
            ))
            .is_ok()
        );
        assert!(load(&valid("").replace("ANTHROPIC_API_KEY", "bad-key")).is_err());
    }
}
