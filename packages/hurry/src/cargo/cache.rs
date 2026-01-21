//! Cargo build cache management.
//!
//! This module provides caching for Cargo build artifacts. It supports two modes:
//!
//! - **Remote mode**: Uses the Courier HTTP API with background daemon uploads
//! - **Local mode**: Uses local filesystem + SQLite storage (no network required)

use std::{process::Stdio, time::Duration};

use color_eyre::{Result, Section, SectionExt, eyre::Context as _};
use derive_more::Debug;
use serde::{Deserialize, Serialize};
use tracing::{info, instrument, trace};
use url::Url;
use uuid::Uuid;

use crate::{
    cache::{CacheBackend, LocalBackend},
    cargo::{QualifiedPath, UnitPlan, Workspace, host_glibc_version},
    cas::CourierCas,
    daemon::{CargoUploadRequest, DaemonPaths},
    progress::TransferBar,
};
use clients::{Courier, Token};

mod restore;
mod save;

pub use restore::{Restored, restore_units};
pub use save::{SaveProgress, save_units};

/// Backend mode for cache operations.
#[derive(Debug, Clone)]
enum CacheBackendMode {
    /// Remote caching via Courier HTTP API.
    Remote {
        #[debug("{:?}", url.as_str())]
        url: Url,
        token: Token,
        courier: Courier,
        cas: CourierCas,
    },
    /// Local caching via filesystem + SQLite.
    Local { backend: LocalBackend },
}

/// Cargo build cache.
///
/// Provides save/restore operations for Cargo build artifacts. Supports both
/// remote (Courier) and local (filesystem + SQLite) backends.
#[derive(Debug, Clone)]
pub struct CargoCache {
    mode: CacheBackendMode,
    ws: Workspace,
}

impl CargoCache {
    /// Open a remote cache using the Courier HTTP API.
    #[instrument(name = "CargoCache::open_remote", skip(courier_token))]
    pub async fn open_remote(
        courier_url: Url,
        courier_token: Token,
        ws: Workspace,
    ) -> Result<Self> {
        let courier = Courier::new(courier_url.clone(), courier_token.clone())?;
        courier.ping().await.context("ping courier service")?;
        let cas = CourierCas::new(courier.clone());
        Ok(Self {
            mode: CacheBackendMode::Remote {
                url: courier_url,
                token: courier_token,
                courier,
                cas,
            },
            ws,
        })
    }

    /// Open a local cache using the default cache directory.
    ///
    /// Default location: `~/.cache/hurry/` on Linux, `~/Library/Caches/hurry/`
    /// on macOS. Can be overridden with `HURRY_CACHE_DIR` environment variable.
    #[instrument(name = "CargoCache::open_local")]
    pub fn open_local(ws: Workspace) -> Result<Self> {
        let backend = LocalBackend::open_default().context("open local cache backend")?;
        Ok(Self {
            mode: CacheBackendMode::Local { backend },
            ws,
        })
    }

    /// Legacy constructor for backwards compatibility.
    ///
    /// This opens a remote cache. Prefer `open_remote` or `open_local` for clarity.
    #[instrument(name = "CargoCache::open", skip(courier_token))]
    pub async fn open(courier_url: Url, courier_token: Token, ws: Workspace) -> Result<Self> {
        Self::open_remote(courier_url, courier_token, ws).await
    }

    /// Check if this cache is in local mode.
    pub fn is_local(&self) -> bool {
        matches!(self.mode, CacheBackendMode::Local { .. })
    }

    /// Save units to the cache.
    ///
    /// For remote mode: Sends to the background daemon for async upload.
    /// For local mode: Saves directly to local storage.
    ///
    /// Returns a request ID for remote mode (for tracking upload progress),
    /// or a dummy UUID for local mode (since local saves are synchronous).
    #[instrument(name = "CargoCache::save", skip_all)]
    pub async fn save(&self, units: Vec<UnitPlan>, restored: Restored) -> Result<Uuid> {
        match &self.mode {
            CacheBackendMode::Remote { url, token, .. } => {
                self.save_remote(url.clone(), token.clone(), units, restored)
                    .await
            }
            CacheBackendMode::Local { backend } => {
                self.save_local(backend, units, restored).await
            }
        }
    }

