use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{
    domain::{ApprovalRequest, JobSpec},
    ports::{JobEventPort, PrivateMount, WorkerAccessIssuer, WorkerLease},
    workspace::{WorkspaceManager, WorkspacePatchStatus},
};

const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MCP_VERSION: &str = "2025-06-18";
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityPolicy {
    Allow,
    Ask,
}

#[derive(Clone, Debug)]
pub struct CapabilityDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub policy: CapabilityPolicy,
}

#[derive(Clone)]
pub struct CapabilityContext {
    pub job_id: String,
    pub ingress_id: String,
    pub principal_id: String,
    pub conversation_id: String,
    pub repository: String,
    pub events: Arc<Mutex<Option<Arc<dyn JobEventPort>>>>,
}

#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    fn descriptors(&self, context: &CapabilityContext) -> Vec<CapabilityDescriptor>;
    async fn normalize(
        &self,
        context: &CapabilityContext,
        capability: &str,
        arguments: Value,
    ) -> Result<Value>;
    async fn invoke(
        &self,
        context: &CapabilityContext,
        capability: &str,
        arguments: &Value,
    ) -> Result<Value>;
}

struct WorkspaceProvider {
    workspaces: WorkspaceManager,
}

#[derive(Clone, Debug)]
pub struct CapabilityLimits {
    pub lease_lifetime: Duration,
    pub calls_per_job: usize,
    pub output_bytes: usize,
}

impl Default for CapabilityLimits {
    fn default() -> Self {
        Self {
            lease_lifetime: Duration::from_secs(60 * 60),
            calls_per_job: 100,
            output_bytes: 6 * 1024 * 1024,
        }
    }
}

struct LeaseState {
    job_id: String,
    ingress_id: String,
    principal_id: String,
    conversation_id: String,
    repository: String,
    expires_at_ms: u128,
    calls: usize,
    request_ids: BTreeSet<String>,
    events: Arc<Mutex<Option<Arc<dyn JobEventPort>>>>,
}

