//! The resolution chain against local HTTP stubs: upstream fill, mirror
//! redirects, and the credential rules of spec §6. Nothing here touches the
//! network beyond loopback.

mod support;

use sha2::{Digest, Sha256};
#[cfg(feature = "mirror-http")]
use starfield_datastore::Mirror;
use starfield_datastore::{
    Artifact, ArtifactKey, ContentCheck, Credential, Datastore, DatastoreBuilder, DatastoreError,
    Layer, Provenance, Secret, Source, StaticProvider,
};
use std::collections::HashMap;
use std::time::{Duration, SystemTime};
use support::stub::{Response, Stub};
use tempfile::TempDir;

fn body(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn key(name: &str) -> ArtifactKey {
    ArtifactKey::new(format!("test/{name}")).unwrap()
}

fn builder(root: &TempDir) -> DatastoreBuilder {
    Datastore::builder()
        .cache_root(root.path().to_path_buf())
        .progress(false)
        .timeout(Duration::from_secs(5))
}

fn basic(user: &str, secret: &str) -> Credential {
    Credential::Basic {
        user: user.into(),
        secret: Secret::new(secret.into()),
    }
}

fn provider(entries: &[(&str, Credential)]) -> Box<StaticProvider> {
    Box::new(StaticProvider(
        entries
            .iter()
            .map(|(h, c)| (h.to_string(), c.clone()))
            .collect::<HashMap<_, _>>(),
    ))
}

fn redirect(to: &str) -> Response {
    Response {
        status: 302,
        headers: vec![("Location".into(), to.into())],
        body: vec![],
    }
}

fn authorization(stub: &Stub, path: &str) -> Vec<Option<String>> {
    stub.requests()
        .iter()
        .filter(|r| r.path == path)
        .map(|r| r.headers.get("authorization").cloned())
        .collect()
}

#[test]
fn upstream_fill_is_validated_local_only_and_byte_identical_on_hit() {
    let upstream = Stub::start("127.0.0.1");
    let bytes = body(1, 4096);
    upstream.route("/de440.bsp", Response::ok(bytes.clone()));
    let root = TempDir::new().unwrap();
    let store = builder(&root).allow_upstream(true).build().unwrap();
    let artifact = Artifact::new(
        key("de440.bsp"),
        vec![Source::new(format!("{}/de440.bsp", upstream.url()))],
    )
    .with_expected_bytes(4096);

    let (path, outcome) = store.get_with_outcome(&artifact).unwrap();
    assert_eq!(outcome.layer, Layer::Upstream);
    assert_eq!(outcome.source_index, Some(0));
    assert_eq!(outcome.bytes, 4096);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    let entry = store.entry(&artifact.key).unwrap();
    assert_eq!(entry.digest, hex(&bytes));
    assert_eq!(path.file_name().unwrap().to_str().unwrap(), entry.digest);
    assert_eq!(entry.layer, Layer::Upstream);
    assert_eq!(
        entry.source.as_deref(),
        Some(format!("{}/de440.bsp", upstream.url()).as_str())
    );
    assert!(entry.provider_identity.is_none());

    let (again, outcome) = store.get_with_outcome(&artifact).unwrap();
    assert_eq!(outcome.layer, Layer::LocalDisk);
    assert_eq!(again, path);
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "a hit never goes upstream");
    assert_eq!(requests[0].method, "GET");
    assert!(store.peek(&artifact.key).is_some());
    assert!(std::fs::read_dir(root.path().join("tmp"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn html_login_page_is_rejected_and_nothing_is_cached() {
    let upstream = Stub::start("127.0.0.1");
    let page = format!(
        "\n<!DOCTYPE html><html><body><form>{}</form></body></html>",
        "x".repeat(20_000)
    );
    upstream.route("/mosaic.tif", Response::ok(page.into_bytes()));
    let root = TempDir::new().unwrap();
    let store = builder(&root).allow_upstream(true).build().unwrap();
    let artifact = Artifact::new(
        key("mosaic.tif"),
        vec![Source::new(format!("{}/mosaic.tif", upstream.url()))],
    );

    let err = store.get(&artifact).unwrap_err();
    let DatastoreError::ContentRejected { failure, .. } = err else {
        panic!("expected ContentRejected, got {err}");
    };
    assert_eq!(failure.check, "NotHtml");
    assert!(!store.contains(&artifact.key));
    assert!(store.keys().unwrap().is_empty());
    assert!(std::fs::read_dir(root.path().join("blobs"))
        .unwrap()
        .next()
        .is_none());
    assert!(std::fs::read_dir(root.path().join("tmp"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn magic_and_pins_reject_the_wrong_thing() {
    let upstream = Stub::start("127.0.0.1");
    let bytes = body(2, 2048);
    upstream.route("/k", Response::ok(bytes.clone()));
    let root = TempDir::new().unwrap();
    let store = builder(&root).allow_upstream(true).build().unwrap();
    let source = Source::new(format!("{}/k", upstream.url()));

    let wrong_magic = Artifact::new(key("magic"), vec![source.clone()])
        .with_check(ContentCheck::magic(vec![b"DAF/SPK".to_vec()], false));
    assert!(matches!(
        store.get(&wrong_magic),
        Err(DatastoreError::ContentRejected { .. })
    ));

    let wrong_pin =
        Artifact::new(key("pin"), vec![source.clone()]).with_check(ContentCheck::All(vec![
            ContentCheck::Sha256("0".repeat(64)),
            ContentCheck::default_binary(),
        ]));
    let err = store.get(&wrong_pin).unwrap_err();
    assert!(
        matches!(&err, DatastoreError::ContentRejected { failure, .. } if failure.check == "Sha256"),
        "{err}"
    );

    let wrong_size = Artifact::new(key("size"), vec![source.clone()]).with_expected_bytes(1);
    let err = store.get(&wrong_size).unwrap_err();
    assert!(
        matches!(&err, DatastoreError::ContentRejected { failure, .. } if failure.check == "expected_bytes"),
        "{err}"
    );

    let right = Artifact::new(key("right"), vec![source])
        .with_check(ContentCheck::All(vec![
            ContentCheck::Sha256(hex(&bytes)),
            ContentCheck::default_binary(),
        ]))
        .with_expected_bytes(2048);
    assert!(store.get(&right).is_ok());
    assert_eq!(store.keys().unwrap(), vec![key("right")]);
}

#[cfg(feature = "mirror-http")]
#[test]
fn default_mode_is_loud_when_there_is_no_mirror_or_it_is_down() {
    let upstream = Stub::start("127.0.0.1");
    upstream.route("/k", Response::ok(body(3, 2048)));
    let root = TempDir::new().unwrap();
    let artifact = Artifact::new(key("k"), vec![Source::new(format!("{}/k", upstream.url()))]);

    let store = builder(&root).build().unwrap();
    let err = store.get(&artifact).unwrap_err();
    assert!(matches!(err, DatastoreError::MirrorUnreachable { .. }));
    assert!(
        err.to_string().contains("STARFIELD_ALLOW_UPSTREAM=1"),
        "{err}"
    );
    assert!(err.to_string().contains("no mirror configured"), "{err}");

    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port())
    };
    let store = builder(&root)
        .mirror(Mirror::Http {
            base_url: closed.clone(),
            writable: false,
        })
        .build()
        .unwrap();
    let err = store.get(&artifact).unwrap_err();
    assert!(
        matches!(err, DatastoreError::MirrorUnreachable { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("HTTP request failed"), "{err}");
    assert_eq!(
        upstream.requests().len(),
        0,
        "upstream is never touched without permission"
    );

    let store = builder(&root)
        .mirror(Mirror::Http {
            base_url: closed,
            writable: false,
        })
        .allow_upstream(true)
        .build()
        .unwrap();
    let (_, outcome) = store.get_with_outcome(&artifact).unwrap();
    assert_eq!(outcome.layer, Layer::Upstream);
}

#[test]
fn offline_and_manual_artifacts_fail_with_their_own_errors() {
    let root = TempDir::new().unwrap();
    let store = builder(&root).offline(true).build().unwrap();
    let artifact = Artifact::new(key("k"), vec![Source::new("https://archive.test/k")]);
    assert!(matches!(
        store.get(&artifact),
        Err(DatastoreError::OfflineMiss { .. })
    ));

    let store = builder(&root).allow_upstream(true).build().unwrap();
    let manual = Artifact::new(key("ecostress"), vec![]).with_provenance(Provenance {
        description: "order the granule from the LP DAAC portal and import it".into(),
        license: "public-domain".into(),
        citation: None,
    });
    let err = store.get(&manual).unwrap_err();
    assert!(matches!(err, DatastoreError::ManualRequired { .. }));
    assert!(err.to_string().contains("LP DAAC portal"), "{err}");
}

#[cfg(feature = "mirror-http")]
#[test]
fn mirror_http_follows_cross_host_redirect_without_any_credentials() {
    let server = Stub::start("127.0.0.1");
    let s3 = Stub::start("localhost");
    let bytes = body(4, 3000);
    let presigned = format!("{}/bucket/prefix/test/k?X-Amz-Signature=deadbeef", s3.url());
    server.route("/artifact/test/k", redirect(&presigned));
    s3.route(
        "/bucket/prefix/test/k?X-Amz-Signature=deadbeef",
        Response::ok(bytes.clone()),
    );
    let root = TempDir::new().unwrap();
    let store = builder(&root)
        .mirror(Mirror::Http {
            base_url: server.url(),
            writable: false,
        })
        .credentials(provider(&[
            ("127.0.0.1", basic("u", "p")),
            ("localhost", basic("u", "p")),
        ]))
        .build()
        .unwrap();
    let artifact = Artifact::new(key("k"), vec![Source::new("https://archive.test/never")]);

    let (path, outcome) = store.get_with_outcome(&artifact).unwrap();
    assert_eq!(outcome.layer, Layer::Mirror);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_eq!(authorization(&server, "/artifact/test/k"), vec![None]);
    assert_eq!(
        authorization(&s3, "/bucket/prefix/test/k?X-Amz-Signature=deadbeef"),
        vec![None]
    );
    let entry = store.entry(&artifact.key).unwrap();
    assert_eq!(entry.layer, Layer::Mirror);
    assert_eq!(entry.digest, hex(&bytes));
    assert_eq!(
        entry.source.as_deref(),
        Some(format!("{}/artifact/test/k", server.url()).as_str())
    );
}

#[cfg(feature = "mirror-http")]
#[test]
fn mirror_miss_falls_through_to_upstream_only_when_allowed() {
    let server = Stub::start("127.0.0.1");
    let upstream = Stub::start("localhost");
    upstream.route("/k", Response::ok(body(5, 2048)));
    let root = TempDir::new().unwrap();
    let artifact = Artifact::new(key("k"), vec![Source::new(format!("{}/k", upstream.url()))]);

    let store = builder(&root)
        .mirror(Mirror::Http {
            base_url: server.url(),
            writable: false,
        })
        .build()
        .unwrap();
    let err = store.get(&artifact).unwrap_err();
    assert!(
        matches!(err, DatastoreError::MirrorUnreachable { .. }),
        "{err}"
    );
    assert!(
        err.to_string().contains("/artifact/test/k has no entry"),
        "{err}"
    );

    let store = builder(&root)
        .mirror(Mirror::Http {
            base_url: server.url(),
            writable: false,
        })
        .allow_upstream(true)
        .build()
        .unwrap();
    let (_, outcome) = store.get_with_outcome(&artifact).unwrap();
    assert_eq!(outcome.layer, Layer::Upstream);
    assert_eq!(server.requests().len(), 2);
}

#[test]
fn credentials_are_host_keyed_and_only_forwarded_when_trusted() {
    let archive = Stub::start("127.0.0.1");
    let cdn = Stub::start("localhost");
    let bytes = body(6, 2048);
    archive.route("/data", redirect(&format!("{}/file", cdn.url())));
    cdn.route("/file", Response::ok(bytes.clone()));
    let root = TempDir::new().unwrap();
    let store = builder(&root)
        .allow_upstream(true)
        .credentials(provider(&[("127.0.0.1", basic("person", "hunter2"))]))
        .build()
        .unwrap();
    let url = format!("{}/data", archive.url());

    let untrusted = Artifact::new(key("untrusted"), vec![Source::new(&url)]);
    store.get(&untrusted).unwrap();
    assert_eq!(authorization(&archive, "/data").len(), 1);
    assert!(authorization(&archive, "/data")[0]
        .as_deref()
        .unwrap()
        .starts_with("Basic "));
    assert_eq!(
        authorization(&cdn, "/file"),
        vec![None],
        "rule 2: dropped on redirect"
    );
    let entry = store.entry(&untrusted.key).unwrap();
    assert_eq!(entry.provider_identity.as_deref(), Some("static:127.0.0.1"));

    let trusted = Artifact::new(key("trusted"), vec![Source::new(&url).trusting_redirects()]);
    store.get(&trusted).unwrap();
    let forwarded = authorization(&cdn, "/file");
    assert_eq!(forwarded.len(), 2);
    assert!(
        forwarded[1].as_deref().unwrap().starts_with("Basic "),
        "trust_redirects forwards"
    );

    let store = builder(&root)
        .allow_upstream(true)
        .credentials(provider(&[("localhost", basic("other", "pw"))]))
        .build()
        .unwrap();
    let destination = Artifact::new(key("destination"), vec![Source::new(&url)]);
    store.get(&destination).unwrap();
    assert_eq!(authorization(&archive, "/data")[2], None);
    let own = authorization(&cdn, "/file");
    assert!(
        own[2].as_deref().unwrap().starts_with("Basic "),
        "rule 4: keyed on the host actually requested"
    );
    let entry = store.entry(&destination.key).unwrap();
    assert_eq!(entry.provider_identity.as_deref(), Some("static:localhost"));
}

#[test]
fn credentials_never_enter_the_cache() {
    let archive = Stub::start("127.0.0.1");
    archive.route("/k?token=sup3rs3cret", Response::ok(body(7, 2048)));
    let root = TempDir::new().unwrap();
    let store = builder(&root)
        .allow_upstream(true)
        .credentials(provider(&[("127.0.0.1", basic("person", "sup3rs3cret"))]))
        .build()
        .unwrap();
    let artifact = Artifact::new(
        key("k"),
        vec![Source::new(format!(
            "{}/k?token=sup3rs3cret",
            archive.url()
        ))],
    );
    store.get(&artifact).unwrap();

    let mut stack = vec![root.path().to_path_buf()];
    let mut inspected = 0;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                inspected += 1;
                let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).to_string();
                assert!(
                    !text.contains("sup3rs3cret"),
                    "secret found in {}",
                    path.display()
                );
                assert!(
                    !text.contains("token="),
                    "query string found in {}",
                    path.display()
                );
            }
        }
    }
    assert!(inspected >= 2, "blob and sidecar were inspected");
}

#[test]
fn missing_and_rejected_credentials_are_different_errors() {
    let archive = Stub::start("127.0.0.1");
    archive.route(
        "/k",
        Response {
            status: 401,
            headers: vec![],
            body: vec![],
        },
    );
    let root = TempDir::new().unwrap();
    let artifact = Artifact::new(key("k"), vec![Source::new(format!("{}/k", archive.url()))]);

    let store = builder(&root).allow_upstream(true).build().unwrap();
    let err = store.get(&artifact).unwrap_err();
    assert!(
        err.to_string()
            .contains("no credential configured for host 127.0.0.1"),
        "{err}"
    );

    let store = builder(&root)
        .allow_upstream(true)
        .credentials(provider(&[("127.0.0.1", basic("u", "p"))]))
        .build()
        .unwrap();
    let err = store.get(&artifact).unwrap_err();
    assert!(
        err.to_string()
            .contains("credential rejected by 127.0.0.1 (HTTP 401)"),
        "{err}"
    );
    assert!(!err.to_string().contains("expired"), "{err}");

    let expired = Credential::Bearer {
        secret: Secret::new("old".into()),
        expires_at: Some(SystemTime::now() - Duration::from_secs(60)),
    };
    let store = builder(&root)
        .allow_upstream(true)
        .credentials(provider(&[("127.0.0.1", expired)]))
        .build()
        .unwrap();
    let err = store.get(&artifact).unwrap_err();
    assert!(err.to_string().contains("it looks expired"), "{err}");
}

#[test]
fn every_source_is_tried_and_the_winner_is_reported() {
    let first = Stub::start("127.0.0.1");
    let second = Stub::start("localhost");
    second.route("/k", Response::ok(body(8, 2048)));
    let root = TempDir::new().unwrap();
    let store = builder(&root).allow_upstream(true).build().unwrap();
    let artifact = Artifact::new(
        key("k"),
        vec![
            Source::new(format!("{}/k", first.url())),
            Source::new(format!("{}/k", second.url())),
        ],
    );
    let (_, outcome) = store.get_with_outcome(&artifact).unwrap();
    assert_eq!(outcome.source_index, Some(1));

    let none = Artifact::new(
        key("none"),
        vec![Source::new(format!("{}/missing", first.url()))],
    );
    let err = store.get(&none).unwrap_err();
    let DatastoreError::AllSourcesFailed { attempts, .. } = &err else {
        panic!("{err}");
    };
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].contains("404"), "{err}");
}

#[test]
fn a_moved_pin_refetches_and_a_corrupt_blob_is_refused() {
    let archive = Stub::start("127.0.0.1");
    let bytes = body(9, 2048);
    archive.route("/k", Response::ok(bytes.clone()));
    let root = TempDir::new().unwrap();
    let store = builder(&root).allow_upstream(true).build().unwrap();
    let source = Source::new(format!("{}/k", archive.url()));

    let unpinned = Artifact::new(key("k"), vec![source.clone()]);
    let path = store.get(&unpinned).unwrap();
    let pinned = unpinned.clone().with_check(ContentCheck::All(vec![
        ContentCheck::Sha256(hex(&bytes)),
        ContentCheck::default_binary(),
    ]));
    let (_, outcome) = store.get_with_outcome(&pinned).unwrap();
    assert_eq!(outcome.layer, Layer::LocalDisk, "a matching pin is a hit");

    let other = body(10, 2048);
    archive.route("/k", Response::ok(other.clone()));
    let moved = unpinned.clone().with_check(ContentCheck::All(vec![
        ContentCheck::Sha256(hex(&other)),
        ContentCheck::default_binary(),
    ]));
    let (new_path, outcome) = store.get_with_outcome(&moved).unwrap();
    assert_eq!(outcome.layer, Layer::Upstream, "a moved pin is a miss");
    assert_ne!(new_path, path);

    let mut perms = std::fs::metadata(&new_path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&new_path, perms).unwrap();
    std::fs::write(&new_path, body(11, 2048)).unwrap();
    let err = store.get(&unpinned).unwrap_err();
    assert!(
        matches!(&err, DatastoreError::ContentRejected { failure, .. } if failure.check == "cached blob digest"),
        "{err}"
    );
    let failures = store.verify().unwrap();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].expected, hex(&other));
}

