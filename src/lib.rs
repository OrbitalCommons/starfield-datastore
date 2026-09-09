//! Pull-through artifact cache and ephemeris server for the OrbitalCommons
//! data stack.
//!
//! A request resolves down a chain — local disk, then the organisation's
//! mirror, then (only when explicitly allowed) the upstream archive —
//! populating the nearer layers as it goes. Nothing that fails validation is
//! ever cached, and upstream credentials live in exactly one place: the
//! ephemeris server.
//!
//! The full design is in `docs/spec.md`. This crate never depends on
//! `starfield`; `starfield` depends on it.
