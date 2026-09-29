# Target architecture: isolated workers and a capability broker

Pocket Agent should treat agent-generated code, shell commands, and MCP clients as untrusted. The trusted control plane may coordinate work, but it must not execute agent-selected commands or expose the host filesystem directly.

## System diagram

```mermaid
flowchart LR
    operator["Operator"]
    terminal["Local terminal"]
    signalapp["Signal app"]
    signal["Signal service"]
    apiclient["Future HTTP client"]

    subgraph host["Trusted host"]
        direction LR

        signalcli["signal-cli<br/>hardened container"]
        ingress["Ingress adapters<br/>CLI | Signal | future HTTP"]

        subgraph control["Pocket Agent harness"]
            controller["Harness<br/>jobs, steer, cancel"]
            approvals["Approval broker"]
            manager["Sandbox manager<br/>create, stop, destroy"]
            audit[("Policy and audit log")]
        end

        subgraph broker["Capability broker"]
            gateway["MCP gateway<br/>authenticate, authorize, constrain"]
            workspace["Workspace adapter<br/>snapshot in / patch out"]
            connectors["Credentialed adapters<br/>tokens never leave host"]
        end

        modelproxy["Model proxy<br/>provider credentials"]
        repos[("Host repositories")]
        external["Approved external systems"]

        controller <--> approvals
        controller <--> manager
        approvals <--> gateway
        gateway --> audit
        gateway --> workspace <--> repos
        gateway --> connectors <--> external
    end

    subgraph worker["Untrusted per-job sandbox"]
        pi["Pi agent runtime"]
        tools["Shell and build tools"]
        copy[("Disposable workspace copy")]
        mcpclient["MCP client"]

        pi --> tools --> copy
        pi --> mcpclient
    end

    operator --> terminal --> ingress
    operator --> signalapp <-->|"encrypted messages"| signal
    signal <-->|"outbound Signal connection"| signalcli
    signalcli <-->|"loopback SSE and JSON-RPC"| ingress
    apiclient -.->|"future authenticated HTTP"| ingress
    ingress --> controller

    manager -->|"lifecycle and control channel"| pi
    workspace -->|"initial snapshot"| copy
    copy -->|"candidate patch"| workspace
    mcpclient -->|"private authenticated MCP transport"| gateway
    pi -->|"private model endpoint"| modelproxy
```

## Core rule

The harness is transport-neutral. CLI, Signal, and future HTTP entry points are ingress adapters that authenticate a principal and translate requests/replies; no ingress can bypass repository aliases, approvals, job ownership, or sandbox policy.

Arbitrary execution happens only inside a disposable worker sandbox. The host capability broker performs a small set of typed operations; it never provides a generic `host.exec` tool.

An agent can run `npm test` against `/workspace` inside its sandbox. It cannot run a command in the original host repository, read the host home directory, access the Docker socket, or receive long-lived provider and connector credentials.

## Modules and seams

### Sandbox manager

The `SandboxRunner` interface should stay small:

```ts
interface SandboxRunner {
  create(spec: JobSandboxSpec): Promise<SandboxJob>;
}

interface JobSandboxSpec {
  id: string;
  workspacePath: string;
  conversationId: string;
  repositoryScope: string;
  deadlineAt: Date;
  outputLimitBytes: number;
  events: {
    status(message: string): Promise<void>;
    approval?(request: SandboxApprovalRequest): Promise<string>;
  };
}

interface SandboxJob {
  start(prompt: string): Promise<string>;
  steer(message: string): Promise<void>;
  cancel(): Promise<void>;
  dispose(): Promise<void>;
}
```

Its implementation owns container or VM creation, limits, networking, workspace initialization, process supervision, and cleanup. The first adapter and its effective controls are documented in [`docker-sandbox.md`](docker-sandbox.md); a VM or native OS sandbox can be added at the same seam later. The versioned start, steer, cancel, status, completion, and failure messages—and their lifecycle invariants—are specified in [`worker-protocol.md`](worker-protocol.md).

### Capability broker

The capability broker exposes curated MCP tools such as:

- `workspace.read_metadata`
- `workspace.submit_patch`
- `github.get_issue`
- `github.create_pull_request`
- `secrets.perform_operation`

Each call is evaluated server-side against the authenticated job identity, repository scope, normalized arguments, configured policy, call and output limits, and any required operator approval. Configuration is loaded by the trusted host and cannot be modified through Signal or by the worker. The authenticated transport, lease lifecycle, audit format, and Linux platform constraint are documented in [`capability-broker.md`](capability-broker.md).

