//! Named credential entries from the config file (spec §11).
//!
//! Each entry names the hosts it serves and a typed source. Known types are
//! validated when the file is loaded. An entry of a type this build does not
//! recognise is kept verbatim so the file round-trips, contributes no
//! credential and no identity, and is reported — by name and type, never by
//! content — so the operator can warn at startup. No entry ever carries a
//! literal secret: passwords and tokens are named by environment variable,
//! `.netrc`, or a 1Password reference, and every secret is read at request
//! time so rotation needs no restart.

use crate::{Credential, CredentialProvider, DatastoreError, NetrcProvider, Result, Secret};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// One credential entry: which hosts it serves and where the secret comes
/// from.
#[derive(Debug, Clone, PartialEq)]
pub struct CredentialConfig {
    pub name: String,
    pub hosts: Vec<String>,
    pub source: CredentialSource,
}

/// Where an entry's secret comes from. Every known variant names a secret
/// indirectly; the file never holds one.
#[derive(Clone, PartialEq)]
pub enum CredentialSource {
    /// Bearer token read from an environment variable at request time.
    Env { variable: String },
    /// HTTP Basic with the password read from an environment variable.
    Basic {
        username: String,
        password_env: String,
    },
    /// `~/.netrc`, looked up by the requested host.
    Netrc,
    /// `op read <reference>` at request time (feature `onepassword`).
    OnePassword { reference: String },
    /// A type this build does not know. Preserved exactly, including its
    /// `type`, so the file round-trips; using it is an error.
    Unrecognized(Value),
}

const TYPE: &str = "type";
const LITERAL_SECRET_KEYS: [&str; 4] = ["password", "token", "secret", "api_key"];

impl std::fmt::Debug for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env { variable } => f.debug_struct("Env").field("variable", variable).finish(),
            Self::Basic {
                username,
                password_env,
            } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password_env", password_env)
                .finish(),
            Self::Netrc => f.write_str("Netrc"),
            Self::OnePassword { reference } => f
                .debug_struct("OnePassword")
                .field("reference", reference)
                .finish(),
            Self::Unrecognized(raw) => write!(f, "Unrecognized(type = {:?})", kind_of(raw)),
        }
    }
}

impl CredentialSource {
    /// The `type` string as written, or `"<missing>"`.
    pub fn kind(&self) -> &str {
        match self {
            Self::Env { .. } => "env",
            Self::Basic { .. } => "basic",
            Self::Netrc => "netrc",
            Self::OnePassword { .. } => "onepassword",
            Self::Unrecognized(raw) => kind_of(raw),
        }
    }
}

fn kind_of(raw: &Value) -> &str {
    raw.get(TYPE).and_then(Value::as_str).unwrap_or("<missing>")
}

