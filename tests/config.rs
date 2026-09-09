//! Configuration precedence (spec §11): builder call, then environment, then
//! the config file, then defaults. One test, because the environment is
//! process-global.

use starfield_datastore::{Artifact, ArtifactKey, DatastoreBuilder, DatastoreError};
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
