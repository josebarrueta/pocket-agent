use std::{
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use clap::Parser;
use pocket_agent::{
    cli::{Cli, CliIngress, Terminal},
    config::Config,
    docker::{DockerJobFactory, DockerJobFactoryConfig, DockerLimits},
    harness::Harness,
    model_proxy::{ModelDescriptor, ModelProxy, ModelProxyLimits},
    ports::{JobFactory, WorkerAccessIssuer},
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
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
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
    let access: Arc<dyn WorkerAccessIssuer> = proxy;
    let permissions = serde_json::to_string(&config.agent.permissions)?;
    let tar = find_tar()?;
    let factory = Arc::new(DockerJobFactory::new(DockerJobFactoryConfig {
        docker: config.sandbox.docker_path,
        tar,
        image: config.sandbox.image,
        model: config.agent.model,
        thinking: serde_json::to_value(config.agent.thinking)?
            .as_str()
            .unwrap_or("medium")
            .to_owned(),
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
    let job_factory: Arc<dyn JobFactory> = factory;
    let harness = Harness::new(job_factory);
    let principal = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "local".into());
    let ingress = CliIngress::new(harness.clone(), Arc::new(StandardTerminal), principal);
    let result = ingress.run(cli.command).await;
    harness.close().await;
    result
}

fn find_tar() -> Result<PathBuf> {
    ["/usr/bin/tar", "/bin/tar"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow!("tar executable was not found"))
}
