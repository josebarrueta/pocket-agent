use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs::OpenOptions,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Mutex, oneshot},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{
    domain::JobSpec,
    ports::{PrivateMount, WorkerAccessIssuer, WorkerLease},
};

const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct ModelDescriptor {
    pub provider: String,
    pub model: String,
    pub name: String,
    pub context_window: u32,
    pub max_tokens: u32,
    pub images: bool,
}

impl ModelDescriptor {
    fn worker_json(&self) -> String {
        json!({
            "provider": self.provider, "id": self.model, "name": self.name,
            "reasoning": false, "input": if self.images { vec!["text", "image"] } else { vec!["text"] },
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": self.context_window, "maxTokens": self.max_tokens,
        }).to_string()
    }
}

#[derive(Clone, Debug)]
pub struct ModelProxyLimits {
    pub lease_lifetime: Duration,
    pub request_timeout: Duration,
    pub requests_per_minute: usize,
    pub tokens_per_request: u32,
    pub tokens_per_job: u64,
}

struct LeaseState {
    job_id: String,
    expires: Instant,
    revoked: Arc<AtomicBool>,
    active: bool,
    request_times: VecDeque<Instant>,
    output_tokens: u64,
}

pub struct ModelProxy {
    socket: PathBuf,
    audit: PathBuf,
    descriptor: ModelDescriptor,
    api_key: String,
    base_url: Option<String>,
    limits: ModelProxyLimits,
    client: reqwest::Client,
    leases: Arc<Mutex<HashMap<String, LeaseState>>>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    server: Mutex<Option<JoinHandle<()>>>,
}

impl ModelProxy {
    pub fn new(
        socket: PathBuf,
        audit: PathBuf,
        descriptor: ModelDescriptor,
        api_key: String,
        base_url: Option<String>,
        limits: ModelProxyLimits,
    ) -> Result<Arc<Self>> {
        ensure!(!api_key.is_empty(), "Model provider credential is empty");
        ensure!(
            limits.requests_per_minute > 0
                && limits.tokens_per_request > 0
                && limits.tokens_per_job > 0,
            "Model proxy limits must be positive"
        );
        let client = reqwest::Client::builder()
            .timeout(limits.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Arc::new(Self {
            socket,
            audit,
            descriptor,
            api_key,
            base_url,
            limits,
            client,
            leases: Arc::new(Mutex::new(HashMap::new())),
            shutdown: Mutex::new(None),
            server: Mutex::new(None),
        }))
    }

