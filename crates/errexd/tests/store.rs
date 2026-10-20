//! Persistence layer integration tests.
//!
//! Each test boots a fresh in-tempdir SQLite DB. We deliberately do NOT
//! mock the store: the whole point of embedding SQLite is that it's cheap
//! to spin up real ones, and "test against the real type" catches a class
//! of bugs (column types, parameter binding, transaction edge cases) that
//! mocks paper over.
//!
//! These tests pin every public method on `Store` so any future refactor
//! that breaks the contract trips a red bar before reaching prod.

// `#[path]`-imported error/store modules export items that are exercised by
// other test binaries (api.rs uses Role::is_admin, the api handlers use
// DaemonError::Crypto). Inside this binary's compilation unit those look
// dead; the silencing is per-crate, not a real warning.
#![allow(dead_code)]

use std::path::PathBuf;

use chrono::{Duration, Utc};
use errex_proto::{
    Event, ExceptionContainer, ExceptionInfo, Fingerprint, Frame, IssueStatus, Level, Stacktrace,
};
use uuid::Uuid;

#[path = "../src/error.rs"]
mod error;
#[path = "../src/store.rs"]
mod store;

use store::{BatchUpsertInput, RetentionSettings, Role, StorageStats, Store, UpsertResult};

// ----- helpers -----

fn unique_tempdir() -> PathBuf {
    // PID + nanos + atomic seq so parallel cargo test workers don't
    // collide. Same-nanosecond starts (likely under heavy parallelism)
    // would otherwise share a SQLite file and re-run migrations.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "errexd-store-{}-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        seq,
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create tempdir");
    p
}

async fn fresh_store() -> (Store, PathBuf) {
    let dir = unique_tempdir();
    let db = dir.join("errex.db");
    let store = Store::open(&db).await.expect("open");
    store.migrate().await.expect("migrate");
    (store, dir)
}

fn sample_event(ty: &str, value: &str, function: &str, lineno: u32) -> Event {
    Event {
        other: Default::default(),
        event_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        platform: Some("javascript".into()),
        level: Some(Level::Error),
        environment: Some("prod".into()),
        release: Some("1.0.0".into()),
        server_name: None,
        message: None,
        exception: Some(ExceptionContainer {
            values: vec![ExceptionInfo {
                other: Default::default(),
                ty: Some(ty.into()),
                value: Some(value.into()),
                module: None,
                stacktrace: Some(Stacktrace {
                    other: Default::default(),
                    frames: vec![Frame {
                        other: Default::default(),
                        filename: Some("app.js".into()),
                        function: Some(function.into()),
                        module: None,
                        lineno: Some(lineno),
                        colno: None,
                        in_app: Some(true),
                    }],
                }),
            }],
        }),
        breadcrumbs: None,
        tags: None,
        contexts: None,
        extra: None,
        user: None,
        request: None,
        logentry: None,
        fingerprint: None,
    }
}

fn fp(s: &str) -> Fingerprint {
    Fingerprint::new(s.to_string())
}

/// Pre-compute the strings the digest task ships through the channel.
/// Tests against `BatchUpsertInput` need to feed the same shape the
/// production handler does: a stringified event_id and a JSON payload.
fn precomputed(ev: &Event) -> (String, String) {
    (
        ev.event_id.simple().to_string(),
        serde_json::to_string(ev).expect("Event serializes"),
    )
}

// ----- retention_settings -----

#[tokio::test]
async fn retention_settings_default_is_unlimited() {
    let (store, _dir) = fresh_store().await;
    let s = store.get_retention_settings().await.unwrap();
    assert_eq!(s.events_per_issue_max, 0);
    assert_eq!(s.issues_per_project_max, 0);
    assert_eq!(s.event_retention_days, 0);
}

#[tokio::test]
async fn retention_settings_round_trip() {
    let (store, _dir) = fresh_store().await;
    let s = RetentionSettings {
        events_per_issue_max: 50,
        issues_per_project_max: 1000,
        event_retention_days: 14,
    };
    store.set_retention_settings(s).await.unwrap();
    let got = store.get_retention_settings().await.unwrap();
    assert_eq!(got, s);
}

#[tokio::test]
async fn retention_settings_set_is_idempotent() {
    let (store, _dir) = fresh_store().await;
    let s = RetentionSettings {
        events_per_issue_max: 5,
        issues_per_project_max: 5,
        event_retention_days: 5,
    };
    store.set_retention_settings(s).await.unwrap();
    store.set_retention_settings(s).await.unwrap();
    let got = store.get_retention_settings().await.unwrap();
    assert_eq!(got, s);
    // Singleton: only one row.
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM retention_settings")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count.0, 1);
}

#[tokio::test]
async fn purge_excess_events_per_issue_keeps_newest_n() {
    let (store, _dir) = fresh_store().await;
    let f = fp("e");
    let issue = store
        .upsert_issue("p", &f, "T", None, None, Utc::now())
        .await
        .unwrap();
    // Insert 10 distinct events for the same issue.
    for i in 0..10 {
        let mut ev = sample_event("Boom", "v", "f", i);
        ev.event_id = Uuid::new_v4();
        store.insert_event(issue.issue.id, &ev).await.unwrap();
    }
    let pre: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events WHERE issue_id = ?")
        .bind(issue.issue.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(pre.0, 10);

    let deleted = store.purge_excess_events_per_issue(3).await.unwrap();
    assert_eq!(deleted, 7);

    let post: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events WHERE issue_id = ?")
        .bind(issue.issue.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(post.0, 3);
}

#[tokio::test]
async fn purge_excess_events_per_issue_zero_is_noop() {
    let (store, _dir) = fresh_store().await;
    let f = fp("e");
    let issue = store
        .upsert_issue("p", &f, "T", None, None, Utc::now())
        .await
        .unwrap();
    let mut ev = sample_event("Boom", "v", "f", 1);
    ev.event_id = Uuid::new_v4();
    store.insert_event(issue.issue.id, &ev).await.unwrap();
    let deleted = store.purge_excess_events_per_issue(0).await.unwrap();
    assert_eq!(deleted, 0);
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count.0, 1);
}

#[tokio::test]
async fn purge_excess_issues_per_project_drops_oldest() {
    let (store, _dir) = fresh_store().await;
    // Seed 5 issues with monotonically newer last_seen so ordering is
    // deterministic.
    let base = Utc::now() - Duration::seconds(100);
    for i in 0..5 {
        store
            .upsert_issue(
                "p",
                &fp(&format!("f{i}")),
                "T",
                None,
                None,
                base + Duration::seconds(i),
            )
            .await
            .unwrap();
    }
    let pre: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM issues WHERE project = 'p'")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(pre.0, 5);

    let deleted = store.purge_excess_issues_per_project(2).await.unwrap();
    assert_eq!(deleted, 3);

    // The two newest fingerprints must survive (f4, f3).
    let surviving: Vec<(String,)> = sqlx::query_as(
        "SELECT fingerprint FROM issues WHERE project = 'p' ORDER BY last_seen DESC",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    let names: Vec<String> = surviving.into_iter().map(|(s,)| s).collect();
    assert_eq!(names, vec!["f4".to_string(), "f3".to_string()]);
}

#[tokio::test]
async fn purge_excess_issues_per_project_cascades_events() {
    // Deleting an issue must cascade to its events via the FK in init.sql.
    // If a future migration ever drops ON DELETE CASCADE, this test will
    // fail loudly.
    let (store, _dir) = fresh_store().await;
    let base = Utc::now() - Duration::seconds(100);
    for i in 0..3 {
        let r = store
            .upsert_issue(
                "p",
                &fp(&format!("f{i}")),
                "T",
                None,
                None,
                base + Duration::seconds(i),
            )
            .await
            .unwrap();
        let mut ev = sample_event("Boom", "v", "f", i as u32);
        ev.event_id = Uuid::new_v4();
        store.insert_event(r.issue.id, &ev).await.unwrap();
    }
    let pre_events: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(pre_events.0, 3);

    let deleted = store.purge_excess_issues_per_project(1).await.unwrap();
    assert_eq!(deleted, 2);

    let post_events: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(post_events.0, 1, "events for purged issues must cascade");
}

#[tokio::test]
async fn purge_excess_isolates_per_project() {
    // Two projects, three issues each. Cap is per-project, so both
    // projects keep their newest 2.
    let (store, _dir) = fresh_store().await;
    let base = Utc::now() - Duration::seconds(100);
    for proj in ["p1", "p2"] {
        for i in 0..3 {
            store
                .upsert_issue(
                    proj,
                    &fp(&format!("{proj}-f{i}")),
                    "T",
                    None,
                    None,
                    base + Duration::seconds(i),
                )
                .await
                .unwrap();
        }
    }
    let deleted = store.purge_excess_issues_per_project(2).await.unwrap();
    assert_eq!(deleted, 2);
    let count_p1: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM issues WHERE project = 'p1'")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let count_p2: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM issues WHERE project = 'p2'")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count_p1.0, 2);
    assert_eq!(count_p2.0, 2);
}

// ----- upsert_batch_with_events -----

#[tokio::test]
async fn upsert_batch_persists_two_distinct_fingerprints() {
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let ev_a = sample_event("Boom", "v", "fa", 1);
    let ev_b = sample_event("Crash", "v", "fb", 2);
    let (id_a, pl_a) = precomputed(&ev_a);
    let (id_b, pl_b) = precomputed(&ev_b);
    let fp_a = fp("a");
    let fp_b = fp("b");
    let batch = vec![
        BatchUpsertInput {
            project: "p",
            fp: &fp_a,
            title: "T1",
            culprit: None,
            level: None,
            now,
            event_id: &id_a,
            payload: &pl_a,
            release: None,
        },
        BatchUpsertInput {
            project: "p",
            fp: &fp_b,
            title: "T2",
            culprit: None,
            level: None,
            now,
            event_id: &id_b,
            payload: &pl_b,
            release: None,
        },
    ];
    let res = store.upsert_batch_with_events(&batch).await.unwrap();
    assert_eq!(res.len(), 2);
    assert!(res[0].created);
    assert!(res[1].created);
    assert_ne!(res[0].issue.id, res[1].issue.id);

    // Both event rows persisted (single transaction).
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count.0, 2);
}

#[tokio::test]
async fn upsert_batch_dedupes_same_fingerprint_within_one_call() {
    // Two events with the same fingerprint in one batch must produce one
    // issue with event_count=2. The first slot's INSERT wins; the second
    // slot's UPDATE bumps the counter.
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let f = fp("dup");
    let ev1 = sample_event("Boom", "v1", "f", 1);
    let ev2 = sample_event("Boom", "v2", "f", 1);
    let (id1, pl1) = precomputed(&ev1);
    let (id2, pl2) = precomputed(&ev2);
    let batch = vec![
        BatchUpsertInput {
            project: "p",
            fp: &f,
            title: "T",
            culprit: None,
            level: None,
            now,
            event_id: &id1,
            payload: &pl1,
            release: None,
        },
        BatchUpsertInput {
            project: "p",
            fp: &f,
            title: "T",
            culprit: None,
            level: None,
            now,
            event_id: &id2,
            payload: &pl2,
            release: None,
        },
    ];
    let res = store.upsert_batch_with_events(&batch).await.unwrap();
    assert!(res[0].created);
    assert!(!res[1].created);
    assert_eq!(res[0].issue.id, res[1].issue.id);
    assert_eq!(res[1].issue.event_count, 2);
}

