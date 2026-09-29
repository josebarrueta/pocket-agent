# Rust trusted host

The trusted host is migrating to one Rust binary. Pi remains in the isolated Node worker; the host does not link or initialize Pi.

## Ingress seam

`Harness` accepts a `HarnessRequest` containing an ingress ID, authenticated principal ID, conversation ID, request ID, and transport-neutral command. It emits structured `HarnessEvent` values through `ReplyPort`. The CLI is the first Rust ingress; Signal remains optional and is being ported at the same seam.

```bash
cargo build --release --locked
./target/release/pocket-agent --config config.json run --repo app --prompt 'Fix the parser'
./target/release/pocket-agent --config config.json shell --repo app
```

`run` exits after one turn. `shell` retains the in-memory job and accepts additional prompts until `/exit`. Approval events prompt on the terminal and are answered through the same principal-, ingress-, conversation-, and job-scoped harness path used by remote adapters.

## Current Rust adapters

- validated configuration; Signal settings are optional for CLI use;
- bounded disposable Git snapshots and patch export;
- hardened Docker job lifecycle and versioned worker protocol;
- job-scoped Unix-socket model leases;
- native Anthropic Messages and OpenAI-compatible Chat Completions adapters;
- provider/model, destination, rate, concurrency, request-token, job-token, timeout, byte, and audit controls.

The provider key is read from `agent.apiKeyEnv` into the host process. The worker receives a random revocable lease, fixed model metadata, and a read-only socket-directory mount. It does not receive the provider key or provider URL.

The Rust CLI currently exposes built-in worker tools and exports candidate changes; curated host workspace capabilities are still being ported. Signal continues to use the TypeScript host until its Rust adapter reaches parity. The TypeScript host will then be removed, leaving Node only in the worker image.

## Platform behavior

Private host Unix sockets work with a native Linux Docker daemon and are exercised in Linux CI. Docker Desktop for macOS cannot bind-mount the host socket through its VM, so model-backed Docker jobs fail closed there, as they do in the TypeScript implementation.
