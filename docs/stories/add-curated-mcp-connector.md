# Story: expose curated Arcade tools through Pocket Agent

## User story

As an operator, I want to configure an Arcade MCP Gateway by its non-secret slug, authorize it interactively once, and expose a reviewed subset of its tools to coding jobs, so an agent runtime can use services such as GitHub, Linear, logs, Gmail, or Slack without receiving OAuth tokens or general network access.

## Boundary correction

Arcade tools are not exposed to the trusted `Harness`. The harness coordinates jobs and carries approvals and authorization challenges; it must not become a tool executor. Tools are exposed by Pocket Agent's job-scoped capability broker to the agent runtime inside an untrusted worker.

```text
Agent runtime in worker
    │ Pocket Agent MCP tools/list + tools/call
    ▼
Pocket Agent capability broker
    │ authenticate → scope → normalize → policy → approve → limit → audit
    ▼
ArcadeGatewayProvider in trusted Rust host
    │ MCP Streamable HTTP
    │ Authorization: Bearer <host-held OAuth access token>
    │ OAuth client registration + refresh token remain in OS credential store
    ▼
https://api.arcade.dev/mcp/<gateway-slug>
    │ Arcade gateway tool allowlist + tool-level OAuth
    ▼
GitHub / Linear / Gmail / Slack / other selected services
```

The worker sees only stable Pocket Agent capability descriptors and bounded results. It never sees the Arcade URL, OAuth registration, access/refresh tokens, session ID, authorization challenges, or downstream OAuth tokens.

## Why Arcade Gateway

Arcade MCP Gateways federate operator-selected tools into one MCP Streamable HTTP endpoint at `https://api.arcade.dev/mcp/{gateway-slug}`. Arcade supports browser-based Arcade Auth, production User Source/OIDC, and an Arcade Headers fallback. The first Pocket Agent integration uses **Arcade Auth**: the operator configures only the non-secret slug, Pocket Agent completes OAuth Authorization Code with PKCE as a native MCP client, and host-only authorization persists across jobs and process restarts. An Arcade API key is not required. Arcade Headers remains an explicit compatibility fallback, not the default.

Arcade's dashboard allowlist is valuable but mutable. Pocket Agent retains a second local allowlist and reviewed descriptors so a gateway dashboard change cannot silently expand worker authority.

Primary-source findings and links are recorded in [`../arcade-mcp-research.md`](../arcade-mcp-research.md).

## Proposed configuration

Configuration is host-local and deny-by-default. Names are illustrative until the configuration slice is implemented.

```json
{
  "connectors": {
    "arcade": {
      "gatewaySlug": "pocket-agent",
      "requestTimeoutMs": 30000,
      "maxCallsPerJob": 30,
      "maxRequestBytes": 65536,
      "maxResponseBytes": 262144,
      "tools": [
        {
          "name": "arcade.github_get_issue",
          "upstreamName": "GitHub.GetIssue",
          "description": "Read one GitHub issue from the operator's authorized account.",
          "inputSchema": {
            "type": "object",
            "properties": {
              "owner": { "type": "string", "maxLength": 100 },
              "repo": { "type": "string", "maxLength": 100 },
              "number": { "type": "integer", "minimum": 1 }
            },
            "required": ["owner", "repo", "number"],
            "additionalProperties": false
          },
          "policy": "allow"
        }
      ]
    }
  }
}
```

The slug is configuration, not a credential. Pocket Agent constructs the only permitted cloud endpoint from it. OAuth grants are keyed by canonical gateway URL and a trusted ingress-qualified principal; raw worker claims cannot select or share an identity. The initial Arcade Auth slice is local-only. Remote multi-user ingress requires a separately designed User Source/OIDC mapping and receives no Arcade capabilities until that design is enabled.

## Provider seam

Extract built-in workspace tools behind the same host-side provider interface used by Arcade:

