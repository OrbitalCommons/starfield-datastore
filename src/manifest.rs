use crate::{Artifact, ArtifactKey, ContentCheck, DatastoreError, Provenance, Result, Source};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path};

#[derive(Debug, Clone, Default)]
pub struct Manifest {
    pub artifacts: Vec<Artifact>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default, rename = "artifact")]
    artifacts: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    key: String,
    #[serde(default)]
    sources: Vec<SourceEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(default = "immutable")]
    freshness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    check: Option<toml::Value>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    license: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    citation: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum SourceEntry {
    Url(String),
    Detailed(SourceFields),
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFields {
    url: String,
    #[serde(default)]
    trust_redirects: bool,
}
fn immutable() -> String {
    "immutable".into()
}
fn invalid(message: &str) -> DatastoreError {
    DatastoreError::Manifest(message.into())
}

impl Manifest {
    pub fn from_toml_str(s: &str) -> Result<Self> {
        // Parser errors can include source lines with tokens; never echo them.
        let document: Document =
            toml::from_str(s).map_err(|_| invalid("invalid TOML manifest or unknown field"))?;
        let mut seen = HashSet::new();
        let mut artifacts = Vec::new();
        for entry in document.artifacts {
            let key = ArtifactKey::new(entry.key)?;
            if !seen.insert(key.clone()) {
                return Err(invalid("duplicate artifact key"));
            }
            if entry.freshness != "immutable" {
                return Err(invalid("only immutable freshness is supported"));
            }
            let mut sources = Vec::new();
            for source in entry.sources {
                let source = match source {
                    SourceEntry::Url(url) => Source::new(url),
                    SourceEntry::Detailed(s) => Source {
                        url: s.url,
                        trust_redirects: s.trust_redirects,
                    },
                };
                validate_url(&source.url)?;
                sources.push(source);
            }
            let mut check = entry
                .check
                .map(|v| parse_check(&v, 0))
                .transpose()?
                .unwrap_or_else(ContentCheck::default_binary);
            if let Some(digest) = entry.sha256 {
                validate_digest(&digest)?;
                check = ContentCheck::All(vec![ContentCheck::Sha256(digest), check]);
            }
            artifacts.push(Artifact {
                key,
                sources,
                check,
                freshness: crate::Freshness::Immutable,
                provenance: Provenance {
                    description: entry.description,
                    license: entry.license,
                    citation: entry.citation,
                },
                expected_bytes: entry.bytes,
            });
        }
        Ok(Self { artifacts })
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        Self::from_toml_str(&std::fs::read_to_string(path)?)
    }

    pub fn to_toml_string(&self) -> Result<String> {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        for artifact in &self.artifacts {
            if !seen.insert(artifact.key.clone()) {
                return Err(invalid("duplicate artifact key"));
            }
            let (sha256, check) = split_pin(&artifact.check);
            if let Some(digest) = &sha256 {
                validate_digest(digest)?;
            }
            let mut sources = Vec::new();
            for source in &artifact.sources {
                validate_url(&source.url)?;
                sources.push(if source.trust_redirects {
                    SourceEntry::Detailed(SourceFields {
                        url: source.url.clone(),
                        trust_redirects: true,
                    })
                } else {
                    SourceEntry::Url(source.url.clone())
                });
            }
            entries.push(Entry {
                key: artifact.key.as_str().into(),
                sources,
                sha256,
                bytes: artifact.expected_bytes,
                freshness: immutable(),
                check: Some(encode_check(&check)?),
                description: artifact.provenance.description.clone(),
                license: artifact.provenance.license.clone(),
                citation: artifact.provenance.citation.clone(),
            });
        }
        toml::to_string_pretty(&Document { artifacts: entries })
            .map_err(|_| invalid("cannot encode manifest"))
    }

    pub fn get(&self, key: &ArtifactKey) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| &a.key == key)
    }

    /// Entries from `other` replace matching keys in place; new keys append.
    pub fn merge(&mut self, other: Manifest) {
        for artifact in other.artifacts {
            if let Some(existing) = self.artifacts.iter_mut().find(|a| a.key == artifact.key) {
                *existing = artifact;
            } else {
                self.artifacts.push(artifact);
            }
        }
    }

    /// Pin a validated mirror result without silently changing an existing pin.
    pub fn pin_sha256(&mut self, key: &ArtifactKey, digest: &str) -> Result<()> {
        validate_digest(digest)?;
        let artifact = self
            .artifacts
            .iter_mut()
            .find(|a| &a.key == key)
            .ok_or_else(|| invalid("cannot pin unknown key"))?;
        ensure_compatible_pin(&artifact.check, digest)?;
        let (pin, check) = split_pin(&artifact.check);
        if pin.as_deref() == Some(digest) {
            return Ok(());
        }
        artifact.check = ContentCheck::All(vec![ContentCheck::Sha256(digest.into()), check]);
        Ok(())
    }
}

