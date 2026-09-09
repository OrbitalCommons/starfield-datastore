//! The HTTP fetch engine shared by the upstream layer and `Mirror::Http`.
//!
//! Redirects are followed by hand so the credential rules of spec §6 apply
//! per hop:
//!
//! - every hop uses the credential the provider holds for *that hop's* host,
//!   which is how Earthdata's redirect through `urs.earthdata.nasa.gov` works;
//! - a hop whose host has no credential of its own receives the original
//!   source's credential only when `Source::trust_redirects` is set;
//! - with no provider (the mirror client) nothing is ever sent, so a
//!   presigned redirect is followed bare.
//!
//! The body is streamed to the caller's sink; it never lives in memory.

use crate::store::ProgressFn;
use crate::{Credential, CredentialProvider, DatastoreError, Result, Source};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{CONTENT_LENGTH, ETAG, LOCATION};
use reqwest::redirect::Policy;
use reqwest::StatusCode;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use url::Url;

const MAX_HOPS: usize = 10;
const CHUNK: usize = 64 * 1024;

/// How a transfer reports on itself.
#[derive(Clone)]
pub(crate) enum Progress {
    Off,
    /// `indicatif` bars on stderr (feature `progress`); a no-op otherwise.
    Bars,
    Callback(Arc<ProgressFn>),
}

pub(crate) struct Fetcher {
    client: Client,
    provider: Option<Arc<dyn CredentialProvider>>,
    progress: Progress,
}

/// What a completed download looked like, for the index sidecar.
pub(crate) struct Download {
    pub etag: Option<String>,
    /// Provider identity of the credential that was actually sent, if any.
    pub provider_identity: Option<String>,
}

impl Fetcher {
    pub(crate) fn new(
        timeout: Duration,
        provider: Option<Arc<dyn CredentialProvider>>,
        progress: Progress,
    ) -> Result<Self> {
        // `timeout(None)`: the blocking client defaults to a 30 s *total*
        // timeout, which would kill every large download.
        let client = Client::builder()
            .connect_timeout(timeout)
            .timeout(None)
            .redirect(Policy::none())
            .cookie_store(true)
            .no_proxy()
            .user_agent(concat!("starfield-datastore/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            client,
            provider,
            progress,
        })
    }

    /// GET `source`, streaming the body into `sink`.
    ///
    /// `Ok(None)` is a definite HTTP 404. Authentication failures map to
    /// `NoCredential` (nothing was sent) or `CredentialRejected` (something
    /// was); every other non-success status is an `Http` error.
    pub(crate) fn request(
        &self,
        source: &Source,
        sink: &mut dyn Write,
    ) -> Result<Option<Download>> {
        let mut url = parse_source_url(&source.url)?;
        let origin_host = host_of(&url)?;
        let origin = self.lookup(&origin_host)?;
        let mut identity_used = None;

        for hop in 0..MAX_HOPS {
            let host = host_of(&url)?;
            let (credential, credential_host) = match self.lookup(&host)? {
                Some(c) => (Some(c), host.clone()),
                None if hop > 0 && source.trust_redirects => (origin.clone(), origin_host.clone()),
                None => (None, host.clone()),
            };
            let request = self.client.get(url.clone());
            let (request, looks_expired) = apply(request, credential.as_ref())?;
            if let (Some(_), Some(provider)) = (&credential, &self.provider) {
                identity_used = provider.identity_for(&credential_host);
            }

            let response = request.send()?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        DatastoreError::Mirror(format!(
                            "{host} redirected without a Location header"
                        ))
                    })?;
                let target = url.join(location).map_err(|e| {
                    DatastoreError::Mirror(format!("bad redirect from {host}: {e}"))
                })?;
                url = validate_hop(&url, target)?;
                continue;
            }
            if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                return Err(if credential.is_some() {
                    DatastoreError::CredentialRejected {
                        host,
                        status: status.as_u16(),
                        looks_expired,
                    }
                } else {
                    DatastoreError::NoCredential { host }
                });
            }
            if status == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            let response = response.error_for_status()?;
            let etag = response
                .headers()
                .get(ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            self.stream(response, sink, &host)?;
            return Ok(Some(Download {
                etag,
                provider_identity: identity_used,
            }));
        }
        Err(DatastoreError::Mirror(format!(
            "more than {MAX_HOPS} redirects from {}",
            sanitize(&source.url)
        )))
    }

    fn lookup(&self, host: &str) -> Result<Option<Credential>> {
        match &self.provider {
            Some(p) => p.credential_for(host),
            None => Ok(None),
        }
    }

    fn stream(&self, mut response: Response, sink: &mut dyn Write, host: &str) -> Result<u64> {
        let expected = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let bar = match &self.progress {
            Progress::Bars => progress_bar(expected, host),
            Progress::Off | Progress::Callback(_) => None,
        };
        let callback = match &self.progress {
            Progress::Callback(cb) => Some(cb),
            Progress::Off | Progress::Bars => None,
        };
        let mut buffer = vec![0u8; CHUNK];
        let mut total = 0u64;
        loop {
            let n = response.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            sink.write_all(&buffer[..n])?;
            total += n as u64;
            if let Some(bar) = &bar {
                bar.set_position(total);
            }
            if let Some(cb) = callback {
                cb(total, expected);
            }
        }
        if let Some(bar) = bar {
            bar.finish_and_clear();
        }
        Ok(total)
    }
}

