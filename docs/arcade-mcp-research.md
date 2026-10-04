# Arcade MCP Gateway integration research

Research date: 2026-09-29. Updated after reviewing Arcade's browser-OAuth gateway modes and persistence behavior.

## Findings

Arcade MCP Gateways are the correct integration point for Pocket Agent. A gateway federates selected tools from Arcade-hosted integrations, registered remote MCP servers, and custom Arcade-deployed MCP servers into one endpoint. Clients connect over MCP Streamable HTTP at `https://api.arcade.dev/mcp/{gateway-slug}`. The slug and the resulting URL are not secrets. [Arcade: MCP Gateways](https://docs.arcade.dev/en/operate/governance/mcp-gateways)

Arcade supports three gateway identity modes:

- **Arcade Auth**: project members sign in through browser OAuth. This is the preferred Pocket Agent mode for local development and internal use.
- **User Source**: production users sign in through an operator-configured OIDC provider. This is the preferred future mode for multi-user remote deployments.
- **Arcade Headers**: a fallback for MCP clients that cannot run browser OAuth or refresh tokens. It requires an Arcade API key and `Arcade-User-ID` on every request.

An Arcade API key is therefore **not required** for Pocket Agent's preferred integration. The earlier design chose Arcade Headers only because it was simpler for a noninteractive host. Pocket Agent can instead be a proper OAuth-enabled MCP client and construct the endpoint from one configured gateway slug. [Arcade: Gateway authentication](https://docs.arcade.dev/en/operate/governance/mcp-gateways#authentication)

## Gateway OAuth flow

Official Arcade documentation confirms that Arcade Auth gateways support MCP clients using browser OAuth and Dynamic Client Registration (DCR). Arcade also supports Client ID Metadata Documents (CIMD), but an installed local Pocket Agent cannot publish a stable HTTPS metadata document by default. The first implementation should therefore use DCR and persist the returned client registration rather than registering on every process start. [Arcade: Skip consent for trusted MCP clients](https://docs.arcade.dev/en/operate/governance/mcp-gateways#skip-consent-for-trusted-mcp-clients)

The client flow should follow MCP Authorization and OAuth 2.1 rather than hard-code undocumented Arcade endpoints:

1. Call the fixed gateway endpoint without credentials and process its `401` and `WWW-Authenticate` resource-metadata reference.
2. Fetch OAuth Protected Resource Metadata, then Authorization Server Metadata.
3. Dynamically register a public native client with loopback redirect URI support.
4. Start Authorization Code with PKCE (`S256`), a cryptographically random `state`, and the exact gateway resource/audience parameters discovered from metadata.
5. Bind a temporary callback listener only to loopback, open or print the authorization URL, verify `state`, and exchange the code.
6. Persist the client registration and token set in host-only secure storage.
7. Refresh before expiry, atomically rotating refresh tokens when the server returns a replacement. An `invalid_grant`, revocation, identity mismatch, or metadata-origin change deletes the unusable grant and requires explicit reauthorization.

Arcade's changelog documents support for OAuth gateway token refresh, DCR, authorization-server discovery, protected-resource behavior, PKCE fixes, and loopback redirect URIs on arbitrary ports. Exact wire behavior must still be captured in a redacted protocol spike before production code is merged. [Arcade changelog](https://docs.arcade.dev/en/references/changelog)

DCR means a new registration has no stable client ID suitable for Arcade's “skip consent” allowlist. Persisting the registration avoids needless re-registration and repeated gateway consent, but the user should still expect consent on the initial connection. A future Pocket Agent release may publish a CIMD URL to gain a stable client ID; that is a deployment/product decision, not required for slug-only local setup.

## Two authorization layers and persistence

Gateway OAuth and tool-level authorization are separate:

1. **Pocket Agent → Arcade gateway**: Pocket Agent stores its OAuth client registration and access/refresh tokens. These tokens authenticate the human to the gateway and must survive jobs and process restarts.
2. **Arcade tool → GitHub/Linear/etc.**: Arcade stores and refreshes the user's downstream provider grants. Arcade's tool-auth documentation says these grants are remembered until revoked, so Pocket Agent should not receive or store GitHub, Linear, Gmail, or other provider tokens. [Arcade: Add user authorization to MCP tools](https://docs.arcade.dev/en/build/create-tools/tool-basics/create-tool-auth)

The persistent Pocket Agent record must be keyed by the canonical gateway URL, trusted local principal, and auth mode/issuer. On macOS, secret material should be stored in Keychain. A permission-restricted state file may contain only non-secret metadata and opaque Keychain references. Other platforms need an equivalent OS credential-store design before this mode is enabled there. Tokens, authorization codes, PKCE verifiers, DCR secrets, and authorization URLs must never enter worker state, model context, logs, audit records, config files, or command arguments.

Tool-level OAuth remains just-in-time. When a call needs a downstream grant, Arcade returns an authorization challenge. Pocket Agent presents a validated HTTPS URL to the authenticated human as an `AuthorizationRequired` event. Arcade's documented behavior requires invoking the tool again after authorization. Pocket Agent must never automatically replay a mutating call; the operator explicitly retries or continues the turn.

## Tool curation and protocol reduction

Arcade's gateway allowlist is valuable but mutable. Pocket Agent must retain a second local allowlist with host-authored names, descriptions, schemas, policy, and bounded output projection. A dashboard change must not silently expand worker authority. Upstream `tools/list` verifies configured tools; it does not register capabilities dynamically.

The connector supports only MCP initialization, `tools/list`, and `tools/call`. It rejects prompts, resources, roots, sampling, server-initiated model calls, arbitrary elicitation, runtime tool-list expansion, and arbitrary remote server URLs. The worker keeps deny-by-default networking and reaches the connector only through Pocket Agent's authenticated capability broker.

## Current-directory operation

Local CLI commands may infer the enclosing Git worktree when `--repo` is omitted. This is a trusted local selection mechanism, not permission for prompts or remote ingress to submit host paths. Pocket Agent canonicalizes the worktree root, registers a process-local alias, snapshots tracked and non-ignored files into the bounded disposable workspace, and gives only that copy to the worker. The original checkout remains outside the sandbox and changes return as candidate patches for review and separately approved application.

Signal and future remote ingress continue to require configured repository aliases. They cannot select `cwd`, absolute paths, or arbitrary host directories.

## Recommended delivery order

1. Keep current-directory selection local-only and preserve disposable workspace/candidate-patch semantics.
2. Extract existing workspace operations behind `CapabilityProvider` without behavior changes.
3. Run a redacted Arcade Auth/DCR/PKCE protocol spike.
4. Add host credential-store and OAuth session abstractions, including restart/refresh/revocation tests.
5. Implement one read-only curated Arcade capability through the existing broker.
6. Add bounded human-facing tool authorization and explicit retry.
7. Add mutating tools only after ambiguous-failure and no-auto-replay tests pass.
8. Design User Source/OIDC separately for remote multi-user deployments.

## Primary sources

- [Arcade MCP Gateways](https://docs.arcade.dev/en/operate/governance/mcp-gateways)
- [Arcade gateway dashboard configuration](https://docs.arcade.dev/en/operate/governance/mcp-gateways/create-via-dashboard)
- [Arcade MCP clients](https://docs.arcade.dev/en/get-started/mcp-clients)
- [Arcade tool authorization](https://docs.arcade.dev/en/build/create-tools/tool-basics/create-tool-auth)
- [Arcade server-level vs tool-level authorization](https://docs.arcade.dev/en/learn/server-level-vs-tool-level-auth)
- [Arcade changelog](https://docs.arcade.dev/en/references/changelog)
