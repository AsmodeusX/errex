use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use errex_proto::{Event, Fingerprint, Issue, IssueStatus};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{SqliteConnection, SqlitePool};

/// Upper bound on issues returned by one list or snapshot query.
const ISSUE_LIST_LIMIT: i64 = 1000;

use crate::error::DaemonError;

/// JSON shape returned by `/api/projects`. Lives next to the query that
/// produces it so adding a column is a one-file change.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub project: String,
    pub issue_count: usize,
}

/// Admin-side project record: name + ingest token + optional webhook URL.
/// Used by the CLI (`errexd project list/add/rotate/set-webhook`) and to
/// validate `sentry_key` on ingest when `ERREX_REQUIRE_AUTH=true`.
#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub name: String,
    pub token: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub webhook_url: Option<String>,
    /// HTTP status from the most recent webhook delivery, or 0 for transport
    /// failure. `None` means no delivery has been attempted yet.
    pub last_webhook_status: Option<i16>,
    pub last_webhook_at: Option<DateTime<Utc>>,
}

/// Counts returned by `delete_project` and `delete_preview`. Both cascade the
/// same way; preview just doesn't commit. Returned to the SPA so the
/// confirmation modal can show "permanently deletes N events and M issues"
/// before the user types the project name.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DeleteSummary {
    pub events_deleted: i64,
    pub issues_deleted: i64,
}

/// 24-hour activity rollup for a single project. Computed on demand from
/// `events` joined to `issues`; no aggregate table.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityStats {
    pub events_24h: i64,
    pub unique_issues_24h: i64,
    pub last_event_at: Option<DateTime<Utc>>,
    /// 24 hourly counts, oldest → newest. Index 23 is the bucket containing
    /// `now`; index 0 is the bucket 23 hours earlier. Sparkline-ready.
    pub hourly_buckets: Vec<i64>,
}

/// Closed enum, validated by a CHECK in the migration AND on the way in via
/// `from_db_str` (defaults to the SAFER value on garbage — viewer, not
/// admin — so a corrupt row never escalates privileges).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Viewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Viewer => "viewer",
        }
    }
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "admin" => Role::Admin,
            // Defensively default to the LEAST-privileged role on unexpected
            // input. A column corruption must never silently escalate.
            _ => Role::Viewer,
        }
    }
    pub fn is_admin(self) -> bool {
        matches!(self, Role::Admin)
    }
}

/// Public user view. Excludes `password_hash` by construction — that field
/// only ever leaves the store via `get_user_password_hash`, which is called
/// exactly once per login attempt.
#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub username: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub last_login_ip: Option<String>,
    pub deactivated_at: Option<DateTime<Utc>>,
}

/// Server-side session row. The `id` is the cookie value; in admin views
/// returned by `list_user_sessions` we still surface it because the team
/// page needs to be able to revoke individual sessions by id.
#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub username: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

/// Thin handle around a sqlx pool with WAL configured.
///
/// All writes go through the digest task (single-writer invariant). Reads
/// can come from anywhere — `/api/issues`, `/api/issues/:id/event`, the
/// startup hydrator.
#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

/// Snapshot returned by `Store::storage_stats`. Drives the Retention
/// settings header so the operator can see how big their DB is and
/// how much history they're holding before tightening any limit.
///
/// `bytes` is computed from `pragma page_count * page_size`, which
/// counts every page reachable from the SQLite file (including free
/// pages from prior deletions). It excludes the `-wal` and `-shm`
/// sidecar files; for a self-host workload the difference is small
/// and the number is stable across reads.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct StorageStats {
    pub bytes: i64,
    pub issues: i64,
    pub events: i64,
    /// `None` when no events have been ingested. Otherwise the integer
    /// number of days between `now` and the oldest `events.received_at`.
    pub oldest_event_age_days: Option<i64>,
}

/// Operator-configurable retention bounds. Defaults are 0 = "unlimited"
/// so a fresh DB matches pre-feature behavior. The retention task reads
/// these every tick, so a UI change takes effect on the next purge run.
///
/// `event_retention_days` of 0 means "fall back to the boot config flag"
/// (`ERREX_RETENTION_DAYS`). Storing the override separately lets an
/// operator tighten retention via the SPA without a redeploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionSettings {
    pub events_per_issue_max: i64,
    pub issues_per_project_max: i64,
    pub event_retention_days: i64,
}

/// One slot in a batched digest write. Borrows everything so the digest
/// task can construct the slice without cloning under the hot loop.
///
/// `event_id` and `payload` are pre-computed by `digest::prepare()` (in
/// the HTTP handler) so the single-writer transaction does no
/// `serde_json::to_string` or `Uuid::to_string` work — both showed up
/// on the digest's hot path under saturation.
#[derive(Debug)]
pub struct BatchUpsertInput<'a> {
    pub project: &'a str,
    pub fp: &'a Fingerprint,
    pub title: &'a str,
    pub culprit: Option<&'a str>,
    pub level: Option<&'a str>,
    pub now: DateTime<Utc>,
    pub event_id: &'a str,
    pub payload: &'a str,
    /// The event's release, already capped by `digest::prepare`.
    pub release: Option<&'a str>,
}

/// Result of an upsert: the full `Issue` row as it stands after the write.
/// `created` is true when this was the first event with that fingerprint —
/// callers use it to choose `IssueCreated` vs `IssueUpdated` on the wire.
/// `regressed` is true when the upsert flipped a `resolved` issue back to
/// `unresolved` (an event from a release newer than the one it was resolved
/// in). `new_release` is true for an open issue's first event from a
/// release newer than every release that reported it before.
#[derive(Debug, Clone)]
pub struct UpsertResult {
    pub issue: Issue,
    pub created: bool,
    pub regressed: bool,
    pub new_release: bool,
}

/// One release that reported an issue, as `/api/issues/:id/releases`
/// returns it.
#[derive(Debug, Clone, Serialize)]
pub struct IssueRelease {
    pub release: String,
    pub event_count: i64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

/// One release a project has reported, as `/api/projects/:name/releases`
/// returns it.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectRelease {
    pub release: String,
    pub first_seen: DateTime<Utc>,
}

/// The `issues` columns `IssueRow` reads, for `concat!` into a query.
macro_rules! issue_columns {
    () => {
        "id, project, fingerprint, title, culprit, level, status, event_count, first_seen, \
         last_seen, first_release, last_release, newest_release, resolved_in_release"
    };
}

/// The latest stored event's `tags`, as JSON text, for an `issues` query:
/// what [`Issue::tags`] is read from. A payload that is not JSON yields NULL
/// rather than failing the whole list.
macro_rules! latest_tags_column {
    () => {
        "(SELECT CASE WHEN json_valid(e.payload) THEN json_extract(e.payload, '$.tags') END \
         FROM events e WHERE e.issue_id = issues.id \
         ORDER BY e.received_at DESC, e.id DESC LIMIT 1) AS latest_tags"
    };
}

/// Most tags [`tag_summary`] keeps for one issue.
pub const SUMMARY_TAGS_MAX: usize = 32;
/// Longest tag key, in characters, [`tag_summary`] keeps.
pub const SUMMARY_TAG_KEY_MAX: usize = 64;
/// Longest tag value, in characters, [`tag_summary`] keeps.
pub const SUMMARY_TAG_VALUE_MAX: usize = 200;

