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
const TRUSTED_ARCADE_HOSTS: [&str; 3] = ["api.arcade.dev", "auth.arcade.dev", "cloud.arcade.dev"];
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

#[derive(Debug)]
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
    validated_grants: Mutex<std::collections::BTreeSet<String>>,
}

impl ArcadeGatewayProvider {
    pub fn new(config: ArcadeConnectorConfig, store: Arc<dyn SecretStore>) -> Result<Arc<Self>> {
        let endpoint = Url::parse(&format!(
            "https://api.arcade.dev/mcp/{}",
            config.gateway_slug
        ))?;
        Self::with_endpoint(config, store, endpoint)
    }

    fn with_endpoint(
        config: ArcadeConnectorConfig,
        store: Arc<dyn SecretStore>,
        endpoint: Url,
    ) -> Result<Arc<Self>> {
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
            validated_grants: Mutex::new(std::collections::BTreeSet::new()),
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

    async fn access_token(&self, context: &CapabilityContext, capability: &str) -> Result<String> {
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
            let validated = self
                .validated_grants
                .lock()
                .expect("Arcade validated grant lock")
                .contains(&key);
            if !validated {
                if let Err(error) = self.validate_stored_grant(&grant).await {
                    self.store.delete(&key)?;
                    return Err(error.context("Stored Arcade grant metadata changed"));
                }
                self.validated_grants
                    .lock()
                    .expect("Arcade validated grant lock")
                    .insert(key.clone());
            }
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
                        self.store.delete(&key)?;
                        self.validated_grants
                            .lock()
                            .expect("Arcade validated grant lock")
                            .remove(&key);
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        let grant = self.authorize(context, capability).await?;
        let access_token = grant.access_token.clone();
        self.save_grant(&key, &grant)?;
        self.validated_grants
            .lock()
            .expect("Arcade validated grant lock")
            .insert(key);
        Ok(access_token)
    }

    async fn validate_stored_grant(&self, grant: &StoredGrant) -> Result<()> {
        let oauth_challenge = self.discover_resource_metadata().await?;
        let resource: ResourceMetadata = self
            .get_metadata(&oauth_challenge.resource_metadata)
            .await?;
        ensure!(
            canonical_url(&resource.resource)? == canonical_url(&grant.resource)?,
            "Arcade resource metadata changed"
        );
        ensure!(
            resource.authorization_servers.len() == 1
                && canonical_url(&resource.authorization_servers[0])?
                    == canonical_url(&grant.issuer)?,
            "Arcade authorization issuer changed"
        );
        let authorization = self
            .authorization_metadata(&canonical_url(&grant.issuer)?)
            .await?;
        ensure!(
            canonical_url(&authorization.issuer)? == canonical_url(&grant.issuer)?
                && canonical_url(&authorization.token_endpoint)?
                    == canonical_url(&grant.token_endpoint)?,
            "Arcade authorization metadata changed"
        );
        Ok(())
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

    async fn authorize(
        &self,
        context: &CapabilityContext,
        capability: &str,
    ) -> Result<StoredGrant> {
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
            if let Some(scope) = oauth_challenge.scope {
                query.append_pair("scope", &scope);
            }
        }

        context
            .authorization_required("arcade", capability, authorization_url.as_str())
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

    async fn verified_tool_names(&self, token: &str) -> Result<Vec<String>> {
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
        let result = async {
            let initialized = initialize
                .value
                .ok_or_else(|| anyhow!("Arcade initialize response is empty"))?;
            ensure!(
                initialized
                    .pointer("/result/protocolVersion")
                    .and_then(Value::as_str)
                    == Some(MCP_VERSION),
                "Arcade negotiated an unsupported MCP protocol version"
            );
            self.mcp(
                token,
                session.as_deref(),
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                false,
            )
            .await?;
            let upstream = self.list_tools(token, session.as_deref()).await?;
            Ok(self
                .tools
                .values()
                .filter(|tool| {
                    upstream.iter().any(|candidate| {
                        candidate.get("name").and_then(Value::as_str)
                            == Some(tool.upstream_name.as_str())
                            && candidate.get("inputSchema") == Some(&tool.upstream_input_schema)
                    })
                })
                .map(|tool| tool.name.clone())
                .collect())
        }
        .await;
        if let Some(session) = session.as_deref() {
            self.close_session(token, session).await;
        }
        result
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
        let initialized = initialize
            .value
            .ok_or_else(|| anyhow!("Arcade initialize response is empty"))?;
        ensure!(
            initialized
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str)
                == Some(MCP_VERSION),
            "Arcade negotiated an unsupported MCP protocol version"
        );
        let result = self
            .execute_in_session(token, session.as_deref(), tool, arguments)
            .await;
        if let Some(session) = session.as_deref() {
            self.close_session(token, session).await;
        }
        result
    }

    async fn execute_in_session(
        &self,
        token: &str,
        session: Option<&str>,
        tool: &ArcadeToolConfig,
        arguments: &Value,
    ) -> Result<Value> {
        self.mcp(
            token,
            session,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            false,
        )
        .await?;
        let upstream = self
            .list_tools(token, session)
            .await?
            .into_iter()
            .find(|candidate| candidate["name"] == tool.upstream_name)
            .ok_or_else(|| anyhow!("Configured Arcade tool is unavailable"))?;
        ensure!(
            upstream.get("inputSchema") == Some(&tool.upstream_input_schema),
            "Configured Arcade tool schema changed"
        );

        let called = self
            .mcp(
                token,
                session,
                json!({
                    "jsonrpc": "2.0", "id": Uuid::new_v4().to_string(), "method": "tools/call",
                    "params": { "name": tool.upstream_name, "arguments": arguments }
                }),
                true,
            )
            .await?
            .value
            .ok_or_else(|| anyhow!("Arcade tool response is empty"))?;
        if called.get("error").is_some()
            || called.pointer("/result/isError") == Some(&Value::Bool(true))
        {
            if let Some(url) = find_arcade_authorization_url(&called) {
                bail!("ARCADE_AUTHORIZATION_REQUIRED:{url}");
            }
            bail!("Arcade tool call failed");
        }
        let result = called
            .pointer("/result/structuredContent")
            .or_else(|| called.pointer("/result/content"))
            .ok_or_else(|| anyhow!("Arcade tool response has no result content"))?;
        project_result(result, &tool.output_schema, "result")
    }

    async fn list_tools(&self, token: &str, session: Option<&str>) -> Result<Vec<Value>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..32 {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
            let listed = self
                .mcp(
                    token,
                    session,
                    json!({ "jsonrpc": "2.0", "id": Uuid::new_v4().to_string(), "method": "tools/list", "params": params }),
                    true,
                )
                .await?
                .value
                .ok_or_else(|| anyhow!("Arcade tools/list response is empty"))?;
            let page = listed
                .pointer("/result/tools")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("Arcade tools/list response is malformed"))?;
            ensure!(
                tools.len().saturating_add(page.len()) <= 10_000,
                "Arcade tools/list returned too many tools"
            );
            tools.extend(page.iter().cloned());
            match listed.pointer("/result/nextCursor") {
                None | Some(Value::Null) => return Ok(tools),
                Some(Value::String(next)) if !next.is_empty() && next.len() <= 1024 => {
                    ensure!(
                        seen.insert(next.clone()),
                        "Arcade tools/list cursor repeated"
                    );
                    cursor = Some(next.clone());
                }
                _ => bail!("Arcade tools/list cursor is malformed"),
            }
        }
        bail!("Arcade tools/list exceeded its page limit")
    }

    async fn close_session(&self, token: &str, session: &str) {
        let _ = self
            .client
            .delete(self.endpoint.clone())
            .bearer_auth(token)
            .header("MCP-Protocol-Version", MCP_VERSION)
            .header("Mcp-Session-Id", session)
            .send()
            .await;
    }

    async fn mcp(
        &self,
        token: &str,
        session: Option<&str>,
        body: Value,
        expect_body: bool,
    ) -> Result<McpResponse> {
        let expected_id = body.get("id").cloned();
        ensure!(
            !expect_body || expected_id.is_some(),
            "MCP request expecting a response has no ID"
        );
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
        ensure!(
            content_type.starts_with("application/json")
                || content_type.starts_with("text/event-stream"),
            "Arcade MCP response has an unsupported content type"
        );
        let bytes = bounded_bytes(response, self.max_response_bytes).await?;
        let value = if content_type.starts_with("text/event-stream") {
            parse_sse(&bytes)?
        } else {
            serde_json::from_slice(&bytes).context("decode Arcade MCP response")?
        };
        validate_mcp_response(
            &value,
            expected_id.as_ref().expect("response ID was checked"),
        )?;
        Ok(McpResponse {
            value: Some(value),
            session_id,
        })
    }
}

