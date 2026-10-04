use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use reqwest::{Client, StatusCode, Url, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};
use uuid::Uuid;

use crate::{
    capability::{CapabilityContext, CapabilityDescriptor, CapabilityPolicy, CapabilityProvider},
    config::{ArcadeConnectorConfig, ArcadeToolConfig, Decision},
};

const MCP_VERSION: &str = "2025-06-18";
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "dev.pocket-agent.arcade-oauth";

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, key: &str, value: &[u8]) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

pub struct SystemSecretStore;

impl SystemSecretStore {
    pub fn new() -> Result<Arc<Self>> {
        ensure!(
            cfg!(target_os = "macos"),
            "Arcade Auth currently requires macOS Keychain"
        );
        Ok(Arc::new(Self))
    }
}

#[cfg(target_os = "macos")]
impl SecretStore for SystemSecretStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        use security_framework::passwords::get_generic_password;
        match get_generic_password(KEYCHAIN_SERVICE, key) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.code() == -25300 => Ok(None),
            Err(error) => Err(anyhow!("read Arcade authorization from Keychain: {error}")),
        }
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<()> {
        use security_framework::passwords::set_generic_password;
        set_generic_password(KEYCHAIN_SERVICE, key, value)
            .map_err(|error| anyhow!("store Arcade authorization in Keychain: {error}"))
    }

    fn delete(&self, key: &str) -> Result<()> {
        use security_framework::passwords::delete_generic_password;
        match delete_generic_password(KEYCHAIN_SERVICE, key) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == -25300 => Ok(()),
            Err(error) => Err(anyhow!(
                "delete Arcade authorization from Keychain: {error}"
            )),
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl SecretStore for SystemSecretStore {
    fn get(&self, _key: &str) -> Result<Option<Vec<u8>>> {
        bail!("Arcade Auth currently requires macOS Keychain")
    }
    fn set(&self, _key: &str, _value: &[u8]) -> Result<()> {
        bail!("Arcade Auth currently requires macOS Keychain")
    }
    fn delete(&self, _key: &str) -> Result<()> {
        bail!("Arcade Auth currently requires macOS Keychain")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredGrant {
    resource: String,
    issuer: String,
    token_endpoint: String,
    client_id: String,
    client_secret: Option<String>,
    access_token: String,
    refresh_token: Option<String>,
    expires_at_unix: u64,
}

#[derive(Debug, Deserialize)]
struct ResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

struct OAuthChallenge {
    resource_metadata: String,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizationMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Registration {
    client_id: String,
    client_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

struct McpResponse {
    value: Option<Value>,
    session_id: Option<String>,
}

pub struct ArcadeGatewayProvider {
    endpoint: Url,
    tools: BTreeMap<String, ArcadeToolConfig>,
    client: Client,
    store: Arc<dyn SecretStore>,
    timeout: Duration,
    max_request_bytes: usize,
    max_response_bytes: usize,
    max_calls_per_job: u32,
    calls: Mutex<HashMap<String, u32>>,
    authorization_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ArcadeGatewayProvider {
    pub fn new(config: ArcadeConnectorConfig, store: Arc<dyn SecretStore>) -> Result<Arc<Self>> {
        let endpoint = Url::parse(&format!(
            "https://api.arcade.dev/mcp/{}",
            config.gateway_slug
        ))?;
        let client = Client::builder()
            .user_agent(concat!("pocket-agent/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()?;
        Ok(Arc::new(Self {
            endpoint,
            tools: config
                .tools
                .into_iter()
                .map(|tool| (tool.name.clone(), tool))
                .collect(),
            client,
            store,
            timeout: Duration::from_millis(config.request_timeout_ms),
            max_request_bytes: config.max_request_bytes,
            max_response_bytes: config.max_response_bytes,
            max_calls_per_job: config.max_calls_per_job,
            calls: Mutex::new(HashMap::new()),
            authorization_locks: Mutex::new(HashMap::new()),
        }))
    }

    fn visible(&self, context: &CapabilityContext) -> bool {
        context.ingress_id == "cli"
    }

    fn store_key(&self, context: &CapabilityContext) -> String {
        self.identity_store_key(&context.ingress_id, &context.principal_id)
    }

    fn identity_store_key(&self, ingress: &str, principal: &str) -> String {
        let value = format!("{}\0{}\0{}", self.endpoint, ingress, principal);
        format!("{:x}", Sha256::digest(value.as_bytes()))
    }

    pub fn disconnect(&self, ingress: &str, principal: &str) -> Result<()> {
        self.store
            .delete(&self.identity_store_key(ingress, principal))
    }

    fn authorization_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.authorization_locks
            .lock()
            .expect("Arcade authorization lock")
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn load_grant(&self, key: &str) -> Result<Option<StoredGrant>> {
        self.store
            .get(key)?
            .map(|bytes| serde_json::from_slice(&bytes).context("decode Arcade authorization"))
            .transpose()
    }

    fn save_grant(&self, key: &str, grant: &StoredGrant) -> Result<()> {
        self.store.set(key, &serde_json::to_vec(grant)?)
    }

    async fn access_token(&self, context: &CapabilityContext) -> Result<String> {
        let key = self.store_key(context);
        let lock = self.authorization_lock(&key);
        let _guard = lock.lock().await;
        if let Some(mut grant) = self.load_grant(&key)? {
            ensure!(
                grant.resource == self.endpoint.as_str(),
                "Stored Arcade grant belongs to another gateway"
            );
            ensure_trusted_oauth_url(&grant.issuer)?;
            ensure_trusted_oauth_url(&grant.token_endpoint)?;
            if grant.expires_at_unix > now_unix()? + 60 {
                return Ok(grant.access_token);
            }
            if grant.refresh_token.is_some() {
                match self.refresh(&grant).await {
                    Ok(token) => {
                        grant.access_token = token.access_token;
                        grant.refresh_token = token.refresh_token.or(grant.refresh_token);
                        grant.expires_at_unix = now_unix()? + token.expires_in.unwrap_or(3600);
                        self.save_grant(&key, &grant)?;
                        return Ok(grant.access_token);
                    }
                    Err(error) if error.to_string().contains("token grant was rejected") => {
                        self.store.delete(&key)?
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        let grant = self.authorize(context).await?;
        let access_token = grant.access_token.clone();
        self.save_grant(&key, &grant)?;
        Ok(access_token)
    }

    async fn refresh(&self, grant: &StoredGrant) -> Result<TokenResponse> {
        let mut form = vec![
            ("grant_type", "refresh_token".to_owned()),
            (
                "refresh_token",
                grant.refresh_token.clone().unwrap_or_default(),
            ),
            ("client_id", grant.client_id.clone()),
            ("resource", grant.resource.clone()),
        ];
        if let Some(secret) = &grant.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        self.token_request(&grant.token_endpoint, &form).await
    }

    async fn authorize(&self, context: &CapabilityContext) -> Result<StoredGrant> {
        let oauth_challenge = self.discover_resource_metadata().await?;
        let resource: ResourceMetadata = self
            .get_metadata(&oauth_challenge.resource_metadata)
            .await?;
        ensure!(
            canonical_url(&resource.resource)? == canonical_url(self.endpoint.as_str())?,
            "Arcade resource metadata does not match the configured gateway"
        );
        ensure!(
            resource.authorization_servers.len() == 1,
            "Arcade must advertise exactly one authorization server"
        );
        let issuer = canonical_url(&resource.authorization_servers[0])?;
        ensure_trusted_oauth_url(&issuer)?;
        let authorization = self.authorization_metadata(&issuer).await?;
        ensure!(
            canonical_url(&authorization.issuer)? == issuer,
            "Arcade authorization issuer mismatch"
        );
        ensure!(
            authorization
                .code_challenge_methods_supported
                .iter()
                .any(|method| method == "S256"),
            "Arcade authorization server does not support PKCE S256"
        );
        for url in [
            &authorization.authorization_endpoint,
            &authorization.token_endpoint,
            &authorization.registration_endpoint,
        ] {
            ensure_trusted_oauth_url(url)?;
        }

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let redirect_uri = format!(
            "http://127.0.0.1:{}/callback",
            listener.local_addr()?.port()
        );
        let registration: Registration = self
            .post_json(
                &authorization.registration_endpoint,
                &json!({
                    "client_name": "Pocket Agent",
                    "redirect_uris": [redirect_uri.clone()],
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"],
                    "token_endpoint_auth_method": "none"
                }),
            )
            .await?;

        let verifier = random_base64(32);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_base64(32);
        let mut authorization_url = Url::parse(&authorization.authorization_endpoint)?;
        {
            let mut query = authorization_url.query_pairs_mut();
            query
                .append_pair("response_type", "code")
                .append_pair("client_id", &registration.client_id)
                .append_pair("redirect_uri", &redirect_uri)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256")
                .append_pair("state", &state)
                .append_pair("resource", self.endpoint.as_str());
            let scope = oauth_challenge.scope.or_else(|| {
                (!resource.scopes_supported.is_empty()).then(|| resource.scopes_supported.join(" "))
            });
            if let Some(scope) = scope {
                query.append_pair("scope", &scope);
            }
        }

        let events = context
            .events
            .lock()
            .expect("capability event lock")
            .clone()
            .ok_or_else(|| anyhow!("No active local turn can authorize Arcade"))?;
        events
            .status(&format!(
                "Arcade authorization required. Open this URL in your browser:\n{authorization_url}"
            ))
            .await?;
        let code = timeout(
            self.timeout.max(Duration::from_secs(300)),
            receive_code(listener, &state),
        )
        .await
        .context("Arcade authorization timed out")??;

        let mut form = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", registration.client_id.clone()),
            ("code_verifier", verifier),
            ("resource", self.endpoint.to_string()),
        ];
        if let Some(secret) = &registration.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let token = self
            .token_request(&authorization.token_endpoint, &form)
            .await?;
        Ok(StoredGrant {
            resource: self.endpoint.to_string(),
            issuer,
            token_endpoint: authorization.token_endpoint,
            client_id: registration.client_id,
            client_secret: registration.client_secret,
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at_unix: now_unix()? + token.expires_in.unwrap_or(3600),
        })
    }

    async fn discover_resource_metadata(&self) -> Result<OAuthChallenge> {
        let response = self
            .client
            .post(self.endpoint.clone())
            .header(header::ACCEPT, "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": "oauth-discovery",
                "method": "initialize",
                "params": {
                    "protocolVersion": MCP_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "pocket-agent", "version": env!("CARGO_PKG_VERSION") }
                }
            }))
            .send()
            .await?;
        ensure!(
            response.status() == StatusCode::UNAUTHORIZED,
            "Arcade gateway did not request OAuth authorization"
        );
        let authenticate = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow!("Arcade OAuth challenge is missing"))?;
        let resource_metadata = bearer_parameter(authenticate, "resource_metadata")
            .ok_or_else(|| anyhow!("Arcade OAuth resource metadata is missing"))?;
        ensure_trusted_oauth_url(&resource_metadata)?;
        Ok(OAuthChallenge {
            resource_metadata,
            scope: bearer_parameter(authenticate, "scope"),
        })
    }

    async fn authorization_metadata(&self, issuer: &str) -> Result<AuthorizationMetadata> {
        let issuer = Url::parse(issuer)?;
        let suffix = issuer.path().trim_start_matches('/');
        ensure!(issuer.host_str().is_some(), "OAuth issuer has no host");
        let origin = issuer.origin().ascii_serialization();
        let suffix = if suffix.is_empty() {
            String::new()
        } else {
            format!("/{suffix}")
        };
        let candidates = [
            format!("{origin}/.well-known/oauth-authorization-server{suffix}"),
            format!(
                "{}/.well-known/openid-configuration",
                issuer.as_str().trim_end_matches('/')
            ),
        ];
        for candidate in candidates {
            let response = self.client.get(&candidate).send().await?;
            if response.status().is_success() {
                return bounded_json(response, self.max_response_bytes).await;
            }
        }
        bail!("Arcade authorization metadata discovery failed")
    }

    async fn get_metadata<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let response = self.client.get(url).send().await?;
        ensure!(
            response.status().is_success(),
            "OAuth metadata request failed"
        );
        bounded_json(response, self.max_response_bytes).await
    }

    async fn post_json<T: for<'de> Deserialize<'de>>(&self, url: &str, body: &Value) -> Result<T> {
        let response = self.client.post(url).json(body).send().await?;
        ensure!(
            response.status().is_success(),
            "OAuth client registration failed"
        );
        bounded_json(response, self.max_response_bytes).await
    }

    async fn token_request(&self, url: &str, form: &[(&str, String)]) -> Result<TokenResponse> {
        let pairs = form
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect::<Vec<_>>();
        let response = self.client.post(url).form(&pairs).send().await?;
        if matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED
        ) {
            bail!("Arcade token grant was rejected");
        }
        ensure!(
            response.status().is_success(),
            "Arcade token exchange failed"
        );
        bounded_json(response, self.max_response_bytes).await
    }

    async fn execute(
        &self,
        token: &str,
        tool: &ArcadeToolConfig,
        arguments: &Value,
    ) -> Result<Value> {
        let initialize = self
            .mcp(
                token,
                None,
                json!({
                    "jsonrpc": "2.0", "id": Uuid::new_v4().to_string(), "method": "initialize",
                    "params": { "protocolVersion": MCP_VERSION, "capabilities": {}, "clientInfo": { "name": "pocket-agent", "version": env!("CARGO_PKG_VERSION") } }
                }),
                true,
            )
            .await?;
        let session = initialize.session_id;
        ensure!(
            initialize.value.is_some(),
            "Arcade initialize response is empty"
        );
        self.mcp(
            token,
            session.as_deref(),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            false,
        )
        .await?;
        let listed = self
            .mcp(
                token,
                session.as_deref(),
                json!({ "jsonrpc": "2.0", "id": Uuid::new_v4().to_string(), "method": "tools/list", "params": {} }),
                true,
            )
            .await?
            .value
            .ok_or_else(|| anyhow!("Arcade tools/list response is empty"))?;
        let upstream = listed
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .and_then(|tools| {
                tools
                    .iter()
                    .find(|candidate| candidate["name"] == tool.upstream_name)
            })
            .ok_or_else(|| anyhow!("Configured Arcade tool is unavailable"))?;
        ensure!(
            upstream.get("inputSchema") == Some(&tool.input_schema),
            "Configured Arcade tool schema changed"
        );

        let called = self
            .mcp(
                token,
                session.as_deref(),
                json!({
                    "jsonrpc": "2.0", "id": Uuid::new_v4().to_string(), "method": "tools/call",
                    "params": { "name": tool.upstream_name, "arguments": arguments }
                }),
                true,
            )
            .await?
            .value
            .ok_or_else(|| anyhow!("Arcade tool response is empty"))?;
        let elicitation = called
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| method == "elicitation/create");
        if elicitation
            || called.get("error").is_some()
            || called.pointer("/result/isError") == Some(&Value::Bool(true))
        {
            if let Some(url) = find_arcade_authorization_url(&called) {
                bail!("ARCADE_AUTHORIZATION_REQUIRED:{url}");
            }
            bail!("Arcade tool call failed");
        }
        Ok(called
            .pointer("/result/structuredContent")
            .cloned()
            .or_else(|| called.pointer("/result/content").cloned())
            .unwrap_or(Value::Null))
    }

    async fn mcp(
        &self,
        token: &str,
        session: Option<&str>,
        body: Value,
        expect_body: bool,
    ) -> Result<McpResponse> {
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(token)
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header("MCP-Protocol-Version", MCP_VERSION)
            .json(&body);
        if let Some(session) = session {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request.send().await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            bail!("ARCADE_GATEWAY_UNAUTHORIZED");
        }
        ensure!(response.status().is_success(), "Arcade MCP request failed");
        let session_id = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if !expect_body || response.status() == StatusCode::ACCEPTED {
            return Ok(McpResponse {
                value: None,
                session_id,
            });
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let bytes = bounded_bytes(response, self.max_response_bytes).await?;
        let value = if content_type.starts_with("text/event-stream") {
            parse_sse(&bytes)?
        } else {
            serde_json::from_slice(&bytes).context("decode Arcade MCP response")?
        };
        Ok(McpResponse {
            value: Some(value),
            session_id,
        })
    }
}

#[async_trait]
impl CapabilityProvider for ArcadeGatewayProvider {
    fn descriptors(&self, context: &CapabilityContext) -> Vec<CapabilityDescriptor> {
        if !self.visible(context) {
            return Vec::new();
        }
        self.tools
            .values()
            .filter(|tool| tool.policy != Decision::Deny)
            .map(|tool| CapabilityDescriptor {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                policy: if tool.policy == Decision::Ask {
                    CapabilityPolicy::Ask
                } else {
                    CapabilityPolicy::Allow
                },
            })
            .collect()
    }

    async fn normalize(
        &self,
        _context: &CapabilityContext,
        capability: &str,
        arguments: Value,
    ) -> Result<Value> {
        let tool = self
            .tools
            .get(capability)
            .ok_or_else(|| anyhow!("Unknown Arcade capability"))?;
        validate_schema(&arguments, &tool.input_schema, "arguments")?;
        ensure!(
            serde_json::to_vec(&arguments)?.len() <= self.max_request_bytes,
            "Arcade capability arguments are too large"
        );
        Ok(arguments)
    }

    async fn invoke(
        &self,
        context: &CapabilityContext,
        capability: &str,
        arguments: &Value,
    ) -> Result<Value> {
        ensure!(
            self.visible(context),
            "Arcade is unavailable to this ingress"
        );
        {
            let mut calls = self.calls.lock().expect("Arcade call limit lock");
            let count = calls.entry(context.job_id.clone()).or_default();
            ensure!(
                *count < self.max_calls_per_job,
                "Arcade call limit exceeded"
            );
            *count += 1;
        }
        let tool = self
            .tools
            .get(capability)
            .ok_or_else(|| anyhow!("Unknown Arcade capability"))?;
        let token = self
            .access_token(context)
            .await
            .map_err(|_| anyhow!("Arcade gateway authorization failed"))?;
        match self.execute(&token, tool, arguments).await {
            Err(error)
                if error
                    .to_string()
                    .starts_with("ARCADE_AUTHORIZATION_REQUIRED:") =>
            {
                let url = error
                    .to_string()
                    .trim_start_matches("ARCADE_AUTHORIZATION_REQUIRED:")
                    .to_owned();
                let events = context
                    .events
                    .lock()
                    .expect("capability event lock")
                    .clone()
                    .ok_or_else(|| anyhow!("No active turn can authorize this Arcade tool"))?;
                events
                    .status(&format!(
                        "Arcade tool authorization required. Open this URL, then explicitly retry the operation:\n{url}"
                    ))
                    .await?;
                bail!("Arcade tool authorization is required; retry after authorizing")
            }
            Ok(value) => Ok(value),
            Err(error) if error.to_string() == "ARCADE_GATEWAY_UNAUTHORIZED" => {
                self.store.delete(&self.store_key(context))?;
                bail!("Arcade gateway authorization expired; explicitly retry to reconnect")
            }
            Err(_) => Err(anyhow!("Arcade gateway request failed")),
        }
    }
}

async fn bounded_json<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    limit: usize,
) -> Result<T> {
    let bytes = bounded_bytes(response, limit).await?;
    serde_json::from_slice(&bytes).context("decode OAuth response")
}

async fn bounded_bytes(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        ensure!(length <= limit as u64, "Arcade response is too large");
    }
    let bytes = response.bytes().await?;
    ensure!(bytes.len() <= limit, "Arcade response is too large");
    Ok(bytes.to_vec())
}

async fn receive_code(listener: TcpListener, expected_state: &str) -> Result<String> {
    let (mut stream, _) = listener.accept().await?;
    let mut bytes = vec![0u8; 16 * 1024];
    let read = stream.read(&mut bytes).await?;
    ensure!(read > 0, "Empty OAuth callback");
    let request = std::str::from_utf8(&bytes[..read]).context("Malformed OAuth callback")?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow!("Malformed OAuth callback"))?;
    let callback = Url::parse(&format!("http://127.0.0.1{target}"))?;
    let parameters = callback.query_pairs().collect::<BTreeMap<_, _>>();
    ensure!(
        parameters.get("state").map(|value| value.as_ref()) == Some(expected_state),
        "OAuth state mismatch"
    );
    let code = parameters
        .get("code")
        .map(|value| value.to_string())
        .ok_or_else(|| anyhow!("OAuth callback did not include a code"))?;
    let body = "Arcade authorization completed. You may close this tab.";
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    Ok(code)
}

fn bearer_parameter(header: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=\"");
    let start = header.find(&marker)? + marker.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_owned())
}

