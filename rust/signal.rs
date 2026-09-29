use std::{collections::BTreeSet, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use reqwest::{Client, Response, Url, redirect::Policy};
use serde_json::{Value, json};
use tokio::time::sleep;
use uuid::Uuid;

use crate::{
    command::parse_text_command,
    domain::{HarnessEvent, HarnessRequest, RequestContext},
    harness::Harness,
    ports::ReplyPort,
};

const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const SIGNAL_MESSAGE_BYTES: usize = 3500;

#[derive(Clone, Debug, Eq, PartialEq)]
struct IncomingSignalMessage {
    request_id: String,
    sender_id: String,
    text: String,
}

#[derive(Clone)]
struct SignalTransport {
    client: Client,
    base_url: Url,
    account: String,
}

impl SignalTransport {
    async fn send(&self, conversation_id: &str, text: &str) -> Result<()> {
        for chunk in message_chunks(text, SIGNAL_MESSAGE_BYTES) {
            let response = self
                .client
                .post(endpoint(&self.base_url, "/api/v1/rpc")?)
                .json(&json!({
                    "jsonrpc": "2.0", "id": Uuid::new_v4().to_string(), "method": "send",
                    "params": { "account": self.account, "recipient": [conversation_id], "message": chunk }
                }))
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .context("Signal send failed")?;
            let status = response.status();
            let body = read_bounded(response, MAX_RESPONSE_BYTES).await?;
            ensure!(status.is_success(), "Signal send failed: HTTP {status}");
            let rpc: Value =
                serde_json::from_slice(&body).context("Signal returned malformed JSON-RPC")?;
            if let Some(error) = rpc.get("error") {
                bail!(
                    "Signal send failed: {}",
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("JSON-RPC error")
                );
            }
        }
        Ok(())
    }
}

struct SignalReplies {
    transport: SignalTransport,
}

#[async_trait]
impl ReplyPort for SignalReplies {
    async fn send(&self, conversation_id: &str, event: HarnessEvent) -> Result<()> {
        self.transport
            .send(conversation_id, &format_event(event))
            .await
    }
}

pub struct SignalIngress {
    harness: Arc<Harness>,
    transport: SignalTransport,
    allowed: BTreeSet<String>,
}

impl SignalIngress {
    pub fn new(
        harness: Arc<Harness>,
        base_url: String,
        account: String,
        allowed_senders: Vec<String>,
    ) -> Result<Self> {
        ensure!(
            !account.is_empty() && !allowed_senders.is_empty(),
            "Signal requires an account and allowed senders"
        );
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(Policy::none())
            .no_proxy()
            .build()?;
        Ok(Self {
            harness,
            transport: SignalTransport {
                client,
                base_url: Url::parse(&base_url).context("Signal daemon URL is invalid")?,
                account,
            },
            allowed: allowed_senders.into_iter().collect(),
        })
    }

    pub async fn run(&self) -> Result<()> {
        self.check().await?;
        eprintln!(
            "Pocket Agent is connected to Signal as {}",
            self.transport.account
        );
        loop {
            tokio::select! {
                signal = shutdown_signal() => return signal,
                result = self.consume_events() => {
                    if let Err(error) = result {
                        eprintln!("Signal event stream disconnected: {error:#}");
                        tokio::select! {
                            signal = shutdown_signal() => return signal,
                            _ = sleep(Duration::from_secs(2)) => {}
                        }
                    }
                }
            }
        }
    }

    async fn check(&self) -> Result<()> {
        let response = self
            .transport
            .client
            .get(endpoint(&self.transport.base_url, "/api/v1/check")?)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("signal-cli daemon is not reachable")?;
        ensure!(
            response.status().is_success(),
            "signal-cli daemon is not ready: HTTP {}",
            response.status()
        );
        Ok(())
    }

    async fn consume_events(&self) -> Result<()> {
        let mut response = self
            .transport
            .client
            .get(endpoint(&self.transport.base_url, "/api/v1/events")?)
            .header("accept", "text/event-stream")
            .send()
            .await
            .context("Signal events failed")?;
        ensure!(
            response.status().is_success(),
            "Signal events failed: HTTP {}",
            response.status()
        );
        let mut buffer = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            buffer.extend_from_slice(&chunk);
            ensure!(
                buffer.len() <= MAX_EVENT_BYTES,
                "Signal event exceeded its byte limit"
            );
            while let Some((end, delimiter)) = event_end(&buffer) {
                let event = buffer.drain(..end).collect::<Vec<_>>();
                buffer.drain(..delimiter);
                if let Some(message) =
                    parse_sse_event(&event, &self.transport.account, &self.allowed)?
                {
                    self.handle(message).await;
                }
            }
        }
        bail!("Signal event stream ended")
    }

    async fn handle(&self, message: IncomingSignalMessage) {
        let replies: Arc<dyn ReplyPort> = Arc::new(SignalReplies {
            transport: self.transport.clone(),
        });
        let command = match parse_text_command(&message.text) {
            Ok(command) => command,
            Err(error) => {
                let _ = self
                    .transport
                    .send(&message.sender_id, &format!("❌ {error}"))
                    .await;
                return;
            }
        };
        let conversation_id = message.sender_id.clone();
        if let Err(error) = self
            .harness
            .handle(
                HarnessRequest {
                    context: RequestContext {
                        ingress_id: "signal".into(),
                        principal_id: message.sender_id.clone(),
                        conversation_id: message.sender_id,
                        request_id: message.request_id,
                    },
                    command,
                },
                replies,
            )
            .await
        {
            let _ = self
                .transport
                .send(&conversation_id, &format!("❌ {error}"))
                .await;
        }
    }
}

