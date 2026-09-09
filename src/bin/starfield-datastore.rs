use clap::{Parser, Subcommand};
use starfield_datastore::{
    ArtifactKey, Datastore, DatastoreBuilder, DatastoreError, Manifest, Result,
};
use std::path::PathBuf;
#[cfg(feature = "mirror-s3")]
use std::{io::Write, path::Path};

#[derive(Parser)]
#[command(version, about = "Validated artifact cache and ephemeris mirror")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Remove a key explicitly, including references blocking shared-blob repair.
    Remove {
        #[arg(long)]
        key: String,
    },
    /// Resolve an artifact into the local cache.
    Fetch {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        key: String,
    },
    /// Seed an artifact from a manually obtained or legacy cache file.
    Import {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        key: String,
        #[arg(long)]
        from: PathBuf,
    },
    /// List cached logical keys.
    List {
        #[arg(long)]
        bytes: bool,
    },
    /// Explicitly evict oldest entries. Do not run while consumers hold paths.
    Gc {
        #[arg(long)]
        max_bytes: Option<u64>,
    },
    /// Rehash content and apply manifest checks; repair requires explicit opt-in.
    Verify {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        repair: bool,
        #[arg(long)]
        region: Option<String>,
    },
    #[cfg(feature = "mirror-s3")]
    /// Prewarm S3 from the manifest and atomically write back digest pins.
    Mirror {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        to: String,
        #[arg(long)]
        region: Option<String>,
    },
    #[cfg(feature = "server")]
    /// Serve the manifest on a loopback or tailnet address.
    Serve {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        bucket: String,
        #[arg(long)]
        region: Option<String>,
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: std::net::SocketAddr,
    },
}

fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(feature = "mirror-s3")]
fn upstream_store() -> Result<Datastore> {
    DatastoreBuilder::from_env()?
        .without_mirror()
        .allow_upstream(true)
        .offline(false)
        .build()
}

fn run(args: Args) -> Result<()> {
    match args.command {
        Command::Remove { key } => {
            Datastore::from_env()?.remove(&ArtifactKey::new(key)?)?;
        }
        Command::Fetch { manifest, key } => {
            let manifest = Manifest::from_path(&manifest)?;
            let key = ArtifactKey::new(key)?;
            let artifact = manifest
                .get(&key)
                .ok_or_else(|| DatastoreError::Manifest(format!("unknown artifact {key}")))?;
            println!(
                "{}",
                DatastoreBuilder::from_env()?
                    .build()?
                    .get(artifact)?
                    .display()
            );
        }
        Command::Import {
            manifest,
            key,
            from,
        } => {
            let manifest = Manifest::from_path(&manifest)?;
            let key = ArtifactKey::new(key)?;
            let artifact = manifest
                .get(&key)
                .ok_or_else(|| DatastoreError::Manifest(format!("unknown artifact {key}")))?;
            println!(
                "{}",
                DatastoreBuilder::from_env()?
                    .build()?
                    .import(artifact, &from)?
                    .display()
            );
        }
        Command::List { bytes } => {
            let store = Datastore::from_env()?;
            for key in store.keys()? {
                if bytes {
                    if let Some(entry) = store.entry(&key) {
                        println!("{}\t{}", key, entry.bytes);
                    }
                } else {
                    println!("{key}");
                }
            }
        }
        Command::Gc { max_bytes } => {
            let store = Datastore::from_env()?;
            let max = max_bytes.or(store.max_bytes()).ok_or_else(|| {
                DatastoreError::Config("gc needs --max-bytes or STARFIELD_CACHE_MAX".into())
            })?;
            for key in store.gc(max)? {
                println!("{key}");
            }
        }
        Command::Verify {
            manifest,
            at,
            repair,
            region,
        } => {
            let manifest = Manifest::from_path(&manifest)?;
            match at {
                None => verify_local(&manifest, repair)?,
                Some(target) => {
                    #[cfg(feature = "mirror-s3")]
                    verify_s3(&manifest, &s3_target(&target, region)?, repair)?;
                    #[cfg(not(feature = "mirror-s3"))]
                    {
                        let _ = (target, region);
                        return Err(DatastoreError::Config(
                            "--at requires the mirror-s3 feature".into(),
                        ));
                    }
                }
            }
        }
        #[cfg(feature = "mirror-s3")]
        Command::Mirror {
            manifest,
            to,
            region,
        } => mirror_manifest(&manifest, &s3_target(&to, region)?)?,
        #[cfg(feature = "server")]
        Command::Serve {
            manifest,
            bucket,
            region,
            bind,
        } => {
            starfield_datastore::server::serve(
                Manifest::from_path(&manifest)?,
                upstream_store()?,
                s3_target(&bucket, region)?,
                bind,
            )?;
        }
    }
    Ok(())
}

