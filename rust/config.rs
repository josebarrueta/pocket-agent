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
    pub repositories: BTreeMap<String, PathBuf>,
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

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxConfig {
    pub runner: SandboxKind,
    #[serde(default = "default_docker_path")]
    pub docker_path: PathBuf,
    pub image: String,
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
        ensure!(
            !config.repositories.is_empty(),
            "at least one repository is required"
        );
        config.state_dir = expand_home(&config.state_dir)?;
        ensure!(
            config.sandbox.docker_path.is_absolute(),
            "sandbox.dockerPath must be absolute"
        );
        ensure!(
            is_digest_pinned(&config.sandbox.image),
            "sandbox.image must be pinned by a complete sha256 digest"
        );
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
