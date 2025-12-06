-- Add unit_hash and libc_version columns for compatibility-based restore.
--
-- This enables querying by unit_hash alone, then filtering by libc compatibility.
-- The existing cache_key column is kept for unique constraint but no longer used
-- as the primary lookup key.

-- Add new columns (nullable initially for migration)
ALTER TABLE cargo_saved_unit
ADD COLUMN unit_hash TEXT,
ADD COLUMN libc_version JSONB;

-- Create index for compatibility-based lookups
CREATE INDEX idx_cargo_saved_unit_org_unit_hash
ON cargo_saved_unit(organization_id, unit_hash);
