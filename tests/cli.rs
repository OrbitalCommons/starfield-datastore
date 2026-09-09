#![cfg(feature = "cli")]
use std::{
    path::Path,
    process::{Command, Output},
};

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_starfield-datastore"))
        .env_clear()
        .env("STARFIELD_CACHE_DIR", root.join("cache"))
        .env("STARFIELD_DATASTORE_CONFIG", root.join("config.toml"))
        .args(args)
        .output()
        .unwrap()
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn commands_import_fetch_verify_list_and_gc_without_network() {
    let root = tempfile::tempdir().unwrap();
    let manifest = root.path().join("manifest.toml");
    std::fs::write(
        &manifest,
        "[[artifact]]\nkey='manual/data'\nsources=[]\nbytes=4\ncheck={none=true}\n",
    )
    .unwrap();
    let input = root.path().join("data");
    std::fs::write(&input, b"data").unwrap();
    let manifest = manifest.to_str().unwrap();
    let input = input.to_str().unwrap();
    let imported = success(run(
        root.path(),
        &[
            "import",
            "--manifest",
            manifest,
            "--key",
            "manual/data",
            "--from",
            input,
        ],
    ));
    let fetched = success(run(
        root.path(),
        &["fetch", "--manifest", manifest, "--key", "manual/data"],
    ));
    assert_eq!(imported, fetched);
    assert_eq!(
        success(run(root.path(), &["list", "--bytes"])),
        "manual/data\t4\n"
    );
    assert_eq!(
        success(run(root.path(), &["verify", "--manifest", manifest])),
        "ok manual/data\n"
    );
    std::fs::write(imported.trim(), b"evil").unwrap();
    let failed = run(root.path(), &["verify", "--manifest", manifest]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("digest"));
    assert_eq!(std::fs::read(imported.trim()).unwrap(), b"evil");
    assert_eq!(
        success(run(root.path(), &["gc", "--max-bytes", "0"])),
        "manual/data\n"
    );
    assert_eq!(success(run(root.path(), &["list"])), "");
}

#[test]
fn missing_manual_artifact_explains_how_to_obtain_it() {
    let root = tempfile::tempdir().unwrap();
    let manifest = root.path().join("manifest.toml");
    std::fs::write(&manifest,"[[artifact]]\nkey='manual/library'\nsources=[]\ndescription='Request the library, then import the emailed archive'\n").unwrap();
    let failed = run(
        root.path(),
        &[
            "fetch",
            "--manifest",
            manifest.to_str().unwrap(),
            "--key",
            "manual/library",
        ],
    );
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("emailed archive"));
}