    pub async fn start(self: &Arc<Self>) -> Result<()> {
        if let Some(parent) = self.socket.parent() {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        if let Some(parent) = self.audit.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&self.socket);
        let listener = UnixListener::bind(&self.socket)?;
        std::fs::set_permissions(&self.socket, std::fs::Permissions::from_mode(0o600))?;
        let (shutdown, mut receiver) = oneshot::channel();
        *self.shutdown.lock().await = Some(shutdown);
        let proxy = self.clone();
        *self.server.lock().await = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut receiver => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => {
                            let proxy = proxy.clone();
                            tokio::spawn(async move { let _ = proxy.handle(stream).await; });
                        }
                        Err(_) => break,
                    }
                }
            }
        }));
        Ok(())
    }

    pub async fn close(&self) {
        if let Some(shutdown) = self.shutdown.lock().await.take() {
            let _ = shutdown.send(());
        }
        if let Some(server) = self.server.lock().await.take() {
            let _ = server.await;
        }
        let mut leases = self.leases.lock().await;
        for lease in leases.values() {
            lease.revoked.store(true, Ordering::SeqCst);
        }
        leases.clear();
        let _ = std::fs::remove_file(&self.socket);
    }

    async fn handle(&self, mut stream: UnixStream) -> Result<()> {
        let request = read_http(&mut stream).await;
        let response = match request {
            Ok(request) => self.respond(request).await,
            Err(error) => Err(error),
        };
        match response {
            Ok(events) => write_http(&mut stream, 200, "application/x-ndjson", &events).await?,
            Err(error) => {
                let body = json!({ "error": public_error(&error) }).to_string();
                write_http(&mut stream, 400, "application/json", &body).await?;
            }
        }
        Ok(())
    }

    async fn respond(&self, request: HttpRequest) -> Result<String> {
        ensure!(
            request.method == "POST" && request.path == "/v1/stream",
            "Unsupported model proxy request"
        );
        let credential = request
            .headers
            .get("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| anyhow!("Model lease is required"))?;
        let job_id = request
            .headers
            .get("x-pocket-agent-job-id")
            .ok_or_else(|| anyhow!("Job identity is required"))?;
        let body: Value =
            serde_json::from_slice(&request.body).context("Malformed model request")?;
        let object = body
            .as_object()
            .ok_or_else(|| anyhow!("Model request must be an object"))?;
        ensure!(
            object
                .keys()
                .all(|key| ["provider", "model", "context", "options"].contains(&key.as_str())),
            "Unsupported model request field"
        );
        if let Some(options) = body.get("options") {
            let options = options
                .as_object()
                .ok_or_else(|| anyhow!("Model options must be an object"))?;
            ensure!(
                options
                    .keys()
                    .all(|key| ["reasoning", "temperature", "toolChoice"].contains(&key.as_str())),
                "Unsupported model option"
            );
        }
        ensure!(
            body.get("provider").and_then(Value::as_str) == Some(&self.descriptor.provider),
            "Provider is outside the model lease"
        );
        ensure!(
            body.get("model").and_then(Value::as_str) == Some(&self.descriptor.model),
            "Model is outside the model lease"
        );
        let context = body
            .get("context")
            .ok_or_else(|| anyhow!("Model context is required"))?;
        let now = Instant::now();
        {
            let mut leases = self.leases.lock().await;
            let lease = leases
                .get_mut(credential)
                .ok_or_else(|| anyhow!("Invalid model lease"))?;
            ensure!(
                lease.job_id == job_id.as_str()
                    && !lease.revoked.load(Ordering::SeqCst)
                    && now < lease.expires,
                "Invalid model lease"
            );
            ensure!(!lease.active, "Model lease concurrency limit exceeded");
            while lease
                .request_times
                .front()
                .is_some_and(|time| now.duration_since(*time) >= Duration::from_secs(60))
            {
                lease.request_times.pop_front();
            }
            ensure!(
                lease.request_times.len() < self.limits.requests_per_minute,
                "Model lease rate limit exceeded"
            );
            lease.active = true;
            lease.request_times.push_back(now);
        }
        let result: Result<String> = async {
            let message = self.generate(context, body.get("options")).await?;
            let output_tokens = message["usage"]["output"].as_u64().unwrap_or(0);
            let mut leases = self.leases.lock().await;
            let lease = leases
                .get_mut(credential)
                .ok_or_else(|| anyhow!("Model lease was revoked"))?;
            ensure!(
                !lease.revoked.load(Ordering::SeqCst),
                "Model lease was revoked"
            );
            ensure!(
                output_tokens <= self.limits.tokens_per_request as u64,
                "Model output token limit exceeded"
            );
            lease.output_tokens = lease.output_tokens.saturating_add(output_tokens);
            ensure!(
                lease.output_tokens <= self.limits.tokens_per_job,
                "Model job token limit exceeded"
            );
            Ok(events(&message))
        }
        .await;
        if let Some(lease) = self.leases.lock().await.get_mut(credential) {
            lease.active = false;
        }
        self.audit(job_id, result.as_ref().map(|_| "ok").unwrap_or("error"))
            .ok();
        result
    }

    async fn generate(&self, context: &Value, options: Option<&Value>) -> Result<Value> {
        if self.descriptor.provider == "anthropic" {
            self.generate_anthropic(context, options).await
        } else {
            self.generate_openai(context, options).await
        }
    }

    async fn generate_anthropic(&self, context: &Value, options: Option<&Value>) -> Result<Value> {
        let converted = convert_anthropic(context)?;
        let url = self
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com/v1/messages".into());
        let mut body = json!({
            "model": self.descriptor.model, "max_tokens": self.limits.tokens_per_request,
            "messages": converted.messages,
        });
        if !converted.system.is_empty() {
            body["system"] = Value::String(converted.system);
        }
        if !converted.tools.is_empty() {
            body["tools"] = Value::Array(converted.tools);
        }
        if let Some(temperature) = options
            .and_then(|value| value.get("temperature"))
            .and_then(Value::as_f64)
        {
            body["temperature"] = json!(temperature);
        }
        let response = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let value: Value = response
            .json()
            .await
            .context("Invalid Anthropic response")?;
        ensure!(status.is_success(), "Model provider request failed");
        let content = value.get("content").and_then(Value::as_array).ok_or_else(|| anyhow!("Invalid Anthropic response"))?.iter().filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => Some(json!({ "type": "text", "text": block.get("text")?.as_str()? })),
            Some("tool_use") => Some(json!({ "type": "toolCall", "id": block.get("id")?.as_str()?, "name": block.get("name")?.as_str()?, "arguments": block.get("input").cloned().unwrap_or_else(|| json!({})) })),
            _ => None,
        }).collect::<Vec<_>>();
        let stop = match value.get("stop_reason").and_then(Value::as_str) {
            Some("tool_use") => "toolUse",
            Some("max_tokens") => "length",
            _ => "stop",
        };
        Ok(assistant(
            &self.descriptor,
            content,
            stop,
            value["usage"]["input_tokens"].as_u64().unwrap_or(0),
            value["usage"]["output_tokens"].as_u64().unwrap_or(0),
        ))
    }

    async fn generate_openai(&self, context: &Value, options: Option<&Value>) -> Result<Value> {
        let converted = convert_openai(context)?;
        let base = self
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        let url = if base.ends_with("/chat/completions") {
            base
        } else {
            format!("{}/chat/completions", base.trim_end_matches('/'))
        };
        let mut body = json!({ "model": self.descriptor.model, "messages": converted.messages, "max_tokens": self.limits.tokens_per_request });
        if !converted.tools.is_empty() {
            body["tools"] = Value::Array(converted.tools);
        }
        if let Some(temperature) = options
            .and_then(|value| value.get("temperature"))
            .and_then(Value::as_f64)
        {
            body["temperature"] = json!(temperature);
        }
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let value: Value = response.json().await.context("Invalid OpenAI response")?;
        ensure!(status.is_success(), "Model provider request failed");
        let choice = value["choices"]
            .as_array()
            .and_then(|choices| choices.first())
            .ok_or_else(|| anyhow!("Invalid OpenAI response"))?;
        let message = &choice["message"];
        let mut content = Vec::new();
        if let Some(text) = message.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            content.push(json!({ "type": "text", "text": text }));
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let arguments = call["function"]["arguments"]
                    .as_str()
                    .and_then(|value| serde_json::from_str(value).ok())
                    .unwrap_or_else(|| json!({}));
                content.push(json!({ "type": "toolCall", "id": call["id"], "name": call["function"]["name"], "arguments": arguments }));
            }
        }
        let stop = match choice.get("finish_reason").and_then(Value::as_str) {
            Some("tool_calls") => "toolUse",
            Some("length") => "length",
            _ => "stop",
        };
        Ok(assistant(
            &self.descriptor,
            content,
            stop,
            value["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            value["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        ))
    }

    fn audit(&self, job_id: &str, outcome: &str) -> Result<()> {
        if let Some(parent) = self.audit.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit)?;
        writeln!(
            file,
            "{}",
            json!({ "timestampUnixMs": now_ms(), "jobId": job_id, "provider": self.descriptor.provider, "model": self.descriptor.model, "outcome": outcome })
        )?;
        Ok(())
    }
}

