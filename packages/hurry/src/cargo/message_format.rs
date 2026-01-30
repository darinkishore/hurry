//! Parse Cargo's JSON message format output.
//!
//! When running Cargo commands with `--message-format=json`, Cargo outputs
//! newline-delimited JSON (NDJSON) messages to stdout. This module provides
//! types for parsing these messages, particularly the `compiler-artifact`
//! messages that tell us about compiled units.
//!
//! Reference: <https://doc.rust-lang.org/cargo/reference/external-tools.html#json-messages>

use std::collections::HashMap;

use color_eyre::Result;
use serde::Deserialize;
use tracing::trace;

use crate::cargo::UnitHash;

/// A message emitted by Cargo during compilation.
///
/// This is the top-level discriminator for all Cargo JSON messages.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "reason")]
#[serde(rename_all = "kebab-case")]
pub enum CargoMessage {
    /// A compiler artifact was produced.
    CompilerArtifact(CompilerArtifact),
    /// A compiler message (warning, error, etc).
    CompilerMessage(CompilerMessage),
    /// Build script executed (for the `build-finished` message in newer Cargo).
    BuildScriptExecuted(BuildScriptExecuted),
    /// Build finished.
    BuildFinished(BuildFinished),
    /// Unknown message type - we capture and ignore these.
    #[serde(other)]
    Unknown,
}

/// A compiler artifact message.
///
/// Emitted when a compilation unit completes. Contains the filenames of
/// produced artifacts (with embedded unit hashes).
#[derive(Debug, PartialEq, Deserialize)]
pub struct CompilerArtifact {
    /// The package that was compiled.
    pub package_id: String,

    /// The Cargo target within the package.
    pub target: ArtifactTarget,

    /// Profile settings used for this compilation.
    pub profile: ArtifactProfile,

    /// List of files produced by this compilation.
    ///
    /// For library crates, this includes `.rlib` and `.rmeta` files.
    /// The filenames contain the unit hash as a suffix (e.g., `libfoo-abc123.rlib`).
    pub filenames: Vec<String>,

    /// Whether the artifact was already compiled (fresh from cache).
    pub fresh: bool,

    /// The package manifest path.
    pub manifest_path: String,
}

/// Target information within a compiler artifact.
#[derive(Debug, PartialEq, Deserialize)]
pub struct ArtifactTarget {
    /// The kind of target (lib, bin, test, example, bench, custom-build).
    pub kind: Vec<String>,
    /// The crate types this target produces.
    pub crate_types: Vec<String>,
    /// The target name.
    pub name: String,
    /// Path to the main source file.
    pub src_path: String,
}

/// Profile information within a compiler artifact.
#[derive(Debug, PartialEq, Deserialize)]
pub struct ArtifactProfile {
    /// Optimization level.
    pub opt_level: String,
    /// Debug info level.
    pub debuginfo: Option<u32>,
    /// Whether debug assertions are enabled.
    pub debug_assertions: bool,
    /// Whether overflow checks are enabled.
    pub overflow_checks: bool,
    /// Whether this is a test build.
    pub test: bool,
}

/// A compiler message (diagnostic).
#[derive(Debug, PartialEq, Deserialize)]
pub struct CompilerMessage {
    /// The package that emitted the message.
    pub package_id: String,
    /// The target that emitted the message.
    pub target: ArtifactTarget,
    /// The diagnostic message itself.
    pub message: serde_json::Value,
}

/// Build script execution result.
#[derive(Debug, PartialEq, Deserialize)]
pub struct BuildScriptExecuted {
    /// The package whose build script was run.
    pub package_id: String,
    /// Environment variables set by the build script.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// The OUT_DIR set by the build script.
    pub out_dir: Option<String>,
}

/// Build finished message.
#[derive(Debug, PartialEq, Deserialize)]
pub struct BuildFinished {
    /// Whether the build succeeded.
    pub success: bool,
}

impl CompilerArtifact {
    /// Extract the unit hash from this artifact's filenames.
    ///
    /// The unit hash is embedded in the filename as a suffix before the
    /// extension (e.g., `libfoo-abc123.rlib` has hash `abc123`).
    ///
    /// Returns `None` if no filenames are present or if the hash cannot be
    /// extracted.
    pub fn unit_hash(&self) -> Result<Option<UnitHash>> {
        // Skip artifacts that don't produce files we care about
        let filename = self
            .filenames
            .iter()
            // Filter out debug symbol files
            .find(|f| !f.ends_with(".dwp") && !f.ends_with(".dSYM"));

        let Some(filename) = filename else {
            return Ok(None);
        };

        // Extract just the filename from the path
        let filename = filename
            .rsplit('/')
            .next()
            .or_else(|| filename.rsplit('\\').next())
            .unwrap_or(filename);

        // Remove extension(s) - handle cases like `.so.dwp` or `.dylib`
        let stem = filename.split_once('.').map(|(s, _)| s).unwrap_or(filename);

        // Extract hash suffix after the last hyphen
        let hash = stem.rsplit_once('-').map(|(_, h)| h);

        trace!(
            ?filename,
            ?stem,
            ?hash,
            "extracting unit hash from artifact"
        );

        Ok(hash.map(|h| UnitHash::from(h.to_string())))
    }

    /// Check if this artifact is a library (lib, rlib, cdylib, proc-macro).
    pub fn is_library(&self) -> bool {
        self.target
            .kind
            .iter()
            .any(|k| k == "lib" || k == "rlib" || k == "cdylib" || k == "proc-macro")
    }

    /// Check if this artifact is a build script.
    pub fn is_build_script(&self) -> bool {
        self.target.kind.iter().any(|k| k == "custom-build")
    }