#[test]
fn nested_conflicting_pins_can_never_pass() {
    let archive = Stub::start("127.0.0.1");
    let bytes = body(12, 2048);
    archive.route("/k", Response::ok(bytes.clone()));
    let root = TempDir::new().unwrap();
    let store = builder(&root).allow_upstream(true).build().unwrap();
    let artifact = Artifact::new(key("k"), vec![Source::new(format!("{}/k", archive.url()))])
        .with_check(ContentCheck::All(vec![
            ContentCheck::Sha256(hex(&bytes)),
            ContentCheck::All(vec![
                ContentCheck::NotHtml,
                ContentCheck::Sha256("0".repeat(64)),
            ]),
        ]));
    let err = store.get(&artifact).unwrap_err();
    assert!(
        matches!(&err, DatastoreError::ContentRejected { failure, .. } if failure.check == "Sha256"),
        "the matching outer pin must not mask the conflicting inner one: {err}"
    );
    assert!(!store.contains(&artifact.key));
}

#[test]
fn concurrent_gets_for_one_key_make_one_upstream_request() {
    let archive = Stub::start("127.0.0.1");
    let bytes = body(13, 8192);
    archive.route("/k", Response::ok(bytes.clone()));
    let root = TempDir::new().unwrap();
    let store = std::sync::Arc::new(builder(&root).allow_upstream(true).build().unwrap());
    let artifact = std::sync::Arc::new(Artifact::new(
        key("k"),
        vec![Source::new(format!("{}/k", archive.url()))],
    ));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            let artifact = artifact.clone();
            std::thread::spawn(move || store.get_with_outcome(&artifact).unwrap())
        })
        .collect();
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let served_upstream = outcomes
        .iter()
        .filter(|(_, o)| o.layer == Layer::Upstream)
        .count();
    assert_eq!(
        served_upstream, 1,
        "the key lock lets exactly one fetch through"
    );
    assert_eq!(archive.requests().len(), 1);
    assert!(outcomes
        .iter()
        .all(|(p, _)| std::fs::read(p).unwrap() == bytes));
    assert!(std::fs::read_dir(root.path().join("tmp"))
        .unwrap()
        .next()
        .is_none());
}

