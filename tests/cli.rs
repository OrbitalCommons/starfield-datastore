#![cfg(feature = "cli")]
use std::{
    path::Path,
    process::{Command, Output},
};
#[allow(dead_code)]
mod support;

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
    success(run(root.path(), &["remove", "--key", "manual/data"]));
}

#[cfg(all(unix, feature = "server"))]
#[test]
fn service_handles_sigterm_cleanly_without_contacting_aws() {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let root = tempfile::tempdir().unwrap();
    let manifest = root.path().join("empty.toml");
    std::fs::write(&manifest, "").unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let mut child = Command::new(env!("CARGO_BIN_EXE_starfield-datastore"))
        .env_clear()
        .env("STARFIELD_CACHE_DIR", root.path().join("cache"))
        .env(
            "STARFIELD_DATASTORE_CONFIG",
            root.path().join("config.toml"),
        )
        .env("AWS_ACCESS_KEY_ID", "test-access")
        .env("AWS_SECRET_ACCESS_KEY", "test-secret")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .args([
            "serve",
            "--manifest",
            manifest.to_str().unwrap(),
            "--bucket",
            "s3://unused-test-bucket/prefix",
            "--region",
            "us-east-1",
            "--bind",
            &address.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if client
            .get(format!("http://{address}/healthz"))
            .send()
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "service failed to start: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "SIGTERM was not handled gracefully");
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("service did not stop after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
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

#[test]
fn explicit_repair_reports_shared_aliases_then_restores_pinned_content() {
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap();
    let stub = support::stub::Stub::start("127.0.0.1");
    stub.route("/data", support::stub::Response::ok(b"data".to_vec()));
    std::fs::write(root.path().join("config.toml"), "allow_upstream=true").unwrap();
    let manifest = root.path().join("manifest.toml");
    let entry = format!(
        "[[artifact]]\nkey='data'\nsources=['{}/data']\nsha256='{:x}'\ncheck={{none=true}}\n",
        stub.url(),
        Sha256::digest(b"data")
    );
    std::fs::write(
        &manifest,
        format!("{entry}[[artifact]]\nkey='alias'\ncheck={{none=true}}\n"),
    )
    .unwrap();
    let input = root.path().join("input");
    std::fs::write(&input, b"data").unwrap();
    let path = success(run(
        root.path(),
        &[
            "import",
            "--manifest",
            manifest.to_str().unwrap(),
            "--key",
            "data",
            "--from",
            input.to_str().unwrap(),
        ],
    ));
    success(run(
        root.path(),
        &[
            "import",
            "--manifest",
            manifest.to_str().unwrap(),
            "--key",
            "alias",
            "--from",
            input.to_str().unwrap(),
        ],
    ));
    std::fs::write(&manifest, entry).unwrap();
    std::fs::write(path.trim(), b"evil").unwrap();
    let failed = run(
        root.path(),
        &[
            "verify",
            "--manifest",
            manifest.to_str().unwrap(),
            "--repair",
        ],
    );
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("alias"));
    assert!(stub.requests().is_empty());
    success(run(root.path(), &["remove", "--key", "alias"]));
    success(run(
        root.path(),
        &[
            "verify",
            "--manifest",
            manifest.to_str().unwrap(),
            "--repair",
        ],
    ));
    assert_eq!(std::fs::read(path.trim()).unwrap(), b"data");
    assert_eq!(stub.requests().len(), 1);
}
