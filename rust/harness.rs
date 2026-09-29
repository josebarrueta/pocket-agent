use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Result, anyhow};
use tokio::sync::{Mutex, RwLock, oneshot};
use uuid::Uuid;

use crate::{
    domain::{
        ApprovalRequest, HarnessCommand, HarnessEvent, HarnessRequest, JobKind, JobSpec, JobState,
        JobSummary,
    },
    ports::{JobEventPort, JobFactory, JobHandle, ReplyPort},
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ScopeKey {
    ingress_id: String,
    principal_id: String,
    conversation_id: String,
}

impl ScopeKey {
    fn from_request(context: &crate::domain::RequestContext) -> Self {
        Self {
            ingress_id: context.ingress_id.clone(),
            principal_id: context.principal_id.clone(),
            conversation_id: context.conversation_id.clone(),
        }
    }
}

struct JobRecord {
    ingress_id: String,
    principal_id: String,
    conversation_id: String,
    repository: String,
    state: Mutex<JobState>,
    handle: Arc<dyn JobHandle>,
}

struct PendingApproval {
    ingress_id: String,
    principal_id: String,
    conversation_id: String,
    job_id: String,
    choices: Vec<String>,
    answer: oneshot::Sender<String>,
}

#[derive(Default)]
struct ApprovalBroker {
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, PendingApproval>>,
}

impl ApprovalBroker {
    async fn request(
        &self,
        ingress_id: &str,
        principal_id: &str,
        conversation_id: &str,
        job_id: &str,
        request: ApprovalRequest,
        replies: &Arc<dyn ReplyPort>,
    ) -> Result<String> {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (answer, receiver) = oneshot::channel();
        self.pending.lock().await.insert(
            request_id.clone(),
            PendingApproval {
                ingress_id: ingress_id.to_owned(),
                principal_id: principal_id.to_owned(),
                conversation_id: conversation_id.to_owned(),
                job_id: job_id.to_owned(),
                choices: request.choices.clone(),
                answer,
            },
        );
        if let Err(error) = replies
            .send(
                conversation_id,
                HarnessEvent::ApprovalRequested {
                    request_id: request_id.clone(),
                    job_id: job_id.to_owned(),
                    title: request.title,
                    detail: request.detail,
                    choices: request.choices,
                },
            )
            .await
        {
            self.pending.lock().await.remove(&request_id);
            return Err(error);
        }
        receiver
            .await
            .map_err(|_| anyhow!("approval request was cancelled"))
    }

    async fn answer(
        &self,
        ingress_id: &str,
        principal_id: &str,
        conversation_id: &str,
        request_id: &str,
        answer: &str,
    ) -> bool {
        let mut pending = self.pending.lock().await;
        let Some(request) = pending.get(request_id) else {
            return false;
        };
        if request.ingress_id != ingress_id
            || request.principal_id != principal_id
            || request.conversation_id != conversation_id
        {
            return false;
        }
        let normalized = if request.choices.is_empty() {
            answer.trim().to_owned()
        } else {
            let Some(choice) = request
                .choices
                .iter()
                .find(|choice| choice.eq_ignore_ascii_case(answer.trim()))
            else {
                return false;
            };
            choice.clone()
        };
        let request = pending.remove(request_id).expect("pending approval exists");
        request.answer.send(normalized).is_ok()
    }

    async fn cancel_job(&self, conversation_id: &str, job_id: &str) {
        let mut pending = self.pending.lock().await;
        let ids = pending
            .iter()
            .filter(|(_, request)| {
                request.conversation_id == conversation_id && request.job_id == job_id
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(request) = pending.remove(&id) {
                let _ = request.answer.send("cancel".to_owned());
            }
        }
    }
}

struct TurnEvents {
    ingress_id: String,
    principal_id: String,
    conversation_id: String,
    job_id: String,
    replies: Arc<dyn ReplyPort>,
    approvals: Arc<ApprovalBroker>,
}

#[async_trait::async_trait]
impl JobEventPort for TurnEvents {
    async fn status(&self, message: &str) -> Result<()> {
        self.replies
            .send(
                &self.conversation_id,
                HarnessEvent::Status {
                    job_id: self.job_id.clone(),
                    message: message.to_owned(),
                },
            )
            .await
    }

    async fn request_approval(&self, request: ApprovalRequest) -> Result<String> {
        self.approvals
            .request(
                &self.ingress_id,
                &self.principal_id,
                &self.conversation_id,
                &self.job_id,
                request,
                &self.replies,
            )
            .await
    }
}

pub struct Harness {
    jobs: RwLock<HashMap<String, Arc<JobRecord>>>,
    active: RwLock<HashMap<ScopeKey, String>>,
    factory: Arc<dyn JobFactory>,
    approvals: Arc<ApprovalBroker>,
}

impl Harness {
    pub fn new(factory: Arc<dyn JobFactory>) -> Arc<Self> {
        Arc::new(Self {
            jobs: RwLock::new(HashMap::new()),
            active: RwLock::new(HashMap::new()),
            factory,
            approvals: Arc::new(ApprovalBroker::default()),
        })
    }

    pub async fn handle(
        self: &Arc<Self>,
        request: HarnessRequest,
        replies: Arc<dyn ReplyPort>,
    ) -> Result<()> {
        let conversation_id = request.context.conversation_id.clone();
        let result = self.dispatch(request, replies.clone()).await;
        if let Err(error) = result {
            replies
                .send(
                    &conversation_id,
                    HarnessEvent::Error {
                        message: error.to_string(),
                    },
                )
                .await?;
        }
        Ok(())
    }

    async fn dispatch(
        self: &Arc<Self>,
        request: HarnessRequest,
        replies: Arc<dyn ReplyPort>,
    ) -> Result<()> {
        let context = request.context;
        match request.command {
            HarnessCommand::Start {
                repository,
                prompt,
                kind,
            } => {
                if !self
                    .factory
                    .repositories()
                    .iter()
                    .any(|candidate| candidate == &repository)
                {
                    return Err(anyhow!("Unknown repository alias {repository}"));
                }
                if prompt.trim().is_empty() {
                    return Err(anyhow!("Prompt cannot be empty"));
                }
                let job_id = Uuid::new_v4().simple().to_string()[..8].to_owned();
                let handle = self
                    .factory
                    .create(JobSpec {
                        job_id: job_id.clone(),
                        ingress_id: context.ingress_id.clone(),
                        principal_id: context.principal_id.clone(),
                        conversation_id: context.conversation_id.clone(),
                        repository: repository.clone(),
                    })
                    .await?;
                let job = Arc::new(JobRecord {
                    ingress_id: context.ingress_id.clone(),
                    principal_id: context.principal_id.clone(),
                    conversation_id: context.conversation_id.clone(),
                    repository: repository.clone(),
                    state: Mutex::new(JobState::Idle),
                    handle,
                });
                self.jobs.write().await.insert(job_id.clone(), job.clone());
                self.active
                    .write()
                    .await
                    .insert(ScopeKey::from_request(&context), job_id.clone());
                replies
                    .send(
                        &context.conversation_id,
                        HarnessEvent::JobStarted {
                            job_id: job_id.clone(),
                            repository,
                        },
                    )
                    .await?;
                let prompt = match kind {
                    JobKind::Task => prompt,
                    JobKind::Bug => format!(
                        "Investigate this bug, reproduce it if possible, implement a safe fix, run relevant tests, and summarize the result: {prompt}"
                    ),
                };
                *job.state.lock().await = JobState::Running;
                self.spawn_turn(job_id, job, prompt, replies);
            }
            HarnessCommand::Continue { message } => {
                let (job_id, job) = self
                    .active_job(
                        &context.ingress_id,
                        &context.principal_id,
                        &context.conversation_id,
                    )
                    .await?;
                let mut state = job.state.lock().await;
                if *state == JobState::Running {
                    drop(state);
                    job.handle.steer(&message).await?;
                    replies
                        .send(
                            &context.conversation_id,
                            HarnessEvent::Acknowledged {
                                message: format!("Steering message queued for [{job_id}]"),
                            },
                        )
                        .await?;
                } else {
                    if matches!(*state, JobState::Cancelled | JobState::Failed) {
                        return Err(anyhow!("Active job is unavailable"));
                    }
                    *state = JobState::Running;
                    drop(state);
                    self.spawn_turn(job_id, job, message, replies);
                }
            }
            HarnessCommand::Steer { message } => {
                let (job_id, job) = self
                    .active_job(
                        &context.ingress_id,
                        &context.principal_id,
                        &context.conversation_id,
                    )
                    .await?;
                if *job.state.lock().await != JobState::Running {
                    return Err(anyhow!("Active job is not running"));
                }
                job.handle.steer(&message).await?;
                replies
                    .send(
                        &context.conversation_id,
                        HarnessEvent::Acknowledged {
                            message: format!("Steering message queued for [{job_id}]"),
                        },
                    )
                    .await?;
            }
            HarnessCommand::Answer { request_id, answer } => {
                let accepted = self
                    .approvals
                    .answer(
                        &context.ingress_id,
                        &context.principal_id,
                        &context.conversation_id,
                        &request_id,
                        &answer,
                    )
                    .await;
                let message = if accepted {
                    format!("Answered [{request_id}]")
                } else {
                    "Unknown request or invalid choice".to_owned()
                };
                replies
                    .send(
                        &context.conversation_id,
                        HarnessEvent::Acknowledged { message },
                    )
                    .await?;
            }
            HarnessCommand::Cancel { job_id } => {
                let (job_id, job) = match job_id {
                    Some(id) => (
                        id.clone(),
                        self.owned_job(
                            &id,
                            &context.ingress_id,
                            &context.principal_id,
                            &context.conversation_id,
                        )
                        .await?,
                    ),
                    None => {
                        self.active_job(
                            &context.ingress_id,
                            &context.principal_id,
                            &context.conversation_id,
                        )
                        .await?
                    }
                };
                self.approvals
                    .cancel_job(&context.conversation_id, &job_id)
                    .await;
                *job.state.lock().await = JobState::Cancelled;
                job.handle.cancel().await?;
                replies
                    .send(
                        &context.conversation_id,
                        HarnessEvent::JobCancelled { job_id },
                    )
                    .await?;
            }
            HarnessCommand::Select { job_id } => {
                let job = self
                    .owned_job(
                        &job_id,
                        &context.ingress_id,
                        &context.principal_id,
                        &context.conversation_id,
                    )
                    .await?;
                if *job.state.lock().await == JobState::Cancelled {
                    return Err(anyhow!("Cancelled jobs cannot be selected"));
                }
                self.active
                    .write()
                    .await
                    .insert(ScopeKey::from_request(&context), job_id.clone());
                replies
                    .send(
                        &context.conversation_id,
                        HarnessEvent::Acknowledged {
                            message: format!("Selected [{job_id}]"),
                        },
                    )
                    .await?;
            }
            HarnessCommand::ListJobs => {
                let active_job_id = self
                    .active
                    .read()
                    .await
                    .get(&ScopeKey::from_request(&context))
                    .cloned();
                let records = self
                    .jobs
                    .read()
                    .await
                    .iter()
                    .map(|(id, job)| (id.clone(), job.clone()))
                    .collect::<Vec<_>>();
                let mut jobs = Vec::new();
                for (job_id, job) in records {
                    if job.ingress_id == context.ingress_id
                        && job.principal_id == context.principal_id
                        && job.conversation_id == context.conversation_id
                    {
                        jobs.push(JobSummary {
                            job_id,
                            repository: job.repository.clone(),
                            state: *job.state.lock().await,
                        });
                    }
                }
                jobs.sort_by(|left, right| left.job_id.cmp(&right.job_id));
                replies
                    .send(
                        &context.conversation_id,
                        HarnessEvent::Jobs {
                            active_job_id,
                            jobs,
                        },
                    )
                    .await?;
            }
            HarnessCommand::Status => {
                let (job_id, job) = self
                    .active_job(
                        &context.ingress_id,
                        &context.principal_id,
                        &context.conversation_id,
                    )
                    .await?;
                replies
                    .send(
                        &context.conversation_id,
                        HarnessEvent::Jobs {
                            active_job_id: Some(job_id.clone()),
                            jobs: vec![JobSummary {
                                job_id,
                                repository: job.repository.clone(),
                                state: *job.state.lock().await,
                            }],
                        },
                    )
                    .await?;
            }
            HarnessCommand::Help => {
                replies
                    .send(
                        &context.conversation_id,
                        HarnessEvent::Help {
                            repositories: self.factory.repositories(),
                        },
                    )
                    .await?;
            }
        }
        Ok(())
    }

    fn spawn_turn(
        self: &Arc<Self>,
        job_id: String,
        job: Arc<JobRecord>,
        prompt: String,
        replies: Arc<dyn ReplyPort>,
    ) {
        let harness = self.clone();
        tokio::spawn(async move {
            let events: Arc<dyn JobEventPort> = Arc::new(TurnEvents {
                ingress_id: job.ingress_id.clone(),
                principal_id: job.principal_id.clone(),
                conversation_id: job.conversation_id.clone(),
                job_id: job_id.clone(),
                replies: replies.clone(),
                approvals: harness.approvals.clone(),
            });
            match job.handle.run_turn(&prompt, events).await {
                Ok(result) => {
                    if *job.state.lock().await == JobState::Cancelled {
                        return;
                    }
                    *job.state.lock().await = JobState::Idle;
                    let _ = replies
                        .send(
                            &job.conversation_id,
                            HarnessEvent::TurnCompleted {
                                job_id,
                                output: result.output,
                                changed_files: result.changed_files,
                            },
                        )
                        .await;
                }
                Err(error) => {
                    if *job.state.lock().await == JobState::Cancelled {
                        return;
                    }
                    *job.state.lock().await = JobState::Failed;
                    harness
                        .approvals
                        .cancel_job(&job.conversation_id, &job_id)
                        .await;
                    let _ = job.handle.cancel().await;
                    let _ = replies
                        .send(
                            &job.conversation_id,
                            HarnessEvent::JobFailed {
                                job_id,
                                message: error.to_string(),
                            },
                        )
                        .await;
                }
            }
        });
    }

    async fn active_job(
        &self,
        ingress_id: &str,
        principal_id: &str,
        conversation_id: &str,
    ) -> Result<(String, Arc<JobRecord>)> {
        let id = self
            .active
            .read()
            .await
            .get(&ScopeKey {
                ingress_id: ingress_id.to_owned(),
                principal_id: principal_id.to_owned(),
                conversation_id: conversation_id.to_owned(),
            })
            .cloned()
            .ok_or_else(|| anyhow!("No active job"))?;
        let job = self
            .owned_job(&id, ingress_id, principal_id, conversation_id)
            .await?;
        Ok((id, job))
    }

    async fn owned_job(
        &self,
        id: &str,
        ingress_id: &str,
        principal_id: &str,
        conversation_id: &str,
    ) -> Result<Arc<JobRecord>> {
        let job = self
            .jobs
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("No matching job"))?;
        if job.ingress_id != ingress_id
            || job.principal_id != principal_id
            || job.conversation_id != conversation_id
        {
            return Err(anyhow!("No matching job"));
        }
        Ok(job)
    }

    pub async fn close(&self) {
        let jobs = self
            .jobs
            .read()
            .await
            .iter()
            .map(|(id, job)| (id.clone(), job.clone()))
            .collect::<Vec<_>>();
        for (job_id, job) in jobs {
            self.approvals
                .cancel_job(&job.conversation_id, &job_id)
                .await;
            *job.state.lock().await = JobState::Cancelled;
            let _ = job.handle.cancel().await;
        }
    }
}
