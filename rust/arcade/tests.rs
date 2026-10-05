use super::*;

use crate::{
    domain::{ApprovalRequest, AuthorizationRequest},
    ports::JobEventPort,
};
use std::sync::atomic::{AtomicUsize, Ordering};

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

struct BrowserEvents(AtomicUsize);

#[derive(Default)]
struct RecordingEvents(Mutex<Vec<AuthorizationRequest>>);

#[async_trait]
impl JobEventPort for RecordingEvents {
    async fn status(&self, _message: &str) -> Result<()> {
        Ok(())
    }
    async fn request_approval(&self, _request: ApprovalRequest) -> Result<String> {
        Ok("no".into())
    }
    async fn authorization_required(&self, request: AuthorizationRequest) -> Result<()> {
        self.0.lock().unwrap().push(request);
        Ok(())
    }
}

#[async_trait]
impl JobEventPort for BrowserEvents {
    async fn status(&self, _message: &str) -> Result<()> {
        Ok(())
    }
    async fn request_approval(&self, _request: ApprovalRequest) -> Result<String> {
        Ok("no".into())
    }
    async fn authorization_required(&self, request: AuthorizationRequest) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let authorization = Url::parse(&request.url)?;
        let parameters = authorization.query_pairs().collect::<BTreeMap<_, _>>();
        let redirect = Url::parse(
            parameters
                .get("redirect_uri")
                .ok_or_else(|| anyhow!("missing redirect"))?,
        )?;
        let state = parameters
            .get("state")
            .ok_or_else(|| anyhow!("missing state"))?;
        let address = format!(
            "127.0.0.1:{}",
            redirect
                .port()
                .ok_or_else(|| anyhow!("missing callback port"))?
        );
        let path = redirect.path().to_owned();
        let state = state.to_string();
        tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(address).await?;
            stream
                .write_all(
                    format!(
                        "GET {path}?code=test-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await?;
            ensure!(response.starts_with(b"HTTP/1.1 200"), "callback failed");
            Ok::<_, anyhow::Error>(())
        });
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
            upstream_input_schema: json!({
                "type": "object",
                "properties": { "number": { "type": "integer" } }
            }),
            input_schema: json!({
                "type": "object",
                "properties": { "number": { "type": "integer", "minimum": 1 } },
                "required": ["number"],
                "additionalProperties": false
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "title": { "type": "string", "maxLength": 256 } },
                "required": ["title"],
                "additionalProperties": false
            }),
            policy: Decision::Allow,
        }],
    }
}

fn context(ingress: &str) -> CapabilityContext {
    CapabilityContext::test(ingress)
}