#[tokio::test]
async fn upsert_batch_flags_regression_for_resolved_issue() {
    // A new event arriving for a resolved issue must flip it back to
    // unresolved and surface `regressed=true` so callers can fire alerts.
    let (store, _dir) = fresh_store().await;
    let f = fp("reg");

    // Seed a resolved issue.
    let initial = store
        .upsert_issue("p", &f, "T", None, None, Utc::now())
        .await
        .unwrap();
    store
        .set_status(initial.issue.id, IssueStatus::Resolved, None)
        .await
        .unwrap();

    // Now batch a fresh event for that fingerprint.
    let ev = sample_event("Boom", "v", "f", 1);
    let (eid, pl) = precomputed(&ev);
    let batch = vec![BatchUpsertInput {
        project: "p",
        fp: &f,
        title: "T",
        culprit: None,
        level: None,
        now: Utc::now(),
        event_id: &eid,
        payload: &pl,
        release: None,
    }];
    let res = store.upsert_batch_with_events(&batch).await.unwrap();
    assert!(!res[0].created);
    assert!(res[0].regressed);
    assert_eq!(res[0].issue.status, IssueStatus::Unresolved);
}

#[tokio::test]
async fn upsert_batch_dedupes_event_id_via_unique_constraint() {
    // Same event_id replayed in a second batch must not double-count
    // (Sentry SDKs retry, we rely on the events.event_id UNIQUE index).
    let (store, _dir) = fresh_store().await;
    let f = fp("uniq");
    let ev = sample_event("Boom", "v", "f", 1);
    let (eid, pl) = precomputed(&ev);
    let batch1 = vec![BatchUpsertInput {
        project: "p",
        fp: &f,
        title: "T",
        culprit: None,
        level: None,
        now: Utc::now(),
        event_id: &eid,
        payload: &pl,
        release: None,
    }];
    store.upsert_batch_with_events(&batch1).await.unwrap();
    // Replay with the same event payload → ON CONFLICT silently no-ops the
    // event insert. The issue's count still bumps because that's a
    // different code path.
    let batch2 = vec![BatchUpsertInput {
        project: "p",
        fp: &f,
        title: "T",
        culprit: None,
        level: None,
        now: Utc::now(),
        event_id: &eid,
        payload: &pl,
        release: None,
    }];
    store.upsert_batch_with_events(&batch2).await.unwrap();

    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(row.0, 1, "duplicate event_id must not double-insert");
}

// ----- open / migrate -----

#[tokio::test]
async fn migrate_creates_issues_table() {
    let (store, _dir) = fresh_store().await;
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM issues")
        .fetch_one(store.pool())
        .await
        .expect("query issues");
    assert_eq!(row.0, 0);
}

#[tokio::test]
async fn migrate_creates_events_table() {
    let (store, _dir) = fresh_store().await;
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .expect("query events");
    assert_eq!(row.0, 0);
}

#[tokio::test]
async fn open_applies_perf_pragmas() {
    // Pin the pragmas tuned for "minimum hosting RAM". Trips a red bar
    // if a future refactor drifts them.
    let (store, _dir) = fresh_store().await;

    let busy: (i64,) = sqlx::query_as("PRAGMA busy_timeout")
        .fetch_one(store.pool())
        .await
        .expect("read busy_timeout");
    assert_eq!(busy.0, 5000, "busy_timeout should be 5000ms");

    let auto: (i64,) = sqlx::query_as("PRAGMA wal_autocheckpoint")
        .fetch_one(store.pool())
        .await
        .expect("read wal_autocheckpoint");
    assert_eq!(auto.0, 1000, "wal_autocheckpoint should be 1000 pages");

    // mmap disabled. SQLite reads via the regular page cache instead
    // of mmap; on a self-host workload that hits a few index pages
    // per request, the page cache is hot and the mmap overhead
    // (virtual mapping bookkeeping) wasn't earning its keep.
    let mmap: (i64,) = sqlx::query_as("PRAGMA mmap_size")
        .fetch_one(store.pool())
        .await
        .expect("read mmap_size");
    assert_eq!(mmap.0, 0, "mmap_size should be 0 (disabled)");

    // 1 MB page cache (negative value = bytes).
    let cache: (i64,) = sqlx::query_as("PRAGMA cache_size")
        .fetch_one(store.pool())
        .await
        .expect("read cache_size");
    assert_eq!(cache.0, -1024, "cache_size should be -1024 (1 MB)");

    let temp: (i64,) = sqlx::query_as("PRAGMA temp_store")
        .fetch_one(store.pool())
        .await
        .expect("read temp_store");
    // 0 = default, 1 = file, 2 = memory.
    assert_eq!(temp.0, 2, "temp_store should be MEMORY");

    // synchronous=NORMAL (1) — the standard WAL pairing; OFF can corrupt on power loss.
    let sync: (i64,) = sqlx::query_as("PRAGMA synchronous")
        .fetch_one(store.pool())
        .await
        .expect("read synchronous");
    assert_eq!(sync.0, 1, "synchronous should be NORMAL (1)");
}

