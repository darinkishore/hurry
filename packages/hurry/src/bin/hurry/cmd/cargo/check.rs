//! Runs Cargo check with an optimized cache.
//!
//! Unlike `cargo build`, check mode doesn't have `--build-plan` support,
//! so we use a learn-and-cache approach:
//!
//! 1. Try to restore from cache (if we have known hashes from previous runs)
//! 2. Run `cargo check --message-format=json` to capture artifact information
//! 3. Parse artifacts as they're produced
//! 4. Save new artifacts to cache
//!
//! Reference:
//! - `docs/DESIGN.md`
//! - `docs/development/cargo.md`

use std::time::Duration;

use clap::Args;
use color_eyre::{
    Result, Section as _, SectionExt as _,
    eyre::{Context, OptionExt as _, bail, eyre},
};
use derive_more::Debug;
use tracing::{debug, info, instrument, trace, warn};
use url::Url;
use uuid::Uuid;

use clients::Token;
use hurry::{
    cargo::{self, CargoBuildArguments, CargoCache, Workspace},
    daemon::{CargoUploadStatus, CargoUploadStatusRequest, CargoUploadStatusResponse, DaemonPaths},
    progress::TransferBar,
};

/// Options for `cargo check`.
///
/// Hurry options are prefixed with `hurry-` to disambiguate from `cargo` args.
#[derive(Clone, Args, Debug)]
#[command(disable_help_flag = true)]
pub struct Options {
    /// Base URL for the Hurry API.
    #[arg(
        long = "hurry-api-url",
        env = "HURRY_API_URL",
        default_value = "https://app.hurry.build"
    )]
    #[debug("{api_url}")]
    api_url: Url,

    /// Authentication token for the Hurry API.
    ///
    /// Required for remote caching. Not required when using `--hurry-local`.
    #[arg(long = "hurry-api-token", env = "HURRY_API_TOKEN")]
    api_token: Option<Token>,

    /// Use local cache instead of remote Courier server.
    ///
    /// When enabled, hurry stores build artifacts locally in ~/.cache/hurry/
    /// (or $HURRY_CACHE_DIR if set) instead of uploading to a remote server.
    /// This is useful for solo developers who don't need distributed caching.
    #[arg(long = "hurry-local", env = "HURRY_LOCAL", default_value_t = false)]
    local_mode: bool,

    /// Skip backing up the cache.
    #[arg(long = "hurry-skip-backup", default_value_t = false)]
    skip_backup: bool,

    /// Skip the Cargo check, only performing the cache actions.
    #[arg(long = "hurry-skip-check", default_value_t = false)]
    skip_check: bool,

    /// Skip restoring the cache.
    #[arg(long = "hurry-skip-restore", default_value_t = false)]
    skip_restore: bool,

    /// Upload artifacts asynchronously in the background instead of waiting.
    ///
    /// By default, hurry waits for uploads to complete before exiting.
    /// Use this flag to upload in the background and exit immediately after the
    /// check.
    #[arg(
        long = "hurry-async-upload",
        env = "HURRY_ASYNC_UPLOAD",
        default_value_t = false
    )]
    async_upload: bool,

    /// Show help for `hurry cargo check`.
    #[arg(long = "hurry-help", default_value_t = false)]
    pub help: bool,

    /// These arguments are passed directly to `cargo check` as provided.
    #[arg(
        num_args = ..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS",
    )]
    argv: Vec<String>,
}

impl Options {
    /// Parse the cargo build arguments.
    ///
    /// Note: We reuse CargoBuildArguments since check accepts the same args.
    #[instrument(name = "Options::parsed_args")]
    pub fn parsed_args(&self) -> CargoBuildArguments {
        CargoBuildArguments::from_iter(&self.argv)
    }

    /// Check if help is requested in the arguments.
    pub fn is_help_request(&self) -> bool {
        self.argv
            .iter()
            .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    }
}

