# Use a Rust host with transport-neutral ingress adapters

Pocket Agent's trusted host will be a small Rust binary built around a transport-neutral harness; the CLI, Signal, and future HTTP endpoint are ingress adapters at the same seam. The existing Node-based Pi worker remains isolated because Pi is a JavaScript runtime dependency, while host-side orchestration, workspaces, brokers, and native Anthropic/OpenAI-compatible model adapters move to Rust. This avoids making Signal part of the domain model, removes the host Node runtime, and preserves the existing worker protocol and isolation controls during an incremental migration.

## Consequences

The CLI is a first-class ingress rather than a debugging wrapper. Ingress adapters authenticate principals and format replies but cannot select host paths or bypass harness policy. Provider support is explicit: Anthropic and OpenAI-compatible adapters replace the broad JavaScript `pi-ai` catalog, while the worker-facing model protocol remains provider-independent. The TypeScript host stays available only until behavioral and adversarial parity tests pass.
