use std::{path::PathBuf, sync::Arc};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use clap::{Args, Parser, Subcommand};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    domain::{HarnessCommand, HarnessEvent, HarnessRequest, JobKind, RequestContext},
    harness::Harness,
    ports::ReplyPort,
};

#[derive(Debug, Parser)]
#[command(name = "pocket-agent", version, about = "Run isolated coding jobs")]
pub struct Cli {
    #[arg(long, env = "POCKET_AGENT_CONFIG", default_value = "config.json")]
    pub config: PathBuf,
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Run one task and exit when its first turn finishes.
    Run(RunArgs),
    /// Start an interactive local conversation.
    Shell(ShellArgs),
    /// Manage the optional Arcade MCP Gateway connection.
    Arcade {
        #[command(subcommand)]
        action: ArcadeCommand,
    },
    /// Run a remote ingress adapter.
    Serve {
        #[command(subcommand)]
        ingress: ServeCommand,
    },
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Configured repository alias. Defaults to a disposable snapshot of the current Git worktree.
    #[arg(long)]
    pub repo: Option<String>,
    #[arg(long, conflicts_with = "bug")]
    pub prompt: Option<String>,
    #[arg(long, conflicts_with = "prompt")]
    pub bug: Option<String>,
}

#[derive(Debug, Args)]
pub struct ShellArgs {
    /// Configured repository alias. Defaults to a disposable snapshot of the current Git worktree.
    #[arg(long)]
    pub repo: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ArcadeCommand {
    /// Delete this local principal's persisted gateway authorization.
    Logout,
}

#[derive(Debug, Subcommand)]
pub enum ServeCommand {
    Signal,
}

#[async_trait]
pub trait Terminal: Send + Sync {
    async fn write(&self, text: &str) -> Result<()>;
    async fn read(&self, prompt: &str) -> Result<Option<String>>;
}

struct CliReplies {
    events: mpsc::UnboundedSender<HarnessEvent>,
}

#[async_trait]
impl ReplyPort for CliReplies {
    async fn send(&self, _conversation_id: &str, event: HarnessEvent) -> Result<()> {
        self.events.send(event).map_err(|_| anyhow!("CLI closed"))
    }
}

pub struct CliIngress {
    harness: Arc<Harness>,
    terminal: Arc<dyn Terminal>,
    principal_id: String,
    conversation_id: String,
}

impl CliIngress {
    pub fn new(harness: Arc<Harness>, terminal: Arc<dyn Terminal>, principal_id: String) -> Self {
        Self {
            harness,
            terminal,
            principal_id,
            conversation_id: format!("cli-{}", Uuid::new_v4().simple()),
        }
    }

    pub async fn run(&self, command: CliCommand) -> Result<()> {
        match command {
            CliCommand::Run(args) => {
                let (prompt, kind) = match (args.prompt, args.bug) {
                    (Some(prompt), None) => (prompt, JobKind::Task),
                    (None, Some(prompt)) => (prompt, JobKind::Bug),
                    _ => return Err(anyhow!("exactly one of --prompt or --bug is required")),
                };
                let (events, mut receiver) = mpsc::unbounded_channel();
                let replies: Arc<dyn ReplyPort> = Arc::new(CliReplies { events });
                self.send(
                    HarnessCommand::Start {
                        repository: args
                            .repo
                            .ok_or_else(|| anyhow!("local repository was not resolved"))?,
                        prompt,
                        kind,
                    },
                    replies.clone(),
                )
                .await?;
                while let Some(event) = receiver.recv().await {
                    if self.handle_event(event, replies.clone()).await? {
                        return Ok(());
                    }
                }
                Err(anyhow!(
                    "harness reply channel closed before the turn finished"
                ))
            }
            CliCommand::Shell(args) => self.shell(args).await,
            CliCommand::Arcade { .. } => Err(anyhow!(
                "Arcade management command was not handled at startup"
            )),
            CliCommand::Serve {
                ingress: ServeCommand::Signal,
            } => Err(anyhow!("Signal ingress has not been migrated to Rust yet")),
        }
    }

