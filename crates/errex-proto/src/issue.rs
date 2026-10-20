use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fingerprint::Fingerprint;

/// Triage state of an issue, shared across all clients.
///
/// - `Unresolved`: open, default in lists, alerts on new events.
/// - `Resolved`: closed, optionally in a release; an event from a newer
///   release regresses it back to `Unresolved`, one from that release or
///   older only counts.
/// - `Muted`: hidden from default view; new events do NOT regress it.
/// - `Ignored`: permanently dismissed; events still increment count but
///   never trigger alerts and the issue stays out of default views.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueStatus {
    #[default]
    Unresolved,
    Resolved,
    Muted,
    Ignored,
}

impl IssueStatus {
    /// String form used by the SQLite column. Stable across versions; the
    /// daemon's migration only adds the column with `'unresolved'` default.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            IssueStatus::Unresolved => "unresolved",
            IssueStatus::Resolved => "resolved",
            IssueStatus::Muted => "muted",
            IssueStatus::Ignored => "ignored",
        }
    }

    /// Inverse of `as_db_str`; unknown values are coerced to `Unresolved`
    /// rather than panicking, so a future schema migration that introduces
    /// new statuses can be downgraded without crashing the read path.
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "resolved" => IssueStatus::Resolved,
            "muted" => IssueStatus::Muted,
            "ignored" => IssueStatus::Ignored,
            _ => IssueStatus::Unresolved,
        }
    }
}

/// A grouped collection of events sharing a fingerprint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: i64,
    pub project: String,
    pub fingerprint: Fingerprint,
    pub title: String,
    pub culprit: Option<String>,
    pub level: Option<String>,
    /// Triage state. Defaults to `Unresolved` so payloads from older daemons
    /// (or hand-crafted JSON) parse without error.
    #[serde(default)]
    pub status: IssueStatus,
    pub event_count: i64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Release of the issue's first event that carried one.
    #[serde(default)]
    pub first_release: Option<String>,
    /// Release of the most recent event that carried one.
    #[serde(default)]
    pub last_release: Option<String>,
    /// Newest release, in [`crate::release`] order, that has reported the
    /// issue.
    #[serde(default)]
    pub newest_release: Option<String>,
    /// While resolved: the release it was resolved in. Events from this
    /// release or an older one count without regressing the issue.
    #[serde(default)]
    pub resolved_in_release: Option<String>,
}
