//! The store: the local content-addressed layer and the resolution chain
//! (spec §7, §12).
//!
//! `get` resolves `local disk → mirror → upstream`, populating the local
//! layer as it goes. Every byte that lands in the cache has passed the
//! artifact's `ContentCheck`; the blob is written to `tmp/`, hashed and
//! validated, then renamed into `blobs/` under its SHA-256. The chain only
//! ever reads a mirror: writing the mirror is an explicit act of the
//! ephemeris server.

mod layout;

use crate::check::digest_file;
use crate::fetch::{sanitize, Fetcher, Progress};
use crate::mirror::MirrorRead;
use crate::{
    config, mirror, Artifact, ArtifactKey, CheckFailure, ContentCheck, CredentialProvider,
    DatastoreError, Mirror, Result,
};
use layout::{now_unix, Layout};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

/// How many leading bytes are shown to `ContentCheck::check_prefix` before
/// the rest of a transfer is allowed to proceed.
const PREFIX_BYTES: usize = 8 * 1024;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Which layer of the chain served a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    LocalDisk,
    Mirror,
    Upstream,
}

#[derive(Debug, Clone)]
pub struct ResolveOutcome {
    pub layer: Layer,
    pub bytes: u64,
    pub duration: Duration,
    /// Which `Source` index succeeded, when `layer == Upstream`.
    pub source_index: Option<usize>,
}

/// The index sidecar for one key (spec §12). Never holds a secret: the
/// provider *identity* is recorded, not the credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Lowercase hex SHA-256; also the blob's address.
    pub digest: String,
    pub bytes: u64,
    /// Unix seconds.
    pub fetched_at: u64,
    /// Sanitised origin and path of what was fetched, the mirror location,
    /// or `local:import`. Never a query string.
    pub source: Option<String>,
    pub etag: Option<String>,
    pub provider_identity: Option<String>,
    pub layer: Layer,
}

/// A blob whose content no longer matches its index entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyFailure {
    pub key: ArtifactKey,
    pub expected: String,
    /// `None` when the blob is missing altogether.
    pub actual: Option<String>,
}

impl std::fmt::Display for VerifyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.actual {
            Some(actual) => write!(f, "{}: expected {} got {actual}", self.key, self.expected),
            None => write!(f, "{}: blob {} is missing", self.key, self.expected),
        }
    }
}

/// Progress callback: `(bytes received so far, total if known)`.
pub type ProgressFn = dyn Fn(u64, Option<u64>) + Send + Sync;

pub struct Datastore {
    layout: Layout,
    mirror: Option<Box<dyn MirrorRead>>,
    fetcher: Fetcher,
    allow_upstream: bool,
    offline: bool,
    max_bytes: Option<u64>,
}

#[derive(Default)]
pub struct DatastoreBuilder {
    pub(crate) cache_root: Option<PathBuf>,
    pub(crate) mirror: Option<Mirror>,
    pub(crate) credentials: Option<Box<dyn CredentialProvider>>,
    pub(crate) allow_upstream: Option<bool>,
    pub(crate) offline: Option<bool>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) progress: Option<bool>,
    pub(crate) on_progress: Option<Arc<ProgressFn>>,
    pub(crate) max_bytes: Option<u64>,
}

impl DatastoreBuilder {
    /// A builder pre-filled from the environment and the config file
    /// (spec §11). Calls made afterwards override what was read.
    pub fn from_env() -> Result<Self> {
        config::apply(Self::default())
    }

    pub fn cache_root(mut self, path: PathBuf) -> Self {
        self.cache_root = Some(path);
        self
    }

    pub fn mirror(mut self, mirror: Mirror) -> Self {
        self.mirror = Some(mirror);
        self
    }

    /// Drop any mirror picked up from the environment. The ephemeris server
    /// uses this so it never resolves through itself.
    pub fn without_mirror(mut self) -> Self {
        self.mirror = None;
        self
    }

    pub fn credentials(mut self, provider: Box<dyn CredentialProvider>) -> Self {
        self.credentials = Some(provider);
        self
    }

