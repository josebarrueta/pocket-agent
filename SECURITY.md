# Security

Pocket Agent connects a remote message to coding tools that can read files, modify code, run processes, access credentials, use network services and spend model quota.

## Trust model

- Only exact `signal.allowedSenders` are accepted.
- Signal group messages are ignored.
- Repository paths and MCP processes are configured locally, never through chat.
- Jobs are scoped to the conversation that created them.
- Writes and shell commands require approval by default.
- MCP tools are denied unless explicitly listed or covered by a non-deny server policy.

## Important limitations

Approvals are not isolation. An allowed shell command can evade later checks, an agent may be prompt-injected by repository content, and an MCP process can act during startup. Signal transport security does not protect a compromised host or unlocked phone. `signal-cli` is unofficial and stores linked-device keys locally.

Use a dedicated OS account or container/VM, mount only required repositories, keep secrets out of the environment, restrict egress, enable backups, and review every approval. Pin dependencies and MCP container image digests. Never publish the Signal daemon port; the supplied compose file binds it to loopback.

## Secrets and logs

Protect `config.json`, Pi's credential directory, `signal-cli-data`, and the configured state directory with owner-only permissions. Tool arguments may be sent to Signal for approval and may contain sensitive data. The application does not intentionally log message bodies, but upstream processes may log diagnostics.

## Reporting

Use a private GitHub security advisory once a remote repository exists. Do not publish exploitable details before a fix is available.
