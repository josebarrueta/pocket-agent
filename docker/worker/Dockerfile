# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
ARG NODE_IMAGE=node:22.19.0-bookworm-slim@sha256:4a4884e8a44826194dff92ba316264f392056cbe243dcc9fd3551e71cea02b90

FROM ${NODE_IMAGE} AS dependencies
WORKDIR /opt/worker
COPY docker/worker/package.json docker/worker/package-lock.json ./
RUN npm ci --omit=dev --no-audit --no-fund \
    && npm cache clean --force

FROM ${NODE_IMAGE} AS runtime
ARG TARGETARCH
ARG IMAGE_REVISION=unknown
ARG PI_VERSION=0.87.1

# Keep OS packages reproducible by installing from an immutable Debian snapshot.
RUN case "${TARGETARCH}" in amd64|arm64) ;; *) echo "unsupported worker architecture: ${TARGETARCH}" >&2; exit 1 ;; esac \
    && rm -f /etc/apt/sources.list /etc/apt/sources.list.d/* \
    && printf '%s\n' \
      'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20260927T000000Z bookworm main' \
      'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20260927T000000Z bookworm-updates main' \
      'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian-security/20260927T000000Z bookworm-security main' \
      > /etc/apt/sources.list \
    && apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends \
      bash \
      ca-certificates \
      g++ \
      git \
      make \
      patch \
      python3 \
      ripgrep \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /opt/worker /workspace \
    && chown 65532:65532 /workspace

LABEL org.opencontainers.image.title="Pocket Agent worker" \
      org.opencontainers.image.description="Non-root Pi runtime for disposable Pocket Agent jobs" \
      org.opencontainers.image.source="https://github.com/josebarrueta/pocket-agent" \
      org.opencontainers.image.revision="${IMAGE_REVISION}" \
      org.opencontainers.image.version="${PI_VERSION}" \
      org.opencontainers.image.licenses="MIT"

COPY --from=dependencies /opt/worker/node_modules /opt/worker/node_modules
COPY --from=dependencies /opt/worker/package.json /opt/worker/package.json
COPY --chmod=0555 docker/worker/worker.mjs /opt/worker/worker.mjs

ENV HOME=/tmp/home \
    PATH=/opt/worker/node_modules/.bin:/usr/local/bin:/usr/bin:/bin \
    NODE_ENV=production

WORKDIR /workspace
USER 65532:65532
ENTRYPOINT ["node", "/opt/worker/worker.mjs"]
