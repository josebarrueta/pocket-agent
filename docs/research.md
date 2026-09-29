# Feasibility research

Research date: 2025-09-27. Primary sources were preferred.

## Recommendation

Start with **Signal**, behind a transport seam, and add the official WhatsApp Cloud API later if needed.

Signal is the lower-friction personal control surface because `signal-cli` can be linked as a secondary device and offers a continuously receiving JSON-RPC daemon over HTTP, including an SSE event stream. The tradeoff is important: Signal has no official bot API; `signal-cli` describes itself as unofficial and says it must be kept current as Signal Server changes. Its account keys live on disk and must be protected. Pocket Agent therefore builds a minimal image directly from a pinned, checksum-verified upstream `signal-cli` native release rather than using a REST-wrapper image. [signal-cli README](https://github.com/AsamK/signal-cli) and [JSON-RPC manual](https://github.com/AsamK/signal-cli/blob/master/man/signal-cli-jsonrpc.5.adoc).

WhatsApp is feasible through Meta's official Cloud API, but it is designed for business messaging. Setup requires a Meta app, business portfolio/account, access token and a public webhook. Free-form outbound messages are allowed during the 24-hour customer-service window; outside it, approved templates are generally required. Webhooks can be duplicated and are retried for up to seven days, so an adapter must authenticate, deduplicate and acknowledge them. [Cloud API getting started](https://developers.facebook.com/docs/whatsapp/cloud-api/get-started), [Cloud API overview](https://developers.facebook.com/docs/whatsapp/cloud-api/overview/), and [webhook guide](https://developers.facebook.com/docs/whatsapp/cloud-api/guides/set-up-webhooks/).

## Agent integration

Pi is a good first agent because its SDK embeds a session directly, streams lifecycle/tool events, supports steering and follow-ups while running, and exposes `abort()` for cancellation. Persistent `SessionManager`s preserve conversation state. Pi extensions can register an `ask_operator` tool and can block tool calls before execution. [Pi SDK documentation](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/sdk.md) and [extension documentation](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md).

Other agents should sit behind the project's `JobFactory`/`JobHandle` seam. Claude Code, Codex, and other ACP-capable tools can be added without changing message routing. The reference project, acpbot, validates the broader shape: separate messaging and agent-host concerns, session control, queue/steer, cancellation, permissions, and MCP. [acpbot README](https://github.com/pmdroid/acpbot) and [security policy](https://github.com/pmdroid/acpbot/blob/main/SECURITY.md).

## MCP and safety

Pi intentionally does not ship an MCP client in core; MCP belongs in an extension. The official MCP TypeScript client supports tool discovery/calls and local stdio transports, and its stdio transport spawns without a shell. [Pi README](https://github.com/earendil-works/pi/tree/main/packages/coding-agent#philosophy) and [MCP TypeScript client guide](https://github.com/modelcontextprotocol/typescript-sdk/blob/main/docs/client.md).

An approval prompt is **not a sandbox**. An MCP server is executable code and may act at startup, before any tool approval. Therefore this project:

1. accepts MCP configuration only from a host-local file, never from chat;
2. requires an absolute executable path and uses argument arrays (no shell);
3. defaults undisclosed tools to `deny` and supports per-tool `ask`/`allow`/`deny`;
4. disables unrelated Pi extensions and project trust;
5. bounds MCP response buffers, calls and model-facing output;
6. recommends pinning an image digest and running untrusted servers in a read-only container with no network, no secrets, and only explicit mounts.

OAuth can be added for remote HTTP MCP servers using the official SDK's auth providers, but remote transports are deliberately excluded from the first release to avoid adding OAuth token handling, SSRF and confused-deputy risks before the local policy model is proven.

## Feasibility conclusion

The requested workflow is feasible now for Signal + Pi. The main operational risks are not protocol limitations; they are the power of coding agents, unofficial Signal integration maintenance, credential storage, and containment of agent/MCP processes. WhatsApp support is technically straightforward at the transport seam but operationally heavier.
