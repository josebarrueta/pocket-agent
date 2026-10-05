use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestContext {
    pub ingress_id: String,
    pub principal_id: String,
    pub conversation_id: String,
    pub request_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessRequest {
    pub context: RequestContext,
    pub command: HarnessCommand,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HarnessCommand {
    Start {
        repository: String,
        prompt: String,
        kind: JobKind,
    },
    Continue {
        message: String,
    },
    Steer {
        message: String,
    },
    Answer {
        request_id: String,
        answer: String,
    },
    Cancel {
        job_id: Option<String>,
    },
    Select {
        job_id: String,
    },
    ListJobs,
    Status,
    Help,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobKind {
    Task,
    Bug,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Idle,
    Running,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    JobStarted {
        job_id: String,
        repository: String,
    },
    Status {
        job_id: String,
        message: String,
    },
    ApprovalRequested {
        request_id: String,
        job_id: String,
        title: String,
        detail: String,
        choices: Vec<String>,
    },
    AuthorizationRequired {
        job_id: String,
        connector: String,
        capability: String,
        url: String,
    },
    TurnCompleted {
        job_id: String,
        output: String,
        changed_files: Vec<ChangedFile>,
    },
    JobFailed {
        job_id: String,
        message: String,
    },
    JobCancelled {
        job_id: String,
    },
    Jobs {
        active_job_id: Option<String>,
        jobs: Vec<JobSummary>,
    },
    Help {
        repositories: Vec<String>,
    },
    Acknowledged {
        message: String,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobSummary {
    pub job_id: String,
    pub repository: String,
    pub state: JobState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobSpec {
    pub job_id: String,
    pub ingress_id: String,
    pub principal_id: String,
    pub conversation_id: String,
    pub repository: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnResult {
    pub output: String,
    pub changed_files: Vec<ChangedFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRequest {
    pub title: String,
    pub detail: String,
    pub choices: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizationRequest {
    pub connector: String,
    pub capability: String,
    pub url: String,
}