pub struct CapabilityBroker {
    socket: PathBuf,
    audit: PathBuf,
    providers: Vec<Arc<dyn CapabilityProvider>>,
    limits: CapabilityLimits,
    leases: Arc<Mutex<HashMap<String, LeaseState>>>,
    started: AtomicBool,
    server: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl CapabilityBroker {
    pub fn new(
        socket: PathBuf,
        audit: PathBuf,
        workspaces: WorkspaceManager,
        limits: CapabilityLimits,
    ) -> Result<Arc<Self>> {
        Self::with_providers(socket, audit, workspaces, limits, Vec::new())
    }

    pub fn with_providers(
        socket: PathBuf,
        audit: PathBuf,
        workspaces: WorkspaceManager,
        limits: CapabilityLimits,
        mut additional_providers: Vec<Arc<dyn CapabilityProvider>>,
    ) -> Result<Arc<Self>> {
        ensure!(
            socket.is_absolute() && audit.is_absolute(),
            "Capability paths must be absolute"
        );
        ensure!(
            limits.lease_lifetime > Duration::ZERO
                && limits.calls_per_job > 0
                && limits.output_bytes > 0,
            "Capability limits must be positive"
        );
        let mut providers: Vec<Arc<dyn CapabilityProvider>> =
            vec![Arc::new(WorkspaceProvider { workspaces })];
        providers.append(&mut additional_providers);
        Ok(Arc::new(Self {
            socket,
            audit,
            providers,
            limits,
            leases: Arc::new(Mutex::new(HashMap::new())),
            started: AtomicBool::new(false),
            server: tokio::sync::Mutex::new(None),
        }))
    }

    pub async fn start(self: &Arc<Self>) -> Result<()> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let result = async {
            let parent = self
                .socket
                .parent()
                .ok_or_else(|| anyhow!("Capability socket has no parent"))?;
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o711))?;
            if let Some(parent) = self.audit.parent() {
                fs::create_dir_all(parent)?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
            let _ = fs::remove_file(&self.socket);
            let listener = UnixListener::bind(&self.socket)?;
            fs::set_permissions(&self.socket, fs::Permissions::from_mode(0o666))?;
            let broker = self.clone();
            *self.server.lock().await = Some(tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let broker = broker.clone();
                    tokio::spawn(async move {
                        let _ = broker.serve(stream).await;
                    });
                }
            }));
            Ok(())
        }
        .await;
        if result.is_err() {
            self.started.store(false, Ordering::SeqCst);
        }
        result
    }

    pub async fn close(&self) {
        self.started.store(false, Ordering::SeqCst);
        self.leases.lock().expect("capability lease lock").clear();
        if let Some(server) = self.server.lock().await.take() {
            server.abort();
        }
        let _ = fs::remove_file(&self.socket);
    }

    async fn serve(&self, mut stream: UnixStream) -> Result<()> {
        let result = match read_http_request(&mut stream).await {
            Ok(request) => self.dispatch(request).await,
            Err(error) => Err(error),
        };
        let response = match result {
            Ok(value) => http_json(200, value),
            Err(error) => http_json(
                400,
                json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32000, "message": safe_error(&error) } }),
            ),
        };
        stream.write_all(&response).await?;
        Ok(())
    }

    async fn dispatch(&self, request: HttpRequest) -> Result<Value> {
        ensure!(
            request.method == "POST" && request.path == "/mcp",
            "Unsupported MCP endpoint"
        );
        let credential = parse_bearer(request.headers.get("authorization"))?;
        let rpc: Value = serde_json::from_slice(&request.body).context("Malformed JSON-RPC")?;
        ensure!(
            rpc["jsonrpc"] == "2.0" && rpc["method"].is_string(),
            "Malformed JSON-RPC"
        );
        let id = rpc.get("id").cloned().unwrap_or(Value::Null);
        let params = rpc.get("params").and_then(Value::as_object);
        let meta = params
            .and_then(|value| value.get("_meta"))
            .and_then(Value::as_object);
        let job_id = meta
            .and_then(|value| value.get("pocket-agent/job-id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match rpc["method"].as_str().unwrap_or_default() {
            "initialize" => {
                self.authenticate(&credential, job_id)?;
                Ok(json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": MCP_VERSION,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "pocket-agent-capability-broker", "version": env!("CARGO_PKG_VERSION") }
                }}))
            }
            "notifications/initialized" => {
                self.authenticate(&credential, job_id)?;
                Ok(json!({ "jsonrpc": "2.0", "id": id, "result": {} }))
            }
            "tools/list" => {
                let context = self.capability_context(&credential, job_id)?;
                let descriptors = self.descriptors(&context)?;
                Ok(
                    json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": descriptors.into_iter().map(descriptor_json).collect::<Vec<_>>() } }),
                )
            }
            "tools/call" => {
                let params = params.ok_or_else(|| anyhow!("Missing capability parameters"))?;
                let request_id = meta
                    .and_then(|value| value.get("pocket-agent/request-id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let result = self
                    .call(&credential, job_id, request_id, name, arguments)
                    .await?;
                Ok(json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "content": [{ "type": "text", "text": serde_json::to_string(&result)? }],
                    "structuredContent": result, "isError": false
                }}))
            }
            _ => Ok(
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Method not found" } }),
            ),
        }
    }

    async fn call(
        &self,
        credential: &str,
        job_id: &str,
        request_id: &str,
        name: &str,
        arguments: Value,
    ) -> Result<Value> {
        ensure!(
            !request_id.is_empty() && request_id.len() <= 256,
            "Request ID is invalid"
        );
        let key = credential_hash(credential);
        let context = {
            let mut leases = self.leases.lock().expect("capability lease lock");
            let lease = authenticate_locked(&mut leases, &key, job_id)?;
            ensure!(
                lease.calls < self.limits.calls_per_job,
                "Capability call limit exceeded"
            );
            ensure!(
                lease.request_ids.insert(request_id.to_owned()),
                "Request ID was already used"
            );
            lease.calls += 1;
            context_from_lease(lease)
        };
        let Some((provider, descriptor)) = self.provider_for(&context, name)? else {
            self.audit(job_id, &context.repository, name, None, "forbidden")?;
            bail!("Capability is outside this job scope");
        };
        let normalized = match provider.normalize(&context, name, arguments).await {
            Ok(normalized) => normalized,
            Err(error) => {
                self.audit(job_id, &context.repository, name, None, "invalid_arguments")?;
                return Err(error);
            }
        };
        let digest = canonical_digest(&normalized);
        if descriptor.policy == CapabilityPolicy::Ask {
            let events = context
                .events
                .lock()
                .expect("capability event lock")
                .clone()
                .ok_or_else(|| anyhow!("No active turn can approve this capability"))?;
            let approval = ApprovalRequest {
                title: format!("Allow capability {name}?"),
                detail: format!(
                    "Repository: {}\nArguments: sha256:{digest}\nConversation: {}",
                    context.repository, context.conversation_id
                ),
                choices: vec!["yes".into(), "no".into()],
            };
            let answer = events.request_approval(approval).await?;
            self.authenticate(credential, job_id)?;
            if answer != "yes" {
                self.audit(job_id, &context.repository, name, Some(&digest), "denied")?;
                bail!("Capability was denied by operator");
            }
        }
        let outcome = provider.invoke(&context, name, &normalized).await;
        match outcome {
            Ok(value) => {
                ensure!(
                    serde_json::to_vec(&value)?.len() <= self.limits.output_bytes,
                    "Capability output limit exceeded"
                );
                self.audit(job_id, &context.repository, name, Some(&digest), "allowed")?;
                Ok(value)
            }
            Err(error) => {
                self.audit(job_id, &context.repository, name, Some(&digest), "failed")?;
                Err(error)
            }
        }
    }

    fn descriptors(&self, context: &CapabilityContext) -> Result<Vec<CapabilityDescriptor>> {
        let mut names = BTreeSet::new();
        let mut descriptors = Vec::new();
        for provider in &self.providers {
            for descriptor in provider.descriptors(context) {
                ensure!(
                    names.insert(descriptor.name.clone()),
                    "Duplicate capability name"
                );
                descriptors.push(descriptor);
            }
        }
        Ok(descriptors)
    }

    fn provider_for(
        &self,
        context: &CapabilityContext,
        name: &str,
    ) -> Result<Option<(Arc<dyn CapabilityProvider>, CapabilityDescriptor)>> {
        let mut found = None;
        for provider in &self.providers {
            if let Some(descriptor) = provider
                .descriptors(context)
                .into_iter()
                .find(|descriptor| descriptor.name == name)
            {
                ensure!(found.is_none(), "Duplicate capability name");
                found = Some((provider.clone(), descriptor));
            }
        }
        Ok(found)
    }

    fn capability_context(&self, credential: &str, job_id: &str) -> Result<CapabilityContext> {
        let mut leases = self.leases.lock().expect("capability lease lock");
        Ok(context_from_lease(authenticate_locked(
            &mut leases,
            &credential_hash(credential),
            job_id,
        )?))
    }

    fn authenticate(&self, credential: &str, job_id: &str) -> Result<()> {
        authenticate_locked(
            &mut self.leases.lock().expect("capability lease lock"),
            &credential_hash(credential),
            job_id,
        )
        .map(|_| ())
    }

    fn audit(
        &self,
        job_id: &str,
        repository: &str,
        tool: &str,
        digest: Option<&str>,
        outcome: &str,
    ) -> Result<()> {
        if let Some(parent) = self.audit.parent() {
            fs::create_dir_all(parent)?;
        }
        let record = json!({
            "timestampUnixMs": now_ms()?, "jobId": job_id, "repositoryScope": repository,
            "tool": tool, "argumentDigest": digest.map(|value| format!("sha256:{value}")), "outcome": outcome
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.audit)?;
        writeln!(file, "{record}")?;
        fs::set_permissions(&self.audit, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
}

#[async_trait]
impl WorkerAccessIssuer for CapabilityBroker {
    async fn issue(&self, spec: &JobSpec) -> Result<Vec<Arc<dyn WorkerLease>>> {
        ensure!(
            self.started.load(Ordering::SeqCst),
            "Capability broker is not started"
        );
        let credential = Uuid::new_v4().to_string();
        let key = credential_hash(&credential);
        let events = Arc::new(Mutex::new(None));
        self.leases.lock().expect("capability lease lock").insert(
            key.clone(),
            LeaseState {
                job_id: spec.job_id.clone(),
                ingress_id: spec.ingress_id.clone(),
                principal_id: spec.principal_id.clone(),
                conversation_id: spec.conversation_id.clone(),
                repository: spec.repository.clone(),
                expires_at_ms: now_ms()? + self.limits.lease_lifetime.as_millis(),
                calls: 0,
                request_ids: BTreeSet::new(),
                events: events.clone(),
            },
        );
        Ok(vec![Arc::new(CapabilityLease {
            leases: self.leases.clone(),
            key,
            credential,
            socket: self.socket.clone(),
            events,
            revoked: AtomicBool::new(false),
        })])
    }
}

struct CapabilityLease {
    leases: Arc<Mutex<HashMap<String, LeaseState>>>,
    key: String,
    credential: String,
    socket: PathBuf,
    events: Arc<Mutex<Option<Arc<dyn JobEventPort>>>>,
    revoked: AtomicBool,
}

impl WorkerLease for CapabilityLease {
    fn environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "POCKET_AGENT_MCP_SOCKET".into(),
                "/run/pocket-agent-broker/broker.sock".into(),
            ),
            (
                "POCKET_AGENT_MCP_CREDENTIAL".into(),
                self.credential.clone(),
            ),
        ])
    }
    fn mounts(&self) -> Vec<PrivateMount> {
        vec![PrivateMount {
            source: self.socket.parent().expect("socket parent").to_owned(),
            destination: PathBuf::from("/run/pocket-agent-broker"),
        }]
    }
    fn set_events(&self, events: Option<Arc<dyn JobEventPort>>) {
        *self.events.lock().expect("capability event lock") = events;
    }
    fn revoke(&self) {
        if !self.revoked.swap(true, Ordering::SeqCst) {
            self.leases
                .lock()
                .expect("capability lease lock")
                .remove(&self.key);
        }
    }
}

