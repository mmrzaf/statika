FROM rust:1.96.0-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release && /src/target/release/statika --version

FROM scratch
ARG VERSION=0.3.4
ARG REVISION=unknown
LABEL org.opencontainers.image.title="statika" \
      org.opencontainers.image.description="Lightweight static file server for containerized deployments" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.licenses="MIT"
COPY --from=builder /src/target/release/statika /statika
USER 10001:10001
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["/statika"]
