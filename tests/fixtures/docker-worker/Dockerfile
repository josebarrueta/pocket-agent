ARG BASE_IMAGE=pocket-agent/worker:test
FROM ${BASE_IMAGE}
COPY --chmod=0555 tests/fixtures/docker-worker/worker.mjs /opt/worker/worker.mjs
