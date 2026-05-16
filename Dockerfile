FROM rust:1.95-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release

FROM ghcr.io/viniciusdsandrade/rinha-de-backend-2026:submission-76fc604 AS index

FROM debian:trixie-slim

WORKDIR /app
COPY --from=builder /app/target/release/rinha-rust-ivf /app/rinha-rust-ivf
COPY --from=index /app/data/index.bin /app/data/index.bin

ENV IVF_INDEX_PATH=/app/data/index.bin

ENTRYPOINT ["/app/rinha-rust-ivf"]
