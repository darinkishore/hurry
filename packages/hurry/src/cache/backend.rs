//! Cache backend abstraction layer.
//!
//! This module defines the `CacheBackend` trait that abstracts over different
//! cache storage implementations. Two implementations are provided:
//!
//! - `CourierBackend`: Uses the remote Courier HTTP API for distributed caching
//! - `LocalBackend`: Uses local filesystem + SQLite for local-only caching

use std::collections::BTreeSet;

use color_eyre::Result;
use futures::Stream;

use clients::courier::v1::{GlibcVersion, Key, SavedUnit, SavedUnitHash};

/// Result of a bulk store operation.
#[derive(Clone, Debug, Default)]
pub struct BulkStoreResult {
    /// Keys that were successfully written.
    pub written: BTreeSet<Key>,
    /// Keys that were skipped because they already existed.
    pub skipped: BTreeSet<Key>,
    /// Keys that failed to write along with their error messages.
    pub errors: Vec<(Key, String)>,
}

/// Trait for cache backend implementations.
///
/// This trait abstracts the storage layer for Hurry's build cache, enabling
/// both remote (Courier) and local (SQLite + filesystem) storage backends.
pub trait CacheBackend: Clone + Send + Sync + 'static {
    /// Save cargo build units to the cache.
    ///
    /// This stores the metadata about compiled units (fingerprints, file
    /// mappings, etc.) so they can be restored later.
    fn cargo_save(
        &self,
        units: impl IntoIterator<Item = (SavedUnitHash, SavedUnit, String, Option<GlibcVersion>)>
            + Send,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Restore cargo build units from the cache.
    ///
    /// Returns the units that match the requested hashes and are compatible
    /// with the host's glibc version (if applicable).
    fn cargo_restore(
        &self,
        unit_hashes: impl IntoIterator<Item = SavedUnitHash> + Send,
        host_glibc_version: Option<GlibcVersion>,
    ) -> impl Future<Output = Result<Vec<(SavedUnitHash, SavedUnit)>>> + Send;

    /// Store a single blob in the CAS.
    ///
    /// Returns the key and whether it was newly stored (true) or already
    /// existed (false).
    fn cas_store(&self, key: &Key, content: &[u8]) -> impl Future<Output = Result<bool>> + Send;

    /// Retrieve a single blob from the CAS.
    ///
    /// Returns None if the blob doesn't exist.
    fn cas_get(&self, key: &Key) -> impl Future<Output = Result<Option<Vec<u8>>>> + Send;

    /// Check if a blob exists in the CAS.
    fn cas_exists(&self, key: &Key) -> impl Future<Output = Result<bool>> + Send;

    /// Store multiple blobs in the CAS.
    ///
    /// This is more efficient than calling `cas_store` repeatedly for large
    /// numbers of blobs.
    fn cas_store_bulk(
        &self,
        entries: impl Stream<Item = (Key, Vec<u8>)> + Send + Unpin + 'static,
    ) -> impl Future<Output = Result<BulkStoreResult>> + Send;

    /// Retrieve multiple blobs from the CAS.
    ///
    /// Returns a stream of (key, data) pairs for blobs that exist.
    fn cas_get_bulk(
        &self,
        keys: impl IntoIterator<Item = Key> + Send,
    ) -> impl Future<Output = Result<impl Stream<Item = Result<(Key, Vec<u8>)>> + Send + Unpin>>
           + Send;
}

use std::future::Future;