#[async_trait]
impl CapabilityProvider for ArcadeGatewayProvider {
    fn configured_descriptors(&self, context: &CapabilityContext) -> Vec<CapabilityDescriptor> {
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

    async fn descriptors(&self, context: &CapabilityContext) -> Result<Vec<CapabilityDescriptor>> {
        if !self.visible(context) {
            return Ok(Vec::new());
        }
        let token = self
            .access_token(context, "arcade.gateway")
            .await
            .map_err(|_| anyhow!("Arcade gateway authorization failed"))?;
        let verified = self
            .verified_tool_names(&token)
            .await
            .map_err(|_| anyhow!("Arcade tool verification failed"))?;
        Ok(self
            .tools
            .values()
            .filter(|tool| tool.policy != Decision::Deny && verified.contains(&tool.name))
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
            .collect())
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
            .access_token(context, capability)
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
                context
                    .authorization_required("arcade", capability, &url)
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

async fn bounded_bytes(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        ensure!(length <= limit as u64, "Arcade response is too large");
    }
    let mut bytes =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= limit,
            "Arcade response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn receive_code(listener: TcpListener, expected_state: &str) -> Result<String> {
    for _ in 0..8 {
        let (mut stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 2048];
        let valid = loop {
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                break None;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.len() > 16 * 1024 {
                break None;
            }
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let request = std::str::from_utf8(&bytes).ok();
                break request.and_then(|request| parse_oauth_callback(request, expected_state));
            }
        };
        if let Some(code) = valid {
            write_callback_response(
                &mut stream,
                200,
                "Arcade authorization completed. You may close this tab.",
            )
            .await?;
            return Ok(code);
        }
        write_callback_response(&mut stream, 400, "Invalid OAuth callback.").await?;
    }
    bail!("OAuth callback did not include a valid code and state")
}

fn parse_oauth_callback(request: &str, expected_state: &str) -> Option<String> {
    let mut lines = request.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    if request_line.next()? != "GET" {
        return None;
    }
    let target = request_line.next()?;
    if request_line.next()? != "HTTP/1.1" {
        return None;
    }
    let host = lines.find_map(|line| {
        line.split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map(|(_, value)| value.trim())
    })?;
    if host != "127.0.0.1" && !host.starts_with("127.0.0.1:") {
        return None;
    }
    let callback = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if callback.path() != "/callback" {
        return None;
    }
    let parameters = callback.query_pairs().collect::<BTreeMap<_, _>>();
    if parameters.get("state").map(|value| value.as_ref()) != Some(expected_state) {
        return None;
    }
    parameters
        .get("code")
        .filter(|code| !code.is_empty())
        .map(ToString::to_string)
}

async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
) -> Result<()> {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} {}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                if status == 200 { "OK" } else { "Bad Request" },
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    Ok(())
}