fn parse_sse_event(
    bytes: &[u8],
    account: &str,
    allowed: &BTreeSet<String>,
) -> Result<Option<IncomingSignalMessage>> {
    let text = std::str::from_utf8(bytes).context("Signal event is not UTF-8")?;
    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(None);
    }
    let notification: Value = serde_json::from_str(&data).context("Signal event is malformed")?;
    Ok(parse_notification(&notification, account, allowed))
}

fn parse_notification(
    notification: &Value,
    account: &str,
    allowed: &BTreeSet<String>,
) -> Option<IncomingSignalMessage> {
    if notification.get("method").and_then(Value::as_str) != Some("receive") {
        return None;
    }
    let params = notification.get("params")?;
    let result = params.get("result");
    let envelope = params.get("envelope").or_else(|| result?.get("envelope"))?;
    let received_account = params
        .get("account")
        .or_else(|| result.and_then(|value| value.get("account")))
        .and_then(Value::as_str);
    if received_account.is_some_and(|value| value != account)
        || envelope.get("syncMessage").is_some()
    {
        return None;
    }
    let data = envelope.get("dataMessage")?;
    if data.get("groupInfo").is_some() {
        return None;
    }
    let sender = ["sourceNumber", "sourceUuid", "source"]
        .into_iter()
        .filter_map(|key| envelope.get(key).and_then(Value::as_str))
        .find(|candidate| allowed.contains(*candidate))?;
    let text = data.get("message")?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    let timestamp = data
        .get("timestamp")
        .or_else(|| envelope.get("timestamp"))
        .and_then(Value::as_i64);
    Some(IncomingSignalMessage {
        request_id: timestamp.map_or_else(|| Uuid::new_v4().to_string(), |value| value.to_string()),
        sender_id: sender.to_owned(),
        text: text.to_owned(),
    })
}