    async fn shell(&self, args: ShellArgs) -> Result<()> {
        let first = self
            .terminal
            .read("task> ")
            .await?
            .ok_or_else(|| anyhow!("No task provided"))?;
        let (events, mut receiver) = mpsc::unbounded_channel();
        let replies: Arc<dyn ReplyPort> = Arc::new(CliReplies { events });
        self.send(
            HarnessCommand::Start {
                repository: args
                    .repo
                    .ok_or_else(|| anyhow!("local repository was not resolved"))?,
                prompt: first,
                kind: JobKind::Task,
            },
            replies.clone(),
        )
        .await?;
        loop {
            let event = receiver
                .recv()
                .await
                .ok_or_else(|| anyhow!("harness reply channel closed"))?;
            let terminal = matches!(
                event,
                HarnessEvent::JobFailed { .. } | HarnessEvent::JobCancelled { .. }
            );
            let completed = matches!(event, HarnessEvent::TurnCompleted { .. });
            self.handle_event(event, replies.clone()).await?;
            if terminal {
                return Ok(());
            }
            if completed {
                let Some(message) = self.terminal.read("> ").await? else {
                    self.send(HarnessCommand::Cancel { job_id: None }, replies.clone())
                        .await?;
                    return Ok(());
                };
                if message.trim().eq_ignore_ascii_case("/exit") {
                    self.send(HarnessCommand::Cancel { job_id: None }, replies.clone())
                        .await?;
                    return Ok(());
                }
                let command = crate::command::parse_text_command(&message)?;
                self.send(command, replies.clone()).await?;
            }
        }
    }

    async fn handle_event(&self, event: HarnessEvent, replies: Arc<dyn ReplyPort>) -> Result<bool> {
        match event {
            HarnessEvent::JobStarted { job_id, repository } => {
                self.terminal
                    .write(&format!("Started [{job_id}] in {repository}"))
                    .await?;
            }
            HarnessEvent::Status { job_id, message } => {
                self.terminal
                    .write(&format!("[{job_id}] {message}"))
                    .await?;
            }
            HarnessEvent::ApprovalRequested {
                request_id,
                title,
                detail,
                choices,
                ..
            } => {
                let choices = if choices.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", choices.join("/"))
                };
                let answer = self
                    .terminal
                    .read(&format!("{title}: {detail}{choices}> "))
                    .await?
                    .unwrap_or_else(|| "no".to_owned());
                self.send(HarnessCommand::Answer { request_id, answer }, replies)
                    .await?;
            }
            HarnessEvent::TurnCompleted {
                job_id,
                output,
                changed_files,
            } => {
                let changes = if changed_files.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nCandidate patch: {}",
                        changed_files
                            .iter()
                            .map(|file| format!("{} {}", file.status, file.path))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                self.terminal
                    .write(&format!("[{job_id}] {output}{changes}"))
                    .await?;
                return Ok(true);
            }
            HarnessEvent::JobFailed { job_id, message } => {
                self.terminal
                    .write(&format!("[{job_id}] failed: {message}"))
                    .await?;
                return Ok(true);
            }
            HarnessEvent::JobCancelled { job_id } => {
                self.terminal
                    .write(&format!("[{job_id}] cancelled"))
                    .await?;
                return Ok(true);
            }
            HarnessEvent::Acknowledged { message } => self.terminal.write(&message).await?,
            HarnessEvent::Error { message } => return Err(anyhow!(message)),
            HarnessEvent::Jobs { .. } | HarnessEvent::Help { .. } => {
                self.terminal.write(&serde_json::to_string(&event)?).await?;
            }
        }
        Ok(false)
    }

    async fn send(&self, command: HarnessCommand, replies: Arc<dyn ReplyPort>) -> Result<()> {
        self.harness
            .handle(
                HarnessRequest {
                    context: RequestContext {
                        ingress_id: "cli".to_owned(),
                        principal_id: self.principal_id.clone(),
                        conversation_id: self.conversation_id.clone(),
                        request_id: Uuid::new_v4().to_string(),
                    },
                    command,
                },
                replies,
            )
            .await
    }
}