#[tokio::test]
async fn migrate_is_idempotent() {
    let (store, dir) = fresh_store().await;
    // Run a second time — must not fail or duplicate rows.
    store.migrate().await.expect("second migrate");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- upsert_issue -----

#[tokio::test]
async fn upsert_issue_creates_new_row() {
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let r = store
        .upsert_issue(
            "p",
            &fp("abc"),
            "TypeError: x",
            Some("f in app.js"),
            Some("error"),
            now,
        )
        .await
        .unwrap();
    assert!(r.created, "first upsert must report created=true");
    assert_eq!(r.issue.event_count, 1);
    assert_eq!(r.issue.title, "TypeError: x");
    assert_eq!(r.issue.project, "p");
    assert_eq!(r.issue.fingerprint.as_str(), "abc");
    assert_eq!(r.issue.first_seen, now);
    assert_eq!(r.issue.last_seen, now);
}

#[tokio::test]
async fn upsert_issue_updates_existing_increments_count_and_bumps_last_seen() {
    let (store, _dir) = fresh_store().await;
    let t0 = Utc::now();
    let t1 = t0 + Duration::seconds(30);

    let first = store
        .upsert_issue("p", &fp("abc"), "T: x", None, None, t0)
        .await
        .unwrap();
    let second = store
        .upsert_issue("p", &fp("abc"), "T: x", None, None, t1)
        .await
        .unwrap();

    assert!(!second.created, "second upsert must report created=false");
    assert_eq!(second.issue.id, first.issue.id, "id must be stable");
    assert_eq!(second.issue.event_count, 2);
    assert_eq!(
        second.issue.first_seen, t0,
        "first_seen frozen at first upsert"
    );
    assert_eq!(second.issue.last_seen, t1, "last_seen tracks newest");
}

#[tokio::test]
async fn upsert_issue_does_not_overwrite_metadata_on_repeat() {
    // The first sighting's title/culprit/level are canonical; subsequent
    // events with refined metadata must not overwrite them. Otherwise UI
    // would flicker between titles when grouped events disagree.
    let (store, _dir) = fresh_store().await;
    let t0 = Utc::now();
    let _ = store
        .upsert_issue(
            "p",
            &fp("abc"),
            "First Title",
            Some("first culprit"),
            Some("error"),
            t0,
        )
        .await
        .unwrap();
    let r = store
        .upsert_issue(
            "p",
            &fp("abc"),
            "Second Title",
            Some("second culprit"),
            Some("warning"),
            t0 + Duration::seconds(1),
        )
        .await
        .unwrap();
    assert_eq!(r.issue.title, "First Title");
    assert_eq!(r.issue.culprit.as_deref(), Some("first culprit"));
    assert_eq!(r.issue.level.as_deref(), Some("error"));
}

#[tokio::test]
async fn upsert_issue_isolates_by_project() {
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let a = store
        .upsert_issue("proj-a", &fp("abc"), "T", None, None, now)
        .await
        .unwrap();
    let b = store
        .upsert_issue("proj-b", &fp("abc"), "T", None, None, now)
        .await
        .unwrap();
    assert!(
        a.created && b.created,
        "same fingerprint in different projects must create distinct issues"
    );
    assert_ne!(a.issue.id, b.issue.id);
}

#[tokio::test]
async fn upsert_issue_isolates_by_fingerprint() {
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let a = store
        .upsert_issue("p", &fp("abc"), "T", None, None, now)
        .await
        .unwrap();
    let b = store
        .upsert_issue("p", &fp("xyz"), "T", None, None, now)
        .await
        .unwrap();
    assert_ne!(a.issue.id, b.issue.id);
}

// ----- insert_event -----

#[tokio::test]
async fn insert_event_persists_payload() {
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let upsert = store
        .upsert_issue("p", &fp("abc"), "T", None, None, now)
        .await
        .unwrap();
    let ev = sample_event("TypeError", "boom", "f", 12);
    store.insert_event(upsert.issue.id, &ev).await.unwrap();

    let stored = store
        .latest_event(upsert.issue.id)
        .await
        .unwrap()
        .expect("event must be retrievable");
    assert_eq!(stored.event_id, ev.event_id.to_string());
    // The payload round-trips verbatim — exception type recoverable.
    let ty = stored
        .payload
        .get("exception")
        .and_then(|e| e.get("values"))
        .and_then(|v| v.get(0))
        .and_then(|v| v.get("type"))
        .and_then(|v| v.as_str());
    assert_eq!(ty, Some("TypeError"));
}

#[tokio::test]
async fn insert_event_is_idempotent_on_duplicate_event_id() {
    // Sentry SDKs retry on transient failures; an event_id that arrives
    // twice must not double-count.
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let upsert = store
        .upsert_issue("p", &fp("abc"), "T", None, None, now)
        .await
        .unwrap();
    let ev = sample_event("E", "v", "f", 1);
    store.insert_event(upsert.issue.id, &ev).await.unwrap();
    store.insert_event(upsert.issue.id, &ev).await.unwrap();
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events WHERE issue_id = ?")
        .bind(upsert.issue.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(row.0, 1, "duplicate event_id must collapse to a single row");
}

// ----- latest_event -----

#[tokio::test]
async fn latest_event_returns_none_when_no_events() {
    let (store, _dir) = fresh_store().await;
    let upsert = store
        .upsert_issue("p", &fp("abc"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let res = store.latest_event(upsert.issue.id).await.unwrap();
    assert!(res.is_none());
}

#[tokio::test]
async fn latest_event_returns_none_for_unknown_issue_id() {
    let (store, _dir) = fresh_store().await;
    let res = store.latest_event(999_999).await.unwrap();
    assert!(res.is_none());
}

#[tokio::test]
async fn latest_event_returns_most_recent() {
    let (store, _dir) = fresh_store().await;
    let upsert = store
        .upsert_issue("p", &fp("abc"), "T", None, None, Utc::now())
        .await
        .unwrap();

    let e1 = sample_event("E", "first", "f1", 1);
    store.insert_event(upsert.issue.id, &e1).await.unwrap();
    // Brief sleep so received_at strictly orders. SQLite's CURRENT_TIMESTAMP
    // resolution is seconds; tied timestamps fall back to `id DESC` per the
    // query, which still favors "most recent insert" — but exercising the
    // primary order keeps the contract honest.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let e2 = sample_event("E", "second", "f2", 2);
    store.insert_event(upsert.issue.id, &e2).await.unwrap();

    let stored = store.latest_event(upsert.issue.id).await.unwrap().unwrap();
    let value = stored
        .payload
        .get("exception")
        .and_then(|e| e.get("values"))
        .and_then(|v| v.get(0))
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str());
    assert_eq!(value, Some("second"));
}

// ----- load_issues / list_issues_by_project / project_summaries -----

#[tokio::test]
async fn load_issues_orders_newest_first() {
    let (store, _dir) = fresh_store().await;
    let t0 = Utc::now();
    let _a = store
        .upsert_issue("p", &fp("a"), "Old", None, None, t0)
        .await
        .unwrap();
    let _b = store
        .upsert_issue(
            "p",
            &fp("b"),
            "Newer",
            None,
            None,
            t0 + Duration::seconds(10),
        )
        .await
        .unwrap();
    let _c = store
        .upsert_issue(
            "p",
            &fp("c"),
            "Newest",
            None,
            None,
            t0 + Duration::seconds(20),
        )
        .await
        .unwrap();
    let list = store.load_issues().await.unwrap();
    let titles: Vec<_> = list.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(titles, vec!["Newest", "Newer", "Old"]);
}

#[tokio::test]
async fn list_issues_by_project_filters() {
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let _ = store
        .upsert_issue("p1", &fp("a"), "in p1", None, None, now)
        .await
        .unwrap();
    let _ = store
        .upsert_issue("p2", &fp("a"), "in p2", None, None, now)
        .await
        .unwrap();
    let _ = store
        .upsert_issue("p1", &fp("b"), "also p1", None, None, now)
        .await
        .unwrap();
    let only_p1 = store.list_issues_by_project("p1").await.unwrap();
    assert_eq!(only_p1.len(), 2);
    assert!(only_p1.iter().all(|i| i.project == "p1"));
}

#[tokio::test]
async fn list_issues_by_project_returns_empty_for_unknown() {
    let (store, _dir) = fresh_store().await;
    let res = store
        .list_issues_by_project("does-not-exist")
        .await
        .unwrap();
    assert!(res.is_empty());
}

#[tokio::test]
async fn project_summaries_groups_and_counts() {
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let _ = store
        .upsert_issue("alpha", &fp("a"), "T", None, None, now)
        .await
        .unwrap();
    let _ = store
        .upsert_issue("alpha", &fp("b"), "T", None, None, now)
        .await
        .unwrap();
    let _ = store
        .upsert_issue("beta", &fp("c"), "T", None, None, now)
        .await
        .unwrap();
    // Repeat ingest of an existing fingerprint must NOT inflate the count.
    let _ = store
        .upsert_issue("alpha", &fp("a"), "T", None, None, now)
        .await
        .unwrap();

    let summaries = store.project_summaries().await.unwrap();
    let map: std::collections::HashMap<_, _> = summaries
        .iter()
        .map(|s| (s.project.clone(), s.issue_count))
        .collect();
    assert_eq!(map.get("alpha").copied(), Some(2));
    assert_eq!(map.get("beta").copied(), Some(1));
}

#[tokio::test]
async fn project_summaries_is_alphabetical() {
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let _ = store
        .upsert_issue("zeta", &fp("a"), "T", None, None, now)
        .await
        .unwrap();
    let _ = store
        .upsert_issue("alpha", &fp("a"), "T", None, None, now)
        .await
        .unwrap();
    let _ = store
        .upsert_issue("mike", &fp("a"), "T", None, None, now)
        .await
        .unwrap();
    let summaries = store.project_summaries().await.unwrap();
    let projects: Vec<_> = summaries.iter().map(|s| s.project.as_str()).collect();
    assert_eq!(projects, vec!["alpha", "mike", "zeta"]);
}

// ----- status -----

#[tokio::test]
async fn newly_created_issue_has_unresolved_status() {
    let (store, _dir) = fresh_store().await;
    let r = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    assert_eq!(r.issue.status, IssueStatus::Unresolved);
    assert!(!r.regressed, "create must not flag regression");
}

#[tokio::test]
async fn set_status_persists_and_round_trips() {
    let (store, _dir) = fresh_store().await;
    let r = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    store
        .set_status(r.issue.id, IssueStatus::Resolved, None)
        .await
        .unwrap();
    let issues = store.load_issues().await.unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].status, IssueStatus::Resolved);
}

#[tokio::test]
async fn set_status_returns_updated_issue() {
    let (store, _dir) = fresh_store().await;
    let r = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let updated = store
        .set_status(r.issue.id, IssueStatus::Muted, None)
        .await
        .unwrap();
    assert_eq!(updated.id, r.issue.id);
    assert_eq!(updated.status, IssueStatus::Muted);
}

#[tokio::test]
async fn set_status_on_unknown_issue_is_an_error() {
    let (store, _dir) = fresh_store().await;
    let res = store.set_status(999_999, IssueStatus::Resolved, None).await;
    assert!(res.is_err(), "missing issue must surface as Err");
}

#[tokio::test]
async fn ingesting_event_for_resolved_issue_regresses_to_unresolved() {
    let (store, _dir) = fresh_store().await;
    let t0 = Utc::now();
    let r1 = store
        .upsert_issue("p", &fp("a"), "T", None, None, t0)
        .await
        .unwrap();
    store
        .set_status(r1.issue.id, IssueStatus::Resolved, None)
        .await
        .unwrap();
    let r2 = store
        .upsert_issue("p", &fp("a"), "T", None, None, t0 + Duration::seconds(60))
        .await
        .unwrap();
    assert_eq!(r2.issue.status, IssueStatus::Unresolved);
    assert!(
        r2.regressed,
        "upsert into resolved issue must flag regression"
    );
}

#[tokio::test]
async fn upsert_keeps_muted_status() {
    let (store, _dir) = fresh_store().await;
    let r1 = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    store
        .set_status(r1.issue.id, IssueStatus::Muted, None)
        .await
        .unwrap();
    let r2 = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    assert_eq!(r2.issue.status, IssueStatus::Muted);
    assert!(!r2.regressed);
}

#[tokio::test]
async fn upsert_keeps_ignored_status() {
    let (store, _dir) = fresh_store().await;
    let r1 = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    store
        .set_status(r1.issue.id, IssueStatus::Ignored, None)
        .await
        .unwrap();
    let r2 = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    assert_eq!(r2.issue.status, IssueStatus::Ignored);
}

// ----- projects (admin / DSN tokens) -----

#[tokio::test]
async fn create_project_returns_record_with_nonempty_token() {
    let (store, _dir) = fresh_store().await;
    let p = store.create_project("my-app").await.unwrap();
    assert_eq!(p.name, "my-app");
    assert!(
        p.token.len() >= 16,
        "token must have meaningful entropy: got {}",
        p.token
    );
}

#[tokio::test]
async fn create_project_rejects_duplicate_name() {
    let (store, _dir) = fresh_store().await;
    store.create_project("dup").await.unwrap();
    let res = store.create_project("dup").await;
    assert!(res.is_err(), "second create with same name must fail");
}

#[tokio::test]
async fn project_by_name_returns_none_for_unknown() {
    let (store, _dir) = fresh_store().await;
    let p = store.project_by_name("ghost").await.unwrap();
    assert!(p.is_none());
}

#[tokio::test]
async fn project_by_token_returns_matching_project() {
    let (store, _dir) = fresh_store().await;
    let created = store.create_project("alpha").await.unwrap();
    let by_token = store.project_by_token(&created.token).await.unwrap();
    assert!(by_token.is_some());
    assert_eq!(by_token.unwrap().name, "alpha");
}

#[tokio::test]
async fn project_by_token_returns_none_for_unknown_token() {
    let (store, _dir) = fresh_store().await;
    let _ = store.create_project("p").await.unwrap();
    let res = store.project_by_token("not-a-real-token").await.unwrap();
    assert!(res.is_none());
}

#[tokio::test]
async fn list_admin_projects_orders_alphabetically() {
    let (store, _dir) = fresh_store().await;
    store.create_project("zeta").await.unwrap();
    store.create_project("alpha").await.unwrap();
    store.create_project("mike").await.unwrap();
    let list = store.list_admin_projects().await.unwrap();
    let names: Vec<_> = list.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "mike", "zeta"]);
}

#[tokio::test]
async fn rotate_token_changes_token_and_invalidates_old() {
    let (store, _dir) = fresh_store().await;
    let created = store.create_project("p").await.unwrap();
    let rotated = store.rotate_token("p").await.unwrap();
    assert_ne!(created.token, rotated.token);
    assert!(store
        .project_by_token(&created.token)
        .await
        .unwrap()
        .is_none());
    assert!(store
        .project_by_token(&rotated.token)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn rotate_token_on_unknown_project_errors() {
    let (store, _dir) = fresh_store().await;
    let res = store.rotate_token("ghost").await;
    assert!(res.is_err());
}

// ----- webhook URL -----

#[tokio::test]
async fn set_project_webhook_persists() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    store
        .set_project_webhook("p", Some("https://hooks.slack.com/services/XXX"))
        .await
        .unwrap();
    let p = store.project_by_name("p").await.unwrap().unwrap();
    assert_eq!(
        p.webhook_url.as_deref(),
        Some("https://hooks.slack.com/services/XXX")
    );
}

#[tokio::test]
async fn set_project_webhook_to_none_clears() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    store
        .set_project_webhook("p", Some("https://x"))
        .await
        .unwrap();
    store.set_project_webhook("p", None).await.unwrap();
    let p = store.project_by_name("p").await.unwrap().unwrap();
    assert!(p.webhook_url.is_none());
}

#[tokio::test]
async fn set_project_webhook_on_unknown_project_errors() {
    let (store, _dir) = fresh_store().await;
    let res = store.set_project_webhook("ghost", Some("https://x")).await;
    assert!(res.is_err());
}

