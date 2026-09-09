use crate::{DatastoreError, Result};
use std::{collections::HashMap, time::SystemTime};
use zeroize::Zeroize;

#[derive(Clone)]
pub struct Secret(String);
impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

#[derive(Clone)]
pub enum Credential {
    Basic {
        user: String,
        secret: Secret,
    },
    Bearer {
        secret: Secret,
        expires_at: Option<SystemTime>,
    },
}

pub trait CredentialProvider: Send + Sync {
    fn credential_for(&self, host: &str) -> Result<Option<Credential>>;
    fn identity_for(&self, host: &str) -> Option<String>;
}

pub struct StaticProvider(pub HashMap<String, Credential>);
impl CredentialProvider for StaticProvider {
    fn credential_for(&self, host: &str) -> Result<Option<Credential>> {
        Ok(self.0.get(host).cloned())
    }
    fn identity_for(&self, host: &str) -> Option<String> {
        self.0.contains_key(host).then(|| format!("static:{host}"))
    }
}

pub struct ChainProvider(pub Vec<Box<dyn CredentialProvider>>);
impl CredentialProvider for ChainProvider {
    fn credential_for(&self, host: &str) -> Result<Option<Credential>> {
        for provider in &self.0 {
            if let Some(value) = provider.credential_for(host)? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }
    fn identity_for(&self, host: &str) -> Option<String> {
        self.0
            .iter()
            .find_map(|provider| provider.identity_for(host))
    }
}

pub struct EnvProvider;
impl EnvProvider {
    fn variable(host: &str) -> String {
        format!(
            "STARFIELD_TOKEN_{}",
            host.chars()
                .map(|c| if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                })
                .collect::<String>()
        )
    }
}
impl CredentialProvider for EnvProvider {
    fn credential_for(&self, host: &str) -> Result<Option<Credential>> {
        match std::env::var(Self::variable(host)) {
            Ok(token) if !token.is_empty() => Ok(Some(Credential::Bearer {
                secret: Secret::new(token),
                expires_at: None,
            })),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(DatastoreError::Config(
                "credential environment variable is not UTF-8".into(),
            )),
        }
    }
    fn identity_for(&self, host: &str) -> Option<String> {
        self.credential_for(host)
            .ok()
            .flatten()
            .map(|_| format!("env:{host}"))
    }
}

/// Reads machine-specific entries in ~/.netrc. A default entry is deliberately
/// ignored so credentials cannot be sent to an arbitrary redirect destination.
pub struct NetrcProvider;
impl CredentialProvider for NetrcProvider {
    fn credential_for(&self, host: &str) -> Result<Option<Credential>> {
        let Some(home) = std::env::var_os("HOME") else {
            return Ok(None);
        };
        let path = std::path::PathBuf::from(home).join(".netrc");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => zeroize::Zeroizing::new(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&path)?.permissions().mode() & 0o077 != 0 {
                return Err(DatastoreError::Config(
                    ".netrc must not be accessible to group or other users (chmod 600)".into(),
                ));
            }
        }
        parse_netrc(&text, host)
    }
    fn identity_for(&self, host: &str) -> Option<String> {
        self.credential_for(host)
            .ok()
            .flatten()
            .map(|_| format!("netrc:{host}"))
    }
}

fn parse_netrc(text: &str, host: &str) -> Result<Option<Credential>> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut comment = false;
    for c in text.chars() {
        if comment {
            if c == '\n' {
                comment = false;
            } else {
                continue;
            }
        }
        if escaped {
            token.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '"' {
            quoted = !quoted;
            continue;
        }
        if c == '#' && !quoted {
            comment = true;
        }
        if (c.is_whitespace() || comment) && !quoted {
            if !token.is_empty() {
                tokens.push(zeroize::Zeroizing::new(std::mem::take(&mut token)));
            }
        } else {
            token.push(c);
        }
    }
    if quoted || escaped {
        token.zeroize();
        return Err(DatastoreError::Config("invalid .netrc quoting".into()));
    }
    if !token.is_empty() {
        tokens.push(zeroize::Zeroizing::new(token));
    }
    let mut i = 0;
    let mut machine = None;
    let mut user = None;
    let mut password = None;
    while i < tokens.len() {
        let field = tokens[i].as_str();
        if field == "machine" || field == "default" {
            if machine.as_deref() == Some(host) {
                return netrc_credential(user, password);
            }
            machine = None;
            user = None;
            password = None;
            if field == "machine" {
                i += 1;
                machine = tokens.get(i).map(|s| s.to_string());
            }
        } else if field == "login" || field == "password" || field == "account" {
            i += 1;
            let value = tokens
                .get(i)
                .ok_or_else(|| DatastoreError::Config("incomplete .netrc entry".into()))?;
            if field == "login" {
                user = Some(value.to_string());
            }
            if field == "password" {
                password = Some(Secret::new(value.to_string()));
            }
        } else {
            return Err(DatastoreError::Config(
                "unsupported or malformed .netrc field".into(),
            ));
        }
        i += 1;
    }
    if machine.as_deref() == Some(host) {
        netrc_credential(user, password)
    } else {
        Ok(None)
    }
}