async fn fake_oauth_gateway(listener: TcpListener, endpoint: Url) -> Result<()> {
    let origin = format!("http://{}", listener.local_addr()?);
    for _ in 0..16 {
        let (mut stream, _) = listener.accept().await?;
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).await?;
            ensure!(read > 0, "fake OAuth request ended early");
            request.extend_from_slice(&chunk[..read]);
            if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break position;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end])?.to_owned();
        let request_line = headers.lines().next().unwrap_or_default().to_owned();
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .map(str::to_owned)
            })
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        while request.len() < header_end + 4 + length {
            let read = stream.read(&mut chunk).await?;
            ensure!(read > 0, "fake OAuth body ended early");
            request.extend_from_slice(&chunk[..read]);
        }
        let body = &request[header_end + 4..header_end + 4 + length];
        let (status, extra_headers, response) = if request_line.starts_with("GET /resource ") {
            (
                200,
                "",
                json!({"resource":endpoint.as_str(),"authorization_servers":[origin.as_str()]})
                    .to_string(),
            )
        } else if request_line.starts_with("GET /.well-known/oauth-authorization-server ") {
            (
                200,
                "",
                json!({
                    "issuer": origin.as_str(),
                    "authorization_endpoint": format!("{origin}/authorize"),
                    "token_endpoint": format!("{origin}/token"),
                    "registration_endpoint": format!("{origin}/register"),
                    "code_challenge_methods_supported": ["S256"]
                })
                .to_string(),
            )
        } else if request_line.starts_with("POST /register ") {
            (200, "", json!({"client_id":"test-client"}).to_string())
        } else if request_line.starts_with("POST /token ") {
            let form = std::str::from_utf8(body)?;
            ensure!(
                form.contains("code_verifier=") || form.contains("refresh_token="),
                "token request lacks proof"
            );
            (200, "", json!({"access_token":"access-token","refresh_token":"refresh-token","expires_in":3600}).to_string())
        } else if request_line.starts_with("DELETE /mcp/test ") {
            (200, "", String::new())
        } else {
            let value: Value = serde_json::from_slice(body)?;
            if headers
                .to_ascii_lowercase()
                .contains("authorization: bearer access-token")
            {
                if value["method"] == "notifications/initialized" {
                    (202, "", String::new())
                } else {
                    let result = match value["method"].as_str().unwrap_or_default() {
                        "initialize" => {
                            json!({"protocolVersion":MCP_VERSION,"capabilities":{"tools":{}}})
                        }
                        "tools/list" => {
                            json!({"tools":[{"name":"GitHub.GetIssue","inputSchema":{"type":"object","properties":{"number":{"type":"integer"}}}}]})
                        }
                        _ => bail!("unexpected authenticated method"),
                    };
                    let session = if value["method"] == "initialize" {
                        "Mcp-Session-Id: oauth-session\r\n"
                    } else {
                        ""
                    };
                    (
                        200,
                        session,
                        json!({"jsonrpc":"2.0","id":value["id"],"result":result}).to_string(),
                    )
                }
            } else {
                (
                    401,
                    "WWW-Authenticate: Bearer resource_metadata=\"RESOURCE\", scope=\"mcp:tools\"\r\n",
                    String::new(),
                )
            }
        };
        let extra_headers = extra_headers.replace("RESOURCE", &format!("{origin}/resource"));
        let reason = match status {
            200 => "OK",
            202 => "Accepted",
            401 => "Unauthorized",
            _ => "Error",
        };
        stream.write_all(format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n{extra_headers}content-length: {}\r\nconnection: close\r\n\r\n{response}",
            response.len()
        ).as_bytes()).await?;
    }
    Ok(())
}

async fn fake_mcp_gateway(
    listener: TcpListener,
    include_call: bool,
    authorization_required: bool,
) -> Result<()> {
    for _ in 0..if include_call { 5 } else { 4 } {
        let (mut stream, _) = listener.accept().await?;
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).await?;
            ensure!(read > 0, "fake gateway request ended early");
            request.extend_from_slice(&chunk[..read]);
            if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break position;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end])?.to_owned();
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .map(str::to_owned)
            })
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        while request.len() < header_end + 4 + length {
            let read = stream.read(&mut chunk).await?;
            ensure!(read > 0, "fake gateway body ended early");
            request.extend_from_slice(&chunk[..read]);
        }
        let request_line = headers.lines().next().unwrap_or_default();
        if request_line.starts_with("DELETE ") {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await?;
            continue;
        }
        let body: Value =
            serde_json::from_slice(&request[header_end + 4..header_end + 4 + length])?;
        if body["method"] == "notifications/initialized" {
            stream
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await?;
            continue;
        }
        let id = body["id"].clone();
        let result = match body["method"].as_str().unwrap_or_default() {
            "initialize" => {
                json!({"protocolVersion": MCP_VERSION, "capabilities": {"tools": {}}})
            }
            "tools/list" => {
                json!({"tools": [{"name":"GitHub.GetIssue","inputSchema":{"type":"object","properties":{"number":{"type":"integer"}}}}]})
            }
            "tools/call" => {
                json!({"structuredContent":{"title":"issue","unreviewed":"drop"},"isError":false})
            }
            _ => bail!("unexpected fake gateway method"),
        };
        let body = if body["method"] == "tools/call" && authorization_required {
            json!({
                "jsonrpc":"2.0",
                "id":id,
                "error": {
                    "code": -32001,
                    "message": "authorization required",
                    "data": {"authorization_url":"https://cloud.arcade.dev/auth/tool"}
                }
            })
            .to_string()
        } else {
            json!({"jsonrpc":"2.0","id":id,"result":result}).to_string()
        };
        let session = if body.contains("protocolVersion") {
            "Mcp-Session-Id: session-1\r\n"
        } else {
            ""
        };
        stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{session}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
    }
    Ok(())
}