#[test]
fn gc_racing_publication_never_leaves_dangling_keys() {
    let root = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let store = std::sync::Arc::new(builder(&root).build().unwrap());
    let mut sources = Vec::new();
    for i in 0..40u8 {
        let path = files.path().join(format!("f{i}"));
        std::fs::write(&path, body(i, 1500 + i as usize)).unwrap();
        sources.push(path);
    }
    let importer = {
        let store = store.clone();
        std::thread::spawn(move || {
            for (i, path) in sources.iter().enumerate() {
                let artifact = Artifact::new(key(&format!("race/{i}")), vec![]);
                store.import(&artifact, path).unwrap();
                let alias = Artifact::new(key(&format!("alias/{i}")), vec![]);
                store.import(&alias, path).unwrap();
            }
        })
    };
    let collector = {
        let store = store.clone();
        std::thread::spawn(move || {
            for _ in 0..60 {
                store.gc(4000).unwrap();
                std::thread::yield_now();
            }
        })
    };
    importer.join().unwrap();
    collector.join().unwrap();

    assert!(
        store.verify().unwrap().is_empty(),
        "every surviving key still has its blob"
    );
    for k in store.keys().unwrap() {
        assert!(store.peek(&k).is_some(), "{k} references a missing blob");
    }
    let referenced: std::collections::HashSet<String> = store
        .keys()
        .unwrap()
        .iter()
        .map(|k| store.entry(k).unwrap().digest)
        .collect();
    store.gc(u64::MAX).unwrap();
    let blobs: Vec<_> = std::fs::read_dir(root.path().join("blobs"))
        .unwrap()
        .flat_map(|d| std::fs::read_dir(d.unwrap().path()).unwrap())
        .map(|f| f.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        blobs.iter().all(|b| referenced.contains(b)),
        "no orphans after gc"
    );
}

