//! Host libc detection for cache compatibility.
//!
//! This module provides functions to detect the libc version of the current
//! host system. This information is used to ensure that cached build artifacts
//! are only restored onto compatible systems.

use clients::courier::v1::cache::LibcVersion;
use color_eyre::Result;
use tracing::debug;

/// Detect the libc version of the current host system.
///
/// Returns `LibcVersion::Unknown` if detection fails, which will result in
/// conservative caching behavior (only compatible with other Unknown hosts).
pub fn detect_host_libc() -> LibcVersion {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        detect_glibc().unwrap_or_else(|err| {
            debug!(?err, "failed to detect glibc version, using Unknown");
            LibcVersion::Unknown
        })
    }

    #[cfg(all(target_os = "linux", target_env = "musl"))]
    {
        LibcVersion::Musl
    }

    #[cfg(target_os = "macos")]
    {
        detect_darwin().unwrap_or_else(|err| {
            debug!(?err, "failed to detect Darwin version, using Unknown");
            LibcVersion::Unknown
        })
    }

    #[cfg(target_os = "windows")]
    {
        LibcVersion::Windows
    }

    #[cfg(not(any(
        all(target_os = "linux", target_env = "gnu"),
        all(target_os = "linux", target_env = "musl"),
        target_os = "macos",
        target_os = "windows"
    )))]
    {
        LibcVersion::Unknown
    }
}

/// Detect glibc version using the `gnu_get_libc_version` function.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn detect_glibc() -> Result<LibcVersion> {
    use color_eyre::eyre::{Context, bail};
    use std::ffi::CStr;

    // SAFETY: gnu_get_libc_version returns a pointer to a static string
    // that is valid for the lifetime of the program.
    let version_ptr = unsafe { libc::gnu_get_libc_version() };
    if version_ptr.is_null() {
        bail!("gnu_get_libc_version returned null");
    }

    // SAFETY: The pointer is non-null and points to a valid C string.
    let version_str = unsafe { CStr::from_ptr(version_ptr) }
        .to_str()
        .context("glibc version is not valid UTF-8")?;

    debug!(version = %version_str, "detected glibc version");

    // Parse version string like "2.31" or "2.17"
    let parts = version_str.split('.').collect::<Vec<_>>();
    if parts.len() < 2 {
        bail!("unexpected glibc version format: {version_str}");
    }

    let major = parts[0]
        .parse::<u32>()
        .context("failed to parse glibc major version")?;
    let minor = parts[1]
        .parse::<u32>()
        .context("failed to parse glibc minor version")?;

    Ok(LibcVersion::Glibc { major, minor })
}

/// Detect macOS deployment target using `rustc --print deployment-target`.
///
/// This queries rustc directly for the deployment target it will use when
/// compiling. This is more accurate than using the Darwin kernel version
/// because:
/// 1. It respects the `MACOSX_DEPLOYMENT_TARGET` environment variable
/// 2. It uses rustc's built-in minimums (e.g., 11.0 for aarch64)
/// 3. It matches what actually gets embedded in the compiled binaries
///
/// The version returned is the macOS version (e.g., 11.0 for Big Sur, 14.0
/// for Sonoma), not the Darwin kernel version.
#[cfg(target_os = "macos")]
fn detect_darwin() -> Result<LibcVersion> {
    use color_eyre::eyre::{Context, bail};
    use std::process::Command;

    // Run rustc to get the deployment target
    let output = Command::new("rustc")
        .args(["--print", "deployment-target"])
        .output()
        .context("failed to run rustc --print deployment-target")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("rustc --print deployment-target failed: {stderr}");
    }

    let stdout = String::from_utf8(output.stdout).context("rustc output is not valid UTF-8")?;

    // Output format: "MACOSX_DEPLOYMENT_TARGET=11.0\n"
    let line = stdout.trim();
    debug!(output = %line, "rustc deployment target output");

    let version_str = line
        .strip_prefix("MACOSX_DEPLOYMENT_TARGET=")
        .ok_or_else(|| color_eyre::eyre::eyre!("unexpected rustc output format: {line}"))?;

    // Parse version string like "11.0" or "14.0"
    let parts = version_str.split('.').collect::<Vec<_>>();
    if parts.is_empty() {
        bail!("unexpected macOS version format: {version_str}");
    }

    let major = parts[0]
        .parse::<u32>()
        .context("failed to parse macOS major version")?;
    let minor = parts
        .get(1)
        .map(|s| s.parse::<u32>())
        .transpose()
        .context("failed to parse macOS minor version")?
        .unwrap_or(0);

    debug!(major, minor, "detected macOS deployment target");

    Ok(LibcVersion::Darwin { major, minor })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_host_libc_returns_valid_version() {
        let version = detect_host_libc();
        // Should detect something - never returns Unknown on supported platforms
        // when detection succeeds
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        {
            match version {
                LibcVersion::Glibc { major, minor } => {
                    assert!(major >= 2, "glibc major version should be >= 2");
                    assert!(
                        minor < 100,
                        "glibc minor version should be reasonable (<100)"
                    );
                }
                _ => panic!("expected Glibc on Linux GNU, got {version:?}"),
            }
        }

        #[cfg(all(target_os = "linux", target_env = "musl"))]
        {
            assert!(
                matches!(version, LibcVersion::Musl),
                "expected Musl on Linux musl"
            );
        }

        #[cfg(target_os = "macos")]
        {
            match version {
                LibcVersion::Darwin { major, minor } => {
                    // macOS deployment target should be >= 10 (we use macOS version now)
                    // aarch64 minimum is 11.0, x86_64 minimum is 10.12
                    assert!(major >= 10, "macOS deployment target should be >= 10");
                    assert!(
                        minor < 100,
                        "macOS minor version should be reasonable (<100)"
                    );
                }
                _ => panic!("expected Darwin on macOS, got {version:?}"),
            }
        }

        #[cfg(target_os = "windows")]
        {
            assert!(
                matches!(version, LibcVersion::Windows),
                "expected Windows on Windows"
            );
        }
    }
}
