//! Courier-based cache backend implementation.
//!
//! This module provides a `CacheBackend` implementation that uses the remote
//! Courier HTTP API for distributed caching. It wraps the existing `Courier`
//! and `CourierCas` clients.

use color_eyre::Result;
use derive_more::Debug;
use futures::Stream;
use tracing::instrument;
use url::Url;

use clients::{
    Courier, Token,
    courier::v1::{
        GlibcVersion, Key, SavedUnit, SavedUnitHash,
        cache::{CargoRestoreRequest, CargoSaveRequest, CargoSaveUnitRequest},
    },
};

use super::backend::{BulkStoreResult, CacheBackend};

/// Cache backend implementation using the remote Courier HTTP API.
///
/// This wraps the existing `Courier` client to implement the `CacheBackend`
/// trait, enabling the same code paths to work with both remote and local
/// storage.
#[derive(Clone, Debug)]
pub struct CourierBackend {
    #[debug("{:?}", client)]
    client: Courier,
}

impl CourierBackend {
    /// Create a new CourierBackend with the given URL and token.
    #[instrument(name = "CourierBackend::new", skip(token))]
    pub async fn new(url: Url, token: Token) -> Result<Self> {
        let client = Courier::new(url, token)?;
        client.ping().await?;
        Ok(Self { client })
    }

    /// Create a new CourierBackend from an existing Courier client.
    pub fn from_client(client: Courier) -> Self {
        Self { client }
    }
}

impl CacheBackend for CourierBackend {
    #[instrument(name = "CourierBackend::cargo_save", skip_all)]
    async fn cargo_save(
        &self,
        units: impl IntoIterator<Item = (SavedUnitHash, SavedUnit, String, Option<GlibcVersion>)>
            + Send,
    ) -> Result<()> {
        let requests = units
            .into_iter()
            .map(|(_hash, unit, resolved_target, glibc_version)| {
                CargoSaveUnitRequest::builder()
                    .unit(unit)
                    .resolved_target(resolved_target)
                    .maybe_linux_glibc_version(glibc_version)
                    .build()
            })
            .collect::<Vec<_>>();

        self.client
            .cargo_cache_save(CargoSaveRequest::new(requests))
            .await?;

        Ok(())
    }

    #[instrument(name = "CourierBackend::cargo_restore", skip_all)]
    async fn cargo_restore(
        &self,
        unit_hashes: impl IntoIterator<Item = SavedUnitHash> + Send,
        host_glibc_version: Option<GlibcVersion>,
    ) -> Result<Vec<(SavedUnitHash, SavedUnit)>> {
        let request = CargoRestoreRequest::new(unit_hashes, host_glibc_version);
        let response = self.client.cargo_cache_restore(request).await?;

        Ok(response.into_iter().collect())
    }

    #[instrument(name = "CourierBackend::cas_store", skip(content))]
    async fn cas_store(&self, key: &Key, content: &[u8]) -> Result<bool> {
        // Check if it exists first
        if self.client.cas_exists(key).await.unwrap_or(false) {
            return Ok(false);
        }

        self.client.cas_write_bytes(key, content.to_vec()).await?;
        Ok(true)
    }

    #[instrument(name = "CourierBackend::cas_get")]
    async fn cas_get(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        self.client.cas_read_bytes(key).await
    }

    #[instrument(name = "CourierBackend::cas_exists")]
    async fn cas_exists(&self, key: &Key) -> Result<bool> {
        self.client.cas_exists(key).await
    }

    #[instrument(name = "CourierBackend::cas_store_bulk", skip(entries))]
    async fn cas_store_bulk(
        &self,
        entries: impl Stream<Item = (Key, Vec<u8>)> + Send + Unpin + 'static,
    ) -> Result<BulkStoreResult> {
        let response = self.client.cas_write_bulk(entries).await?;

        Ok(BulkStoreResult {
            written: response.written,
            skipped: response.skipped,
            errors: response
                .errors
                .into_iter()
                .map(|item| (item.key, item.error))
                .collect(),
        })
    }

    #[instrument(name = "CourierBackend::cas_get_bulk", skip(keys))]
    async fn cas_get_bulk(
        &self,
        keys: impl IntoIterator<Item = Key> + Send,
    ) -> Result<impl Stream<Item = Result<(Key, Vec<u8>)>> + Send + Unpin> {
        self.client.cas_read_bulk(keys).await
    }
}
