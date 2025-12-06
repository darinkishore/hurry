-- Remove libc compatibility columns
DROP INDEX IF EXISTS idx_cargo_saved_unit_org_unit_hash;
ALTER TABLE cargo_saved_unit
DROP COLUMN IF EXISTS unit_hash,
DROP COLUMN IF EXISTS libc_version;
