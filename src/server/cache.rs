//! Server-owned cache admission and quiescent maintenance.
use crate::{Datastore, DatastoreError, Result, ServiceCachePolicy};
use fs2::FileExt;
use std::{
    fs::File,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct State {
    active: usize,
    reserved: u64,
    maintenance: bool,
    failed: Option<String>,
}

pub(super) struct Coordinator {
    store: Arc<Datastore>,
    policy: ServiceCachePolicy,
    state: Mutex<State>,
    // Held for the service lifetime. This coordinates managed servers, not
    // arbitrary library users: the directory must be exclusively server-owned.
    _owner: File,
}

pub(super) struct FillGuard {
    coordinator: Arc<Coordinator>,
    pub(super) limit: u64,
}

fn unavailable(reason: impl Into<String>) -> DatastoreError {
    DatastoreError::FillUnavailable(reason.into())
}

impl Coordinator {
    pub(super) fn new(store: Arc<Datastore>, policy: ServiceCachePolicy) -> Result<Arc<Self>> {
        policy.validate()?;
        let owner = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(store.cache_root().join("locks/.managed-service.lock"))?;
        owner.try_lock_exclusive().map_err(|e| {
            DatastoreError::Config(format!("cannot exclusively own managed cache: {e}"))
        })?;
        let temporary_bytes = std::fs::read_dir(store.cache_root().join("tmp"))?.try_fold(
            0u64,
            |total, entry| -> std::io::Result<u64> {
                let bytes = entry?.metadata()?.len();
                total
                    .checked_add(bytes)
                    .ok_or_else(|| std::io::Error::other("temporary byte count overflow"))
            },
        )?;
        eprintln!("managed cache: retained_max={} inflight_max={} artifact_max={} concurrent_fills={} min_free={} existing_tmp_bytes={temporary_bytes}; exclusive server directory required",
            policy.max_bytes, policy.max_inflight_bytes, policy.max_artifact_bytes,
            policy.max_concurrent_fills, policy.min_free_bytes);
        let this = Arc::new(Self {
            store,
            policy,
            state: Mutex::default(),
            _owner: owner,
        });
        this.maintain()?;
        Ok(this)
    }

    pub(super) fn admit(self: &Arc<Self>, expected: Option<u64>) -> Result<FillGuard> {
        let limit = expected.unwrap_or(self.policy.max_artifact_bytes);
        if limit > self.policy.max_artifact_bytes {
            return Err(DatastoreError::TransferLimit);
        }
        // Do not wait on a blocking worker or hold up S3-hit requests during GC.
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| unavailable("admission busy; retry"))?;
        if let Some(reason) = &state.failed {
            return Err(unavailable(format!(
                "maintenance failed; repair and restart: {reason}"
            )));
        }
        if state.maintenance || state.active >= self.policy.max_concurrent_fills {
            return Err(unavailable(
                "fills draining or concurrency exhausted; retry",
            ));
        }
        let reserved = state
            .reserved
            .checked_add(limit)
            .filter(|n| *n <= self.policy.max_inflight_bytes)
            .ok_or_else(|| unavailable("staging reservations exhausted; retry"))?;
        // Conservative: already-written bytes remain reserved until GC. Never
        // assume a Content-Length or a sibling transfer's progress is reliable.
        let required = self
            .policy
            .min_free_bytes
            .checked_add(reserved)
            .ok_or_else(|| unavailable("filesystem reservation overflow"))?;
        if fs2::available_space(self.store.cache_root())? < required {
            return Err(unavailable("insufficient unreserved filesystem space"));
        }
        state.active += 1;
        state.reserved = reserved;
        Ok(FillGuard {
            coordinator: self.clone(),
            limit,
        })
    }

    fn maintain(&self) -> Result<()> {
        let before = self.store.blob_bytes()?;
        let removed = self.store.gc(self.policy.max_bytes)?;
        let after = self.store.blob_bytes()?;
        eprintln!("cache maintenance: before_bytes={before} after_bytes={after} target_bytes={} removed_keys={}", self.policy.max_bytes, removed.len());
        if after > self.policy.max_bytes {
            return Err(unavailable("retained cache remains above budget"));
        }
        Ok(())
    }

    fn finish(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Closing admission and completing the fill are atomic. Reservations
        // remain charged even when this fill leaves a blob or orphan behind.
        state.maintenance = true;
        state.active -= 1;
        if state.active != 0 {
            return;
        }
        // No fill holds a path or a key/store lock here. Keeping this mutex
        // excludes admission until GC and its independent remeasurement finish.
        let result = catch_unwind(AssertUnwindSafe(|| self.maintain()));
        match result {
            Ok(Ok(())) => {
                state.reserved = 0;
                state.maintenance = false;
            }
            Ok(Err(error)) => state.failed = Some(error.to_string()),
            Err(_) => state.failed = Some("maintenance panicked".into()),
        }
        if let Some(reason) = &state.failed {
            eprintln!("cold fills disabled until restart: {reason}");
        }
    }
}