/// An event's `tags` (object or `[key, value]` pairs) as the bounded
/// string map [`Issue::tags`] carries. Strings, numbers and booleans are
/// kept; nulls and structures are not tags and are skipped. None when
/// nothing is left.
pub fn tag_summary(tags: &serde_json::Value) -> Option<BTreeMap<String, String>> {
    use serde_json::Value;
    let pairs: Vec<(&str, &Value)> = match tags {
        Value::Object(map) => map.iter().map(|(k, v)| (k.as_str(), v)).collect(),
        Value::Array(pairs) => pairs
            .iter()
            .filter_map(|pair| {
                let pair = pair.as_array()?;
                Some((pair.first()?.as_str()?, pair.get(1)?))
            })
            .collect(),
        _ => return None,
    };
    let cap = |s: &str, max: usize| s.chars().take(max).collect::<String>();
    let mut out = BTreeMap::new();
    for (key, value) in pairs {
        if out.len() == SUMMARY_TAGS_MAX {
            break;
        }
        let value = match value {
            Value::String(s) => cap(s, SUMMARY_TAG_VALUE_MAX),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        out.insert(cap(key, SUMMARY_TAG_KEY_MAX), value);
    }
    (!out.is_empty()).then_some(out)
}

/// A persisted event with its raw payload kept verbatim. The renderer can
/// re-parse this as the proto evolves without a re-ingest. Serialized
/// directly to JSON by the `/api/issues/:id/event` endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct StoredEvent {
    pub event_id: String,
    pub received_at: DateTime<Utc>,
    pub payload: serde_json::Value,
}

impl Store {
    pub async fn open(db_path: &Path) -> Result<Self, DaemonError> {
        // WAL gives us non-blocking readers alongside the single writer task.
        // `synchronous=NORMAL` is the standard pairing with WAL: durable across
        // app crashes, only loses writes on a host power loss.
        //
        // Pragma choices are tuned for "minimum hosting RAM" — the
        // load-bearing cost on Railway / Fly / similar.
        //
        // `mmap_size = 16 MB`: virtual mapping window. Default sqlx is
        //   0 (no mmap). 16 MB is enough for hot index pages on a
        //   self-host with ~10k issues; previously 256 MB which over-
        //   committed the address space. RSS only counts touched pages
        //   so the practical RAM impact is small either way; keeping
        //   the window tight lets the OS reclaim cold pages sooner.
        //
        // `cache_size = -1024` (1 MB page cache): default ~2 MB. Drops
        //   ~1 MB of resident page cache. The workload reads few rows
        //   per request so the smaller cache is a wash on hits.
        //
        // `temp_store = MEMORY`: query-planner spills go to RAM
        //   instead of disk. Spills are rare in our queries; this is
        //   a few KB at most.
        //
        // `busy_timeout = 5000` ms: writer waits if a reader holds a
        //   shared lock at checkpoint time.
        // `wal_autocheckpoint = 1000` pages: SQLite default, made
        //   explicit so a future change is intentional.
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_millis(5000))
            .pragma("wal_autocheckpoint", "1000")
            .pragma("mmap_size", "0")
            .pragma("cache_size", "-1024")
            .pragma("temp_store", "MEMORY")
            .foreign_keys(true);

        // 1 connection: under WAL, sqlx serializes all queries through the
        // pool, but the daemon's hot path is already single-writer (the
        // digest task) and the read side is `/api` + WS snapshots issued at
        // self-host volume — sub-ms each. A second connection's
        // prepared-statement cache + page buffer (~0.5 MB) was paying for
        // contention that doesn't exist at this scale. If readers ever
        // start showing up in /metrics queue depth, bump this back.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;