#[async_trait]
impl WorkerAccessIssuer for ModelProxy {
    async fn issue(&self, spec: &JobSpec) -> Result<Vec<Arc<dyn WorkerLease>>> {
        let credential = Uuid::new_v4().to_string();
        let revoked = Arc::new(AtomicBool::new(false));
        self.leases.lock().await.insert(
            credential.clone(),
            LeaseState {
                job_id: spec.job_id.clone(),
                expires: Instant::now() + self.limits.lease_lifetime,
                revoked: revoked.clone(),
                active: false,
                request_times: VecDeque::new(),
                output_tokens: 0,
            },
        );
        let directory = self
            .socket
            .parent()
            .ok_or_else(|| anyhow!("Model socket has no directory"))?
            .to_owned();
        Ok(vec![Arc::new(ModelLease {
            credential,
            directory,
            socket_name: self
                .socket
                .file_name()
                .ok_or_else(|| anyhow!("Model socket has no name"))?
                .to_owned(),
            descriptor: self.descriptor.worker_json(),
            revoked,
        })])
    }
}

struct ModelLease {
    credential: String,
    directory: PathBuf,
    socket_name: std::ffi::OsString,
    descriptor: String,
    revoked: Arc<AtomicBool>,
}

impl WorkerLease for ModelLease {
    fn environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "POCKET_AGENT_MODEL_SOCKET".into(),
                format!(
                    "/run/pocket-agent-model/{}",
                    self.socket_name.to_string_lossy()
                ),
            ),
            (
                "POCKET_AGENT_MODEL_CREDENTIAL".into(),
                self.credential.clone(),
            ),
            ("POCKET_AGENT_PROXY_MODEL".into(), self.descriptor.clone()),
        ])
    }
    fn mounts(&self) -> Vec<PrivateMount> {
        vec![PrivateMount {
            source: self.directory.clone(),
            destination: PathBuf::from("/run/pocket-agent-model"),
        }]
    }
    fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
    }
}

