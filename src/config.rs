//! Configuration resolution (spec §11): explicit builder call, then
//! environment, then `~/.config/starfield/datastore.toml`, then defaults.

use crate::{
    ChainProvider, CredentialProvider, DatastoreBuilder, DatastoreError, EnvProvider, Mirror,
    NetrcProvider, Result,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(crate) const ENV_CACHE_DIR: &str = "STARFIELD_CACHE_DIR";
pub(crate) const ENV_MIRROR: &str = "STARFIELD_MIRROR";
pub(crate) const ENV_MIRROR_REGION: &str = "STARFIELD_MIRROR_REGION";
pub(crate) const ENV_ALLOW_UPSTREAM: &str = "STARFIELD_ALLOW_UPSTREAM";
pub(crate) const ENV_OFFLINE: &str = "STARFIELD_OFFLINE";
pub(crate) const ENV_CACHE_MAX: &str = "STARFIELD_CACHE_MAX";
pub(crate) const ENV_CONFIG_FILE: &str = "STARFIELD_DATASTORE_CONFIG";
/// `op://vault/item` reference for the 1Password provider (feature
/// `onepassword`); each host is a field of that item.
#[cfg(feature = "onepassword")]
pub(crate) const ENV_OP_ITEM: &str = "STARFIELD_OP_ITEM";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatastoreConfig {
    pub cache_dir: Option<PathBuf>,
    pub mirror: Option<String>,
    pub mirror_region: Option<String>,
    pub allow_upstream: Option<bool>,
    pub offline: Option<bool>,
    pub cache_max: Option<u64>,
    #[serde(default)]
    pub credentials: Vec<crate::CredentialConfig>,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceConfig>,
}

/// A named instance of the artifact service. Paths are relative to the config
/// file when loaded with `from_path`; CLI flags override these values.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub manifest: Option<PathBuf>,
    pub bucket: Option<String>,
    pub region: Option<String>,
    pub bind: Option<std::net::SocketAddr>,
    /// `None` inherits the configured/default chain. An explicit list selects
    /// only those named credentials, without ambient credential fallback.
    pub credentials: Option<Vec<String>>,
}

impl DatastoreConfig {
    pub fn from_env() -> Result<Self> {
        read_file()
    }

