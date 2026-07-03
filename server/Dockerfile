# Self-contained multi-stage build (context = ./server). Used by Railway and
# docker-compose.
#
# `rust:1-slim-bookworm` tracks the latest stable 1.x — some transitive deps
# (icu via url/idna) require a recent compiler; Cargo.lock keeps deps pinned.

# ---- Build stage ----
FROM rust:1-slim-bookworm AS builder

# native-tls (tokio-tungstenite / reqwest) needs pkg-config + OpenSSL headers.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Cache dependencies separately from source.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# `sqlx::migrate!("./migrations")` embeds the migration files at COMPILE TIME,
# so the directory must exist during the build (not just at runtime).
COPY migrations ./migrations
# `include_bytes!` in src/pdf.rs embeds the PDF fonts at COMPILE TIME too.
COPY assets ./assets
# Touch main so cargo rebuilds with the real sources.
RUN touch src/main.rs && cargo build --release

# ---- Runtime stage ----
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/voxtranslate-server /usr/local/bin/voxtranslate-server

# Drop root: run as an unprivileged system user (defense-in-depth if the process is
# ever compromised). The binary needs no write access to the image.
RUN useradd --system --no-create-home --uid 10001 appuser
USER appuser

# The server reads PORT from the environment (Railway injects it); defaults to 3001.
EXPOSE 3001
CMD ["voxtranslate-server"]