fn format_event(event: HarnessEvent) -> String {
    match event {
        HarnessEvent::JobStarted { job_id, repository } => {
            format!("🚀 [{job_id}] Starting in {repository}.")
        }
        HarnessEvent::Status { job_id, message } => format!("[{job_id}] {message}"),
        HarnessEvent::ApprovalRequested {
            request_id,
            job_id,
            title,
            detail,
            choices,
        } => format!(
            "❓ [{job_id}] {title}\n{detail}\nReply /answer {request_id} <{}>",
            choices.join("|")
        ),
        HarnessEvent::TurnCompleted {
            job_id,
            output,
            changed_files,
        } => {
            let changed = if changed_files.is_empty() {
                String::new()
            } else {
                format!(
                    "\n\nCandidate patch: {}",
                    changed_files
                        .iter()
                        .map(|file| format!("{} {:?}", file.status, file.path))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            format!("✅ [{job_id}]\n{output}{changed}")
        }
        HarnessEvent::JobFailed { job_id, message } => format!("❌ [{job_id}] {message}"),
        HarnessEvent::JobCancelled { job_id } => format!("🛑 [{job_id}] Cancelled."),
        HarnessEvent::Jobs {
            active_job_id,
            jobs,
        } => {
            if jobs.is_empty() {
                "No jobs. Use /new <repo> <task>.".into()
            } else {
                jobs.into_iter()
                    .map(|job| {
                        format!(
                            "{} [{}] {} — {}",
                            if active_job_id.as_deref() == Some(&job.job_id) {
                                "*"
                            } else {
                                " "
                            },
                            job.job_id,
                            job.repository,
                            format!("{:?}", job.state).to_lowercase()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        HarnessEvent::Help { repositories } => format!(
            "Pocket Agent commands\n/new <repo> <task>\n/bug <repo> <description>\n/steer <message>\n/answer <id> <answer>\n/cancel [job-id]\n/jobs\n/use <job-id>\n/status\nRepos: {}",
            repositories.join(", ")
        ),
        HarnessEvent::Acknowledged { message } => message,
        HarnessEvent::Error { message } => format!("❌ {message}"),
    }
}

fn endpoint(base: &Url, path: &str) -> Result<Url> {
    Ok(base.join(path)?)
}
fn event_end(bytes: &[u8]) -> Option<(usize, usize)> {
    bytes
        .windows(4)
        .position(|value| value == b"\r\n\r\n")
        .map(|end| (end, 4))
        .or_else(|| {
            bytes
                .windows(2)
                .position(|value| value == b"\n\n")
                .map(|end| (end, 2))
        })
}
fn message_chunks(text: &str, maximum: usize) -> Vec<&str> {
    if text.is_empty() {
        return vec![""];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + maximum).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}
async fn read_bounded(mut response: Response, maximum: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= maximum,
            "Signal response exceeded its byte limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(extra: Value) -> Value {
        let mut envelope = json!({ "sourceNumber": "+1556", "timestamp": 42, "dataMessage": { "message": " /help " } });
        if let (Some(target), Some(extra)) = (envelope.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        json!({ "method": "receive", "params": { "account": "+1555", "envelope": envelope } })
    }

    #[test]
    fn accepts_only_allowlisted_private_messages_for_the_configured_account() {
        let allowed = BTreeSet::from(["+1556".to_owned()]);
        let message = parse_notification(&notification(json!({})), "+1555", &allowed).unwrap();
        assert_eq!(message.sender_id, "+1556");
        assert_eq!(message.text, "/help");
        assert!(
            parse_notification(
                &notification(json!({ "dataMessage": { "message": "/help", "groupInfo": {} } })),
                "+1555",
                &allowed
            )
            .is_none()
        );
        assert!(
            parse_notification(
                &notification(json!({ "syncMessage": {} })),
                "+1555",
                &allowed
            )
            .is_none()
        );
        assert!(parse_notification(&notification(json!({})), "+other", &allowed).is_none());
        assert!(parse_notification(&notification(json!({})), "+1555", &BTreeSet::new()).is_none());
    }

    #[test]
    fn signal_formatting_and_utf8_chunking_stay_in_the_adapter() {
        let message = format_event(HarnessEvent::ApprovalRequested {
            request_id: "request".into(),
            job_id: "job".into(),
            title: "Approve?".into(),
            detail: "detail".into(),
            choices: vec!["yes".into(), "no".into()],
        });
        assert!(message.contains("/answer request <yes|no>"));
        let text = "🦀".repeat(2000);
        let chunks = message_chunks(&text, SIGNAL_MESSAGE_BYTES);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= SIGNAL_MESSAGE_BYTES)
        );
        assert_eq!(chunks.concat(), text);
    }
}