    /// Permit the upstream layer. Default false; see spec §2.4.
    pub fn allow_upstream(mut self, allow: bool) -> Self {
        self.allow_upstream = Some(allow);
        self
    }

    /// Disable the mirror and upstream layers. What CI sets once the cache
    /// is warm.
    pub fn offline(mut self, offline: bool) -> Self {
        self.offline = Some(offline);
        self
    }

    /// Connect timeout. There is deliberately no read or total timeout: a
    /// 12 GB mosaic takes as long as it takes.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Draw progress bars on stderr (feature `progress`). Default on; bars
    /// are hidden when stderr is not a terminal.
    pub fn progress(mut self, enabled: bool) -> Self {
        self.progress = Some(enabled);
        self
    }

    /// Receive `(received, total)` per chunk instead of drawing bars.
    pub fn on_progress(mut self, callback: Box<ProgressFn>) -> Self {
        self.on_progress = Some(Arc::from(callback));
        self
    }

    /// Advisory cache budget; applied only by an explicit `gc`.
    pub fn max_bytes(mut self, max: u64) -> Self {
        self.max_bytes = Some(max);
        self
    }

    pub fn build(self) -> Result<Datastore> {
        let root = match self.cache_root {
            Some(root) => root,
            None => config::default_cache_root()?,
        };
        let layout = Layout::new(root);
        layout.ensure()?;
        let timeout = self.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let progress = match (self.on_progress, self.progress) {
            (Some(callback), _) => Progress::Callback(callback),
            (None, Some(false)) => Progress::Off,
            (None, _) => Progress::Bars,
        };
        let provider: Option<Arc<dyn CredentialProvider>> = self.credentials.map(Arc::from);
        let fetcher = Fetcher::new(timeout, provider, progress.clone())?;
        let mirror = match self.mirror {
            Some(m) => Some(mirror::open(&m, timeout, progress)?),
            None => None,
        };
        Ok(Datastore {
            layout,
            mirror,
            fetcher,
            allow_upstream: self.allow_upstream.unwrap_or(false),
            offline: self.offline.unwrap_or(false),
            max_bytes: self.max_bytes,
        })
    }
}

impl Datastore {
    pub fn builder() -> DatastoreBuilder {
        DatastoreBuilder::default()
    }

    /// From env + `~/.config/starfield/datastore.toml` + defaults.
    pub fn from_env() -> Result<Self> {
        DatastoreBuilder::from_env()?.build()
    }

    pub fn cache_root(&self) -> &Path {
        self.layout.root()
    }

    /// The configured cache budget, if any. Enforced only by `gc`.
    pub fn max_bytes(&self) -> Option<u64> {
        self.max_bytes
    }

    /// Resolve to a local path, fetching through the chain if needed.
    pub fn get(&self, artifact: &Artifact) -> Result<PathBuf> {
        self.get_with_outcome(artifact).map(|(path, _)| path)
    }

