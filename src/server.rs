//! Tailnet ephemeris service. Upstream work runs on blocking threads, while
//! artifact bytes flow directly from S3 to clients through presigned redirects.
use crate::{
    Artifact, ArtifactKey, ContentCheck, Datastore, DatastoreError, Manifest, MirrorObject,
    PutOutcome, Result, S3Mirror,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use std::{
    net::{IpAddr, SocketAddr},
    path::Path as FilePath,
    sync::Arc,
    time::Duration,
};

trait ObjectStore: Send + Sync {
    fn head(&self, key: &ArtifactKey) -> Result<Option<MirrorObject>>;
    fn put(&self, key: &ArtifactKey, path: &FilePath, meta: &MirrorObject) -> Result<PutOutcome>;
    fn presign(&self, key: &ArtifactKey) -> Result<url::Url>;
}
impl ObjectStore for S3Mirror {
    fn head(&self, key: &ArtifactKey) -> Result<Option<MirrorObject>> {
        self.head(key)
    }
    fn put(&self, key: &ArtifactKey, path: &FilePath, meta: &MirrorObject) -> Result<PutOutcome> {
        self.put(key, path, meta)
    }
    fn presign(&self, key: &ArtifactKey) -> Result<url::Url> {
        self.presign_get(key, Duration::from_secs(300))
    }
}

struct Service {
    manifest: Manifest,
    store: Datastore,
    mirror: Box<dyn ObjectStore>,
    work: Arc<tokio::sync::Semaphore>,
}

impl Service {
    fn resolve(&self, artifact: &Artifact) -> Result<(url::Url, MirrorObject)> {
        if artifact.provenance.license.trim().is_empty() {
            return Err(DatastoreError::Manifest(format!(
                "{} needs a license before redistribution",
                artifact.key
            )));
        }
        let meta = match self.mirror.head(&artifact.key)? {
            Some(meta) => meta,
            None => {
                // Upload explicitly even if get() serves a local hit. Client
                // Datastore::get never writes to the mirror.
                let path = self.store.get(artifact)?;
                let entry = self.store.entry(&artifact.key).ok_or_else(|| {
                    DatastoreError::Mirror("local entry disappeared before upload".into())
                })?;
                let meta = MirrorObject {
                    sha256: entry.digest,
                    bytes: entry.bytes,
                    etag: None,
                    source: entry.source,
                    provider_identity: entry.provider_identity,
                    fetched_at: entry.fetched_at,
                };
                self.mirror.put(&artifact.key, &path, &meta)?;
                meta
            }
        };
        check_metadata(artifact, &meta)?;
        Ok((self.mirror.presign(&artifact.key)?, meta))
    }
}

fn check_metadata(artifact: &Artifact, meta: &MirrorObject) -> Result<()> {
    fn check_pins(check: &ContentCheck, meta: &MirrorObject) -> bool {
        match check {
            ContentCheck::Sha256(pin) => *pin == meta.sha256,
            ContentCheck::All(checks) => checks.iter().all(|c| check_pins(c, meta)),
            ContentCheck::MinBytes(n) => meta.bytes >= *n,
            _ => true,
        }
    }
    if artifact.expected_bytes.is_some_and(|n| n != meta.bytes)
        || !check_pins(&artifact.check, meta)
    {
        return Err(DatastoreError::ContentRejected { key: artifact.key.clone(), failure: crate::CheckFailure { check: "mirror metadata pin".into(), got: "S3 digest or size disagrees with manifest; run verify --at with explicit repair".into() } });
    }
    Ok(())
}

fn router(service: Arc<Service>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/artifact/{*key}", get(artifact))
        .with_state(service)
}

async fn artifact(State(service): State<Arc<Service>>, Path(key): Path<String>) -> Response {
    let key = match ArtifactKey::new(key) {
        Ok(key) => key,
        Err(_) => return (StatusCode::NOT_FOUND, "unknown artifact").into_response(),
    };
    let Some(artifact) = service.manifest.get(&key).cloned() else {
        return (StatusCode::NOT_FOUND, "unknown artifact").into_response();
    };
    let permit = match service.work.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        service.resolve(&artifact)
    })
    .await;
    match result {
        Ok(Ok((url, meta))) => {
            let Ok(location) = HeaderValue::from_str(url.as_str()) else {
                return StatusCode::BAD_GATEWAY.into_response();
            };
            let mut response = StatusCode::FOUND.into_response();
            response.headers_mut().insert(header::LOCATION, location);
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            if let Ok(value) = HeaderValue::from_str(&meta.sha256) {
                response.headers_mut().insert("x-artifact-sha256", value);
            }
            if let Ok(value) = HeaderValue::from_str(&meta.bytes.to_string()) {
                response.headers_mut().insert("x-artifact-bytes", value);
            }
            response
        }
        Ok(Err(error)) => {
            let status = if matches!(error, DatastoreError::ContentRejected { .. }) {
                StatusCode::UNPROCESSABLE_ENTITY
            } else {
                StatusCode::BAD_GATEWAY
            };
            (status, error.to_string()).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "artifact worker failed").into_response(),
    }
}

