//! Cargo cache API types.

use std::collections::{HashMap, HashSet};

use bon::Builder;
use derive_more::From;
use serde::{Deserialize, Serialize};
use tap::Pipe;

use crate::courier::v1::{SavedUnit, SavedUnitHash};

/// Libc version information for cache compatibility checking.
///
/// This type represents the libc implementation and version used when building
/// cached artifacts. It's used to ensure that cached binaries (build scripts,
/// proc-macros) are only restored onto compatible systems.
///
/// ## Compatibility Rules
///
/// Compatibility is determined by whether the current host can run binaries
/// built against the cached libc version:
///
/// - Glibc: Forward-compatible. A binary built against glibc 2.17 can run on
///   glibc 2.31, but NOT vice versa. The host version must be >= cached
///   version.
/// - Darwin: Forward-compatible. A binary targeting macOS 11.0 deployment
///   target can run on macOS 14.0, but NOT vice versa. Uses the rustc
///   deployment target, not the Darwin kernel version.
/// - Musl: Typically statically linked, so version is less critical.
/// - Windows: No libc compatibility concerns for this use case.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LibcVersion {
    /// GNU libc with major.minor version (e.g., 2.31).
    Glibc { major: u32, minor: u32 },

    /// musl libc. Version tracking is less critical since musl is typically
    /// statically linked.
    Musl,

    /// macOS with deployment target version (e.g., 11.0 for Big Sur, 14.0 for
    /// Sonoma). This is the macOS version, not the Darwin kernel version.
    /// Determined by `rustc --print deployment-target`.
    Darwin { major: u32, minor: u32 },

    /// Windows. No libc compatibility concerns for cached Rust artifacts.
    Windows,

    /// Unknown or unsupported libc. Artifacts with this libc version will
    /// only restore on hosts that also report Unknown.
    Unknown,
}

impl LibcVersion {
    /// Check if the current host (self) can run binaries built for the given
    /// required libc version.
    ///
    /// Returns `true` if `self` (the host) is compatible with `required` (the
    /// version the artifact was built against).
    ///
    /// ## Compatibility Rules
    ///
    /// - Same libc type required (can't run glibc binaries on musl)
    /// - For versioned libcs (Glibc, Darwin): host version must be >= required
    /// - For Musl: always compatible with other Musl
    /// - For Windows: always compatible with other Windows
    /// - Unknown: only compatible with Unknown
    pub fn can_run(&self, required: &LibcVersion) -> bool {
        match (self, required) {
            // Glibc: host version must be >= required version
            (
                LibcVersion::Glibc {
                    major: host_major,
                    minor: host_minor,
                },
                LibcVersion::Glibc {
                    major: req_major,
                    minor: req_minor,
                },
            ) => (host_major, host_minor) >= (req_major, req_minor),

            // Darwin: host version must be >= required version
            (
                LibcVersion::Darwin {
                    major: host_major,
                    minor: host_minor,
                },
                LibcVersion::Darwin {
                    major: req_major,
                    minor: req_minor,
                },
            ) => (host_major, host_minor) >= (req_major, req_minor),

            // Musl: compatible with any musl
            (LibcVersion::Musl, LibcVersion::Musl) => true,

            // Windows: compatible with any Windows
            (LibcVersion::Windows, LibcVersion::Windows) => true,

            // Unknown: only compatible with Unknown
            (LibcVersion::Unknown, LibcVersion::Unknown) => true,

            // Different libc types are never compatible
            _ => false,
        }
    }

    /// Returns a stable string representation for hashing.
    fn stable_repr(&self) -> String {
        match self {
            LibcVersion::Glibc { major, minor } => format!("glibc:{major}.{minor}"),
            LibcVersion::Musl => String::from("musl"),
            LibcVersion::Darwin { major, minor } => format!("darwin:{major}.{minor}"),
            LibcVersion::Windows => String::from("windows"),
            LibcVersion::Unknown => String::from("unknown"),
        }
    }
}

