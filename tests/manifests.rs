//! The checked-in manifests must parse, round-trip, and carry the checks the
//! spec assigns to each kernel type (§5.2).

use starfield_datastore::{ArtifactKey, ContentCheck, Manifest};

fn ephemeris() -> Manifest {
    Manifest::from_path(std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/manifests/ephemeris.toml"
    )))
    .unwrap()
}

#[test]
fn ephemeris_manifest_round_trips_and_covers_the_loader_kernels() {
    let manifest = ephemeris();
    let keys: Vec<_> = manifest.artifacts.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "naif/spk/de421.bsp",
            "naif/spk/de440.bsp",
            "naif/pck/pck00011.tpc",
            "naif/pck/moon_pa_de421_1900-2050.bpc",
            "naif/fk/moon_080317.tf",
            "naif/lsk/naif0012.tls",
        ]
    );
    for artifact in &manifest.artifacts {
        assert!(!artifact.sources.is_empty(), "{}", artifact.key);
        assert!(artifact.expected_bytes.is_some(), "{}", artifact.key);
        assert!(
            matches!(&artifact.check, ContentCheck::All(checks) if matches!(checks.first(), Some(ContentCheck::Sha256(_)))),
            "{} is pinned",
            artifact.key
        );
        assert_eq!(artifact.provenance.license, "public-domain");
        assert!(!artifact.provenance.description.is_empty());
    }
    let again = Manifest::from_toml_str(&manifest.to_toml_string().unwrap()).unwrap();
    assert_eq!(again.artifacts.len(), manifest.artifacts.len());
}

#[test]
fn kernel_checks_reject_the_wrong_kind_and_accept_both_daf_id_words() {
    let manifest = ephemeris();
    // Every entry is `All([Sha256(pin), kind-check])`; exercise the kind
    // check on synthetic bytes, since only real kernels satisfy the pin.
    let check = |key: &str| -> ContentCheck {
        let pinned = manifest
            .get(&ArtifactKey::new(key).unwrap())
            .unwrap()
            .check
            .clone();
        let ContentCheck::All(checks) = pinned else {
            panic!("{key}: expected a pinned check");
        };
        assert!(matches!(checks[0], ContentCheck::Sha256(_)));
        checks[1].clone()
    };
    let html = "<html><body>login</body></html>".repeat(100);

    let spk = check("naif/spk/de440.bsp");
    assert!(spk
        .check(&[b"DAF/SPK ".as_slice(), &[0u8; 4096]].concat())
        .is_ok());
    assert!(spk
        .check(&[b"NAIF/DAF".as_slice(), &[0u8; 4096]].concat())
        .is_ok());
    assert!(
        spk.check(&[b" DAF/SPK".as_slice(), &[0u8; 4096]].concat())
            .is_err(),
        "binary: no trim"
    );
    assert!(spk
        .check(&[b"DAF/PCK ".as_slice(), &[0u8; 4096]].concat())
        .is_err());
    assert!(spk.check(html.as_bytes()).is_err());

    let bpc = check("naif/pck/moon_pa_de421_1900-2050.bpc");
    assert!(bpc
        .check(&[b"DAF/PCK ".as_slice(), &[0u8; 4096]].concat())
        .is_ok());
    assert!(bpc
        .check(&[b"DAF/SPK ".as_slice(), &[0u8; 4096]].concat())
        .is_err());

    let tpc = check("naif/pck/pck00011.tpc");
    assert!(
        tpc.check(b"\n\nKPL/PCK\n\\begindata\n").is_ok(),
        "text: leading blank lines trimmed"
    );
    assert!(tpc.check(html.as_bytes()).is_err());

    let tf = check("naif/fk/moon_080317.tf");
    assert!(tf.check(b"KPL/FK\n").is_ok());

    let tls = check("naif/lsk/naif0012.tls");
    assert!(tls.check(b"KPL/LSK\n").is_ok());
    assert!(tls.check(b"KPL/PCK\n").is_err());
}