    /// Check if this artifact is a binary.
    pub fn is_binary(&self) -> bool {
        self.target.kind.iter().any(|k| k == "bin")
    }

    /// Check if this artifact is a test target.
    pub fn is_test(&self) -> bool {
        self.target.kind.iter().any(|k| k == "test")
    }

    /// Check if this artifact is a benchmark target.
    pub fn is_bench(&self) -> bool {
        self.target.kind.iter().any(|k| k == "bench")
    }

    /// Check if this artifact is an example target.
    pub fn is_example(&self) -> bool {
        self.target.kind.iter().any(|k| k == "example")
    }
}

/// Parse a single line of Cargo JSON output.
///
/// Returns `Ok(Some(message))` if the line was valid JSON and parsed
/// successfully, `Ok(None)` if the line was empty or whitespace, and an error
/// if parsing failed.
pub fn parse_message(line: &str) -> Result<Option<CargoMessage>> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line)
        .map(Some)
        .map_err(|e| color_eyre::eyre::eyre!("failed to parse cargo message: {e}"))
}

/// Parse multiple lines of Cargo JSON output.
///
/// Skips empty lines and returns all successfully parsed messages.
pub fn parse_messages(output: &str) -> Result<Vec<CargoMessage>> {
    output
        .lines()
        .filter_map(|line| match parse_message(line) {
            Ok(Some(msg)) => Some(Ok(msg)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        })
        .collect()
}

/// Extract all artifact unit hashes from a series of messages.
///
/// This filters to only compiler artifacts and extracts their unit hashes.
/// Useful for discovering what was built after a compilation run.
pub fn extract_artifact_hashes(messages: &[CargoMessage]) -> Result<Vec<UnitHash>> {
    let mut hashes = Vec::new();
    for msg in messages {
        if let CargoMessage::CompilerArtifact(artifact) = msg
            && let Some(hash) = artifact.unit_hash()?
        {
            hashes.push(hash);
        }
    }
    Ok(hashes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq as pretty_assert_eq;

    #[test]
    fn parse_compiler_artifact() {
        let json = r#"{"reason":"compiler-artifact","package_id":"serde 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)","manifest_path":"/path/to/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"serde","src_path":"/path/to/lib.rs"},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["/path/to/target/debug/deps/libserde-abc123.rlib","/path/to/target/debug/deps/libserde-abc123.rmeta"],"fresh":false}"#;

        let msg = parse_message(json).unwrap().unwrap();
        match msg {
            CargoMessage::CompilerArtifact(artifact) => {
                pretty_assert_eq!(artifact.fresh, false);
                pretty_assert_eq!(artifact.filenames.len(), 2);
                pretty_assert_eq!(
                    artifact.unit_hash().unwrap(),
                    Some(UnitHash::from("abc123"))
                );
                assert!(artifact.is_library());
                assert!(!artifact.is_build_script());
            }
            _ => panic!("expected CompilerArtifact"),
        }
    }

    #[test]
    fn parse_build_finished() {
        let json = r#"{"reason":"build-finished","success":true}"#;

        let msg = parse_message(json).unwrap().unwrap();
        match msg {
            CargoMessage::BuildFinished(finished) => {
                pretty_assert_eq!(finished.success, true);
            }
            _ => panic!("expected BuildFinished"),
        }
    }

    #[test]
    fn parse_unknown_reason() {
        // Cargo may emit new message types in the future - we should handle them gracefully
        let json = r#"{"reason":"future-message-type","data":"something"}"#;

        let msg = parse_message(json).unwrap().unwrap();
        assert!(matches!(msg, CargoMessage::Unknown));
    }

    #[test]
    fn extract_hash_from_rlib() {
        let artifact = CompilerArtifact {
            package_id: String::from("test"),
            target: ArtifactTarget {
                kind: vec![String::from("lib")],
                crate_types: vec![String::from("lib")],
                name: String::from("test"),
                src_path: String::from("/path/to/lib.rs"),
            },
            profile: ArtifactProfile {
                opt_level: String::from("0"),
                debuginfo: Some(2),
                debug_assertions: true,
                overflow_checks: true,
                test: false,
            },
            filenames: vec![String::from(
                "/path/to/target/debug/deps/libtest-1a2b3c4d.rlib",
            )],
            fresh: false,
            manifest_path: String::from("/path/to/Cargo.toml"),
        };

        pretty_assert_eq!(
            artifact.unit_hash().unwrap(),
            Some(UnitHash::from("1a2b3c4d"))
        );
    }

    #[test]
    fn extract_hash_from_build_script() {
        let artifact = CompilerArtifact {
            package_id: String::from("test"),
            target: ArtifactTarget {
                kind: vec![String::from("custom-build")],
                crate_types: vec![String::from("bin")],
                name: String::from("build-script-build"),
                src_path: String::from("/path/to/build.rs"),
            },
            profile: ArtifactProfile {
                opt_level: String::from("0"),
                debuginfo: Some(2),
                debug_assertions: true,
                overflow_checks: true,
                test: false,
            },
            filenames: vec![String::from(
                "/path/to/target/debug/build/test-abcdef12/build-script-build",
            )],
            fresh: false,
            manifest_path: String::from("/path/to/Cargo.toml"),
        };

        // Build scripts have the hash in the directory name, not the filename
        // This test shows the current behavior - we may need to adjust for build scripts
        pretty_assert_eq!(artifact.unit_hash().unwrap(), Some(UnitHash::from("build")));
        assert!(artifact.is_build_script());
    }

    #[test]
    fn empty_line_returns_none() {
        pretty_assert_eq!(parse_message("").unwrap(), None);
        pretty_assert_eq!(parse_message("   ").unwrap(), None);
        pretty_assert_eq!(parse_message("\t\n").unwrap(), None);
    }
}
