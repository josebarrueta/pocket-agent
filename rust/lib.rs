pub mod command;
pub mod domain;
pub mod harness;
pub mod ports;

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use anyhow::Result;
    use async_trait::async_trait;
    use tokio::sync::{Mutex, Notify};

    use crate::{
        domain::{
            ApprovalRequest, HarnessCommand, HarnessEvent, HarnessRequest, JobKind, JobSpec,
            RequestContext, TurnResult,
        },
        harness::Harness,
        ports::{JobEventPort, JobFactory, JobHandle, ReplyPort},
    };

    #[derive(Default)]
    struct Replies {
        events: Mutex<Vec<(String, HarnessEvent)>>,
        changed: Notify,
    }

    #[async_trait]
    impl ReplyPort for Replies {
        async fn send(&self, conversation_id: &str, event: HarnessEvent) -> Result<()> {
            self.events
                .lock()
                .await
                .push((conversation_id.to_owned(), event));
            self.changed.notify_waiters();
            Ok(())
        }
    }

    impl Replies {
        async fn wait_for(&self, predicate: impl Fn(&HarnessEvent) -> bool) -> HarnessEvent {
            loop {
                let changed = self.changed.notified();
                if let Some((_, event)) = self
                    .events
                    .lock()
                    .await
                    .iter()
                    .find(|(_, event)| predicate(event))
                    .cloned()
                {
                    return event;
                }
                changed.await;
            }
        }
    }

    struct FakeJob {
        approval: bool,
        cancelled: AtomicBool,
    }

    #[async_trait]
    impl JobHandle for FakeJob {
        async fn run_turn(
            &self,
            prompt: &str,
            events: Arc<dyn JobEventPort>,
        ) -> Result<TurnResult> {
            events.status("working").await?;
            let suffix = if self.approval {
                events
                    .request_approval(ApprovalRequest {
                        title: "Apply?".into(),
                        detail: "one operation".into(),
                        choices: vec!["yes".into(), "no".into()],
                    })
                    .await?
            } else {
                "done".into()
            };
            Ok(TurnResult {
                output: format!("{prompt}:{suffix}"),
                changed_files: vec![],
            })
        }

        async fn steer(&self, _message: &str) -> Result<()> {
            Ok(())
        }
        async fn cancel(&self) -> Result<()> {
            self.cancelled.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeFactory {
        approval: bool,
    }

    #[async_trait]
    impl JobFactory for FakeFactory {
        async fn create(&self, _spec: JobSpec) -> Result<Arc<dyn JobHandle>> {
            Ok(Arc::new(FakeJob {
                approval: self.approval,
                cancelled: AtomicBool::new(false),
            }))
        }
        fn repositories(&self) -> Vec<String> {
            vec!["app".into()]
        }
    }

    fn request(principal: &str, conversation: &str, command: HarnessCommand) -> HarnessRequest {
        request_from("test", principal, conversation, command)
    }

    fn request_from(
        ingress: &str,
        principal: &str,
        conversation: &str,
        command: HarnessCommand,
    ) -> HarnessRequest {
        HarnessRequest {
            context: RequestContext {
                ingress_id: ingress.into(),
                principal_id: principal.into(),
                conversation_id: conversation.into(),
                request_id: "request".into(),
            },
            command,
        }
    }

    #[tokio::test]
    async fn ingress_identity_is_independent_from_job_coordination() {
        let harness = Harness::new(Arc::new(FakeFactory { approval: false }));
        let replies = Arc::new(Replies::default());
        harness
            .handle(
                request(
                    "local-user",
                    "terminal-1",
                    HarnessCommand::Start {
                        repository: "app".into(),
                        prompt: "fix".into(),
                        kind: JobKind::Task,
                    },
                ),
                replies.clone(),
            )
            .await
            .unwrap();

        let started = replies
            .wait_for(|event| matches!(event, HarnessEvent::JobStarted { .. }))
            .await;
        let HarnessEvent::JobStarted { job_id, .. } = started else {
            unreachable!()
        };
        let completed = replies
            .wait_for(|event| matches!(event, HarnessEvent::TurnCompleted { .. }))
            .await;
        assert!(
            matches!(completed, HarnessEvent::TurnCompleted { output, .. } if output == "fix:done")
        );

        harness
            .handle(
                request_from(
                    "signal",
                    "local-user",
                    "terminal-1",
                    HarnessCommand::Select { job_id },
                ),
                replies.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(
            replies.events.lock().await.last().unwrap().1,
            HarnessEvent::Error { .. }
        ));
        harness.close().await;
    }

    #[tokio::test]
    async fn approvals_are_scoped_and_resume_the_waiting_turn() {
        let harness = Harness::new(Arc::new(FakeFactory { approval: true }));
        let replies = Arc::new(Replies::default());
        harness
            .handle(
                request(
                    "user",
                    "terminal",
                    HarnessCommand::Start {
                        repository: "app".into(),
                        prompt: "change".into(),
                        kind: JobKind::Task,
                    },
                ),
                replies.clone(),
            )
            .await
            .unwrap();
        let approval = replies
            .wait_for(|event| matches!(event, HarnessEvent::ApprovalRequested { .. }))
            .await;
        let HarnessEvent::ApprovalRequested { request_id, .. } = approval else {
            unreachable!()
        };

        harness
            .handle(
                request(
                    "other",
                    "terminal",
                    HarnessCommand::Answer {
                        request_id: request_id.clone(),
                        answer: "yes".into(),
                    },
                ),
                replies.clone(),
            )
            .await
            .unwrap();
        assert!(
            matches!(replies.events.lock().await.last().unwrap().1, HarnessEvent::Acknowledged { ref message } if message.contains("Unknown"))
        );

        harness
            .handle(
                request(
                    "user",
                    "terminal",
                    HarnessCommand::Answer {
                        request_id,
                        answer: "YES".into(),
                    },
                ),
                replies.clone(),
            )
            .await
            .unwrap();
        let completed = replies
            .wait_for(|event| matches!(event, HarnessEvent::TurnCompleted { .. }))
            .await;
        assert!(
            matches!(completed, HarnessEvent::TurnCompleted { output, .. } if output == "change:yes")
        );
        harness.close().await;
    }
}
