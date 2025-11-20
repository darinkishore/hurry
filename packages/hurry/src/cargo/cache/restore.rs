use color_eyre::{
    Result,
    eyre::{OptionExt as _, bail},
};
use dashmap::DashSet;
use derive_more::Debug;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::task::JoinSet;
use tracing::{debug, instrument, trace, warn};

use crate::{
    cargo::{self, QualifiedPath, UnitPlan, Workspace, cache, workspace::UnitHash},
    cas::CourierCas,
    fs,
    path::{AbsFilePath, JoinWith as _},
    progress::TransferBar,
};
use clients::{
    Courier,
    courier::v1::{
        Key, SavedUnit,
        cache::{CargoRestoreRequest2, SavedUnitCacheKey},
    },
};

/// Tracks items that were restored from the cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Restored {
    /// Stores the unit hashes of restored units.
    pub units: DashSet<UnitHash>,
    pub files: DashSet<Key>,
}

impl Restored {
    /// Records that an artifact was restored from cache.
    fn record_unit(&self, unit_hash: UnitHash) {
        self.units.insert(unit_hash);
    }

    /// Records that an object was restored from cache.
    fn record_file(&self, key: Key) {
        self.files.insert(key);
    }
}

#[derive(Debug, Clone)]
struct FileRestoreKey {
    path: AbsFilePath,
    key: Key,
    transform: fn(Vec<u8>) -> Vec<u8>,
}

#[instrument(skip(units, progress))]
pub async fn restore_units(
    courier: &Courier,
    cas: &CourierCas,
    ws: &Workspace,
    units: &Vec<UnitPlan>,
    // artifact_plan: &ArtifactPlan,
    progress: &TransferBar,
) -> Result<Restored> {
    trace!(?units, "units");

    let restored = Restored::default();

    // TODO: Check which units are already fresh on disk, and don't attempt to
    // restore them.

    // Load unit information from remote cache. Note that this does NOT download
    // the actual files, which are loaded as CAS keys.
    let bulk_req = CargoRestoreRequest2::new(units.iter().map(|unit| {
        SavedUnitCacheKey::builder()
            .unit_hash(unit.info().unit_hash.clone())
            .build()
    }));
    let mut saved_units = courier.cargo_cache_restore2(bulk_req).await?;

    // Spawn concurrent workers for doing parallel downloads.
    let (tx, mut workers) = {
        let worker_count = num_cpus::get();
        let (tx, rx) = flume::bounded::<FileRestoreKey>(0);
        let mut workers = JoinSet::new();
        for _ in 0..worker_count {
            let rx = rx.clone();
            let cas = cas.clone();
            let progress = progress.clone();
            let restored = restored.clone();
            workers.spawn(restore_worker(rx, cas, progress, restored));
        }
        (tx, workers)
    };

    let mut dep_fingerprints = HashMap::new();
    let mut files_to_restore = Vec::<FileRestoreKey>::new();
    for unit in units {
        let unit_hash = unit.info().unit_hash.clone();

        // Load the saved file info from the response.
        let saved = saved_units.take(
            &SavedUnitCacheKey::builder()
                .unit_hash(unit_hash.clone())
                .build(),
        );
        let Some(saved) = saved else {
            debug!(?unit_hash, "unit missing from cache");
            progress.dec_length(1);
            continue;
        };

        // Write the fingerprint. This happens during this loop because
        // fingerprint rewriting must occur in dependency order.
        //
        // TODO: Ideally, we would only write fingerprints _after_ all the files
        // for the unit are restored, to be maximally correct. This requires
        // plumbing some sort of work-dependency relationship between units and
        // restores.
        match (saved, unit) {
            (
                SavedUnit::LibraryCrate(saved_library_files, _),
                UnitPlan::LibraryCrate(unit_plan),
            ) => {
                // Restore the fingerprint directly, because fingerprint
                // rewriting needs to occur in dependency order.
                let fingerprint: cargo::Fingerprint =
                    serde_json::from_str(saved_library_files.fingerprint.as_str())?;
                cache::LibraryFiles::restore_fingerprint(
                    ws,
                    &mut dep_fingerprints,
                    fingerprint,
                    unit_plan,
                )
                .await?;

                // Queue the output files.
                for file in saved_library_files.output_files {
                    let path: QualifiedPath = serde_json::from_str(file.path.as_str())?;
                    files_to_restore.push(FileRestoreKey {
                        path: path
                            .reconstruct(ws, &unit_plan.info.target_arch)?
                            .try_into()?,
                        key: file.object_key.clone(),
                        transform: |data| data,
                    });
                }

                let profile_dir = ws.unit_profile_dir(&unit_plan.info);

                // Queue the dep-info file.
                files_to_restore.push(FileRestoreKey {
                    path: profile_dir.join(&unit_plan.dep_info_file()?),
                    key: saved_library_files.dep_info_file.clone(),
                    transform: |data| data,
                });

                // Queue the encoded dep-info file.
                files_to_restore.push(FileRestoreKey {
                    path: profile_dir.join(&unit_plan.encoded_dep_info_file()?),
                    key: saved_library_files.encoded_dep_info_file.clone(),
                    transform: |data| data,
                });
            }
            (
                SavedUnit::BuildScriptCompilation(
                    build_script_compiled_files,
                    build_script_compilation_unit_plan,
                ),
                UnitPlan::BuildScriptCompilation(unit_plan),
            ) => todo!(),
            (
                SavedUnit::BuildScriptExecution(
                    build_script_output_files,
                    build_script_execution_unit_plan,
                ),
                UnitPlan::BuildScriptExecution(unit_plan),
            ) => todo!(),
            _ => bail!("unit type mismatch"),
        }

        // Queue the other files in the unit to be batch downloaded and
        // restored.

        // Mark the unit as restored. It's not _technically_ restored yet, but
        // this function will return an error if the restore doesn't happen
        // anyway.
        restored.record_unit(unit_hash);
        // TODO: Ideally we would not increment this until the restore is
        // actually finished, but we haven't plumbed that through yet.
        progress.inc(1);
    }

    for file in files_to_restore {
        tx.send_async(file).await?;
        progress.add_files(1);
    }
    drop(tx);

    while let Some(worker) = workers.join_next().await {
        worker??;
    }

    Ok(restored)
}

