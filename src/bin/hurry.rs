use anyhow::anyhow;
use aws_config::BehaviorVersion;
use bytes::buf::Buf;
use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

#[derive(Parser)]
#[command(name = "hurry")]
#[command(about = "A CLI tool with subcommands", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build something quickly
    Build,
}

async fn build() -> anyhow::Result<()> {
    let lock_path = Path::new("Cargo.lock");
    let lock_bytes = fs::read(lock_path)?;
    let mut hasher = Sha256::new();
    hasher.update(&lock_bytes);
    let hash = hasher.finalize();
    let sha_hex = format!("{:x}", hash);
    println!("Cargo.lock SHA256: {}", sha_hex);

    // S3 setup
    let config = aws_config::load_defaults(BehaviorVersion::latest()).await;
    let client = aws_sdk_s3::Client::new(&config);
    let key = format!("{}.tar.bz2", sha_hex);

    // Try to get the object
    let resp = client
        .get_object()
        .bucket("hurry-dev-0")
        .key(&key)
        .send()
        .await;
    let resp = match resp {
        Ok(output) => output,
        Err(e) => {
            // eprintln!("Failed to get object: {e}");
            return Err(anyhow!(
                "Failed to get object: {}",
                e.into_service_error().to_string()
            ));
        }
    };
    let resp = resp.body.collect().await?.reader();

    // Decompress .tar.bz2 into target/
    let target_dir = Path::new("target");
    std::fs::create_dir_all(&target_dir)?;
    let tar = bzip2::read::BzDecoder::new(resp);
    let mut archive = tar::Archive::new(tar);
    archive.unpack(target_dir)?;
    println!("Decompressed artifact to target/");
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Build => {
            if let Err(e) = build().await {
                eprintln!("hurry build error: {e}");
            }
        }
    }
}
