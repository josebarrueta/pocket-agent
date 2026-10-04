# Architectural review: secure remote wrapper

## Product boundary

Pocket Agent is a secure remote wrapper around replaceable coding agents. Its value is not a better reasoning loop; it is safely turning an authenticated remote request into bounded work by an untrusted agent runtime and returning a reviewable result.

Pocket Agent owns:

- authenticated local and remote ingress;
- principal, conversation, job, and approval scope;
- worker isolation and lifecycle;
- provider-credential mediation;
- deny-by-default typed host capabilities;
- disposable workspace creation and candidate-patch exchange;
- limits, revocation, cleanup, and security audit records.

Pocket Agent deliberately delegates:

- reasoning and planning loops;
- prompts and coding behavior;
- filesystem, shell, and editing tools inside the worker;
- model-specific interaction beyond the bounded host proxy;
- IDE experiences and general plugin ecosystems.

Pi is the first agent runtime, not part of the trusted product boundary. Another agent runtime is acceptable when it speaks the worker protocol and satisfies the same isolation tests.

## Trust boundaries

The operator, trusted configuration, Rust host, repository originals, provider credentials, and connector credentials are trusted. Prompts, repository contents, model output, agent runtimes, MCP clients, generated code, and every agent-selected command are untrusted.

No sandbox is “complete.” Kernel, sandbox-runtime, daemon, host-process, and dependency compromise remain outside the boundary. Each runner must state its actual guarantees. The Docker runner is the hardened unattended baseline. The native macOS runner is a convenience boundary with shared host UID and weaker resource containment; it must not silently weaken or replace Docker guarantees.

## Non-negotiable invariants

Every production feature and runner must preserve these properties:

1. **Arbitrary execution stays in a worker.** The trusted host has no generic shell, script, package, hook, or MCP execution path.
2. **The original checkout stays outside the worker.** A worker receives a disposable validated snapshot. Host mutation uses a typed, scoped operation against a reviewed candidate patch.
3. **Credentials stay in the trusted host.** Workers receive short-lived, job-scoped opaque leases, never provider keys, connector tokens, host credential files, or the ambient host environment.
4. **Network is deny-by-default.** Model and capability access use authenticated private endpoints. Any additional destination requires a bounded host adapter; workers do not receive general egress.
5. **Host authority is typed and narrow.** The capability broker exposes normalized domain operations, never `host.exec`, arbitrary paths, arbitrary URLs, or caller-selected credentials.
6. **Identity and scope are checked at every boundary.** Ingress, job, conversation, repository, run, capability, approval, and lease identities cannot be inferred from worker claims alone.
7. **Approvals are authorization, not isolation.** An approval covers one normalized operation and cannot compensate for an unsafe worker boundary.
8. **Lifecycle revokes authority.** Cancellation, timeout, protocol failure, worker failure, and host shutdown revoke leases and terminate the complete worker process tree before workspace disposal.
9. **Failure is closed and visible.** Unsupported isolation, missing mediation, invalid configuration, or unavailable private transport stops the job. There is no unrestricted fallback.
10. **Accepted outputs are bounded and reviewable.** Protocol traffic, model use, capability results, imported/exported workspace content, and patches are validated against explicit limits. Agent changes are returned as a manifest and candidate patch rather than silently applied. Runtime resource containment remains a documented runner guarantee and is weaker on native macOS.

## Runner admission rule

A new sandbox runner is an adapter, not a relaxation of policy. Before it can be enabled it must document and test:

- filesystem read/write boundaries;
- process-tree termination;
- environment and credential exclusion;
- network denial and private endpoint access;
- workspace import/export integrity;
- runtime, output, and available resource limits;
- crash, timeout, cancellation, and restart cleanup;
- platform and kernel assumptions;
- known differences from the hardened baseline.

A runner that cannot enforce an invariant must fail startup or be explicitly classified as a weaker convenience runner. Marketing, defaults, and unattended deployment guidance must reflect that classification.

## Feature admission rule

Before implementing a feature, answer these questions in its issue or design note:

| Question | Required outcome |
|---|---|
| Does agent-selected code execute in the trusted host? | No. Move it into a worker. |
| Does it expose a secret or ambient host state to a worker? | No. Add a scoped host proxy or broker adapter. |
| Does it require arbitrary network egress? | No. Add a fixed-destination, bounded host adapter. |
| Can it read an arbitrary host path or mutate the original checkout? | No. Use aliases, snapshots, candidate patches, and typed capabilities. |
| Can an ingress bypass harness ownership, policy, or approval? | No. All ingress remains translation and authentication only. |
| Is behavior specific to Pi or another coding agent? | Put it behind the worker protocol, not in the host domain. |
| Does failure degrade to unrestricted execution or broader authority? | No. Fail closed. |
| Are authority, bytes, time, concurrency, and cleanup bounded? | Yes, with tests at the public security seam. |
| Does the audit record avoid prompts, source, credentials, and unnecessary tool output? | Yes. Record decisions and bounded metadata. |

A “no” in a required-outcome column is a design rejection, not a request for an approval dialog.

## Explicit non-goals

Pocket Agent will not compete with coding agents on reasoning quality, recreate their tool ecosystems, provide a generic remote shell, execute arbitrary third-party MCP servers in the trusted host, expose a general credential vault to workers, or become an IDE. Those capabilities increase trusted surface without strengthening the secure-wrapper thesis.

## Review cadence

Revisit this review when adding a runner, ingress, credentialed connector, host capability, automatic patch application mode, persistent worker state, or general network access. Security-sensitive changes should update the threat documentation and adversarial matrix in the same change. Architectural boundary changes require an ADR.

This review applies [ADR 0002](adr/0002-secure-wrapper-not-agent-framework.md) to the target architecture in [`architecture.md`](architecture.md).
