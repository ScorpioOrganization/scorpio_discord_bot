# syntax=docker/dockerfile:1

FROM rust:1-slim-bookworm AS builder
WORKDIR /app

# Build dependencies first so they are cached between source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY migrations ./migrations
COPY templates ./templates
COPY static ./static
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /data scorpio \
    && mkdir -p /data && chown scorpio /data

COPY --from=builder /app/target/release/scorpio-discord-bot /usr/local/bin/scorpio-discord-bot

USER scorpio
WORKDIR /data
ENV DATABASE_URL=sqlite:///data/scorpio.db?mode=rwc \
    WEB_BIND=0.0.0.0:8080 \
    RUST_LOG=info
EXPOSE 8080
VOLUME ["/data"]
ENTRYPOINT ["scorpio-discord-bot"]
CMD ["serve"]