impl Drop for FillGuard {
    fn drop(&mut self) {
        self.coordinator.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Artifact, ArtifactKey, ContentCheck};

    fn policy() -> ServiceCachePolicy {
        ServiceCachePolicy {
            evict_on_fetch: true,
            max_bytes: 0,
            max_concurrent_fills: 2,
            max_artifact_bytes: 16,
            max_inflight_bytes: 32,
            min_free_bytes: 0,
        }
    }

    fn store(root: &std::path::Path) -> Arc<Datastore> {
        Arc::new(
            Datastore::builder()
                .cache_root(root.to_owned())
                .build()
                .unwrap(),
        )
    }

    fn seed(store: &Datastore, key: &str) -> std::path::PathBuf {
        let artifact =
            Artifact::new(ArtifactKey::new(key).unwrap(), vec![]).with_check(ContentCheck::None);
        let input = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(input.path(), b"same payload").unwrap();
        store.import(&artifact, input.path()).unwrap()
    }

    #[test]
    fn every_active_fill_protects_shared_blobs_and_stops_replenishment() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let cache = Coordinator::new(store.clone(), policy()).unwrap();
        let a = cache.admit(None).unwrap();
        let b = cache.admit(None).unwrap();
        let path = seed(&store, "first");
        assert_eq!(seed(&store, "alias"), path);
        assert!(cache.admit(None).is_err());
        drop(a);
        assert!(path.exists(), "another fill still holds this shared blob");
        assert!(
            cache.admit(Some(1)).is_err(),
            "maintenance must prevent replacement fills"
        );
        drop(b);
        assert!(!path.exists());
        assert_eq!(store.blob_bytes().unwrap(), 0);
        assert!(store.entry(&ArtifactKey::new("alias").unwrap()).is_none());
        assert!(cache.admit(None).is_ok());
    }

    #[test]
    fn startup_cleans_blobs_but_leaves_temporary_files_and_excludes_second_owner() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let path = seed(&store, "old");
        let tmp = root.path().join("tmp/leftover");
        std::fs::write(&tmp, b"operator-owned crash residue").unwrap();
        let cache = Coordinator::new(store.clone(), policy()).unwrap();
        assert!(!path.exists());
        assert!(tmp.exists());
        assert!(Coordinator::new(store.clone(), policy()).is_err());
        drop(cache);
        assert!(Coordinator::new(store, policy()).is_ok());
    }

    #[test]
    fn byte_reservations_and_headroom_are_independent_of_concurrency() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let mut p = policy();
        p.max_inflight_bytes = 20;
        p.max_concurrent_fills = 8;
        let cache = Coordinator::new(store.clone(), p).unwrap();
        let a = cache.admit(None).unwrap();
        let b = cache.admit(Some(4)).unwrap();
        assert!(cache.admit(Some(1)).is_err());
        assert!(matches!(
            cache.admit(Some(17)),
            Err(DatastoreError::TransferLimit)
        ));
        drop(a);
        drop(b);
        drop(cache);
        let mut p = policy();
        p.min_free_bytes = fs2::available_space(root.path()).unwrap() + 1024 * 1024 * 1024;
        let cache = Coordinator::new(store, p).unwrap();
        assert!(matches!(
            cache.admit(None),
            Err(DatastoreError::FillUnavailable(_))
        ));
    }

    #[test]
    fn panic_releases_fill_and_gc_failure_closes_admission() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let cache = Coordinator::new(store.clone(), policy()).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = cache.admit(None).unwrap();
            seed(&store, "panic");
            panic!("upload worker panicked");
        }));
        assert!(result.is_err());
        assert_eq!(store.blob_bytes().unwrap(), 0);
        let guard = cache.admit(None).unwrap();
        // A malformed index prevents safe GC. Failure injection works even as root.
        std::fs::write(root.path().join("index/broken.json"), b"not json").unwrap();
        drop(guard);
        assert!(matches!(
            cache.admit(None),
            Err(DatastoreError::FillUnavailable(_))
        ));
    }

    #[test]
    fn retained_budget_uses_unique_blob_bytes() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path());
        let mut p = policy();
        p.max_bytes = 12;
        let cache = Coordinator::new(store.clone(), p).unwrap();
        let guard = cache.admit(None).unwrap();
        let path = seed(&store, "one");
        seed(&store, "two");
        drop(guard);
        assert!(path.exists());
        assert_eq!(store.blob_bytes().unwrap(), 12);
    }
}