```rust
#[async_trait]
trait CapabilityProvider: Send + Sync {
    fn descriptors(&self, context: &CapabilityContext) -> Vec<CapabilityDescriptor>;

    async fn normalize(
        &self,
        context: &CapabilityContext,
        capability: &str,
        arguments: Value,
    ) -> Result<NormalizedCapabilityCall>;

    async fn invoke(
        &self,
        context: &CapabilityContext,
        call: NormalizedCapabilityCall,
    ) -> Result<CapabilityOutcome>;
}
```

The exact API may change, but responsibilities may not:

- the broker owns lease authentication, job/conversation scope, replay prevention, policy, approval, global limits, and audit;
- the provider owns tool-specific validation, normalization, fixed destination selection, upstream translation, session handling, and bounded result projection;
- `CapabilityContext` supplies trusted principal, job, conversation, and repository identity;
- callers cannot provide or override endpoint, upstream tool name, OAuth identity, host path, credential, or policy;
- the worker-facing private MCP endpoint and Pi extension remain unchanged.

## Streamable HTTP behavior

`ArcadeGatewayProvider` is an MCP client, not a raw JSON POST proxy. It must:

1. initialize a bounded upstream MCP session;
2. send the required MCP protocol/version and Streamable HTTP accept headers;
3. retain any upstream session ID only in host memory and scope it to the connector identity;
4. send the initialized notification when required;
5. use `tools/list` only to verify configured upstream names/schema compatibility;
6. invoke only configured tools with `tools/call`;
7. reject server requests for sampling, roots, resources, elicitation, or other client capabilities;
8. ignore or reject runtime list-change notifications until trusted configuration is reviewed;
9. close/drop upstream MCP session state on shutdown, grant revocation, or connector failure.

An initial protocol spike against a development Arcade gateway must capture the exact response content types, session-header behavior, tool-name format, and authorization-challenge shape. Only redacted protocol structure may be committed. Arcade's public guides document the behavior but not every wire-level response detail.

## Persistent gateway and tool authorization

Gateway authentication and third-party tool authorization are separate. Pocket Agent persists the Arcade gateway OAuth registration and token set in the operating system credential store. Arcade persists and refreshes GitHub/Gmail/Linear/etc. grants for that gateway identity, so users normally authorize each required scope once rather than once per job or process. Neither token layer is exposed to the worker.

On first connection, Pocket Agent discovers OAuth metadata from the gateway, uses Dynamic Client Registration, Authorization Code with PKCE `S256`, a random `state`, and a loopback callback, then stores the registration and tokens. Later sessions refresh the token. Revocation, `invalid_grant`, issuer/resource mismatch, or identity mismatch fails closed and requires explicit reconnection.

Individual tools may still require the operator to visit an Arcade authorization URL.

Pocket Agent must treat this as an operator interaction, not ordinary model output:

1. Arcade reports that authorization is required.
2. The provider recognizes the documented response shape and validates the authorization URL against an explicit HTTPS origin allowlist learned during the protocol spike.
3. The harness emits an `AuthorizationRequired` event to the originating conversation with connector/tool identity and the bounded URL; the raw upstream body is not forwarded.
4. The current call completes as authorization-required rather than pretending the action succeeded.
5. After the operator authorizes, they explicitly retry or continue the turn; the host does not poll forever or replay a mutating call automatically.

The Arcade URL is safe to show to the authenticated operator, but it is not added to model-visible output unless needed to explain that operator action is pending.

## Security requirements