fn authenticate_locked<'a>(
    leases: &'a mut HashMap<String, LeaseState>,
    key: &str,
    job_id: &str,
) -> Result<&'a mut LeaseState> {
    let expired = leases
        .get(key)
        .is_some_and(|lease| lease.expires_at_ms <= now_ms().unwrap_or(u128::MAX));
    if expired {
        leases.remove(key);
        bail!("Capability credential expired");
    }
    let lease = leases
        .get_mut(key)
        .ok_or_else(|| anyhow!("Capability credential is invalid or revoked"))?;
    ensure!(
        lease.job_id == job_id,
        "Capability credential belongs to another job"
    );
    Ok(lease)
}

#[async_trait]
impl CapabilityProvider for WorkspaceProvider {
    fn descriptors(&self, _context: &CapabilityContext) -> Vec<CapabilityDescriptor> {
        vec![
            descriptor(
                "workspace.read_metadata",
                "Read bounded metadata for the authenticated job workspace.",
                json!({ "type": "object", "additionalProperties": false }),
                CapabilityPolicy::Allow,
            ),
            descriptor(
                "workspace.submit_patch",
                "Validate and submit a candidate unified Git patch for the authenticated job.",
                json!({ "type": "object", "properties": { "patch": { "type": "string" } }, "required": ["patch"], "additionalProperties": false }),
                CapabilityPolicy::Allow,
            ),
            descriptor(
                "workspace.get_patch_status",
                "Read the current candidate patch status for the authenticated job.",
                json!({ "type": "object", "additionalProperties": false }),
                CapabilityPolicy::Allow,
            ),
            descriptor(
                "workspace.apply_patch",
                "Apply one previously submitted candidate patch to its configured repository.",
                json!({ "type": "object", "properties": { "patchId": { "type": "string", "pattern": "^[a-f0-9]{64}$" } }, "required": ["patchId"], "additionalProperties": false }),
                CapabilityPolicy::Ask,
            ),
        ]
    }

