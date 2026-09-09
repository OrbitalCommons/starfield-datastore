//! Configuration precedence (spec §11): builder call, then environment, then
//! the config file, then defaults. One test, because the environment is
//! process-global.

mod support;

use starfield_datastore::{Artifact, ArtifactKey, DatastoreBuilder, DatastoreError, Source};
use support::stub::{Response, Stub};
use tempfile::TempDir;

#[test]
fn builder_beats_env_beats_file_beats_default() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("datastore.toml");
    std::fs::write(
        &file,
        format!(
            "cache_dir = \"{}\"\nallow_upstream = true\noffline = true\ncache_max = 10\n",
            dir.path().join("from-file").display()
        ),
    )
    .unwrap();
    std::env::set_var("STARFIELD_DATASTORE_CONFIG", &file);
    std::env::set_var("STARFIELD_OFFLINE", "0");
    for name in [
        "STARFIELD_MIRROR",
        "STARFIELD_CACHE_DIR",
        "STARFIELD_ALLOW_UPSTREAM",
        "STARFIELD_CACHE_MAX",
    ] {
        std::env::remove_var(name);
    }

    let store = DatastoreBuilder::from_env()
        .unwrap()
        .cache_root(dir.path().join("explicit"))
        .progress(false)
        .build()
        .unwrap();
    assert_eq!(
        store.cache_root(),
        dir.path().join("explicit"),
        "builder beats file"
    );
    assert_eq!(
        store.max_bytes(),
        Some(10),
        "file supplies what nothing else set"
    );

    let store = DatastoreBuilder::from_env()
        .unwrap()
        .progress(false)
        .build()
        .unwrap();
    assert_eq!(
        store.cache_root(),
        dir.path().join("from-file"),
        "file beats the default"
    );
    let artifact = Artifact::new(ArtifactKey::new("x").unwrap(), vec![]);
    let err = store.get(&artifact).unwrap_err();
    assert!(
        matches!(err, DatastoreError::ManualRequired { .. }),
        "env offline=0 beats file offline=true, and the file's allow_upstream holds: {err}"
    );

    let store = DatastoreBuilder::from_env()
        .unwrap()
        .offline(true)
        .progress(false)
        .build()
        .unwrap();
    assert!(
        matches!(
            store.get(&artifact),
            Err(DatastoreError::OfflineMiss { .. })
        ),
        "builder beats env"
    );

    let archive = Stub::start("127.0.0.1");
    archive.route("/k", Response::ok(vec![9u8; 2048]));
    std::env::set_var("STARFIELD_TOKEN_127_0_0_1", "env-token");
    std::env::set_var("HOME", dir.path());
    let store = DatastoreBuilder::from_env()
        .unwrap()
        .cache_root(dir.path().join("creds"))
        .progress(false)
        .build()
        .unwrap();
    let fetched = Artifact::new(
        ArtifactKey::new("k").unwrap(),
        vec![Source::new(format!("{}/k", archive.url()))],
    );
    store.get(&fetched).unwrap();
    let sent = archive.requests()[0].headers.get("authorization").cloned();
    assert_eq!(
        sent.as_deref(),
        Some("Bearer env-token"),
        "from_env wires the caller's own credentials for the upstream layer"
    );
    assert_eq!(
        store
            .entry(&fetched.key)
            .unwrap()
            .provider_identity
            .as_deref(),
        Some("env:127.0.0.1")
    );
    std::env::remove_var("STARFIELD_TOKEN_127_0_0_1");

    let hermetic = starfield_datastore::Datastore::builder()
        .cache_root(dir.path().join("hermetic"))
        .allow_upstream(true)
        .progress(false)
        .build()
        .unwrap();
    std::env::set_var("STARFIELD_TOKEN_127_0_0_1", "env-token");
    hermetic.get(&fetched).unwrap();
    assert!(
        !archive.requests()[1].headers.contains_key("authorization"),
        "builder() alone reads nothing from the environment"
    );
    std::env::remove_var("STARFIELD_TOKEN_127_0_0_1");

    std::env::set_var("STARFIELD_OFFLINE", "sometimes");
    assert!(DatastoreBuilder::from_env().is_err());
    std::env::set_var("STARFIELD_OFFLINE", "0");
    std::env::set_var("STARFIELD_CACHE_MAX", "lots");
    assert!(DatastoreBuilder::from_env().is_err());

    for name in [
        "STARFIELD_OFFLINE",
        "STARFIELD_CACHE_MAX",
        "STARFIELD_DATASTORE_CONFIG",
    ] {
        std::env::remove_var(name);
    }
}
