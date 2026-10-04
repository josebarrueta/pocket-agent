# Native macOS sandbox adapter

The `NativeJobFactory` runs the existing worker protocol as a local child process under macOS Seatbelt (`/usr/bin/sandbox-exec`). It is intended for Apple Silicon Macs that should run coding jobs without a Docker daemon.

## Configuration

Install the worker's integrity-locked JavaScript dependencies from the reviewed checkout:

```bash
npm ci --ignore-scripts --prefix docker/worker
```

Then use absolute paths for Node and the worker entrypoint:

```json
{
  "sandbox": {
    "runner": "native",
    "nodePath": "/opt/homebrew/bin/node",
    "workerPath": "/absolute/path/to/pocket-agent/docker/worker/worker.mjs"
  }
}
```

The adapter resolves symlinks before constructing the policy. The current native profile supports an ARM Homebrew Node installation under `/opt/homebrew`. Docker-only image and resource fields may be omitted.

## Effective controls

Each job receives a validated disposable workspace. The child has a cleared environment and receives only fixed runtime values and short-lived broker/model credentials. Seatbelt allows writes only to the disposable workspace and per-job runtime directory. Reads are limited to those directories, the worker and its dependencies, macOS runtime files, Homebrew runtime/tool files, and installed Xcode command-line tools. General network access is denied; authenticated broker and model-proxy Unix sockets are explicitly allowed. Cancellation and failure kill the worker's process group, revoke leases, and dispose the workspace.

The adapter retains the protocol deadline, output limit, and open-file limit. It does **not** currently provide Docker-equivalent CPU, memory, PID, or disk quotas. macOS Seatbelt is an undocumented/deprecated system interface and may change between OS releases. Native jobs run under the host user's UID and share the host kernel, installed toolchain, and software supply chain. Treat this as a narrower and less reproducible boundary than the hardened Docker runner, and do not interpret tool approvals as additional isolation.

The adapter fails closed on non-macOS systems, missing paths, malformed policies, inaccessible private sockets, or worker startup failure. It never falls back to an unrestricted child process.
