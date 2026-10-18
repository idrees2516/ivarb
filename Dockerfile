# syntax=docker/dockerfile:1
# ---------------------------------------------------------------------------
# IVARB — production image
#
# Stage 1 (builder): compile the Rust server (release) and the Leptos WASM
#                    frontend (trunk) in one stage so the cargo registry and
#                    target artifacts are shared between both builds.
# Stage 2 (runtime): debian-slim + the server binary + the static frontend.
#                    No toolchain, no source, no secrets — ~90 MB.
#
# Build:  docker build -t ivarb:latest .
# Run:    docker run -p 3000:3000 --env-file .env ivarb:latest
# ---------------------------------------------------------------------------

FROM rust:1-slim AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates pkg-config \
    && rm -rf /var/lib/apt/lists/*
# WASM frontend toolchain (versions pinned to match Cargo.lock)
RUN rustup target add wasm32-unknown-unknown
RUN cargo install --locked trunk --version 0.21.14 --no-default-features --features rustls
RUN cargo install --locked wasm-bindgen-cli --version 0.2.128

WORKDIR /app
# Dependencies first (cached across source-only changes)
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY frontend ./frontend
RUN cargo build --release --bin ivarb-server
RUN cd frontend && trunk build --release

# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /var/lib/ivarb --shell /usr/sbin/nologin ivarb

COPY --from=builder /app/target/release/ivarb-server /usr/local/bin/ivarb-server
COPY --from=builder /app/frontend/dist /opt/ivarb/www

# Writable state (JSONL audit logs) under an unprivileged user
ENV IVARB_STATIC_DIR=/opt/ivarb/www \
    IVARB_DATA_DIR=/var/lib/ivarb/data \
    BIND=0.0.0.0 \
    PORT=3000
RUN mkdir -p /var/lib/ivarb/data && chown -R ivarb:ivarb /var/lib/ivarb
USER ivarb
WORKDIR /var/lib/ivarb
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3000/healthz || exit 1

ENTRYPOINT ["/usr/local/bin/ivarb-server"]