fn bearer_parameter(header: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=\"");
    let start = header.find(&marker)? + marker.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_owned())
}

fn ensure_trusted_oauth_url(value: &str) -> Result<()> {
    let url = Url::parse(value)?;
    #[cfg(test)]
    let test_loopback =
        url.scheme() == "http" && url.host_str().is_some_and(|host| host == "127.0.0.1");
    #[cfg(not(test))]
    let test_loopback = false;
    ensure!(
        url.scheme() == "https" || test_loopback,
        "OAuth endpoint must use HTTPS"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "OAuth URL contains credentials"
    );
    ensure!(url.fragment().is_none(), "OAuth URL contains a fragment");
    let host = url.host_str().unwrap_or_default();
    ensure!(
        test_loopback || TRUSTED_ARCADE_HOSTS.contains(&host),
        "OAuth endpoint is outside the reviewed Arcade origins"
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

fn validate_mcp_response(value: &Value, expected_id: &Value) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("Arcade MCP response is not an object"))?;
    ensure!(
        object.get("jsonrpc") == Some(&Value::String("2.0".into())),
        "Arcade MCP response has an invalid JSON-RPC version"
    );
    ensure!(
        !object.contains_key("method"),
        "Arcade sent an unsupported server request or notification"
    );
    ensure!(
        object.get("id") == Some(expected_id),
        "Arcade MCP response ID mismatch"
    );
    ensure!(
        object.contains_key("result") ^ object.contains_key("error"),
        "Arcade MCP response must contain exactly one result or error"
    );
    Ok(())
}