Approvals authorize one normalized operation, not a tool forever. An approval record binds the job ID, tool name, canonical argument digest, lease expiry, and a one-time request nonce. The broker re-authenticates after the answer, so cancellation or expiry wins approval races. Cancellation revokes outstanding approvals and the job's broker lease.

### Workspace adapter

The original host checkout is not mounted into the worker. The workspace adapter creates a disposable copy or worktree for the job and imports it into a sandbox-owned volume. When work completes, it returns a patch for review or applies an explicitly approved patch through the broker.

This avoids giving compromised worker code a path to unrelated repositories, host Git configuration, SSH keys, or editor credentials. Snapshot validation, patch limits, and crash-recovery cleanup are detailed in [`workspaces.md`](workspaces.md).

### Model proxy

The worker reaches a narrow host proxy over a private Unix socket and never receives provider credentials. The proxy binds requests to a job, fixes the configured provider/model and destination, applies concurrency, token, rate, byte, and timeout limits, redacts operational records, and revokes access with the sandbox. Details are in [`model-proxy.md`](model-proxy.md). Workers retain `network=none`; the broker and model proxy are explicit socket mounts, not general egress.

## Sandbox invariants

A job worker should have:

- a pinned, read-only runtime image;
- a non-root user and no added capabilities;
- no host home directory, original repository, credential store, or Docker socket;
- one writable disposable workspace and bounded temporary storage;
- CPU, memory, PID, runtime, and output limits;
- no inbound host ports;
- denied network egress except private broker/model endpoints;
- a per-job identity and short-lived broker credential;
- forced termination and capability revocation on cancel or timeout.

MCP is the mediation protocol, not the isolation mechanism. The sandbox provides isolation; the broker provides narrowly scoped access through explicit capabilities.

## Request flow

1. An allowlisted Signal sender starts a job using a configured repository alias.
2. The controller resolves the alias; the raw host path is never accepted from chat.
3. The workspace adapter prepares a disposable repository snapshot.
4. The sandbox manager starts a worker with that snapshot and a per-job identity.
5. Pi may execute arbitrary build commands only inside the worker.
6. A privileged operation is requested as an MCP tool call to the capability broker.
7. The broker denies it, allows it by policy, or asks the operator through Signal.
8. If approved, the broker performs exactly the normalized operation and records the result.
9. The worker submits a patch. Applying it to the host repository remains a separate policy or approval decision.
10. Cancellation, timeout, failure, or controller shutdown revokes the job identity and destroys the worker. A successful turn may leave the job idle for a later turn in the same in-memory Pi session.

## Current implementation

Pi, its built-in read/write/bash tools, and its in-memory session now run only inside the Docker worker. The host package no longer installs Pi or exposes a host-side agent/MCP adapter. Workers receive only a disposable workspace, safe model-selection metadata, and normalized approval responses; they receive no host environment or credentials.

The host exposes authenticated, job-scoped broker and model-proxy Unix sockets mounted read-only into native Linux workers. The worker registers its scoped tools and proxy provider with Pi. The broker exposes only metadata, patch submission/status, and separately approved patch application; the model proxy fixes one trusted provider/model without disclosing its credential. Docker Desktop for macOS cannot forward host Unix sockets and fails both paths closed. Approvals reduce accidental tool use inside the disposable workspace; sandbox isolation—not approval—is the security seam.

The trusted host is being migrated from the initial TypeScript/Signal composition to a Rust harness with transport-neutral commands and structured replies. The Rust harness seam and scoped principal/conversation model are implemented alongside the TypeScript host while sandbox, workspace, broker, model, and ingress adapters are ported. The Node worker remains intentional because it contains Pi; it is not part of the trusted host runtime. See [ADR 0001](adr/0001-rust-host-and-ingress-adapters.md).

## Recommended migration order

1. Define the sandbox job protocol and add a fake `SandboxRunner` for controller tests.
2. Package a pinned worker image containing Pi and required build tools.
3. Give every job a disposable workspace copy; remove direct host repository access from Pi.
4. Run shell/read/write only in the worker and enforce hard resource limits.
5. Move the MCP gateway to the trusted host and add per-job authentication and scope enforcement.
6. Add a model proxy so provider credentials do not enter workers.
7. Make patch export/application an explicit capability with approval and audit records.
8. Add adversarial integration tests for path traversal, symlinks, cancellation, credential leakage, network escape, replayed approvals, and orphaned workers.
