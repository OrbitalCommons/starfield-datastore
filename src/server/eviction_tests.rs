use super::*;
#[allow(dead_code)]
#[path = "../../tests/support/stub.rs"]
mod stub;
use std::{
    collections::HashMap,
    sync::{mpsc, Mutex},
    thread,
};

#[derive(Default)]
struct Mirror {
    objects: Mutex<HashMap<ArtifactKey, MirrorObject>>,
    slow: Option<(mpsc::Sender<std::path::PathBuf>, Mutex<mpsc::Receiver<()>>)>,
}
impl ObjectStore for Arc<Mirror> {
    fn head(&self, key: &ArtifactKey) -> Result<Option<MirrorObject>> {
        Ok(self.objects.lock().unwrap().get(key).cloned())
    }
    fn put(&self, key: &ArtifactKey, path: &FilePath, meta: &MirrorObject) -> Result<PutOutcome> {
        if key.as_str() == "slow" {
            if let Some((entered, release)) = &self.slow {
                entered.send(path.to_owned()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            }
        }
        // Read AFTER the wait: GC must preserve the path for the entire upload.
        assert_eq!(std::fs::read(path)?.len() as u64, meta.bytes);
        if key.as_str() == "fail" {
            return Err(DatastoreError::Mirror("injected upload failure".into()));
        }
        self.objects
            .lock()
            .unwrap()
            .insert(key.clone(), meta.clone());
        Ok(PutOutcome::Stored)
    }
    fn presign(&self, key: &ArtifactKey) -> Result<url::Url> {
        Ok(url::Url::parse(&format!(
            "https://bucket.test/{}?signature=test",
            key.as_str()
        ))
        .unwrap())
    }
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

struct Server {
    // Stop the listener before dropping the runtime.
    task: tokio::task::JoinHandle<()>,
    _runtime: tokio::runtime::Runtime,
    store: Arc<Datastore>,
    base: String,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn start(
    root: &FilePath,
    upstream: &stub::Stub,
    mirror: Arc<Mirror>,
    keys: &[&str],
    cap: u64,
) -> Server {
    let store = Arc::new(
        Datastore::builder()
            .cache_root(root.to_owned())
            .allow_upstream(true)
            .progress(false)
            .build()
            .unwrap(),
    );
    let policy = ServiceCachePolicy {
        evict_on_fetch: true,
        max_bytes: 0,
        max_concurrent_fills: 2,
        max_artifact_bytes: cap,
        max_inflight_bytes: cap * 2,
        min_free_bytes: 0,
    };
    let cache = cache::Coordinator::new(store.clone(), policy).unwrap();
    let artifacts = keys
        .iter()
        .map(|key| {
            Artifact::new(
                ArtifactKey::new(*key).unwrap(),
                vec![crate::Source::new(format!("{}/{}", upstream.url(), key))],
            )
            .with_check(ContentCheck::None)
            .with_provenance(crate::Provenance {
                license: "public-domain".into(),
                ..Default::default()
            })
        })
        .collect();
    let service = Arc::new(Service {
        manifest: Manifest { artifacts },
        store: store.clone(),
        mirror: Box::new(mirror),
        work: Arc::new(tokio::sync::Semaphore::new(1)),
        fills: Arc::new(tokio::sync::Semaphore::new(2)),
        cache: Some(cache),
    });
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = runtime.spawn(async move { axum::serve(listener, router(service)).await.unwrap() });
    Server {
        task,
        _runtime: runtime,
        store,
        base,
    }
}

#[test]
fn cleanup_failure_closes_cold_fills_but_preserves_s3_redirects() {
    let root = tempfile::tempdir().unwrap();
    let upstream = stub::Stub::start("127.0.0.1");
    upstream.route("/first", stub::Response::ok(b"payload"));
    let server = start(
        root.path(),
        &upstream,
        Arc::new(Mirror::default()),
        &["first", "later"],
        100,
    );
    std::fs::write(root.path().join("index/broken.json"), b"not json").unwrap();
    let client = client();
    for _ in 0..2 {
        assert_eq!(
            client
                .get(format!("{}/artifact/first", server.base))
                .send()
                .unwrap()
                .status(),
            StatusCode::FOUND
        );
    }
    assert_eq!(
        client
            .get(format!("{}/artifact/later", server.base))
            .send()
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(upstream.requests().len(), 1);
}

#[test]
fn sustained_demand_fill_and_redirect_after_eviction_never_refetch() {
    let root = tempfile::tempdir().unwrap();
    let upstream = stub::Stub::start("127.0.0.1");
    let keys: Vec<String> = (0..12).map(|n| format!("artifact-{n}")).collect();
    for (n, key) in keys.iter().enumerate() {
        upstream.route(&format!("/{key}"), stub::Response::ok(vec![n as u8; 80]));
    }
    let mirror = Arc::new(Mirror::default());
    let server = start(
        root.path(),
        &upstream,
        mirror.clone(),
        &keys.iter().map(String::as_str).collect::<Vec<_>>(),
        100,
    );
    let client = client();
    for key in &keys {
        let response = client
            .get(format!("{}/artifact/{key}", server.base))
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(response.headers()["x-artifact-bytes"], "80");
        assert_eq!(server.store.blob_bytes().unwrap(), 0);
        assert!(mirror
            .objects
            .lock()
            .unwrap()
            .contains_key(&ArtifactKey::new(key).unwrap()));
    }
    assert_eq!(upstream.requests().len(), keys.len());
    for key in &keys {
        assert_eq!(
            client
                .get(format!("{}/artifact/{key}", server.base))
                .send()
                .unwrap()
                .status(),
            StatusCode::FOUND
        );
    }
    assert_eq!(
        upstream.requests().len(),
        keys.len(),
        "S3 hits must never refetch"
    );
    assert_eq!(server.store.blob_bytes().unwrap(), 0);
}

#[test]
fn another_upload_and_disconnected_request_remain_protected_while_hits_bypass_gc() {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let upstream = stub::Stub::start("127.0.0.1");
    for key in ["slow", "fast", "later"] {
        upstream.route(&format!("/{key}"), stub::Response::ok(key.as_bytes()));
    }
    let (entered, path) = mpsc::channel();
    let (release, waiting) = mpsc::channel();
    let mirror = Arc::new(Mirror {
        slow: Some((entered, Mutex::new(waiting))),
        ..Default::default()
    });
    let server = start(
        root.path(),
        &upstream,
        mirror,
        &["slow", "fast", "later"],
        100,
    );
    let mut socket =
        std::net::TcpStream::connect(server.base.trim_start_matches("http://")).unwrap();
    socket
        .write_all(b"GET /artifact/slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let upload_path = path.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(socket); // The blocking upload must keep its guard despite disconnect.
    let client = client();
    assert_eq!(
        client
            .get(format!("{}/artifact/fast", server.base))
            .send()
            .unwrap()
            .status(),
        StatusCode::FOUND
    );
    assert!(upload_path.exists());
    assert_eq!(
        client
            .get(format!("{}/artifact/fast", server.base))
            .send()
            .unwrap()
            .status(),
        StatusCode::FOUND
    );
    let response = client
        .get(format!("{}/artifact/later", server.base))
        .send()
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "1");
    release.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while upload_path.exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !upload_path.exists(),
        "GC must run after the disconnected upload finishes"
    );
    assert_eq!(
        client
            .get(format!("{}/artifact/slow", server.base))
            .send()
            .unwrap()
            .status(),
        StatusCode::FOUND
    );
    assert_eq!(upstream.requests().len(), 2);
}

#[test]
fn upload_failure_and_oversize_transfer_leave_no_retained_payload() {
    let root = tempfile::tempdir().unwrap();
    let upstream = stub::Stub::start("127.0.0.1");
    upstream.route("/fail", stub::Response::ok(b"valid but upload fails"));
    upstream.route("/large", stub::Response::ok(vec![0u8; 1000]));
    let server = start(
        root.path(),
        &upstream,
        Arc::new(Mirror::default()),
        &["fail", "large"],
        100,
    );
    for key in ["fail", "large"] {
        let response = client()
            .get(format!("{}/artifact/{key}", server.base))
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        if key == "large" {
            assert!(response
                .text()
                .unwrap()
                .contains("reserved fill byte limit"));
        }
        assert_eq!(server.store.blob_bytes().unwrap(), 0);
        assert_eq!(
            std::fs::read_dir(root.path().join("tmp")).unwrap().count(),
            0
        );
    }
}
