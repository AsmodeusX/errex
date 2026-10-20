-- Releases. An issue stays one row per (project, fingerprint) across
-- releases; these tables record which releases reported it, so a resolve
-- can name the release it was fixed in and an alert fires once per issue
-- per release.
--
-- `releases`: every release a project has reported, with when it first
-- did. Release order is decided from the strings where they carry a build
-- number or a version (errex_proto::release::compare); `first_seen` is the
-- fallback for every other format, and the source of a resolve's default
-- release (the project's newest).
--
-- `issue_releases`: per issue, per release, how many events and when.
--
-- New columns on `issues`, all NULL when unknown:
--   first_release        release of the first event that carried one
--   last_release         release of the most recent event that carried one
--   newest_release       highest release, in release order, that reported it
--   resolved_in_release  while resolved: events from this release or older
--                        count without regressing the issue
--
-- Back-fill below reads the release out of every stored event. Issues whose
-- events retention already purged get no release rows, and the per-release
-- counts cover only the events still stored. `newest_release`, and
-- `resolved_in_release` for issues already resolved, need release order,
-- which SQL cannot compute; `Store::migrate` fills them in Rust right after
-- this migration.

ALTER TABLE issues ADD COLUMN first_release TEXT;
ALTER TABLE issues ADD COLUMN last_release TEXT;
ALTER TABLE issues ADD COLUMN newest_release TEXT;
ALTER TABLE issues ADD COLUMN resolved_in_release TEXT;

CREATE TABLE IF NOT EXISTS releases (
    project    TEXT NOT NULL,
    release    TEXT NOT NULL,
    first_seen TEXT NOT NULL,
    PRIMARY KEY (project, release)
);

CREATE TABLE IF NOT EXISTS issue_releases (
    issue_id    INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    release     TEXT NOT NULL,
    event_count INTEGER NOT NULL,
    first_seen  TEXT NOT NULL,
    last_seen   TEXT NOT NULL,
    PRIMARY KEY (issue_id, release)
);

-- Ingest caps a release at 512 characters before it is stored; the
-- back-fill applies the same cap so a back-filled row and a live one for
-- the same release are the same row. Unparseable payloads and events
-- without a string release are skipped, never an error.
INSERT OR IGNORE INTO issue_releases (issue_id, release, event_count, first_seen, last_seen)
SELECT issue_id, release, COUNT(*), MIN(received_at), MAX(received_at)
FROM (
    SELECT e.issue_id,
           substr(json_extract(e.payload, '$.release'), 1, 512) AS release,
           e.received_at
    FROM events e
    WHERE json_valid(e.payload)
      AND json_type(e.payload, '$.release') = 'text'
      AND json_extract(e.payload, '$.release') <> ''
)
GROUP BY issue_id, release;

INSERT OR IGNORE INTO releases (project, release, first_seen)
SELECT i.project, ir.release, MIN(ir.first_seen)
FROM issue_releases ir
JOIN issues i ON i.id = ir.issue_id
GROUP BY i.project, ir.release;

UPDATE issues
SET first_release = (
        SELECT ir.release FROM issue_releases ir
        WHERE ir.issue_id = issues.id
        ORDER BY ir.first_seen ASC, ir.release ASC LIMIT 1
    ),
    last_release = (
        SELECT ir.release FROM issue_releases ir
        WHERE ir.issue_id = issues.id
        ORDER BY ir.last_seen DESC, ir.release DESC LIMIT 1
    )
WHERE EXISTS (SELECT 1 FROM issue_releases ir WHERE ir.issue_id = issues.id);