// ----- retention -----

#[tokio::test]
async fn purge_events_older_than_deletes_only_old_rows() {
    let (store, _dir) = fresh_store().await;
    let r = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let issue_id = r.issue.id;

    // Insert 3 events at t=now and 2 at t=10 days ago (via direct UPDATE
    // to backdate; the API doesn't take received_at).
    for i in 0..3 {
        let mut ev = sample_event("E", &format!("recent-{i}"), "f", i);
        ev.event_id = Uuid::new_v4();
        store.insert_event(issue_id, &ev).await.unwrap();
    }
    for i in 0..2 {
        let mut ev = sample_event("E", &format!("old-{i}"), "f", 100 + i);
        ev.event_id = Uuid::new_v4();
        store.insert_event(issue_id, &ev).await.unwrap();
        sqlx::query("UPDATE events SET received_at = ? WHERE event_id = ?")
            .bind((Utc::now() - Duration::days(10)).to_rfc3339())
            .bind(ev.event_id.to_string())
            .execute(store.pool())
            .await
            .unwrap();
    }

    let cutoff = Utc::now() - Duration::days(7);
    let deleted = store.purge_events_older_than(cutoff).await.unwrap();
    assert_eq!(deleted, 2);

    let remaining: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(remaining.0, 3);
}

#[tokio::test]
async fn purge_events_older_than_keeps_issue_row() {
    // Issue rows live longer than their events — a high-count issue from
    // months ago is still useful "context" even after retention prunes its
    // payloads.
    let (store, _dir) = fresh_store().await;
    let r = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let mut ev = sample_event("E", "old", "f", 1);
    ev.event_id = Uuid::new_v4();
    store.insert_event(r.issue.id, &ev).await.unwrap();
    sqlx::query("UPDATE events SET received_at = ? WHERE event_id = ?")
        .bind((Utc::now() - Duration::days(60)).to_rfc3339())
        .bind(ev.event_id.to_string())
        .execute(store.pool())
        .await
        .unwrap();

    store
        .purge_events_older_than(Utc::now() - Duration::days(7))
        .await
        .unwrap();

    let issues = store.load_issues().await.unwrap();
    assert_eq!(issues.len(), 1, "issue must survive event purge");
    assert_eq!(issues[0].id, r.issue.id);
}

#[tokio::test]
async fn purge_events_older_than_with_no_old_rows_returns_zero() {
    let (store, _dir) = fresh_store().await;
    let deleted = store
        .purge_events_older_than(Utc::now() - Duration::days(7))
        .await
        .unwrap();
    assert_eq!(deleted, 0);
}

// ----- persistence -----

#[tokio::test]
async fn data_survives_close_and_reopen() {
    let dir = unique_tempdir();
    let db = dir.join("errex.db");

    {
        let store = Store::open(&db).await.unwrap();
        store.migrate().await.unwrap();
        let upsert = store
            .upsert_issue("p", &fp("abc"), "T: persisted", None, None, Utc::now())
            .await
            .unwrap();
        let ev = sample_event("T", "persisted", "f", 1);
        store.insert_event(upsert.issue.id, &ev).await.unwrap();
        // Drop the store (and pool) to flush WAL on next reopen.
    }

    let store = Store::open(&db).await.unwrap();
    store.migrate().await.unwrap();
    let issues = store.load_issues().await.unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].title, "T: persisted");
    let ev = store.latest_event(issues[0].id).await.unwrap();
    assert!(ev.is_some(), "event payload must survive restart");
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- delete_project (cascade) -----

#[tokio::test]
async fn delete_project_returns_summary_of_deleted_rows() {
    let (store, _dir) = fresh_store().await;
    store.create_project("doomed").await.unwrap();
    let r1 = store
        .upsert_issue("doomed", &fp("a"), "T1", None, None, Utc::now())
        .await
        .unwrap();
    let r2 = store
        .upsert_issue("doomed", &fp("b"), "T2", None, None, Utc::now())
        .await
        .unwrap();
    for i in 0..3 {
        let mut ev = sample_event("E", &format!("e{i}"), "f", i);
        ev.event_id = Uuid::new_v4();
        store.insert_event(r1.issue.id, &ev).await.unwrap();
    }
    let mut ev = sample_event("E", "lone", "f", 99);
    ev.event_id = Uuid::new_v4();
    store.insert_event(r2.issue.id, &ev).await.unwrap();

    let summary = store.delete_project("doomed").await.unwrap();
    assert_eq!(summary.issues_deleted, 2);
    assert_eq!(summary.events_deleted, 4);
}

#[tokio::test]
async fn delete_project_removes_project_row() {
    let (store, _dir) = fresh_store().await;
    store.create_project("doomed").await.unwrap();
    store.delete_project("doomed").await.unwrap();
    assert!(store.project_by_name("doomed").await.unwrap().is_none());
}

#[tokio::test]
async fn delete_project_cascades_issues_and_events() {
    let (store, _dir) = fresh_store().await;
    store.create_project("doomed").await.unwrap();
    let r = store
        .upsert_issue("doomed", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let mut ev = sample_event("E", "v", "f", 1);
    ev.event_id = Uuid::new_v4();
    store.insert_event(r.issue.id, &ev).await.unwrap();

    store.delete_project("doomed").await.unwrap();

    let issues = store.list_issues_by_project("doomed").await.unwrap();
    assert!(issues.is_empty(), "issues must be gone");
    let leftover: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(leftover.0, 0, "events must cascade with issues");
}

#[tokio::test]
async fn delete_project_does_not_touch_other_projects() {
    let (store, _dir) = fresh_store().await;
    store.create_project("doomed").await.unwrap();
    store.create_project("survivor").await.unwrap();
    store
        .upsert_issue("doomed", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let r = store
        .upsert_issue("survivor", &fp("b"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let mut ev = sample_event("E", "v", "f", 1);
    ev.event_id = Uuid::new_v4();
    store.insert_event(r.issue.id, &ev).await.unwrap();

    store.delete_project("doomed").await.unwrap();

    assert!(store.project_by_name("survivor").await.unwrap().is_some());
    let surviving = store.list_issues_by_project("survivor").await.unwrap();
    assert_eq!(surviving.len(), 1);
}

#[tokio::test]
async fn delete_project_unknown_returns_not_found() {
    let (store, _dir) = fresh_store().await;
    let res = store.delete_project("ghost").await;
    assert!(res.is_err());
}

#[tokio::test]
async fn delete_preview_returns_counts_without_deleting() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    let r = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let mut ev = sample_event("E", "v", "f", 1);
    ev.event_id = Uuid::new_v4();
    store.insert_event(r.issue.id, &ev).await.unwrap();

    let preview = store.delete_preview("p").await.unwrap();
    assert_eq!(preview.issues_deleted, 1);
    assert_eq!(preview.events_deleted, 1);

    // Still there.
    assert!(store.project_by_name("p").await.unwrap().is_some());
    assert_eq!(store.list_issues_by_project("p").await.unwrap().len(), 1);
}

#[tokio::test]
async fn delete_preview_unknown_returns_not_found() {
    let (store, _dir) = fresh_store().await;
    let res = store.delete_preview("ghost").await;
    assert!(res.is_err());
}

// ----- rename_project -----

#[tokio::test]
async fn rename_project_changes_name_and_returns_record() {
    let (store, _dir) = fresh_store().await;
    let original = store.create_project("old-name").await.unwrap();
    let renamed = store.rename_project("old-name", "new-name").await.unwrap();
    assert_eq!(renamed.name, "new-name");
    assert_eq!(renamed.token, original.token, "token must be preserved");
    assert!(store.project_by_name("old-name").await.unwrap().is_none());
    assert!(store.project_by_name("new-name").await.unwrap().is_some());
}

#[tokio::test]
async fn rename_project_cascades_to_issues() {
    let (store, _dir) = fresh_store().await;
    store.create_project("old-name").await.unwrap();
    store
        .upsert_issue("old-name", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    store
        .upsert_issue("old-name", &fp("b"), "T2", None, None, Utc::now())
        .await
        .unwrap();

    store.rename_project("old-name", "new-name").await.unwrap();

    assert!(store
        .list_issues_by_project("old-name")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .list_issues_by_project("new-name")
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn rename_project_unknown_source_errors() {
    let (store, _dir) = fresh_store().await;
    let res = store.rename_project("ghost", "anything").await;
    assert!(res.is_err());
}

#[tokio::test]
async fn rename_project_to_existing_name_errors() {
    let (store, _dir) = fresh_store().await;
    store.create_project("a").await.unwrap();
    store.create_project("b").await.unwrap();
    let res = store.rename_project("a", "b").await;
    assert!(res.is_err(), "rename onto existing name must fail");
    // Both must still exist with original names.
    assert!(store.project_by_name("a").await.unwrap().is_some());
    assert!(store.project_by_name("b").await.unwrap().is_some());
}

#[tokio::test]
async fn rename_project_to_same_name_is_noop_success() {
    let (store, _dir) = fresh_store().await;
    let original = store.create_project("p").await.unwrap();
    let renamed = store.rename_project("p", "p").await.unwrap();
    assert_eq!(renamed.name, "p");
    assert_eq!(renamed.token, original.token);
}

// ----- record_webhook_attempt -----

#[tokio::test]
async fn record_webhook_attempt_persists_status_and_timestamp() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    store.record_webhook_attempt("p", 200).await;
    let p = store.project_by_name("p").await.unwrap().unwrap();
    assert_eq!(p.last_webhook_status, Some(200));
    assert!(p.last_webhook_at.is_some(), "timestamp must be recorded");
}

#[tokio::test]
async fn record_webhook_attempt_overwrites_previous() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    store.record_webhook_attempt("p", 200).await;
    store.record_webhook_attempt("p", 502).await;
    let p = store.project_by_name("p").await.unwrap().unwrap();
    assert_eq!(p.last_webhook_status, Some(502));
}

#[tokio::test]
async fn record_webhook_attempt_zero_means_network_error() {
    // Convention: 0 = transport-level failure (no HTTP status returned).
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    store.record_webhook_attempt("p", 0).await;
    let p = store.project_by_name("p").await.unwrap().unwrap();
    assert_eq!(p.last_webhook_status, Some(0));
}

#[tokio::test]
async fn record_webhook_attempt_unknown_project_is_silent_noop() {
    // Best-effort: unknown project is logged but does not panic. (We don't
    // want a webhook hot-path to crash on a race with delete_project.)
    let (store, _dir) = fresh_store().await;
    store.record_webhook_attempt("ghost", 200).await;
    // No assert needed — must just not panic.
}

// ----- activity_stats -----

#[tokio::test]
async fn activity_stats_returns_zeros_for_project_with_no_events() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    let now = Utc::now();
    let stats = store.activity_stats("p", now).await.unwrap();
    assert_eq!(stats.events_24h, 0);
    assert_eq!(stats.unique_issues_24h, 0);
    assert!(stats.last_event_at.is_none());
    assert_eq!(stats.hourly_buckets.len(), 24);
    assert!(stats.hourly_buckets.iter().all(|&n| n == 0));
}

#[tokio::test]
async fn activity_stats_counts_events_in_last_24h() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    let r = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    for i in 0..5 {
        let mut ev = sample_event("E", &format!("e{i}"), "f", i);
        ev.event_id = Uuid::new_v4();
        store.insert_event(r.issue.id, &ev).await.unwrap();
    }
    let stats = store.activity_stats("p", Utc::now()).await.unwrap();
    assert_eq!(stats.events_24h, 5);
}

#[tokio::test]
async fn activity_stats_excludes_events_older_than_24h() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    let r = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    // Two recent, two ancient.
    for i in 0..2 {
        let mut ev = sample_event("E", &format!("recent-{i}"), "f", i);
        ev.event_id = Uuid::new_v4();
        store.insert_event(r.issue.id, &ev).await.unwrap();
    }
    for i in 0..2 {
        let mut ev = sample_event("E", &format!("old-{i}"), "f", 100 + i);
        ev.event_id = Uuid::new_v4();
        store.insert_event(r.issue.id, &ev).await.unwrap();
        sqlx::query("UPDATE events SET received_at = ? WHERE event_id = ?")
            .bind((Utc::now() - Duration::days(2)).to_rfc3339())
            .bind(ev.event_id.to_string())
            .execute(store.pool())
            .await
            .unwrap();
    }
    let stats = store.activity_stats("p", Utc::now()).await.unwrap();
    assert_eq!(stats.events_24h, 2);
}

#[tokio::test]
async fn activity_stats_counts_unique_issues_in_last_24h() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    let r1 = store
        .upsert_issue("p", &fp("a"), "T1", None, None, Utc::now())
        .await
        .unwrap();
    let r2 = store
        .upsert_issue("p", &fp("b"), "T2", None, None, Utc::now())
        .await
        .unwrap();
    // 3 events on issue 1, 1 event on issue 2 → 2 unique issues.
    for i in 0..3 {
        let mut ev = sample_event("E", &format!("a{i}"), "f", i);
        ev.event_id = Uuid::new_v4();
        store.insert_event(r1.issue.id, &ev).await.unwrap();
    }
    let mut ev = sample_event("E", "b0", "f", 99);
    ev.event_id = Uuid::new_v4();
    store.insert_event(r2.issue.id, &ev).await.unwrap();

    let stats = store.activity_stats("p", Utc::now()).await.unwrap();
    assert_eq!(stats.unique_issues_24h, 2);
}

