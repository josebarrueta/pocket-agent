use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;

use crate::domain::{ApprovalRequest, HarnessEvent, JobSpec, TurnResult};

#[async_trait]
pub trait ReplyPort: Send + Sync {
    async fn send(&self, conversation_id: &str, event: HarnessEvent) -> Result<()>;
}

#[async_trait]
pub trait JobEventPort: Send + Sync {
    async fn status(&self, message: &str) -> Result<()>;
    async fn request_approval(&self, request: ApprovalRequest) -> Result<String>;
}

#[async_trait]
pub trait JobHandle: Send + Sync {
    async fn run_turn(&self, prompt: &str, events: Arc<dyn JobEventPort>) -> Result<TurnResult>;
    async fn steer(&self, message: &str) -> Result<()>;
    async fn cancel(&self) -> Result<()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivateMount {
    pub source: PathBuf,
    pub destination: PathBuf,
}

pub trait WorkerLease: Send + Sync {
    fn environment(&self) -> BTreeMap<String, String>;
    fn mounts(&self) -> Vec<PrivateMount>;
    fn set_events(&self, _events: Option<Arc<dyn JobEventPort>>) {}
    fn revoke(&self);
}

#[async_trait]
pub trait WorkerAccessIssuer: Send + Sync {
    async fn issue(&self, spec: &JobSpec) -> Result<Vec<Arc<dyn WorkerLease>>>;
}

pub struct CombinedAccessIssuers(pub Vec<Arc<dyn WorkerAccessIssuer>>);

#[async_trait]
impl WorkerAccessIssuer for CombinedAccessIssuers {
    async fn issue(&self, spec: &JobSpec) -> Result<Vec<Arc<dyn WorkerLease>>> {
        let mut leases = Vec::new();
        for issuer in &self.0 {
            match issuer.issue(spec).await {
                Ok(mut issued) => leases.append(&mut issued),
                Err(error) => {
                    for lease in &leases {
                        lease.revoke();
                    }
                    return Err(error);
                }
            }
        }
        Ok(leases)
    }
}

#[async_trait]
pub trait JobFactory: Send + Sync {
    async fn create(&self, spec: JobSpec) -> Result<Arc<dyn JobHandle>>;
    fn repositories(&self) -> Vec<String>;
}