        Ok(Self { pool })
    }

    /// Read-side handle for tests and future query helpers.
    #[allow(dead_code)]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn migrate(&self) -> Result<(), DaemonError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        self.backfill_release_order().await?;
        Ok(())
    }

    /// Completes the releases migration's back-fill with what needs release
    /// order: each issue's `newest_release`, and for an issue that was
    /// already resolved, `resolved_in_release` set to that newest release.
    /// Resolving in the newest release that reported the issue is the
    /// cautious choice: an event from any release after it regresses.
    ///
    /// Live ingest sets `newest_release` whenever an issue gains a release,
    /// so only back-filled issues match and every later start is one empty
    /// query.
    async fn backfill_release_order(&self) -> Result<(), DaemonError> {
        let mut tx = self.pool.begin().await?;
        let issues: Vec<(i64, String)> = sqlx::query_as(
            "SELECT i.id, i.project FROM issues i WHERE i.newest_release IS NULL \
             AND EXISTS (SELECT 1 FROM issue_releases ir WHERE ir.issue_id = i.id)",
        )
        .fetch_all(&mut *tx)
        .await?;
        for (issue_id, project) in &issues {
            let releases: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT ir.release, r.first_seen FROM issue_releases ir \
                 LEFT JOIN releases r ON r.project = ? AND r.release = ir.release \
                 WHERE ir.issue_id = ?",
            )
            .bind(project)
            .bind(issue_id)
            .fetch_all(&mut *tx)
            .await?;
            let newest = releases
                .iter()
                .max_by(|a, b| release_order(&a.0, a.1.as_deref(), &b.0, b.1.as_deref()))
                .map(|r| r.0.as_str());
            sqlx::query(
                "UPDATE issues SET newest_release = ?, resolved_in_release = \
                 CASE WHEN status = 'resolved' AND resolved_in_release IS NULL THEN ? \
                 ELSE resolved_in_release END WHERE id = ?",
            )
            .bind(newest)
            .bind(newest)
            .bind(issue_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        if !issues.is_empty() {
            tracing::info!(issues = issues.len(), "back-filled release order");
        }
        Ok(())
    }

    /// Insert-or-update an issue keyed by `(project, fingerprint)`. When the
    /// row already exists the event_count is incremented and `last_seen` is
    /// bumped; the title/culprit/level fields are NOT overwritten because the
    /// first sighting is the canonical one (subsequent events refining the
    /// title would be confusing without a UI affordance).
    // reason: still used directly by tests/api.rs and tests/store.rs to set
    // up scenarios; the digest task uses `upsert_batch_with_events` so the
    // bin compilation flags this as unused.
    #[allow(dead_code)]
    pub async fn upsert_issue(
        &self,
        project: &str,
        fp: &Fingerprint,
        title: &str,
        culprit: Option<&str>,
        level: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<UpsertResult, DaemonError> {
        let mut tx = self.pool.begin().await?;
        let now_str = now.to_rfc3339();
        let fp_str = fp.as_str();

        // Step 1: try INSERT. If the unique key already matches, the conflict
        // clause becomes a no-op so we can detect it via row_count below.
        let inserted = sqlx::query(
            "INSERT INTO issues (project, fingerprint, title, culprit, level, status, \
             event_count, first_seen, last_seen) \
             VALUES (?, ?, ?, ?, ?, 'unresolved', 1, ?, ?) \
             ON CONFLICT(project, fingerprint) DO NOTHING",
        )
        .bind(project)
        .bind(fp_str)
        .bind(title)
        .bind(culprit)
        .bind(level)
        .bind(&now_str)
        .bind(&now_str)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        let created = inserted == 1;
        let mut regressed = false;
        if !created {
            // Existing row: read current status so we can detect regression
            // (resolved → new event → flip to unresolved). Muted/Ignored stay
            // put — the user explicitly silenced the noise.
            let prev_status: String = sqlx::query_scalar(
                "SELECT status FROM issues WHERE project = ? AND fingerprint = ?",
            )
            .bind(project)
            .bind(fp_str)
            .fetch_one(&mut *tx)
            .await?;

            if IssueStatus::from_db_str(&prev_status) == IssueStatus::Resolved {
                regressed = true;
                sqlx::query(
                    "UPDATE issues \
                     SET event_count = event_count + 1, last_seen = ?, status = 'unresolved' \
                     WHERE project = ? AND fingerprint = ?",
                )
                .bind(&now_str)
                .bind(project)
                .bind(fp_str)
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query(
                    "UPDATE issues SET event_count = event_count + 1, last_seen = ? \
                     WHERE project = ? AND fingerprint = ?",
                )
                .bind(&now_str)
                .bind(project)
                .bind(fp_str)
                .execute(&mut *tx)
                .await?;
            }
        }

        // Read back the post-write row so callers get DB-assigned id and the
        // updated event_count without a second roundtrip.
        let row: IssueRow = sqlx::query_as(
            "SELECT id, project, fingerprint, title, culprit, level, status, \
             event_count, first_seen, last_seen, \
             first_release, last_release, newest_release, resolved_in_release \
             FROM issues WHERE project = ? AND fingerprint = ?",
        )
        .bind(project)
        .bind(fp_str)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(UpsertResult {
            issue: row.into(),
            created,
            regressed,
            new_release: false,
        })
    }

    /// Upsert N events under one transaction. The digest task calls this
    /// with up to `BATCH_SIZE` events drained from the ingest channel; the
    /// single COMMIT amortizes the fsync that dominates per-event cost
    /// under SQLite WAL with synchronous=NORMAL. Per stress testing, the
    /// fsync at COMMIT was the main remaining latency contributor once
    /// fingerprinting moved to the HTTP handler.
    ///
    /// Each tuple is `(upsert input, raw event payload to persist)`. The
    /// returned `Vec<UpsertResult>` is in the same order as `batch`.
    ///
    /// Two events with the same `(project, fingerprint)` in one batch are
    /// applied in order: the first creates the row, the second's UPDATE
    /// bumps event_count to 2. That matches per-event semantics and is
    /// the correct behavior for back-to-back arrivals.
    pub async fn upsert_batch_with_events(
        &self,
        batch: &[BatchUpsertInput<'_>],
    ) -> Result<Vec<UpsertResult>, DaemonError> {
        let mut tx = self.pool.begin().await?;
        let mut results = Vec::with_capacity(batch.len());

        for input in batch {
            let fp_str = input.fp.as_str();
            // Bind `DateTime<Utc>` directly via sqlx's chrono integration;
            // saves the per-event RFC3339 String allocation that
            // `to_rfc3339()` produces. SQLite still stores ISO-8601 TEXT.
            let now = input.now;

            let replayed_issue: Option<i64> =
                sqlx::query_scalar("SELECT issue_id FROM events WHERE event_id = ?")
                    .bind(input.event_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if let Some(issue_id) = replayed_issue {
                let row: IssueRow = sqlx::query_as(
                    "SELECT id, project, fingerprint, title, culprit, level, status, \
                     event_count, first_seen, last_seen, \
                     first_release, last_release, newest_release, resolved_in_release \
                     FROM issues WHERE id = ?",
                )
                .bind(issue_id)
                .fetch_one(&mut *tx)
                .await?;
                results.push(UpsertResult {
                    issue: row.into(),
                    created: false,
                    regressed: false,
                    new_release: false,
                });
                continue;
            }

            let inserted = sqlx::query(
                "INSERT INTO issues (project, fingerprint, title, culprit, level, status, \
                 event_count, first_seen, last_seen) \
                 VALUES (?, ?, ?, ?, ?, 'unresolved', 1, ?, ?) \
                 ON CONFLICT(project, fingerprint) DO NOTHING",
            )
            .bind(input.project)
            .bind(fp_str)
            .bind(input.title)
            .bind(input.culprit)
            .bind(input.level)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();

            let created = inserted == 1;

            let row: IssueRow = sqlx::query_as(
                "SELECT id, project, fingerprint, title, culprit, level, status, \
                 event_count, first_seen, last_seen, \
                 first_release, last_release, newest_release, resolved_in_release \
                 FROM issues WHERE project = ? AND fingerprint = ?",
            )
            .bind(input.project)
            .bind(fp_str)
            .fetch_one(&mut *tx)
            .await?;

            // Append the raw event in the same tx so a partial commit can
            // never leave an issue without its events. A replayed event_id
            // is ignored here, and must not bump counters or reopen the issue.
            let event_inserted = sqlx::query(
                "INSERT INTO events (issue_id, event_id, payload, received_at) \
                 VALUES (?, ?, ?, ?) ON CONFLICT(event_id) DO NOTHING",
            )
            .bind(row.id)
            .bind(input.event_id)
            .bind(input.payload)
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1;

            let release = input.release.filter(|r| !r.is_empty());
            if created {
                let row = match release {
                    Some(release) => {
                        note_release(&mut tx, input.project, release, now).await?;
                        count_issue_release(&mut tx, row.id, release, now).await?;
                        sqlx::query_as(concat!(
                            "UPDATE issues SET first_release = ?, last_release = ?, \
                             newest_release = ? WHERE id = ? RETURNING ",
                            issue_columns!()
                        ))
                        .bind(release)
                        .bind(release)
                        .bind(release)
                        .bind(row.id)
                        .fetch_one(&mut *tx)
                        .await?
                    }
                    None => row,
                };
                results.push(UpsertResult {
                    issue: row.into(),
                    created: true,
                    regressed: false,
                    new_release: false,
                });
                continue;
            }
            if !event_inserted {
                results.push(UpsertResult {
                    issue: row.into(),
                    created: false,
                    regressed: false,
                    new_release: false,
                });
                continue;
            }

            let first_in_release = match release {
                Some(release) => {
                    note_release(&mut tx, &row.project, release, now).await?;
                    count_issue_release(&mut tx, row.id, release, now).await?
                }
                None => false,
            };
            let status = IssueStatus::from_db_str(&row.status);
            // A resolved issue regresses on an event from a release newer
            // than the one it was resolved in. Old builds still in the field
            // keep reporting a fixed defect; their events only count. Without
            // a release on either side there is nothing to compare, and the
            // event regresses the issue as it always did.
            let regressed = status == IssueStatus::Resolved
                && match (release, row.resolved_in_release.as_deref()) {
                    (Some(release), Some(fixed)) => {
                        is_newer(&mut tx, &row.project, release, fixed).await?
                    }
                    _ => true,
                };
            let beyond_newest = match (release, row.newest_release.as_deref()) {
                (Some(release), Some(newest)) => {
                    is_newer(&mut tx, &row.project, release, newest).await?
                }
                (Some(_), None) => true,
                (None, _) => false,
            };
            // An open issue's first event from a release newer than every
            // release that reported it before. An issue with no release
            // history has nothing to be newer than, so it stays quiet: that
            // keeps issues whose events retention purged from alerting.
            let new_release = first_in_release
                && beyond_newest
                && row.newest_release.is_some()
                && status != IssueStatus::Resolved;
            let newest = if beyond_newest {
                release
            } else {
                row.newest_release.as_deref()
            };
            let (status, resolved_in) = if regressed {
                (IssueStatus::Unresolved, None)
            } else {
                (status, row.resolved_in_release.as_deref())
            };
            let updated: IssueRow = sqlx::query_as(concat!(
                "UPDATE issues SET event_count = event_count + 1, last_seen = ?, \
                 first_release = ?, last_release = ?, newest_release = ?, \
                 status = ?, resolved_in_release = ? WHERE id = ? RETURNING ",
                issue_columns!()
            ))
            .bind(now)
            .bind(row.first_release.as_deref().or(release))
            .bind(release.or(row.last_release.as_deref()))
            .bind(newest)
            .bind(status.as_db_str())
            .bind(resolved_in)
            .bind(row.id)
            .fetch_one(&mut *tx)
            .await?;

            results.push(UpsertResult {
                issue: updated.into(),
                created: false,
                regressed,
                new_release,
            });
        }

        tx.commit().await?;
        Ok(results)
    }

    /// Set the triage status for an issue, returning the updated row.
    /// Errors when the issue does not exist (callers should validate the
    /// id from the URL before calling).
    ///
    /// Resolving records the release it is resolved in: `release` when
    /// given, which must be one the issue's project has reported
    /// (`DaemonError::Invalid` otherwise), else the project's newest
    /// release, else none. Any other status clears it.
    pub async fn set_status(
        &self,
        issue_id: i64,
        status: IssueStatus,
        release: Option<&str>,
    ) -> Result<Issue, DaemonError> {
        let mut tx = self.pool.begin().await?;
        let project: Option<String> = sqlx::query_scalar("SELECT project FROM issues WHERE id = ?")
            .bind(issue_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(project) = project else {
            return Err(DaemonError::NotFound(format!("issue {issue_id}")));
        };
        let resolved_in = match (status, release) {
            (IssueStatus::Resolved, Some(release)) => {
                if release_first_seen(&mut tx, &project, release)
                    .await?
                    .is_none()
                {
                    return Err(DaemonError::Invalid(format!(
                        "release {release} has not reported anything in project {project}"
                    )));
                }
                Some(release.to_string())
            }
            (IssueStatus::Resolved, None) => newest_project_release(&mut tx, &project).await?,
            _ => None,
        };
        let mut row: IssueRow = sqlx::query_as(concat!(
            "UPDATE issues SET status = ?, resolved_in_release = ? WHERE id = ? RETURNING ",
            issue_columns!()
        ))
        .bind(status.as_db_str())
        .bind(resolved_in)
        .bind(issue_id)
        .fetch_one(&mut *tx)
        .await?;
        row.latest_tags = sqlx::query_scalar(concat!(
            "SELECT ",
            latest_tags_column!(),
            " FROM issues WHERE id = ?"
        ))
        .bind(issue_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row.into())
    }

    /// The releases that reported an issue, newest first in release order.
    /// Errors when the issue does not exist.
    pub async fn issue_releases(&self, issue_id: i64) -> Result<Vec<IssueRelease>, DaemonError> {
        let mut conn = self.pool.acquire().await?;
        let project: Option<String> = sqlx::query_scalar("SELECT project FROM issues WHERE id = ?")
            .bind(issue_id)
            .fetch_optional(&mut *conn)
            .await?;
        if project.is_none() {
            return Err(DaemonError::NotFound(format!("issue {issue_id}")));
        }
        let mut rows: Vec<(String, i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT ir.release, ir.event_count, ir.first_seen, ir.last_seen, r.first_seen \
             FROM issue_releases ir \
             JOIN issues i ON i.id = ir.issue_id \
             LEFT JOIN releases r ON r.project = i.project AND r.release = ir.release \
             WHERE ir.issue_id = ? ORDER BY ir.last_seen DESC LIMIT ?",
        )
        .bind(issue_id)
        .bind(ISSUE_LIST_LIMIT)
        .fetch_all(&mut *conn)
        .await?;
        rows.sort_by(|a, b| release_order(&b.0, b.4.as_deref(), &a.0, a.4.as_deref()));
        Ok(rows
            .into_iter()
            .map(
                |(release, event_count, first_seen, last_seen, _)| IssueRelease {
                    release,
                    event_count,
                    first_seen: parse_or_now(&first_seen),
                    last_seen: parse_or_now(&last_seen),
                },
            )
            .collect())
    }

    /// Every release a project has reported, newest first in release order.
    pub async fn project_releases(
        &self,
        project: &str,
    ) -> Result<Vec<ProjectRelease>, DaemonError> {
        let mut conn = self.pool.acquire().await?;
        let mut rows = project_release_rows(&mut conn, project).await?;
        rows.sort_by(|a, b| release_order(&b.0, Some(&b.1), &a.0, Some(&a.1)));
        Ok(rows
            .into_iter()
            .map(|(release, first_seen)| ProjectRelease {
                release,
                first_seen: parse_or_now(&first_seen),
            })
            .collect())
    }

    /// Append the raw event payload for an issue. `event_id` is the SDK-side
    /// UUID; UNIQUE constraint silently swallows duplicate POSTs (Sentry SDKs
    /// retry on transient failures and we don't want double-counting).
    // reason: digest now writes events via `upsert_batch_with_events`; this
    // single-event variant is only exercised by tests that need to seed
    // events directly without going through the channel.
    #[allow(dead_code)]
    pub async fn insert_event(&self, issue_id: i64, event: &Event) -> Result<(), DaemonError> {
        let payload = serde_json::to_string(event)?;
        sqlx::query(
            "INSERT INTO events (issue_id, event_id, payload, received_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(event_id) DO NOTHING",
        )
        .bind(issue_id)
        .bind(event.event_id.to_string())
        .bind(payload)
        .bind(Utc::now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load every issue, newest-first by `last_seen`. Used by the WS snapshot
    /// and the `/api/issues` endpoint when no project filter is applied.
    pub async fn load_issues(&self) -> Result<Vec<Issue>, DaemonError> {
        let rows: Vec<IssueRow> = sqlx::query_as(concat!(
            "SELECT id, project, fingerprint, title, culprit, level, status, \
             event_count, first_seen, last_seen, \
             first_release, last_release, newest_release, resolved_in_release, ",
            latest_tags_column!(),
            " FROM issues ORDER BY last_seen DESC LIMIT ?"
        ))
        .bind(ISSUE_LIST_LIMIT)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Issues for a single project, newest-first.
    pub async fn list_issues_by_project(&self, project: &str) -> Result<Vec<Issue>, DaemonError> {
        let rows: Vec<IssueRow> = sqlx::query_as(concat!(
            "SELECT id, project, fingerprint, title, culprit, level, status, \
             event_count, first_seen, last_seen, \
             first_release, last_release, newest_release, resolved_in_release, ",
            latest_tags_column!(),
            " FROM issues WHERE project = ? ORDER BY last_seen DESC LIMIT ?"
        ))
        .bind(project)
        .bind(ISSUE_LIST_LIMIT)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Distinct project names with issue counts. Done in SQL with GROUP BY
    /// rather than loading every row into RAM.
    pub async fn project_summaries(&self) -> Result<Vec<ProjectSummary>, DaemonError> {
        #[derive(sqlx::FromRow)]
        struct Row {
            project: String,
            issue_count: i64,
        }
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT project, COUNT(*) AS issue_count FROM issues \
             GROUP BY project ORDER BY project",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ProjectSummary {
                project: r.project,
                issue_count: r.issue_count.max(0) as usize,
            })
            .collect())
    }

    /// Create a new project record with a freshly-generated ingest token.
    /// Errors if the project name already exists. The token is derived from
    /// `Uuid::new_v4()` (122 bits of entropy) — sufficient for self-host
    /// without dragging in `rand` or `getrandom` crates directly.
    pub async fn create_project(&self, name: &str) -> Result<Project, DaemonError> {
        let token = generate_token();
        let now = Utc::now().to_rfc3339();
        sqlx::query("INSERT INTO projects (name, token, created_at) VALUES (?, ?, ?)")
            .bind(name)
            .bind(&token)
            .bind(&now)
            .execute(&self.pool)
            .await?;
        Ok(Project {
            name: name.to_string(),
            token,
            created_at: parse_or_now(&now),
            last_used_at: None,
            webhook_url: None,
            last_webhook_status: None,
            last_webhook_at: None,
        })
    }

    /// Set or clear the webhook URL for an existing project.
    pub async fn set_project_webhook(
        &self,
        name: &str,
        url: Option<&str>,
    ) -> Result<(), DaemonError> {
        let updated = sqlx::query("UPDATE projects SET webhook_url = ? WHERE name = ?")
            .bind(url)
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if updated == 0 {
            return Err(DaemonError::NotFound(format!("project {name}")));
        }
        Ok(())
    }

    pub async fn project_by_name(&self, name: &str) -> Result<Option<Project>, DaemonError> {
        let row: Option<ProjectRow> = sqlx::query_as(
            "SELECT name, token, created_at, last_used_at, webhook_url, last_webhook_status, last_webhook_at \
             FROM projects WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    pub async fn project_by_token(&self, token: &str) -> Result<Option<Project>, DaemonError> {
        let row: Option<ProjectRow> = sqlx::query_as(
            "SELECT name, token, created_at, last_used_at, webhook_url, last_webhook_status, last_webhook_at \
             FROM projects WHERE token = ?",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    pub async fn list_admin_projects(&self) -> Result<Vec<Project>, DaemonError> {
        let rows: Vec<ProjectRow> = sqlx::query_as(
            "SELECT name, token, created_at, last_used_at, webhook_url, last_webhook_status, last_webhook_at \
             FROM projects ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn rotate_token(&self, name: &str) -> Result<Project, DaemonError> {
        let token = generate_token();
        let updated = sqlx::query("UPDATE projects SET token = ? WHERE name = ?")
            .bind(&token)
            .bind(name)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if updated == 0 {
            return Err(DaemonError::NotFound(format!("project {name}")));
        }
        // We could RETURNING but sqlx-sqlite makes that fiddly; one extra
        // round-trip is fine for an admin op.
        self.project_by_name(name)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("project {name} (post-rotate)")))
    }

    /// Bumps `last_used_at` for telemetry. Best-effort — a missing row or
    /// transient SQLite contention is logged and swallowed.
    /// Called on every successful envelope ingest; tests/store.rs doesn't
    /// exercise it directly (covered via the api.rs ingest tests instead).
    #[allow(dead_code)] // reason: tests/store.rs binary doesn't call this; api.rs binary does
    pub async fn touch_project_used(&self, name: &str) {
        if let Err(err) = sqlx::query("UPDATE projects SET last_used_at = ? WHERE name = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(name)
            .execute(&self.pool)
            .await
        {
            tracing::debug!(%err, %name, "touch_project_used failed");
        }
    }

    /// Read the singleton retention settings row.
    pub async fn get_retention_settings(&self) -> Result<RetentionSettings, DaemonError> {
        // The migration seeds id=1 with all-zero defaults so this always
        // returns a row; we still use `fetch_optional` defensively against
        // a hand-edited DB with the row deleted.
        let row: Option<(i64, i64, i64)> = sqlx::query_as(
            "SELECT events_per_issue_max, issues_per_project_max, event_retention_days \
             FROM retention_settings WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .map(|(e, i, d)| RetentionSettings {
                events_per_issue_max: e,
                issues_per_project_max: i,
                event_retention_days: d,
            })
            .unwrap_or(RetentionSettings {
                events_per_issue_max: 0,
                issues_per_project_max: 0,
                event_retention_days: 0,
            }))
    }

    /// Update the singleton retention settings row.
    pub async fn set_retention_settings(&self, s: RetentionSettings) -> Result<(), DaemonError> {
        // INSERT…ON CONFLICT handles both the seeded path and the defensive
        // case where the row was nuked by hand.
        sqlx::query(
            "INSERT INTO retention_settings \
             (id, events_per_issue_max, issues_per_project_max, event_retention_days, updated_at) \
             VALUES (1, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                 events_per_issue_max   = excluded.events_per_issue_max, \
                 issues_per_project_max = excluded.issues_per_project_max, \
                 event_retention_days   = excluded.event_retention_days, \
                 updated_at             = excluded.updated_at",
        )
        .bind(s.events_per_issue_max)
        .bind(s.issues_per_project_max)
        .bind(s.event_retention_days)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Snapshot of DB size + row counts + oldest-event age, for the
    /// Retention settings header. All four facts come from a single
    /// connection so they're consistent under concurrent writes (the
    /// SQLite reader sees a snapshot at first query time in WAL mode).
    pub async fn storage_stats(&self) -> Result<StorageStats, DaemonError> {
        // page_count * page_size gives the on-disk size of the main DB
        // file. We multiply server-side rather than fetching two pragmas
        // and folding them in Rust, so the round-trip cost is one query.
        let (bytes,): (i64,) = sqlx::query_as(
            "SELECT (SELECT page_count FROM pragma_page_count) * \
                    (SELECT page_size  FROM pragma_page_size)",
        )
        .fetch_one(&self.pool)
        .await?;
        let (issues,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM issues")
            .fetch_one(&self.pool)
            .await?;
        let (events,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
            .fetch_one(&self.pool)
            .await?;
        // We compute the age in SQLite via JULIANDAY so the result is a
        // stable integer regardless of host timezone or daylight-savings
        // edges. NULL when the events table is empty.
        let oldest: Option<f64> =
            sqlx::query_scalar("SELECT JULIANDAY('now') - JULIANDAY(MIN(received_at)) FROM events")
                .fetch_one(&self.pool)
                .await?;
        let oldest_event_age_days = oldest.map(|d| d.floor() as i64);
        Ok(StorageStats {
            bytes,
            issues,
            events,
            oldest_event_age_days,
        })
    }

    /// Bound the per-issue event count to `max`, dropping oldest rows past
    /// the limit. `max <= 0` is a no-op (the UI semantic for "unlimited").
    /// Returns the number of event rows deleted.
    pub async fn purge_excess_events_per_issue(&self, max: i64) -> Result<u64, DaemonError> {
        if max <= 0 {
            return Ok(0);
        }
        // Window function partitions by issue_id and ranks newest-first;
        // anything past `max` is excess. Single statement so it's
        // serializable under the digest task's writer lock.
        let res = sqlx::query(
            "DELETE FROM events WHERE id IN ( \
                 SELECT id FROM ( \
                     SELECT id, ROW_NUMBER() OVER ( \
                         PARTITION BY issue_id \
                         ORDER BY received_at DESC, id DESC \
                     ) AS rn FROM events \
                 ) WHERE rn > ? \
             )",
        )
        .bind(max)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Bound issues per project to `max`, dropping oldest issues
    /// (smallest `last_seen`). Cascading FK on `events.issue_id` deletes
    /// associated events with the issue row. `max <= 0` is a no-op.
    pub async fn purge_excess_issues_per_project(&self, max: i64) -> Result<u64, DaemonError> {
        if max <= 0 {
            return Ok(0);
        }
        let res = sqlx::query(
            "DELETE FROM issues WHERE id IN ( \
                 SELECT id FROM ( \
                     SELECT id, ROW_NUMBER() OVER ( \
                         PARTITION BY project \
                         ORDER BY last_seen DESC, id DESC \
                     ) AS rn FROM issues \
                 ) WHERE rn > ? \
             )",
        )
        .bind(max)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Delete events whose `received_at` predates `cutoff`. Issue rows are
    /// preserved on purpose — long-tail issues whose payloads have aged out
    /// are still useful as historical context (count, first/last seen).
    /// Returns the number of rows deleted.
    pub async fn purge_events_older_than(&self, cutoff: DateTime<Utc>) -> Result<u64, DaemonError> {
        let res = sqlx::query("DELETE FROM events WHERE received_at < ?")
            .bind(cutoff.to_rfc3339())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Most recent event payload for an issue. Drives the StackTrace /
    /// Breadcrumbs / Tags panes; returns None for issues seen before
    /// persistence was enabled (or whose events have been purged by retention).
    pub async fn latest_event(&self, issue_id: i64) -> Result<Option<StoredEvent>, DaemonError> {
        let row: Option<EventRow> = sqlx::query_as(
            "SELECT event_id, payload, received_at FROM events \
             WHERE issue_id = ? ORDER BY received_at DESC, id DESC LIMIT 1",
        )
        .bind(issue_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(StoredEvent::try_from).transpose()
    }

    /// Count what `delete_project` would destroy, without committing. The
    /// SPA calls this to populate the type-to-confirm modal copy.
    pub async fn delete_preview(&self, name: &str) -> Result<DeleteSummary, DaemonError> {
        if self.project_by_name(name).await?.is_none() {
            return Err(DaemonError::NotFound(format!("project {name}")));
        }
        let (issues_deleted, events_deleted) = self.count_project_rows(name).await?;
        Ok(DeleteSummary {
            events_deleted,
            issues_deleted,
        })
    }

    /// Hard-delete a project and everything it owns: its issues, the events
    /// belonging to those issues (via FK cascade), and the project row
    /// itself. All in one transaction so a partial failure leaves no
    /// dangling rows. Returns counts for the SPA's "deleted N events and M
    /// issues" toast.
    pub async fn delete_project(&self, name: &str) -> Result<DeleteSummary, DaemonError> {
        if self.project_by_name(name).await?.is_none() {
            return Err(DaemonError::NotFound(format!("project {name}")));
        }
        // Snapshot counts before the delete so the return value is accurate
        // (rows_affected on a CASCADE doesn't include the cascaded children).
        let (issues_deleted, events_deleted) = self.count_project_rows(name).await?;

        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM issues WHERE project = ?")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM releases WHERE project = ?")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM projects WHERE name = ?")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(DeleteSummary {
            events_deleted,
            issues_deleted,
        })
    }

    /// Issue + event counts for a project. Used by both delete paths so the
    /// preview and the actual delete return consistent numbers.
    async fn count_project_rows(&self, name: &str) -> Result<(i64, i64), DaemonError> {
        let issues_deleted: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM issues WHERE project = ?")
                .bind(name)
                .fetch_one(&self.pool)
                .await?;
        let events_deleted: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM events e \
             JOIN issues i ON e.issue_id = i.id \
             WHERE i.project = ?",
        )
        .bind(name)
        .fetch_one(&self.pool)
        .await?;
        Ok((issues_deleted.0, events_deleted.0))
    }

    /// Rename a project: updates the `projects` row AND fans out to the
    /// denormalized `issues.project` column. Single transaction. Same-name
    /// rename is a successful no-op (so the SPA doesn't have to special-case
    /// "user pressed save without changes").
    pub async fn rename_project(&self, old: &str, new: &str) -> Result<Project, DaemonError> {
        if old == new {
            return self
                .project_by_name(old)
                .await?
                .ok_or_else(|| DaemonError::NotFound(format!("project {old}")));
        }
        if self.project_by_name(old).await?.is_none() {
            return Err(DaemonError::NotFound(format!("project {old}")));
        }
        let mut tx = self.pool.begin().await?;
        // The PRIMARY KEY on projects.name will trip a unique violation if
        // `new` already exists — we surface that via the SQLx error path so
        // the API can map it to 409.
        sqlx::query("UPDATE projects SET name = ? WHERE name = ?")
            .bind(new)
            .bind(old)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE issues SET project = ? WHERE project = ?")
            .bind(new)
            .bind(old)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE releases SET project = ? WHERE project = ?")
            .bind(new)
            .bind(old)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.project_by_name(new)
            .await?
            .ok_or_else(|| DaemonError::NotFound(format!("project {new} (post-rename)")))
    }

    /// Best-effort write of the most recent webhook delivery outcome. Called
    /// by the webhook task after each POST. A missing row (project deleted
    /// while a webhook was in flight) or transient SQLite contention is
    /// logged and swallowed — webhook health is a UX nicety, not a system
    /// invariant.
    pub async fn record_webhook_attempt(&self, name: &str, status: u16) {
        let res = sqlx::query(
            "UPDATE projects SET last_webhook_status = ?, last_webhook_at = ? WHERE name = ?",
        )
        .bind(status as i64)
        .bind(Utc::now().to_rfc3339())
        .bind(name)
        .execute(&self.pool)
        .await;
        if let Err(err) = res {
            tracing::debug!(%err, %name, "record_webhook_attempt failed");
        }
    }

    /// 24-hour activity rollup for a project. One round-trip per metric;
    /// indexed lookups (`idx_events_issue_received` for the bucket scan,
    /// `idx_issues_project_last_seen` for the issue join) keep this in the
    /// single-digit-ms range on a SQLite of any reasonable size.
    pub async fn activity_stats(
        &self,
        project: &str,
        now: DateTime<Utc>,
    ) -> Result<ActivityStats, DaemonError> {
        let cutoff = now - chrono::Duration::hours(24);
        let cutoff_str = cutoff.to_rfc3339();

        // Total events in the window.
        let events_24h: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM events e \
             JOIN issues i ON e.issue_id = i.id \
             WHERE i.project = ? AND e.received_at >= ?",
        )
        .bind(project)
        .bind(&cutoff_str)
        .fetch_one(&self.pool)
        .await?;

        // Distinct issues touched in the window.
        let unique_issues_24h: (i64,) = sqlx::query_as(
            "SELECT COUNT(DISTINCT e.issue_id) FROM events e \
             JOIN issues i ON e.issue_id = i.id \
             WHERE i.project = ? AND e.received_at >= ?",
        )
        .bind(project)
        .bind(&cutoff_str)
        .fetch_one(&self.pool)
        .await?;

        // Most recent event timestamp (ever, not just within the window). The
        // sub-line under the project name uses this so a "you haven't sent
        // anything in 3 days" project still shows its real last-seen time.
        let last_event_at: Option<(String,)> = sqlx::query_as(
            "SELECT MAX(e.received_at) FROM events e \
             JOIN issues i ON e.issue_id = i.id \
             WHERE i.project = ? AND e.received_at IS NOT NULL",
        )
        .bind(project)
        .fetch_optional(&self.pool)
        .await?;
        let last_event_at = last_event_at
            .and_then(|(s,)| Some(s).filter(|s| !s.is_empty()))
            .map(|s| parse_or_now(&s));

        // Per-hour buckets. We pull every event timestamp in the window and
        // bucket in Rust — simpler and more portable than juggling SQLite's
        // `strftime` modulo arithmetic, and the row count is bounded by a
        // single project's 24h traffic (small for self-host).
        let hourly: Vec<(String, i64)> = sqlx::query_as(
            "SELECT strftime('%Y-%m-%dT%H', e.received_at) AS hour, COUNT(*) \
             FROM events e JOIN issues i ON e.issue_id = i.id \
             WHERE i.project = ? AND e.received_at >= ? GROUP BY hour",
        )
        .bind(project)
        .bind(&cutoff_str)
        .fetch_all(&self.pool)
        .await?;

        let mut hourly_buckets = vec![0i64; 24];
        for (hour, count) in hourly {
            let Ok(naive) = chrono::NaiveDateTime::parse_from_str(
                &format!("{hour}:00:00"),
                "%Y-%m-%dT%H:%M:%S",
            ) else {
                continue;
            };
            // Bucket index: oldest = 0, newest = 23. `now` lands in 23.
            let hours_ago = (now - naive.and_utc()).num_hours();
            if !(0..24).contains(&hours_ago) {
                continue;
            }
            let idx = (23 - hours_ago) as usize;
            hourly_buckets[idx] += count;
        }

        Ok(ActivityStats {
            events_24h: events_24h.0,
            unique_issues_24h: unique_issues_24h.0,
            last_event_at,
            hourly_buckets,
        })
    }

    // ----- users -----

    /// Total user rows. The setup wizard reads this to gate the env-token
    /// bootstrap path: empty table = bootstrap allowed, anything else = no.
    pub async fn user_count(&self) -> Result<i64, DaemonError> {
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// Insert a new user. Caller is responsible for hashing the password
    /// (this layer is crypto-agnostic so callers can pre-validate
    /// strength). Errors with NOT NULL violation if username exists.
    /// Creates the first user only if the table is still empty. The check and
    /// insert are one statement, so two concurrent setup requests cannot both
    /// succeed. Returns `None` when a user already exists.
    pub async fn create_first_user(
        &self,
        username: &str,
        password_hash: &str,
        role: Role,
    ) -> Result<Option<User>, DaemonError> {
        let now = Utc::now().to_rfc3339();
        let inserted = sqlx::query(
            "INSERT INTO users (username, password_hash, role, created_at) \
             SELECT ?, ?, ?, ? WHERE NOT EXISTS (SELECT 1 FROM users)",
        )
        .bind(username)
        .bind(password_hash)
        .bind(role.as_str())
        .bind(&now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if inserted == 0 {
            return Ok(None);
        }
        Ok(Some(User {
            username: username.to_string(),
            role,
            created_at: parse_or_now(&now),
            last_login_at: None,
            last_login_ip: None,
            deactivated_at: None,
        }))
    }

    pub async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        role: Role,
    ) -> Result<User, DaemonError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (username, password_hash, role, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(username)
        .bind(password_hash)
        .bind(role.as_str())
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(User {
            username: username.to_string(),
            role,
            created_at: parse_or_now(&now),
            last_login_at: None,
            last_login_ip: None,
            deactivated_at: None,
        })
    }

    pub async fn list_users(&self) -> Result<Vec<User>, DaemonError> {
        let rows: Vec<UserRow> = sqlx::query_as(
            "SELECT username, role, created_at, last_login_at, last_login_ip, deactivated_at \
             FROM users ORDER BY username",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get_user(&self, username: &str) -> Result<Option<User>, DaemonError> {
        let row: Option<UserRow> = sqlx::query_as(
            "SELECT username, role, created_at, last_login_at, last_login_ip, deactivated_at \
             FROM users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Hash retrieval is its own method so it never accidentally goes out
    /// over the wire as part of the public `User` shape.
    pub async fn get_user_password_hash(
        &self,
        username: &str,
    ) -> Result<Option<String>, DaemonError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT password_hash FROM users WHERE username = ?")
                .bind(username)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(h,)| h))
    }

    /// Sets the password and revokes the user's existing sessions, so a
    /// stolen cookie stops working once the credential changes.
    pub async fn set_user_password(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<(), DaemonError> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query("UPDATE users SET password_hash = ? WHERE username = ?")
            .bind(password_hash)
            .bind(username)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if updated == 0 {
            return Err(DaemonError::NotFound(format!("user {username}")));
        }
        sqlx::query("DELETE FROM sessions WHERE username = ?")
            .bind(username)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_user_role(&self, username: &str, role: Role) -> Result<(), DaemonError> {
        let updated = sqlx::query("UPDATE users SET role = ? WHERE username = ?")
            .bind(role.as_str())
            .bind(username)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if updated == 0 {
            return Err(DaemonError::NotFound(format!("user {username}")));
        }
        Ok(())
    }

    /// Deactivation is reversible (`set_user_deactivated(name, false)` to
    /// re-enable) and on flip-to-true revokes every session — otherwise a
    /// freshly-deactivated user would keep their cookie until expiry.
    pub async fn set_user_deactivated(
        &self,
        username: &str,
        deactivated: bool,
    ) -> Result<(), DaemonError> {
        let mut tx = self.pool.begin().await?;
        let value = if deactivated {
            Some(Utc::now().to_rfc3339())
        } else {
            None
        };
        let updated = sqlx::query("UPDATE users SET deactivated_at = ? WHERE username = ?")
            .bind(value.as_deref())
            .bind(username)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if updated == 0 {
            return Err(DaemonError::NotFound(format!("user {username}")));
        }
        if deactivated {
            sqlx::query("DELETE FROM sessions WHERE username = ?")
                .bind(username)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Hard-delete. The `sessions` FK has ON DELETE CASCADE so sessions go
    /// with the user. `auth_attempts` does NOT cascade — historical brute
    /// force evidence stays useful even after the target user is gone.
    pub async fn delete_user(&self, username: &str) -> Result<(), DaemonError> {
        let updated = sqlx::query("DELETE FROM users WHERE username = ?")
            .bind(username)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if updated == 0 {
            return Err(DaemonError::NotFound(format!("user {username}")));
        }
        Ok(())
    }

    /// Records a successful login on the user row. Best-effort — we don't
    /// fail a login because telemetry didn't write.
    pub async fn touch_user_login(&self, username: &str, ip: Option<&str>) {
        let res =
            sqlx::query("UPDATE users SET last_login_at = ?, last_login_ip = ? WHERE username = ?")
                .bind(Utc::now().to_rfc3339())
                .bind(ip)
                .bind(username)
                .execute(&self.pool)
                .await;
        if let Err(err) = res {
            tracing::debug!(%err, %username, "touch_user_login failed");
        }
    }

    /// Counts admins NOT counting deactivated rows. Used as the lockout
    /// guard: refuse to demote/delete/deactivate the last admin so an
    /// errant click can't lock everyone out of the daemon.
    pub async fn count_active_admins(&self) -> Result<i64, DaemonError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM users WHERE role = 'admin' AND deactivated_at IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    // ----- sessions -----

    pub async fn create_session(
        &self,
        id: &str,
        username: &str,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<Session, DaemonError> {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        sqlx::query(
            "INSERT INTO sessions (id, username, created_at, last_seen_at, ip, user_agent) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(username)
        .bind(&now_str)
        .bind(&now_str)
        .bind(ip)
        .bind(user_agent)
        .execute(&self.pool)
        .await?;
        Ok(Session {
            id: id.to_string(),
            username: username.to_string(),
            created_at: now,
            last_seen_at: now,
            ip: ip.map(str::to_string),
            user_agent: user_agent.map(str::to_string),
        })
    }

    /// Returns `(session, role, deactivated)` so the auth middleware can
    /// reject deactivated-but-still-cookied users in one round-trip.
    pub async fn session_for_id(
        &self,
        id: &str,
    ) -> Result<Option<(Session, Role, bool)>, DaemonError> {
        let row: Option<SessionWithUserRow> = sqlx::query_as(
            "SELECT s.id, s.username, s.created_at, s.last_seen_at, s.ip, s.user_agent, \
                    u.role, u.deactivated_at \
             FROM sessions s JOIN users u ON u.username = s.username \
             WHERE s.id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            let role = Role::from_db_str(&r.role);
            let deactivated = r.deactivated_at.is_some();
            (
                Session {
                    id: r.id,
                    username: r.username,
                    created_at: parse_or_now(&r.created_at),
                    last_seen_at: parse_or_now(&r.last_seen_at),
                    ip: r.ip,
                    user_agent: r.user_agent,
                },
                role,
                deactivated,
            )
        }))
    }

    pub async fn touch_session(&self, id: &str) {
        let res = sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(&self.pool)
            .await;
        if let Err(err) = res {
            tracing::debug!(%err, "touch_session failed");
        }
    }

    pub async fn revoke_session(&self, id: &str) -> Result<(), DaemonError> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Revokes every session for a user. Returns the number revoked so the
    /// admin UI can show "5 sessions ended" instead of guessing.
    pub async fn revoke_user_sessions(&self, username: &str) -> Result<u64, DaemonError> {
        let res = sqlx::query("DELETE FROM sessions WHERE username = ?")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    pub async fn list_user_sessions(&self, username: &str) -> Result<Vec<Session>, DaemonError> {
        let rows: Vec<SessionRow> = sqlx::query_as(
            "SELECT id, username, created_at, last_seen_at, ip, user_agent \
             FROM sessions WHERE username = ? ORDER BY last_seen_at DESC",
        )
        .bind(username)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Session {
                id: r.id,
                username: r.username,
                created_at: parse_or_now(&r.created_at),
                last_seen_at: parse_or_now(&r.last_seen_at),
                ip: r.ip,
                user_agent: r.user_agent,
            })
            .collect())
    }

    /// Sweeps sessions whose `last_seen_at` predates `cutoff`. Called by
    /// the retention task on the same schedule as event purges. Returns the
    /// number of sessions removed.
    pub async fn purge_sessions_idle_since(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<u64, DaemonError> {
        let res = sqlx::query("DELETE FROM sessions WHERE last_seen_at < ?")
            .bind(cutoff.to_rfc3339())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    // ----- auth attempts -----

    pub async fn record_attempt(&self, username: Option<&str>, ip: &str, success: bool) {
        let res = sqlx::query(
            "INSERT INTO auth_attempts (username, ip, ts, success) VALUES (?, ?, ?, ?)",
        )
        .bind(username)
        .bind(ip)
        .bind(Utc::now().to_rfc3339())
        .bind(success as i64)
        .execute(&self.pool)
        .await;
        if let Err(err) = res {
            tracing::debug!(%err, "record_attempt failed");
        }
    }

    /// Number of FAILED attempts against this username since `since`.
    pub async fn count_recent_failures_for_username(
        &self,
        username: &str,
        since: DateTime<Utc>,
    ) -> Result<i64, DaemonError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM auth_attempts \
             WHERE username = ? AND success = 0 AND ts >= ?",
        )
        .bind(username)
        .bind(since.to_rfc3339())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Number of FAILED attempts from this IP since `since`. Counts across
    /// all usernames so a spray attack can't dilute its rate.
    pub async fn count_recent_failures_for_ip(
        &self,
        ip: &str,
        since: DateTime<Utc>,
    ) -> Result<i64, DaemonError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM auth_attempts \
             WHERE ip = ? AND success = 0 AND ts >= ?",
        )
        .bind(ip)
        .bind(since.to_rfc3339())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    pub async fn prune_auth_attempts_older_than(
        &self,
        cutoff: DateTime<Utc>,
    ) -> Result<u64, DaemonError> {
        let res = sqlx::query("DELETE FROM auth_attempts WHERE ts < ?")
            .bind(cutoff.to_rfc3339())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}

/// Release order inside one project. The strings decide where they can
/// (`errex_proto::release::compare`); otherwise the release that first
/// reported anything earlier is the older one, and a release the project
/// has no record of sorts below every recorded one. Equal times fall back
/// to the strings so the order is total.
pub fn release_order(
    a: &str,
    a_first_seen: Option<&str>,
    b: &str,
    b_first_seen: Option<&str>,
) -> std::cmp::Ordering {
    let seen = |s: Option<&str>| s.and_then(|s| DateTime::parse_from_rfc3339(s).ok());
    errex_proto::release::compare(a, b).unwrap_or_else(|| {
        seen(a_first_seen)
            .cmp(&seen(b_first_seen))
            .then_with(|| a.cmp(b))
    })
}

async fn release_first_seen(
    conn: &mut SqliteConnection,
    project: &str,
    release: &str,
) -> Result<Option<String>, DaemonError> {
    Ok(
        sqlx::query_scalar("SELECT first_seen FROM releases WHERE project = ? AND release = ?")
            .bind(project)
            .bind(release)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// Whether release `a` is newer than release `b` in `project`.
async fn is_newer(
    conn: &mut SqliteConnection,
    project: &str,
    a: &str,
    b: &str,
) -> Result<bool, DaemonError> {
    if let Some(ord) = errex_proto::release::compare(a, b) {
        return Ok(ord == std::cmp::Ordering::Greater);
    }
    let a_seen = release_first_seen(conn, project, a).await?;
    let b_seen = release_first_seen(conn, project, b).await?;
    Ok(release_order(a, a_seen.as_deref(), b, b_seen.as_deref()) == std::cmp::Ordering::Greater)
}

async fn project_release_rows(
    conn: &mut SqliteConnection,
    project: &str,
) -> Result<Vec<(String, String)>, DaemonError> {
    Ok(
        sqlx::query_as("SELECT release, first_seen FROM releases WHERE project = ?")
            .bind(project)
            .fetch_all(&mut *conn)
            .await?,
    )
}

async fn newest_project_release(
    conn: &mut SqliteConnection,
    project: &str,
) -> Result<Option<String>, DaemonError> {
    Ok(project_release_rows(conn, project)
        .await?
        .into_iter()
        .max_by(|a, b| release_order(&a.0, Some(&a.1), &b.0, Some(&b.1)))
        .map(|(release, _)| release))
}

/// Records that `release` has reported something in `project`.
async fn note_release(
    conn: &mut SqliteConnection,
    project: &str,
    release: &str,
    now: DateTime<Utc>,
) -> Result<(), DaemonError> {
    sqlx::query("INSERT OR IGNORE INTO releases (project, release, first_seen) VALUES (?, ?, ?)")
        .bind(project)
        .bind(release)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Counts one event from `release` against the issue. True when it is the
/// issue's first event from that release.
async fn count_issue_release(
    conn: &mut SqliteConnection,
    issue_id: i64,
    release: &str,
    now: DateTime<Utc>,
) -> Result<bool, DaemonError> {
    let count: i64 = sqlx::query_scalar(
        "INSERT INTO issue_releases (issue_id, release, event_count, first_seen, last_seen) \
         VALUES (?, ?, 1, ?, ?) \
         ON CONFLICT(issue_id, release) DO UPDATE SET \
             event_count = event_count + 1, \
             first_seen = MIN(first_seen, excluded.first_seen), \
             last_seen = MAX(last_seen, excluded.last_seen) \
         RETURNING event_count",
    )
    .bind(issue_id)
    .bind(release)
    .bind(now)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    Ok(count == 1)
}

fn generate_token() -> String {
    // `Uuid::new_v4().simple()` → 32 lowercase hex chars, 122 bits of
    // entropy. We don't want hyphens (some operators paste this into env
    // files where hyphens trigger word-wrap), so use the simple form.
    uuid::Uuid::new_v4().simple().to_string()
}

fn parse_or_now(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

// ----- Row types isolate sqlx FromRow from the public proto types. -----

#[derive(Debug, sqlx::FromRow)]
struct IssueRow {
    id: i64,
    project: String,
    fingerprint: String,
    title: String,
    culprit: Option<String>,
    level: Option<String>,
    status: String,
    event_count: i64,
    first_seen: String,
    last_seen: String,
    first_release: Option<String>,
    last_release: Option<String>,
    newest_release: Option<String>,
    resolved_in_release: Option<String>,
    /// [`latest_tags_column`]: only the list and status queries select it.
    #[sqlx(default)]
    latest_tags: Option<String>,
}

impl From<IssueRow> for Issue {
    fn from(r: IssueRow) -> Self {
        // Timestamps were written as RFC3339; on read we parse leniently and
        // fall back to "now" rather than crash if the column is corrupt.
        let parse = |s: &str| {
            DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now())
        };
        Issue {
            id: r.id,
            project: r.project,
            fingerprint: Fingerprint::new(r.fingerprint),
            title: r.title,
            culprit: r.culprit,
            level: r.level,
            status: IssueStatus::from_db_str(&r.status),
            event_count: r.event_count,
            first_seen: parse(&r.first_seen),
            last_seen: parse(&r.last_seen),
            first_release: r.first_release,
            last_release: r.last_release,
            newest_release: r.newest_release,
            resolved_in_release: r.resolved_in_release,
            tags: r
                .latest_tags
                .and_then(|t| serde_json::from_str(&t).ok())
                .and_then(|t| tag_summary(&t)),
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ProjectRow {
    name: String,
    token: String,
    created_at: String,
    last_used_at: Option<String>,
    webhook_url: Option<String>,
    last_webhook_status: Option<i64>,
    last_webhook_at: Option<String>,
}

impl From<ProjectRow> for Project {
    fn from(r: ProjectRow) -> Self {
        Project {
            name: r.name,
            token: r.token,
            created_at: parse_or_now(&r.created_at),
            last_used_at: r.last_used_at.as_deref().map(parse_or_now),
            webhook_url: r.webhook_url,
            // SQLite returns INTEGER as i64; status fits comfortably in i16
            // (0..599) so we narrow with saturation.
            last_webhook_status: r
                .last_webhook_status
                .map(|n| n.clamp(0, i16::MAX as i64) as i16),
            last_webhook_at: r.last_webhook_at.as_deref().map(parse_or_now),
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct EventRow {
    event_id: String,
    payload: String,
    received_at: String,
}

#[derive(Debug, sqlx::FromRow)]
struct UserRow {
    username: String,
    role: String,
    created_at: String,
    last_login_at: Option<String>,
    last_login_ip: Option<String>,
    deactivated_at: Option<String>,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        User {
            username: r.username,
            role: Role::from_db_str(&r.role),
            created_at: parse_or_now(&r.created_at),
            last_login_at: r.last_login_at.as_deref().map(parse_or_now),
            last_login_ip: r.last_login_ip,
            deactivated_at: r.deactivated_at.as_deref().map(parse_or_now),
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct SessionRow {
    id: String,
    username: String,
    created_at: String,
    last_seen_at: String,
    ip: Option<String>,
    user_agent: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct SessionWithUserRow {
    id: String,
    username: String,
    created_at: String,
    last_seen_at: String,
    ip: Option<String>,
    user_agent: Option<String>,
    role: String,
    deactivated_at: Option<String>,
}

impl TryFrom<EventRow> for StoredEvent {
    type Error = DaemonError;
    fn try_from(r: EventRow) -> Result<Self, DaemonError> {
        Ok(StoredEvent {
            event_id: r.event_id,
            payload: serde_json::from_str(&r.payload)?,
            received_at: DateTime::parse_from_rfc3339(&r.received_at)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        })
    }
}
