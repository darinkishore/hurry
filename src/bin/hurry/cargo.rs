use std::{fs, os::unix, path::PathBuf, process::ExitStatus};

use git2::Repository;
use homedir::my_home;
use tracing::{instrument, trace};

#[instrument]
pub fn build(argv: Vec<String>) -> anyhow::Result<()> {
    // Open and parse the git repository.
    let repo = Repository::open(".")?;

    // Identify the current HEAD of the repository.
    let head = repo.head()?;
    trace!(kind = ?head.kind(), name = ?head.name());

    // Check whether the current workspace target is already a Hurry cache. If
    // not, initialize the Hurry cache for this project, initializing the system
    // Hurry cache if necessary.
    let system_cache_root = ensure_workspace_target_hurry_cache()?;

    // The cache has an "active" reference, a set of target folders, and a CAS
    // of compiled artifacts indexed by a SQLite database. If the current git
    // reference is different from the cache's active reference, we should
    // restore the cache of the active reference if one exists.

    // Run the build.

    // Snapshot the state of the cache post-build to be the new state for the
    // active reference.

    Ok(())
}

#[instrument]
pub async fn exec(argv: Vec<String>) -> anyhow::Result<ExitStatus> {
    let mut cmd = std::process::Command::new("cargo");
    cmd.args(argv);
    Ok(cmd.spawn()?.wait()?)
}


#[instrument]
fn ensure_system_hurry_cache() -> anyhow::Result<PathBuf> {
    // The location of the system Hurry cache. Later, we might make this
    // configurable.
    let system_cache_root = {
        let mut path = my_home()?.unwrap();
        path.push(".cache");
        path.push("hurry");
        path.push("v1");
        path.push("cargo");
        path.canonicalize()?
    };

    // Check whether the system Hurry cache folder exists, and create it if it
    // doesn't.
    if !fs::exists(&system_cache_root)? {
        fs::create_dir_all(&system_cache_root)?;
    }

    Ok(system_cache_root)
}

#[instrument]
fn ensure_workspace_target_hurry_cache() -> anyhow::Result<PathBuf> {
    let system_cache_root = ensure_system_hurry_cache()?;
    let target_path = {
        let mut path = std::env::current_dir()?;
        path.push("target");
        path.canonicalize()?
    };
    let target_cache_path = {
        let mut path = system_cache_root.join("target");
        path.push(
            blake3::hash(target_path.as_os_str().as_encoded_bytes())
                .to_hex()
                .as_str(),
        );
        path
    };

    // Check whether the current workspace target points to a valid Hurry cache.
    if !(fs::exists(&target_path)?
        && fs::exists(&target_cache_path)?
        && fs::symlink_metadata(&target_path)?.file_type().is_symlink()
        && fs::read_link(&target_path)? == target_cache_path)
    {
        // The current workspace target does not point to a Hurry cache. We
        // should create a new Hurry cache for this workspace target.
        fs::create_dir_all(&target_cache_path)?;
        unix::fs::symlink(&target_cache_path, &target_path)?;
    }

    Ok(system_cache_root)
}
