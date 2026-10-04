# Be a secure remote wrapper, not an agent framework

Pocket Agent owns remote ingress, identity, isolation, credential mediation, scoped host capabilities, disposable workspaces, audit, and lifecycle control around replaceable agent runtimes. It does not own agent reasoning loops, coding-tool design, prompt ecosystems, model catalogs, IDE workflows, or a general plugin platform. Existing coding agents should run as untrusted workers behind the worker protocol rather than being reimplemented in the trusted host. This keeps the differentiated security boundary small and prevents convenience features from gradually moving arbitrary execution or credentials into the control plane.

## Consequences

A feature belongs in Pocket Agent only when it strengthens or uses the secure wrapper: ingress authentication, policy, isolation, mediation, workspace exchange, audit, or lifecycle. Agent-specific behavior belongs in a worker adapter. The host never gains a generic command-execution capability, workers never receive ambient host credentials or unrestricted network access, and changes to an original checkout cross an explicit typed capability and review boundary. New runners must fail closed, document their effective guarantees, and pass adversarial tests; weaker convenience runners must not be described as equivalent to hardened isolation.
