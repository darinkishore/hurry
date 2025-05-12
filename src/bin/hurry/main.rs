use anyhow::anyhow;
use aws_config::BehaviorVersion;
use bytes::buf::Buf;
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use tracing::{debug, instrument};
use tracing_subscriber::{
    fmt::format::FmtSpan, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

mod cargo;

#[derive(Parser)]
#[command(name = "hurry")]
#[command(about = "Really, really fast builds", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Subcommand)]
enum Command {
    /// Fast `cargo` builds
    #[command(dont_delimit_trailing_values = true)]
    Cargo {
        // #[command(subcommand)]
        // command: Option<CargoCommand>,
        #[arg(
            num_args = ..,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            // dont_delimit_trailing_values = true,
        )]
        argv: Vec<String>,
    },
    /// Manage remote authentication
    Auth,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_file(true)
                .with_line_number(true)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_writer(std::io::stderr)
                .pretty(),
        )
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Cargo { argv } => {
            debug!(?argv, "cargo");

            // TODO: Technically, we should parse the argv properly in case
            // this string is passed as some sort of configuration flag value.
            if argv.contains(&"build".to_string()) {
                match cargo::build(argv) {
                    Ok(_) => {}
                    Err(e) => panic!("hurry cargo build failed: {}", e),
                }
            } else {
                match cargo::exec(argv).await {
                    Ok(_) => {}
                    Err(e) => panic!("hurry cargo exec failed: {}", e),
                }
            }
        }
        _ => {
            eprintln!("unimplemented");
        }
    }
}

#[instrument]
async fn build() -> anyhow::Result<()> {
    // Read the lockfile.
    let lock_path = Path::new("Cargo.lock");
    let lock_bytes = fs::read(lock_path)?;
    let mut hasher = Sha256::new();
    hasher.update(&lock_bytes);
    let hash = hasher.finalize();
    let sha_hex = format!("{:x}", hash);
    println!("Cargo.lock SHA256: {}", sha_hex);

    // Connect to S3.
    let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
    let client = aws_sdk_s3::Client::new(&config);

    // Try to find the cached target directory for this lockfile.
    let key = format!("{}.tar.bz2", sha_hex);
    let resp = client
        .get_object()
        .bucket("hurry-dev-0")
        .key(&key)
        .send()
        .await;
    match resp {
        Ok(cached) => {
            // Download and decompress the cached object.
            let download = cached.body.collect().await?.reader();
            let tar = bzip2::read::BzDecoder::new(download);
            let mut archive = tar::Archive::new(tar);

            // Decompress cache into target/
            let target_dir = Path::new("target");
            std::fs::create_dir_all(&target_dir)?;
            archive.unpack(target_dir)?;
            println!("Decompressed artifact to target/");
        }
        Err(e) => match e {
            // If the cache doesn't exist, continue.
            aws_smithy_runtime_api::client::result::SdkError::ServiceError(e) => {
                let e = e.into_err();
                if !e.is_no_such_key() {
                    return Err(anyhow!("Failed to get object: {}", e.to_string()));
                }
            }
            // For all other errors, crash.
            _ => return Err(anyhow!("Failed to get object: {}", e.to_string())),
        },
    }

    // Run the build.
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("build");
    cmd.spawn()?.wait()?;

    // Compress target/ into a .tar.bz2.
    // println!("Compressing target");
    // let mut buf = Vec::new();
    // {
    //     let target_dir = Path::new("target");
    //     let tar = bzip2::write::BzEncoder::new(&mut buf, bzip2::Compression::default());
    //     let mut archive = tar::Builder::new(tar);
    //     archive.append_dir_all("target", target_dir)?;
    //     archive.finish()?;
    // }
    // println!("Compressed target");

    // // Upload the cache.
    // let lock_path = Path::new("Cargo.lock");
    // let lock_bytes = fs::read(lock_path)?;
    // let mut hasher = Sha256::new();
    // hasher.update(&lock_bytes);
    // let hash = hasher.finalize();
    // let sha_hex = format!("{:x}", hash);
    // println!("Cargo.lock SHA256: {}", sha_hex);
    // let key = format!("{}.tar.bz2", sha_hex);
    // let resp = client
    //     .put_object()
    //     .bucket("hurry-dev-0")
    //     .key(&key)
    //     .body(ByteStream::from(buf))
    //     .send()
    //     .await?;

    Ok(())
}