fn apply(
    request: RequestBuilder,
    credential: Option<&Credential>,
) -> Result<(RequestBuilder, bool)> {
    Ok(match credential {
        None => (request, false),
        Some(Credential::Basic { user, secret }) => {
            (request.basic_auth(user, Some(secret.expose())), false)
        }
        Some(Credential::Bearer { secret, expires_at }) => {
            let expired = expires_at.is_some_and(|at| at <= SystemTime::now());
            (request.bearer_auth(secret.expose()), expired)
        }
    })
}

/// Every redirect target is held to the same rules as a source URL, plus no
/// downgrade to plaintext: a credential looked up for the target host must
/// never travel over http because an https archive said so.
fn validate_hop(from: &Url, to: Url) -> Result<Url> {
    let checked = parse_source_url(to.as_str())?;
    if from.scheme() == "https" && checked.scheme() != "https" {
        return Err(DatastoreError::Mirror(format!(
            "{} redirected to plaintext {}; refusing the downgrade",
            sanitize(from.as_str()),
            sanitize(checked.as_str())
        )));
    }
    Ok(checked)
}

fn parse_source_url(raw: &str) -> Result<Url> {
    let url =
        Url::parse(raw).map_err(|e| DatastoreError::Config(format!("invalid source URL: {e}")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(DatastoreError::Config(format!(
            "source URL {} carries userinfo; supply credentials through a CredentialProvider",
            sanitize(raw)
        )));
    }
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(DatastoreError::Config(format!(
            "source URL {} is not http(s)",
            sanitize(raw)
        )));
    }
    Ok(url)
}

fn host_of(url: &Url) -> Result<String> {
    url.host_str().map(str::to_string).ok_or_else(|| {
        DatastoreError::Config(format!("URL {} has no host", sanitize(url.as_str())))
    })
}

/// Origin and path only: no query (a presigned URL's signature lives there),
/// no fragment, no userinfo.
pub(crate) fn sanitize(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{}://{host}:{port}{}", url.scheme(), url.path()),
            (Some(host), None) => format!("{}://{host}{}", url.scheme(), url.path()),
            (None, _) => format!("{}:{}", url.scheme(), url.path()),
        },
        Err(_) => "<unparseable url>".into(),
    }
}

#[cfg(feature = "progress")]
fn progress_bar(expected: Option<u64>, host: &str) -> Option<indicatif::ProgressBar> {
    use indicatif::{ProgressBar, ProgressStyle};
    let bar = match expected {
        Some(len) => ProgressBar::new(len).with_style(
            ProgressStyle::with_template(
                "{msg} {bar:30} {bytes}/{total_bytes} {bytes_per_sec} {eta}",
            )
            .expect("static template"),
        ),
        None => ProgressBar::new_spinner().with_style(
            ProgressStyle::with_template("{msg} {spinner} {bytes} {bytes_per_sec}")
                .expect("static template"),
        ),
    };
    bar.set_message(host.to_string());
    Some(bar)
}

#[cfg(not(feature = "progress"))]
fn progress_bar(_expected: Option<u64>, _host: &str) -> Option<NoBar> {
    None
}

#[cfg(not(feature = "progress"))]
struct NoBar;

#[cfg(not(feature = "progress"))]
impl NoBar {
    fn set_position(&self, _: u64) {}
    fn finish_and_clear(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_drops_query_fragment_and_userinfo() {
        assert_eq!(
            sanitize("https://user:pw@bucket.s3.amazonaws.com/p/k.bsp?X-Amz-Signature=abc#frag"),
            "https://bucket.s3.amazonaws.com/p/k.bsp"
        );
        assert_eq!(
            sanitize("http://127.0.0.1:8080/a?b=c"),
            "http://127.0.0.1:8080/a"
        );
    }

    #[test]
    fn redirect_hops_are_revalidated() {
        let https = Url::parse("https://archive.test/a").unwrap();
        assert!(validate_hop(&https, Url::parse("https://cdn.test/b").unwrap()).is_ok());
        assert!(validate_hop(&https, Url::parse("http://cdn.test/b").unwrap()).is_err());
        assert!(validate_hop(&https, Url::parse("https://u:p@cdn.test/b").unwrap()).is_err());
        assert!(validate_hop(&https, Url::parse("ftp://cdn.test/b").unwrap()).is_err());
        let http = Url::parse("http://archive.test/a").unwrap();
        assert!(validate_hop(&http, Url::parse("http://cdn.test/b").unwrap()).is_ok());
        assert!(validate_hop(&http, Url::parse("https://cdn.test/b").unwrap()).is_ok());
    }

    #[test]
    fn userinfo_and_non_http_schemes_are_rejected() {
        assert!(matches!(
            parse_source_url("https://user:pw@archive.test/a"),
            Err(DatastoreError::Config(_))
        ));
        assert!(matches!(
            parse_source_url("ftp://archive.test/a"),
            Err(DatastoreError::Config(_))
        ));
        assert!(parse_source_url("https://archive.test/a").is_ok());
    }
}
