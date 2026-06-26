# Changelog

## 0.3.0

### Stability and security

- Deny dotfiles by default with `STATIKA_DENY_DOTFILES=true`; `/.well-known/...` remains allowed.
- Enforce exactly one non-empty `Host` header for HTTP/1.1 requests.
- Reject control characters, malformed header lines, request-target fragments, NULs, backslashes, and decoded `.` / `..` path components.
- Keep symlink rejection and fd-relative file opening.
- Add queue-expiration observability through `queue_discarded` structured logs.
- Add a buffered file-send fallback for kernels/filesystems where `sendfile` is unsupported.

### HTTP/cache behavior

- Add Brotli sidecar support through `.br` files.
- Respect `Accept-Encoding` quality values; Brotli wins ties over gzip.
- Add `Date` and `Last-Modified` response headers.
- Add `If-Modified-Since` support; `If-None-Match` keeps precedence.
- Add more MIME types for common static assets.
- Add optional validated custom response headers with `STATIKA_EXTRA_HEADERS`.

### Operations and release

- Bump package version to `0.3.0`.
- Add `statika --version` / `statika -V`.
- Harden Docker metadata and add `STOPSIGNAL SIGTERM`.
- Harden CI/container smoke tests with `--cap-drop=ALL` and `no-new-privileges`.
- Add dependency audit, static binary validation, SBOM generation, and release checks.
- Add `scripts/verify-release.sh` and `scripts/load-test.sh`.
