//! Local cache backend using filesystem + SQLite.
//!
//! This module provides a `CacheBackend` implementation that stores data
//! locally without requiring a remote Courier server. It uses:
//! - Filesystem-based CAS for blob storage (with zstd compression)
//! - SQLite for metadata storage (unit hashes, fingerprints, etc.)
//!
//! This is ideal for:
//! - Solo developers who don't need distributed caching
//! - Offline development
//! - Testing and development of Hurry itself

mod cas;
mod metadata;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use color_eyre::{
    Result,
    eyre::{Context, eyre},
};
use derive_more::Debug;
use directories::ProjectDirs;
use futures::{Stream, StreamExt, stream};
use tracing::instrument;

use clients::courier::v1::{GlibcVersion, Key, SavedUnit, SavedUnitHash};

use self::cas::LocalCas;
use self::metadata::LocalMetadata;
use super::backend::{BulkStoreResult, CacheBackend};

/// Default cache directory name under the user's cache directory.
const CACHE_DIR_NAME: &str = "hurry";

/// Local cache backend using filesystem + SQLite.
///
/// This provides a complete `CacheBackend` implementation without requiring
/// network access. All data is stored locally:
/// - Blobs are stored in `{cache_dir}/cas/`
/// - Metadata is stored in `{cache_dir}/metadata.db`
#[derive(Clone, Debug)]
pub struct LocalBackend {
    cas: LocalCas,
    #[debug("<metadata>")]
    metadata: Arc<Mutex<LocalMetadata>>,
}

impl LocalBackend {
    /// Open or create a local cache at the default location.
    ///
    /// Default location: `~/.cache/hurry/` on Linux, `~/Library/Caches/hurry/`
    /// on macOS, `C:\Users\{user}\AppData\Local\hurry\cache\` on Windows.
    ///
    /// Can be overridden with the `HURRY_CACHE_DIR` environment variable.
    #[instrument(name = "LocalBackend::open_default")]
    pub fn open_default() -> Result<Self> {
        let cache_dir = default_cache_dir()?;
        Self::open(cache_dir)
    }

    /// Open or create a local cache at the specified directory.
    #[instrument(name = "LocalBackend::open", skip(cache_dir))]
    pub fn open(cache_dir: impl Into<PathBuf>) -> Result<Self> {
        let cache_dir = cache_dir.into();

        let cas_dir = cache_dir.join("cas");
        let metadata_path = cache_dir.join("metadata.db");

        let cas = LocalCas::new(cas_dir);
        let metadata =
            LocalMetadata::open(&metadata_path).context("open local metadata database")?;

        Ok(Self {
            cas,
            metadata: Arc::new(Mutex::new(metadata)),
        })
    }

    /// Get the CAS storage.
    pub fn cas(&self) -> &LocalCas {
        &self.cas
    }
}

impl CacheBackend for LocalBackend {
    #[instrument(name = "LocalBackend::cargo_save", skip_all)]
    async fn cargo_save(
        &self,
        units: impl IntoIterator<Item = (SavedUnitHash, SavedUnit, String, Option<GlibcVersion>)>
            + Send,
    ) -> Result<()> {
        let metadata = self.metadata.lock().map_err(|e| eyre!("lock error: {e}"))?;

        for (hash, unit, resolved_target, glibc_version) in units {
            metadata.save(&hash, &unit, &resolved_target, glibc_version.as_ref())?;
        }

        Ok(())
    }

    #[instrument(name = "LocalBackend::cargo_restore", skip_all)]
    async fn cargo_restore(
        &self,
        unit_hashes: impl IntoIterator<Item = SavedUnitHash> + Send,
        host_glibc_version: Option<GlibcVersion>,
    ) -> Result<Vec<(SavedUnitHash, SavedUnit)>> {
        let metadata = self.metadata.lock().map_err(|e| eyre!("lock error: {e}"))?;
        metadata.restore(unit_hashes, host_glibc_version.as_ref())
    }

    #[instrument(name = "LocalBackend::cas_store", skip(content))]
    async fn cas_store(&self, key: &Key, content: &[u8]) -> Result<bool> {
        self.cas.write(key, content).await
    }

