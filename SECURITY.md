# Security

Pocket Agent connects a remote message to coding tools that can read files, modify code, run processes, access credentials, use network services and spend model quota.

## Trust model

- Only exact `signal.allowedSenders` are accepted.
- Signal group messages are ignored.
- Repository paths and MCP processes are configured locally, never through chat.
- Jobs are scoped to the conversation that created them.
- Pi and all built-in filesystem/shell tools execute only in a constrained disposable worker.
- Writes and shell commands require approval by default; approvals are not the isolation seam.
- Workers have no network access; native Linux workers receive only a short-lived authenticated MCP lease over a private Unix socket. No production capabilities are registered yet.

## Important limitations

Approvals are not isolation. An allowed shell command can perform any action available inside its worker, and an agent may be prompt-injected by repository content. Docker isolation depends on the host kernel/VM and daemon configuration. Signal transport security does not protect a compromised host or unlocked phone. `signal-cli` is unofficial and stores linked-device keys locally.

The worker currently receives no provider credentials and has no network, so model-backed jobs remain unavailable until the model proxy is implemented. Capability transport currently requires native Linux Docker; Docker Desktop for macOS cannot forward the host Unix socket and access fails closed. Keep secrets out of repositories and the daemon environment, enable backups, review every approval and candidate patch, and pin worker image digests. Never publish the Signal daemon port; the supplied compose file binds it to loopback.

## Signal image provenance

The daemon image is built locally from the repository Dockerfile. Its final `scratch` stage contains the checksum-verified upstream `signal-cli` native executable, its required runtime libraries, and CA certificates—no shell, package manager, Java runtime, curl, or REST wrapper. Review changes to the Dockerfile, pinned version, archive digest, or base-image digest as security-sensitive supply-chain changes.

The separate `link-helper` image includes Debian, a shell, and `qrencode` for one-time setup. It is not the daemon image and should not remain running. Both images still trust the upstream `signal-cli` release artifact, Debian build-stage packages, Docker/build tooling, and the host kernel/VM. A locally authored image narrows and makes the contents auditable; it does not make upstream code inherently trusted.

The daemon requires outbound network access to Signal and persistent write access to `signal-cli-data`. All other filesystem content is read-only, capabilities are dropped, and inbound access is loopback-only. Do not loosen these controls without reviewing the consequence.

## Workspace isolation

Repository aliases resolve only from trusted startup configuration. Jobs receive a validated disposable snapshot containing tracked and non-ignored untracked regular files, never the configured checkout or its `.git` directory. Symlinks, hard links, submodules, special files, traversal, and oversized snapshots or patches are rejected. Candidate patches are not automatically applied. See [`docs/workspaces.md`](docs/workspaces.md).

## Worker image provenance

The worker image uses a digest-pinned multi-platform Node base, an immutable Debian package snapshot, and a separate integrity-locked npm dependency tree. It runs as numeric UID/GID `65532`; its Docker runtime has a read-only root filesystem, dropped capabilities, no network by default, and writable storage only for bounded temporary data and the disposable workspace. The image contains no Docker client/socket, host checkout, configuration, or credentials. Review changes to any image, package, snapshot, or runtime control as security-sensitive. See [`docs/worker-image.md`](docs/worker-image.md) and [`docs/docker-sandbox.md`](docs/docker-sandbox.md).

## Secrets and logs

Protect `config.json`, `signal-cli-data`, and the configured state directory with owner-only permissions. Tool arguments may be sent to Signal for approval and may contain sensitive data. Pi settings and sessions are in-memory inside each worker; no host Pi credential directory is mounted. The application does not intentionally log message bodies, but upstream processes may log diagnostics.

## Reporting

Use a private GitHub security advisory once a remote repository exists. Do not publish exploitable details before a fix is available.