async fn restore_worker(
    rx: flume::Receiver<FileRestoreKey>,
    cas: CourierCas,
    progress: TransferBar,
    restored: Restored,
) -> Result<()> {
    const BATCH_SIZE: usize = 50;
    let mut batch = Vec::new();
    while let Ok(file) = rx.recv_async().await {
        trace!(?file, "worker got file");

        // Add the file to the batch.
        batch.push(file);
        // If the batch is not full, wait for the batch to fill.
        if batch.len() < BATCH_SIZE {
            continue;
        }

        // Restore batches once full.
        restore_batch(batch.clone(), &cas, &progress, &restored).await?;

        // Clear the restored batch.
        batch.clear();
    }

    // Once the channel closes, there may still be a partially filled batch
    // remaining. Restore the remaining files in the batch.

    Ok(())
}

async fn restore_batch(
    batch: Vec<FileRestoreKey>,
    cas: &CourierCas,
    progress: &TransferBar,
    restored: &Restored,
) -> Result<()> {
    // Construct a map of object keys to file restore keys.
    let mut key_to_file_restore = batch
        .clone()
        .into_iter()
        .map(|file| (file.key.clone(), file))
        .collect::<HashMap<_, _>>();

    // For each streamed CAS key, restore the file to the local filesystem.
    let mut res = cas.get_bulk(batch.into_iter().map(|file| file.key)).await?;
    while let Some(result) = res.next().await {
        match result {
            Ok((key, data)) => {
                let file = key_to_file_restore
                    .remove(&key)
                    .ok_or_eyre("unrecognized key from CAS bulk response")?;
                restored.record_file(file.key);

                progress.add_files(1);
                progress.add_bytes(data.len() as u64);
                let data = (file.transform)(data);
                fs::write(&file.path, data).await?;
            }
            Err(error) => {
                warn!(?error, "failed to fetch file from CAS");
            }
        }
    }

    Ok(())
}