    pub fn from_path(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            DatastoreError::Config(format!(
                "cannot read configuration {}: {error}; supply --config or mount the file at this path",
                path.display()
            ))
        })?;
        let mut config = Self::from_toml_str(&text)?;
        let base = std::fs::canonicalize(path)?;
        let parent = base.parent().expect("a config file has a parent");
        if let Some(root) = &mut config.cache_dir {
            if root.is_relative() {
                *root = parent.join(&*root);
            }
        }
        for service in config.services.values_mut() {
            if let Some(manifest) = &mut service.manifest {
                if manifest.is_relative() {
                    *manifest = parent.join(&*manifest);
                }
            }
        }
        Ok(config)
    }

    pub fn from_toml_str(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).map_err(|error: toml::de::Error| {
            let line = error.span().map(|span| {
                text[..span.start.min(text.len())]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count()
                    + 1
            });
            DatastoreError::Config(format!(
                "invalid configuration{}; check field names and credential types",
                line.map(|n| format!(" at line {n}")).unwrap_or_default()
            ))
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_toml_string(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|_| {
            DatastoreError::Config("configuration cannot be represented as TOML".into())
        })
    }

    /// Apply environment overrides, then this file. Credentials remain lazy.
    pub fn builder(&self) -> Result<DatastoreBuilder> {
        self.validate()?;
        apply_file(DatastoreBuilder::default(), self.clone())
    }

    pub fn builder_for_service(&self, name: &str) -> Result<DatastoreBuilder> {
        let service = self
            .services
            .get(name)
            .ok_or_else(|| DatastoreError::Config(format!("unknown service {name}")))?;
        self.validate()?;
        let mut builder = DatastoreBuilder::default();
        if let Some(names) = &service.credentials {
            let entries = names
                .iter()
                .map(|name| {
                    self.credentials
                        .iter()
                        .find(|entry| &entry.name == name)
                        .cloned()
                        .ok_or_else(|| DatastoreError::Config(format!("unknown credential {name}")))
                })
                .collect::<Result<Vec<_>>>()?;
            builder.credentials = Some(provider_with_warnings(entries)?);
        }
        apply_file(builder, self.clone())
    }

    fn validate(&self) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        for entry in &self.credentials {
            if !names.insert(&entry.name) {
                return Err(DatastoreError::Config(format!(
                    "duplicate credential {}",
                    entry.name
                )));
            }
            crate::credential_config::validate_entries(std::slice::from_ref(entry))?;
        }
        for (service_name, service) in &self.services {
            if service_name.trim().is_empty() {
                return Err(DatastoreError::Config("service name is empty".into()));
            }
            if let Some(selected) = &service.credentials {
                for name in selected {
                    if !names.contains(name) {
                        return Err(DatastoreError::Config(format!(
                            "service {service_name} names unknown credential {name}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Fill every builder field the caller left unset from the environment,
/// then the config file. Explicit builder calls always win.
pub(crate) fn apply(builder: DatastoreBuilder) -> Result<DatastoreBuilder> {
    apply_file(builder, read_file()?)
}

fn apply_file(mut builder: DatastoreBuilder, file: DatastoreConfig) -> Result<DatastoreBuilder> {
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
    if builder.credentials.is_none() {
        builder.credentials = Some(Box::new(ChainProvider(vec![
            provider_with_warnings(file.credentials)?,
            default_credentials(),
        ])));
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

fn provider_with_warnings(
    entries: Vec<crate::CredentialConfig>,
) -> Result<Box<dyn CredentialProvider>> {
    let (provider, skipped) = crate::credential_config::configured_provider(entries)?;
    for warning in skipped {
        eprintln!("warning: {warning}");
    }
    Ok(provider)
}

/// What a client off the tailnet fetches upstream with (spec §2.4): its own
/// `STARFIELD_TOKEN_<HOST>` variables and `~/.netrc`, plus 1Password when
/// built in and `STARFIELD_OP_ITEM` names an item. Every provider looks its
/// credential up lazily, per request, so nothing is read until the upstream
/// layer is actually used.
fn default_credentials() -> Box<dyn CredentialProvider> {
    let chain: Vec<Box<dyn CredentialProvider>> =
        vec![Box::new(EnvProvider), Box::new(NetrcProvider)];
    #[cfg(feature = "onepassword")]
    let chain = match env_var(ENV_OP_ITEM) {
        Some(item) => {
            let mut chain = chain;
            chain.push(Box::new(crate::OnePasswordProvider { item }));
            chain
        }
        None => chain,
    };
    Box::new(ChainProvider(chain))
}

fn read_file() -> Result<DatastoreConfig> {
    let Some(path) = config_path() else {
        return Ok(DatastoreConfig::default());
    };
    if !path.exists() {
        return Ok(DatastoreConfig::default());
    }
    DatastoreConfig::from_path(&path)
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
        let parsed: std::result::Result<DatastoreConfig, _> =
            toml::from_str("cache_dir = \"/x\"\nmirror = \"https://m\"\n");
        assert!(parsed.is_ok());
        let bad: std::result::Result<DatastoreConfig, _> = toml::from_str("cache_dirr = \"/x\"\n");
        assert!(bad.is_err());
    }

    #[test]
    fn profiles_select_credentials_before_resolving_secrets() {
        let config = DatastoreConfig::from_toml_str(
            r#"
[[credentials]]
name = "first"
hosts = ["archive.example.org"]
type = "env"
variable = "SFD_CONFIG_UNSET_FIRST"
[[credentials]]
name = "second"
hosts = ["archive.example.org"]
type = "env"
variable = "SFD_CONFIG_UNSET_SECOND"
[services.first]
credentials = ["first"]
[services.second]
credentials = ["second"]
"#,
        )
        .unwrap();
        assert!(config.builder_for_service("first").is_ok());
        assert!(config.builder_for_service("second").is_ok());
        assert!(
            config.builder().is_err(),
            "an ambiguous active host selection fails"
        );
        assert!(config.builder_for_service("missing").is_err());
        assert!(DatastoreConfig::from_toml_str("[services.bad]\ncredentials=['missing']").is_err());
    }

    #[test]
    fn configuration_round_trips_unknown_payloads_without_debug_leaks() {
        let config = DatastoreConfig::from_toml_str(
            r#"
[[credentials]]
name = "future"
hosts = ["future.example.org"]
type = "future-auth"
options = { opaque = "sensitive-payload", flags = [true, false], count = 4 }
[services.preview]
credentials = ["future"]
"#,
        )
        .unwrap();
        let encoded = config.to_toml_string().unwrap();
        let decoded = DatastoreConfig::from_toml_str(&encoded).unwrap();
        assert_eq!(config.credentials, decoded.credentials);
        assert!(!format!("{config:?}").contains("sensitive-payload"));
        assert!(decoded.builder_for_service("preview").is_ok());
        let error = DatastoreConfig::from_toml_str(
            "[[credentials]]\nname='bad'\nhosts=['h']\ntype='basic'\npassword='secret-value'",
        )
        .unwrap_err();
        assert!(!error.to_string().contains("secret-value"));
    }

    #[test]
    fn config_relative_paths_follow_the_file_location() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "cache_dir='cache'\n[services.one]\nmanifest='kernels.toml'",
        )
        .unwrap();
        let config = DatastoreConfig::from_path(&path).unwrap();
        assert_eq!(config.cache_dir.unwrap(), dir.path().join("cache"));
        assert_eq!(
            config.services["one"].manifest.as_ref().unwrap(),
            &dir.path().join("kernels.toml")
        );
    }
}