    async fn normalize(
        &self,
        _context: &CapabilityContext,
        name: &str,
        arguments: Value,
    ) -> Result<Value> {
        let object = arguments
            .as_object()
            .ok_or_else(|| anyhow!("arguments must be an object"))?;
        match name {
            "workspace.read_metadata" | "workspace.get_patch_status" => ensure!(
                object.is_empty(),
                "arguments contain missing or unknown fields"
            ),
            "workspace.submit_patch" => ensure!(
                object.len() == 1 && object.get("patch").and_then(Value::as_str).is_some(),
                "patch must be a string"
            ),
            "workspace.apply_patch" => {
                let id = object
                    .get("patchId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                ensure!(
                    object.len() == 1
                        && id.len() == 64
                        && id
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                    "patchId must be a SHA-256 identifier"
                );
            }
            _ => bail!("Capability is outside this job scope"),
        }
        Ok(arguments)
    }

    async fn invoke(
        &self,
        context: &CapabilityContext,
        name: &str,
        arguments: &Value,
    ) -> Result<Value> {
        match name {
            "workspace.read_metadata" => self
                .workspaces
                .read_metadata(&context.job_id, &context.repository),
            "workspace.submit_patch" => status_json(self.workspaces.submit_patch(
                &context.job_id,
                &context.repository,
                arguments["patch"].as_str().unwrap_or_default(),
            )?),
            "workspace.get_patch_status" => status_json(
                self.workspaces
                    .patch_status(&context.job_id, &context.repository)?,
            ),
            "workspace.apply_patch" => status_json(self.workspaces.apply_patch(
                &context.job_id,
                &context.repository,
                arguments["patchId"].as_str().unwrap_or_default(),
            )?),
            _ => bail!("Capability is outside this job scope"),
        }
    }
}

