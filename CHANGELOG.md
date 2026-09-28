# Changelog

## 0.1.3 — 2026-09-28

- Add opt-in server-owned cache maintenance with bounded cold-fill concurrency,
  aggregate byte reservations, streamed transfer limits, and filesystem headroom.
- Support staging-only serving (`max_bytes = 0`): reclaim local blobs after all
  active fills finish uploading, while continuing S3 redirects after eviction.
- Keep ordinary library gets and default server operation free of automatic
  eviction. Leave temporary crash residue untouched.
- Return retryable 503 responses for cold-fill backpressure; keep S3-hit work
  independent. Retain fill protection after HTTP disconnects and on error paths.
- Add service-root ownership locking, startup/maintenance diagnostics, and
  fail-closed cold-fill admission after maintenance errors.

## 0.1.2 — 2026-09-15

- Bump indicatif 0.17 → 0.18, replacing the unmaintained `number_prefix`
  (RUSTSEC-2025-0119) with `unit-prefix` behind the `progress` feature. No
  API change.

## 0.1.1 — 2026-09-10

- Add one-file service profiles and lazy named credential sources, with
  preserved unknown types that are skipped with a redacted startup warning.
- Add a non-root Docker image and gated GHCR publication on every main push.
- Support config/profile selection and flag overrides across CLI commands.

## 0.1.0 — 2026-09-09

Breaking release replacing the 0.0.1 placeholder with the datastore API.

- Synchronous, content-addressed local cache with streaming validation,
  explicit import, verification, repair and garbage collection.
- Host-scoped credentials, explicit redirect trust, and upstream access
  controlled independently from offline mode.
- Declarative manifests with content pins and a verified ephemeris manifest.
- HTTP and S3 mirrors, conditional multipart uploads, and a private
  ephemeris server returning presigned redirects.
- CLI commands, systemd deployment files, and required feature-matrix CI.

Cache hits are rehashed. Garbage collection is explicit and does not protect
paths held by consumers. Range resume and asynchronous public APIs are not
provided. Custom checks read the whole artifact; built-in checks stream.
