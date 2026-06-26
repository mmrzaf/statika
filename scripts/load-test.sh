#!/usr/bin/env sh
set -eu

url="${1:-http://127.0.0.1:8080/assets/app.js}"
connections="${CONNECTIONS:-64}"
duration="${DURATION:-30s}"

if command -v wrk >/dev/null 2>&1; then
  exec wrk -t "${THREADS:-4}" -c "$connections" -d "$duration" "$url"
elif command -v hey >/dev/null 2>&1; then
  exec hey -z "$duration" -c "$connections" "$url"
elif command -v bombardier >/dev/null 2>&1; then
  exec bombardier -c "$connections" -d "$duration" "$url"
else
  echo "install wrk, hey, or bombardier" >&2
  exit 2
fi