fn descriptor(
    name: &str,
    description: &str,
    input_schema: Value,
    policy: CapabilityPolicy,
) -> CapabilityDescriptor {
    CapabilityDescriptor {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
        policy,
    }
}

fn descriptor_json(descriptor: CapabilityDescriptor) -> Value {
    json!({
        "name": descriptor.name,
        "description": descriptor.description,
        "inputSchema": descriptor.input_schema,
    })
}

fn context_from_lease(lease: &LeaseState) -> CapabilityContext {
    CapabilityContext {
        job_id: lease.job_id.clone(),
        ingress_id: lease.ingress_id.clone(),
        principal_id: lease.principal_id.clone(),
        conversation_id: lease.conversation_id.clone(),
        repository: lease.repository.clone(),
        events: lease.events.clone(),
    }
}

fn status_json(status: WorkspacePatchStatus) -> Result<Value> {
    Ok(json!({
        "state": status.state, "patchId": status.patch_id, "files": status.files,
        "bytes": status.bytes, "patch": status.patch, "appliedAtUnixMs": status.applied_at_unix_ms
    }))
}

fn credential_hash(credential: &str) -> String {
    format!("{:x}", Sha256::digest(credential.as_bytes()))
}
fn canonical_digest(value: &Value) -> String {
    format!("{:x}", Sha256::digest(canonical_json(value).as_bytes()))
}
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        canonical_json(&map[key])
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => value.to_string(),
    }
}
fn now_ms() -> Result<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}
fn safe_error(error: &anyhow::Error) -> String {
    error.to_string().chars().take(1024).collect()
}

