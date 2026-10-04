# Isolation verification matrix

The adversarial suite treats the Rust `DockerJobFactory`, `CapabilityBroker`, `ModelProxy`, and `WorkspaceManager` as the public security seams. Tests use malicious worker behavior rather than mocking Docker internals.

Native Linux is required for Docker private host Unix-socket tests. GitHub Actions runs that matrix on `ubuntu-latest`; Docker Desktop for macOS intentionally skips socket forwarding because its daemon runs in a VM. On macOS, `tests/rust_native.rs` exercises the Seatbelt runner's disposable workspace, host-file and internet denial, and capability socket access. Other unit tests remain portable.

| Architecture invariant / attack | Automated evidence |
| --- | --- |
| Agent-selected shell, read, write, and Pi execution remain outside the host process | Worker image smoke/protocol checks; Docker and native macOS worker tests; the Rust host has no Pi dependency |
| Image is pinned, read-only, non-root, capability-free, and `no-new-privileges` | Rust config tests; Docker inspection in `tests/rust_docker.rs`; CI multi-platform build and hardened smoke test |
| No host home, sibling repository, Signal key, credential store, original checkout, or Docker socket is available | Active boundary probe in `rust_worker_cannot_reach_host_files_ports_internet_secrets_or_docker`; workspace scanner tests; inspected mount allowlist |
| Only a disposable workspace and bounded temporary storage are writable | Docker mount inspection; disk-pressure integration test; workspace cleanup tests |
| CPU, memory, PID, open-file, workspace/tmpfs, runtime, protocol, and completion-output limits are effective | Docker hardening inspection; kernel/storage/output pressure and deadline tests |
| No inbound ports, internet egress, or host-port access | Docker network/port inspection and active boundary probes |
| The only host communication is authenticated broker/model Unix sockets | Native-Linux capability and real-worker model-proxy integration tests; request schema and destination checks |
| Job credentials cannot be replayed, altered, reused across jobs, or used after expiry/revocation | Capability and model proxy unit tests |
| Approvals bind one normalized operation and cancellation wins races | Harness approval scope tests and capability post-approval reauthentication |
| Traversal, links, hard links, submodules, binary/rename patches, oversize input, and target-link races fail closed | Rust workspace submission, export, scanner, and application tests |
| Provider and connector credentials do not enter worker environment, arguments, files, events, or audit records | Docker environment/argument inspection, active boundary probes, and redacted proxy/broker audit tests |
| Model/provider/destination scope, usage attribution, token/rate/concurrency limits, timeout, and streaming cancellation are enforced | Rust model proxy tests and native-Linux real-worker proxy integration |
| Broker/model outages fail closed without granting general network access | Networkless worker configuration, authenticated socket handling, and worker failure cleanup tests |
| Hangs, ignored cancellation, worker crashes, fork/memory/disk pressure, and huge output are contained and reclaimed | Rust Docker lifecycle, pressure, byte-limit, and orphan tests |
| Duplicate or stale activity cannot cross a run/job boundary | Versioned run identity validation and single-active-turn enforcement in `rust/docker.rs` and harness tests |
| Restart cleanup removes orphan containers, volumes, and stale disposable workspaces | Rust Docker reconciliation and workspace reclamation tests; startup invokes both |
| Cancellation, timeout, protocol violation, failed worker exit, and host shutdown revoke access and destroy resources | Rust Docker lifecycle tests and synchronous lease revocation |
| Repository selection is by trusted alias and jobs cannot cross ingress/principal/conversation scope | Harness, config, and workspace scope tests |
| Patch submission and host application remain separate, with application bound to the reviewed patch digest | Rust capability and workspace candidate/application tests |

## Manual verification

Some assumptions are outside what an unprivileged CI job can prove:

1. Review worker and Signal image base digests, package lock changes, Debian snapshot, upstream Signal checksum, and generated SBOM when updating dependencies.
2. Verify the production Docker daemon and host kernel are patched and not configured with unsafe authorization plugins, privileged defaults, or unexpected socket mounts.
3. Verify `config.json`, the state directory, provider environment, and `signal-cli-data` are owner-only on the host, and that configured repositories contain no secrets intended to be hidden from the agent.
4. On deployment, inspect one worker and compare its effective controls to [`docker-sandbox.md`](docker-sandbox.md). Docker Desktop cannot be accepted for broker/model socket transport until a VM-local authenticated relay exists.

These checks do not claim resistance to a Docker daemon or kernel compromise. See [`SECURITY.md`](../SECURITY.md) for remaining boundary and operational limitations.
