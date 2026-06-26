# Statika

Statika is a lightweight Linux static file server optimized for containerized deployments and high-throughput static asset delivery.

It serves files. It does not proxy, transform, render, list directories, or execute application code.

## Behavior

- `GET` and `HEAD` only.
- `/health` and `/healthz` return `200 OK`.
- Missing `/assets/...` paths return `404 Not Found`.
- Missing non-asset paths fall back to the configured SPA index when it exists.
- Precompressed `.br` sidecars are served when the client accepts Brotli.
- Precompressed `.gz` sidecars are served when the client accepts gzip.
- Encoding quality values are respected; Brotli wins ties over gzip when both sidecars exist.
- Fingerprinted assets such as `app.0123abcd.js` receive immutable one-year caching.
- Other assets receive one-hour caching.
- The SPA index and route fallbacks receive `Cache-Control: no-cache`.
- `ETag`, `Last-Modified`, `If-None-Match`, and `If-Modified-Since` are supported.
- Dotfiles are denied by default, except `/.well-known/...` for standard ACME/security workflows.
- Files are opened relative to the document-root file descriptor with `O_NOFOLLOW`; symlinks are intentionally rejected.
- Each connection has a bounded total lifetime from acceptance, including queue wait time.
- HTTP/1.1 requests must include exactly one non-empty `Host` header.
- `SIGINT` and `SIGTERM` initiate graceful shutdown.

## Build and test

```bash
cargo test --locked
cargo build --locked --release
```

For release verification:

```bash
./scripts/verify-release.sh
```

Statika currently targets Linux. The Docker image builds a static musl-linked binary and runs from `scratch` as UID/GID `10001`.

## Configuration

`STATIKA_ROOT` is required. All other values have production-safe defaults.

| Variable | Default | Constraint |
| --- | --- | --- |
| `STATIKA_ROOT` | required | absolute document-root path |
| `STATIKA_LISTEN_ADDR` | `0.0.0.0:8080` | socket address |
| `STATIKA_INDEX` | `index.html` | relative path below root |
| `STATIKA_ASSETS_PATH` | `/assets` | absolute URL prefix |
| `STATIKA_THREADS` | available CPUs, capped at `32` | `1..=256` |
| `STATIKA_QUEUE_SIZE` | `threads * 64` | `1..=65536` |
| `STATIKA_GZIP` | `true` | boolean |
| `STATIKA_BROTLI` | `true` | boolean |
| `STATIKA_DENY_DOTFILES` | `true` | boolean |
| `STATIKA_REQUEST_TIMEOUT_SECS` | `5` | `1..=300` |
| `STATIKA_SHUTDOWN_TIMEOUT_SECS` | `10` | `1..=300` |
| `STATIKA_EXTRA_HEADERS` | empty | newline-separated response headers |

Accepted booleans are `1`, `true`, `yes`, `on`, `0`, `false`, `no`, and `off`.

`STATIKA_EXTRA_HEADERS` is for site/security headers. Use one `Header-Name: value` per line. Protocol/body/cache headers such as `Content-Length`, `Content-Type`, `Content-Encoding`, `Cache-Control`, `ETag`, `Last-Modified`, `Vary`, `Date`, `Server`, `Connection`, and `Transfer-Encoding` are rejected because Statika owns them.

Example:

```bash
export STATIKA_EXTRA_HEADERS='Strict-Transport-Security: max-age=31536000; includeSubDomains
Referrer-Policy: no-referrer
Cross-Origin-Resource-Policy: same-origin'
```

## Run locally

```bash
STATIKA_ROOT=/srv/www cargo run --locked --release
```

Print the version without configuration:

```bash
statika --version
```

## Build and run the container

```bash
docker build -t statika .

docker run --rm \
  --read-only \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  --user 10001:10001 \
  -e STATIKA_ROOT=/srv/www \
  -v /srv/www:/srv/www:ro \
  -p 8080:8080 \
  statika
```

Mount the document root read-only. Statika rejects symlinks, but a read-only mount also prevents accidental runtime mutation.

## Frontend runtime image

```dockerfile
FROM node:24-alpine AS frontend
WORKDIR /app
COPY package*.json ./
RUN npm ci
COPY . .
RUN npm run build

FROM ghcr.io/mmrzaf/statika:VERSION
ENV STATIKA_ROOT=/srv/www
COPY --from=frontend /app/dist/ /srv/www/
```

## Production guidance

Use Statika behind a CDN, ingress, or reverse proxy. Keep `STATIKA_ROOT` pointed at a clean build output directory such as `dist/`, not a repository root or home directory.

Recommended container settings:

```bash
--read-only
--cap-drop=ALL
--security-opt=no-new-privileges
--user 10001:10001
-v /srv/www:/srv/www:ro
```

Recommended asset pipeline:

- Generate fingerprinted assets under `/assets`.
- Precompress large text assets as `.br` and `.gz` during build.
- Keep `index.html` uncompressed sidecars if your deployment needs older clients; Statika still serves sidecars when accepted.
- Leave dotfile denial enabled unless you intentionally serve hidden files.

## Load testing

With Statika running:

```bash
./scripts/load-test.sh http://127.0.0.1:8080/assets/app.js
```

The script uses `wrk`, `hey`, or `bombardier`, whichever is installed.

## Release artifacts

Pushing a `v*` tag runs the release workflow. It verifies the project, publishes the GHCR image, and creates a GitHub Release containing:

- `statika-<tag>-linux-amd64`: static Linux binary.
- `statika-<tag>-linux-amd64-docker.tar.gz`: Docker image archive.
- `statika-<tag>-linux-amd64.spdx.json`: binary SBOM.
- `statika-<tag>-linux-amd64-docker.spdx.json`: container SBOM.
- `SHA256SUMS`: checksums for all artifacts.

Load the downloadable container archive with:

```bash
docker load < statika-<tag>-linux-amd64-docker.tar.gz
```
