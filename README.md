# Statika

Statika is a lightweight static file server optimized for containerized deployments and high-throughput static asset delivery.

---

## Build

```bash
cargo build --release
````

## Test

```bash
cargo test
```

---

## Run (local)

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

---

## Docker (local build)

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

---

## GitHub Container Registry (GHCR)

### Pull prebuilt image

```bash
docker pull ghcr.io/mmrzaf/statika:latest
```

or pinned version:

```bash
docker pull ghcr.io/mmrzaf/statika:v0.1.0
```

### Run from GHCR

```bash
docker run --rm \
  --read-only \
  --user 10001:10001 \
  -e STATIKA_ROOT=/srv/www \
  -e STATIKA_LISTEN_ADDR=0.0.0.0:8080 \
  -v /srv/www:/srv/www:ro \
  -p 8080:8080 \
  ghcr.io/mmrzaf/statika:latest
```

---

## Frontend Multi-Stage Deployment (Recommended Pattern)

Statika is designed to serve static assets produced by frontend build pipelines (React, Vue, Svelte, etc.).

### Example Dockerfile

```dockerfile
# Build frontend
FROM node:24-alpine AS frontend-builder
WORKDIR /app

COPY package*.json ./
RUN npm ci

COPY . .
RUN npm run build


# Runtime (Statika from GHCR)
FROM ghcr.io/mmrzaf/statika:latest

ENV STATIKA_ROOT=/srv/www \
    STATIKA_LISTEN_ADDR=0.0.0.0:8080 \
    STATIKA_INDEX=index.html \
    STATIKA_ASSETS_PATH=/assets

COPY --from=frontend-builder /app/dist/ /srv/www/

EXPOSE 8080
```

---

## GitHub Actions (CI/CD to GHCR)

Statika images can be automatically built and published:

* Builds on push to `main`
* Tags on version releases (`v*`)
* Publishes to GHCR (`ghcr.io/<owner>/statika`)

(See `.github/workflows/docker.yml`)

---

## Design Notes

* Minimal runtime footprint (`scratch`-based image)
* Zero Node.js in production images
* Optimized for immutable deployments
* Works well in Kubernetes, Docker Swarm, or bare containers

```

---

If you want, next step is tightening the GHCR strategy further (digest pinning + multi-arch builds + SBOM signing), which is where this becomes enterprise-grade instead of “just working.”
```

