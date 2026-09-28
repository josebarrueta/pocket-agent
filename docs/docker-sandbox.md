# Docker sandbox adapter

`DockerSandboxRunner` launches one container and one bounded workspace volume per job. The controller depends only on `SandboxRunner`; Docker commands, protocol transport, process supervision, workspace transfer, and cleanup remain inside the adapter.

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

The Docker executable path must be absolute and the configured image must use a `sha256` digest. The daemon reconciles labeled containers and volumes from a previous run before accepting messages. Run only one controller against a Docker daemon because reconciliation intentionally removes every resource carrying the `pocket-agent.managed=true` label.

Docker is the only production runner. The host forwards only configured model/thinking identifiers and tool policy; it does not forward provider credentials or its ambient environment. Model traffic remains unavailable until the job-scoped proxy from issue #8 is connected through a private network.

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

The adapter negotiates protocol v1, rejects malformed or oversized messages, ignores stale terminal events, and maps unexpected container exit to `worker_crash`. Deadline and operator cancellation force-remove the container, which kills its complete process tree. Disposal then force-removes both container and volume and is idempotent.

CI builds the worker for amd64 and arm64, inspects effective container settings, exercises multi-turn workspace export, simulates worker crash and timeout, and verifies orphan reconciliation. Fork and memory pressure are bounded by the same inspected kernel-enforced PID and memory settings.
