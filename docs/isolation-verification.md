# Isolation verification matrix

The adversarial suite treats `DockerSandboxRunner`, `CapabilityBroker`, `ModelProxy`, and `DisposableWorkspaceManager` as the public security seams. Tests use malicious fixture behavior rather than mocking Docker internals.

Native Linux is required for private host Unix-socket tests. GitHub Actions runs the full matrix on `ubuntu-latest`; Docker Desktop for macOS intentionally skips socket forwarding because its daemon runs in a VM. Unit tests and non-socket Docker tests run on both platforms.

| Architecture invariant / attack | Automated evidence |
| --- | --- |
| Agent-selected shell, read, write, and Pi execution remain outside the host process | `worker-entrypoint.test.ts`; worker image smoke test; the host dependency tree excludes `pi-coding-agent` |
| Image is pinned, read-only, non-root, capability-free, and `no-new-privileges` | `config.test.ts`; `worker-entrypoint.test.ts`; `Docker adapter applies hard isolation settings…`; CI multi-platform build and hardened smoke test |
| No host home, sibling repository, Signal key, credential store, original checkout, or Docker socket is available | `malicious worker cannot reach host files…`; workspace snapshot tests; inspected mount allowlist |
| Only a disposable workspace and bounded temporary storage are writable | Docker mount inspection; `Docker byte and storage limits…`; workspace cleanup tests |
| CPU, memory, PID, open-file, workspace/tmpfs, runtime, protocol, and completion-output limits are effective | Docker hardening inspection; `Docker kernel limits…`; `Docker byte and storage limits…`; deadline and protocol unit tests |
| No inbound ports, internet egress, or host-port access | Docker network/port inspection and active probes in `malicious worker cannot reach…` |
| The only host communication is authenticated broker/model Unix sockets | Native-Linux broker and real-worker model-proxy integration tests; arbitrary URL/header rejection tests |
| Job credentials cannot be replayed, altered, reused across jobs, or used after expiry/revocation | `capability-broker.test.ts`; `model-proxy.test.ts` |
| Approvals bind one normalized operation and cancellation wins races | capability broker approval/race tests; approval broker scope tests |
| Traversal, links, hard links, submodules, binary/rename patches, oversize input, and target-link races fail closed | `workspace.test.ts`; `workspace-capabilities.test.ts` |
| Provider and connector credentials do not enter worker environment, arguments, files, events, or audit records | active Docker environment/argument probes; generic host-path/environment probes; provider-error and audit-redaction tests. Credential files are absent because the inspected mount allowlist contains only the disposable volume and private socket directories |
| Model/provider/destination scope, usage attribution, token/rate/concurrency limits, timeout, and streaming cancellation are enforced | `model-proxy.test.ts`; native-Linux real-worker proxy integration |
| Broker/model outages fail closed without leaking lease credentials | `worker-entrypoint.test.ts` |
| Hangs, ignored cancellation, worker crashes, fork/memory/disk pressure, and huge output are contained and reclaimed | Docker deadline, cancellation, pressure, and byte/storage integration tests |
| Duplicate/late terminal events cannot settle a run twice | `sandbox.test.ts`; run/job identity checks in Docker integration |
| Restart cleanup removes orphan containers, volumes, and stale disposable workspaces | Docker reconciliation and workspace reclamation tests; CLI invokes both before accepting work |
| Cancellation, timeout, protocol violation, failed worker exit, and controller shutdown revoke access and destroy resources | Docker lifecycle tests; broker/model revocation tests; controller failure/cancellation tests |
| Repository selection is by trusted alias and jobs cannot cross conversations | `controller.test.ts`; `config.test.ts`; workspace scope tests |
| Patch submission and host application remain separate, with application bound to the reviewed patch digest | `workspace-capabilities.test.ts`; workspace submission/application tests |

## Manual verification

Some assumptions are outside what an unprivileged CI job can prove:

1. Review worker and Signal image base digests, package lock changes, Debian snapshot, upstream Signal checksum, and generated SBOM when updating dependencies.
2. Verify the production Docker daemon and host kernel are patched and not configured with unsafe authorization plugins, privileged defaults, or unexpected socket mounts.
3. Verify `config.json`, the state directory, provider environment, and `signal-cli-data` are owner-only on the host, and that the configured repository aliases contain no secrets intended to be hidden from the agent.
4. On deployment, inspect one worker with `docker inspect` and compare its effective controls to [`docker-sandbox.md`](docker-sandbox.md). Docker Desktop cannot be accepted for broker/model socket transport until a VM-local authenticated relay exists.

These checks do not claim resistance to a Docker daemon or kernel compromise. See [`SECURITY.md`](../SECURITY.md) for the remaining boundary and operational limitations.