    /// As `get`, reporting which layer served it.
    pub fn get_with_outcome(&self, artifact: &Artifact) -> Result<(PathBuf, ResolveOutcome)> {
        let start = Instant::now();
        let key = &artifact.key;
        let _lock = self.layout.lock(key)?;

        if let Some(entry) = self.layout.read_entry(key)? {
            if let Some(path) = self.local_hit(artifact, &entry)? {
                return Ok((path, outcome(Layer::LocalDisk, entry.bytes, start, None)));
            }
        }
        if self.offline {
            return Err(DatastoreError::OfflineMiss { key: key.clone() });
        }

        let mirror_reason = match &self.mirror {
            None => "no mirror configured".to_string(),
            Some(mirror) => match self.fetch_from_mirror(artifact, mirror.as_ref()) {
                Ok(Some((path, bytes))) => {
                    return Ok((path, outcome(Layer::Mirror, bytes, start, None)));
                }
                Ok(None) => format!("{} has no entry", mirror.location(key)),
                Err(e @ DatastoreError::ContentRejected { .. }) => return Err(e),
                Err(e) => e.to_string(),
            },
        };
        if artifact.sources.is_empty() {
            return Err(DatastoreError::ManualRequired {
                key: key.clone(),
                instructions: artifact.provenance.description.clone(),
            });
        }
        if !self.allow_upstream {
            return Err(DatastoreError::MirrorUnreachable {
                key: key.clone(),
                reason: mirror_reason,
            });
        }

        let mut attempts = Vec::with_capacity(artifact.sources.len());
        for (index, source) in artifact.sources.iter().enumerate() {
            match self.fetch_from_upstream(artifact, index) {
                Ok((path, bytes)) => {
                    return Ok((path, outcome(Layer::Upstream, bytes, start, Some(index))));
                }
                Err(
                    e @ (DatastoreError::ContentRejected { .. }
                    | DatastoreError::NoCredential { .. }
                    | DatastoreError::CredentialRejected { .. }),
                ) if artifact.sources.len() == 1 => {
                    return Err(e);
                }
                Err(e) => attempts.push(format!("{}: {e}", sanitize(&source.url))),
            }
        }
        Err(DatastoreError::AllSourcesFailed {
            key: key.clone(),
            attempts,
        })
    }

