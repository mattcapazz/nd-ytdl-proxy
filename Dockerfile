# builder
FROM rust:1-slim AS builder

RUN apt-get update && \
    apt-get install -y build-essential pkg-config musl-tools binutils && \
    rm -rf /var/lib/apt/lists/*

RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app
COPY Cargo.toml Cargo.lock ./

# build deps with dummy main.rs
RUN mkdir -p src && \
    echo "fn main() {}" > src/main.rs && \
    CC=musl-gcc cargo build --release --target x86_64-unknown-linux-musl 2>&1 | grep -v "warning:" || true

COPY src ./src

# build binaries
RUN CC=musl-gcc cargo build --release \
    --target x86_64-unknown-linux-musl \
    --bin nd-ytdl-proxy \
    --bin repair-metadata && \
    strip target/x86_64-unknown-linux-musl/release/nd-ytdl-proxy && \
    strip target/x86_64-unknown-linux-musl/release/repair-metadata

# runtime
FROM alpine:3.21

RUN apk add --no-cache \
    ca-certificates \
    python3 \
    ffmpeg \
    yt-dlp \
    py3-mutagen

WORKDIR /app

COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/nd-ytdl-proxy ./
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/repair-metadata ./

RUN adduser -D appuser && chown appuser:appuser /app
USER appuser

EXPOSE 4532

CMD ["./nd-ytdl-proxy"]