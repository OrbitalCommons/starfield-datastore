//! The mirror layer: the organisation's copy of every artifact.
//!
//! Clients read it through the ephemeris server (`Mirror::Http`, which
//! follows the server's presigned redirect without ever sending credentials).
//! The server reads and writes S3 directly (`Mirror::S3`). The resolution
//! chain only ever reads a mirror; writing is an explicit act of the server.

use crate::fetch::Progress;
#[cfg(any(not(feature = "mirror-http"), not(feature = "mirror-s3")))]
use crate::DatastoreError;
use crate::{ArtifactKey, Result};
use std::io::Write;
use std::time::Duration;

#[cfg(feature = "mirror-http")]
pub(crate) mod http;

/// Where the organisation's mirror lives and how it is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mirror {
    /// Plain HTTPS GET against a base URL; follows cross-host redirects
    /// without forwarding credentials. This is what clients use for the
    /// ephemeris server.
    Http { base_url: String, writable: bool },
    /// Requires the `mirror-s3` feature. This is what the server uses.
    S3 {
        bucket: String,
        prefix: String,
        region: String,
        writable: bool,
    },
}

/// The read side of a mirror, as the resolution chain sees it.
pub(crate) trait MirrorRead: Send + Sync {
    /// Stream the object stored under `key` into `sink`.
    ///
    /// `Ok(false)` means the mirror has no such object. Any other failure is
    /// an error; in the default resolution mode it surfaces as
    /// `DatastoreError::MirrorUnreachable` with the error text as the reason.
    fn fetch(&self, key: &ArtifactKey, sink: &mut dyn Write) -> Result<bool>;

    /// Sanitised URL of the object in this mirror, e.g.
    /// `https://ephem.example/artifact/naif/spk/de440.bsp` or
    /// `s3://bucket/prefix/naif/spk/de440.bsp`. Recorded as the sidecar's
    /// source and used in error messages; never carries a query string.
    fn location(&self, key: &ArtifactKey) -> String;
}

#[cfg_attr(not(feature = "mirror-http"), allow(unused_variables))]
pub(crate) fn open(
    mirror: &Mirror,
    timeout: Duration,
    progress: Progress,
) -> Result<Box<dyn MirrorRead>> {
    match mirror {
        #[cfg(feature = "mirror-http")]
        Mirror::Http { base_url, .. } => Ok(Box::new(http::HttpMirror::new(
            base_url, timeout, progress,
        )?)),
        #[cfg(not(feature = "mirror-http"))]
        Mirror::Http { .. } => Err(DatastoreError::Mirror(
            "this build has no HTTP mirror support (feature `mirror-http`)".into(),
        )),
        #[cfg(feature = "mirror-s3")]
        Mirror::S3 {
            bucket,
            prefix,
            region,
            ..
        } => Ok(Box::new(crate::S3Mirror::new(bucket, prefix, region)?)),
        #[cfg(not(feature = "mirror-s3"))]
        Mirror::S3 { .. } => Err(DatastoreError::Mirror(
            "this build has no S3 mirror support (feature `mirror-s3`)".into(),
        )),
    }
}
