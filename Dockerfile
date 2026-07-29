FROM rust:1.86-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p treasury-service

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN useradd -m app
USER app
WORKDIR /app
COPY --from=builder /app/target/release/treasury-service /usr/local/bin/treasury-service
COPY crates/treasury-service/config /app/config
EXPOSE 8090
HEALTHCHECK CMD curl -f http://localhost:8090/health || exit 1
CMD ["treasury-service"]