impl std::fmt::Display for LibcVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LibcVersion::Glibc { major, minor } => write!(f, "glibc {major}.{minor}"),
            LibcVersion::Musl => write!(f, "musl"),
            LibcVersion::Darwin { major, minor } => write!(f, "macOS {major}.{minor}"),
            LibcVersion::Windows => write!(f, "Windows"),
            LibcVersion::Unknown => write!(f, "unknown"),
        }
    }
}

/// Compound cache key for `SavedUnit`.
///
/// Cache keys include the unit hash and libc version to ensure that cached
/// artifacts are only restored onto compatible systems. The libc version is
/// particularly important for build scripts and proc-macros, which are
/// executables that must run on the host system.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, Builder)]
#[non_exhaustive]
pub struct SavedUnitCacheKey {
    /// The key generation, used to invalidate keys that no longer point to the
    /// same content.
    #[builder(skip = SavedUnitCacheKey::GENERATION)]
    generation: u8,

    /// `SavedUnit` instances are primarily keyed by their hash.
    #[builder(into)]
    pub unit_hash: SavedUnitHash,

    /// The libc version of the host where the unit was built.
    ///
    /// This is used to ensure that cached binaries (build scripts, proc-macros)
    /// are only restored onto hosts with compatible libc versions.
    #[builder(into)]
    pub libc_version: LibcVersion,
}

impl SavedUnitCacheKey {
    /// The current generation for the cache key.
    ///
    /// This exists so that if we _semantically_ change how the cache works
    /// without actually changing how the cache key is generated (so e.g. the
    /// same key means something different than it used to mean, or holds
    /// different content) we can increment the generation to force a change
    /// to the key.
    ///
    /// Generation history:
    /// - 1: Initial generation (unit_hash only)
    /// - 2: Added libc_version to cache key for platform compatibility
    const GENERATION: u8 = 2;

