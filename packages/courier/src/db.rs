//! Database interface.
//!
//! # Serialization/Deserialization
//!
//! Types in this module do not implement `Serialize` or `Deserialize` because
//! they are internal implementation details for Courier. If you want to
//! serialize or deserialize these types, create public-facing types that do so
//! and are able to convert back and forth with the internal types.

use std::collections::HashMap;

use clients::courier::v1::{
    Key, SavedUnit,
    cache::{CargoRestoreRequest, CargoSaveRequest, SavedUnitCacheKey},
};
use color_eyre::{
    Result,
    eyre::{Context, bail},
};
use derive_more::Debug;
use futures::StreamExt;
use sqlx::{PgPool, migrate::Migrator};
use tracing::debug;

use crate::{
    auth::{AccountId, AuthenticatedToken, OrgId, RawToken},
    crypto::TokenHash,
};

/// A connected Postgres database instance.
#[derive(Clone, Debug)]
#[debug("Postgres(pool_size = {})", self.pool.size())]
pub struct Postgres {
    pub pool: PgPool,
}

impl Postgres {
    /// The migrator for the database.
    pub const MIGRATOR: Migrator = sqlx::migrate!("./schema/migrations");

    /// Connect to the Postgres database.
    #[tracing::instrument(name = "Postgres::connect")]
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPool::connect(url).await?;
        Ok(Self { pool })
    }

    /// Ping the database to ensure the connection is alive.
    #[tracing::instrument(name = "Postgres::ping")]
    pub async fn ping(&self) -> Result<()> {
        let row = sqlx::query!("select 1 as pong")
            .fetch_one(&self.pool)
            .await
            .context("ping database")?;
        if row.pong.is_none_or(|pong| pong != 1) {
            bail!("database ping failed; unexpected response: {row:?}");
        }
        Ok(())
    }
}

impl AsRef<PgPool> for Postgres {
    fn as_ref(&self) -> &PgPool {
        &self.pool
    }
}

impl Postgres {
    #[tracing::instrument(name = "Postgres::save_cargo_cache")]
    pub async fn cargo_cache_save(
        &self,
        auth: &AuthenticatedToken,
        request: CargoSaveRequest,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        // TODO: bulk insert
        for item in request {
            let data = serde_json::to_value(&item.unit)
                .with_context(|| format!("serialize data to json: {:?}", item.unit))?;
            let libc_version = serde_json::to_value(item.key.libc_version())
                .with_context(|| format!("serialize libc_version: {:?}", item.key.libc_version()))?;
            sqlx::query!(
                "insert into cargo_saved_unit (organization_id, cache_key, unit_hash, libc_version, data)
                values ($1, $2, $3, $4, $5)
                on conflict do nothing",
                auth.org_id.as_i64(),
                item.key.stable_hash(),
                item.key.unit_hash.as_str(),
                libc_version,
                data,
            )
            .execute(tx.as_mut())
            .await
            .context("insert serialized cache data")?;
        }

        tx.commit().await.context("commit transaction")
    }