    #[instrument(name = "LocalBackend::cas_get")]
    async fn cas_get(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        self.cas.read_buffered(key).await
    }

    #[instrument(name = "LocalBackend::cas_exists")]
    async fn cas_exists(&self, key: &Key) -> Result<bool> {
        self.cas.exists(key).await
    }

    #[instrument(name = "LocalBackend::cas_store_bulk", skip(entries))]
    async fn cas_store_bulk(
        &self,
        entries: impl Stream<Item = (Key, Vec<u8>)> + Send + Unpin + 'static,
    ) -> Result<BulkStoreResult> {
        let mut result = BulkStoreResult::default();

        tokio::pin!(entries);
        while let Some((key, content)) = entries.next().await {
            match self.cas.write(&key, &content).await {
                Ok(true) => {
                    result.written.insert(key);
                }
                Ok(false) => {
                    result.skipped.insert(key);
                }
                Err(e) => {
                    result.errors.push((key, e.to_string()));
                }
            }
        }

        Ok(result)
    }

    #[instrument(name = "LocalBackend::cas_get_bulk", skip(keys))]
    async fn cas_get_bulk(
        &self,
        keys: impl IntoIterator<Item = Key> + Send,
    ) -> Result<impl Stream<Item = Result<(Key, Vec<u8>)>> + Send + Unpin> {
        let keys = keys.into_iter().collect::<Vec<_>>();
        let cas = self.cas.clone();

        // Create a stream that fetches each key
        let stream = stream::iter(keys).then(move |key| {
            let cas = cas.clone();
            async move {
                match cas.read_buffered(&key).await {
                    Ok(Some(data)) => Ok((key, data)),
                    Ok(None) => Err(eyre!("key not found: {}", key)),
                    Err(e) => Err(e),
                }
            }
        });

        Ok(Box::pin(stream))
    }
}

/// Get the default cache directory.
///
/// Uses `HURRY_CACHE_DIR` if set, otherwise uses platform-specific defaults.
fn default_cache_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("HURRY_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }

    let dirs = ProjectDirs::from("", "", CACHE_DIR_NAME)
        .ok_or_else(|| eyre!("could not determine cache directory"))?;

    Ok(dirs.cache_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clients::courier::v1::{
        BuildScriptExecutionUnitPlan, BuildScriptOutputFiles, Fingerprint, UnitPlanInfo,
    };
    use pretty_assertions::assert_eq as pretty_assert_eq;

    fn make_saved_unit(hash: &str) -> SavedUnit {
        let info = UnitPlanInfo::builder()
            .unit_hash(hash)
            .package_name("test-pkg")
            .crate_name("test_pkg")
            .maybe_target_arch(Some("x86_64-unknown-linux-gnu"))
            .build();

        let files = BuildScriptOutputFiles::builder()
            .stdout(Key::from_buffer(b"stdout"))
            .stderr(Key::from_buffer(b"stderr"))
            .fingerprint(Fingerprint::from(String::from("test-fingerprint")))
            .build();

        let plan = BuildScriptExecutionUnitPlan::builder()
            .info(info)
            .build_script_program_name("build_script_build")
            .build();

        SavedUnit::BuildScriptExecution(files, plan)
    }

    #[tokio::test]
    async fn backend_round_trip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let backend = LocalBackend::open(temp_dir.path()).unwrap();

        // Test CAS
        let content = b"test content";
        let key = Key::from_buffer(content);

        let stored = backend.cas_store(&key, content).await.unwrap();
        pretty_assert_eq!(stored, true);

        let retrieved = backend.cas_get(&key).await.unwrap().unwrap();
        pretty_assert_eq!(retrieved, content);

        // Test metadata
        let hash = SavedUnitHash::new("test-hash");
        let unit = make_saved_unit("test-hash");
        let target = String::from("x86_64-unknown-linux-gnu");

        backend
            .cargo_save([(hash.clone(), unit.clone(), target, None)])
            .await
            .unwrap();

        let results = backend.cargo_restore([hash.clone()], None).await.unwrap();
        pretty_assert_eq!(results.len(), 1);
        pretty_assert_eq!(results[0].0, hash);
    }
}
