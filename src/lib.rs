//! Pull-through artifact cache and ephemeris server for the OrbitalCommons
//! data stack.
//!
//! A request resolves down a chain — local disk, then the organisation's
//! mirror, then (only when explicitly allowed) the upstream archive —
//! populating the local cache. Only the server and its batch commands write
//! the mirror. Validation failures are never cached. Tailnet clients need no
//! archive credentials; an upstream-enabled client uses its own providers.
//!
//! The full design is in `docs/spec.md`. This crate never depends on
//! `starfield`; `starfield` depends on it.

mod artifact;
mod check;
mod config;
mod credential;
mod error;
mod fetch;
mod manifest;
mod mirror;
#[cfg(feature = "mirror-s3")]
mod s3;
#[cfg(feature = "server")]
pub mod server;
mod store;

pub use artifact::{Artifact, ArtifactKey, Freshness, Provenance, Source};
pub use check::{CheckFailure, ContentCheck};
#[cfg(feature = "onepassword")]
pub use credential::OnePasswordProvider;
pub use credential::{
    ChainProvider, Credential, CredentialProvider, EnvProvider, NetrcProvider, Secret,
    StaticProvider,
};
pub use error::{DatastoreError, Result};
pub use manifest::Manifest;
pub use mirror::Mirror;
#[cfg(feature = "mirror-s3")]
pub use s3::{MirrorObject, PutOutcome, S3Mirror};
pub use store::{
    Datastore, DatastoreBuilder, IndexEntry, Layer, ProgressFn, ResolveOutcome, VerifyFailure,
};