    #[tracing::instrument(name = "Postgres::cargo_cache_restore")]
    pub async fn cargo_cache_restore(
        &self,
        auth: &AuthenticatedToken,
        request: CargoRestoreRequest,
    ) -> Result<HashMap<SavedUnitCacheKey, SavedUnit>> {
        use clients::courier::v1::cache::LibcVersion;

        // Build a map of unit_hash -> requested cache key for later reconstruction.
        // The request contains the host's libc version; we'll use it to filter compatible
        // cached artifacts.
        let request_by_unit_hash = request
            .into_iter()
            .map(|item| (item.unit_hash.as_str().to_owned(), item))
            .collect::<HashMap<_, _>>();

        // Query by unit_hash to find all potentially compatible cached units.
        let mut rows = sqlx::query!(
            "select unit_hash, libc_version, data
            from cargo_saved_unit
            where organization_id = $1
            and unit_hash = any($2)",
            auth.org_id.as_i64(),
            &request_by_unit_hash.keys().cloned().collect::<Vec<_>>(),
        )
        .fetch(&self.pool);

        // For each unit_hash, we may find multiple cached entries with different libc
        // versions. We need to:
        // 1. Filter to only those that are compatible (host can_run cached)
        // 2. Prefer exact matches over compatible-but-different versions
        // 3. If no exact match, pick the newest compatible version
        let mut candidates: HashMap<String, Vec<(LibcVersion, SavedUnit)>> = HashMap::new();

        while let Some(row) = rows.next().await {
            let row = row.context("read rows")?;
            let Some(unit_hash) = row.unit_hash else {
                // Old rows without unit_hash column - skip
                continue;
            };
            let Some(libc_json) = row.libc_version else {
                // Old rows without libc_version column - skip
                continue;
            };

            let cached_libc = serde_json::from_value::<LibcVersion>(libc_json)
                .with_context(|| format!("deserialize libc_version for unit: {unit_hash}"))?;
            let unit = serde_json::from_value::<SavedUnit>(row.data)
                .with_context(|| format!("deserialize data for unit: {unit_hash}"))?;

            candidates
                .entry(unit_hash)
                .or_default()
                .push((cached_libc, unit));
        }

        // Now filter and select the best match for each requested unit_hash
        let mut artifacts = HashMap::with_capacity(request_by_unit_hash.len());
        for (unit_hash, requested_key) in request_by_unit_hash {
            let Some(unit_candidates) = candidates.remove(&unit_hash) else {
                // No cached entries for this unit_hash
                debug!(?unit_hash, "no cached entries found");
                continue;
            };

            let host_libc = requested_key.libc_version();

            // Find compatible candidates and prefer exact match
            let mut best_match: Option<(LibcVersion, SavedUnit)> = None;
            for (cached_libc, unit) in unit_candidates {
                if !host_libc.can_run(&cached_libc) {
                    debug!(
                        ?unit_hash,
                        ?host_libc,
                        ?cached_libc,
                        "cached entry incompatible"
                    );
                    continue;
                }

                // Compatible! Check if it's a better match than current best.
                match &best_match {
                    None => {
                        // First compatible match
                        best_match = Some((cached_libc, unit));
                    }
                    Some((best_libc, _)) => {
                        // Prefer exact match, otherwise prefer newer (larger) version
                        if &cached_libc == host_libc {
                            // Exact match - use it
                            best_match = Some((cached_libc, unit));
                        } else if best_libc != host_libc && cached_libc > *best_libc {
                            // Neither is exact, prefer newer
                            best_match = Some((cached_libc, unit));
                        }
                    }
                }
            }

            if let Some((matched_libc, unit)) = best_match {
                debug!(
                    ?unit_hash,
                    ?host_libc,
                    ?matched_libc,
                    "found compatible cached entry"
                );
                // Return with the original requested key (which has the host's libc)
                // so the client can match it back to the request
                artifacts.insert(requested_key, unit);
            } else {
                debug!(?unit_hash, ?host_libc, "no compatible cached entries");
            }
        }

        Ok(artifacts)
    }

    /// Lookup account and org for a raw token by direct hash comparison.
    #[tracing::instrument(name = "Postgres::token_lookup", skip(token))]
    async fn token_lookup(
        &self,
        token: impl AsRef<RawToken>,
    ) -> Result<Option<(AccountId, OrgId)>> {
        let hash = TokenHash::new(token.as_ref().expose());
        let row = sqlx::query!(
            r#"
            SELECT
                account.id as account_id,
                account.organization_id
            FROM api_key
            JOIN account ON api_key.account_id = account.id
            WHERE api_key.hash = $1 AND api_key.revoked_at IS NULL
            "#,
            hash.as_bytes(),
        )
        .fetch_optional(&self.pool)
        .await
        .context("query for token")?;

        Ok(row.map(|r| {
            (
                AccountId::from_i64(r.account_id),
                OrgId::from_i64(r.organization_id),
            )
        }))
    }

    /// Validate a raw token against the database.
    ///
    /// Returns `Some(AuthenticatedToken)` if the token is valid and not
    /// revoked, otherwise returns `None`. Errors are only returned for
    /// database failures.
    #[tracing::instrument(name = "Postgres::validate", skip(token))]
    pub async fn validate(&self, token: impl Into<RawToken>) -> Result<Option<AuthenticatedToken>> {
        let token = token.into();
        Ok(self
            .token_lookup(&token)
            .await?
            .map(|(account_id, org_id)| AuthenticatedToken {
                account_id,
                org_id,
                plaintext: token,
            }))
    }

    /// Generate a new token for the account in the database.
    /// Currently only used in tests. If used elsewhere, feel free to make this
    /// generally available.
    #[allow(dead_code)]
    #[tracing::instrument(name = "Postgres::create_token")]
    pub async fn create_token(&self, account: AccountId, name: &str) -> Result<RawToken> {
        use rand::RngCore;

        let plaintext = {
            let mut plaintext = [0u8; 16];
            rand::thread_rng()
                .try_fill_bytes(&mut plaintext)
                .context("generate plaintext key")?;
            hex::encode(plaintext)
        };

        let token = TokenHash::new(&plaintext);
        sqlx::query!(
            r#"
            INSERT INTO api_key (account_id, name, hash)
            VALUES ($1, $2, $3)
            "#,
            account.as_i64(),
            name,
            token.as_bytes(),
        )
        .execute(&self.pool)
        .await
        .context("insert token")?;

        Ok(RawToken::new(plaintext))
    }

