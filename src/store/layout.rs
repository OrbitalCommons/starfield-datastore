//! On-disk layout of the local cache (spec §12).
//!
//! ```text
//! <root>/blobs/<aa>/<sha256>      content-addressed, immutable once written
//! <root>/index/<key path>.json    sidecar: digest, bytes, fetched_at, source…
//! <root>/tmp/                     in-flight downloads; temp + fsync + rename
//! <root>/locks/<sha256(key)>.lock advisory, one per key
//! <root>/locks/.store.lock        store-wide; guards blob publication and
//!                                 reference scans, always taken after a key lock
//! ```

use super::IndexEntry;
use crate::check::digest_file;
use crate::{ArtifactKey, DatastoreError, Result};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

pub(crate) struct Layout {
    root: PathBuf,
}

/// Exclusive advisory lock, released on drop.
pub(crate) struct Lock {
    file: File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(crate) fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Layout {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn ensure(&self) -> Result<()> {
        for dir in ["blobs", "index", "tmp", "locks"] {
            fs::create_dir_all(self.root.join(dir))?;
        }
        Ok(())
    }

    /// Callers pass digests that came from a sidecar (validated on read) or
    /// from hashing, so slicing is safe.
    pub(crate) fn blob_path(&self, digest: &str) -> PathBuf {
        debug_assert!(is_digest(digest));
        self.root.join("blobs").join(&digest[..2]).join(digest)
    }

    fn index_path(&self, key: &ArtifactKey) -> PathBuf {
        self.root
            .join("index")
            .join(format!("{}.json", key.as_str()))
    }

    pub(crate) fn tmp_file(&self) -> Result<NamedTempFile> {
        Ok(NamedTempFile::new_in(self.root.join("tmp"))?)
    }

    fn lock_file(&self, name: &str) -> Result<Lock> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join("locks").join(name))?;
        file.lock_exclusive()?;
        Ok(Lock { file })
    }

    /// Serialises every operation on one key across processes.
    pub(crate) fn lock(&self, key: &ArtifactKey) -> Result<Lock> {
        self.lock_file(&format!(
            "{:x}.lock",
            Sha256::digest(key.as_str().as_bytes())
        ))
    }

    /// Serialises blob publication against reference scans and deletion.
    /// Always taken while holding a key lock, never the other way round.
    pub(crate) fn store_lock(&self) -> Result<Lock> {
        self.lock_file(".store.lock")
    }

    pub(crate) fn read_entry(&self, key: &ArtifactKey) -> Result<Option<IndexEntry>> {
        let path = self.index_path(key);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let corrupt = |detail: String| {
            DatastoreError::Io(std::io::Error::other(format!(
                "corrupt index sidecar {}: {detail}",
                path.display()
            )))
        };
        let entry: IndexEntry = serde_json::from_str(&text).map_err(|e| corrupt(e.to_string()))?;
        if !is_digest(&entry.digest) {
            return Err(corrupt("digest is not 64 lowercase hex characters".into()));
        }
        Ok(Some(entry))
    }

    pub(crate) fn write_entry(&self, key: &ArtifactKey, entry: &IndexEntry) -> Result<()> {
        let path = self.index_path(key);
        let parent = path.parent().expect("index path has a parent");
        fs::create_dir_all(parent)?;
        let mut tmp = NamedTempFile::new_in(parent)?;
        tmp.write_all(
            serde_json::to_string_pretty(entry)
                .expect("entry serialises")
                .as_bytes(),
        )?;
        tmp.as_file().sync_all()?;
        tmp.persist(&path).map_err(|e| e.error)?;
        sync_dir(parent)
    }

    pub(crate) fn remove_entry(&self, key: &ArtifactKey) -> Result<bool> {
        match fs::remove_file(self.index_path(key)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Every key with an index sidecar, in sorted order.
    pub(crate) fn keys(&self) -> Result<Vec<ArtifactKey>> {
        let index = self.root.join("index");
        let mut keys = Vec::new();
        if index.is_dir() {
            walk(&index, &index, &mut keys)?;
        }
        keys.sort();
        Ok(keys)
    }

    /// Publish a validated temp file as the blob for `digest`. An existing
    /// blob is kept if it still hashes to its address; `None` if it does
    /// not, because repair is explicit. Call under the store lock.
    pub(crate) fn publish(&self, tmp: NamedTempFile, digest: &str) -> Result<Option<PathBuf>> {
        let path = self.blob_path(digest);
        if path.is_file() {
            return Ok((digest_file(&path)? == digest).then_some(path));
        }
        let parent = path.parent().expect("blob path has a parent");
        fs::create_dir_all(parent)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&path).map_err(|e| e.error)?;
        sync_dir(parent)?;
        Ok(Some(path))
    }

    /// Every blob physically present, whether or not an index entry refers
    /// to it: a crash between rename and sidecar write, or a pin that moved
    /// on, leaves orphans that only an explicit `gc` reclaims.
    pub(crate) fn blobs(&self) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        let blobs = self.root.join("blobs");
        if !blobs.is_dir() {
            return Ok(out);
        }
        for shard in fs::read_dir(&blobs)? {
            let shard = shard?.path();
            if !shard.is_dir() {
                continue;
            }
            for blob in fs::read_dir(&shard)? {
                let blob = blob?;
                let Some(name) = blob.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if is_digest(&name) && blob.file_type()?.is_file() {
                    out.push((name, blob.metadata()?.len()));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Delete in-flight temp files older than `max_age`: a crashed process
    /// never gets to remove its own.
    pub(crate) fn remove_stale_tmp(&self, max_age: std::time::Duration) -> Result<()> {
        let cutoff = std::time::SystemTime::now()
            .checked_sub(max_age)
            .unwrap_or(std::time::UNIX_EPOCH);
        for entry in fs::read_dir(self.root.join("tmp"))? {
            let entry = entry?;
            if entry.metadata()?.modified()? < cutoff {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    pub(crate) fn remove_blob(&self, digest: &str) -> Result<()> {
        match fs::remove_file(self.blob_path(digest)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Make a rename durable: the directory entry lives in the parent.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<ArtifactKey>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk(base, &path, out)?;
            continue;
        }
        let Ok(relative) = path.strip_prefix(base) else {
            continue;
        };
        let Some(text) = relative.to_str() else {
            continue;
        };
        if let Some(key) = text.strip_suffix(".json") {
            if let Ok(key) = ArtifactKey::new(key.replace('\\', "/")) {
                out.push(key);
            }
        }
    }
    Ok(())
}

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_shape_is_strict() {
        assert!(is_digest(&"a".repeat(64)));
        assert!(!is_digest(&"A".repeat(64)));
        assert!(!is_digest(&"a".repeat(63)));
        assert!(!is_digest(&format!("../{}", "a".repeat(61))));
    }

    #[test]
    fn malformed_sidecars_are_errors_not_panics() {
        let root = tempfile::TempDir::new().unwrap();
        let layout = Layout::new(root.path().to_path_buf());
        layout.ensure().unwrap();
        let key = ArtifactKey::new("k").unwrap();
        fs::write(
            layout.index_path(&key),
            r#"{"digest":"../x","bytes":1,"fetched_at":1,"source":null,"etag":null,"provider_identity":null,"layer":"upstream"}"#,
        )
        .unwrap();
        let err = layout.read_entry(&key).unwrap_err();
        assert!(err.to_string().contains("corrupt index sidecar"), "{err}");
        fs::write(layout.index_path(&key), "not json").unwrap();
        assert!(layout.read_entry(&key).is_err());
    }
}