fn parse_sse(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes).context("Arcade SSE is not UTF-8")?;
    let mut events = Vec::new();
    let mut data = Vec::new();
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if !data.is_empty() {
                events.push(data.join("\n"));
                data.clear();
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start().to_owned());
        } else if !line.starts_with(':') && !line.starts_with("event:") && !line.starts_with("id:")
        {
            bail!("Arcade SSE response contains an unsupported field");
        }
    }
    ensure!(
        events.len() == 1,
        "Arcade SSE response must contain exactly one data event"
    );
    serde_json::from_str(&events[0]).context("decode Arcade SSE data")
}

fn find_arcade_authorization_url(value: &Value) -> Option<String> {
    fn validated(value: &Value) -> Option<String> {
        let candidate = value.as_str()?;
        ensure_trusted_oauth_url(candidate)
            .ok()
            .map(|_| candidate.to_owned())
    }

    let object = value.as_object()?;
    for pointer in [
        "/error/data/authorization_url",
        "/error/data/authorizationUrl",
        "/result/_meta/authorization_url",
        "/result/_meta/authorizationUrl",
    ] {
        if let Some(url) = value.pointer(pointer).and_then(validated) {
            return Some(url);
        }
    }
    object
        .get("error")
        .and_then(Value::as_object)
        .and_then(|error| error.get("data"))
        .and_then(Value::as_object)
        .and_then(|data| data.get("url"))
        .and_then(validated)
}

fn validate_schema(value: &Value, schema: &Value, path: &str) -> Result<()> {
    validate_schema_at(value, schema, path, 0)
}

