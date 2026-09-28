# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
# signal-cli's published native binary is currently x86-64. docker-compose
# deliberately requests linux/amd64, including on Apple Silicon hosts.
FROM --platform=linux/amd64 debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS fetch

ARG SIGNAL_CLI_VERSION=0.13.20
ARG SIGNAL_CLI_SHA256=7af2f0b09dc2e05c2d97284f8967d05e697afe7df4866624aae2302610a4f841

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && curl --fail --location --proto '=https' --tlsv1.2 --retry 3 \
      --output /tmp/signal-cli.tar.gz \
      "https://github.com/AsamK/signal-cli/releases/download/v${SIGNAL_CLI_VERSION}/signal-cli-${SIGNAL_CLI_VERSION}-Linux-native.tar.gz" \
    && echo "${SIGNAL_CLI_SHA256}  /tmp/signal-cli.tar.gz" | sha256sum --check --strict \
    && tar --extract --gzip --file /tmp/signal-cli.tar.gz --directory /usr/local/bin \
    && test "$(find /usr/local/bin -maxdepth 1 -type f -name signal-cli | wc -l)" = 1 \
    && chmod 0555 /usr/local/bin/signal-cli \
    && mkdir -p /runtime/lib64 /runtime/lib/x86_64-linux-gnu /runtime/etc/ssl/certs \
    && cp -L /lib64/ld-linux-x86-64.so.2 /runtime/lib64/ \
    && cp -L /lib/x86_64-linux-gnu/libc.so.6 /runtime/lib/x86_64-linux-gnu/ \
    && cp -L /lib/x86_64-linux-gnu/libgcc_s.so.1 /runtime/lib/x86_64-linux-gnu/ \
    && cp -L /lib/x86_64-linux-gnu/libm.so.6 /runtime/lib/x86_64-linux-gnu/ \
    && cp -L /lib/x86_64-linux-gnu/libz.so.1 /runtime/lib/x86_64-linux-gnu/ \
    && cp /etc/ssl/certs/ca-certificates.crt /runtime/etc/ssl/certs/

FROM scratch AS runtime

ARG SIGNAL_CLI_VERSION=0.13.20
LABEL org.opencontainers.image.title="Pocket Agent signal-cli" \
      org.opencontainers.image.description="Minimal, checksum-verified signal-cli runtime for Pocket Agent" \
      org.opencontainers.image.source="https://github.com/AsamK/signal-cli" \
      org.opencontainers.image.version="${SIGNAL_CLI_VERSION}" \
      org.opencontainers.image.licenses="GPL-3.0-only"

COPY --from=fetch /runtime/ /
COPY --from=fetch /usr/local/bin/signal-cli /usr/local/bin/signal-cli

ENV HOME=/var/lib/signal-cli \
    XDG_DATA_HOME=/var/lib/signal-cli

USER 65532:65532
EXPOSE 8080
VOLUME ["/var/lib/signal-cli"]
ENTRYPOINT ["/usr/local/bin/signal-cli", "--config", "/var/lib/signal-cli/data"]
CMD ["daemon", "--http", "0.0.0.0:8080"]

# One-off setup target. It is never used by the long-running daemon image.
# qrencode and a shell exist only here so the link URI can be shown as a QR.
FROM --platform=linux/amd64 debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251 AS link-helper
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates qrencode \
    && rm -rf /var/lib/apt/lists/*
COPY --from=fetch /usr/local/bin/signal-cli /usr/local/bin/signal-cli
COPY docker/signal-cli/link-signal.sh /usr/local/bin/link-signal
RUN chmod 0555 /usr/local/bin/signal-cli /usr/local/bin/link-signal
ENV HOME=/var/lib/signal-cli \
    XDG_DATA_HOME=/var/lib/signal-cli
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/link-signal"]