fn ensure_trusted_oauth_url(value: &str) -> Result<()> {
    let url = Url::parse(value)?;
    ensure!(url.scheme() == "https", "OAuth endpoint must use HTTPS");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "OAuth URL contains credentials"
    );
    ensure!(url.fragment().is_none(), "OAuth URL contains a fragment");
    let host = url.host_str().unwrap_or_default();
    ensure!(
        host == "arcade.dev" || host.ends_with(".arcade.dev"),
        "OAuth endpoint is outside Arcade"
    );
    Ok(())
}

fn canonical_url(value: &str) -> Result<String> {
    let mut url = Url::parse(value)?;
    url.set_fragment(None);
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

fn random_base64(bytes: usize) -> String {
    let mut value = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}

fn now_unix() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn parse_sse(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes).context("Arcade SSE is not UTF-8")?;
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            return serde_json::from_str(data.trim()).context("decode Arcade SSE data");
        }
    }
    bail!("Arcade SSE response has no data event")
}

fn find_arcade_authorization_url(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => text.split_whitespace().find_map(|word| {
            let candidate = word.trim_matches(|character: char| {
                matches!(character, '"' | '\'' | '(' | ')' | '<' | '>' | ',' | '.')
            });
            ensure_trusted_oauth_url(candidate)
                .ok()
                .map(|_| candidate.to_owned())
        }),
        Value::Array(values) => values.iter().find_map(find_arcade_authorization_url),
        Value::Object(values) => values.values().find_map(find_arcade_authorization_url),
        _ => None,
    }
}

