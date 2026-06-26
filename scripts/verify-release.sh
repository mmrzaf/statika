#!/usr/bin/env sh
set -eu

cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked

if command -v cargo-audit >/dev/null 2>&1; then
  cargo audit --locked
else
  echo "cargo-audit not installed; skipping dependency audit" >&2
fi

docker build \
  --build-arg VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)" \
  --build-arg REVISION="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)" \
  -t statika:verify .

workdir="$(mktemp -d)"
trap 'docker rm -f statika-verify >/dev/null 2>&1 || true; rm -rf "$workdir"' EXIT
mkdir -p "$workdir/www/assets"
printf 'ok' > "$workdir/www/index.html"

docker run -d --rm --name statika-verify \
  --read-only \
  --cap-drop=ALL \
  --security-opt=no-new-privileges \
  --user 10001:10001 \
  -e STATIKA_ROOT=/srv/www \
  -v "$workdir/www:/srv/www:ro" \
  -p 18080:8080 \
  statika:verify >/dev/null

for _ in $(seq 1 50); do
  if curl --fail --silent http://127.0.0.1:18080/health >/dev/null; then
    break
  fi
  sleep 0.1
done

test "$(curl --fail --silent http://127.0.0.1:18080/)" = ok
docker run --rm --entrypoint /statika statika:verify --version
