FROM rust:1.86-bookworm AS builder
WORKDIR /app
COPY . .
# ceremony-check rides along deliberately. KEY-CEREMONY.md's step 5 is not a one-off: an untested
# recovery path decays silently as IAM changes, so the document requires re-exercising it on a
# schedule. A tool that has to be built from source each time is a tool that stops being run.
#
# It grants nothing new. This image already holds the credentials that can sign, and already runs a
# service whose job is to sign; a binary that signs one synthetic digest adds no capability an
# attacker with exec here would not already have.
#
# Two invocations rather than one with combined selectors: they share the target directory so the
# second is nearly free, and each is unambiguous. The image build only runs on `main`, so a
# Dockerfile mistake here is found after merge rather than on the pull request.
RUN cargo build --release -p treasury-service \
 && cargo build --release -p clutch-chain --bin ceremony-check

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN useradd -m app
USER app
WORKDIR /app
COPY --from=builder /app/target/release/treasury-service /usr/local/bin/treasury-service
COPY --from=builder /app/target/release/ceremony-check /usr/local/bin/ceremony-check
COPY crates/treasury-service/config /app/config
EXPOSE 8090
HEALTHCHECK CMD curl -f http://localhost:8090/health || exit 1
CMD ["treasury-service"]