#[tokio::test]
async fn rejects_chunked_mcp_responses_as_soon_as_the_byte_limit_is_exceeded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Url::parse(&format!(
        "http://{}/mcp/test",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).await?;
        let oversized = "x".repeat(128);
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                    oversized.len(), oversized
                )
                .as_bytes(),
            )
            .await?;
        Ok::<_, anyhow::Error>(())
    });
    let mut test_config = config();
    test_config.max_response_bytes = 64;
    let provider = ArcadeGatewayProvider::with_endpoint(
        test_config,
        Arc::new(MemoryStore::default()),
        endpoint,
    )
    .unwrap();
    let result = provider
        .mcp(
            "token",
            None,
            json!({"jsonrpc":"2.0","id":"one","method":"tools/list","params":{}}),
            true,
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("too large"));
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn oauth_callback_ignores_invalid_requests_before_accepting_the_matching_state() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let receiver = tokio::spawn(async move { receive_code(listener, "expected").await });
    for target in [
        "/callback?code=bad&state=wrong",
        "/callback?code=good&state=expected",
    ] {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
    }
    assert_eq!(receiver.await.unwrap().unwrap(), "good");
}

#[tokio::test]
async fn speaks_strict_mcp_and_projects_results_through_a_fake_gateway() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Url::parse(&format!(
        "http://{}/mcp/test",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = tokio::spawn(fake_mcp_gateway(listener, true, false));
    let provider =
        ArcadeGatewayProvider::with_endpoint(config(), Arc::new(MemoryStore::default()), endpoint)
            .unwrap();
    let tool = provider.tools["arcade.issue"].clone();
    let result = provider
        .execute("secret-token", &tool, &json!({"number":1}))
        .await
        .unwrap();
    assert_eq!(result, json!({"title":"issue"}));
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn tool_authorization_is_structured_and_the_mutation_is_not_replayed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Url::parse(&format!(
        "http://{}/mcp/test",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = tokio::spawn(fake_mcp_gateway(listener, true, true));
    let provider = ArcadeGatewayProvider::with_endpoint(
        config(),
        Arc::new(MemoryStore::default()),
        endpoint.clone(),
    )
    .unwrap();
    let events = Arc::new(RecordingEvents::default());
    let context = CapabilityContext::test_with_events("cli", Some(events.clone()));
    provider
        .save_grant(
            &provider.store_key(&context),
            &StoredGrant {
                resource: endpoint.to_string(),
                issuer: "https://auth.arcade.dev".into(),
                token_endpoint: "https://auth.arcade.dev/token".into(),
                client_id: "client".into(),
                client_secret: None,
                access_token: "token".into(),
                refresh_token: None,
                expires_at_unix: now_unix().unwrap() + 3600,
            },
        )
        .unwrap();
    provider
        .validated_grants
        .lock()
        .unwrap()
        .insert(provider.store_key(&context));
    let result = provider
        .invoke(&context, "arcade.issue", &json!({"number":1}))
        .await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("retry after authorizing"), "{error}");
    {
        let recorded = events.0.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].capability, "arcade.issue");
        assert_eq!(recorded[0].url, "https://cloud.arcade.dev/auth/tool");
    }
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn discovers_authorizes_persists_and_reuses_a_gateway_grant() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Url::parse(&format!(
        "http://{}/mcp/test",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = tokio::spawn(fake_oauth_gateway(listener, endpoint.clone()));
    let store = Arc::new(MemoryStore::default());
    let provider =
        ArcadeGatewayProvider::with_endpoint(config(), store.clone(), endpoint.clone()).unwrap();
    let events = Arc::new(BrowserEvents(AtomicUsize::new(0)));
    let context = CapabilityContext::test_with_events("cli", Some(events.clone()));

    assert_eq!(provider.descriptors(&context).await.unwrap().len(), 1);
    drop(provider);
    let restarted = ArcadeGatewayProvider::with_endpoint(config(), store, endpoint).unwrap();
    assert_eq!(restarted.descriptors(&context).await.unwrap().len(), 1);
    assert_eq!(events.0.load(Ordering::SeqCst), 1);
    assert!(
        restarted
            .load_grant(&restarted.store_key(&context))
            .unwrap()
            .is_some()
    );
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn refreshes_and_atomically_rotates_a_persisted_grant() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let endpoint = Url::parse(&format!("{origin}/mcp/test")).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut request = vec![0u8; 8192];
        let read = stream.read(&mut request).await?;
        let request = std::str::from_utf8(&request[..read])?;
        ensure!(
            request.contains("refresh_token=old-refresh"),
            "old refresh token missing"
        );
        let body =
            json!({"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600})
                .to_string();
        stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
        Ok::<_, anyhow::Error>(())
    });
    let provider = ArcadeGatewayProvider::with_endpoint(
        config(),
        Arc::new(MemoryStore::default()),
        endpoint.clone(),
    )
    .unwrap();
    let context = context("cli");
    provider
        .save_grant(
            &provider.store_key(&context),
            &StoredGrant {
                resource: endpoint.to_string(),
                issuer: origin.clone(),
                token_endpoint: format!("{origin}/token"),
                client_id: "client".into(),
                client_secret: None,
                access_token: "expired".into(),
                refresh_token: Some("old-refresh".into()),
                expires_at_unix: 0,
            },
        )
        .unwrap();
    provider
        .validated_grants
        .lock()
        .unwrap()
        .insert(provider.store_key(&context));
    assert_eq!(
        provider
            .access_token(&context, "arcade.gateway")
            .await
            .unwrap(),
        "new-access"
    );
    let saved = provider
        .load_grant(&provider.store_key(&context))
        .unwrap()
        .unwrap();
    assert_eq!(saved.refresh_token.as_deref(), Some("new-refresh"));
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn lists_only_tools_verified_against_the_upstream_schema() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = Url::parse(&format!(
        "http://{}/mcp/test",
        listener.local_addr().unwrap()
    ))
    .unwrap();
    let server = tokio::spawn(fake_mcp_gateway(listener, false, false));
    let provider = ArcadeGatewayProvider::with_endpoint(
        config(),
        Arc::new(MemoryStore::default()),
        endpoint.clone(),
    )
    .unwrap();
    let context = context("cli");
    provider
        .save_grant(
            &provider.store_key(&context),
            &StoredGrant {
                resource: endpoint.to_string(),
                issuer: "https://auth.arcade.dev".into(),
                token_endpoint: "https://auth.arcade.dev/token".into(),
                client_id: "client".into(),
                client_secret: None,
                access_token: "token".into(),
                refresh_token: None,
                expires_at_unix: now_unix().unwrap() + 3600,
            },
        )
        .unwrap();
    provider
        .validated_grants
        .lock()
        .unwrap()
        .insert(provider.store_key(&context));
    let descriptors = provider.descriptors(&context).await.unwrap();
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].name, "arcade.issue");
    server.await.unwrap().unwrap();
}