#[tokio::test]
async fn activity_stats_returns_last_event_timestamp() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    let r = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let mut ev = sample_event("E", "v", "f", 1);
    ev.event_id = Uuid::new_v4();
    store.insert_event(r.issue.id, &ev).await.unwrap();

    let stats = store.activity_stats("p", Utc::now()).await.unwrap();
    assert!(
        stats.last_event_at.is_some(),
        "must surface most recent event timestamp"
    );
}

#[tokio::test]
async fn activity_stats_buckets_events_by_hour() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    let r = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let now = Utc::now();
    // Place 1 event each at "1 hour ago" and "5 hours ago" — they must land
    // in distinct, non-zero buckets.
    let inserts = [(1i64, "h1"), (5, "h5"), (5, "h5b")];
    for (hours_ago, tag) in inserts {
        let mut ev = sample_event("E", tag, "f", hours_ago as u32);
        ev.event_id = Uuid::new_v4();
        store.insert_event(r.issue.id, &ev).await.unwrap();
        sqlx::query("UPDATE events SET received_at = ? WHERE event_id = ?")
            .bind((now - Duration::hours(hours_ago)).to_rfc3339())
            .bind(ev.event_id.to_string())
            .execute(store.pool())
            .await
            .unwrap();
    }

    let stats = store.activity_stats("p", now).await.unwrap();
    assert_eq!(stats.hourly_buckets.len(), 24);
    let total: i64 = stats.hourly_buckets.iter().sum();
    assert_eq!(total, 3, "all three events must be present in some bucket");
    let nonzero_buckets = stats.hourly_buckets.iter().filter(|&&n| n > 0).count();
    assert_eq!(
        nonzero_buckets, 2,
        "events 1h-ago and 5h-ago should be in two distinct buckets"
    );
}

#[tokio::test]
async fn activity_stats_excludes_other_projects() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    store.create_project("other").await.unwrap();
    let r_p = store
        .upsert_issue("p", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let r_o = store
        .upsert_issue("other", &fp("a"), "T", None, None, Utc::now())
        .await
        .unwrap();
    for i in 0..3 {
        let mut ev = sample_event("E", &format!("p{i}"), "f", i);
        ev.event_id = Uuid::new_v4();
        store.insert_event(r_p.issue.id, &ev).await.unwrap();
    }
    for i in 0..7 {
        let mut ev = sample_event("E", &format!("o{i}"), "f", i);
        ev.event_id = Uuid::new_v4();
        store.insert_event(r_o.issue.id, &ev).await.unwrap();
    }
    let stats = store.activity_stats("p", Utc::now()).await.unwrap();
    assert_eq!(stats.events_24h, 3);
    assert_eq!(stats.unique_issues_24h, 1);
}

// ----- users -----

#[tokio::test]
async fn user_count_starts_at_zero() {
    let (store, _dir) = fresh_store().await;
    assert_eq!(store.user_count().await.unwrap(), 0);
}

#[tokio::test]
async fn create_user_persists_and_returns_record() {
    let (store, _dir) = fresh_store().await;
    let u = store
        .create_user("daisy", "argon2-stored-hash", Role::Admin)
        .await
        .unwrap();
    assert_eq!(u.username, "daisy");
    assert_eq!(u.role, Role::Admin);
    assert!(u.last_login_at.is_none());
    assert!(u.deactivated_at.is_none());
    assert_eq!(store.user_count().await.unwrap(), 1);
}

#[tokio::test]
async fn create_user_rejects_duplicate_username() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Admin).await.unwrap();
    let res = store.create_user("daisy", "h2", Role::Viewer).await;
    assert!(res.is_err());
}

#[tokio::test]
async fn list_users_orders_alphabetically() {
    let (store, _dir) = fresh_store().await;
    store.create_user("zoe", "h", Role::Viewer).await.unwrap();
    store.create_user("alex", "h", Role::Admin).await.unwrap();
    store.create_user("max", "h", Role::Viewer).await.unwrap();
    let names: Vec<_> = store
        .list_users()
        .await
        .unwrap()
        .into_iter()
        .map(|u| u.username)
        .collect();
    assert_eq!(names, vec!["alex", "max", "zoe"]);
}

#[tokio::test]
async fn get_user_returns_none_for_unknown() {
    let (store, _dir) = fresh_store().await;
    assert!(store.get_user("ghost").await.unwrap().is_none());
}

#[tokio::test]
async fn user_view_never_carries_password_hash() {
    // Defense-in-depth: the public User struct must not have a password_hash
    // field. If someone adds one, this will fail to compile (no field) —
    // the assertion is just here so the test name is greppable.
    let (store, _dir) = fresh_store().await;
    let u = store
        .create_user("daisy", "secret-hash", Role::Admin)
        .await
        .unwrap();
    let _ = u.username; // touch fields to keep the compiler honest
    let _ = u.role;
    let json = serde_json::to_string(&u).unwrap();
    assert!(
        !json.contains("password"),
        "serialized User must not leak password fields: {json}"
    );
    assert!(
        !json.contains("secret-hash"),
        "serialized User must not contain the hash itself"
    );
}

#[tokio::test]
async fn get_user_password_hash_returns_what_was_stored() {
    let (store, _dir) = fresh_store().await;
    store
        .create_user("daisy", "stored-hash", Role::Admin)
        .await
        .unwrap();
    let h = store.get_user_password_hash("daisy").await.unwrap();
    assert_eq!(h.as_deref(), Some("stored-hash"));
}

#[tokio::test]
async fn set_user_password_persists() {
    let (store, _dir) = fresh_store().await;
    store
        .create_user("daisy", "old", Role::Admin)
        .await
        .unwrap();
    store.set_user_password("daisy", "new").await.unwrap();
    assert_eq!(
        store
            .get_user_password_hash("daisy")
            .await
            .unwrap()
            .as_deref(),
        Some("new")
    );
}

#[tokio::test]
async fn set_user_password_unknown_user_errors() {
    let (store, _dir) = fresh_store().await;
    assert!(store.set_user_password("ghost", "x").await.is_err());
}

#[tokio::test]
async fn set_user_role_changes_role() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Viewer).await.unwrap();
    store.set_user_role("daisy", Role::Admin).await.unwrap();
    let u = store.get_user("daisy").await.unwrap().unwrap();
    assert_eq!(u.role, Role::Admin);
}

#[tokio::test]
async fn deactivate_user_sets_timestamp_and_revokes_sessions() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Viewer).await.unwrap();
    store
        .create_session("sess1", "daisy", Some("1.2.3.4"), None)
        .await
        .unwrap();
    store
        .create_session("sess2", "daisy", Some("5.6.7.8"), None)
        .await
        .unwrap();
    store.set_user_deactivated("daisy", true).await.unwrap();
    let u = store.get_user("daisy").await.unwrap().unwrap();
    assert!(u.deactivated_at.is_some());
    assert!(
        store.list_user_sessions("daisy").await.unwrap().is_empty(),
        "deactivation must revoke every existing session"
    );
}

#[tokio::test]
async fn deactivate_user_can_be_reversed() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Viewer).await.unwrap();
    store.set_user_deactivated("daisy", true).await.unwrap();
    store.set_user_deactivated("daisy", false).await.unwrap();
    let u = store.get_user("daisy").await.unwrap().unwrap();
    assert!(u.deactivated_at.is_none());
}

#[tokio::test]
async fn delete_user_cascades_sessions() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Viewer).await.unwrap();
    store
        .create_session("s1", "daisy", None, None)
        .await
        .unwrap();
    store.delete_user("daisy").await.unwrap();
    assert!(store.session_for_id("s1").await.unwrap().is_none());
    assert!(store.get_user("daisy").await.unwrap().is_none());
}

#[tokio::test]
async fn delete_user_unknown_errors() {
    let (store, _dir) = fresh_store().await;
    assert!(store.delete_user("ghost").await.is_err());
}

#[tokio::test]
async fn touch_user_login_updates_timestamp_and_ip() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Admin).await.unwrap();
    store.touch_user_login("daisy", Some("9.9.9.9")).await;
    let u = store.get_user("daisy").await.unwrap().unwrap();
    assert!(u.last_login_at.is_some());
    assert_eq!(u.last_login_ip.as_deref(), Some("9.9.9.9"));
}

