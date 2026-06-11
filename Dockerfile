FROM rust:1.96.0-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM scratch
COPY --from=builder /src/target/release/statika /statika
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/statika"]