/// Run the service on loopback or a Tailscale address. This synchronous entry
/// point owns the HTTP runtime; the provided store must permit upstream fills.
pub fn serve(
    manifest: Manifest,
    store: Datastore,
    mirror: S3Mirror,
    bind: SocketAddr,
) -> Result<()> {
    if !private_bind(bind.ip()) {
        return Err(DatastoreError::Config(
            "serve must bind loopback or a Tailscale address".into(),
        ));
    }
    let service = Arc::new(Service {
        manifest,
        store,
        mirror: Box::new(mirror),
        work: Arc::new(tokio::sync::Semaphore::new(8)),
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        #[cfg(unix)]
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        axum::serve(listener, router(service))
            .with_graceful_shutdown(async move {
                #[cfg(unix)]
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = terminate.recv() => {},
                }
                #[cfg(not(unix))]
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok(())
    })
}

fn private_bind(ip: IpAddr) -> bool {
    if ip.is_loopback() {
        return true;
    }
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            octets[0] == 100 && (64..128).contains(&octets[1])
        }
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

    struct Fake {
        object: Mutex<Option<MirrorObject>>,
        writes: Arc<AtomicUsize>,
    }
    impl ObjectStore for Fake {
        fn head(&self, _: &ArtifactKey) -> Result<Option<MirrorObject>> {
            Ok(self.object.lock().unwrap().clone())
        }
        fn put(&self, _: &ArtifactKey, _: &FilePath, meta: &MirrorObject) -> Result<PutOutcome> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            *self.object.lock().unwrap() = Some(meta.clone());
            Ok(PutOutcome::Stored)
        }
        fn presign(&self, _: &ArtifactKey) -> Result<url::Url> {
            Ok(url::Url::parse("https://bucket.test/data?signature=temporary").unwrap())
        }
    }
    #[test]
    fn miss_uploads_local_hit_then_redirects_and_unknown_keys_never_fetch() {
        let root = tempfile::tempdir().unwrap();
        let store = Datastore::builder()
            .cache_root(root.path().to_owned())
            .offline(true)
            .build()
            .unwrap();
        let artifact = Artifact::new(ArtifactKey::new("archive/data").unwrap(), vec![])
            .with_check(ContentCheck::None)
            .with_provenance(crate::Provenance {
                license: "public-domain".into(),
                ..Default::default()
            });
        let file = root.path().join("input");
        std::fs::write(&file, b"data").unwrap();
        store.import(&artifact, &file).unwrap();
        let writes = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(Service {
            manifest: Manifest {
                artifacts: vec![artifact],
            },
            store,
            mirror: Box::new(Fake {
                object: Mutex::new(None),
                writes: writes.clone(),
            }),
            work: Arc::new(tokio::sync::Semaphore::new(2)),
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let server =
            runtime.spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        for _ in 0..2 {
            let response = client
                .get(format!("http://{addr}/artifact/archive/data"))
                .send()
                .unwrap();
            assert_eq!(response.status().as_u16(), 302);
            assert_eq!(response.headers()["x-artifact-bytes"], "4");
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert_eq!(
                response.headers()["location"],
                "https://bucket.test/data?signature=temporary"
            );
        }
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert_eq!(
            client
                .get(format!("http://{addr}/artifact/missing"))
                .send()
                .unwrap()
                .status()
                .as_u16(),
            404
        );
        assert_eq!(
            client
                .get(format!("http://{addr}/healthz"))
                .send()
                .unwrap()
                .status()
                .as_u16(),
            200
        );
        server.abort();
    }
    #[test]
    fn public_bind_and_mismatched_pins_are_rejected() {
        assert!(!private_bind("0.0.0.0".parse().unwrap()));
        assert!(!private_bind("8.8.8.8".parse().unwrap()));
        assert!(private_bind("100.101.102.103".parse().unwrap()));
        let artifact = Artifact::new(ArtifactKey::new("data").unwrap(), vec![]).with_check(
            ContentCheck::All(vec![
                ContentCheck::Sha256("a".repeat(64)),
                ContentCheck::Sha256("b".repeat(64)),
            ]),
        );
        let meta = MirrorObject {
            sha256: "a".repeat(64),
            bytes: 4,
            etag: None,
            source: None,
            provider_identity: None,
            fetched_at: 1,
        };
        assert!(check_metadata(&artifact, &meta).is_err());
    }
}
