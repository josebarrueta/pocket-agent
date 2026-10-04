# Rust trusted host

The trusted host is one Rust binary. Pi remains in the isolated Node worker; the host does not link or initialize Pi, and its deployment requires no host Node runtime.

## Ingress seam

`Harness` accepts a `HarnessRequest` containing an ingress ID, authenticated principal ID, conversation ID, request ID, and transport-neutral command. It emits structured `HarnessEvent` values through `ReplyPort`. CLI and Signal are peer Rust ingress adapters at this seam; Signal remains optional.

```bash
cargo build --release --locked
# From a Git worktree, local commands default to a disposable snapshot of cwd.
./target/release/pocket-agent --config config.json run --prompt 'Fix the parser'
./target/release/pocket-agent --config config.json shell
# Or select an explicitly configured alias.
./target/release/pocket-agent --config config.json run --repo app --prompt 'Fix the parser'
./target/release/pocket-agent --config config.json serve signal
```

`run` exits after one turn. `shell` retains the in-memory job and accepts additional prompts until `/exit`. Approval events prompt on the terminal and are answered through the same principal-, ingress-, conversation-, and job-scoped harness path used by remote adapters.

## Current Rust adapters

- validated configuration; Signal settings are optional for CLI use;
- bounded disposable Git snapshots and patch export;
- hardened Docker job lifecycle and versioned worker protocol;
- job-scoped Unix-socket capability and model leases;
- curated workspace metadata, candidate patch, review, and approved application capabilities;
- native Anthropic Messages and OpenAI-compatible Chat Completions adapters;
- provider/model, destination, capability scope, replay, rate, concurrency, call, token, timeout, byte, and redacted audit controls.

The provider key is read from `agent.apiKeyEnv` into the host process. The worker receives a random revocable lease, fixed model metadata, and a read-only socket-directory mount. It does not receive the provider key or provider URL.

The Rust host exposes built-in worker tools, four curated workspace capabilities, and any explicitly configured local-only Arcade capabilities. Capability scope comes only from the authenticated job lease; no capability accepts a host path, generic command, or arbitrary network destination. Applying a submitted canonical patch and Arcade tools configured with `ask` require conversation- and job-scoped operator approval.

`serve signal` requires the optional `signal` configuration, waits for the local `signal-cli` HTTP/SSE daemon, accepts only allowlisted private senders, ignores group and sync messages, and formats structured harness events back into bounded Signal messages. CLI commands do not require Signal configuration. Signal account keys remain in the separately hardened daemon and never enter workers.

## Footprint

The release profile enables thin LTO, one codegen unit, abort-on-panic, and symbol stripping. A local macOS arm64 release build measured 3,389,728 bytes (3.2 MiB); a `--help` startup measured 3,276,800 bytes peak RSS. These are reproducible reference measurements rather than cross-platform guarantees. Linux CI builds the stripped binary and enforces a 15 MiB upper bound to catch accidental trusted-host growth. Node and the Pi dependency tree exist only in the worker image.

## Platform behavior

Private host Unix sockets work with a native Linux Docker daemon and are exercised in Linux CI. Docker Desktop for macOS cannot bind-mount the host socket through its VM, so model- and capability-backed Docker jobs fail closed there.