    /// Construct a single opaque string representing the cache key.
    ///
    /// The contents of this string should be treated as opaque: its format may
    /// change at any time. The only guaranteed quality of the returned value is
    /// that it will always be the same if the contents of the
    /// `SavedUnitCacheKey` instance are the same, and always different if the
    /// contents are different.
    ///
    /// Note: this is meant to be similar to a derived `Hash` implementation,
    /// but stable across compiler versions and platforms.
    pub fn stable_hash(&self) -> String {
        // When we add new fields, this will show a compile time error; if you got here
        // due to a compilation error please handle the new field(s) appropriately.
        let Self {
            unit_hash,
            generation,
            libc_version,
        } = self;
        let mut hasher = blake3::Hasher::new();
        hasher.update(format!("{generation}").as_bytes());
        hasher.update(unit_hash.as_str().as_bytes());
        hasher.update(libc_version.stable_repr().as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    /// Returns the libc version associated with this cache key.
    pub fn libc_version(&self) -> &LibcVersion {
        &self.libc_version
    }
}

impl AsRef<SavedUnitHash> for SavedUnitCacheKey {
    fn as_ref(&self) -> &SavedUnitHash {
        &self.unit_hash
    }
}

impl From<&SavedUnitCacheKey> for SavedUnitCacheKey {
    fn from(key: &SavedUnitCacheKey) -> Self {
        key.clone()
    }
}

/// A single `SavedUnit` and its associated cache key in a save request.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, Builder)]
#[non_exhaustive]
pub struct CargoSaveUnitRequest {
    /// The cache key for the `SavedUnit` instance.
    #[builder(into)]
    pub key: SavedUnitCacheKey,

    /// The `SavedUnit` to save.
    #[builder(into)]
    pub unit: SavedUnit,
}

/// Request to save cargo cache metadata.
#[derive(Debug, Clone, Serialize, Deserialize, From)]
#[non_exhaustive]
pub struct CargoSaveRequest(HashSet<CargoSaveUnitRequest>);

impl CargoSaveRequest {
    /// Create a new instance from the provided units.
    pub fn new(units: impl IntoIterator<Item = impl Into<CargoSaveUnitRequest>>) -> Self {
        units
            .into_iter()
            .map(Into::into)
            .collect::<HashSet<_>>()
            .pipe(Self)
    }

    /// Iterate over the units in the request.
    pub fn iter(&self) -> impl Iterator<Item = &CargoSaveUnitRequest> {
        self.0.iter()
    }
}

impl IntoIterator for CargoSaveRequest {
    type Item = CargoSaveUnitRequest;
    type IntoIter = std::collections::hash_set::IntoIter<CargoSaveUnitRequest>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl FromIterator<CargoSaveUnitRequest> for CargoSaveRequest {
    fn from_iter<T: IntoIterator<Item = CargoSaveUnitRequest>>(iter: T) -> Self {
        Self::new(iter)
    }
}

impl From<&CargoSaveRequest> for CargoSaveRequest {
    fn from(req: &CargoSaveRequest) -> Self {
        req.clone()
    }
}

/// Request to restore cargo cache metadata.
#[derive(Debug, Clone, Serialize, Deserialize, From)]
#[non_exhaustive]
pub struct CargoRestoreRequest(HashSet<SavedUnitCacheKey>);

impl CargoRestoreRequest {
    /// Create a new instance from the provided hashes.
    pub fn new(units: impl IntoIterator<Item = impl Into<SavedUnitCacheKey>>) -> Self {
        units
            .into_iter()
            .map(Into::into)
            .collect::<HashSet<_>>()
            .pipe(Self)
    }

    /// Iterate over the hashes in the request.
    pub fn iter(&self) -> impl Iterator<Item = &SavedUnitCacheKey> {
        self.0.iter()
    }
}

impl IntoIterator for CargoRestoreRequest {
    type Item = SavedUnitCacheKey;
    type IntoIter = std::collections::hash_set::IntoIter<SavedUnitCacheKey>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl FromIterator<SavedUnitCacheKey> for CargoRestoreRequest {
    fn from_iter<T: IntoIterator<Item = SavedUnitCacheKey>>(iter: T) -> Self {
        Self::new(iter)
    }
}

impl From<&CargoRestoreRequest> for CargoRestoreRequest {
    fn from(req: &CargoRestoreRequest) -> Self {
        req.clone()
    }
}

/// Intermediate transport type used when requesting a restore.
///
/// JSON does not permit non-string keys in objects, and we would like to use
/// the struct `SavedUnitCacheKey` as a key in our response map. We work around
/// this by instead sending a list of (key, value) object pairs using this type
/// instead of CargoRestoreResponse, and parsing the list of keys and values
/// back into a map when received.
#[derive(Debug, Clone, Serialize, Deserialize, From)]
pub struct CargoRestoreResponseTransport(HashSet<(SavedUnitCacheKey, SavedUnit)>);

impl CargoRestoreResponseTransport {
    /// Iterate over the units in the response.
    pub fn iter(&self) -> impl Iterator<Item = (&SavedUnitCacheKey, &SavedUnit)> {
        // This looks odd, but it's sugar going from `&(A, B)` to `(&A, &B)`.
        self.0.iter().map(|(a, b)| (a, b))
    }
}

impl From<CargoRestoreResponseTransport> for CargoRestoreResponse {
    fn from(resp: CargoRestoreResponseTransport) -> Self {
        resp.into_iter().collect()
    }
}

impl IntoIterator for CargoRestoreResponseTransport {
    type Item = (SavedUnitCacheKey, SavedUnit);
    type IntoIter = std::collections::hash_set::IntoIter<(SavedUnitCacheKey, SavedUnit)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl FromIterator<(SavedUnitCacheKey, SavedUnit)> for CargoRestoreResponseTransport {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (SavedUnitCacheKey, SavedUnit)>,
    {
        Self(iter.into_iter().collect())
    }
}

/// Response from restoring cargo cache metadata.
#[derive(Debug, Clone, Serialize, Deserialize, From)]
pub struct CargoRestoreResponse(HashMap<SavedUnitCacheKey, SavedUnit>);

impl CargoRestoreResponse {
    /// Create a new instance from the provided hashes.
    pub fn new<I, H, U>(units: I) -> Self
    where
        I: IntoIterator<Item = (H, U)>,
        H: Into<SavedUnitCacheKey>,
        U: Into<SavedUnit>,
    {
        units
            .into_iter()
            .map(|(hash, unit)| (hash.into(), unit.into()))
            .collect::<HashMap<_, _>>()
            .pipe(Self)
    }

    /// Iterate over the units in the response.
    pub fn iter(&self) -> impl Iterator<Item = (&SavedUnitCacheKey, &SavedUnit)> {
        self.0.iter()
    }

    /// Check if the response is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Get a unit by its cache key.
    pub fn get(&self, key: &SavedUnitCacheKey) -> Option<&SavedUnit> {
        self.0.get(key)
    }

    /// Consume a unit by its cache key, removing it from the response.
    pub fn take(&mut self, key: &SavedUnitCacheKey) -> Option<SavedUnit> {
        self.0.remove(key)
    }
}

impl IntoIterator for CargoRestoreResponse {
    type Item = (SavedUnitCacheKey, SavedUnit);
    type IntoIter = std::collections::hash_map::IntoIter<SavedUnitCacheKey, SavedUnit>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl FromIterator<(SavedUnitCacheKey, SavedUnit)> for CargoRestoreResponse {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (SavedUnitCacheKey, SavedUnit)>,
    {
        Self(iter.into_iter().collect())
    }
}

impl From<&CargoRestoreResponse> for CargoRestoreResponse {
    fn from(resp: &CargoRestoreResponse) -> Self {
        resp.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq as pretty_assert_eq;

    #[test]
    fn glibc_newer_host_can_run_older() {
        let host = LibcVersion::Glibc {
            major: 2,
            minor: 31,
        };
        let required = LibcVersion::Glibc {
            major: 2,
            minor: 17,
        };
        assert!(host.can_run(&required));
    }

    #[test]
    fn glibc_older_host_cannot_run_newer() {
        let host = LibcVersion::Glibc {
            major: 2,
            minor: 17,
        };
        let required = LibcVersion::Glibc {
            major: 2,
            minor: 31,
        };
        assert!(!host.can_run(&required));
    }

    #[test]
    fn glibc_same_version_compatible() {
        let host = LibcVersion::Glibc {
            major: 2,
            minor: 31,
        };
        let required = LibcVersion::Glibc {
            major: 2,
            minor: 31,
        };
        assert!(host.can_run(&required));
    }

    #[test]
    fn glibc_major_version_upgrade_compatible() {
        let host = LibcVersion::Glibc { major: 3, minor: 0 };
        let required = LibcVersion::Glibc {
            major: 2,
            minor: 99,
        };
        assert!(host.can_run(&required));
    }

    #[test]
    fn glibc_cannot_downgrade_major() {
        let host = LibcVersion::Glibc { major: 2, minor: 0 };
        let required = LibcVersion::Glibc { major: 3, minor: 0 };
        assert!(!host.can_run(&required));
    }

    #[test]
    fn darwin_newer_host_can_run_older() {
        let host = LibcVersion::Darwin {
            major: 23,
            minor: 0,
        };
        let required = LibcVersion::Darwin {
            major: 21,
            minor: 0,
        };
        assert!(host.can_run(&required));
    }

    #[test]
    fn darwin_older_host_cannot_run_newer() {
        let host = LibcVersion::Darwin {
            major: 21,
            minor: 0,
        };
        let required = LibcVersion::Darwin {
            major: 23,
            minor: 0,
        };
        assert!(!host.can_run(&required));
    }

    #[test]
    fn musl_always_compatible_with_musl() {
        assert!(LibcVersion::Musl.can_run(&LibcVersion::Musl));
    }

    #[test]
    fn windows_always_compatible_with_windows() {
        assert!(LibcVersion::Windows.can_run(&LibcVersion::Windows));
    }

    #[test]
    fn glibc_incompatible_with_musl() {
        let glibc = LibcVersion::Glibc {
            major: 2,
            minor: 31,
        };
        assert!(!glibc.can_run(&LibcVersion::Musl));
        assert!(!LibcVersion::Musl.can_run(&glibc));
    }

    #[test]
    fn darwin_incompatible_with_glibc() {
        let darwin = LibcVersion::Darwin {
            major: 23,
            minor: 0,
        };
        let glibc = LibcVersion::Glibc {
            major: 2,
            minor: 31,
        };
        assert!(!darwin.can_run(&glibc));
        assert!(!glibc.can_run(&darwin));
    }

    #[test]
    fn libc_version_serialization() {
        let glibc = LibcVersion::Glibc {
            major: 2,
            minor: 31,
        };
        let json = serde_json::to_string(&glibc).unwrap();
        pretty_assert_eq!(json, r#"{"type":"glibc","major":2,"minor":31}"#);

        let darwin = LibcVersion::Darwin {
            major: 23,
            minor: 5,
        };
        let json = serde_json::to_string(&darwin).unwrap();
        pretty_assert_eq!(json, r#"{"type":"darwin","major":23,"minor":5}"#);

        let musl = LibcVersion::Musl;
        let json = serde_json::to_string(&musl).unwrap();
        pretty_assert_eq!(json, r#"{"type":"musl"}"#);
    }

    #[test]
    fn libc_version_deserialization() {
        let glibc = serde_json::from_str::<LibcVersion>(r#"{"type":"glibc","major":2,"minor":31}"#)
            .unwrap();
        pretty_assert_eq!(
            glibc,
            LibcVersion::Glibc {
                major: 2,
                minor: 31
            }
        );

        let darwin =
            serde_json::from_str::<LibcVersion>(r#"{"type":"darwin","major":23,"minor":5}"#)
                .unwrap();
        pretty_assert_eq!(
            darwin,
            LibcVersion::Darwin {
                major: 23,
                minor: 5
            }
        );
    }

    #[test]
    fn libc_version_display() {
        pretty_assert_eq!(
            LibcVersion::Glibc {
                major: 2,
                minor: 31
            }
            .to_string(),
            "glibc 2.31"
        );
        pretty_assert_eq!(
            LibcVersion::Darwin {
                major: 14,
                minor: 0
            }
            .to_string(),
            "macOS 14.0"
        );
        pretty_assert_eq!(LibcVersion::Musl.to_string(), "musl");
        pretty_assert_eq!(LibcVersion::Windows.to_string(), "Windows");
        pretty_assert_eq!(LibcVersion::Unknown.to_string(), "unknown");
    }

    #[test]
    fn cache_key_includes_libc_in_hash() {
        let key1 = SavedUnitCacheKey::builder()
            .unit_hash("abc123")
            .libc_version(LibcVersion::Glibc {
                major: 2,
                minor: 31,
            })
            .build();

        let key2 = SavedUnitCacheKey::builder()
            .unit_hash("abc123")
            .libc_version(LibcVersion::Glibc {
                major: 2,
                minor: 17,
            })
            .build();

        // Same unit_hash but different libc versions should produce different hashes
        assert_ne!(key1.stable_hash(), key2.stable_hash());

        // Same everything should produce same hash
        let key3 = SavedUnitCacheKey::builder()
            .unit_hash("abc123")
            .libc_version(LibcVersion::Glibc {
                major: 2,
                minor: 31,
            })
            .build();
        pretty_assert_eq!(key1.stable_hash(), key3.stable_hash());
    }
}
