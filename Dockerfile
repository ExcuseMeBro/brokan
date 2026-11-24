# Brokan for a server: one board behind a reverse proxy that holds the TLS certificate.
# See docs/self-hosting.md; deploy/ has a ready Compose setup with Caddy.

FROM rust:1.89.0-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY src src
RUN cargo build --release --locked

FROM debian:12.11-slim
# git (and ssh) for `brokan sync` from the server; curl for the health check.
RUN apt-get update \
 && apt-get install -y --no-install-recommends git openssh-client ca-certificates curl tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --home-dir /home/brokan --uid 10001 brokan \
 && mkdir -p /data /home/brokan/.local/state \
 && chown brokan:brokan /data /home/brokan/.local/state
COPY --from=build /src/target/release/brokan /usr/local/bin/brokan
ENV BROKAN_HOME=/data \
    BROKAN_LISTEN=0.0.0.0 \
    BROKAN_PORT=1317 \
    BROKAN_UPDATES=off \
    HOME=/home/brokan
USER brokan
# The board, and the machine-local state (MCP metrics, sync settings) beside it.
VOLUME ["/data", "/home/brokan/.local/state"]
EXPOSE 1317
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD curl -fsS http://127.0.0.1:1317/api/ping || exit 1
ENTRYPOINT ["tini", "--", "brokan", "--foreground", "--no-open"]
