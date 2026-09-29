# Use a Rust host with transport-neutral ingress adapters

Pocket Agent's trusted host is a small Rust binary built around a transport-neutral harness; the CLI, Signal, and future HTTP endpoint are ingress adapters at the same seam. The Node-based Pi worker remains isolated because Pi is a JavaScript runtime dependency, while host-side orchestration, workspaces, brokers, and native Anthropic/OpenAI-compatible model adapters run in Rust. This avoids making Signal part of the domain model, removes the host Node runtime, and preserves the worker protocol and isolation controls.

## Consequences

The CLI is a first-class ingress rather than a debugging wrapper. Ingress adapters authenticate principals and format replies but cannot select host paths or bypass harness policy. Provider support is explicit: Anthropic and OpenAI-compatible adapters replace the broad host-side JavaScript provider catalog, while the worker-facing model protocol remains provider-independent. The TypeScript host was removed after the Rust compatibility and adversarial matrix passed.
