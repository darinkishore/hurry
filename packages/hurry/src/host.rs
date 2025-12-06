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

/// Detect Darwin (macOS) version using uname.
#[cfg(target_os = "macos")]
fn detect_darwin() -> Result<LibcVersion> {
    use color_eyre::eyre::{Context, bail};
    use std::ffi::CStr;
    use std::mem::MaybeUninit;

    // Use uname to get the Darwin kernel version
    let mut utsname = MaybeUninit::<libc::utsname>::uninit();

    // SAFETY: uname writes to the provided buffer and returns 0 on success
    let result = unsafe { libc::uname(utsname.as_mut_ptr()) };
    if result != 0 {
        bail!("uname failed with result: {result}");
    }

    // SAFETY: uname succeeded, so utsname is now initialized
    let utsname = unsafe { utsname.assume_init() };

    // SAFETY: release is a null-terminated C string filled by uname
    let release = unsafe { CStr::from_ptr(utsname.release.as_ptr()) }
        .to_str()
        .context("Darwin release is not valid UTF-8")?;

    debug!(release = %release, "detected Darwin release");

    // Parse version string like "24.6.0" (Darwin version)
    let parts = release.split('.').collect::<Vec<_>>();
    if parts.len() < 2 {
        bail!("unexpected Darwin version format: {release}");
    }

    let major = parts[0]
        .parse::<u32>()
        .context("failed to parse Darwin major version")?;
    let minor = parts[1]
        .parse::<u32>()
        .context("failed to parse Darwin minor version")?;

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
                    // Darwin 20+ corresponds to macOS 11+
                    assert!(major >= 15, "Darwin version should be >= 15 (macOS 10.11+)");
                    assert!(
                        minor < 100,
                        "Darwin minor version should be reasonable (<100)"
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
