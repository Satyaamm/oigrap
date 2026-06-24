# Stage 1: builder
FROM rust:1.78-slim AS builder

WORKDIR /app

# Install build deps
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

# Copy workspace files
COPY Cargo.toml Cargo.lock* ./
COPY crates/ crates/

# Build release binary
RUN cargo build --release --bin oigrap

# Stage 2: runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/oigrap /app/oigrap

# Create data directory
RUN mkdir -p /data

EXPOSE 7432

ENV RUST_LOG=info

CMD ["/app/oigrap", "0.0.0.0:7432"]