1. **Trusted configuration only.** Ingress messages, prompts, repositories, workers, and Arcade responses cannot register connectors or alter endpoint/tool/user policy.
2. **No arbitrary execution.** Pocket Agent does not install or spawn Arcade CLI, Python, `npx`, a local MCP server, package manager, or hook. It calls an existing Arcade Gateway from Rust.
3. **Fixed egress.** Cloud mode accepts only canonical `https://api.arcade.dev/mcp/{validated-slug}` URLs. Redirects and proxies are disabled. Self-hosted Arcade requires a separate design because it changes the destination trust policy.
4. **Credential confinement.** OAuth client registrations and access/refresh tokens are stored only in an OS credential store and used only by the Rust host. They are absent from config, worker environment/arguments, model context, events, approvals, results, errors, and audit.
5. **Trusted identity binding.** A stored grant is keyed by canonical gateway URL, auth mode/issuer, and trusted ingress-qualified principal. Workers and prompts cannot choose, export, or impersonate users.
6. **Dual tool allowlists.** A tool must be selected in the Arcade Gateway and configured locally. Upstream discovery cannot automatically register a worker capability.
7. **Reviewed descriptors.** Worker-facing names, descriptions, schemas, and policies are host-authored. Upstream schema mismatch disables the tool instead of broadening it.
8. **Protocol reduction.** The connector supports only initialization, tool verification, and tool calls. Prompts, resources, roots, sampling, elicitation, arbitrary notifications, and server-initiated model calls are unavailable.
9. **Bounded projection.** Expected structured fields are extracted under byte/depth/item limits. Raw upstream descriptions, instructions, HTML, logs, and payloads are not blindly inserted into model context.
10. **Scoped authorization.** Pocket Agent policy still binds calls to lease, principal, job, conversation, request ID, expiry, call count, normalized digest, and one-operation approval when configured as `ask`.
11. **No unsafe automatic retry.** Mutating calls are never replayed automatically after timeout, transport ambiguity, authorization, or reconnect.
12. **Cancellation and revocation.** Job cancellation, timeout, lease revocation, and host shutdown abort in-flight Arcade requests. Late responses cannot cross a run/job boundary.
13. **Redacted audit and errors.** Records contain connector/capability identity, job/repository identity, digest, timing, byte counts, and outcome—not raw arguments, endpoint headers, OAuth data, authorization URLs, or result bodies.
14. **Fail closed.** Missing/expired grants, identity mismatch, TLS failure, redirect, unknown tool, schema drift, oversized data, unsupported MCP behavior, malformed responses, and audit failure reject the call with no direct-worker fallback.

## Acceptance criteria

- [ ] One Arcade Auth gateway can be configured using only a validated non-secret slug plus curated local tools; no Arcade API key is required.
- [ ] First use performs discovered DCR + Authorization Code/PKCE through a loopback callback, and subsequent processes reuse/refresh the host-only credential-store grant without another login.
- [ ] Startup fails for an invalid slug, missing limits, duplicate local names, duplicate upstream names, or invalid schemas; configuration cannot supply an alternate URL, query, fragment, or credentials.
- [ ] A job for an unmapped principal receives no Arcade tools.
- [ ] `tools/list` to the worker includes only locally configured tools confirmed to exist on the Arcade gateway.
- [ ] An allowed call reaches only the constructed gateway URL and includes the current OAuth access token only in the upstream authorization header.
- [ ] An `ask` call uses the existing conversation-scoped approval before sending an upstream `tools/call`.
- [ ] A denied, malformed, replayed, cross-job, or schema-drifted call sends no upstream tool request.
- [ ] Arguments cannot alter destination, upstream tool, OAuth identity, credential, host scope, or policy.
- [ ] OAuth registrations, tokens, PKCE/state values, authorization codes, and upstream session IDs are absent from worker state and all model/audit output.
- [ ] A tool-level OAuth challenge becomes a bounded `AuthorizationRequired` event for the originating conversation and is never auto-approved or auto-replayed.
- [ ] Redirects, unsupported methods, malformed Streamable HTTP, oversized input/output, timeout, and ambiguous mutating-call failures fail closed.
- [ ] Cancellation aborts an in-flight request and a late response has no effect.
- [ ] Docker and native macOS workers discover/call Arcade tools only through the existing private broker socket and retain no general egress.
- [ ] Existing workspace capabilities remain behaviorally unchanged.