#[tokio::test]
async fn count_active_admins_excludes_deactivated() {
    let (store, _dir) = fresh_store().await;
    store.create_user("a1", "h", Role::Admin).await.unwrap();
    store.create_user("a2", "h", Role::Admin).await.unwrap();
    store.create_user("v", "h", Role::Viewer).await.unwrap();
    assert_eq!(store.count_active_admins().await.unwrap(), 2);
    store.set_user_deactivated("a2", true).await.unwrap();
    assert_eq!(store.count_active_admins().await.unwrap(), 1);
}

#[tokio::test]
async fn role_from_db_str_defaults_to_viewer_on_garbage() {
    // Critical: a corrupt column must NEVER silently grant admin. The
    // application enum maps unknown values to viewer (least privilege).
    assert_eq!(Role::from_db_str("admin"), Role::Admin);
    assert_eq!(Role::from_db_str("viewer"), Role::Viewer);
    assert_eq!(Role::from_db_str("ghost"), Role::Viewer);
    assert_eq!(Role::from_db_str(""), Role::Viewer);
}

// ----- sessions -----

#[tokio::test]
async fn create_session_returns_record() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Admin).await.unwrap();
    let s = store
        .create_session("abc123", "daisy", Some("1.2.3.4"), Some("Mozilla"))
        .await
        .unwrap();
    assert_eq!(s.id, "abc123");
    assert_eq!(s.username, "daisy");
    assert_eq!(s.ip.as_deref(), Some("1.2.3.4"));
}

#[tokio::test]
async fn session_for_id_returns_role_and_deactivated_flag() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Admin).await.unwrap();
    store
        .create_session("s1", "daisy", None, None)
        .await
        .unwrap();
    let (s, role, deactivated) = store.session_for_id("s1").await.unwrap().unwrap();
    assert_eq!(s.username, "daisy");
    assert_eq!(role, Role::Admin);
    assert!(!deactivated);
}

#[tokio::test]
async fn session_for_id_reflects_deactivation() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Admin).await.unwrap();
    store
        .create_session("s1", "daisy", None, None)
        .await
        .unwrap();
    store.set_user_deactivated("daisy", true).await.unwrap();
    // Sessions are revoked on deactivation.
    assert!(store.session_for_id("s1").await.unwrap().is_none());
}

#[tokio::test]
async fn session_for_id_returns_none_for_unknown() {
    let (store, _dir) = fresh_store().await;
    assert!(store.session_for_id("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn touch_session_advances_last_seen() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Admin).await.unwrap();
    store
        .create_session("s1", "daisy", None, None)
        .await
        .unwrap();
    let before = store
        .session_for_id("s1")
        .await
        .unwrap()
        .unwrap()
        .0
        .last_seen_at;
    // Sleep a tick so the timestamp can move at second precision.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    store.touch_session("s1").await;
    let after = store
        .session_for_id("s1")
        .await
        .unwrap()
        .unwrap()
        .0
        .last_seen_at;
    assert!(after > before, "touch must advance last_seen_at");
}

#[tokio::test]
async fn revoke_session_removes_only_target() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Admin).await.unwrap();
    store
        .create_session("a", "daisy", None, None)
        .await
        .unwrap();
    store
        .create_session("b", "daisy", None, None)
        .await
        .unwrap();
    store.revoke_session("a").await.unwrap();
    assert!(store.session_for_id("a").await.unwrap().is_none());
    assert!(store.session_for_id("b").await.unwrap().is_some());
}

#[tokio::test]
async fn revoke_user_sessions_returns_count() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Admin).await.unwrap();
    store
        .create_session("a", "daisy", None, None)
        .await
        .unwrap();
    store
        .create_session("b", "daisy", None, None)
        .await
        .unwrap();
    store
        .create_session("c", "daisy", None, None)
        .await
        .unwrap();
    let n = store.revoke_user_sessions("daisy").await.unwrap();
    assert_eq!(n, 3);
    assert!(store.list_user_sessions("daisy").await.unwrap().is_empty());
}

#[tokio::test]
async fn list_user_sessions_orders_by_last_seen_desc() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Admin).await.unwrap();
    store
        .create_session("old", "daisy", None, None)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    store
        .create_session("new", "daisy", None, None)
        .await
        .unwrap();
    let ids: Vec<_> = store
        .list_user_sessions("daisy")
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec!["new", "old"]);
}

#[tokio::test]
async fn purge_sessions_idle_since_drops_only_old() {
    let (store, _dir) = fresh_store().await;
    store.create_user("daisy", "h", Role::Admin).await.unwrap();
    store
        .create_session("recent", "daisy", None, None)
        .await
        .unwrap();
    store
        .create_session("old", "daisy", None, None)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET last_seen_at = ? WHERE id = ?")
        .bind((Utc::now() - Duration::days(60)).to_rfc3339())
        .bind("old")
        .execute(store.pool())
        .await
        .unwrap();
    let n = store
        .purge_sessions_idle_since(Utc::now() - Duration::days(30))
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert!(store.session_for_id("old").await.unwrap().is_none());
    assert!(store.session_for_id("recent").await.unwrap().is_some());
}

// ----- auth_attempts -----

#[tokio::test]
async fn record_attempt_persists_with_username_and_ip() {
    let (store, _dir) = fresh_store().await;
    store.record_attempt(Some("daisy"), "1.2.3.4", false).await;
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM auth_attempts WHERE username = 'daisy' AND ip = '1.2.3.4'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(row.0, 1);
}

#[tokio::test]
async fn record_attempt_supports_null_username() {
    let (store, _dir) = fresh_store().await;
    store.record_attempt(None, "1.2.3.4", false).await;
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM auth_attempts WHERE username IS NULL AND ip = '1.2.3.4'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(row.0, 1);
}

#[tokio::test]
async fn count_recent_failures_for_username_excludes_successes() {
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    for _ in 0..3 {
        store.record_attempt(Some("daisy"), "1.1.1.1", false).await;
    }
    store.record_attempt(Some("daisy"), "1.1.1.1", true).await;
    let n = store
        .count_recent_failures_for_username("daisy", now - Duration::minutes(15))
        .await
        .unwrap();
    assert_eq!(n, 3, "successful attempts must not count toward lockout");
}

