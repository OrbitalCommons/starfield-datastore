//! Configuration resolution (spec §11): explicit builder call, then
//! environment, then `~/.config/starfield/datastore.toml`, then defaults.

use crate::{DatastoreBuilder, DatastoreError, Mirror, Result};
use serde::Deserialize;
use std::path::PathBuf;

pub(crate) const ENV_CACHE_DIR: &str = "STARFIELD_CACHE_DIR";
pub(crate) const ENV_MIRROR: &str = "STARFIELD_MIRROR";
pub(crate) const ENV_MIRROR_REGION: &str = "STARFIELD_MIRROR_REGION";
pub(crate) const ENV_ALLOW_UPSTREAM: &str = "STARFIELD_ALLOW_UPSTREAM";
pub(crate) const ENV_OFFLINE: &str = "STARFIELD_OFFLINE";
pub(crate) const ENV_CACHE_MAX: &str = "STARFIELD_CACHE_MAX";
pub(crate) const ENV_CONFIG_FILE: &str = "STARFIELD_DATASTORE_CONFIG";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    cache_dir: Option<PathBuf>,
    mirror: Option<String>,
    mirror_region: Option<String>,
    allow_upstream: Option<bool>,
    offline: Option<bool>,
    cache_max: Option<u64>,
}

/// Fill every builder field the caller left unset from the environment,
/// then the config file. Explicit builder calls always win.
pub(crate) fn apply(mut builder: DatastoreBuilder) -> Result<DatastoreBuilder> {
    let file = read_file()?;

    if builder.cache_root.is_none() {
        builder.cache_root = env_var(ENV_CACHE_DIR).map(PathBuf::from).or(file.cache_dir);
    }
    if builder.mirror.is_none() {
        let region = env_var(ENV_MIRROR_REGION).or(file.mirror_region);
        builder.mirror = match env_var(ENV_MIRROR).or(file.mirror) {
            Some(spec) => Some(parse_mirror(&spec, region)?),
            None => None,
        };
    }
    if builder.allow_upstream.is_none() {
        builder.allow_upstream = env_bool(ENV_ALLOW_UPSTREAM)?.or(file.allow_upstream);
    }
    if builder.offline.is_none() {
        builder.offline = env_bool(ENV_OFFLINE)?.or(file.offline);
    }
    if builder.max_bytes.is_none() {
        builder.max_bytes = match env_var(ENV_CACHE_MAX) {
            Some(raw) => Some(raw.parse().map_err(|_| {
                DatastoreError::Config(format!("{ENV_CACHE_MAX} must be a byte count, got {raw:?}"))
            })?),
            None => file.cache_max,
        };
    }
    Ok(builder)
}

fn read_file() -> Result<FileConfig> {
    let Some(path) = config_path() else {
        return Ok(FileConfig::default());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileConfig::default()),
        Err(e) => return Err(e.into()),
    };
    toml::from_str(&text).map_err(|e| DatastoreError::Config(format!("{}: {e}", path.display())))
}

fn config_path() -> Option<PathBuf> {
    if let Some(explicit) = env_var(ENV_CONFIG_FILE) {
        return Some(PathBuf::from(explicit));
    }
    let base = env_var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".config")))?;
    Some(base.join("starfield").join("datastore.toml"))
}

/// `~/.cache/starfield`, shared with the pre-datastore downloader so nothing
/// is re-downloaded on the day of the switch.
pub(crate) fn default_cache_root() -> Result<PathBuf> {
    let base = env_var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".cache")))
        .ok_or_else(|| {
            DatastoreError::Config(format!(
                "cannot locate a cache directory: set {ENV_CACHE_DIR} or HOME"
            ))
        })?;
    Ok(base.join("starfield"))
}

fn home() -> Option<PathBuf> {
    env_var("HOME").map(PathBuf::from)
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_bool(name: &str) -> Result<Option<bool>> {
    env_var(name).map(|raw| parse_bool(name, &raw)).transpose()
}

fn parse_bool(name: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(DatastoreError::Config(format!(
            "{name} must be 1/0, true/false, yes/no or on/off, got {other:?}"
        ))),
    }
}

/// `https://…` is an HTTP mirror; `s3://bucket/prefix` is an S3 mirror and
/// needs a region from the argument, `AWS_REGION` or `AWS_DEFAULT_REGION`.
pub(crate) fn parse_mirror(spec: &str, region: Option<String>) -> Result<Mirror> {
    if let Some(rest) = spec.strip_prefix("s3://") {
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty() {
            return Err(DatastoreError::Config(format!(
                "mirror {spec:?} has no bucket"
            )));
        }
        let region = region
            .or_else(|| env_var("AWS_REGION"))
            .or_else(|| env_var("AWS_DEFAULT_REGION"))
            .ok_or_else(|| {
                DatastoreError::Config(format!(
                    "S3 mirror {spec} needs a region: set {ENV_MIRROR_REGION} or AWS_REGION"
                ))
            })?;
        return Ok(Mirror::S3 {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
            region,
            writable: false,
        });
    }
    if spec.starts_with("http://") || spec.starts_with("https://") {
        return Ok(Mirror::Http {
            base_url: spec.trim_end_matches('/').to_string(),
            writable: false,
        });
    }
    Err(DatastoreError::Config(format!(
        "mirror {spec:?} must be an http(s):// URL or s3://bucket/prefix"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booleans_are_lenient_but_explicit() {
        for raw in ["1", "true", "YES", "on"] {
            assert!(parse_bool("X", raw).unwrap());
        }
        for raw in ["0", "false", "No", "off"] {
            assert!(!parse_bool("X", raw).unwrap());
        }
        assert!(parse_bool("X", "maybe").is_err());
    }

    #[test]
    fn mirror_specs_parse() {
        assert_eq!(
            parse_mirror("https://ephem.example/", None).unwrap(),
            Mirror::Http {
                base_url: "https://ephem.example".into(),
                writable: false
            }
        );
        assert_eq!(
            parse_mirror("s3://bucket/some/prefix/", Some("us-west-2".into())).unwrap(),
            Mirror::S3 {
                bucket: "bucket".into(),
                prefix: "some/prefix".into(),
                region: "us-west-2".into(),
                writable: false
            }
        );
        assert!(parse_mirror("ftp://x", None).is_err());
        assert!(parse_mirror("s3:///prefix", None).is_err());
    }

    #[test]
    fn file_config_rejects_unknown_keys() {
        let parsed: std::result::Result<FileConfig, _> =
            toml::from_str("cache_dir = \"/x\"\nmirror = \"https://m\"\n");
        assert!(parsed.is_ok());
        let bad: std::result::Result<FileConfig, _> = toml::from_str("cache_dirr = \"/x\"\n");
        assert!(bad.is_err());
    }
}