fn ensure_compatible_pin(check: &ContentCheck, digest: &str) -> Result<()> {
    match check {
        ContentCheck::Sha256(existing) if existing != digest => {
            Err(invalid("refusing to replace an existing digest pin"))
        }
        ContentCheck::All(checks) => {
            for check in checks {
                ensure_compatible_pin(check, digest)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn split_pin(check: &ContentCheck) -> (Option<String>, ContentCheck) {
    match check {
        ContentCheck::Sha256(digest) => (Some(digest.clone()), ContentCheck::None),
        ContentCheck::All(checks) if matches!(checks.first(), Some(ContentCheck::Sha256(_))) => {
            let ContentCheck::Sha256(digest) = &checks[0] else {
                unreachable!()
            };
            let rest = &checks[1..];
            (
                Some(digest.clone()),
                if rest.len() == 1 {
                    rest[0].clone()
                } else {
                    ContentCheck::All(rest.to_vec())
                },
            )
        }
        _ => (None, check.clone()),
    }
}

fn validate_url(raw: &str) -> Result<()> {
    let url = url::Url::parse(raw).map_err(|_| invalid("invalid source URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid(
            "source must be HTTP(S) without embedded credentials",
        ));
    }
    Ok(())
}
fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid(
            "sha256 must be 64 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn parse_check(value: &toml::Value, depth: usize) -> Result<ContentCheck> {
    if depth > 32 {
        return Err(invalid("checks are nested too deeply"));
    }
    if let Some(s) = value.as_str() {
        return match s {
            "none" => Ok(ContentCheck::None),
            "not-html" => Ok(ContentCheck::NotHtml),
            _ => Err(invalid("unknown check")),
        };
    }
    let table = value
        .as_table()
        .ok_or_else(|| invalid("check must be a table or named check"))?;
    if table.contains_key("magic") {
        if table
            .keys()
            .any(|k| k != "magic" && k != "trim_leading_whitespace")
        {
            return Err(invalid("unknown magic-check field"));
        }
        let prefixes = table["magic"]
            .as_array()
            .ok_or_else(|| invalid("magic must be an array"))?
            .iter()
            .map(|p| {
                if let Some(s) = p.as_str() {
                    return Ok(s.as_bytes().to_vec());
                }
                p.as_array()
                    .ok_or_else(|| invalid("magic prefix must be text or a byte array"))?
                    .iter()
                    .map(|b| {
                        b.as_integer()
                            .and_then(|n| u8::try_from(n).ok())
                            .ok_or_else(|| invalid("magic byte out of range"))
                    })
                    .collect::<Result<Vec<u8>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        if prefixes.is_empty() || prefixes.iter().any(Vec::is_empty) {
            return Err(invalid("magic requires nonempty prefixes"));
        }
        let trim = table
            .get("trim_leading_whitespace")
            .map(|v| {
                v.as_bool()
                    .ok_or_else(|| invalid("trim_leading_whitespace must be boolean"))
            })
            .transpose()?
            .unwrap_or(false);
        return Ok(ContentCheck::magic(prefixes, trim));
    }
    if table.len() != 1 {
        return Err(invalid("a check must contain exactly one variant"));
    }
    let (name, value) = table.iter().next().unwrap();
    match name.as_str() {
        "none" if value.as_bool() == Some(true) => Ok(ContentCheck::None),
        "not_html" if value.as_bool() == Some(true) => Ok(ContentCheck::NotHtml),
        "sha256" => {
            let digest = value
                .as_str()
                .ok_or_else(|| invalid("sha256 must be text"))?;
            validate_digest(digest)?;
            Ok(ContentCheck::Sha256(digest.into()))
        }
        "min_bytes" => Ok(ContentCheck::MinBytes(
            value
                .as_integer()
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| invalid("min_bytes must be nonnegative"))?,
        )),
        "all" => Ok(ContentCheck::All(
            value
                .as_array()
                .ok_or_else(|| invalid("all must be an array"))?
                .iter()
                .map(|v| parse_check(v, depth + 1))
                .collect::<Result<_>>()?,
        )),
        _ => Err(invalid(
            "unknown or invalid declarative check; custom predicates cannot appear in manifests",
        )),
    }
}

fn encode_check(check: &ContentCheck) -> Result<toml::Value> {
    let mut table = toml::map::Map::new();
    let (name, value) = match check {
        ContentCheck::None => ("none", toml::Value::Boolean(true)),
        ContentCheck::NotHtml => ("not_html", toml::Value::Boolean(true)),
        ContentCheck::MinBytes(n) => (
            "min_bytes",
            toml::Value::Integer(
                i64::try_from(*n).map_err(|_| invalid("min_bytes exceeds TOML integer range"))?,
            ),
        ),
        ContentCheck::Sha256(digest) => {
            validate_digest(digest)?;
            ("sha256", toml::Value::String(digest.clone()))
        }
        ContentCheck::All(checks) => (
            "all",
            toml::Value::Array(checks.iter().map(encode_check).collect::<Result<_>>()?),
        ),
        ContentCheck::Magic {
            prefixes,
            trim_leading_whitespace,
        } => {
            if prefixes.is_empty() || prefixes.iter().any(Vec::is_empty) {
                return Err(invalid("magic requires nonempty prefixes"));
            }
            table.insert(
                "trim_leading_whitespace".into(),
                toml::Value::Boolean(*trim_leading_whitespace),
            );
            (
                "magic",
                toml::Value::Array(
                    prefixes
                        .iter()
                        .map(|p| match std::str::from_utf8(p) {
                            Ok(s) => toml::Value::String(s.into()),
                            Err(_) => toml::Value::Array(
                                p.iter()
                                    .map(|b| toml::Value::Integer(i64::from(*b)))
                                    .collect(),
                            ),
                        })
                        .collect(),
                ),
            )
        }
        ContentCheck::Custom(_) => {
            return Err(invalid("custom predicates cannot appear in manifests"))
        }
    };
    table.insert(name.into(), value);
    Ok(toml::Value::Table(table))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn declarative_checks_round_trip_without_losing_binary_prefixes() {
        let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(b"ABC"));
        let checks = vec![
            ContentCheck::None,
            ContentCheck::NotHtml,
            ContentCheck::MinBytes(3),
            ContentCheck::Sha256(digest),
            ContentCheck::magic(vec![b"ABC".to_vec(), vec![255, 0]], true),
            ContentCheck::All(vec![ContentCheck::NotHtml, ContentCheck::MinBytes(3)]),
        ];
        for check in checks {
            let artifact = Artifact::new(
                ArtifactKey::new("archive/data").unwrap(),
                vec![Source::new("https://archive.test/file?version=2").trusting_redirects()],
            )
            .with_check(check);
            let manifest = Manifest {
                artifacts: vec![artifact],
            };
            let encoded = manifest.to_toml_string().unwrap();
            let decoded = Manifest::from_toml_str(&encoded).unwrap();
            assert_eq!(encoded, decoded.to_toml_string().unwrap());
            for bytes in [b"ABC".as_slice(), b"<html>", &[255, 0, 1], b""] {
                assert_eq!(
                    manifest.artifacts[0].check.check(bytes).is_ok(),
                    decoded.artifacts[0].check.check(bytes).is_ok()
                );
            }
            assert!(decoded.artifacts[0].sources[0].trust_redirects);
        }
    }
    #[test]
    fn unknown_fields_and_custom_checks_fail_closed() {
        for suffix in [
            "check = {custom=true}",
            "check = {min_bites=10}",
            "check = {none=false}",
            "sha256 = 'bad'",
            "freshness = 'ttl'",
            "unknown = 1",
        ] {
            assert!(
                Manifest::from_toml_str(&format!("[[artifact]]\nkey='x'\n{suffix}\n")).is_err()
            );
        }
        let artifact = Artifact::new(ArtifactKey::new("x").unwrap(), vec![])
            .with_check(ContentCheck::custom(Arc::new(|_| Ok(()))));
        assert!(Manifest {
            artifacts: vec![artifact]
        }
        .to_toml_string()
        .is_err());
        assert!(Manifest::from_toml_str(
            "[[artifact]]\nkey='x'\nsources=['https://person:secret@host/path']"
        )
        .is_err());
    }
    #[test]
    fn defaults_pin_and_merge_are_stable() {
        let mut manifest =
            Manifest::from_toml_str("[[artifact]]\nkey='x'\n[[artifact]]\nkey='y'").unwrap();
        assert!(manifest.artifacts[0].check.check(b"small").is_err());
        let key = ArtifactKey::new("x").unwrap();
        let digest = "a".repeat(64);
        manifest.pin_sha256(&key, &digest).unwrap();
        let first = manifest.to_toml_string().unwrap();
        manifest.pin_sha256(&key, &digest).unwrap();
        assert_eq!(first, manifest.to_toml_string().unwrap());
        assert!(manifest.pin_sha256(&key, &"b".repeat(64)).is_err());
        manifest.merge(
            Manifest::from_toml_str(
                "[[artifact]]\nkey='y'\ncheck={none=true}\n[[artifact]]\nkey='z'",
            )
            .unwrap(),
        );
        assert_eq!(
            manifest
                .artifacts
                .iter()
                .map(|a| a.key.as_str())
                .collect::<Vec<_>>(),
            ["x", "y", "z"]
        );
        assert!(manifest.artifacts[1].check.check(b"").is_ok());
    }
}
