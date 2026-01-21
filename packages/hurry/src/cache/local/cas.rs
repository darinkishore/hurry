//! Local filesystem-based content-addressed storage.
//!
//! This module provides a local CAS implementation that stores blobs on the
//! filesystem with zstd compression. It uses the same structure as Courier's
//! Disk storage: `{root}/{key[0..2]}/{key[2..4]}/{key}`.

use std::io::Cursor;
use std::path::PathBuf;

use async_compression::Level;
use async_compression::tokio::bufread::ZstdDecoder;
use async_compression::tokio::write::ZstdEncoder;
use color_eyre::{
    Result,
    eyre::{Context, bail},
};
use derive_more::{Debug, Display};
use tokio::fs::{File, create_dir_all, metadata, remove_file, rename};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{instrument, warn};
use uuid::Uuid;

use clients::courier::v1::Key;

/// Default buffer size for read/write operations.
const DEFAULT_BUF_SIZE: usize = 64 * 1024;

/// Local content-addressed storage backed by the filesystem.
///
/// ## File structure
///
/// The CAS uses a two-level directory structure where each file is named by the
/// hex-encoded blake3 hash of its content. Files are prefixed with two levels
/// of folders computed from the first four characters of the hex hash.
///
/// ## Compression
///
/// All content is transparently compressed with zstd. Users write uncompressed
/// data and read back the same uncompressed data.
///
/// ## Atomic writes
///
/// Writes use a temp-file-then-rename pattern to ensure atomicity.
#[derive(Clone, Eq, PartialEq, Debug, Display)]
#[debug("LocalCas(root = {})", self.root.display())]
#[display("{}", root.display())]
pub struct LocalCas {
    root: PathBuf,
}

impl LocalCas {
    /// Create a new instance with the given root directory.
    ///
    /// The directory will be created when the first file is written.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Get the path to the file for the given key.
    ///
    /// Example: `Key("abcd1234...")` -> `root/ab/cd/abcd1234...`
    fn key_path(&self, key: &Key) -> PathBuf {
        let hex = key.to_hex();
        let prefix1 = hex.chars().take(2).collect::<String>();
        let prefix2 = hex.chars().skip(2).take(2).collect::<String>();
        self.root.join(prefix1).join(prefix2).join(&hex)
    }

    /// Check if a blob exists in storage.
    #[instrument(name = "LocalCas::exists")]
    pub async fn exists(&self, key: &Key) -> Result<bool> {
        let path = self.key_path(key);
        match metadata(&path).await {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err).context(format!("check if blob exists at {path:?}")),
        }
    }

    /// Read content from storage for the provided key.
    #[instrument(name = "LocalCas::read")]
    pub async fn read(&self, key: &Key) -> Result<impl AsyncRead + Unpin + 'static> {
        let path = self.key_path(key);
        File::open(&path)
            .await
            .map(BufReader::new)
            .map(ZstdDecoder::new)
            .map(|reader| BufReader::with_capacity(DEFAULT_BUF_SIZE, reader))
            .with_context(|| format!("open blob file {:?}", path))
    }

    /// Read and buffer the entire content from storage.
    #[instrument(name = "LocalCas::read_buffered")]
    pub async fn read_buffered(&self, key: &Key) -> Result<Option<Vec<u8>>> {
        let path = self.key_path(key);
        match File::open(&path).await {
            Ok(file) => {
                let reader = BufReader::new(file);
                let decoder = ZstdDecoder::new(reader);
                let mut buffered = BufReader::with_capacity(DEFAULT_BUF_SIZE, decoder);

                let mut buffer = Vec::new();
                tokio::io::copy(&mut buffered, &mut buffer)
                    .await
                    .context("read decompressed blob content")?;
                Ok(Some(buffer))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).context(format!("open blob file {path:?}")),
        }
    }

    /// Write content to storage for the provided key.
    ///
    /// Returns `true` if the content was newly written, `false` if it already
    /// existed.
    #[instrument(name = "LocalCas::write", skip(content))]
    pub async fn write(&self, key: &Key, content: &[u8]) -> Result<bool> {
        let path = self.key_path(key);

        // Check if it already exists
        if self.exists(key).await? {
            return Ok(false);
        }

        // Create parent directories
        if let Some(parent) = path.parent() {
            create_dir_all(parent)
                .await
                .with_context(|| format!("create parent directory {parent:?}"))?;
        }

        // Write to temp file first
        let temp = temp_path(&path);
        let file = File::create(&temp).await.context("create temporary file")?;

        // Compress and write
        let mut encoder = ZstdEncoder::with_quality(file, Level::Default);
        let (hash, _size) = hashed_copy(&mut Cursor::new(content), &mut encoder)
            .await
            .with_context(|| format!("write content to {temp:?}"))?;

        encoder.shutdown().await.context("flush zstd encoder")?;
        let mut file = encoder.into_inner();
        file.flush().await.context("flush file")?;
        drop(file);

        // Verify hash
        if key != hash {
            if let Err(err) = remove_file(&temp).await {
                warn!("failed to remove temp file {temp:?}: {err}");
            }
            bail!("hash mismatch: {hash:?} != {key:?}");
        }

        // Atomic rename
        match rename(&temp, &path).await {
            Ok(()) => Ok(true),
            Err(err) => {
                if let Err(err) = remove_file(&temp).await {
                    warn!("failed to remove temp file {temp:?}: {err}");
                }
                if err.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(false)
                } else {
                    Err(err).context(format!("rename {temp:?} to {path:?}"))
                }
            }
        }
    }
}

/// Generate a temporary file path in the same directory as the target.
fn temp_path(target: &std::path::Path) -> PathBuf {
    let mut temp = target.as_os_str().to_owned();
    temp.push(".tmp.");
    temp.push(Uuid::new_v4().to_string());
    PathBuf::from(temp)
}

/// Copy content while computing the blake3 hash.
async fn hashed_copy(
    mut source: impl AsyncRead + Unpin,
    mut target: impl tokio::io::AsyncWrite + Unpin,
) -> Result<(blake3::Hash, u64)> {
    let mut buffer = vec![0; 16 * 1024];
    let mut hasher = blake3::Hasher::new();
    let mut copied = 0;

    loop {
        let n = source.read(&mut buffer).await.context("read source")?;
        if n == 0 {
            break;
        }

        let chunk = &buffer[..n];
        hasher.update(chunk);
        target.write_all(chunk).await.context("write target")?;
        copied += n as u64;
    }

    Ok((hasher.finalize(), copied))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq as pretty_assert_eq;

    #[tokio::test]
    async fn round_trip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cas = LocalCas::new(temp_dir.path());

        let content = b"hello world";
        let key = Key::from_buffer(content);

        // Write
        let was_new = cas.write(&key, content).await.unwrap();
        pretty_assert_eq!(was_new, true);

        // Write again should return false
        let was_new = cas.write(&key, content).await.unwrap();
        pretty_assert_eq!(was_new, false);

        // Read
        let read_content = cas.read_buffered(&key).await.unwrap().unwrap();
        pretty_assert_eq!(read_content, content);
    }

    #[tokio::test]
    async fn missing_key() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cas = LocalCas::new(temp_dir.path());

        let key = Key::from_buffer(b"nonexistent");

        let exists = cas.exists(&key).await.unwrap();
        pretty_assert_eq!(exists, false);

        let content = cas.read_buffered(&key).await.unwrap();
        pretty_assert_eq!(content, None);
    }
}