#[cfg(target_os = "macos")]
struct InteractiveEvents;

#[cfg(target_os = "macos")]
#[async_trait]
impl JobEventPort for InteractiveEvents {
    async fn status(&self, message: &str) -> Result<()> {
        eprintln!("{message}");
        Ok(())
    }
    async fn request_approval(&self, _request: ApprovalRequest) -> Result<String> {
        Ok("no".into())
    }
    async fn authorization_required(&self, request: AuthorizationRequest) -> Result<()> {
        eprintln!(
            "Authorization required for {}: {}",
            request.capability, request.url
        );
        let status = std::process::Command::new("/usr/bin/open")
            .arg(&request.url)
            .status()?;
        ensure!(
            status.success(),
            "failed to open the Arcade authorization URL"
        );
        Ok(())
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires POCKET_AGENT_ARCADE_LIVE_CONFIG and interactive browser authorization"]
async fn live_gateway_verifies_the_reviewed_wire_protocol() {
    let raw = std::env::var("POCKET_AGENT_ARCADE_LIVE_CONFIG")
        .expect("set POCKET_AGENT_ARCADE_LIVE_CONFIG to one Arcade connector JSON object");
    let config: ArcadeConnectorConfig = serde_json::from_str(&raw).unwrap();
    let provider = ArcadeGatewayProvider::new(config, SystemSecretStore::new().unwrap()).unwrap();
    let context = CapabilityContext::test_with_events("cli", Some(Arc::new(InteractiveEvents)));
    let descriptors = provider.descriptors(&context).await.unwrap();
    assert!(!descriptors.is_empty());
}

#[tokio::test]
async fn hides_tools_from_remote_ingress_and_normalizes_arguments() {
    let provider = ArcadeGatewayProvider::new(config(), Arc::new(MemoryStore::default())).unwrap();
    assert!(
        provider
            .descriptors(&context("signal"))
            .await
            .unwrap()
            .is_empty()
    );
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
fn validates_and_projects_the_complete_supported_local_schema() {
    let schema = json!({
        "type": "object",
        "properties": {
            "owner": { "type": "string", "minLength": 2, "maxLength": 5, "enum": ["acme", "other"] },
            "number": { "type": "integer", "minimum": 1, "maximum": 10 },
            "labels": { "type": "array", "minItems": 1, "maxItems": 2, "items": { "type": "string", "maxLength": 8 } }
        },
        "required": ["owner", "number", "labels"],
        "additionalProperties": false
    });
    let value = json!({"owner":"acme","number":1,"labels":["bug"]});
    validate_schema(&value, &schema, "arguments").unwrap();
    assert!(
        validate_schema(
            &json!({"owner":"nope","number":1,"labels":["bug"]}),
            &schema,
            "arguments"
        )
        .is_err()
    );
    assert!(
        validate_schema(
            &json!({"owner":"acme","number":11,"labels":["bug"]}),
            &schema,
            "arguments"
        )
        .is_err()
    );
    assert!(
        validate_schema(
            &json!({"owner":"acme","number":1,"labels":[]}),
            &schema,
            "arguments"
        )
        .is_err()
    );

    let output_schema = json!({
        "type": "object",
        "properties": { "title": { "type": "string", "maxLength": 10 } },
        "required": ["title"],
        "additionalProperties": false
    });
    assert_eq!(
        project_result(
            &json!({"title":"issue","secret":"drop"}),
            &output_schema,
            "result"
        )
        .unwrap(),
        json!({"title":"issue"})
    );
}

#[test]
fn rejects_mismatched_and_server_initiated_mcp_responses() {
    let expected = json!("request-1");
    assert!(
        validate_mcp_response(
            &json!({"jsonrpc":"2.0","id":"request-1","result":{}}),
            &expected
        )
        .is_ok()
    );
    assert!(
        validate_mcp_response(
            &json!({"jsonrpc":"2.0","id":"other","result":{}}),
            &expected
        )
        .is_err()
    );
    assert!(
        validate_mcp_response(
            &json!({"jsonrpc":"2.0","id":"request-1","method":"sampling/create","params":{}}),
            &expected
        )
        .is_err()
    );
    assert!(validate_mcp_response(&json!({"jsonrpc":"2.0","id":"request-1"}), &expected).is_err());
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
    assert!(validate_schema(&json!({"owner":"acme","number":0}), &schema, "arguments").is_err());
    assert!(
        validate_schema(
            &json!({"owner":"acme","number":1,"url":"https://evil.test"}),
            &schema,
            "arguments"
        )
        .is_err()
    );
    assert_eq!(
        find_arcade_authorization_url(
            &json!({"error":{"data":{"authorization_url":"https://cloud.arcade.dev/auth/x"}}})
        )
        .as_deref(),
        Some("https://cloud.arcade.dev/auth/x")
    );
    assert!(
        find_arcade_authorization_url(
            &json!({"error":{"data":{"authorization_url":"https://evil.test/auth"}}})
        )
        .is_none()
    );
    assert!(
        find_arcade_authorization_url(&json!({"url":"https://cloud.arcade.dev/auth/x"})).is_none()
    );
    let store = MemoryStore::default();
    store.set("one", b"secret").unwrap();
    assert_eq!(store.get("one").unwrap().unwrap(), b"secret");
    store.delete("one").unwrap();
    assert!(store.get("one").unwrap().is_none());
}