fn validate_schema_at(value: &Value, schema: &Value, path: &str, depth: usize) -> Result<()> {
    ensure!(depth <= 16, "{path} is too deeply nested");
    if let Some(expected) = schema.get("const") {
        ensure!(
            value == expected,
            "{path} does not match its constant value"
        );
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        ensure!(
            allowed.contains(value),
            "{path} is outside its allowed values"
        );
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("{path} must be an object"))?;
            ensure!(object.len() <= 10_000, "{path} contains too many fields");
            let properties = schema
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            ensure!(
                object.keys().all(|key| properties.contains_key(key)),
                "{path} contains unknown fields"
            );
            validate_size(object.len(), schema, "minProperties", "maxProperties", path)?;
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    ensure!(object.contains_key(name), "{path}.{name} is required");
                }
            }
            for (name, child) in object {
                if let Some(child_schema) = properties.get(name) {
                    validate_schema_at(child, child_schema, &format!("{path}.{name}"), depth + 1)?;
                }
            }
        }
        Some("string") => {
            let string = value
                .as_str()
                .ok_or_else(|| anyhow!("{path} must be a string"))?;
            validate_size(
                string.chars().count(),
                schema,
                "minLength",
                "maxLength",
                path,
            )?;
        }
        Some("integer") => {
            let integer = value
                .as_i64()
                .ok_or_else(|| anyhow!("{path} must be an integer"))?;
            validate_number(integer as f64, schema, path)?;
        }
        Some("number") => {
            let number = value
                .as_f64()
                .ok_or_else(|| anyhow!("{path} must be a number"))?;
            validate_number(number, schema, path)?;
        }
        Some("boolean") => ensure!(value.is_boolean(), "{path} must be a boolean"),
        Some("null") => ensure!(value.is_null(), "{path} must be null"),
        Some("array") => {
            let values = value
                .as_array()
                .ok_or_else(|| anyhow!("{path} must be an array"))?;
            ensure!(values.len() <= 10_000, "{path} contains too many items");
            validate_size(values.len(), schema, "minItems", "maxItems", path)?;
            let items = schema
                .get("items")
                .ok_or_else(|| anyhow!("{path} has no item schema"))?;
            for (index, child) in values.iter().enumerate() {
                validate_schema_at(child, items, &format!("{path}[{index}]"), depth + 1)?;
            }
        }
        Some(other) => bail!("Unsupported schema type {other}"),
        None => bail!("{path} has no schema type"),
    }
    Ok(())
}

fn validate_size(
    actual: usize,
    schema: &Value,
    minimum_name: &str,
    maximum_name: &str,
    path: &str,
) -> Result<()> {
    if let Some(minimum) = schema.get(minimum_name).and_then(Value::as_u64) {
        ensure!(
            actual >= minimum as usize,
            "{path} is below its minimum size"
        );
    }
    if let Some(maximum) = schema.get(maximum_name).and_then(Value::as_u64) {
        ensure!(
            actual <= maximum as usize,
            "{path} exceeds its maximum size"
        );
    }
    Ok(())
}

fn validate_number(number: f64, schema: &Value, path: &str) -> Result<()> {
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
        ensure!(number >= minimum, "{path} is below its minimum");
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
        ensure!(number <= maximum, "{path} exceeds its maximum");
    }
    Ok(())
}

fn project_result(value: &Value, schema: &Value, path: &str) -> Result<Value> {
    fn project(value: &Value, schema: &Value, path: &str, depth: usize) -> Result<Value> {
        ensure!(depth <= 16, "{path} is too deeply nested");
        match schema.get("type").and_then(Value::as_str) {
            Some("object") => {
                let source = value
                    .as_object()
                    .ok_or_else(|| anyhow!("{path} must be an object"))?;
                let properties = schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .ok_or_else(|| anyhow!("{path} has no projected properties"))?;
                let mut projected = serde_json::Map::new();
                for (name, child_schema) in properties {
                    if let Some(child) = source.get(name) {
                        projected.insert(
                            name.clone(),
                            project(child, child_schema, &format!("{path}.{name}"), depth + 1)?,
                        );
                    }
                }
                Ok(Value::Object(projected))
            }
            Some("array") => {
                let values = value
                    .as_array()
                    .ok_or_else(|| anyhow!("{path} must be an array"))?;
                ensure!(values.len() <= 10_000, "{path} contains too many items");
                let items = schema
                    .get("items")
                    .ok_or_else(|| anyhow!("{path} has no item schema"))?;
                values
                    .iter()
                    .enumerate()
                    .map(|(index, child)| {
                        project(child, items, &format!("{path}[{index}]"), depth + 1)
                    })
                    .collect::<Result<Vec<_>>>()
                    .map(Value::Array)
            }
            Some(_) => Ok(value.clone()),
            None => bail!("{path} has no schema type"),
        }
    }

    let projected = project(value, schema, path, 0)?;
    validate_schema(&projected, schema, path)?;
    Ok(projected)
}

#[cfg(test)]
mod tests;