fn verify_local(manifest: &Manifest, repair: bool) -> Result<()> {
    let store = DatastoreBuilder::from_env()?.offline(true).build()?;
    let mut failed = Vec::new();
    for artifact in &manifest.artifacts {
        match store.get(artifact) {
            Ok(_) => println!("ok {}", artifact.key),
            Err(error) => {
                eprintln!("{error}");
                failed.push(artifact);
            }
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    let outside = store
        .verify()?
        .into_iter()
        .filter(|failure| manifest.get(&failure.key).is_none())
        .collect::<Vec<_>>();
    for failure in &outside {
        eprintln!("{failure}");
    }
    if repair && !outside.is_empty() {
        return Err(DatastoreError::Config(format!(
            "corrupt entries outside the manifest need explicit remove --key before repair: {}",
            outside
                .iter()
                .map(|f| f.key.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if !repair {
        return Err(DatastoreError::Config(format!(
            "verification failed for {} artifacts; use --repair to refetch explicitly",
            failed.len()
        )));
    }
    // Remove all failed aliases before fetching, so a shared corrupt blob is
    // removed only after its last reference is removed.
    for artifact in &failed {
        store.remove(&artifact.key)?;
    }
    let store = DatastoreBuilder::from_env()?.build()?;
    for artifact in failed {
        store.get(artifact)?;
        println!("repaired {}", artifact.key);
    }
    Ok(())
}

#[cfg(feature = "mirror-s3")]
fn s3_target(target: &str, region: Option<String>) -> Result<starfield_datastore::S3Mirror> {
    let url =
        url::Url::parse(target).map_err(|_| DatastoreError::Config("invalid S3 URL".into()))?;
    if url.scheme() != "s3"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port().is_some()
    {
        return Err(DatastoreError::Config(
            "expected s3://bucket/prefix without credentials, query or fragment".into(),
        ));
    }
    let bucket = url
        .host_str()
        .ok_or_else(|| DatastoreError::Config("S3 URL requires a bucket".into()))?;
    let region = region
        .or_else(|| std::env::var("AWS_REGION").ok())
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
        .ok_or_else(|| DatastoreError::Config("set --region or AWS_REGION".into()))?;
    starfield_datastore::S3Mirror::new(bucket, url.path().trim_matches('/'), region)
}

#[cfg(feature = "mirror-s3")]
fn metadata(store: &Datastore, key: &ArtifactKey) -> Result<starfield_datastore::MirrorObject> {
    let entry = store
        .entry(key)
        .ok_or_else(|| DatastoreError::Config(format!("cache entry disappeared for {key}")))?;
    Ok(starfield_datastore::MirrorObject {
        sha256: entry.digest,
        bytes: entry.bytes,
        etag: None,
        source: entry.source,
        provider_identity: entry.provider_identity,
        fetched_at: entry.fetched_at,
    })
}

#[cfg(feature = "mirror-s3")]
fn mirror_manifest(path: &Path, mirror: &starfield_datastore::S3Mirror) -> Result<()> {
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut manifest = Manifest::from_path(path)?;
    let store = upstream_store()?;
    for artifact in manifest.artifacts.clone() {
        if artifact.provenance.license.trim().is_empty() {
            return Err(DatastoreError::Manifest(format!(
                "{} needs a license before redistribution",
                artifact.key
            )));
        }
        let blob = store.get(&artifact)?;
        let meta = metadata(&store, &artifact.key)?;
        mirror.put(&artifact.key, &blob, &meta)?;
        manifest.pin_sha256(&artifact.key, &meta.sha256)?;
        atomic_manifest(path, &manifest)?;
        println!("mirrored {} {}", artifact.key, meta.sha256);
    }
    Ok(())
}

#[cfg(feature = "mirror-s3")]
fn atomic_manifest(path: &Path, manifest: &Manifest) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(manifest.to_toml_string()?.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(feature = "mirror-s3")]
fn verify_s3(
    manifest: &Manifest,
    mirror: &starfield_datastore::S3Mirror,
    repair: bool,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut failed = 0;
    let verification_cache = DatastoreBuilder::from_env()?
        .without_mirror()
        .offline(true)
        .build()?;
    let store = if repair {
        Some(upstream_store()?)
    } else {
        None
    };
    for artifact in &manifest.artifacts {
        let (original, head_error) = match mirror.head(&artifact.key) {
            Ok(object) => (object, None),
            Err(error) => (None, Some(error)),
        };
        let repair_etag = if repair {
            mirror.observed_etag(&artifact.key)?
        } else {
            None
        };
        let mut temp =
            tempfile::NamedTempFile::new_in(verification_cache.cache_root().join("tmp"))?;
        let check = (|| -> Result<()> {
            if let Some(error) = head_error {
                return Err(error);
            }
            let meta = original.as_ref().ok_or_else(|| {
                DatastoreError::Mirror(format!("{} missing from S3", artifact.key))
            })?;
            if !mirror.fetch(&artifact.key, temp.as_file_mut())? {
                return Err(DatastoreError::Mirror(format!(
                    "{} disappeared from S3",
                    artifact.key
                )));
            }
            let mut file = std::fs::File::open(temp.path())?;
            let mut hash = Sha256::new();
            std::io::copy(&mut file, &mut hash)?;
            if format!("{:x}", hash.finalize()) != meta.sha256
                || temp.as_file().metadata()?.len() != meta.bytes
            {
                return Err(DatastoreError::Mirror(format!(
                    "{} differs from S3 digest/size metadata",
                    artifact.key
                )));
            }
            if artifact.expected_bytes.is_some_and(|n| n != meta.bytes) {
                return Err(DatastoreError::Mirror(format!(
                    "{} differs from manifest size",
                    artifact.key
                )));
            }
            artifact.check.check_file(temp.path()).map_err(|failure| {
                DatastoreError::ContentRejected {
                    key: artifact.key.clone(),
                    failure,
                }
            })
        })();
        match check {
            Ok(()) => println!("ok {}", artifact.key),
            Err(error) => {
                eprintln!("{error}");
                if let Some(store) = &store {
                    if artifact.provenance.license.trim().is_empty() {
                        return Err(DatastoreError::Manifest(format!(
                            "{} needs a license before redistribution",
                            artifact.key
                        )));
                    }
                    let path = store.get(artifact)?;
                    let meta = metadata(store, &artifact.key)?;
                    if let Some(etag) = repair_etag {
                        mirror.replace(&artifact.key, &path, &meta, &etag)?;
                    } else {
                        mirror.put(&artifact.key, &path, &meta)?;
                    }
                    println!("repaired {}", artifact.key);
                } else {
                    failed += 1;
                }
            }
        }
    }
    if failed > 0 {
        return Err(DatastoreError::Mirror(format!(
            "verification failed for {failed} artifacts"
        )));
    }
    Ok(())
}
