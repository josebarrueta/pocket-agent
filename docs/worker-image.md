# Pi worker image

`docker/worker/Dockerfile` builds the disposable job runtime for `linux/amd64` and `linux/arm64`. It contains Node.js, Pi, the protocol entrypoint, and an explicit baseline toolset: Bash, CA certificates, Git, GNU C/C++ and Make, Patch, Python 3, and ripgrep. It does not install a Docker/Podman client or copy configuration, credentials, repository content, or the host application into the image.

The entrypoint owns an in-memory Pi session and explicitly enables built-in `read`, `bash`, `edit`, and `write` tools rooted at `/workspace`. It disables project/global extensions, skills, prompt templates, and themes, then loads only three host-supplied inline extensions: tool approval policy, the authenticated workspace capability client, and the credential-free model proxy provider. Scoped MCP tools are translated to provider-safe Pi tool names and remain subject to host broker policy. The proxy provider accepts only safe model metadata and streams over its job socket; no provider key enters the image. Extension initialization errors fail the run closed. The entrypoint emits bounded tool status and implements start, steer, cancel, completion, failure, and approval messages. No Pi runtime or built-in execution tool is installed in the host application; the host uses only Pi AI provider adapters.

## Build and smoke test

Use BuildKit so `TARGETARCH` is populated and unsupported platforms fail clearly:

```bash
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --file docker/worker/Dockerfile \
  --tag pocket-agent/worker:0.1.0 \
  .

# Load the local platform and test the same restrictions used by CI.
docker buildx build --load --file docker/worker/Dockerfile --tag pocket-agent/worker:test .
docker run --rm \
  --read-only \
  --tmpfs /tmp:rw,nosuid,nodev,noexec,size=16m \
  --network none \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --pids-limit 64 \
  pocket-agent/worker:test --smoke-test
```

The smoke result must report UID/GID `65532`, Pi `0.87.1`, and protocol version `1`. The normal entrypoint accepts bounded newline-delimited JSON on standard input. It rejects malformed messages, unknown fields, unsupported versions, and command-line overrides. Session/settings state is in memory; only files written beneath the job workspace survive long enough to become a candidate patch.

## Provenance and inspection

Reproducibility comes from four reviewed pins:

1. the Dockerfile frontend digest;
2. the multi-platform Node base-image digest;
3. the timestamped Debian package snapshot;
4. exact npm versions and integrity hashes in `docker/worker/package-lock.json`.

Inspect image metadata and every filesystem layer:

```bash
docker image inspect pocket-agent/worker:test
docker image history --no-trunc pocket-agent/worker:test
docker sbom pocket-agent/worker:test       # Docker Scout plugin
# Alternative:
syft pocket-agent/worker:test -o spdx-json > worker.spdx.json
```

An SBOM is generated from the completed image rather than committed because it includes platform-specific native packages. Generate one for each published platform and attach it to the corresponding image digest.

## Updating pins

Treat every update as a supply-chain change:

1. Choose an exact Node release supported by Pi. Resolve the tag's multi-platform manifest digest with `docker buildx imagetools inspect node:<version>-bookworm-slim` and update both the readable tag and digest.
2. Advance the Debian snapshot timestamp deliberately. Review package changes shown in the build log.
3. Set an exact Pi version in `docker/worker/package.json`, run `npm install --package-lock-only --ignore-scripts --prefix docker/worker`, and verify the changed registry URLs and integrity hashes.
4. Keep `PI_VERSION`, the smoke-test expectation, and image documentation synchronized.
5. Run the Rust formatting, Clippy, unit, and Docker integration checks; build both worker platforms; run the hardened smoke test; inspect image history; and compare generated SBOMs before merging.

Do not copy provider credentials into the image or pass them as build arguments. Future workers receive model access through the job-scoped model proxy.
