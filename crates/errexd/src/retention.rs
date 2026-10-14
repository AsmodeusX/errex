//! Background retention task: bounds payload age, events-per-issue, and
//! issues-per-project. Runs on a 1-hour cadence — frequent enough that a
//! UI-driven setting change takes effect within an hour, infrequent enough
//! that we don't churn the DB during normal operation.
//!
//! Settings are read from the `retention_settings` row each tick (cheap)
//! so an operator can tighten retention via the SPA without redeploying
//! the daemon. Each bound has a deploy-time default (`ERREX_RETENTION_DAYS`,
//! `ERREX_EVENTS_PER_ISSUE_MAX`, `ERREX_ISSUES_PER_PROJECT_MAX`) that applies
//! while the DB-level value is 0 (the migration default).

use std::time::Duration;

use chrono::Utc;
use tokio::time::{interval, MissedTickBehavior};

use crate::store::Store;

/// Deploy-time defaults for each bound; `0` = unlimited.
#[derive(Debug, Clone, Copy)]
pub struct Defaults {
    pub days: u32,
    pub events_per_issue_max: u32,
    pub issues_per_project_max: u32,
}

/// The DB-level value when the operator set one, else the deploy default.
fn effective(db: i64, boot: u32) -> i64 {
    if db > 0 {
        db
    } else {
        i64::from(boot)
    }
}

/// Spawn the retention task. With every bound 0 at both levels the task
/// still runs (so a UI-set bound takes effect), but each individual purge
/// becomes a no-op until something is configured.
pub async fn run(store: Store, defaults: Defaults) {
    let mut tick = interval(Duration::from_secs(60 * 60));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tracing::info!(?defaults, "retention task started");

    loop {
        tick.tick().await;
        run_one_tick(&store, defaults).await;
    }
}

/// One purge sweep. Extracted so tests can drive it deterministically
/// without waiting on the hourly interval.
pub async fn run_one_tick(store: &Store, defaults: Defaults) {
    // Pull the live settings each tick so a UI change takes effect
    // without restart. Failure here is fatal-to-the-tick only — the next
    // hour gets a fresh chance.
    let settings = match store.get_retention_settings().await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "retention: failed to read settings; skipping tick");
            return;
        }
    };

    // DB-level wins when set, else the deploy default. Both 0 ⇒ that purge
    // is disabled.
    let effective_days = effective(settings.event_retention_days, defaults.days);
    let events_per_issue_max =
        effective(settings.events_per_issue_max, defaults.events_per_issue_max);
    let issues_per_project_max = effective(
        settings.issues_per_project_max,
        defaults.issues_per_project_max,
    );
    if effective_days > 0 {
        let event_cutoff = Utc::now() - chrono::Duration::days(effective_days);
        match store.purge_events_older_than(event_cutoff).await {
            Ok(0) => tracing::debug!("retention: nothing aged out"),
            Ok(n) => {
                tracing::info!(deleted = n, cutoff = %event_cutoff, "retention: purged aged events")
            }
            Err(err) => tracing::warn!(%err, "retention: age purge failed"),
        }
    }

    if events_per_issue_max > 0 {
        match store
            .purge_excess_events_per_issue(events_per_issue_max)
            .await
        {
            Ok(0) => tracing::debug!("retention: events_per_issue cap met"),
            Ok(n) => tracing::info!(
                deleted = n,
                max = events_per_issue_max,
                "retention: trimmed events per issue"
            ),
            Err(err) => tracing::warn!(%err, "retention: events_per_issue purge failed"),
        }
    }

    if issues_per_project_max > 0 {
        match store
            .purge_excess_issues_per_project(issues_per_project_max)
            .await
        {
            Ok(0) => tracing::debug!("retention: issues_per_project cap met"),
            Ok(n) => tracing::info!(
                deleted = n,
                max = issues_per_project_max,
                "retention: trimmed issues per project"
            ),
            Err(err) => tracing::warn!(%err, "retention: issues_per_project purge failed"),
        }
    }

    // Auth attempts are bounded by the lockout window (15 min) — keeping
    // anything older than 24h is purely for forensic context and quickly
    // becomes noise. The same hourly tick handles it; we don't need a
    // dedicated task for two extra DELETEs.
    let attempts_cutoff = Utc::now() - chrono::Duration::hours(24);
    match store.prune_auth_attempts_older_than(attempts_cutoff).await {
        Ok(0) => {}
        Ok(n) => tracing::debug!(deleted = n, "retention: pruned auth attempts"),
        Err(err) => tracing::warn!(%err, "retention: auth-attempts purge failed"),
    }

    // Sessions: drop any whose `last_seen_at` is past the sliding-cookie
    // expiry. The browser would refuse the cookie at this point anyway,
    // but evicting the row keeps the table from accumulating ghost
    // sessions for users who just closed their tab and never came back.
    let session_cutoff = Utc::now() - chrono::Duration::days(crate::auth::SESSION_IDLE_DAYS);
    match store.purge_sessions_idle_since(session_cutoff).await {
        Ok(0) => {}
        Ok(n) => tracing::debug!(deleted = n, "retention: purged idle sessions"),
        Err(err) => tracing::warn!(%err, "retention: session purge failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::RetentionSettings;
    use errex_proto::{Event, Fingerprint};

    #[test]
    fn db_value_wins_when_set_else_the_deploy_default() {
        assert_eq!(effective(0, 200), 200);
        assert_eq!(effective(50, 200), 50);
        assert_eq!(effective(0, 0), 0);
    }

    async fn store_with_events(n: usize) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "errexd-retention-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("errex.db")).await.unwrap();
        store.migrate().await.unwrap();
        let r = store
            .upsert_issue("p", &Fingerprint::new("a"), "T", None, None, Utc::now())
            .await
            .unwrap();
        for _ in 0..n {
            let ev: Event = serde_json::from_str(r#"{"message":"m"}"#).unwrap();
            store.insert_event(r.issue.id, &ev).await.unwrap();
        }
        (store, dir)
    }

    const DEFAULTS: Defaults = Defaults {
        days: 30,
        events_per_issue_max: 3,
        issues_per_project_max: 100,
    };

    #[tokio::test]
    async fn deploy_default_caps_events_per_issue() {
        let (store, dir) = store_with_events(5).await;
        run_one_tick(&store, DEFAULTS).await;
        assert_eq!(store.storage_stats().await.unwrap().events, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_admin_setting_overrides_the_deploy_default() {
        let (store, dir) = store_with_events(5).await;
        store
            .set_retention_settings(RetentionSettings {
                events_per_issue_max: 1,
                issues_per_project_max: 0,
                event_retention_days: 0,
            })
            .await
            .unwrap();
        run_one_tick(&store, DEFAULTS).await;
        assert_eq!(store.storage_stats().await.unwrap().events, 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
