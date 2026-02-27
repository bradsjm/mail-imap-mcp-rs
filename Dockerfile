FROM rust:1-alpine AS builder
WORKDIR /app
RUN apk add --no-cache musl-dev
COPY . .
RUN cargo build --release

FROM scratch
COPY --from=builder /app/target/release/mail-imap-mcp-rs /mail-imap-mcp-rs
ENTRYPOINT ["/mail-imap-mcp-rs"]
