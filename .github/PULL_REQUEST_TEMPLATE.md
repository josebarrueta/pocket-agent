## What changed

<!-- Describe the user-visible behavior and the public seam it changes. -->

## Secure-wrapper boundary

For security-sensitive or architectural changes, link the design note/ADR and complete these checks. Mark non-applicable items with a short reason.

- [ ] Agent-selected execution remains inside an untrusted worker; no generic host execution path was added.
- [ ] No provider/connector credential, ambient host environment, or arbitrary host path enters a worker.
- [ ] Worker network access remains deny-by-default; new external access uses a fixed-destination bounded host adapter.
- [ ] Original repositories remain outside workers; mutation crosses the candidate-patch and typed-capability boundary.
- [ ] Ingress cannot bypass principal, conversation, job, repository, policy, or approval scope.
- [ ] Failure is closed: there is no unrestricted or broader-authority fallback.
- [ ] Authority, time, bytes, concurrency, lifecycle, revocation, and cleanup are bounded where applicable.
- [ ] Tests exercise behavior at the public security seam, including an adversarial failure case.
- [ ] Security, architecture, runner guarantees, and the isolation matrix were updated where applicable.

See [`docs/architectural-review.md`](../docs/architectural-review.md).

## Validation

<!-- List exact commands and any platform-specific checks run. -->
