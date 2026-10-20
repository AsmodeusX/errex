-- Takes a database migrated by 20261020000001_releases back to the schema
-- the binary before it expects, keeping every issue and event (and what
-- arrived since the upgrade). Run with errexd stopped:
--
--   sudo -u errex sqlite3 /var/lib/errex/errex.db < 20261020000001_releases.sql
--
-- What is lost: the per-release history and the release each issue was
-- resolved in. Upgrading again re-runs the migration, which back-fills them
-- from the stored events.
BEGIN;
DROP TABLE issue_releases;
DROP TABLE releases;
ALTER TABLE issues DROP COLUMN first_release;
ALTER TABLE issues DROP COLUMN last_release;
ALTER TABLE issues DROP COLUMN newest_release;
ALTER TABLE issues DROP COLUMN resolved_in_release;
DELETE FROM _sqlx_migrations WHERE version = 20261020000001;
COMMIT;
