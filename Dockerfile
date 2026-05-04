FROM rust:stable-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
COPY tests ./tests
RUN rustup target add x86_64-unknown-linux-musl
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=builder /src/target/x86_64-unknown-linux-musl/release/statika /statika
USER 10001:10001
ENTRYPOINT ["/statika"]
