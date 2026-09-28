# Sandbox worker protocol v1

The sandbox runner is the host-side seam between the trusted controller and an untrusted per-job worker. The protocol is transport-independent; a Docker adapter may carry these messages over newline-delimited JSON, while tests use the in-memory adapter.

The TypeScript source of truth is [`src/sandbox-protocol.ts`](../src/sandbox-protocol.ts).

## Negotiation

Before any job messages, both peers exchange a `hello` containing `supportedVersions`. The host selects the highest version present in both lists. If there is no overlap, job creation fails before a worker can run. Every later message contains the selected `protocolVersion`; a message with another version is rejected.

Version 1 messages also contain a host-generated `jobId` and `runId`. `jobId` is the identity to which workspace and broker capabilities are scoped. `runId` identifies one `start`/terminal-event cycle within a persistent job and prevents a delayed event from an earlier turn completing a later one.

## Host-to-worker messages

- `start`: prompt, absolute ISO-8601 `deadlineAt`, and maximum UTF-8 `outputLimitBytes` for the completion.
- `steer`: additional input for the active run.
- `cancel`: termination request with reason `operator`, `deadline`, or `dispose`.

The worker must treat the deadline and output limit as hard limits. The host adapter independently enforces them and must forcibly terminate a worker that does not honor cancellation.

## Worker-to-host messages

- `status`: non-terminal progress text.
- `completion`: terminal output for a successful run.
- `failure`: terminal error with a stable `code`, human-readable `message`, and `retryable` flag. Version 1 codes are `worker_crash`, `deadline_exceeded`, `output_limit_exceeded`, and `internal_error`.

## Lifecycle invariants

1. A job has at most one active run. `steer` is valid only while that run is active.
2. Exactly one `completion` or `failure` is accepted for each `start`. Duplicate and stale terminal events are ignored.
3. The job deadline is absolute and covers all turns in that job. The host rejects work after it and sends `cancel` for active work.
4. Completion output is measured as UTF-8 bytes at the host. Oversized output becomes `output_limit_exceeded`.
5. `cancel` and `dispose` are idempotent. Disposal includes cancellation when needed and releases all implementation resources.
6. No worker event is delivered after disposal. Late status and terminal events are dropped.
7. A worker transport exit before a terminal event is reported as `failure` with code `worker_crash`.

The controller knows only `SandboxRunner` and `SandboxJob`. Process supervision, transport framing, forced termination, and container cleanup remain implementation details of the runner adapter.
