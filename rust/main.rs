use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use clap::Parser;
use pocket_agent::{
    arcade::{ArcadeGatewayProvider, SystemSecretStore},
    capability::{CapabilityBroker, CapabilityLimits, CapabilityProvider},
    cli::{ArcadeCommand, Cli, CliCommand, CliIngress, ServeCommand, Terminal},
    config::{Config, SandboxKind},
    docker::{DockerJobFactory, DockerJobFactoryConfig, DockerLimits},
    harness::Harness,
    model_proxy::{ModelDescriptor, ModelProxy, ModelProxyLimits},
    native::{NativeJobFactory, NativeJobFactoryConfig, NativeLimits},
    ports::{CombinedAccessIssuers, JobFactory, WorkerAccessIssuer},
    signal::SignalIngress,
    workspace::{WorkspaceLimits, WorkspaceManager},
};

struct StandardTerminal;

#[async_trait]
impl Terminal for StandardTerminal {
    async fn write(&self, text: &str) -> Result<()> {
        println!("{text}");
        Ok(())
    }

    async fn read(&self, prompt: &str) -> Result<Option<String>> {
        let prompt = prompt.to_owned();
        tokio::task::spawn_blocking(move || {
            print!("{prompt}");
            io::stdout().flush()?;
            let mut line = String::new();
            if io::stdin().read_line(&mut line)? == 0 {
                return Ok(None);
            }
            Ok(Some(line.trim_end().to_owned()))
        })
        .await?
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut cli = Cli::parse();
    let mut config = Config::load(&cli.config)?;
    prepare_local_repository(&mut cli, &mut config)?;
    if matches!(
        &cli.command,
        CliCommand::Serve {
            ingress: ServeCommand::Signal
        }
    ) {
        if config.signal.is_none() {
            return Err(anyhow!(
                "Signal configuration is required for 'serve signal'"
            ));
        }
        if config.sandbox.runner == SandboxKind::Native {
            return Err(anyhow!(
                "the native macOS sandbox is local-interactive-only; use the Docker runner for remote ingress"
            ));
        }
    }
    if matches!(
        &cli.command,
        CliCommand::Arcade {
            action: ArcadeCommand::Logout
        }
    ) {
        let arcade = config
            .connectors
            .arcade
            .clone()
            .ok_or_else(|| anyhow!("Arcade connector is not configured"))?;
        ArcadeGatewayProvider::new(arcade, SystemSecretStore::new()?)?
            .disconnect("cli", &local_principal())?;
        println!("Deleted the local Arcade gateway authorization");
        return Ok(());
    }
    if config.repositories.is_empty() {
        return Err(anyhow!(
            "at least one repository is required; local run and shell commands may omit --repo to use the current Git worktree"
        ));
    }
    let api_key = std::env::var(&config.agent.api_key_env).with_context(|| {
        format!(
            "Configured model credential {} is not set",
            config.agent.api_key_env
        )
    })?;
    let (provider, model) = config.model_parts();
    let descriptor = ModelDescriptor {
        provider: provider.to_owned(),
        model: model.to_owned(),
        name: model.to_owned(),
        context_window: 200_000,
        max_tokens: config.agent.model_max_tokens_per_request,
        images: true,
    };
    let proxy = ModelProxy::new(
        config.state_dir.join("model-proxy/model.sock"),
        config.state_dir.join("audit/models.ndjson"),
        descriptor,
        api_key,
        config.agent.base_url.clone(),
        ModelProxyLimits {
            lease_lifetime: Duration::from_secs(60 * 60),
            request_timeout: Duration::from_millis(config.agent.model_request_timeout_ms),
            requests_per_minute: config.agent.model_max_requests_per_minute as usize,
            tokens_per_request: config.agent.model_max_tokens_per_request,
            tokens_per_job: config.agent.model_max_tokens_per_job,
        },
    )?;
    proxy.start().await?;

    let result = run_with_proxy(cli, config, proxy.clone()).await;
    proxy.close().await;
    result
}

async fn run_with_proxy(cli: Cli, config: Config, proxy: Arc<ModelProxy>) -> Result<()> {
    let workspaces = WorkspaceManager::new(
        config.state_dir.join("workspaces"),
        config.repositories.clone(),
        WorkspaceLimits::default(),
    )?;
    workspaces.reclaim_all()?;
    let mut providers: Vec<Arc<dyn CapabilityProvider>> = Vec::new();
    if let Some(arcade) = config.connectors.arcade.clone() {
        providers.push(ArcadeGatewayProvider::new(
            arcade,
            SystemSecretStore::new()?,
        )?);
    }
    let capabilities = CapabilityBroker::with_providers(
        config.state_dir.join("capability-broker/broker.sock"),
        config.state_dir.join("audit/capabilities.ndjson"),
        workspaces.clone(),
        CapabilityLimits::default(),
        providers,
    )?;
    capabilities.start().await?;
    let result = async {
        let access: Arc<dyn WorkerAccessIssuer> =
            Arc::new(CombinedAccessIssuers(vec![proxy, capabilities.clone()]));
        let permissions = serde_json::to_string(&config.agent.permissions)?;
        let thinking = serde_json::to_value(config.agent.thinking)?
            .as_str()
            .unwrap_or("medium")
            .to_owned();
        let job_factory: Arc<dyn JobFactory> = match config.sandbox.runner {
            SandboxKind::Docker => {
                let factory = Arc::new(DockerJobFactory::new(DockerJobFactoryConfig {
                    docker: config.sandbox.docker_path.clone(),
                    tar: find_tar()?,
                    image: config
                        .sandbox
                        .image
                        .clone()
                        .expect("validated Docker image"),
                    model: config.agent.model.clone(),
                    thinking,
                    permissions,
                    limits: DockerLimits {
                        cpus: config.sandbox.cpus,
                        memory_bytes: config.sandbox.memory_bytes,
                        pids: config.sandbox.pids,
                        temporary_storage_bytes: config.sandbox.temporary_storage_bytes,
                        workspace_storage_bytes: config.sandbox.workspace_storage_bytes,
                        ..DockerLimits::default()
                    },
                    workspaces,
                    access: Some(access),
                    allow_unpinned_image_for_tests: false,
                })?);
                factory.reconcile().await?;
                factory
            }
            SandboxKind::Native => Arc::new(NativeJobFactory::new(NativeJobFactoryConfig {
                sandbox_exec: PathBuf::from("/usr/bin/sandbox-exec"),
                node: config
                    .sandbox
                    .node_path
                    .clone()
                    .expect("validated Node path"),
                worker: config
                    .sandbox
                    .worker_path
                    .clone()
                    .expect("validated worker path"),
                model: config.agent.model.clone(),
                thinking,
                permissions,
                limits: NativeLimits::default(),
                workspaces,
                access: Some(access),
            })?),
        };
        let harness = Harness::new(job_factory);
        let principal = local_principal();
        let result = match cli.command {
            CliCommand::Serve {
                ingress: ServeCommand::Signal,
            } => {
                let signal = config.signal.ok_or_else(|| {
                    anyhow!("Signal configuration is required for 'serve signal'")
                })?;
                SignalIngress::new(
                    harness.clone(),
                    signal.daemon_url,
                    signal.account,
                    signal.allowed_senders,
                )?
                .run()
                .await
            }
            command => {
                CliIngress::new(harness.clone(), Arc::new(StandardTerminal), principal)
                    .run(command)
                    .await
            }
        };
        harness.close().await;
        result
    }
    .await;
    capabilities.close().await;
    result
}

fn prepare_local_repository(cli: &mut Cli, config: &mut Config) -> Result<()> {
    let repository = match &mut cli.command {
        CliCommand::Run(args) => &mut args.repo,
        CliCommand::Shell(args) => &mut args.repo,
        CliCommand::Arcade { .. } | CliCommand::Serve { .. } => return Ok(()),
    };
    if repository.is_some() {
        return Ok(());
    }

    let cwd = std::env::current_dir()?.canonicalize()?;
    let output = Command::new("git")
        .args(["-C", path_text(&cwd)?, "rev-parse", "--show-toplevel"])
        .output()
        .context("inspect current Git worktree")?;
    if !output.status.success() {
        return Err(anyhow!(
            "current directory is not inside a Git worktree; pass --repo with a configured alias"
        ));
    }
    let root = PathBuf::from(
        std::str::from_utf8(&output.stdout)
            .context("current Git worktree path is not UTF-8")?
            .trim(),
    )
    .canonicalize()?;

    if let Some((alias, _)) = config.repositories.iter().find(|(_, path)| {
        path.canonicalize()
            .is_ok_and(|configured| configured == root)
    }) {
        *repository = Some(alias.clone());
        return Ok(());
    }

    const CWD_ALIAS: &str = "local-cwd";
    if config.repositories.contains_key(CWD_ALIAS) {
        return Err(anyhow!(
            "repository alias '{CWD_ALIAS}' is reserved for local current-directory mode; pass --repo explicitly"
        ));
    }
    config.repositories.insert(CWD_ALIAS.to_owned(), root);
    *repository = Some(CWD_ALIAS.to_owned());
    Ok(())
}

fn local_principal() -> String {
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    format!("uid:{}", unsafe { libc::geteuid() })
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("current directory path is not UTF-8"))
}

fn find_tar() -> Result<PathBuf> {
    ["/usr/bin/tar", "/bin/tar"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow!("tar executable was not found"))
}