fn netrc_credential(user: Option<String>, password: Option<Secret>) -> Result<Option<Credential>> {
    match (user, password) {
        (Some(user), Some(secret)) => Ok(Some(Credential::Basic { user, secret })),
        _ => Err(DatastoreError::Config(
            ".netrc machine needs login and password".into(),
        )),
    }
}

/// `item` is an op://vault/item reference; each host names a bearer-token field.
#[cfg(feature = "onepassword")]
pub struct OnePasswordProvider {
    pub item: String,
}
#[cfg(feature = "onepassword")]
impl CredentialProvider for OnePasswordProvider {
    fn credential_for(&self, host: &str) -> Result<Option<Credential>> {
        if !self.item.starts_with("op://")
            || host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
        {
            return Err(DatastoreError::Config(
                "invalid 1Password reference or host".into(),
            ));
        }
        let output = std::process::Command::new("op")
            .args([
                "read",
                &format!("{}/{host}", self.item.trim_end_matches('/')),
            ])
            .output()?;
        let stdout = zeroize::Zeroizing::new(output.stdout);
        let _stderr = zeroize::Zeroizing::new(output.stderr);
        if !output.status.success() {
            return Err(DatastoreError::Config("1Password read failed".into()));
        }
        let token = std::str::from_utf8(&stdout)
            .map_err(|_| DatastoreError::Config("1Password field is not UTF-8".into()))?
            .trim_end_matches(['\r', '\n']);
        if token.is_empty() {
            return Ok(None);
        }
        Ok(Some(Credential::Bearer {
            secret: Secret::new(token.to_owned()),
            expires_at: None,
        }))
    }
    fn identity_for(&self, host: &str) -> Option<String> {
        Some(format!("onepassword:{host}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets_and_identities_are_redacted() {
        let secret = Secret::new("sensitive".into());
        assert_eq!(format!("{secret:?}"), "Secret([redacted])");
        let provider = StaticProvider(HashMap::from([(
            "archive.test".into(),
            Credential::Basic {
                user: "person".into(),
                secret,
            },
        )]));
        assert_eq!(
            provider.identity_for("archive.test").as_deref(),
            Some("static:archive.test")
        );
        assert!(provider.credential_for("elsewhere.test").unwrap().is_none());
    }
    #[test]
    fn netrc_is_host_specific_and_supports_quoted_values() {
        let text = "# comment\nmachine urs.test login person password \"a b\\\"c\"\nmachine other.test login x password y\n";
        let Some(Credential::Basic { user, secret }) = parse_netrc(text, "urs.test").unwrap()
        else {
            panic!("missing credential");
        };
        assert_eq!(user, "person");
        assert_eq!(secret.expose(), "a b\"c");
        assert!(parse_netrc(text, "unknown.test").unwrap().is_none());
        assert!(
            parse_netrc("default login user password secret", "unknown.test")
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn chain_returns_first_match_and_host_env_name_is_stable() {
        assert_eq!(
            EnvProvider::variable("urs.earthdata.nasa.gov"),
            "STARFIELD_TOKEN_URS_EARTHDATA_NASA_GOV"
        );
        let chain = ChainProvider(vec![
            Box::new(StaticProvider(HashMap::new())),
            Box::new(StaticProvider(HashMap::from([(
                "x".into(),
                Credential::Bearer {
                    secret: Secret::new("token".into()),
                    expires_at: None,
                },
            )]))),
        ]);
        assert!(chain.credential_for("x").unwrap().is_some());
        assert_eq!(chain.identity_for("x").as_deref(), Some("static:x"));
    }
}