#[tokio::test]
async fn count_recent_failures_for_username_respects_window() {
    let (store, _dir) = fresh_store().await;
    store.record_attempt(Some("daisy"), "1.1.1.1", false).await;
    // Backdate the row beyond the window.
    sqlx::query("UPDATE auth_attempts SET ts = ? WHERE username = 'daisy'")
        .bind((Utc::now() - Duration::hours(2)).to_rfc3339())
        .execute(store.pool())
        .await
        .unwrap();
    let n = store
        .count_recent_failures_for_username("daisy", Utc::now() - Duration::minutes(15))
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn count_recent_failures_for_ip_aggregates_across_usernames() {
    let (store, _dir) = fresh_store().await;
    store.record_attempt(Some("daisy"), "9.9.9.9", false).await;
    store.record_attempt(Some("alex"), "9.9.9.9", false).await;
    store.record_attempt(None, "9.9.9.9", false).await;
    let n = store
        .count_recent_failures_for_ip("9.9.9.9", Utc::now() - Duration::minutes(15))
        .await
        .unwrap();
    assert_eq!(
        n, 3,
        "IP-bucketed lockout must count across all usernames AND null-username probes"
    );
}

// ----- storage_stats -----
//
// `/api/admin/storage` is the readout that powers the Retention settings
// header (DB size, issue/event counts, age of oldest event). It MUST
// return monotone-correct numbers from a real Store, because the UI
// renders them as the literal answer to "how big is this?".

#[tokio::test]
async fn storage_stats_empty_db_has_no_rows() {
    let (store, _dir) = fresh_store().await;
    let s: StorageStats = store.storage_stats().await.unwrap();
    // page_count * page_size for a freshly-migrated DB is non-zero —
    // SQLite always allocates the schema header + the `_sqlx_migrations`
    // table on open. Anything > 0 is fine for the assertion.
    assert!(s.bytes > 0, "fresh DB should report non-zero bytes");
    assert_eq!(s.issues, 0);
    assert_eq!(s.events, 0);
    assert_eq!(s.oldest_event_age_days, None);
}

#[tokio::test]
async fn storage_stats_counts_issues_and_events() {
    let (store, _dir) = fresh_store().await;
    // Two issues, three events on the first, two on the second → 5 events.
    let i1 = store
        .upsert_issue("p", &fp("a"), "T1", None, None, Utc::now())
        .await
        .unwrap();
    let i2 = store
        .upsert_issue("p", &fp("b"), "T2", None, None, Utc::now())
        .await
        .unwrap();
    for n in 0..3 {
        let mut ev = sample_event("Boom", "v", "f", n);
        ev.event_id = Uuid::new_v4();
        store.insert_event(i1.issue.id, &ev).await.unwrap();
    }
    for n in 0..2 {
        let mut ev = sample_event("Bang", "v", "g", n);
        ev.event_id = Uuid::new_v4();
        store.insert_event(i2.issue.id, &ev).await.unwrap();
    }
    let s = store.storage_stats().await.unwrap();
    assert_eq!(s.issues, 2);
    assert_eq!(s.events, 5);
    // Bytes growth from row inserts is real. The assertion is qualitative
    // because exact size depends on SQLite's page allocation strategy.
    assert!(s.bytes > 0);
}

#[tokio::test]
async fn storage_stats_oldest_event_age_reflects_received_at() {
    let (store, _dir) = fresh_store().await;
    let issue = store
        .upsert_issue("p", &fp("c"), "T", None, None, Utc::now())
        .await
        .unwrap();
    let mut ev = sample_event("Old", "v", "f", 1);
    ev.event_id = Uuid::new_v4();
    store.insert_event(issue.issue.id, &ev).await.unwrap();

    // Backdate the row by 7 days so we can assert on the readback.
    let backdated = (Utc::now() - Duration::days(7)).to_rfc3339();
    sqlx::query("UPDATE events SET received_at = ? WHERE issue_id = ?")
        .bind(&backdated)
        .bind(issue.issue.id)
        .execute(store.pool())
        .await
        .unwrap();

    let s = store.storage_stats().await.unwrap();
    let age = s
        .oldest_event_age_days
        .expect("oldest event age should be reported");
    // Allow off-by-one for clock drift between the test and the SQL query.
    assert!(
        (6..=8).contains(&age),
        "oldest event age should be ~7d, got {age}"
    );
}

#[tokio::test]
async fn prune_auth_attempts_deletes_only_old() {
    let (store, _dir) = fresh_store().await;
    store.record_attempt(Some("daisy"), "1.1.1.1", false).await;
    store.record_attempt(Some("daisy"), "1.1.1.1", false).await;
    sqlx::query("UPDATE auth_attempts SET ts = ? WHERE id = (SELECT MIN(id) FROM auth_attempts)")
        .bind((Utc::now() - Duration::days(2)).to_rfc3339())
        .execute(store.pool())
        .await
        .unwrap();
    let n = store
        .prune_auth_attempts_older_than(Utc::now() - Duration::days(1))
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn replayed_event_does_not_bump_count_or_reopen_issue() {
    let (store, _dir) = fresh_store().await;
    let now = Utc::now();
    let ev = sample_event("Boom", "v", "fa", 1);
    let (id, pl) = precomputed(&ev);
    let fp_a = fp("a");
    let batch = [BatchUpsertInput {
        project: "p",
        fp: &fp_a,
        title: "T",
        culprit: None,
        level: None,
        now,
        event_id: &id,
        payload: &pl,
        release: None,
    }];
    store.upsert_batch_with_events(&batch).await.unwrap();
    sqlx::query("UPDATE issues SET status = 'resolved'")
        .execute(store.pool())
        .await
        .unwrap();

    let replay = store.upsert_batch_with_events(&batch).await.unwrap();
    assert!(!replay[0].regressed);
    assert_eq!(replay[0].issue.status, errex_proto::IssueStatus::Resolved);
    assert_eq!(replay[0].issue.event_count, 1);

    let received: (String,) = sqlx::query_as("SELECT received_at FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(
        received.0.contains('T'),
        "received_at must be RFC3339: {}",
        received.0
    );
}

#[tokio::test]
async fn create_first_user_succeeds_only_once() {
    let (store, _dir) = fresh_store().await;
    let first = store
        .create_first_user("alice", "hash", Role::Admin)
        .await
        .unwrap();
    assert!(first.is_some());
    let second = store
        .create_first_user("mallory", "hash", Role::Admin)
        .await
        .unwrap();
    assert!(second.is_none());
    assert!(store.get_user("mallory").await.unwrap().is_none());
}

#[tokio::test]
async fn password_change_revokes_existing_sessions() {
    let (store, _dir) = fresh_store().await;
    store
        .create_user("bob", "old-hash", Role::Admin)
        .await
        .unwrap();
    store
        .create_session("sess-1", "bob", None, None)
        .await
        .unwrap();
    assert!(store.session_for_id("sess-1").await.unwrap().is_some());

    store.set_user_password("bob", "new-hash").await.unwrap();
    assert!(store.session_for_id("sess-1").await.unwrap().is_none());
}

// ----- releases -----
//
// One issue per defect across releases; a resolve records the release it
// is resolved in; alerts fire once per issue per release. These drive the
// digest's batch path, which is what ingest uses.

const B11: &str = "zeus-suite@0.0.14+11";
const B12: &str = "zeus-suite@0.0.15+12";
const B13: &str = "zeus-suite@0.0.16+13";
const B14: &str = "zeus-suite@0.0.17+14";

/// One event for fingerprint `f` from `release`, received at `t`.
async fn event_in(
    store: &Store,
    f: &str,
    release: Option<&str>,
    t: chrono::DateTime<Utc>,
) -> UpsertResult {
    let mut ev = sample_event("FlashError", "v", "flash", 1);
    ev.release = release.map(Into::into);
    let (event_id, payload) = precomputed(&ev);
    let fingerprint = fp(f);
    store
        .upsert_batch_with_events(&[BatchUpsertInput {
            project: "p",
            fp: &fingerprint,
            title: "FlashError: v",
            culprit: None,
            level: Some("error"),
            now: t,
            event_id: &event_id,
            payload: &payload,
            release,
        }])
        .await
        .unwrap()
        .remove(0)
}

fn quiet(r: &UpsertResult) -> bool {
    !r.created && !r.regressed && !r.new_release
}

#[tokio::test]
async fn repeat_events_in_one_release_are_one_issue_and_never_alert() {
    let (store, _dir) = fresh_store().await;
    let t = Utc::now();
    let first = event_in(&store, "a", Some(B12), t).await;
    assert!(first.created && !first.new_release);
    for i in 1..4 {
        let r = event_in(&store, "a", Some(B12), t + Duration::seconds(i)).await;
        assert!(quiet(&r), "repeat {i} in the same release alerted");
        assert_eq!(r.issue.id, first.issue.id);
    }
    let issue = &store.load_issues().await.unwrap()[0];
    assert_eq!(issue.event_count, 4);
    assert_eq!(issue.first_release.as_deref(), Some(B12));
    assert_eq!(issue.newest_release.as_deref(), Some(B12));
    let releases = store.issue_releases(issue.id).await.unwrap();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].release, B12);
    assert_eq!(releases[0].event_count, 4);
    assert_eq!(releases[0].first_seen, t);
    assert_eq!(releases[0].last_seen, t + Duration::seconds(3));
}

#[tokio::test]
async fn an_open_issue_alerts_once_when_a_newer_release_first_reports_it() {
    let (store, _dir) = fresh_store().await;
    let t = Utc::now();
    event_in(&store, "a", Some(B12), t).await;
    let r = event_in(&store, "a", Some(B13), t + Duration::seconds(1)).await;
    assert!(r.new_release && !r.created && !r.regressed);
    assert!(quiet(
        &event_in(&store, "a", Some(B13), t + Duration::seconds(2)).await
    ));
    assert!(
        quiet(&event_in(&store, "a", Some(B12), t + Duration::seconds(3)).await),
        "an older release already seen"
    );
    assert!(
        quiet(&event_in(&store, "a", Some(B11), t + Duration::seconds(4)).await),
        "an older release seen for the first time is not newer"
    );
    let r = event_in(&store, "a", None, t + Duration::seconds(5)).await;
    assert!(quiet(&r), "an event without a release");
    let issue = r.issue;
    assert_eq!(issue.first_release.as_deref(), Some(B12));
    assert_eq!(issue.last_release.as_deref(), Some(B11));
    assert_eq!(issue.newest_release.as_deref(), Some(B13));
    assert_eq!(issue.event_count, 6);
    let order: Vec<_> = store
        .issue_releases(issue.id)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.release, r.event_count))
        .collect();
    assert_eq!(
        order,
        vec![(B13.into(), 2), (B12.into(), 2), (B11.into(), 1)],
        "newest first"
    );
}

#[tokio::test]
async fn a_muted_issue_reports_its_new_release_for_the_alert_sinks_to_drop() {
    let (store, _dir) = fresh_store().await;
    let t = Utc::now();
    let r = event_in(&store, "a", Some(B12), t).await;
    store
        .set_status(r.issue.id, IssueStatus::Muted, None)
        .await
        .unwrap();
    let r = event_in(&store, "a", Some(B13), t + Duration::seconds(1)).await;
    assert!(r.new_release && !r.regressed);
    assert_eq!(r.issue.status, IssueStatus::Muted);
}

#[tokio::test]
async fn resolving_defaults_to_the_projects_newest_release() {
    let (store, _dir) = fresh_store().await;
    let t = Utc::now();
    let a = event_in(&store, "a", Some(B12), t).await;
    event_in(&store, "b", Some(B13), t + Duration::seconds(1)).await;
    let resolved = store
        .set_status(a.issue.id, IssueStatus::Resolved, None)
        .await
        .unwrap();
    assert_eq!(
        resolved.resolved_in_release.as_deref(),
        Some(B13),
        "newest in the project, not just in the issue"
    );
    let reopened = store
        .set_status(a.issue.id, IssueStatus::Unresolved, None)
        .await
        .unwrap();
    assert_eq!(reopened.resolved_in_release, None);
}

#[tokio::test]
async fn resolving_in_a_named_release_needs_one_the_project_reported() {
    let (store, _dir) = fresh_store().await;
    let a = event_in(&store, "a", Some(B12), Utc::now()).await;
    let named = store
        .set_status(a.issue.id, IssueStatus::Resolved, Some(B12))
        .await
        .unwrap();
    assert_eq!(named.resolved_in_release.as_deref(), Some(B12));
    let err = store
        .set_status(a.issue.id, IssueStatus::Resolved, Some(B14))
        .await
        .unwrap_err();
    assert!(matches!(err, error::DaemonError::Invalid(_)), "{err:?}");
    let issue = &store.load_issues().await.unwrap()[0];
    assert_eq!(issue.resolved_in_release.as_deref(), Some(B12), "unchanged");
}

#[tokio::test]
async fn resolved_in_a_release_then_an_older_release_only_counts() {
    let (store, _dir) = fresh_store().await;
    let t = Utc::now();
    let a = event_in(&store, "a", Some(B12), t).await;
    event_in(&store, "a", Some(B13), t + Duration::seconds(1)).await;
    store
        .set_status(a.issue.id, IssueStatus::Resolved, None)
        .await
        .unwrap();
    for (i, release) in [B12, B13, B11].into_iter().enumerate() {
        let r = event_in(
            &store,
            "a",
            Some(release),
            t + Duration::seconds(2 + i as i64),
        )
        .await;
        assert!(quiet(&r), "{release} is at or below the resolved release");
        assert_eq!(r.issue.status, IssueStatus::Resolved);
        assert_eq!(r.issue.resolved_in_release.as_deref(), Some(B13));
    }
    let issue = &store.load_issues().await.unwrap()[0];
    assert_eq!(issue.event_count, 5, "every event counts");
    assert_eq!(issue.last_release.as_deref(), Some(B11));
}

#[tokio::test]
async fn resolved_in_a_release_then_a_newer_release_regresses_once() {
    let (store, _dir) = fresh_store().await;
    let t = Utc::now();
    let a = event_in(&store, "a", Some(B12), t).await;
    store
        .set_status(a.issue.id, IssueStatus::Resolved, None)
        .await
        .unwrap();
    let r = event_in(&store, "a", Some(B13), t + Duration::seconds(1)).await;
    assert!(r.regressed && !r.new_release && !r.created);
    assert_eq!(r.issue.status, IssueStatus::Unresolved);
    assert_eq!(r.issue.resolved_in_release, None);
    assert_eq!(r.issue.newest_release.as_deref(), Some(B13));
    assert!(quiet(
        &event_in(&store, "a", Some(B13), t + Duration::seconds(2)).await
    ));
    assert!(quiet(
        &event_in(&store, "a", Some(B12), t + Duration::seconds(3)).await
    ));
    let r = event_in(&store, "a", Some(B14), t + Duration::seconds(4)).await;
    assert!(r.new_release, "open again, so a newer release alerts once");
}

#[tokio::test]
async fn resolved_in_an_older_release_than_the_issue_has_seen() {
    let (store, _dir) = fresh_store().await;
    let t = Utc::now();
    let a = event_in(&store, "a", Some(B12), t).await;
    event_in(&store, "a", Some(B13), t + Duration::seconds(1)).await;
    store
        .set_status(a.issue.id, IssueStatus::Resolved, Some(B12))
        .await
        .unwrap();
    let r = event_in(&store, "a", Some(B13), t + Duration::seconds(2)).await;
    assert!(
        r.regressed,
        "13 is newer than the release it was resolved in"
    );
}

