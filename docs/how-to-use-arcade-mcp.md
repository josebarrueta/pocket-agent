# How to use an Arcade MCP Gateway

Pocket Agent can expose selected tools from an Arcade MCP Gateway to a local coding job without an Arcade API key. Pocket Agent acts as an OAuth-enabled MCP client, stores its gateway grant in macOS Keychain, and keeps the worker offline.

This first implementation supports **local CLI jobs on macOS**. Signal and other remote ingress adapters do not receive Arcade capabilities yet.

## What stays where

```text
Isolated worker
  └─ curated Pocket Agent capability
       └─ trusted Pocket Agent host
            ├─ Arcade gateway OAuth grant → macOS Keychain
            └─ https://api.arcade.dev/mcp/{gateway-slug}
                 └─ GitHub / Linear / Datadog / other provider grant → Arcade
```

The gateway slug and local tool descriptors are configuration, not secrets. OAuth tokens never enter `config.json`, the worker, model context, approval text, or audit records.

## 1. Create an Arcade gateway

1. Sign in to the [Arcade Dashboard](https://api.arcade.dev/dashboard).
2. Create or select a project.
3. Create an MCP Gateway.
4. For **Who are the users of this Gateway?**, select **Members of this Project (Arcade Auth)**.
5. Select only the tools Pocket Agent should be able to use.
6. Save the gateway.
7. Copy the slug from its endpoint:

   ```text
   https://api.arcade.dev/mcp/YOUR-GATEWAY-SLUG
   ```

   Configure only `YOUR-GATEWAY-SLUG`, not the complete URL.

See Arcade's official [MCP Gateway guide](https://docs.arcade.dev/en/operate/governance/mcp-gateways) and [dashboard configuration guide](https://docs.arcade.dev/en/operate/governance/mcp-gateways/create-via-dashboard).

## 2. Choose and review tool descriptors

Arcade's gateway tool picker is the first allowlist. Pocket Agent deliberately requires a second local allowlist so a later dashboard change cannot silently expand worker authority.

For every tool you intend to expose, obtain its exact MCP `tools/list` descriptor from a trusted OAuth-capable MCP inspector/client or Arcade's official tool documentation. Record:

- the exact upstream `name`;
- the exact `inputSchema`;
- what data it reads or changes;
- whether Pocket Agent should `allow`, `ask`, or `deny` it.

The configured `inputSchema` must exactly equal Arcade's current descriptor. Pocket Agent disables calls when it detects schema drift. Do not copy tool names or schemas from model output, repository files, or an untrusted message.

Start with one read-only tool. Add mutating tools only after the read-only path works.

## 3. Configure Pocket Agent

Add `connectors.arcade` to `config.json`. Merge it with the existing repository, sandbox, and model settings.

```json
{
  "connectors": {
    "arcade": {
      "gatewaySlug": "YOUR-GATEWAY-SLUG",
      "requestTimeoutMs": 30000,
      "maxCallsPerJob": 30,
      "maxRequestBytes": 65536,
      "maxResponseBytes": 262144,
      "tools": [
        {
          "name": "arcade.github_get_issue",
          "upstreamName": "COPY_THE_EXACT_ARCADE_TOOL_NAME",
          "description": "Read one GitHub issue from the authorized account.",
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

The example name and schema are illustrative. Replace `upstreamName` and the complete `inputSchema` with Arcade's exact descriptor.

Configuration fields:

| Field | Purpose |
| --- | --- |
| `gatewaySlug` | Constructs the only permitted endpoint under `https://api.arcade.dev/mcp/`. |
| `requestTimeoutMs` | Bounds each OAuth or MCP request. |
| `maxCallsPerJob` | Limits Arcade calls made by one coding job. |
| `maxRequestBytes` | Bounds serialized tool arguments. |
| `maxResponseBytes` | Bounds each upstream response. |
| `tools[].name` | Stable worker-facing name; it must begin with `arcade.`. |
| `tools[].upstreamName` | Exact Arcade MCP tool name. |
| `tools[].description` | Locally reviewed description shown to the agent. |
| `tools[].inputSchema` | Closed JSON object schema pinned to Arcade's descriptor. |
| `tools[].policy` | `allow`, `ask`, or `deny`. Use `ask` for mutations. |

Pocket Agent rejects arbitrary gateway URLs, open-ended object schemas, duplicate names, unknown configuration fields, and unsafe limits.

## 4. Start Pocket Agent in the project

From the Git worktree you want the agent to inspect:

```bash
export ANTHROPIC_API_KEY='...'
pocket-agent --config /path/to/config.json shell
```

You can also select a configured repository alias:

```bash
pocket-agent --config /path/to/config.json shell --repo website
```

Current-directory mode still creates a disposable snapshot. The worker never receives or modifies the original checkout directly.

## 5. Complete gateway authorization

Ask the agent to use the configured read-only Arcade tool. On its first call, Pocket Agent prints an authorization URL:

```text
Arcade authorization required. Open this URL in your browser:
https://...
```

1. Open the URL on the same Mac.
2. Sign in to Arcade as a member of the gateway's project.
3. Review and approve the gateway consent request.
4. Allow the browser to return to Pocket Agent's loopback callback.

Pocket Agent uses OAuth discovery, Dynamic Client Registration, Authorization Code, PKCE `S256`, and a random state value. It stores the resulting client registration and token set in macOS Keychain.

Later jobs and Pocket Agent restarts reuse the Keychain grant. Expired access tokens are refreshed without another browser login while the refresh grant remains valid.

## 6. Complete tool-level authorization

Gateway login and provider authorization are separate. A GitHub, Linear, Datadog, or other tool may require its own account grant on first use.

When Arcade requests it, Pocket Agent prints a second validated Arcade URL. Open it, authorize the provider scopes, then explicitly ask the agent to retry the operation.

Pocket Agent does not automatically replay the tool call. This matters for operations such as opening a pull request, posting a comment, changing an issue, or triggering a deployment. Arcade stores and refreshes the downstream provider grant; Pocket Agent never receives that provider token.

## 7. Suggested policies

A conservative starting point is:

| Operation | Suggested policy |
| --- | --- |
| Read issue, PR, build status, or logs | `allow` |
| Search code or production logs containing sensitive data | `ask` |
| Create/update an issue or PR | `ask` |
| Post comments or send messages | `ask` |
| Merge, deploy, delete, rotate, or change permissions | `deny` initially |

Arcade's provider permissions still apply. Pocket Agent policy can narrow those permissions but cannot broaden them.

## 8. Example prompts

After configuring matching gateway tools:

```text
Use arcade.github_get_issue to read issue 142, inspect this disposable
workspace, and propose a patch. Do not create a PR yet.
```

```text
Use the configured logs tool to inspect errors for checkout-service over
the last 30 minutes. Do not mutate infrastructure.
```

```text
Review the candidate patch and, if it is ready, use the configured
ask-protected GitHub tool to open a pull request.
```

The agent can only call the local names present in `connectors.arcade.tools`; it cannot select another gateway, upstream tool, user identity, or network destination.

## 9. Disconnect or switch accounts

Delete the local gateway grant:

```bash
pocket-agent --config /path/to/config.json arcade logout
```

The next tool call starts browser authorization again. This removes Pocket Agent's gateway grant from Keychain; use Arcade or the downstream provider's account settings when you also need to revoke provider-level grants.

## Troubleshooting

### The tool is not visible

- Confirm the command is a local CLI `run` or `shell` job. Arcade capabilities are hidden from Signal.
- Confirm the local name starts with `arcade.`.
- Confirm its policy is not `deny`.
- Restart Pocket Agent after editing configuration.

### `Configured Arcade tool is unavailable`

The upstream name is not present in the gateway's current `tools/list`. Confirm that the tool remains selected in Arcade and copy its exact case-sensitive MCP name.

### `Configured Arcade tool schema changed`

Arcade's current schema no longer equals the locally reviewed schema. Review the change before updating `config.json`; do not bypass the check.

### Gateway authorization repeatedly fails

- Confirm the gateway uses **Arcade Auth**, not Arcade Headers.
- Confirm you are an Arcade project member.
- Complete the browser flow on the same Mac so the loopback callback is reachable.
- Run `pocket-agent ... arcade logout` and retry if the grant was revoked or corrupted.

### A provider asks for authorization again

The downstream grant may have expired, been revoked, or require additional scopes for a newly selected tool. Review the requested scopes, authorize them, and explicitly retry.

### A tool call times out

Pocket Agent does not automatically replay ambiguous calls. Check Arcade/provider activity before retrying a mutating operation.

## Current limitations

- macOS Keychain is the only implemented persistent credential store;
- Arcade tools are local-CLI-only;
- only Streamable HTTP tools are supported;
- prompts, resources, roots, sampling, generic elicitation, and arbitrary MCP servers are not exposed;
- tool descriptors must be copied and reviewed manually;
- the live Arcade wire flow still needs verification against an operator-provided development gateway.