    /// Revoke the specified token.
    /// Currently only used in tests. If used elsewhere, feel free to make this
    /// generally available.
    #[allow(dead_code)]
    #[tracing::instrument(name = "Postgres::revoke_token", skip(token))]
    pub async fn revoke_token(&self, token: impl AsRef<RawToken>) -> Result<()> {
        let hash = TokenHash::new(token.as_ref().expose());

        let results = sqlx::query!(
            r#"
            UPDATE api_key
            SET revoked_at = now()
            WHERE hash = $1
            "#,
            hash.as_bytes(),
        )
        .execute(&self.pool)
        .await
        .context("revoke token")?;

        if results.rows_affected() == 0 {
            bail!("no such token to revoke in the database");
        }

        Ok(())
    }

    /// Grant an organization access to a CAS key.
    ///
    /// This is idempotent: if the organization already has access, this is a
    /// no-op.
    ///
    /// Returns `true` if access was newly granted, `false` if the org already
    /// had access.
    #[tracing::instrument(name = "Postgres::grant_cas_access")]
    pub async fn grant_cas_access(&self, org_id: OrgId, key: &Key) -> Result<bool> {
        // First, ensure the CAS key exists
        let key_id = sqlx::query!(
            r#"
            INSERT INTO cas_key (content)
            VALUES ($1)
            ON CONFLICT (content) DO UPDATE SET content = EXCLUDED.content
            RETURNING id
            "#,
            key.as_bytes(),
        )
        .fetch_one(&self.pool)
        .await
        .context("upsert cas key")?
        .id;

        // Then grant access to the organization
        let result = sqlx::query!(
            r#"
            INSERT INTO cas_access (organization_id, cas_key_id)
            VALUES ($1, $2)
            ON CONFLICT (organization_id, cas_key_id) DO NOTHING
            "#,
            org_id.as_i64(),
            key_id,
        )
        .execute(&self.pool)
        .await
        .context("grant org access to cas key")?;

        // If rows_affected is 1, we inserted a new row (newly granted access)
        // If rows_affected is 0, the row already existed (org already had access)
        Ok(result.rows_affected() == 1)
    }

    /// Check if an organization has access to a CAS key.
    #[tracing::instrument(name = "Postgres::check_cas_access")]
    pub async fn check_cas_access(&self, org_id: OrgId, key: &Key) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM cas_access
                WHERE organization_id = $1
                AND cas_key_id = (SELECT id FROM cas_key WHERE content = $2)
            ) as "exists!"
            "#,
            org_id.as_i64(),
            key.as_bytes(),
        )
        .fetch_one(&self.pool)
        .await
        .context("check cas access")?;

        Ok(result.exists)
    }

    /// Check which keys from a set the organization has access to.
    /// Returns a HashSet of keys that the organization can access.
    #[tracing::instrument(name = "Postgres::check_cas_access_bulk", skip(keys))]
    pub async fn check_cas_access_bulk(
        &self,
        org_id: OrgId,
        keys: &[Key],
    ) -> Result<std::collections::HashSet<Key>> {
        if keys.is_empty() {
            return Ok(std::collections::HashSet::new());
        }

        let key_bytes: Vec<Vec<u8>> = keys.iter().map(|k| k.as_bytes().to_vec()).collect();

        let rows = sqlx::query!(
            r#"
            SELECT cas_key.content
            FROM cas_key
            JOIN cas_access ON cas_key.id = cas_access.cas_key_id
            WHERE cas_access.organization_id = $1
            AND cas_key.content = ANY($2)
            "#,
            org_id.as_i64(),
            &key_bytes,
        )
        .fetch_all(&self.pool)
        .await
        .context("check cas access bulk")?;

        rows.into_iter()
            .map(|row| {
                Key::from_bytes(&row.content)
                    .with_context(|| format!("parse key: {:x?}", &row.content))
            })
            .collect()
    }

    #[tracing::instrument(name = "Postgres::cargo_cache_reset")]
    pub async fn cargo_cache_reset(&self, auth: &AuthenticatedToken) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        sqlx::query!(
            "delete from cargo_saved_unit where organization_id = $1",
            auth.org_id.as_i64()
        )
        .execute(tx.as_mut())
        .await
        .context("delete saved units")?;

        sqlx::query!(
            "delete from cas_access where organization_id = $1",
            auth.org_id.as_i64()
        )
        .execute(tx.as_mut())
        .await
        .context("delete cas access")?;

        tx.commit().await?;
        Ok(())
    }
}
