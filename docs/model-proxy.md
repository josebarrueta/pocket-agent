# Job-scoped model proxy

The trusted host owns provider credentials and executes provider adapters. A worker receives only:

- a read-only mount containing the private proxy Unix socket;
- a random, short-lived job credential;
- its job ID;
- non-secret metadata for the one configured model.

No provider key, provider URL, custom header, host credential file, or ambient cloud environment is copied into the worker. `DockerSandboxRunner` revokes the model lease synchronously on cancellation, timeout, creation rollback, worker failure disposal, and normal disposal.

## Request path

The worker registers an inline `pocket-agent-proxy` Pi provider. Its stream implementation sends Pi's normalized transcript and a small allowlist of generation options to `POST /v1/stream` over the Unix socket. The worker cannot choose a destination URL or forward headers. The host checks the bearer lease and job ID, then requires the exact provider/model fixed by trusted startup configuration.

The host uses Pi AI's provider implementation with the configured API key, optional trusted base URL, retries disabled, a hard timeout, and the worker disconnect/abort signal. Events are streamed back as bounded NDJSON. Provider diagnostics are removed and provider error text is replaced with a generic error before crossing into the worker.

Docker remains `network=none`; only the host process can contact the provider. Native Linux Docker supports the private socket mount. Docker Desktop for macOS cannot forward host Unix sockets through its VM, so model access fails closed on that platform until a VM-local relay is added.

## Limits and revocation

Each lease binds one job to one provider/model and enforces:

- one concurrent request;
- total requests per job;
- requests per rolling minute;
- output tokens per request;
- output tokens per job;
- request timeout, request bytes, and streamed event bytes;
- absolute lease expiry.

A disconnected worker aborts the upstream provider request. Revoked, expired, cross-job, unconfigured model, extra URL/header, and malformed requests are rejected before provider dispatch.

## Configuration and audit

`agent.model` is required in `provider/model-id` form. `agent.apiKeyEnv` names the host environment variable containing the provider credential; its value is read by the trusted host only. Optional host-trusted `agent.baseUrl` supports a compatible endpoint without allowing the worker to select destinations.

The host appends `stateDir/audit/models.ndjson` records containing timestamp, job ID, configured provider/model, token count, and outcome. It does not log prompts, responses, headers, URLs, credentials, or provider error bodies.

`test/model-proxy.test.ts` covers authentication and scope, arbitrary URL/header rejection, limits, timeout, provider-error redaction, revocation, and streaming cancellation. The Linux Docker integration test inspects the effective worker environment and runs a real Pi turn through a fake host backend.
