# Pocket Agent

A private Signal control plane for a coding agent running on your machine.

From Signal you can report a bug, start a Pi task in an allowlisted repository, receive progress and final output, answer questions/permission prompts, steer active work, cancel it, and switch between sessions. MCP tools are exposed through a deny-by-default gateway.

> **Early MVP:** use on a development machine with backups. Coding agents and MCP servers can execute code with the daemon user's privileges. Chat approvals reduce accidents; they do not provide process isolation.

## Why Signal first?

Signal works for a private, self-hosted workflow through the unofficial `signal-cli` daemon and does not need a public webhook. WhatsApp support fits the same `Messenger` interface, but its official Cloud API requires business setup, a public webhook, and template handling outside the 24-hour customer-service window. See [`docs/research.md`](docs/research.md).

## Architecture

```text
Signal app
   │ encrypted Signal message
   ▼
signal-cli (loopback only)
   │ SSE + JSON-RPC
   ▼
Message adapter ──► Controller ──► AgentRun seam ──► Pi SDK
                         │                         ├─ built-in tools + approvals
                         │                         └─ MCP gateway + approvals
                         └─ sessions / cancel / steer
```

The deep seams are intentionally small:

- `Messenger`: incoming messages and outbound text. A WhatsApp adapter can replace Signal.
- `AgentFactory` / `AgentRun`: start, steer, cancel and dispose. ACP/Claude Code/Codex adapters can be added later.
- `ApprovalPort`: turns blocking agent questions and tool permissions into chat requests.

## Current capabilities

- `/new <repo> <task>` starts a persistent Pi conversation.
- `/bug <repo> <description>` asks Pi to reproduce, fix and test a bug.
- Plain text or `/steer` continues/redirects the selected session.
- `/answer` resolves agent questions, Pi tool approvals and MCP approvals.
- `/cancel`, `/jobs`, `/use`, and `/status` control concurrent sessions.
- Incoming senders and repositories are host-configured allowlists.
- Pi writes and shell calls default to explicit approval.
- MCP uses local stdio only, absolute executables, no shell, deny-by-default tool policy, timeouts and output limits.
- Signal group messages are ignored in the MVP; only allowlisted private senders are accepted.

## Prerequisites

- Node.js 20.12+
- Pi credentials already configured (`pi /login` or provider API environment variables)
- Docker (recommended for `signal-cli` and MCP isolation)
- A Signal account. Linking `signal-cli` as a secondary device is recommended.

## Setup

```bash
npm install
cp config.example.json config.json
cp docker-compose.signal.yml docker-compose.yml
mkdir -p signal-cli-data
docker compose up -d
```

Link Signal before using `json-rpc` mode. The easiest route is to temporarily set `MODE: normal`, restart, open:

```text
http://127.0.0.1:8080/v1/qrcodelink?device_name=pocket-agent
```

Scan it in Signal under **Settings → Linked devices**, then restore `MODE: json-rpc` and restart the container. Keep `signal-cli-data` private; it contains account cryptographic material.

Edit `config.json`:

- `account`: the linked Signal account in international format.
- `allowedSenders`: exact trusted sender number(s) or UUID(s). Pairing is never performed over chat.
- `repositories`: chat-safe aliases mapped to absolute local paths.
- `agent.model`: optional `provider/model-id`; omit it to use Pi's configured default.
- `permissions`: `allow`, `ask`, or `deny` for reads, writes, and shell calls.

Then:

```bash
npm run check
npm run dev -- ./config.json
```

Send `/help` to the linked account from an allowlisted Signal account.

## Example conversation

```text
You: /bug website checkout hangs after an expired session
Bot: 🚀 [ab12cd34] Starting in website.
Bot: 🔧 read
Bot: ❓ [1] Allow Pi tool bash?
     { "command": "npm test -- checkout" }
     Reply: /answer 1 <answer>
You: /answer 1 yes
Bot: ✅ [ab12cd34]
     Reproduced the race, fixed ..., and all checkout tests pass.
```

## MCP safety model

MCP servers are executable programs, not passive tool descriptions. A malicious server can act as soon as it starts. For that reason:

- MCP config is read only at daemon startup and cannot be changed from Signal.
- Commands must be absolute paths and are spawned directly, without a shell.
- Every server defaults to denying every tool.
- Tool calls can require an operator approval showing their arguments.
- Project and global Pi extensions are disabled for hosted sessions.

For untrusted MCP servers, configure the executable as the absolute Docker/Podman path and use a pinned image digest, `--network=none`, `--read-only`, a non-root user, resource limits, no host secrets, and only narrowly scoped read-only mounts. The example config shows the shape. Do the same for the **agent daemon itself** if you require a hard security boundary.

## Deliberate MVP limits / roadmap

1. Add crash-safe job metadata restoration (Pi transcripts are already persisted, controller job selection is not).
2. Add an official WhatsApp Cloud API adapter with signature verification and webhook deduplication.
3. Add ACP-backed agent adapters for Claude Code, Codex and other clients.
4. Add a first-class container runner for both agents and MCP servers.
5. Add attachments, git worktrees, schedules, and richer progress summaries.

## Development

```bash
npm test
npm run build
npm run check
```

See [`SECURITY.md`](SECURITY.md) before exposing the daemon or adding MCP servers.