    /// Save units via the daemon (remote mode).
    async fn save_remote(
        &self,
        courier_url: Url,
        courier_token: Token,
        units: Vec<UnitPlan>,
        restored: Restored,
    ) -> Result<Uuid> {
        let paths = DaemonPaths::initialize().await?;

        // Start daemon if it's not already running.
        let daemon = if let Some(daemon) = paths.daemon_running().await? {
            daemon
        } else {
            let hurry_binary = std::env::current_exe().context("read current binary path")?;

            let mut cmd = tokio::process::Command::new(hurry_binary);
            cmd.arg("daemon")
                .arg("start")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            cmd.spawn()?;

            const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
            tokio::time::timeout(DAEMON_STARTUP_TIMEOUT, async {
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                loop {
                    interval.tick().await;
                    if let Some(daemon) = paths.daemon_running().await? {
                        break Result::<_>::Ok(daemon);
                    }
                }
            })
            .await
            .context("wait for daemon to start")??
        };

        // Send upload request to daemon.
        let client = reqwest::Client::default();
        let endpoint = format!("http://{}/api/v0/cargo/upload", daemon.url);

        let request_id = Uuid::new_v4();
        let request = CargoUploadRequest {
            request_id,
            courier_url,
            courier_token,
            ws: self.ws.clone(),
            units,
            skip: restored,
        };
        trace!(?request, "submitting upload request");
        let response = client
            .post(&endpoint)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("send upload request to daemon at: {endpoint}"))
            .with_section(|| format!("{daemon:?}").header("Daemon context:"))?;
        trace!(?response, "got upload response");