    /// Read fully into memory. For small artifacts only.
    pub fn get_bytes(&self, artifact: &Artifact) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.get(artifact)?)?)
    }

    /// Seed the cache from a file already on disk, validating it exactly as
    /// a download would be. The file is copied, not moved.
    pub fn import(&self, artifact: &Artifact, path: &Path) -> Result<PathBuf> {
        let _lock = self.layout.lock(&artifact.key)?;
        let mut file = std::fs::File::open(path)?;
        let (tmp, digest, bytes) = self
            .receive(artifact, |sink| {
                std::io::copy(&mut file, sink)?;
                Ok(true)
            })?
            .expect("a local file is always served");
        let mut entry = IndexEntry::new(digest, bytes, Layer::LocalDisk);
        entry.source = Some("local:import".into());
        self.publish(artifact, tmp, entry)
    }

    /// Local path if already cached; never fetches and never rehashes.
    pub fn peek(&self, key: &ArtifactKey) -> Option<PathBuf> {
        let entry = self.layout.read_entry(key).ok()??;
        let path = self.layout.blob_path(&entry.digest);
        path.is_file().then_some(path)
    }

    pub fn contains(&self, key: &ArtifactKey) -> bool {
        self.peek(key).is_some()
    }

    /// The index sidecar for `key`, if cached.
    pub fn entry(&self, key: &ArtifactKey) -> Option<IndexEntry> {
        let entry = self.layout.read_entry(key).ok()??;
        self.layout
            .blob_path(&entry.digest)
            .is_file()
            .then_some(entry)
    }

    pub fn remove(&self, key: &ArtifactKey) -> Result<()> {
        let _lock = self.layout.lock(key)?;
        let _store = self.layout.store_lock()?;
        let Some(entry) = self.layout.read_entry(key)? else {
            return Ok(());
        };
        self.layout.remove_entry(key)?;
        if !self.digest_referenced(&entry.digest)? {
            self.layout.remove_blob(&entry.digest)?;
        }
        Ok(())
    }

    pub fn keys(&self) -> Result<Vec<ArtifactKey>> {
        self.layout.keys()
    }

    /// Bytes on disk across all blobs; shared blobs count once.
    pub fn total_bytes(&self) -> Result<u64> {
        let mut seen = HashSet::new();
        let mut total = 0;
        for (_, entry) in self.entries()? {
            if seen.insert(entry.digest.clone()) {
                total += self.blob_size(&entry);
            }
        }
        Ok(total)
    }

    /// Rehash every blob; report keys whose content no longer matches.
    pub fn verify(&self) -> Result<Vec<VerifyFailure>> {
        let mut failures = Vec::new();
        for (key, entry) in self.entries()? {
            let path = self.layout.blob_path(&entry.digest);
            let actual = if path.is_file() {
                Some(digest_file(&path)?)
            } else {
                None
            };
            if actual.as_deref() != Some(entry.digest.as_str()) {
                failures.push(VerifyFailure {
                    key,
                    expected: entry.digest,
                    actual,
                });
            }
        }
        Ok(failures)
    }

    /// Evict least-recently-fetched keys until the store is within
    /// `max_bytes`. Explicit only: nothing is evicted during `get`.
    pub fn gc(&self, max_bytes: u64) -> Result<Vec<ArtifactKey>> {
        let mut entries = self.entries()?;
        entries.sort_by_key(|(_, e)| e.fetched_at);
        let mut sizes: HashMap<String, u64> = HashMap::new();
        for (_, entry) in &entries {
            sizes
                .entry(entry.digest.clone())
                .or_insert_with(|| self.blob_size(entry));
        }
        let mut total: u64 = sizes.values().sum();
        let mut removed = Vec::new();
        for (key, snapshot) in &entries {
            if total <= max_bytes {
                break;
            }
            let _lock = self.layout.lock(key)?;
            let _store = self.layout.store_lock()?;
            // Re-read under the locks: the key may have been refetched or
            // removed since the snapshot was taken.
            let Some(entry) = self.layout.read_entry(key)? else {
                continue;
            };
            if entry.digest != snapshot.digest {
                continue;
            }
            self.layout.remove_entry(key)?;
            if !self.digest_referenced(&entry.digest)? {
                self.layout.remove_blob(&entry.digest)?;
                total = total.saturating_sub(sizes.get(&entry.digest).copied().unwrap_or(0));
            }
            removed.push(key.clone());
        }
        Ok(removed)
    }

    fn entries(&self) -> Result<Vec<(ArtifactKey, IndexEntry)>> {
        let mut out = Vec::new();
        for key in self.layout.keys()? {
            if let Some(entry) = self.layout.read_entry(&key)? {
                out.push((key, entry));
            }
        }
        Ok(out)
    }

    fn blob_size(&self, entry: &IndexEntry) -> u64 {
        std::fs::metadata(self.layout.blob_path(&entry.digest))
            .map(|m| m.len())
            .unwrap_or(entry.bytes)
    }

    fn digest_referenced(&self, digest: &str) -> Result<bool> {
        Ok(self.entries()?.iter().any(|(_, e)| e.digest == digest))
    }

    /// A cached blob is served only after it has been rehashed against its
    /// index entry and re-validated against the artifact's check. A blob
    /// whose pin has moved on in the manifest is treated as a miss.
    fn local_hit(&self, artifact: &Artifact, entry: &IndexEntry) -> Result<Option<PathBuf>> {
        let path = self.layout.blob_path(&entry.digest);
        if !path.is_file() {
            return Ok(None);
        }
        if pinned_sha256s(&artifact.check)
            .iter()
            .any(|pin| *pin != entry.digest)
        {
            return Ok(None);
        }
        let corrupt = |what: &str, got: String| DatastoreError::ContentRejected {
            key: artifact.key.clone(),
            failure: CheckFailure {
                check: format!("cached blob {what}"),
                got: format!(
                    "{got}; the blob is corrupt — run verify and remove the keys it reports"
                ),
            },
        };
        let length = std::fs::metadata(&path)?.len();
        if length != entry.bytes {
            return Err(corrupt(
                "size",
                format!("{length} bytes, but the index records {}", entry.bytes),
            ));
        }
        let actual = digest_file(&path)?;
        if actual != entry.digest {
            return Err(corrupt(
                "digest",
                format!("{actual}, but the index records {}", entry.digest),
            ));
        }
        validate(artifact, &path, entry.bytes, &actual)?;
        Ok(Some(path))
    }

    fn fetch_from_mirror(
        &self,
        artifact: &Artifact,
        mirror: &dyn MirrorRead,
    ) -> Result<Option<(PathBuf, u64)>> {
        let Some((tmp, digest, bytes)) =
            self.receive(artifact, |sink| mirror.fetch(&artifact.key, sink))?
        else {
            return Ok(None);
        };
        let mut entry = IndexEntry::new(digest, bytes, Layer::Mirror);
        entry.source = Some(mirror.location(&artifact.key));
        let path = self.publish(artifact, tmp, entry)?;
        Ok(Some((path, bytes)))
    }

    fn fetch_from_upstream(&self, artifact: &Artifact, index: usize) -> Result<(PathBuf, u64)> {
        let source = &artifact.sources[index];
        let mut download = None;
        let received = self.receive(artifact, |sink| {
            download = self.fetcher.request(source, sink)?;
            Ok(download.is_some())
        })?;
        let (Some((tmp, digest, bytes)), Some(download)) = (received, download) else {
            return Err(DatastoreError::Mirror("HTTP 404 Not Found".into()));
        };
        let mut entry = IndexEntry::new(digest, bytes, Layer::Upstream);
        entry.source = Some(sanitize(&source.url));
        entry.etag = download.etag;
        entry.provider_identity = download.provider_identity;
        let path = self.publish(artifact, tmp, entry)?;
        Ok((path, bytes))
    }

    /// Run `producer` against a hashing temp file. The first 8 KB are shown
    /// to `check_prefix` before the transfer continues, so a login page is
    /// rejected before 12 GB of it arrive. Returns the temp file, digest and
    /// byte count; `None` when the producer reported no object.
    fn receive<F>(
        &self,
        artifact: &Artifact,
        producer: F,
    ) -> Result<Option<(NamedTempFile, String, u64)>>
    where
        F: FnOnce(&mut dyn Write) -> Result<bool>,
    {
        let mut tmp = self.layout.tmp_file()?;
        let mut writer = Receiver::new(tmp.as_file_mut(), &artifact.check);
        let served = match producer(&mut writer) {
            Ok(served) => served,
            Err(DatastoreError::Io(_)) if writer.rejected.is_some() => false,
            Err(e) => return Err(e),
        };
        if let Some(failure) = writer.rejected.take() {
            return Err(DatastoreError::ContentRejected {
                key: artifact.key.clone(),
                failure,
            });
        }
        if !served {
            return Ok(None);
        }
        writer
            .finish_prefix()
            .map_err(|failure| DatastoreError::ContentRejected {
                key: artifact.key.clone(),
                failure,
            })?;
        let (digest, bytes) = writer.finish();
        tmp.as_file_mut().flush()?;
        Ok(Some((tmp, digest, bytes)))
    }

    /// Validate, then move the temp file into place and write the sidecar,
    /// under the store lock so reference scans see a consistent picture.
    /// An existing blob at the same address that no longer hashes to it is
    /// never silently overwritten: repair is explicit.
    fn publish(
        &self,
        artifact: &Artifact,
        tmp: NamedTempFile,
        entry: IndexEntry,
    ) -> Result<PathBuf> {
        validate(artifact, tmp.path(), entry.bytes, &entry.digest)?;
        let _store = self.layout.store_lock()?;
        let path = self
            .layout
            .publish(tmp, &entry.digest)?
            .ok_or_else(|| DatastoreError::ContentRejected {
                key: artifact.key.clone(),
                failure: CheckFailure {
                    check: "cached blob digest".into(),
                    got: format!(
                        "an existing blob at {} no longer hashes to its address; run verify and remove the keys it reports",
                        entry.digest
                    ),
                },
            })?;
        self.layout.write_entry(&artifact.key, &entry)?;
        Ok(path)
    }
}

