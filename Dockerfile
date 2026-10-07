# One image, three binaries (invoice-service, mock-psp, webhook-receiver);
# docker-compose picks the binary per service via `command`.
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked \
    && mkdir -p /out \
    && cp target/release/invoice-service target/release/mock-psp target/release/webhook-receiver /out/

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 app
COPY --from=builder /out/ /usr/local/bin/
USER app