struct HttpRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

async fn read_http(stream: &mut UnixStream) -> Result<HttpRequest> {
    let mut data = Vec::new();
    let header_end;
    loop {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await?;
        ensure!(read > 0, "Unexpected end of model request");
        data.extend_from_slice(&chunk[..read]);
        ensure!(
            data.len() <= MAX_HEADER_BYTES + MAX_REQUEST_BYTES,
            "Model request is too large"
        );
        if let Some(index) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = index + 4;
            break;
        }
        ensure!(
            data.len() <= MAX_HEADER_BYTES,
            "Model request headers are too large"
        );
    }
    let header = std::str::from_utf8(&data[..header_end])?;
    let mut lines = header.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("Missing HTTP request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow!("Missing method"))?
        .to_owned();
    let path = request_parts
        .next()
        .ok_or_else(|| anyhow!("Missing path"))?
        .to_owned();
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("Malformed header"))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let length: usize = headers
        .get("content-length")
        .ok_or_else(|| anyhow!("Content length is required"))?
        .parse()?;
    ensure!(length <= MAX_REQUEST_BYTES, "Model request is too large");
    while data.len() - header_end < length {
        let mut chunk = [0u8; 8192];
        let read = stream.read(&mut chunk).await?;
        ensure!(read > 0, "Partial model request");
        data.extend_from_slice(&chunk[..read]);
        ensure!(
            data.len() - header_end <= length,
            "Unexpected model request bytes"
        );
    }
    Ok(HttpRequest {
        method,
        path,
        headers,
        body: data[header_end..].to_vec(),
    })
}