#[tokio::test]
async fn events_without_a_release_regress_a_resolved_issue_as_before() {
    let (store, _dir) = fresh_store().await;
    let t = Utc::now();
    let a = event_in(&store, "a", None, t).await;
    let resolved = store
        .set_status(a.issue.id, IssueStatus::Resolved, None)
        .await
        .unwrap();
    assert_eq!(resolved.resolved_in_release, None, "no release to name");
    let r = event_in(&store, "a", Some(B12), t + Duration::seconds(1)).await;
    assert!(r.regressed, "resolved in no release: any event regresses");

    let b = event_in(&store, "b", Some(B12), t).await;
    store
        .set_status(b.issue.id, IssueStatus::Resolved, None)
        .await
        .unwrap();
    let r = event_in(&store, "b", None, t + Duration::seconds(1)).await;
    assert!(
        r.regressed,
        "an event without a release cannot be shown to be old"
    );
}

#[tokio::test]
async fn releases_order_by_build_number_before_version() {
    let (store, _dir) = fresh_store().await;
    let t = Utc::now();
    let newer_build = "zeus-suite@0.0.9+10";
    let older_build = "zeus-suite@0.0.10+9";
    let a = event_in(&store, "a", Some(newer_build), t).await;
    let r = event_in(&store, "a", Some(older_build), t + Duration::seconds(1)).await;
    assert!(quiet(&r), "build 9 is older than build 10");
    assert_eq!(r.issue.newest_release.as_deref(), Some(newer_build));
    store
        .set_status(a.issue.id, IssueStatus::Resolved, None)
        .await
        .unwrap();
    let resolved = &store.load_issues().await.unwrap()[0];
    assert_eq!(resolved.resolved_in_release.as_deref(), Some(newer_build));
    let r = event_in(&store, "a", Some(older_build), t + Duration::seconds(2)).await;
    assert!(quiet(&r) && r.issue.status == IssueStatus::Resolved);
    let r = event_in(
        &store,
        "a",
        Some("zeus-suite@0.0.9+11"),
        t + Duration::seconds(3),
    )
    .await;
    assert!(r.regressed);
    let listed: Vec<_> = store
        .project_releases("p")
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.release)
        .collect();
    assert_eq!(
        listed,
        vec!["zeus-suite@0.0.9+11", newer_build, older_build]
    );
}

#[tokio::test]
async fn releases_without_a_version_order_by_first_sighting() {
    let (store, _dir) = fresh_store().await;
    let t = Utc::now();
    let a = event_in(&store, "a", Some("5114f85"), t).await;
    let r = event_in(&store, "a", Some("9ac0e21"), t + Duration::seconds(10)).await;
    assert!(r.new_release, "first reported later, so newer");
    store
        .set_status(a.issue.id, IssueStatus::Resolved, None)
        .await
        .unwrap();
    let r = event_in(&store, "a", Some("5114f85"), t + Duration::seconds(20)).await;
    assert!(quiet(&r));
    let r = event_in(&store, "a", Some("0aa0aa0"), t + Duration::seconds(30)).await;
    assert!(r.regressed, "a release first seen after the resolved one");
    assert_eq!(
        store::release_order("x", Some("2026-01-01T00:00:00Z"), "y", None),
        std::cmp::Ordering::Greater,
        "a release with no record sorts below a recorded one"
    );
}

#[tokio::test]
async fn a_replayed_event_counts_against_no_release() {
    let (store, _dir) = fresh_store().await;
    let mut ev = sample_event("E", "v", "f", 1);
    ev.release = Some(B12.into());
    let (event_id, payload) = precomputed(&ev);
    let f = fp("a");
    let input = BatchUpsertInput {
        project: "p",
        fp: &f,
        title: "T",
        culprit: None,
        level: None,
        now: Utc::now(),
        event_id: &event_id,
        payload: &payload,
        release: Some(B12),
    };
    store.upsert_batch_with_events(&[input]).await.unwrap();
    let input = BatchUpsertInput {
        project: "p",
        fp: &f,
        title: "T",
        culprit: None,
        level: None,
        now: Utc::now(),
        event_id: &event_id,
        payload: &payload,
        release: Some(B13),
    };
    let r = store
        .upsert_batch_with_events(&[input])
        .await
        .unwrap()
        .remove(0);
    assert!(quiet(&r));
    let releases = store.issue_releases(r.issue.id).await.unwrap();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].event_count, 1);
}

#[tokio::test]
async fn project_rename_and_delete_carry_its_releases() {
    let (store, _dir) = fresh_store().await;
    store.create_project("p").await.unwrap();
    event_in(&store, "a", Some(B12), Utc::now()).await;
    store.rename_project("p", "q").await.unwrap();
    assert!(store.project_releases("p").await.unwrap().is_empty());
    assert_eq!(store.project_releases("q").await.unwrap().len(), 1);
    store.delete_project("q").await.unwrap();
    assert!(store.project_releases("q").await.unwrap().is_empty());
    let leftovers: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issue_releases")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(leftovers, 0, "issue rows cascade to their release rows");
}

#[tokio::test]
async fn issue_releases_of_an_unknown_issue_is_not_found() {
    let (store, _dir) = fresh_store().await;
    assert!(matches!(
        store.issue_releases(42).await,
        Err(error::DaemonError::NotFound(_))
    ));
}

/// The releases migration upgrades a database the previous daemon wrote:
/// every issue and event stays, and release rows are back-filled from the
/// stored events.
#[tokio::test]
async fn releases_migration_backfills_a_database_the_previous_daemon_wrote() {
    const RELEASES_MIGRATION: &str = "20261020000001";
    let dir = unique_tempdir();
    let earlier = dir.join("earlier-migrations");
    std::fs::create_dir_all(&earlier).unwrap();
    let migrations = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&migrations).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if name.as_str() < RELEASES_MIGRATION {
            std::fs::copy(entry.path(), earlier.join(&name)).unwrap();
        }
    }
    let store = Store::open(&dir.join("errex.db")).await.unwrap();
    sqlx::migrate::Migrator::new(earlier.as_path())
        .await
        .unwrap()
        .run(store.pool())
        .await
        .unwrap();

    // What the previous daemon stored: issue 1 resolved, seen in builds 12
    // and 13 (13 received first) plus an event with no release and one
    // whose payload is not JSON; issue 2 open with versionless releases;
    // issue 3 with every event purged by retention.
    let at = |m: u32| format!("2026-10-01T10:{m:02}:00.000+00:00");
    for (id, fp, status, count) in [
        (1, "fa", "resolved", 6),
        (2, "fb", "unresolved", 2),
        (3, "fc", "resolved", 9),
    ] {
        sqlx::query(
            "INSERT INTO issues (id, project, fingerprint, title, status, event_count, \
             first_seen, last_seen) VALUES (?, 'p', ?, 'T', ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(fp)
        .bind(status)
        .bind(count)
        .bind(at(0))
        .bind(at(59))
        .execute(store.pool())
        .await
        .unwrap();
    }
    let events: [(i64, &str, String, String); 7] = [
        (1, "e1", format!(r#"{{"release":"{B13}"}}"#), at(1)),
        (1, "e2", format!(r#"{{"release":"{B12}"}}"#), at(2)),
        (1, "e3", format!(r#"{{"release":"{B12}"}}"#), at(3)),
        (1, "e4", r#"{"message":"no release"}"#.into(), at(4)),
        (1, "e5", "not json".into(), at(5)),
        (2, "e6", r#"{"release":"5114f85"}"#.into(), at(6)),
        (2, "e7", r#"{"release":"9ac0e21"}"#.into(), at(7)),
    ];
    for (issue_id, event_id, payload, received_at) in &events {
        sqlx::query(
            "INSERT INTO events (issue_id, event_id, payload, received_at) VALUES (?, ?, ?, ?)",
        )
        .bind(issue_id)
        .bind(event_id)
        .bind(payload)
        .bind(received_at)
        .execute(store.pool())
        .await
        .unwrap();
    }

    store.migrate().await.unwrap();

    let issues = store.list_issues_by_project("p").await.unwrap();
    assert_eq!(issues.len(), 3, "every issue kept");
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM events")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(stored, 7, "every event kept");
    let by_id = |id: i64| issues.iter().find(|i| i.id == id).unwrap().clone();

    let one = by_id(1);
    assert_eq!(one.status, IssueStatus::Resolved);
    assert_eq!(one.event_count, 6, "issue counters untouched");
    assert_eq!(one.first_release.as_deref(), Some(B13), "first received");
    assert_eq!(one.last_release.as_deref(), Some(B12), "last received");
    assert_eq!(one.newest_release.as_deref(), Some(B13), "highest build");
    assert_eq!(
        one.resolved_in_release.as_deref(),
        Some(B13),
        "an already-resolved issue is resolved in its newest release"
    );
    let releases = store.issue_releases(1).await.unwrap();
    let counts: Vec<_> = releases
        .iter()
        .map(|r| (r.release.as_str(), r.event_count))
        .collect();
    assert_eq!(counts, vec![(B13, 1), (B12, 2)]);
    assert_eq!(
        releases[1].first_seen.to_rfc3339(),
        "2026-10-01T10:02:00+00:00"
    );
    assert_eq!(
        releases[1].last_seen.to_rfc3339(),
        "2026-10-01T10:03:00+00:00"
    );

    let two = by_id(2);
    assert_eq!(
        two.newest_release.as_deref(),
        Some("9ac0e21"),
        "by first sighting"
    );
    assert_eq!(
        two.resolved_in_release, None,
        "open issues are not resolved in anything"
    );

    let three = by_id(3);
    assert_eq!(three.first_release, None);
    assert_eq!(three.newest_release, None);
    assert_eq!(three.resolved_in_release, None);
    assert_eq!(three.status, IssueStatus::Resolved);

    // Running it again (every later start) changes nothing.
    store.migrate().await.unwrap();
    assert_eq!(store.list_issues_by_project("p").await.unwrap().len(), 3);
    assert_eq!(store.issue_releases(1).await.unwrap().len(), 2);

    // And the upgraded issues follow the release rules.
    let t = Utc::now();
    let old = event_in(&store, "fa", Some(B12), t).await;
    assert!(quiet(&old) && old.issue.status == IssueStatus::Resolved);
    let newer = event_in(&store, "fa", Some(B14), t + Duration::seconds(1)).await;
    assert!(newer.regressed);
    let purged = event_in(&store, "fc", Some(B12), t).await;
    assert!(
        purged.regressed,
        "resolved in no release: any event regresses"
    );
    let open = event_in(&store, "fb", Some("0aa0aa0"), t).await;
    assert!(
        open.new_release,
        "a release first reported after the back-filled ones"
    );
}
