FROM rust:1-slim AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev musl-tools && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --target x86_64-unknown-linux-musl

FROM alpine:3.21

RUN apk add --no-cache ca-certificates python3 py3-pip ffmpeg \
    && pip3 install --break-system-packages yt-dlp

WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/nd-ytdl-proxy .

EXPOSE 4532

CMD ["./nd-ytdl-proxy"]
