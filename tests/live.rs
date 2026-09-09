use starfield_datastore::{Artifact, ArtifactKey, ContentCheck, Datastore, Source};

#[test]
#[ignore = "live upstream NAIF; bypasses the mirror; requires STARFIELD_ALLOW_UPSTREAM=1"]
fn naif_upstream_canary() {
    assert_eq!(
        std::env::var("STARFIELD_ALLOW_UPSTREAM").as_deref(),
        Ok("1"),
        "live upstream canary requires STARFIELD_ALLOW_UPSTREAM=1"
    );
    let root = tempfile::tempdir().unwrap();
    let store = Datastore::builder()
        .cache_root(root.path().to_owned())
        .allow_upstream(true)
        .progress(false)
        .build()
        .unwrap();
    let artifact = Artifact::new(
        ArtifactKey::new("naif/lsk/naif0012.tls").unwrap(),
        vec![Source::new(
            "https://naif.jpl.nasa.gov/pub/naif/generic_kernels/lsk/naif0012.tls",
        )],
    )
    .with_check(ContentCheck::All(vec![
        ContentCheck::NotHtml,
        ContentCheck::magic(vec![b"KPL/LSK".to_vec()], true),
    ]));
    store
        .get(&artifact)
        .expect("NAIF upstream failed with STARFIELD_ALLOW_UPSTREAM=1");
}
