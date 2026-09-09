//! `Mirror::Http`: the client's view of the ephemeris server.
//!
//! `GET <base>/artifact/<key>` answers 302 to a presigned S3 URL. The fetch
//! engine follows that redirect with no credential provider at all, so
//! nothing can be forwarded to the redirect target; the presigned URL carries
//! its own authorisation.

use super::MirrorRead;
use crate::fetch::{sanitize, Fetcher, Progress};
use crate::{ArtifactKey, DatastoreError, Result, Source};
use std::io::Write;
use std::time::Duration;
use url::Url;

pub(crate) struct HttpMirror {
    base: String,
    fetcher: Fetcher,
}

impl HttpMirror {
    pub(crate) fn new(base_url: &str, timeout: Duration, progress: Progress) -> Result<Self> {
        let parsed = Url::parse(base_url)
            .map_err(|e| DatastoreError::Mirror(format!("invalid mirror URL: {e}")))?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(DatastoreError::Mirror(
                "mirror URL must not carry userinfo".into(),
            ));
        }
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(DatastoreError::Mirror("mirror URL must be http(s)".into()));
        }
        Ok(Self {
            base: sanitize(base_url).trim_end_matches('/').to_string(),
            fetcher: Fetcher::new(timeout, None, progress, false)?,
        })
    }
}

impl MirrorRead for HttpMirror {
    fn fetch(&self, key: &ArtifactKey, sink: &mut dyn Write) -> Result<bool> {
        let source = Source::new(self.location(key));
        match self.fetcher.request(&source, sink) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(DatastoreError::NoCredential { host }) => Err(DatastoreError::Mirror(format!(
                "{host} refused the request (HTTP 401/403); the mirror is expected to need no credentials"
            ))),
            Err(e) => Err(e),
        }
    }

    fn location(&self, key: &ArtifactKey) -> String {
        format!("{}/artifact/{}", self.base, key.as_str())
    }
}
