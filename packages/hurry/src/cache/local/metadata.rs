//! SQLite-based metadata storage for local caching.
//!
//! This module stores cargo build unit metadata in a SQLite database,
//! providing efficient lookup and filtering of cached units.

use std::path::Path;

use color_eyre::{Result, eyre::Context};
use derive_more::Debug;
use rusqlite::{Connection, params};
use tracing::instrument;

use clients::courier::v1::{GlibcVersion, SavedUnit, SavedUnitHash};

/// SQLite-based metadata storage for cargo build units.
///
/// Stores unit metadata with their hashes for efficient lookup during
/// restore operations. Supports glibc version filtering for compatibility.
#[derive(Debug)]
pub struct LocalMetadata {
    #[debug("<connection>")]
    conn: Connection,
}

impl LocalMetadata {
    /// Open or create a metadata database at the given path.
    #[instrument(name = "LocalMetadata::open", skip(path))]
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            // Use std::fs here since this runs synchronously at startup
            #[allow(clippy::disallowed_methods)]
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create metadata directory {:?}", parent))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("open metadata database at {:?}", path))?;

        let db = Self { conn };
        db.init_schema()?;

        Ok(db)
    }

    /// Create an in-memory database for testing.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("open in-memory database")?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    /// Initialize the database schema.
    fn init_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS saved_units (
                    unit_hash TEXT PRIMARY KEY,
                    resolved_target TEXT NOT NULL,
                    glibc_major INTEGER,
                    glibc_minor INTEGER,
                    glibc_patch INTEGER,
                    data TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_saved_units_target
                ON saved_units(resolved_target);
                "#,
            )
            .context("initialize database schema")?;

        Ok(())
    }

    /// Save a unit to the database.
    #[instrument(name = "LocalMetadata::save", skip(self, unit))]
    pub fn save(
        &self,
        unit_hash: &SavedUnitHash,
        unit: &SavedUnit,
        resolved_target: &str,
        glibc_version: Option<&GlibcVersion>,
    ) -> Result<()> {
        let data = serde_json::to_string(unit).context("serialize unit")?;

        self.conn
            .execute(
                r#"
                INSERT OR REPLACE INTO saved_units
                (unit_hash, resolved_target, glibc_major, glibc_minor, glibc_patch, data)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
                params![
                    unit_hash.as_str(),
                    resolved_target,
                    glibc_version.map(|v| v.major),
                    glibc_version.map(|v| v.minor),
                    glibc_version.map(|v| v.patch),
                    data,
                ],
            )
            .context("insert unit")?;

        Ok(())
    }

    /// Restore units from the database by their hashes.
    ///
    /// Filters units by glibc compatibility: units compiled against a newer
    /// glibc version than the host will be excluded.
    #[instrument(name = "LocalMetadata::restore", skip(self, unit_hashes))]
    pub fn restore(
        &self,
        unit_hashes: impl IntoIterator<Item = SavedUnitHash>,
        host_glibc_version: Option<&GlibcVersion>,
    ) -> Result<Vec<(SavedUnitHash, SavedUnit)>> {
        let hashes = unit_hashes
            .into_iter()
            .map(|h| h.as_str().to_string())
            .collect::<Vec<_>>();

        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        // Build a query with placeholders for all hashes
        let placeholders = hashes.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let query = format!(
            "SELECT unit_hash, glibc_major, glibc_minor, glibc_patch, data FROM saved_units WHERE unit_hash IN ({})",
            placeholders
        );

        let mut stmt = self.conn.prepare(&query).context("prepare restore query")?;

        let params = rusqlite::params_from_iter(hashes.iter());
        let rows = stmt
            .query_map(params, |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<u32>>(1)?,
                    row.get::<_, Option<u32>>(2)?,
                    row.get::<_, Option<u32>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .context("execute restore query")?;

        let mut results = Vec::new();
        for row in rows {
            let (hash_str, glibc_major, glibc_minor, glibc_patch, data) =
                row.context("read row")?;

            // Filter by glibc compatibility
            if let (Some(host), Some(major), Some(minor), Some(patch)) =
                (host_glibc_version, glibc_major, glibc_minor, glibc_patch)
            {
                let unit_glibc = GlibcVersion {
                    major,
                    minor,
                    patch,
                };
                // Skip units compiled against a newer glibc
                if unit_glibc > *host {
                    continue;
                }
            }

            let unit: SavedUnit = serde_json::from_str(&data)
                .with_context(|| format!("deserialize unit {}", hash_str))?;

            results.push((SavedUnitHash::new(hash_str), unit));
        }

        Ok(results)
    }

    /// Reset (clear) all cached units.
    #[allow(dead_code)]
    #[instrument(name = "LocalMetadata::reset", skip(self))]
    pub fn reset(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM saved_units", [])
            .context("delete all units")?;
        Ok(())
    }

    /// Get the number of cached units.
    #[allow(dead_code)]
    #[instrument(name = "LocalMetadata::count", skip(self))]
    pub fn count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM saved_units", [], |row| row.get(0))
            .context("count units")?;
        Ok(count as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clients::courier::v1::{
        BuildScriptExecutionUnitPlan, BuildScriptOutputFiles, Fingerprint, Key, UnitPlanInfo,
    };
    use pretty_assertions::assert_eq as pretty_assert_eq;

    fn make_saved_unit(hash: &str) -> SavedUnit {
        let info = UnitPlanInfo::builder()
            .unit_hash(hash)
            .package_name("test-pkg")
            .crate_name("test_pkg")
            .maybe_target_arch(Some("x86_64-unknown-linux-gnu"))
            .build();

        let files = BuildScriptOutputFiles::builder()
            .stdout(Key::from_buffer(b"stdout"))
            .stderr(Key::from_buffer(b"stderr"))
            .fingerprint(Fingerprint::from(String::from("test-fingerprint")))
            .build();

        let plan = BuildScriptExecutionUnitPlan::builder()
            .info(info)
            .build_script_program_name("build_script_build")
            .build();

        SavedUnit::BuildScriptExecution(files, plan)
    }

    #[test]
    fn round_trip() {
        let db = LocalMetadata::in_memory().unwrap();

        let hash = SavedUnitHash::new("unit-hash-1");
        let unit = make_saved_unit("unit-hash-1");
        let target = "x86_64-unknown-linux-gnu";

        db.save(&hash, &unit, target, None).unwrap();

        let results = db.restore([hash.clone()], None).unwrap();
        pretty_assert_eq!(results.len(), 1);
        pretty_assert_eq!(results[0].0, hash);
        pretty_assert_eq!(results[0].1, unit);
    }

    #[test]
    fn glibc_filtering() {
        let db = LocalMetadata::in_memory().unwrap();

        let hash = SavedUnitHash::new("unit-hash-1");
        let unit = make_saved_unit("unit-hash-1");
        let target = "x86_64-unknown-linux-gnu";

        // Save with glibc 2.35.0
        let unit_glibc = GlibcVersion {
            major: 2,
            minor: 35,
            patch: 0,
        };
        db.save(&hash, &unit, target, Some(&unit_glibc)).unwrap();

        // Restore with older host glibc should filter it out
        let older_glibc = GlibcVersion {
            major: 2,
            minor: 31,
            patch: 0,
        };
        let results = db.restore([hash.clone()], Some(&older_glibc)).unwrap();
        pretty_assert_eq!(results.len(), 0);

        // Restore with same or newer host glibc should include it
        let same_glibc = GlibcVersion {
            major: 2,
            minor: 35,
            patch: 0,
        };
        let results = db.restore([hash.clone()], Some(&same_glibc)).unwrap();
        pretty_assert_eq!(results.len(), 1);

        let newer_glibc = GlibcVersion {
            major: 2,
            minor: 38,
            patch: 0,
        };
        let results = db.restore([hash.clone()], Some(&newer_glibc)).unwrap();
        pretty_assert_eq!(results.len(), 1);
    }

    #[test]
    fn reset() {
        let db = LocalMetadata::in_memory().unwrap();

        let hash = SavedUnitHash::new("unit-hash-1");
        let unit = make_saved_unit("unit-hash-1");

        db.save(&hash, &unit, "target", None).unwrap();
        pretty_assert_eq!(db.count().unwrap(), 1);

        db.reset().unwrap();
        pretty_assert_eq!(db.count().unwrap(), 0);
    }
}
