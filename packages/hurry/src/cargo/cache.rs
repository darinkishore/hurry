//! Cargo build cache management.
//!
//! This module provides caching for Cargo build artifacts. It supports two modes:
//!
//! - **Remote mode**: Uses the Courier HTTP API with background daemon uploads
//! - **Local mode**: Uses local filesystem + SQLite storage (no network required)

use std::{
    collections::{HashMap, HashSet},
    process::Stdio,
    time::{Duration, SystemTime},
};

use color_eyre::{Result, Section, SectionExt, eyre::Context as _};
use derive_more::Debug;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, trace, warn};
use url::Url;
use uuid::Uuid;

use crate::{
    cache::{CacheBackend, LocalBackend},
    cargo::{self, Fingerprint, QualifiedPath, UnitHash, UnitPlan, Workspace, host_glibc_version},
    cas::CourierCas,
    daemon::{CargoUploadRequest, DaemonPaths},
    fs,
    path::JoinWith as _,
    progress::TransferBar,
};
use clients::courier::v1::SavedUnit;
use clients::{Courier, Token};

mod restore;
mod save;

use restore::filter_units_with_incomplete_deps;
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
            CacheBackendMode::Local { backend } => self.save_local(backend, units, restored).await,
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
                    let dep_info_key = clients::courier::v1::Key::from_buffer(&dep_info_contents);
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
                    let dep_info_key = clients::courier::v1::Key::from_buffer(&dep_info_contents);
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
    /// Queries SQLite for cached units, fetches files from local CAS, and writes
    /// them to the target directory with proper fingerprint rewriting.
    async fn restore_local(
        &self,
        backend: &LocalBackend,
        units: &[UnitPlan],
        progress: &TransferBar,
    ) -> Result<Restored> {
        info!("Restoring from local cache");

        let restored = Restored::default();

        // Check which units are already on disk, and don't attempt to restore them.
        let mut units_to_skip = HashSet::new();
        for unit in units {
            let info = unit.info();
            if fs::exists(
                &self
                    .ws
                    .unit_profile_dir(info)
                    .join(unit.fingerprint_json_file()?),
            )
            .await
            {
                units_to_skip.insert(info.unit_hash.clone());
                debug!(
                    unit_hash = ?info.unit_hash,
                    pkg_name = %info.package_name,
                    "skipping unit: already fresh locally"
                );
                restored.units.insert(info.unit_hash.clone());
            }
        }

        // Get host glibc version for compatibility filtering.
        let host_glibc_symbol_version = host_glibc_version()?;
        debug!(
            ?host_glibc_symbol_version,
            "restore starting with host glibc"
        );

        // Query all unit hashes (including skipped) to get fingerprint data.
        // We need fingerprints for skipped units to populate dep_fingerprints.
        let unit_hashes = units
            .iter()
            .map(|u| {
                clients::courier::v1::SavedUnitHash::new(String::from(u.info().unit_hash.clone()))
            })
            .collect::<Vec<_>>();

        let host_glibc_v1 = host_glibc_symbol_version.map(|g| clients::courier::v1::GlibcVersion {
            major: g.major,
            minor: g.minor,
            patch: g.patch,
        });

        let saved_units_vec = backend.cargo_restore(unit_hashes, host_glibc_v1).await?;

        // Convert to a HashMap for easier lookup.
        let mut saved_units = saved_units_vec
            .into_iter()
            .map(|(hash, unit)| (UnitHash::from(hash.as_str()), unit))
            .collect::<HashMap<_, _>>();

        info!(
            requested_count = units.len(),
            returned_count = saved_units.len(),
            "local cache restore response"
        );

        // Filter units with incomplete dependency chains.
        // Create a temporary CargoRestoreResponse-like structure for the filter function.
        let saved_units_for_filter = saved_units
            .iter()
            .map(|(k, v)| {
                (
                    clients::courier::v1::SavedUnitHash::new(String::from(k.clone())),
                    v.clone(),
                )
            })
            .collect::<clients::courier::v1::cache::CargoRestoreResponse>();

        let (units_with_incomplete_deps, incomplete_deps_count) =
            filter_units_with_incomplete_deps(units, &saved_units_for_filter, &units_to_skip);

        if incomplete_deps_count > 0 {
            warn!(
                incomplete_deps_count,
                "filtered units with incomplete dependency chains"
            );
        }

        // Track fingerprint mappings for rewriting.
        let mut dep_fingerprints = HashMap::new();

        // Mtime management: start from UNIX_EPOCH to avoid dirtying first-party builds.
        let starting_mtime = SystemTime::UNIX_EPOCH;

        for (i, unit) in units.iter().enumerate() {
            let unit_hash = &unit.info().unit_hash;

            // Calculate mtime - increment by 1 second per unit for ordering.
            let mtime = starting_mtime + Duration::from_secs(i as u64);

            if units_with_incomplete_deps.contains(unit_hash) {
                progress.dec_length(1);
                continue;
            }

            // Get the saved unit from cache.
            let Some(saved) = saved_units.remove(unit_hash) else {
                debug!(
                    ?unit_hash,
                    pkg_name = %unit.info().package_name,
                    "unit missing from local cache"
                );

                // Touch skipped units to maintain mtime invariants.
                if units_to_skip.contains(unit_hash)
                    && let Err(err) = unit.touch(&self.ws, starting_mtime).await
                {
                    warn!(?unit_hash, ?err, "could not set mtime for skipped unit");
                }
                progress.dec_length(1);
                continue;
            };

            // Parse the cached fingerprint.
            let cached_fingerprint = saved.fingerprint().as_str();
            let cached_fingerprint = serde_json::from_str::<Fingerprint>(cached_fingerprint)?;

            // Handle skipped units - just record fingerprint mapping and touch mtime.
            if units_to_skip.contains(unit_hash) {
                let profile = self.ws.unit_profile_dir(unit.info());
                let cached_hash = cached_fingerprint.hash_u64();

                let file = unit.fingerprint_json_file()?;
                let file = profile.join(&file);
                let json = fs::must_read_buffered_utf8(&file).await?;
                let local = serde_json::from_str::<Fingerprint>(&json)?;
                let local_hash = local.hash_u64();

                debug!(
                    ?cached_hash,
                    ?local_hash,
                    "recorded fingerprint mapping for skipped unit"
                );

                dep_fingerprints.insert(cached_hash, local);

                if let Err(err) = unit.touch(&self.ws, mtime).await {
                    warn!(?unit_hash, ?err, "could not set mtime for skipped unit");
                }
                progress.dec_length(1);
                continue;
            }

            // Rewrite fingerprint and write it.
            let info = unit.info();
            let src_path = unit.src_path().map(|p| p.into());
            let rewritten_fingerprint =
                cached_fingerprint.rewrite(src_path, &mut dep_fingerprints)?;
            let fingerprint_hash = rewritten_fingerprint.fingerprint_hash();

            let profile_dir = self.ws.unit_profile_dir(info);
            fs::write(
                &profile_dir.join(&unit.fingerprint_hash_file()?),
                fingerprint_hash,
            )
            .await?;
            fs::write(
                &profile_dir.join(&unit.fingerprint_json_file()?),
                serde_json::to_vec(&rewritten_fingerprint)?,
            )
            .await?;

            // Restore files from local CAS based on unit type.
            match (saved, unit) {
                (
                    SavedUnit::LibraryCrate(saved_library_files, _),
                    UnitPlan::LibraryCrate(unit_plan),
                ) => {
                    trace!(
                        pkg_name = %unit_plan.info.package_name,
                        unit_hash = %unit_plan.info.unit_hash,
                        num_output_files = saved_library_files.output_files.len(),
                        "restoring library crate unit from local cache"
                    );

                    // Restore output files.
                    for file in saved_library_files.output_files {
                        let path: QualifiedPath = serde_json::from_str(file.path.as_str())?;
                        let path = path.reconstruct(&self.ws, &unit_plan.info).try_into()?;

                        let data = backend.cas_get(&file.object_key).await?.ok_or_else(|| {
                            color_eyre::eyre::eyre!(
                                "missing CAS key for output file: {}",
                                file.object_key
                            )
                        })?;

                        fs::write(&path, data).await?;
                        fs::set_executable(&path, file.executable).await?;
                        fs::set_mtime(&path, mtime).await?;

                        restored.files.insert(file.object_key);
                        progress.add_files(1);
                    }

                    // Restore dep-info file with reconstruction.
                    let dep_info_data = backend
                        .cas_get(&saved_library_files.dep_info_file)
                        .await?
                        .ok_or_else(|| {
                            color_eyre::eyre::eyre!("missing CAS key for dep-info file")
                        })?;
                    let dep_info: cargo::DepInfo = serde_json::from_slice(&dep_info_data)?;
                    let dep_info = dep_info.reconstruct(&self.ws, &unit_plan.info);
                    let path = profile_dir.join(&unit_plan.dep_info_file()?);
                    fs::write(&path, dep_info).await?;
                    fs::set_mtime(&path, mtime).await?;
                    restored.files.insert(saved_library_files.dep_info_file);

                    // Restore encoded dep-info file (no transformation).
                    let encoded_dep_info_data = backend
                        .cas_get(&saved_library_files.encoded_dep_info_file)
                        .await?
                        .ok_or_else(|| {
                            color_eyre::eyre::eyre!("missing CAS key for encoded dep-info file")
                        })?;
                    let path = profile_dir.join(&unit_plan.encoded_dep_info_file()?);
                    fs::write(&path, encoded_dep_info_data).await?;
                    fs::set_mtime(&path, mtime).await?;
                    restored
                        .files
                        .insert(saved_library_files.encoded_dep_info_file);
                }
                (
                    SavedUnit::BuildScriptCompilation(build_script_compiled_files, _),
                    UnitPlan::BuildScriptCompilation(unit_plan),
                ) => {
                    debug!(
                        pkg_name = %unit_plan.info.package_name,
                        unit_hash = %unit_plan.info.unit_hash,
                        "restoring build script compilation unit from local cache"
                    );

                    // Restore compiled program.
                    let compiled_data = backend
                        .cas_get(&build_script_compiled_files.compiled_program)
                        .await?
                        .ok_or_else(|| {
                            color_eyre::eyre::eyre!("missing CAS key for compiled program")
                        })?;

                    let path = profile_dir.join(unit_plan.program_file()?);
                    let linked_path = profile_dir.join(unit_plan.linked_program_file()?);

                    fs::write(&path, compiled_data).await?;
                    fs::set_executable(&path, true).await?;
                    fs::set_mtime(&path, mtime).await?;
                    fs::hard_link(&path, &linked_path).await?;
                    fs::set_mtime(&linked_path, mtime).await?;
                    restored
                        .files
                        .insert(build_script_compiled_files.compiled_program);

                    // Restore dep-info file with reconstruction.
                    let dep_info_data = backend
                        .cas_get(&build_script_compiled_files.dep_info_file)
                        .await?
                        .ok_or_else(|| {
                            color_eyre::eyre::eyre!("missing CAS key for dep-info file")
                        })?;
                    let dep_info: cargo::DepInfo = serde_json::from_slice(&dep_info_data)?;
                    let dep_info = dep_info.reconstruct(&self.ws, &unit_plan.info);
                    let path = profile_dir.join(&unit_plan.dep_info_file()?);
                    fs::write(&path, dep_info).await?;
                    fs::set_mtime(&path, mtime).await?;
                    restored
                        .files
                        .insert(build_script_compiled_files.dep_info_file);

                    // Restore encoded dep-info file (no transformation).
                    let encoded_dep_info_data = backend
                        .cas_get(&build_script_compiled_files.encoded_dep_info_file)
                        .await?
                        .ok_or_else(|| {
                            color_eyre::eyre::eyre!("missing CAS key for encoded dep-info file")
                        })?;
                    let path = profile_dir.join(&unit_plan.encoded_dep_info_file()?);
                    fs::write(&path, encoded_dep_info_data).await?;
                    fs::set_mtime(&path, mtime).await?;
                    restored
                        .files
                        .insert(build_script_compiled_files.encoded_dep_info_file);
                }
                (
                    SavedUnit::BuildScriptExecution(build_script_output_files, _),
                    UnitPlan::BuildScriptExecution(unit_plan),
                ) => {
                    let out_dir = unit_plan.out_dir()?;
                    let out_dir_absolute = profile_dir.join(&out_dir);

                    debug!(
                        pkg_name = %unit_plan.info.package_name,
                        unit_hash = %unit_plan.info.unit_hash,
                        out_dir = %out_dir,
                        num_out_dir_files = build_script_output_files.out_dir_files.len(),
                        "restoring build script execution unit from local cache"
                    );

                    // Create OUT_DIR directory.
                    fs::create_dir_all(&out_dir_absolute).await?;

                    // Restore all OUT_DIR files.
                    for file in build_script_output_files.out_dir_files {
                        let path: QualifiedPath = serde_json::from_str(file.path.as_str())?;
                        let path = path.reconstruct(&self.ws, &unit_plan.info).try_into()?;

                        let data = backend.cas_get(&file.object_key).await?.ok_or_else(|| {
                            color_eyre::eyre::eyre!(
                                "missing CAS key for OUT_DIR file: {}",
                                file.object_key
                            )
                        })?;

                        debug!(
                            pkg_name = %unit_plan.info.package_name,
                            file_path = %AsRef::<std::path::Path>::as_ref(&path).display(),
                            "restoring build script OUT_DIR file"
                        );

                        fs::write(&path, data).await?;
                        fs::set_executable(&path, file.executable).await?;
                        fs::set_mtime(&path, mtime).await?;
                        restored.files.insert(file.object_key);
                        progress.add_files(1);
                    }

                    // Restore stdout with BuildScriptOutput reconstruction.
                    let stdout_data = backend
                        .cas_get(&build_script_output_files.stdout)
                        .await?
                        .ok_or_else(|| color_eyre::eyre::eyre!("missing CAS key for stdout"))?;
                    let stdout: cargo::BuildScriptOutput = serde_json::from_slice(&stdout_data)?;
                    let stdout = stdout.reconstruct(&self.ws, &unit_plan.info);
                    let path = profile_dir.join(&unit_plan.stdout_file()?);
                    fs::write(&path, stdout).await?;
                    fs::set_mtime(&path, mtime).await?;
                    restored.files.insert(build_script_output_files.stdout);

                    // Restore stderr (no transformation).
                    let stderr_data = backend
                        .cas_get(&build_script_output_files.stderr)
                        .await?
                        .ok_or_else(|| color_eyre::eyre::eyre!("missing CAS key for stderr"))?;
                    let path = profile_dir.join(&unit_plan.stderr_file()?);
                    fs::write(&path, stderr_data).await?;
                    fs::set_mtime(&path, mtime).await?;
                    restored.files.insert(build_script_output_files.stderr);

                    // Generate root-output file.
                    let root_output_path = profile_dir.join(&unit_plan.root_output_file()?);
                    fs::write(
                        &root_output_path,
                        out_dir_absolute.as_os_str().as_encoded_bytes(),
                    )
                    .await?;
                    fs::set_mtime(&root_output_path, mtime).await?;
                }
                _ => {
                    return Err(color_eyre::eyre::eyre!("unit type mismatch"));
                }
            }

            // Mark the unit as restored.
            debug!(?unit, "marking unit as restored");
            restored.units.insert(unit_hash.clone());
            progress.inc(1);
        }

        Ok(restored)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedFile {
    pub path: QualifiedPath,
    pub contents: Vec<u8>,
    pub executable: bool,
}