/// Everything that must hold before bytes become a blob: the size pin, the
/// digest pins, and the artifact's own check. The digest was computed while
/// streaming, so `Sha256` nodes are compared here rather than rehashed.
fn validate(artifact: &Artifact, path: &Path, bytes: u64, digest: &str) -> Result<()> {
    let reject = |check: &str, got: String| DatastoreError::ContentRejected {
        key: artifact.key.clone(),
        failure: CheckFailure {
            check: check.into(),
            got,
        },
    };
    if let Some(expected) = artifact.expected_bytes {
        if expected != bytes {
            return Err(reject(
                "expected_bytes",
                format!("{bytes} bytes, expected {expected}"),
            ));
        }
    }
    for pin in pinned_sha256s(&artifact.check) {
        if pin != digest {
            return Err(reject("Sha256", format!("digest {digest}, expected {pin}")));
        }
    }
    without_sha256(&artifact.check)
        .check_file(path)
        .map_err(|failure| DatastoreError::ContentRejected {
            key: artifact.key.clone(),
            failure,
        })
}

/// Every `Sha256` pin in the tree, in order. Conflicting pins can never all
/// match, so an artifact carrying two different ones is rejected outright.
fn pinned_sha256s(check: &ContentCheck) -> Vec<&str> {
    match check {
        ContentCheck::Sha256(digest) => vec![digest],
        ContentCheck::All(checks) => checks.iter().flat_map(pinned_sha256s).collect(),
        _ => Vec::new(),
    }
}