fn validate_schema(value: &Value, schema: &Value, path: &str) -> Result<()> {
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("{path} must be an object"))?;
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                ensure!(
                    object.keys().all(|key| properties.contains_key(key)),
                    "{path} contains unknown fields"
                );
            }
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    ensure!(object.contains_key(name), "{path}.{name} is required");
                }
            }
            for (name, child) in object {
                if let Some(child_schema) = properties.get(name) {
                    validate_schema(child, child_schema, &format!("{path}.{name}"))?;
                }
            }
        }
        Some("string") => {
            let string = value
                .as_str()
                .ok_or_else(|| anyhow!("{path} must be a string"))?;
            if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64) {
                ensure!(
                    string.chars().count() <= maximum as usize,
                    "{path} is too long"
                );
            }
        }
        Some("integer") => {
            let integer = value
                .as_i64()
                .ok_or_else(|| anyhow!("{path} must be an integer"))?;
            if let Some(minimum) = schema.get("minimum").and_then(Value::as_i64) {
                ensure!(integer >= minimum, "{path} is below its minimum");
            }
        }
        Some("number") => ensure!(value.is_number(), "{path} must be a number"),
        Some("boolean") => ensure!(value.is_boolean(), "{path} must be a boolean"),
        Some("array") => {
            let values = value
                .as_array()
                .ok_or_else(|| anyhow!("{path} must be an array"))?;
            if let Some(items) = schema.get("items") {
                for (index, child) in values.iter().enumerate() {
                    validate_schema(child, items, &format!("{path}[{index}]"))?;
                }
            }
        }
        Some(other) => bail!("Unsupported schema type {other}"),
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MemoryStore(Mutex<HashMap<String, Vec<u8>>>);
    impl SecretStore for MemoryStore {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, value: &[u8]) -> Result<()> {
            self.0.lock().unwrap().insert(key.into(), value.into());
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<()> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    fn config() -> ArcadeConnectorConfig {
        ArcadeConnectorConfig {
            gateway_slug: "test-gateway".into(),
            request_timeout_ms: 30_000,
            max_calls_per_job: 3,
            max_request_bytes: 1024,
            max_response_bytes: 4096,
            tools: vec![ArcadeToolConfig {
                name: "arcade.issue".into(),
                upstream_name: "GitHub.GetIssue".into(),
                description: "Read one issue".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "number": { "type": "integer", "minimum": 1 } },
                    "required": ["number"],
                    "additionalProperties": false
                }),
                policy: Decision::Allow,
            }],
        }
    }

    fn context(ingress: &str) -> CapabilityContext {
        CapabilityContext {
            job_id: "job".into(),
            ingress_id: ingress.into(),
            principal_id: "principal".into(),
            conversation_id: "conversation".into(),
            repository: "app".into(),
            events: Arc::new(Mutex::new(None)),
        }
    }

    #[tokio::test]
    async fn exposes_only_curated_tools_to_local_jobs_and_normalizes_arguments() {
        let provider =
            ArcadeGatewayProvider::new(config(), Arc::new(MemoryStore::default())).unwrap();
        assert_eq!(
            provider.descriptors(&context("cli"))[0].name,
            "arcade.issue"
        );
        assert!(provider.descriptors(&context("signal")).is_empty());
        assert!(
            provider
                .normalize(&context("cli"), "arcade.issue", json!({"number": 1}))
                .await
                .is_ok()
        );
        assert!(
            provider
                .normalize(&context("cli"), "arcade.issue", json!({"number": 0}))
                .await
                .is_err()
        );
        assert!(
            provider
                .normalize(
                    &context("cli"),
                    "arcade.issue",
                    json!({"number": 1, "host": "evil"})
                )
                .await
                .is_err()
        );
    }

    #[test]
    fn validates_closed_tool_arguments_and_redacts_non_arcade_urls() {
        let schema = json!({
            "type": "object",
            "properties": { "owner": { "type": "string", "maxLength": 5 }, "number": { "type": "integer", "minimum": 1 } },
            "required": ["owner", "number"],
            "additionalProperties": false
        });
        validate_schema(&json!({"owner":"acme","number":1}), &schema, "arguments").unwrap();
        assert!(
            validate_schema(&json!({"owner":"acme","number":0}), &schema, "arguments").is_err()
        );
        assert!(
            validate_schema(
                &json!({"owner":"acme","number":1,"url":"https://evil.test"}),
                &schema,
                "arguments"
            )
            .is_err()
        );
        assert_eq!(
            find_arcade_authorization_url(&json!({"url":"https://cloud.arcade.dev/auth/x"}))
                .as_deref(),
            Some("https://cloud.arcade.dev/auth/x")
        );
        assert!(find_arcade_authorization_url(&json!({"url":"https://evil.test/auth"})).is_none());
        let store = MemoryStore::default();
        store.set("one", b"secret").unwrap();
        assert_eq!(store.get("one").unwrap().unwrap(), b"secret");
        store.delete("one").unwrap();
        assert!(store.get("one").unwrap().is_none());
    }
}