#[instrument]
pub async fn exec(options: Options) -> Result<()> {
    // If help is requested, passthrough directly to cargo to show cargo's help
    if options.is_help_request() {
        return cargo::invoke("check", &options.argv).await;
    }

    // Validate API token for remote mode.
    if !options.local_mode && options.api_token.is_none() {
        return Err(eyre!("Hurry API authentication token is required"))
            .suggestion("Set the `HURRY_API_TOKEN` environment variable")
            .suggestion("Provide it with the `--hurry-api-token` argument")
            .suggestion("Or use `--hurry-local` for local-only caching");
    }

    info!(
        "Starting check ({})",
        if options.local_mode { "local mode" } else { "remote mode" }
    );

    // Parse and validate cargo check arguments.
    let args = options.parsed_args();
    debug!(?args, "parsed cargo check arguments");

    // Open workspace.
    let workspace = Workspace::from_argv(&args)
        .await
        .context("opening workspace")?;
    debug!(?workspace, "opened workspace");

    // For check mode, we use the build plan with the same arguments.
    // Cargo's build plan doesn't work with check, but the unit hashes
    // from build mode can help us understand what _might_ be checked.
    // However, check produces different artifacts with different hashes.
    //
    // For now, we compute units using the build plan (which works),
    // then run check and let Cargo do its thing. The cache will store
    // whatever check produces.
    let units = workspace
        .units(&args)
        .await
        .context("calculating expected units")?;

    // Initialize cache based on mode.
    let cache = if options.local_mode {
        CargoCache::open_local(workspace).context("opening local cache")?
    } else {
        let token = options.api_token.as_ref().expect("token validated above");
        CargoCache::open_remote(options.api_url, token.clone(), workspace)
            .await
            .context("opening remote cache")?
    };

    // Restore artifacts.
    let unit_count = units.len() as u64;
    let restored = if !options.skip_restore {
        let progress = TransferBar::new(unit_count, "Restoring cache");
        cache.restore(&units, &progress).await?
    } else {
        Default::default()
    };

    // Run the check.
    if !options.skip_check {
        info!("Running cargo check");

        cargo::invoke("check", &options.argv)
            .await
            .context("check with cargo")?;
    }

    // Cache the checked artifacts.
    if !options.skip_backup {
        let upload_id = cache.save(units, restored).await?;
        // For local mode, saves are synchronous (no daemon), so no need to wait.
        if !cache.is_local() && !options.async_upload {
            let progress = TransferBar::new(unit_count, "Uploading cache");
            wait_for_upload(upload_id, &progress).await?;
        }
    }

    Ok(())
}

#[instrument]
async fn wait_for_upload(request_id: Uuid, progress: &TransferBar) -> Result<()> {
    let paths = DaemonPaths::initialize().await?;
    let Some(daemon) = paths.daemon_running().await? else {
        bail!("daemon is not running");
    };

    let client = reqwest::Client::default();
    let endpoint = format!("http://{}/api/v0/cargo/status", daemon.url);
    let request = CargoUploadStatusRequest { request_id };
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    let mut last_uploaded_artifacts = 0u64;
    let mut last_uploaded_files = 0u64;
    let mut last_uploaded_bytes = 0u64;
    let mut last_total_artifacts = 0u64;
    loop {
        interval.tick().await;
        trace!(?request, "submitting upload status request");
        let response = client
            .post(&endpoint)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("send upload status request to daemon at: {endpoint}"))
            .with_section(|| format!("{daemon:?}").header("Daemon context:"))?;
        trace!(?response, "got upload status response");
        let response = response.json::<CargoUploadStatusResponse>().await?;
        trace!(?response, "parsed upload status response");
        let status = response.status.ok_or_eyre("no upload status")?;
        match status {
            CargoUploadStatus::Complete => break,
            CargoUploadStatus::InProgress(save_progress) => {
                progress.add_bytes(
                    save_progress
                        .uploaded_bytes
                        .saturating_sub(last_uploaded_bytes),
                );
                last_uploaded_bytes = save_progress.uploaded_bytes;
                progress.add_files(
                    save_progress
                        .uploaded_files
                        .saturating_sub(last_uploaded_files),
                );
                last_uploaded_files = save_progress.uploaded_files;
                progress.inc(
                    save_progress
                        .uploaded_units
                        .saturating_sub(last_uploaded_artifacts),
                );
                last_uploaded_artifacts = save_progress.uploaded_units;
                progress.dec_length(last_total_artifacts.saturating_sub(save_progress.total_units));
                last_total_artifacts = save_progress.total_units;
            }
        }
    }

    Ok(())
}