#[derive(Deserialize)]
struct Envelope {
    name: String,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(flatten)]
    source: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvFields {
    variable: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BasicFields {
    username: String,
    password_env: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NetrcFields {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OnePasswordFields {
    reference: String,
}

impl<'de> Deserialize<'de> for CredentialConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let envelope = Envelope::deserialize(deserializer)?;
        let name = envelope.name;
        let source = decode_source(&name, envelope.source).map_err(D::Error::custom)?;
        Ok(Self {
            name,
            hosts: envelope.hosts,
            source,
        })
    }
}

/// Two-stage decode: the `type` string decides the shape; a known type is
/// held to its exact field set, anything else is kept verbatim.
fn decode_source(
    name: &str,
    raw: Map<String, Value>,
) -> std::result::Result<CredentialSource, String> {
    let kind = match raw.get(TYPE) {
        Some(Value::String(kind)) => kind.clone(),
        Some(_) => return Err(format!("credential \"{name}\": `type` must be a string")),
        None => return Err(format!("credential \"{name}\" has no `type`")),
    };
    for key in LITERAL_SECRET_KEYS {
        if raw.contains_key(key) {
            return Err(format!(
                "credential \"{name}\" carries a literal `{key}`; name an environment variable with `{key}_env` or use netrc/onepassword instead"
            ));
        }
    }
    let mut fields = raw.clone();
    fields.remove(TYPE);
    let fields = Value::Object(fields);
    let malformed = |e: serde_json::Error| format!("credential \"{name}\" (type {kind}): {e}");
    Ok(match kind.as_str() {
        "env" => {
            let EnvFields { variable } = serde_json::from_value(fields).map_err(malformed)?;
            CredentialSource::Env { variable }
        }
        "basic" => {
            let BasicFields {
                username,
                password_env,
            } = serde_json::from_value(fields).map_err(malformed)?;
            CredentialSource::Basic {
                username,
                password_env,
            }
        }
        "netrc" => {
            let NetrcFields {} = serde_json::from_value(fields).map_err(malformed)?;
            CredentialSource::Netrc
        }
        "onepassword" => {
            let OnePasswordFields { reference } =
                serde_json::from_value(fields).map_err(malformed)?;
            CredentialSource::OnePassword { reference }
        }
        _ => CredentialSource::Unrecognized(Value::Object(raw)),
    })
}

impl Serialize for CredentialConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = Map::new();
        map.insert("name".into(), Value::String(self.name.clone()));
        map.insert(
            "hosts".into(),
            Value::Array(self.hosts.iter().cloned().map(Value::String).collect()),
        );
        match &self.source {
            CredentialSource::Env { variable } => {
                map.insert(TYPE.into(), "env".into());
                map.insert("variable".into(), variable.clone().into());
            }
            CredentialSource::Basic {
                username,
                password_env,
            } => {
                map.insert(TYPE.into(), "basic".into());
                map.insert("username".into(), username.clone().into());
                map.insert("password_env".into(), password_env.clone().into());
            }
            CredentialSource::Netrc => {
                map.insert(TYPE.into(), "netrc".into());
            }
            CredentialSource::OnePassword { reference } => {
                map.insert(TYPE.into(), "onepassword".into());
                map.insert("reference".into(), reference.clone().into());
            }
            CredentialSource::Unrecognized(raw) => {
                let Value::Object(fields) = raw else {
                    return Err(serde::ser::Error::custom(format!(
                        "credential \"{}\": unrecognised source is not a table",
                        self.name
                    )));
                };
                for (key, value) in fields {
                    map.insert(key.clone(), value.clone());
                }
            }
        }
        map.serialize(serializer)
    }
}

/// An entry `configured_provider` left out because this build does not
/// know its type. Print it as a warning at startup; it names no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedCredential {
    pub name: String,
    pub kind: String,
}

impl std::fmt::Display for SkippedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "credential \"{}\" has unrecognised type \"{}\" and is skipped; its hosts get no credential from this entry",
            self.name, self.kind
        )
    }
}

