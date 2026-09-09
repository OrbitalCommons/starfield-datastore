use crate::{ContentCheck, DatastoreError, Result};

/// Stable, slash-separated logical identity, independent of archive URLs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtifactKey(String);

impl ArtifactKey {
    pub fn new(key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        if key.is_empty()
            || key.len() > 1024
            || key
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".." || part.len() > 240)
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._/".contains(&b))
        {
            return Err(DatastoreError::InvalidKey(
                "expected nonempty safe path components".into(),
            ));
        }
        Ok(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn mirror_path(&self, prefix: &str) -> String {
        let prefix = prefix.trim_matches('/');
        if prefix.is_empty() {
            self.0.clone()
        } else {
            format!("{prefix}/{}", self.0)
        }
    }
}

impl std::fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone)]
pub struct Source {
    pub url: String,
    pub trust_redirects: bool,
}

impl Source {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            trust_redirects: false,
        }
    }
    pub fn trusting_redirects(mut self) -> Self {
        self.trust_redirects = true;
        self
    }
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("url", &"[redacted]")
            .field("trust_redirects", &self.trust_redirects)
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Provenance {
    pub description: String,
    pub license: String,
    pub citation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Freshness {
    #[default]
    Immutable,
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub key: ArtifactKey,
    pub sources: Vec<Source>,
    pub check: ContentCheck,
    pub freshness: Freshness,
    pub provenance: Provenance,
    pub expected_bytes: Option<u64>,
}

impl Artifact {
    pub fn new(key: ArtifactKey, sources: Vec<Source>) -> Self {
        Self {
            key,
            sources,
            check: ContentCheck::default_binary(),
            freshness: Freshness::Immutable,
            provenance: Provenance::default(),
            expected_bytes: None,
        }
    }
    pub fn with_check(mut self, check: ContentCheck) -> Self {
        self.check = check;
        self
    }
    pub fn with_freshness(mut self, freshness: Freshness) -> Self {
        self.freshness = freshness;
        self
    }
    pub fn with_provenance(mut self, provenance: Provenance) -> Self {
        self.provenance = provenance;
        self
    }
    pub fn with_expected_bytes(mut self, bytes: u64) -> Self {
        self.expected_bytes = Some(bytes);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_cannot_escape_the_index_or_encode_credentials() {
        for key in [
            "",
            "/a",
            "a/",
            "a//b",
            "a/../b",
            "./a",
            "a\\b",
            "a?token=secret",
            "https://host/file",
            "a%2fb",
            "a\n",
        ] {
            assert!(ArtifactKey::new(key).is_err(), "accepted {key:?}");
        }
        let key = ArtifactKey::new("naif/spk/de440.bsp").unwrap();
        assert_eq!(key.mirror_path("/mirror/"), "mirror/naif/spk/de440.bsp");
        assert_eq!(key.mirror_path(""), key.as_str());
    }
}
