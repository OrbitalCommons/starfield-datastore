use crate::{ArtifactKey, CheckFailure};

#[derive(Debug, thiserror::Error)]
pub enum DatastoreError {
    #[error("no credential configured for host {host}")]
    NoCredential { host: String },
    #[error("credential rejected by {host} (HTTP {status}){}", if *.looks_expired { "; it looks expired" } else { "" })]
    CredentialRejected {
        host: String,
        status: u16,
        looks_expired: bool,
    },
    #[error("content rejected for {key}: failed {}; got {}", .failure.check, .failure.got)]
    ContentRejected {
        key: ArtifactKey,
        failure: CheckFailure,
    },
    #[error("offline, and {key} is not in the local cache")]
    OfflineMiss { key: ArtifactKey },
    #[error("{key} is not cached and the mirror is unreachable ({reason}); set STARFIELD_ALLOW_UPSTREAM=1 to fetch from the archive")]
    MirrorUnreachable { key: ArtifactKey, reason: String },
    #[error("all {} sources failed for {key}: {}", .attempts.len(), .attempts.join("; "))]
    AllSourcesFailed {
        key: ArtifactKey,
        attempts: Vec<String>,
    },
    #[error("invalid artifact key: {0}")]
    InvalidKey(String),
    #[error("mirror error: {0}")]
    Mirror(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("manifest error: {0}")]
    Manifest(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("HTTP request failed")]
    Http(#[source] reqwest::Error),
}

impl From<reqwest::Error> for DatastoreError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error.without_url())
    }
}

pub type Result<T> = std::result::Result<T, DatastoreError>;
