use std::process::ExitStatus;

use git2::Repository;
use tracing::{instrument, trace};

#[instrument]
pub fn build(argv: Vec<String>) -> anyhow::Result<()> {
    // Open and parse the git repository.
    let repo = Repository::open(".")?;

    // Identify the current HEAD of the repository.
    let head = repo.head()?;
    trace!(kind = ?head.kind(), name = ?head.name());

    // Check whether the current workspace target is already a Hurry cache. If
    // not, initialize the Hurry cache for this project, initializing the global
    // Hurry cache if necessary.

    // The cache has an "active" reference, a target folder layout, and a CAS of
    // compiled artifacts indexed by a SQLite database. If the current git
    // reference is different from the cache's active reference, we should
    // restore the cache of the active reference if one exists.

    // Run the build.

    // Snapshot the state of the cache post-build to be the new state for the
    // active reference.


    return Ok(());
}

#[instrument]
pub async fn exec(argv: Vec<String>) -> anyhow::Result<ExitStatus> {
    let mut cmd = std::process::Command::new("cargo");
    cmd.args(argv);
    Ok(cmd.spawn()?.wait()?)
}
