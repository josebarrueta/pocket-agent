# Capability broker

The trusted host runs a deny-by-default MCP capability broker. It is a deep module behind two small interfaces:

- `CapabilityLeaseIssuer.issue(...)` creates one short-lived job lease for the sandbox adapter.
- `CapabilityBroker.list(...)` and `call(...)` are the authenticated test surface used by the MCP transport.

Capabilities are registered by trusted startup code. Workers cannot add tools or change policy. The initial broker intentionally registers no production capabilities; issue #7 adds the curated workspace tools. Generic execution names such as `host.exec` and `*.shell` are rejected at registration.

## Private transport

The broker listens on an owner-controlled Unix-domain socket under the state directory. `DockerSandboxRunner` bind-mounts only that socket directory read-only at `/run/pocket-agent-broker` and provides the socket location, opaque lease credential, and job ID to that job. It does not publish a TCP port or enable worker network access.

Host Unix-socket forwarding works with native Linux Docker and is tested in Linux CI. Docker Desktop for macOS cannot connect through a bind-mounted host Unix socket (`ENOTSUP`); capability calls are therefore not supported on that platform until a VM-local relay is implemented. The worker remains networkless and fails closed.

The endpoint accepts MCP JSON-RPC at `POST /mcp` and supports `initialize`, `notifications/initialized`, `tools/list`, and `tools/call`. Requests use a bearer lease and include the bound job ID in `_meta["pocket-agent/job-id"]`; calls also include a unique `_meta["pocket-agent/request-id"]`.

## Authorization

A lease binds:

- an unguessable credential stored only as a SHA-256 hash by the broker;
- job and conversation identity;
- configured repository alias (never a host path);
- an exact capability allowlist;
- expiry, call count, and output limits.

For every call the broker authenticates the credential and claimed job, consumes a one-time request ID, checks scope and limits, invokes the capability's normalizer, and applies `allow`, `ask`, or `deny` policy. Normalizers must return bounded JSON and derive host scope from `CapabilityContext`; callers cannot supply host paths.

An `ask` approval is bound to job ID, tool name, canonical normalized-argument digest, lease expiry, and a host-generated one-time nonce. Request IDs are also consumed once to reject replay or argument mutation. The broker authenticates the lease again after the asynchronous answer, so cancellation or expiry wins the race and the operation is not invoked.

Cancellation, timeout, creation rollback, disposal, and controller shutdown revoke leases synchronously before container cleanup. Credentials cannot be replayed after revocation.

## Audit records

The broker appends NDJSON records to `stateDir/audit/capabilities.ndjson`. Records contain timestamp, job ID, repository alias, capability name, normalized-argument SHA-256 digest, and outcome. Raw arguments, results, bearer credentials, message bodies, and provider secrets are not logged. Audit write failures fail the call rather than silently dropping the record.

## Verification

`test/capability-broker.test.ts` covers cross-job use, expiry, replay and mutation, scoped enumeration, approval/revocation races, generic-shell rejection, bounded output, and audit redaction. `test/docker-sandbox.integration.test.ts` verifies worker reachability and lease revocation on native Linux Docker.