## Test seams

1. **Configuration seam:** slug/tool validation and trusted principal-to-grant binding, including unknown/duplicate fields and endpoint construction.
2. **Provider seam:** workspace capabilities work unchanged after extraction; provider name collisions and lease-visible descriptors fail closed.
3. **Arcade transport seam:** a fake Streamable HTTP gateway verifies metadata discovery, DCR, PKCE/state checks, token refresh/rotation, initialization, session headers, exact destination, auth headers, tool verification/call, no redirects, timeout, cancellation, content types, and bounded parsing.
4. **Broker seam:** verifies principal/job scope, normalization-before-approval, replay rejection, policy, limits, redacted audit, and revocation.
5. **Authorization seam:** fake Arcade challenge becomes a conversation-scoped authorization event; malformed or wrong-origin links are rejected.
6. **Worker seam:** adversarial Docker/native workers can call the curated tool through the private socket but cannot reach Arcade directly or observe credential canaries.
7. **Live opt-in seam:** an ignored-by-default interactive test uses a development gateway and the host credential store to verify redacted wire assumptions without committing secrets.

## Vertical delivery slices

1. Run and document the redacted Arcade Auth protocol spike; finalize discovery, DCR, PKCE, refresh, challenge, and session assumptions.
2. Extract existing workspace tools behind `CapabilityProvider` without changing behavior.
3. Add provider registration, duplicate-name rejection, and lease/principal-specific descriptor filtering.
4. Add slug-only configuration validation and an OS credential-store abstraction without tool network calls.
5. Implement OAuth discovery, DCR, browser/loopback authorization, persistence, refresh rotation, logout/revocation handling, and restart tests against a fake server.
6. Implement bounded Streamable HTTP initialization and configured-tool verification against a fake gateway.
7. Implement one read-only Arcade tool call with policy, projection, audit, timeout, cancellation, and secret-leak tests.
8. Add the operator authorization-challenge event and explicit retry behavior.
9. Add mutating-tool support only after transport ambiguity and no-retry tests pass.
10. Complete native integration and one opt-in live Arcade test; design remote User Source/OIDC separately.

Each slice must preserve [`../architectural-review.md`](../architectural-review.md). User Source/OIDC, Arcade Headers fallback, self-hosted gateways, arbitrary remote MCP servers, local `stdio` servers, and dynamic tool passthrough are separate future decisions.

## Explicitly rejected shortcuts

- Giving the worker Arcade's gateway URL, OAuth tokens/registration, authorization challenges, or direct network route.
- Letting a prompt choose or share an OAuth identity.
- Treating Arcade's gateway allowlist as a replacement for Pocket Agent policy.
- Registering every result of Arcade `tools/list` or trusting upstream descriptions as instructions.
- Running `arcade configure`, Arcade CLI, Python, `uv`, `npx`, or arbitrary MCP server code in the trusted host.
- Adding a generic `mcp.call(server, tool, arguments)` capability.
- Forwarding Arcade authorization challenges or raw results directly to the model.
- Automatically retrying a mutating tool after authorization or an ambiguous network failure.

## Primary sources

- [Arcade MCP Gateways](https://docs.arcade.dev/en/operate/governance/mcp-gateways)
- [Arcade gateway dashboard configuration](https://docs.arcade.dev/en/operate/governance/mcp-gateways/create-via-dashboard)
- [Arcade MCP client quickstart](https://docs.arcade.dev/en/get-started/quickstarts/call-tool-client)
- [Arcade MCP clients overview](https://docs.arcade.dev/en/get-started/mcp-clients)
- [Arcade server-level vs tool-level authorization](https://docs.arcade.dev/en/learn/server-level-vs-tool-level-auth)
- [Arcade MCP open-source framework](https://github.com/ArcadeAI/arcade-mcp)