fn with_cookie(mut response: Response) -> Response {
    response
        .headers
        .push(("Set-Cookie".into(), "session=abc; Path=/".into()));
    response
}

fn cookies_seen(stub: &Stub, path: &str) -> Vec<Option<String>> {
    stub.requests()
        .iter()
        .filter(|r| r.path == path)
        .map(|r| r.headers.get("cookie").cloned())
        .collect()
}

#[cfg(feature = "mirror-http")]
#[test]
fn mirror_engine_has_no_cookie_jar() {
    let server = Stub::start("127.0.0.1");
    let bytes = body(14, 2048);
    let presigned = format!("{}/presigned/test/k?sig=1", server.url());
    server.route("/artifact/test/k", with_cookie(redirect(&presigned)));
    server.route("/presigned/test/k?sig=1", Response::ok(bytes.clone()));
    let root = TempDir::new().unwrap();
    let store = builder(&root)
        .mirror(Mirror::Http {
            base_url: server.url(),
            writable: false,
        })
        .build()
        .unwrap();
    let artifact = Artifact::new(key("k"), vec![]);
    let (path, outcome) = store.get_with_outcome(&artifact).unwrap();
    assert_eq!(outcome.layer, Layer::Mirror);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_eq!(
        cookies_seen(&server, "/presigned/test/k?sig=1"),
        vec![None],
        "a cookie set by the mirror never rides along on its redirect"
    );
}

#[test]
fn upstream_engine_keeps_cookies_for_the_login_dance() {
    let archive = Stub::start("127.0.0.1");
    let bytes = body(15, 2048);
    archive.route(
        "/data",
        with_cookie(redirect(&format!("{}/file", archive.url()))),
    );
    archive.route("/file", Response::ok(bytes.clone()));
    let root = TempDir::new().unwrap();
    let store = builder(&root).allow_upstream(true).build().unwrap();
    let artifact = Artifact::new(
        key("k"),
        vec![Source::new(format!("{}/data", archive.url()))],
    );
    store.get(&artifact).unwrap();
    assert_eq!(
        cookies_seen(&archive, "/file"),
        vec![Some("session=abc".into())],
        "Earthdata authorises the final hop with a cookie set on an earlier one"
    );
}
