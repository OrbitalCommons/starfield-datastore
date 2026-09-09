# Changelog

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