struct HttpRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
async fn read_http_request(stream: &mut UnixStream) -> Result<HttpRequest> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end;
    loop {
        let read = stream.read(&mut chunk).await?;
        ensure!(read > 0, "Unexpected end of MCP request");
        bytes.extend_from_slice(&chunk[..read]);
        ensure!(bytes.len() <= MAX_REQUEST_BYTES, "MCP request is too large");
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = end;
            break;
        }
    }
    let header = std::str::from_utf8(&bytes[..header_end]).context("Malformed MCP headers")?;
    let mut lines = header.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .ok_or_else(|| anyhow!("Missing content length"))?
        .parse::<usize>()?;
    ensure!(length <= MAX_REQUEST_BYTES, "MCP request is too large");
    let body_start = header_end + 4;
    while bytes.len() < body_start + length {
        let read = stream.read(&mut chunk).await?;
        ensure!(read > 0, "Partial MCP request");
        bytes.extend_from_slice(&chunk[..read]);
        ensure!(
            bytes.len() <= body_start + MAX_REQUEST_BYTES,
            "MCP request is too large"
        );
    }
    ensure!(
        bytes.len() == body_start + length,
        "Unexpected MCP request bytes"
    );
    Ok(HttpRequest {
        method,
        path,
        headers,
        body: bytes[body_start..].to_vec(),
    })
}
fn parse_bearer(value: Option<&String>) -> Result<String> {
    let value = value.ok_or_else(|| anyhow!("Missing capability credential"))?;
    ensure!(
        value.starts_with("Bearer ") && value.len() <= 1024,
        "Missing capability credential"
    );
    Ok(value[7..].to_owned())
}
fn http_json(status: u16, value: Value) -> Vec<u8> {
    let body = value.to_string();
    format!("HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::JobEventPort;
    use std::{
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Approvals(AtomicUsize);
    #[async_trait]
    impl JobEventPort for Approvals {
        async fn status(&self, _message: &str) -> Result<()> {
            Ok(())
        }
        async fn request_approval(&self, request: ApprovalRequest) -> Result<String> {
            assert!(request.detail.contains("sha256:"));
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("yes".into())
        }
    }

    fn git(path: &std::path::Path, arguments: &[&str]) {
        assert!(
            Command::new("git")
                .args(arguments)
                .current_dir(path)
                .status()
                .unwrap()
                .success()
        );
    }

    #[tokio::test]
    async fn scopes_replays_approvals_revocation_and_audits() {
        let root = PathBuf::from("/tmp").join(format!("pa-cap-{}", Uuid::new_v4()));
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "--quiet"]);
        git(&source, &["config", "user.name", "Test"]);
        git(&source, &["config", "user.email", "test@example.com"]);
        fs::write(source.join("file.txt"), "before\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "--quiet", "-m", "baseline"]);
        let workspaces = WorkspaceManager::new(
            root.join("workspaces"),
            BTreeMap::from([("app".into(), source.clone())]),
            Default::default(),
        )
        .unwrap();
        let mut workspace = workspaces.create("job-a", "app").unwrap();
        let audit = root.join("audit/capabilities.ndjson");
        let broker = CapabilityBroker::new(
            root.join("broker/broker.sock"),
            audit.clone(),
            workspaces,
            Default::default(),
        )
        .unwrap();
        broker.start().await.unwrap();
        let spec = JobSpec {
            job_id: "job-a".into(),
            ingress_id: "test".into(),
            principal_id: "user".into(),
            conversation_id: "conversation".into(),
            repository: "app".into(),
        };
        let leases = broker.issue(&spec).await.unwrap();
        let credential = leases[0].environment()["POCKET_AGENT_MCP_CREDENTIAL"].clone();
        let approvals = Arc::new(Approvals(AtomicUsize::new(0)));
        leases[0].set_events(Some(approvals.clone()));
        assert!(
            broker
                .call(
                    &credential,
                    "other-job",
                    "cross",
                    "workspace.read_metadata",
                    json!({})
                )
                .await
                .is_err()
        );
        let patch = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n";
        let submitted = broker
            .call(
                &credential,
                "job-a",
                "submit",
                "workspace.submit_patch",
                json!({ "patch": patch }),
            )
            .await
            .unwrap();
        let patch_id = submitted["patchId"].as_str().unwrap().to_owned();
        assert!(
            broker
                .call(
                    &credential,
                    "job-a",
                    "submit",
                    "workspace.submit_patch",
                    json!({ "patch": "mutated" })
                )
                .await
                .is_err()
        );
        assert!(
            broker
                .call(&credential, "job-a", "forbidden", "host.exec", json!({}))
                .await
                .is_err()
        );
        broker
            .call(
                &credential,
                "job-a",
                "apply",
                "workspace.apply_patch",
                json!({ "patchId": patch_id }),
            )
            .await
            .unwrap();
        assert_eq!(approvals.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read_to_string(source.join("file.txt")).unwrap(),
            "after\n"
        );
        let expired = broker.issue(&spec).await.unwrap();
        let expired_credential = expired[0].environment()["POCKET_AGENT_MCP_CREDENTIAL"].clone();
        broker
            .leases
            .lock()
            .unwrap()
            .get_mut(&credential_hash(&expired_credential))
            .unwrap()
            .expires_at_ms = 0;
        assert!(broker.authenticate(&expired_credential, "job-a").is_err());
        leases[0].revoke();
        assert!(broker.authenticate(&credential, "job-a").is_err());
        let audit_text = fs::read_to_string(audit).unwrap();
        assert!(audit_text.contains("workspace.apply_patch"));
        assert!(audit_text.contains("host.exec"));
        assert!(!audit_text.contains(&credential));
        assert!(!audit_text.contains("before"));
        workspace.dispose().unwrap();
        broker.close().await;
        let _ = fs::remove_dir_all(root);
    }
}
