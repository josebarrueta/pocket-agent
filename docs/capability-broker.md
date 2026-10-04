# Capability broker

The trusted Rust host runs a deny-by-default MCP capability broker behind the small `WorkerAccessIssuer`/`WorkerLease` seam. It creates one short-lived job lease for the Docker adapter and serves authenticated MCP requests over a private Unix socket.

Workers cannot add tools or change policy. The broker always exposes the curated workspace tools documented in [`workspaces.md`](workspaces.md) and can expose an explicitly configured set of Arcade Gateway tools through the host-side `CapabilityProvider` seam. There is no worker-facing registration, generic host execution surface, arbitrary endpoint, or MCP passthrough. The Arcade provider design is recorded in [`stories/add-curated-mcp-connector.md`](stories/add-curated-mcp-connector.md), with primary-source protocol and authentication findings in [`arcade-mcp-research.md`](arcade-mcp-research.md).

## Private transport

The broker listens on an owner-controlled Unix-domain socket under the state directory. The Rust Docker adapter bind-mounts only that socket directory read-only at `/run/pocket-agent-broker` and provides the socket location, opaque lease credential, and job ID to that job. It does not publish a TCP port or enable worker network access.

Host Unix-socket forwarding works with native Linux Docker and is tested in Linux CI. The native macOS runner directly permits only the lease's Unix socket through Seatbelt. Docker Desktop for macOS cannot connect through a bind-mounted host Unix socket (`ENOTSUP`); capability calls therefore fail closed when that runner is selected. The worker remains without general network access.

The endpoint accepts MCP JSON-RPC at `POST /mcp` and supports `initialize`, `notifications/initialized`, `tools/list`, and `tools/call`. Requests use a bearer lease and include the bound job ID in `_meta["pocket-agent/job-id"]`; calls also include a unique `_meta["pocket-agent/request-id"]`. The worker translates the scoped MCP list into Pi extension tools; host policy still governs every invocation.

## Authorization

A lease binds:

- an unguessable credential stored only as a SHA-256 hash by the broker;
- job and conversation identity;
- configured repository alias (never a host path);
- an exact capability allowlist;
- expiry, call count, and output limits.

For every call the broker authenticates the credential and claimed job, consumes a one-time request ID, checks scope and limits, invokes the capability's normalizer, and applies `allow`, `ask`, or `deny` policy. Normalizers must return bounded JSON and derive host scope from `CapabilityContext`; callers cannot supply host paths.

An `ask` approval is bound to job ID, tool name, canonical normalized-argument digest, lease expiry, and a host-generated one-time nonce. Request IDs are also consumed once to reject replay or argument mutation. The broker authenticates the lease again after the asynchronous answer, so cancellation or expiry wins the race and the operation is not invoked.

Cancellation, timeout, creation rollback, disposal, and host shutdown revoke leases synchronously before container cleanup. Credentials cannot be replayed after revocation.

## Audit records

The broker appends NDJSON records to `stateDir/audit/capabilities.ndjson`. Records contain timestamp, job ID, repository alias, capability name, normalized-argument SHA-256 digest, and outcome. Raw arguments, results, bearer credentials, message bodies, and provider secrets are not logged. Audit write failures fail the call rather than silently dropping the record.

## Verification

Rust unit tests cover cross-job use, expiry, replay and mutation, scoped enumeration, approval/revocation behavior, generic-shell rejection, bounded output, and audit redaction. [`tests/rust_docker.rs`](../tests/rust_docker.rs) verifies worker reachability and read-only socket mounting on native Linux Docker; [`tests/rust_native.rs`](../tests/rust_native.rs) verifies native macOS socket access and surrounding filesystem/network denial.