/// Everything about a list of entries that can be checked without touching
/// a secret: non-empty unique names, well-formed hosts, required fields.
/// Hosts may repeat across entries here, because two service profiles may
/// legitimately reach one archive with different credentials; a clash is
/// an error only within one active selection (`configured_provider`).
pub fn validate_entries(entries: &[CredentialConfig]) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for entry in entries {
        let invalid = |detail: String| {
            DatastoreError::Config(format!("credential \"{}\": {detail}", entry.name))
        };
        if entry.name.trim().is_empty() {
            return Err(DatastoreError::Config(
                "a credential entry has an empty name".into(),
            ));
        }
        if !names.insert(entry.name.as_str()) {
            return Err(invalid("name is used twice".into()));
        }
        if entry.hosts.is_empty() {
            return Err(invalid("lists no hosts".into()));
        }
        for host in &entry.hosts {
            if !is_bare_host(host) {
                return Err(invalid(format!(
                    "host {host:?} must be a bare hostname: no port, path, scheme or wildcard"
                )));
            }
        }
        match &entry.source {
            CredentialSource::Env { variable } if variable.trim().is_empty() => {
                return Err(invalid("`variable` is empty".into()));
            }
            CredentialSource::Basic {
                username,
                password_env,
            } if username.is_empty() || password_env.trim().is_empty() => {
                return Err(invalid("`username` and `password_env` are required".into()));
            }
            CredentialSource::OnePassword { reference } if !reference.starts_with("op://") => {
                return Err(invalid("`reference` must start with op://".into()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_bare_host(host: &str) -> bool {
    let host = host.trim();
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// Build the lazy provider for one active selection of entries. Validates
/// them, rejects a host claimed twice within the selection, and leaves out
/// entries of unrecognised type, returning those so the caller can warn.
pub fn configured_provider(
    entries: Vec<CredentialConfig>,
) -> Result<(Box<dyn CredentialProvider>, Vec<SkippedCredential>)> {
    validate_entries(&entries)?;
    let mut by_host: HashMap<String, Arc<CredentialConfig>> = HashMap::new();
    let mut skipped = Vec::new();
    for mut entry in entries {
        if let CredentialSource::Unrecognized(raw) = &entry.source {
            skipped.push(SkippedCredential {
                name: entry.name.clone(),
                kind: kind_of(raw).to_string(),
            });
            continue;
        }
        for host in &mut entry.hosts {
            *host = host.trim().to_ascii_lowercase();
        }
        let entry = Arc::new(entry);
        for host in &entry.hosts {
            if let Some(other) = by_host.insert(host.clone(), entry.clone()) {
                return Err(DatastoreError::Config(format!(
                    "host {host} is claimed by both credential \"{}\" and \"{}\" in the same selection",
                    other.name, entry.name
                )));
            }
        }
    }
    Ok((Box::new(ConfigProvider { by_host }), skipped))
}

struct ConfigProvider {
    by_host: HashMap<String, Arc<CredentialConfig>>,
}

impl ConfigProvider {
    fn entry(&self, host: &str) -> Option<&CredentialConfig> {
        self.by_host
            .get(&host.to_ascii_lowercase())
            .map(Arc::as_ref)
    }
}

impl CredentialProvider for ConfigProvider {
    fn credential_for(&self, host: &str) -> Result<Option<Credential>> {
        let Some(entry) = self.entry(host) else {
            return Ok(None);
        };
        let unset = |variable: &str| {
            DatastoreError::Config(format!(
                "credential \"{}\" for {host} needs environment variable {variable}, which is not set",
                entry.name
            ))
        };
        match &entry.source {
            CredentialSource::Env { variable } => Ok(Some(Credential::Bearer {
                secret: Secret::new(read_env(variable).ok_or_else(|| unset(variable))?),
                expires_at: None,
            })),
            CredentialSource::Basic {
                username,
                password_env,
            } => Ok(Some(Credential::Basic {
                user: username.clone(),
                secret: Secret::new(read_env(password_env).ok_or_else(|| unset(password_env))?),
            })),
            CredentialSource::Netrc => NetrcProvider.credential_for(host),
            CredentialSource::OnePassword { reference } => one_password(&entry.name, reference),
            CredentialSource::Unrecognized(_) => Ok(None),
        }
    }

    fn identity_for(&self, host: &str) -> Option<String> {
        self.entry(host)
            .map(|entry| format!("config:{}", entry.name))
    }
}

fn read_env(variable: &str) -> Option<String> {
    std::env::var(variable).ok().filter(|v| !v.is_empty())
}

#[cfg(feature = "onepassword")]
fn one_password(name: &str, reference: &str) -> Result<Option<Credential>> {
    use zeroize::Zeroizing;
    let output = std::process::Command::new("op")
        .args(["read", reference])
        .output()?;
    let stdout = Zeroizing::new(output.stdout);
    let _stderr = Zeroizing::new(output.stderr);
    if !output.status.success() {
        return Err(DatastoreError::Config(format!(
            "credential \"{name}\": `op read` failed for its reference"
        )));
    }
    let token = std::str::from_utf8(&stdout)
        .map_err(|_| {
            DatastoreError::Config(format!(
                "credential \"{name}\": 1Password field is not UTF-8"
            ))
        })?
        .trim_end_matches(['\r', '\n']);
    if token.is_empty() {
        return Ok(None);
    }
    Ok(Some(Credential::Bearer {
        secret: Secret::new(token.to_owned()),
        expires_at: None,
    }))
}

#[cfg(not(feature = "onepassword"))]
fn one_password(name: &str, _reference: &str) -> Result<Option<Credential>> {
    Err(DatastoreError::Config(format!(
        "credential \"{name}\" uses 1Password, but this build has no `onepassword` feature"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    struct Doc {
        credential: Vec<CredentialConfig>,
    }

    const FILE: &str = r#"
[[credential]]
name = "earthdata"
hosts = ["urs.earthdata.nasa.gov", "LADSWEB.modaps.eosdis.nasa.gov"]
type = "netrc"

[[credential]]
name = "mast"
hosts = ["archive.stsci.edu"]
type = "env"
variable = "MAST_TOKEN"

[[credential]]
name = "usgs"
hosts = ["astrogeology.usgs.gov"]
type = "basic"
username = "person"
password_env = "USGS_PW"

[[credential]]
name = "vault"
hosts = ["data.example.org"]
type = "onepassword"
reference = "op://infra/example/token"

[[credential]]
name = "future"
hosts = ["oauth.example.org"]
type = "oauth2-device"
client_id = "abc"
scopes = ["read", "write"]
[credential.extra]
nested = 1
"#;

    fn load() -> Vec<CredentialConfig> {
        toml::from_str::<Doc>(FILE).unwrap().credential
    }

    #[test]
    fn known_types_decode_and_unknown_types_round_trip_verbatim() {
        let entries = load();
        assert_eq!(entries[0].source, CredentialSource::Netrc);
        assert_eq!(
            entries[1].source,
            CredentialSource::Env {
                variable: "MAST_TOKEN".into()
            }
        );
        assert_eq!(
            entries[2].source,
            CredentialSource::Basic {
                username: "person".into(),
                password_env: "USGS_PW".into()
            }
        );
        assert_eq!(
            entries[3].source,
            CredentialSource::OnePassword {
                reference: "op://infra/example/token".into()
            }
        );
        let CredentialSource::Unrecognized(raw) = &entries[4].source else {
            panic!("unknown type must be preserved");
        };
        assert_eq!(raw["type"], "oauth2-device");
        assert_eq!(raw["client_id"], "abc");
        assert_eq!(raw["scopes"][1], "write");
        assert_eq!(raw["extra"]["nested"], 1);

        let text = toml::to_string(&Doc {
            credential: entries.clone(),
        })
        .unwrap();
        assert!(text.contains("type = \"oauth2-device\""), "{text}");
        assert!(text.contains("client_id = \"abc\""), "{text}");
        let again = toml::from_str::<Doc>(&text).unwrap().credential;
        assert_eq!(again, entries, "round trip is lossless");
    }

    #[test]
    fn malformed_known_types_and_literal_secrets_fail_at_load() {
        let bad = |body: &str| {
            toml::from_str::<Doc>(&format!(
                "[[credential]]\nname = \"x\"\nhosts = [\"h\"]\n{body}\n"
            ))
            .unwrap_err()
            .to_string()
        };
        assert!(bad("type = \"env\"").contains("missing field `variable`"));
        assert!(
            bad("type = \"env\"\nvariable = \"V\"\nextra = 1").contains("unknown field `extra`")
        );
        assert!(bad("type = \"netrc\"\nlogin = \"u\"").contains("unknown field `login`"));
        assert!(
            bad("type = \"basic\"\nusername = \"u\"\npassword = \"hunter2\"")
                .contains("password_env")
        );
        let err = bad("type = \"env\"\nvariable = \"V\"\ntoken = \"sup3r\"");
        assert!(err.contains("token_env"), "{err}");
        assert!(
            !err.contains("sup3r"),
            "a literal secret is never echoed: {err}"
        );
        assert!(bad("variable = \"V\"").contains("has no `type`"));
        assert!(bad("type = 3").contains("must be a string"));
    }

    #[test]
    fn validation_rejects_bad_names_hosts_and_fields_but_allows_shared_hosts() {
        let entry = |name: &str, hosts: &[&str]| CredentialConfig {
            name: name.into(),
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            source: CredentialSource::Netrc,
        };
        let err =
            |entries: Vec<CredentialConfig>| validate_entries(&entries).err().unwrap().to_string();
        assert!(err(vec![entry("a", &["h"]), entry("a", &["i"])]).contains("used twice"));
        assert!(err(vec![entry("a", &[])]).contains("lists no hosts"));
        assert!(err(vec![entry("a", &["https://h"])]).contains("bare hostname"));
        assert!(err(vec![entry("a", &["h:8080"])]).contains("bare hostname"));
        assert!(err(vec![entry("a", &["*.h"])]).contains("bare hostname"));
        assert!(err(vec![entry("", &["h"])]).contains("empty name"));
        assert!(err(vec![CredentialConfig {
            name: "v".into(),
            hosts: vec!["h".into()],
            source: CredentialSource::OnePassword {
                reference: "infra/x".into()
            },
        }])
        .contains("op://"));
        assert!(err(vec![CredentialConfig {
            name: "e".into(),
            hosts: vec!["h".into()],
            source: CredentialSource::Env {
                variable: " ".into()
            },
        }])
        .contains("`variable` is empty"));
        // Two profiles may reach one archive with different credentials.
        assert!(validate_entries(&[entry("a", &["h"]), entry("b", &["H"])]).is_ok());
        // …but not within one active selection.
        let clash = configured_provider(vec![entry("a", &["h"]), entry("b", &["H"])])
            .err()
            .unwrap()
            .to_string();
        assert!(
            clash.contains("claimed by both") && clash.contains("same selection"),
            "{clash}"
        );
        assert!(configured_provider(vec![entry("a", &["h"]), entry("b", &["i"])]).is_ok());
    }

    #[test]
    fn unknown_entries_are_skipped_with_a_safe_warning_and_grant_nothing() {
        let (provider, skipped) = configured_provider(load()).unwrap();
        assert_eq!(
            skipped,
            vec![SkippedCredential {
                name: "future".into(),
                kind: "oauth2-device".into()
            }]
        );
        let warning = skipped[0].to_string();
        assert!(
            warning.contains("\"future\"") && warning.contains("oauth2-device"),
            "{warning}"
        );
        assert!(
            !warning.contains("client_id") && !warning.contains("abc"),
            "no payload: {warning}"
        );
        assert!(provider
            .credential_for("oauth.example.org")
            .unwrap()
            .is_none());
        assert!(provider.identity_for("oauth.example.org").is_none());
        assert!(
            validate_entries(&load()).is_ok(),
            "validation itself never warns or fails on unknown types"
        );
    }

    #[test]
    fn known_entries_resolve_lazily_by_exact_host() {
        std::env::set_var("CREDENTIAL_CONFIG_TEST_MAST", "mast-token");
        let mut entries = load();
        entries[1].source = CredentialSource::Env {
            variable: "CREDENTIAL_CONFIG_TEST_MAST".into(),
        };
        let (provider, _) = configured_provider(entries).unwrap();
        let Some(Credential::Bearer { secret, .. }) =
            provider.credential_for("archive.stsci.edu").unwrap()
        else {
            panic!("env entry resolves when its host is requested");
        };
        assert_eq!(secret.expose(), "mast-token");
        assert_eq!(
            provider.identity_for("ARCHIVE.stsci.edu").as_deref(),
            Some("config:mast")
        );
        assert!(provider.credential_for("nasa.gov").unwrap().is_none());
        assert!(provider
            .credential_for("sub.archive.stsci.edu")
            .unwrap()
            .is_none());
        assert!(provider.identity_for("nasa.gov").is_none());
        assert_eq!(
            provider
                .identity_for("ladsweb.modaps.eosdis.nasa.gov")
                .as_deref(),
            Some("config:earthdata")
        );
        std::env::remove_var("USGS_PW");
        let err = provider
            .credential_for("astrogeology.usgs.gov")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("USGS_PW") && err.contains("\"usgs\""), "{err}");
    }

    #[test]
    fn debug_never_prints_an_unknown_payload() {
        let entries = load();
        let text = format!("{:?}", entries[4]);
        assert!(
            text.contains("Unrecognized(type = \"oauth2-device\")"),
            "{text}"
        );
        assert!(!text.contains("client_id"), "{text}");
        assert_eq!(entries[4].source.kind(), "oauth2-device");
        assert_eq!(entries[1].source.kind(), "env");
    }
}