        Ok(request_id)
    }

    /// Save units directly to local storage.
    async fn save_local(
        &self,
        backend: &LocalBackend,
        units: Vec<UnitPlan>,
        skip: Restored,
    ) -> Result<Uuid> {
        info!("Saving to local cache");

        // For units compiled against glibc, we need to track the version.
        let host_glibc = host_glibc_version()?;

        // Process each unit that wasn't restored from cache.
        for unit in &units {
            let unit_hash = &unit.info().unit_hash;

            if skip.units.contains(unit_hash) {
                trace!(?unit_hash, "skipping unit: was restored from cache");
                continue;
            }

            // Determine target architecture.
            let unit_arch = match &unit.info().target_arch {
                crate::cargo::RustcTarget::Specified(arch) => arch.clone(),
                crate::cargo::RustcTarget::ImplicitHost => self.ws.host_arch.clone(),
            };

            // Determine glibc version if applicable.
            let glibc_version = if unit_arch.uses_glibc() && unit_arch == self.ws.host_arch {
                host_glibc.clone()
            } else {
                None
            };

            // Read unit files and store in CAS.
            let saved_unit = match unit {
                UnitPlan::LibraryCrate(plan) => {
                    let files = plan.read(&self.ws).await?;

                    // Store files in CAS.
                    let mut output_files = Vec::new();
                    for output_file in &files.output_files {
                        let key = clients::courier::v1::Key::from_buffer(&output_file.contents);
                        if !skip.files.contains(&key) {
                            backend.cas_store(&key, &output_file.contents).await?;
                        }
                        output_files.push(
                            clients::courier::v1::SavedFile::builder()
                                .object_key(key)
                                .executable(output_file.executable)
                                .path(serde_json::to_string(&output_file.path)?)
                                .build(),
                        );
                    }

                    let dep_info_contents = serde_json::to_vec(&files.dep_info_file)?;
                    let dep_info_key =
                        clients::courier::v1::Key::from_buffer(&dep_info_contents);
                    if !skip.files.contains(&dep_info_key) {
                        backend.cas_store(&dep_info_key, &dep_info_contents).await?;
                    }

                    let encoded_dep_info_key =
                        clients::courier::v1::Key::from_buffer(&files.encoded_dep_info_file);
                    if !skip.files.contains(&encoded_dep_info_key) {
                        backend
                            .cas_store(&encoded_dep_info_key, &files.encoded_dep_info_file)
                            .await?;
                    }

                    let fingerprint_str = serde_json::to_string(&files.fingerprint)?;

                    clients::courier::v1::SavedUnit::LibraryCrate(
                        clients::courier::v1::LibraryFiles::builder()
                            .output_files(output_files)
                            .dep_info_file(dep_info_key)
                            .encoded_dep_info_file(encoded_dep_info_key)
                            .fingerprint(clients::courier::v1::Fingerprint::new(fingerprint_str))
                            .build(),
                        plan.clone().try_into()?,
                    )
                }
                UnitPlan::BuildScriptCompilation(plan) => {
                    let files = plan.read(&self.ws).await?;

                    let compiled_key =
                        clients::courier::v1::Key::from_buffer(&files.compiled_program);
                    if !skip.files.contains(&compiled_key) {
                        backend
                            .cas_store(&compiled_key, &files.compiled_program)
                            .await?;
                    }

                    let dep_info_contents = serde_json::to_vec(&files.dep_info_file)?;
                    let dep_info_key =
                        clients::courier::v1::Key::from_buffer(&dep_info_contents);
                    if !skip.files.contains(&dep_info_key) {
                        backend.cas_store(&dep_info_key, &dep_info_contents).await?;
                    }

                    let encoded_dep_info_key =
                        clients::courier::v1::Key::from_buffer(&files.encoded_dep_info_file);
                    if !skip.files.contains(&encoded_dep_info_key) {
                        backend
                            .cas_store(&encoded_dep_info_key, &files.encoded_dep_info_file)
                            .await?;
                    }

                    let fingerprint_str = serde_json::to_string(&files.fingerprint)?;

                    clients::courier::v1::SavedUnit::BuildScriptCompilation(
                        clients::courier::v1::BuildScriptCompiledFiles::builder()
                            .compiled_program(compiled_key)
                            .dep_info_file(dep_info_key)
                            .encoded_dep_info_file(encoded_dep_info_key)
                            .fingerprint(clients::courier::v1::Fingerprint::new(fingerprint_str))
                            .build(),
                        plan.clone().try_into()?,
                    )
                }
                UnitPlan::BuildScriptExecution(plan) => {
                    let files = plan.read(&self.ws).await?;

                    let mut out_dir_files = Vec::new();
                    for file in &files.out_dir_files {
                        let key = clients::courier::v1::Key::from_buffer(&file.contents);
                        if !skip.files.contains(&key) {
                            backend.cas_store(&key, &file.contents).await?;
                        }
                        out_dir_files.push(
                            clients::courier::v1::SavedFile::builder()
                                .object_key(key)
                                .executable(file.executable)
                                .path(serde_json::to_string(&file.path)?)
                                .build(),
                        );
                    }

                    let stdout_contents = serde_json::to_vec(&files.stdout)?;
                    let stdout_key = clients::courier::v1::Key::from_buffer(&stdout_contents);
                    if !skip.files.contains(&stdout_key) {
                        backend.cas_store(&stdout_key, &stdout_contents).await?;
                    }

                    let stderr_key = clients::courier::v1::Key::from_buffer(&files.stderr);
                    if !skip.files.contains(&stderr_key) {
                        backend.cas_store(&stderr_key, &files.stderr).await?;
                    }

                    let fingerprint_str = serde_json::to_string(&files.fingerprint)?;

                    clients::courier::v1::SavedUnit::BuildScriptExecution(
                        clients::courier::v1::BuildScriptOutputFiles::builder()
                            .out_dir_files(out_dir_files)
                            .stdout(stdout_key)
                            .stderr(stderr_key)
                            .fingerprint(clients::courier::v1::Fingerprint::new(fingerprint_str))
                            .build(),
                        plan.clone().try_into()?,
                    )
                }
            };

            // Save metadata to SQLite.
            let glibc_v1 = glibc_version.map(|g| clients::courier::v1::GlibcVersion {
                major: g.major,
                minor: g.minor,
                patch: g.patch,
            });
            backend
                .cargo_save([(
                    clients::courier::v1::SavedUnitHash::new(String::from(unit_hash.clone())),
                    saved_unit,
                    unit_arch.as_str().to_string(),
                    glibc_v1,
                )])
                .await?;
        }

        // Return a dummy UUID since local saves are synchronous.
        Ok(Uuid::nil())
    }

    /// Restore units from the cache.
    #[instrument(name = "CargoCache::restore", skip_all)]
    pub async fn restore(&self, units: &Vec<UnitPlan>, progress: &TransferBar) -> Result<Restored> {
        match &self.mode {
            CacheBackendMode::Remote { courier, cas, .. } => {
                restore_units(courier, cas, &self.ws, units, progress).await
            }
            CacheBackendMode::Local { backend } => {
                self.restore_local(backend, units, progress).await
            }
        }
    }

    /// Restore units from local storage.
    ///
    /// For now, this is a no-op that just updates the progress bar. Full local
    /// cache restore (downloading from local CAS + SQLite metadata) is a future
    /// enhancement.
    ///
    /// Note: We intentionally return an empty `Restored` so that `save_local`
    /// will save all newly built units. If we marked units as "restored" just
    /// because their fingerprint files exist on disk (from previous non-hurry
    /// builds), we'd skip saving them to the local cache.
    async fn restore_local(
        &self,
        _backend: &LocalBackend,
        units: &[UnitPlan],
        progress: &TransferBar,
    ) -> Result<Restored> {
        info!("Checking local cache (restore not yet implemented)");

        // Just update progress for now.
        progress.inc(units.len() as u64);

        // Return empty - we'll save everything after the build.
        // TODO: Implement actual restore from local CAS + SQLite.
        Ok(Restored::default())
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedFile {
    pub path: QualifiedPath,
    pub contents: Vec<u8>,
    pub executable: bool,
}
