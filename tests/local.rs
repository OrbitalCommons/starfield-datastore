//! The local layer on its own: import, removal, accounting, verify and gc.

use starfield_datastore::{
    Artifact, ArtifactKey, ContentCheck, Datastore, DatastoreBuilder, Layer,
};
use tempfile::TempDir;

fn builder(root: &TempDir) -> DatastoreBuilder {
    Datastore::builder()
        .cache_root(root.path().to_path_buf())
        .progress(false)
}

fn artifact(name: &str) -> Artifact {
    Artifact::new(ArtifactKey::new(format!("local/{name}")).unwrap(), vec![])
        .with_check(ContentCheck::NotHtml)
}

fn seed(dir: &TempDir, name: &str, content: &[u8]) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, content).unwrap();
    path
}

#[test]
fn import_validates_copies_and_dedupes() {
    let root = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let store = builder(&root).build().unwrap();

    let source = seed(&files, "a.bin", &[7u8; 512]);
    let a = artifact("a");
    let path = store.import(&a, &source).unwrap();
    assert!(source.exists(), "import copies");
    assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; 512]);
    let entry = store.entry(&a.key).unwrap();
    assert_eq!(entry.layer, Layer::LocalDisk);
    assert_eq!(entry.source.as_deref(), Some("local:import"));
    assert_eq!(store.get(&a).unwrap(), path);

    let b = artifact("b");
    assert_eq!(
        store.import(&b, &source).unwrap(),
        path,
        "same bytes, same blob"
    );
    assert_eq!(store.total_bytes().unwrap(), 512, "shared blobs count once");
    assert_eq!(store.keys().unwrap(), vec![a.key.clone(), b.key.clone()]);

    let html = seed(&files, "login.html", b"<html><body>sign in</body></html>");
    assert!(store.import(&artifact("html"), &html).is_err());
    assert_eq!(store.keys().unwrap().len(), 2);

    store.remove(&a.key).unwrap();
    assert!(path.exists(), "blob still referenced by b");
    store.remove(&b.key).unwrap();
    assert!(!path.exists());
    assert!(store.keys().unwrap().is_empty());
    store.remove(&b.key).unwrap();
}

#[test]
fn gc_evicts_oldest_first_until_within_budget() {
    let root = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let store = builder(&root).build().unwrap();
    for (i, name) in ["one", "two", "three"].iter().enumerate() {
        let path = seed(&files, name, &vec![i as u8 + 1; 1000]);
        store.import(&artifact(name), &path).unwrap();
        let index = root.path().join("index/local").join(format!("{name}.json"));
        let text = std::fs::read_to_string(&index).unwrap();
        let aged = text.replace(
            &format!("\"fetched_at\": {}", entry_fetched_at(&text)),
            &format!("\"fetched_at\": {}", 1_000 + i as u64),
        );
        std::fs::write(&index, aged).unwrap();
    }
    assert_eq!(store.total_bytes().unwrap(), 3000);
    let removed = store.gc(1500).unwrap();
    assert_eq!(
        removed,
        vec![
            ArtifactKey::new("local/one").unwrap(),
            ArtifactKey::new("local/two").unwrap()
        ]
    );
    assert_eq!(store.total_bytes().unwrap(), 1000);
    assert!(store.gc(1500).unwrap().is_empty());
    assert!(store.verify().unwrap().is_empty());
}

fn entry_fetched_at(text: &str) -> u64 {
    text.split("\"fetched_at\": ")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap()
}

#[test]
fn cache_root_is_shared_and_layout_matches_the_spec() {
    let root = TempDir::new().unwrap();
    let store = builder(&root).build().unwrap();
    assert_eq!(store.cache_root(), root.path());
    for dir in ["blobs", "index", "tmp", "locks"] {
        assert!(root.path().join(dir).is_dir(), "{dir}");
    }
    assert!(store.peek(&ArtifactKey::new("nothing").unwrap()).is_none());
    assert!(!store.contains(&ArtifactKey::new("nothing").unwrap()));
    assert_eq!(store.total_bytes().unwrap(), 0);
}

#[test]
fn gc_reclaims_orphan_blobs_but_never_touches_temp_files() {
    let root = TempDir::new().unwrap();
    let files = TempDir::new().unwrap();
    let store = builder(&root).build().unwrap();
    let kept = artifact("kept");
    store
        .import(&kept, &seed(&files, "kept", &[1u8; 1000]))
        .unwrap();

    let orphan = root
        .path()
        .join("blobs/ab")
        .join(format!("ab{}", "c".repeat(62)));
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, [2u8; 500]).unwrap();
    let old = root.path().join("tmp/.tmpslow");
    std::fs::write(&old, [3u8; 100]).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 24 * 3600))
        .unwrap();
    assert_eq!(
        store.total_bytes().unwrap(),
        1500,
        "orphans count against the budget"
    );

    assert!(
        store.gc(u64::MAX).unwrap().is_empty(),
        "within budget: no key evicted"
    );
    assert!(!orphan.exists(), "orphan reclaimed");
    assert!(
        old.exists(),
        "age proves nothing about a transfer; temp files are never reclaimed"
    );
    assert_eq!(store.total_bytes().unwrap(), 1000);
    assert!(store.contains(&kept.key));
}
