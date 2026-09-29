use std::sync::Arc;

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

#[async_trait]
pub trait JobFactory: Send + Sync {
    async fn create(&self, spec: JobSpec) -> Result<Arc<dyn JobHandle>>;
    fn repositories(&self) -> Vec<String>;
}