async fn write_http(
    stream: &mut UnixStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

struct AnthropicContext {
    system: String,
    messages: Vec<Value>,
    tools: Vec<Value>,
}
struct OpenAiContext {
    messages: Vec<Value>,
    tools: Vec<Value>,
}

fn convert_anthropic(context: &Value) -> Result<AnthropicContext> {
    let messages = context
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Model messages are required"))?;
    let mut system = Vec::new();
    let mut output = Vec::new();
    let mut tools = BTreeMap::new();
    for message in messages {
        update_tools(message, &mut tools);
        match message.get("role").and_then(Value::as_str) {
            Some("system") => system.push(system_text(message)),
            Some("user") => output.push(json!({ "role": "user", "content": anthropic_content(message.get("content").unwrap_or(&Value::Null)) })),
            Some("assistant") => output.push(json!({ "role": "assistant", "content": anthropic_content(message.get("content").unwrap_or(&Value::Null)) })),
            Some("toolResult") => output.push(json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": message["toolCallId"], "content": content_text(&message["content"]), "is_error": message["isError"].as_bool().unwrap_or(false) }] })),
            _ => bail!("Unsupported model message"),
        }
    }
    Ok(AnthropicContext { system: system.join("\n\n"), messages: output, tools: tools.into_values().map(|tool| json!({ "name": tool["name"], "description": tool["description"], "input_schema": tool["parameters"] })).collect() })
}

fn convert_openai(context: &Value) -> Result<OpenAiContext> {
    let messages = context
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Model messages are required"))?;
    let mut output = Vec::new();
    let mut tools = BTreeMap::new();
    for message in messages {
        update_tools(message, &mut tools);
        match message.get("role").and_then(Value::as_str) {
            Some("system") => output.push(json!({ "role": "system", "content": system_text(message) })),
            Some("user") => output.push(json!({ "role": "user", "content": content_text(&message["content"]) })),
            Some("assistant") => {
                let content = message["content"].as_array().cloned().unwrap_or_default();
                let text = content.iter().filter(|part| part["type"] == "text").filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("");
                let calls = content.iter().filter(|part| part["type"] == "toolCall").map(|part| json!({ "id": part["id"], "type": "function", "function": { "name": part["name"], "arguments": part["arguments"].to_string() } })).collect::<Vec<_>>();
                let mut converted = json!({ "role": "assistant", "content": text });
                if !calls.is_empty() { converted["tool_calls"] = Value::Array(calls); }
                output.push(converted);
            }
            Some("toolResult") => output.push(json!({ "role": "tool", "tool_call_id": message["toolCallId"], "content": content_text(&message["content"]) })),
            _ => bail!("Unsupported model message"),
        }
    }
    Ok(OpenAiContext { messages: output, tools: tools.into_values().map(|tool| json!({ "type": "function", "function": { "name": tool["name"], "description": tool["description"], "parameters": tool["parameters"] } })).collect() })
}

fn update_tools(message: &Value, tools: &mut BTreeMap<String, Value>) {
    if let Some(added) = message.get("toolsAdded").and_then(Value::as_array) {
        for tool in added {
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                tools.insert(name.to_owned(), tool.clone());
            }
        }
    }
    if let Some(removed) = message.get("toolsRemoved").and_then(Value::as_array) {
        for tool in removed {
            if let Some(name) = tool.get("name").and_then(Value::as_str) {
                tools.remove(name);
            }
        }
    }
}

fn system_text(message: &Value) -> String {
    let mut sections = vec![content_text(&message["content"])];
    if let Some(values) = message.get("sections").and_then(Value::as_object) {
        sections.extend(values.values().filter_map(Value::as_str).map(str::to_owned));
    }
    sections
        .into_iter()
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn anthropic_content(content: &Value) -> Value {
    if content.is_string() {
        return Value::String(content.as_str().unwrap_or_default().to_owned());
    }
    Value::Array(content.as_array().map(|items| items.iter().filter_map(|part| match part["type"].as_str() {
        Some("text") => Some(json!({ "type": "text", "text": part["text"] })),
        Some("image") => Some(json!({ "type": "image", "source": { "type": "base64", "media_type": part["mimeType"], "data": part["data"] } })),
        Some("toolCall") => Some(json!({ "type": "tool_use", "id": part["id"], "name": part["name"], "input": part["arguments"] })),
        _ => None,
    }).collect()).unwrap_or_default())
}

fn assistant(
    descriptor: &ModelDescriptor,
    content: Vec<Value>,
    stop_reason: &str,
    input: u64,
    output: u64,
) -> Value {
    json!({
        "role": "assistant", "content": content, "api": descriptor.provider, "provider": descriptor.provider,
        "model": descriptor.model, "usage": { "input": input, "output": output, "cacheRead": 0, "cacheWrite": 0,
        "totalTokens": input + output, "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
        "stopReason": stop_reason, "timestamp": now_ms(),
    })
}

fn events(message: &Value) -> String {
    let content = message["content"].as_array().cloned().unwrap_or_default();
    let mut partial = message.clone();
    partial["content"] = Value::Array(Vec::new());
    partial["stopReason"] = Value::String("pending".into());
    let mut events = vec![json!({ "event": { "type": "start", "partial": partial.clone() } })];
    for (index, part) in content.iter().enumerate() {
        if part["type"] == "text" {
            let text = part["text"].as_str().unwrap_or_default();
            let mut start = partial.clone();
            start["content"]
                .as_array_mut()
                .unwrap()
                .push(json!({ "type": "text", "text": "" }));
            events.push(json!({ "event": { "type": "text_start", "contentIndex": index, "partial": start } }));
            let mut delta = partial.clone();
            delta["content"].as_array_mut().unwrap().push(part.clone());
            events.push(json!({ "event": { "type": "text_delta", "contentIndex": index, "delta": text, "partial": delta.clone() } }));
            events.push(json!({ "event": { "type": "text_end", "contentIndex": index, "content": text, "partial": delta } }));
        } else if part["type"] == "toolCall" {
            let mut current = partial.clone();
            current["content"]
                .as_array_mut()
                .unwrap()
                .push(part.clone());
            events.push(json!({ "event": { "type": "toolcall_start", "contentIndex": index, "partial": current.clone() } }));
            events.push(json!({ "event": { "type": "toolcall_end", "contentIndex": index, "toolCall": part, "partial": current } }));
        }
        partial["content"]
            .as_array_mut()
            .unwrap()
            .push(part.clone());
    }
    events.push(
        json!({ "event": { "type": "done", "reason": message["stopReason"], "message": message } }),
    );
    events
        .into_iter()
        .map(|event| format!("{event}\n"))
        .collect()
}

fn public_error(_error: &anyhow::Error) -> &'static str {
    "Model proxy request failed"
}
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_transcripts_without_forwarding_destinations_or_headers() {
        let context = json!({ "messages": [
            { "role": "system", "content": "safe", "toolsAdded": [{ "name": "read", "description": "read", "parameters": { "type": "object" } }] },
            { "role": "user", "content": "hello" },
            { "role": "assistant", "content": [{ "type": "toolCall", "id": "1", "name": "read", "arguments": { "path": "x" } }] },
            { "role": "toolResult", "toolCallId": "1", "toolName": "read", "content": [{ "type": "text", "text": "ok" }], "isError": false }
        ] });
        let anthropic = convert_anthropic(&context).unwrap();
        assert_eq!(anthropic.system, "safe");
        assert_eq!(anthropic.tools.len(), 1);
        assert_eq!(anthropic.messages.len(), 3);
        let openai = convert_openai(&context).unwrap();
        assert_eq!(openai.tools.len(), 1);
        assert_eq!(openai.messages.len(), 4);
    }

    #[test]
    fn emits_a_valid_terminal_event_tunnel() {
        let descriptor = ModelDescriptor {
            provider: "fake".into(),
            model: "model".into(),
            name: "Fake".into(),
            context_window: 4096,
            max_tokens: 256,
            images: false,
        };
        let message = assistant(
            &descriptor,
            vec![json!({ "type": "text", "text": "works" })],
            "stop",
            1,
            1,
        );
        let records = events(&message)
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.first().unwrap()["event"]["type"], "start");
        assert_eq!(records.last().unwrap()["event"]["type"], "done");
    }

    #[tokio::test]
    async fn leases_fix_provider_model_and_redact_audits() {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..1 {
                let (mut stream, _) = server.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let read = stream.read(&mut chunk).await.unwrap();
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let body = json!({
                    "choices": [{ "message": { "content": "proxy works" }, "finish_reason": "stop" }],
                    "usage": { "prompt_tokens": 2, "completion_tokens": 2 }
                }).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let root =
            std::env::temp_dir().join(format!("pocket-agent-model-proxy-{}", Uuid::new_v4()));
        let proxy = ModelProxy::new(
            root.join("model.sock"),
            root.join("audit.ndjson"),
            ModelDescriptor {
                provider: "openai".into(),
                model: "fake".into(),
                name: "Fake".into(),
                context_window: 4096,
                max_tokens: 256,
                images: false,
            },
            "host-provider-secret".into(),
            Some(format!("http://{address}/v1")),
            ModelProxyLimits {
                lease_lifetime: Duration::from_secs(60),
                request_timeout: Duration::from_secs(5),
                requests_per_minute: 2,
                tokens_per_request: 10,
                tokens_per_job: 20,
            },
        )
        .unwrap();
        let spec = JobSpec {
            job_id: "job-a".into(),
            ingress_id: "test".into(),
            principal_id: "user".into(),
            conversation_id: "conversation".into(),
            repository: "app".into(),
        };
        let leases = proxy.issue(&spec).await.unwrap();
        let credential = leases[0].environment()["POCKET_AGENT_MODEL_CREDENTIAL"].clone();
        let request = |body: Value, job: &str| HttpRequest {
            method: "POST".into(),
            path: "/v1/stream".into(),
            headers: BTreeMap::from([
                ("authorization".into(), format!("Bearer {credential}")),
                ("x-pocket-agent-job-id".into(), job.into()),
            ]),
            body: serde_json::to_vec(&body).unwrap(),
        };
        let context = json!({ "messages": [{ "role": "user", "content": "sensitive prompt" }] });
        assert!(
            proxy
                .respond(request(
                    json!({ "provider": "openai", "model": "other", "context": context }),
                    "job-a"
                ))
                .await
                .is_err()
        );
        assert!(proxy.respond(request(json!({ "provider": "openai", "model": "fake", "context": context, "url": "https://attacker.invalid" }), "job-a")).await.is_err());
        assert!(
            proxy
                .respond(request(
                    json!({ "provider": "openai", "model": "fake", "context": context }),
                    "job-b"
                ))
                .await
                .is_err()
        );
        let streamed = proxy
            .respond(request(
                json!({ "provider": "openai", "model": "fake", "context": context }),
                "job-a",
            ))
            .await
            .unwrap();
        assert!(streamed.contains("proxy works"));
        leases[0].revoke();
        assert!(
            proxy
                .respond(request(
                    json!({ "provider": "openai", "model": "fake", "context": context }),
                    "job-a"
                ))
                .await
                .is_err()
        );
        let audit = std::fs::read_to_string(root.join("audit.ndjson")).unwrap();
        assert!(!audit.contains("sensitive prompt"));
        assert!(!audit.contains("host-provider-secret"));
        assert!(!audit.contains(&credential));
        let _ = std::fs::remove_dir_all(root);
    }
}
