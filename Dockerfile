# Multi-stage Dockerfile for Wastebin customized fork (stash.home)

# --- build stage
FROM rust:1.95-bookworm AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY assets ./assets
COPY crates ./crates

RUN cargo build --release --bin wastebin --bin wastebin-ctl

RUN addgroup --gid 10001 app && adduser --disabled-password --gecos "" --home "/nonexistent" --shell "/sbin/nologin" --no-create-home --uid 10001 --ingroup app app

# --- runtime stage
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /etc/passwd /etc/passwd
COPY --from=builder /etc/group /etc/group

WORKDIR /app
COPY --from=builder /app/target/release/wastebin /app/wastebin
COPY --from=builder /app/target/release/wastebin-ctl /app/wastebin-ctl

RUN mkdir -p /app/data && chown -R app:app /app

USER app:app

ENV WASTEBIN_ADDRESS_PORT=0.0.0.0:8088 \
    WASTEBIN_DATABASE_PATH=/app/data/wastebin.db \
    WASTEBIN_MAX_BODY_SIZE=16777216

EXPOSE 8088

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 CMD curl -fsS http://127.0.0.1:8088/ || exit 1

CMD ["/app/wastebin"]
