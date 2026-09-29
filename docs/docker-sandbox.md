# Docker sandbox adapter

The Rust `DockerJobFactory` launches one container and one bounded workspace volume per job. The harness depends only on `JobFactory`/`JobHandle`; Docker commands, protocol transport, process supervision, workspace transfer, and cleanup remain inside the adapter.

## Configuration

Build and publish the worker image from [`worker-image.md`](worker-image.md), then configure its immutable manifest digest:

```json
{
  "sandbox": {
    "runner": "docker",
    "dockerPath": "/usr/bin/docker",
    "image": "registry.example/pocket-agent-worker@sha256:...",
    "cpus": 1,
    "memoryBytes": 1073741824,
    "pids": 256,
    "temporaryStorageBytes": 268435456,
    "workspaceStorageBytes": 805306368
  }
}
```

The Docker executable path must be absolute. The configured image must be either a complete local image ID (`sha256:...`) or a registry reference pinned by manifest digest (`name@sha256:...`). The host reconciles labeled containers and volumes from a previous run before accepting messages. Run only one host against a Docker daemon because reconciliation intentionally removes every resource carrying the `pocket-agent.managed=true` label.

Docker is the only production runner. The host forwards only safe model metadata, tool policy, and short-lived capability/model credentials; it does not forward provider credentials or its ambient environment. On native Linux, the broker and model-proxy Unix-socket directories are the only read-only host bind mounts. Both grant access only after job authentication and are documented in [`capability-broker.md`](capability-broker.md) and [`model-proxy.md`](model-proxy.md). The worker remains `network=none`.

## Effective controls

The job container is created with:

- numeric user/group `65532:65532`;
- read-only root filesystem;
- all Linux capabilities dropped and `no-new-privileges`;
- `network=none`, `ipc=none`, no published ports, and no Docker socket;
- configured CPU, memory, PID, nofile, runtime, and protocol-output limits;
- a bounded `noexec,nosuid,nodev` `/tmp`;
- logging disabled;
- one bounded local-driver `tmpfs` volume mounted at `/workspace`.

The original repository and host home are never mounted. A short-lived, networkless helper streams the validated disposable snapshot into the volume. On completion the worker is paused, a constrained helper streams the volume into a fresh staging directory, and that directory atomically replaces the host-side disposable workspace. The workspace module then rejects links, special files, traversal, excessive content, and excessive patches before returning a candidate patch.

The local-driver `tmpfs` volume requires a Linux Docker daemon, including Docker Desktop's Linux VM. Unsupported volume drivers fail during job creation rather than silently creating unbounded storage.

## Lifecycle

The adapter negotiates protocol v1, rejects malformed or oversized messages, ignores stale terminal events, and maps unexpected container exit to `worker_crash`. Worker-reported failures, process exit, output-limit failure, deadline, and operator cancellation revoke both leases and force-remove the container and volume through one idempotent cleanup path. Force removal kills the complete process tree even when a malicious worker ignores cancellation.

CI builds the worker for amd64 and arm64, inspects effective container settings, exercises multi-turn workspace export, actively probes host and internet isolation, simulates failures and resource pressure, and verifies orphan reconciliation. Fork, memory, disk, and output pressure are bounded by inspected kernel/runtime controls. See the complete [`isolation-verification.md`](isolation-verification.md) matrix.
