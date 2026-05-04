# Statika

## Build

```bash
cargo build --release
```

## Test

```bash
cargo test
```

## Run

```bash
export STATIKA_ROOT=/srv/www
export STATIKA_LISTEN_ADDR=0.0.0.0:8080
export STATIKA_INDEX=index.html
export STATIKA_ASSETS_PATH=/assets
export STATIKA_THREADS=4
export STATIKA_QUEUE_SIZE=32
export STATIKA_GZIP=1
export STATIKA_SHUTDOWN_TIMEOUT_SECS=5
cargo run --release
```

## Docker

```bash
docker build -t statika .
docker run --rm \
  --read-only \
  --user 10001:10001 \
  -e STATIKA_ROOT=/srv/www \
  -e STATIKA_LISTEN_ADDR=0.0.0.0:8080 \
  -v /srv/www:/srv/www:ro \
  -p 8080:8080 \
  statika
```