fn without_sha256(check: &ContentCheck) -> ContentCheck {
    match check {
        ContentCheck::Sha256(_) => ContentCheck::None,
        ContentCheck::All(checks) => ContentCheck::All(checks.iter().map(without_sha256).collect()),
        other => other.clone(),
    }
}

fn outcome(
    layer: Layer,
    bytes: u64,
    start: Instant,
    source_index: Option<usize>,
) -> ResolveOutcome {
    ResolveOutcome {
        layer,
        bytes,
        duration: start.elapsed(),
        source_index,
    }
}

/// Hashes and counts what passes through, and shows the first `PREFIX_BYTES`
/// to the artifact's check before letting the rest of the body in.
struct Receiver<'a> {
    inner: &'a mut std::fs::File,
    check: &'a ContentCheck,
    hasher: Sha256,
    bytes: u64,
    head: Vec<u8>,
    prefix_checked: bool,
    rejected: Option<CheckFailure>,
}

impl<'a> Receiver<'a> {
    fn new(inner: &'a mut std::fs::File, check: &'a ContentCheck) -> Self {
        Self {
            inner,
            check,
            hasher: Sha256::new(),
            bytes: 0,
            head: Vec::with_capacity(PREFIX_BYTES),
            prefix_checked: false,
            rejected: None,
        }
    }

    fn check_prefix(&mut self) -> std::io::Result<()> {
        self.prefix_checked = true;
        if let Err(failure) = self.check.check_prefix(&self.head) {
            let message = format!("{}: {}", failure.check, failure.got);
            self.rejected = Some(failure);
            return Err(std::io::Error::other(message));
        }
        Ok(())
    }

    /// A body shorter than the prefix window is checked at the end.
    fn finish_prefix(&mut self) -> std::result::Result<(), CheckFailure> {
        if !self.prefix_checked {
            self.prefix_checked = true;
            self.check.check_prefix(&self.head)?;
        }
        Ok(())
    }

    fn finish(self) -> (String, u64) {
        (format!("{:x}", self.hasher.finalize()), self.bytes)
    }
}

impl Write for Receiver<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(failure) = &self.rejected {
            return Err(std::io::Error::other(format!(
                "{}: {}",
                failure.check, failure.got
            )));
        }
        if !self.prefix_checked {
            let take = (PREFIX_BYTES - self.head.len()).min(buf.len());
            self.head.extend_from_slice(&buf[..take]);
            if self.head.len() == PREFIX_BYTES {
                self.check_prefix()?;
            }
        }
        self.inner.write_all(buf)?;
        self.hasher.update(buf);
        self.bytes += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl IndexEntry {
    fn new(digest: String, bytes: u64, layer: Layer) -> Self {
        Self {
            digest,
            bytes,
            fetched_at: now_unix(),
            source: None,
            etag: None,
            provider_identity: None,
            layer,
        }
    }
}
