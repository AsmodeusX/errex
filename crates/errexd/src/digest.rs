//! Single-writer digest task.
//!
//! Owns the only "write side" of SQLite. HTTP handlers shovel `IngestEvent`s
//! into a channel; this task drains them, fingerprints, persists, and
//! broadcasts the resulting `IssueCreated`/`IssueUpdated` to subscribers.
//!
//! There is intentionally no in-memory cache — SQLite WAL reads are
//! sub-millisecond and an in-RAM mirror would just double the footprint.
//! The WS server queries the store directly when a client connects.

use chrono::{DateTime, Utc};
use errex_proto::{Event, Fingerprint, Level, ServerMessage};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::error::DaemonError;
use crate::fingerprint;
use crate::store::{BatchUpsertInput, Store, UpsertResult};
use crate::webhook::{self, AlertSinks, TriggerKind};

/// Max events per digest batch. The single-COMMIT-per-batch is what makes
/// batching worth it under WAL+synchronous=NORMAL: one fsync amortized
/// over up to N events.
///
/// Stress-tested at 32 — increasing it bumps the saturation throughput
/// but enlarges the worst-case tail when a checkpoint stalls a big tx.
const BATCH_SIZE: usize = 16;
const BATCH_RETRIES: u32 = 3;

/// What the ingest layer hands to the digest task.
///
/// Everything that requires touching the raw `Event` (fingerprint,
/// title, level, culprit, JSON serialization, event_id stringification)
/// is precomputed in the HTTP handler via [`prepare`]. The digest task
/// then only does SQLite I/O. Per-event JSON serialization showed up on
/// the digest's hot path under saturation; moving it off the writer
/// lifts the throughput plateau.
#[derive(Debug, Clone)]
pub struct IngestEvent {
    pub project: String,
    pub fingerprint: Fingerprint,
    pub title: String,
    pub level: Option<String>,
    pub culprit: Option<String>,
    pub exception_type: String,
    pub first_frame: String,
    pub received_at: DateTime<Utc>,
    /// SDK event_id pre-stringified so the writer doesn't pay
    /// `Uuid::to_string` per insert.
    pub event_id: String,
    /// Pre-serialized event JSON for the `events.payload` column.
    pub payload: String,
    /// The event's release, for alert text. Capped like the title.
    pub release: Option<String>,
    /// The event matched the paging rule ([`pages`]).
    pub page: bool,
    /// This event's share of the ingest memory budget. Released when the
    /// digest task drops the record after persisting it.
    pub _budget: Option<std::sync::Arc<tokio::sync::OwnedSemaphorePermit>>,
}

/// Build a digest-ready record from a raw `Event` parsed off the wire.
/// Issue titles and culprits are SDK-controlled and get listed in bulk, so
/// they are capped to keep snapshot responses bounded.
const MAX_LABEL_CHARS: usize = 512;

fn truncate_chars(mut s: String, max: usize) -> String {
    if let Some((idx, _)) = s.char_indices().nth(max) {
        s.truncate(idx);
    }
    s
}

/// The paging rule: a fatal event tagged `flash.erased=true` is an ECU left
/// with erased flash and a silent DME, and pages on every occurrence.
const PAGE_TAG: (&str, &str) = ("flash.erased", "true");

pub fn pages(event: &Event) -> bool {
    event.level == Some(Level::Fatal)
        && match tag_value(event.tags.as_ref(), PAGE_TAG.0) {
            Some(Value::String(s)) => s == PAGE_TAG.1,
            Some(Value::Bool(b)) => b.to_string() == PAGE_TAG.1,
            _ => false,
        }
}

/// Inbound drop rules (`ERREX_DROP_EVENTS`): events an operator never wants
/// stored, such as what app builds already in the field send for a user's
/// own situation. Rules are separated by `;`; each is an exception type,
/// optionally with conditions on tags that must all hold:
///
/// ```text
/// licensing_refused:api.status=401|403|404 ; flash_ok ;
/// flash_failed:flash.erased=false,flash.kind=adapter|timeout
/// ```
///
/// A tag holds when its value, as a string, is one of the `|`-separated
/// values; a missing tag never holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DropRules(Vec<DropRule>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct DropRule {
    exception_type: String,
    tags: Vec<(String, Vec<String>)>,
}

impl DropRules {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut rules = Vec::new();
        for rule in spec.split(';').map(str::trim).filter(|r| !r.is_empty()) {
            let (ty, conditions) = match rule.split_once(':') {
                Some((ty, rest)) => (ty.trim(), Some(rest)),
                None => (rule, None),
            };
            if ty.is_empty() || ty.contains(char::is_whitespace) {
                return Err(format!("drop rule `{rule}`: no exception type"));
            }
            let mut tags = Vec::new();
            for condition in conditions.into_iter().flat_map(|c| c.split(',')) {
                let Some((key, values)) = condition.split_once('=') else {
                    return Err(format!(
                        "drop rule `{rule}`: `{condition}` is not tag=value"
                    ));
                };
                let values: Vec<String> = values
                    .split('|')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
                    .collect();
                let key = key.trim();
                if key.is_empty() || values.is_empty() {
                    return Err(format!(
                        "drop rule `{rule}`: `{condition}` is not tag=value"
                    ));
                }
                tags.push((key.to_string(), values));
            }
            rules.push(DropRule {
                exception_type: ty.to_string(),
                tags,
            });
        }
        Ok(Self(rules))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `event` matches a rule and is dropped at ingest.
    pub fn drops(&self, event: &Event) -> bool {
        if self.0.is_empty() {
            return false;
        }
        let Some(ty) = event.primary_exception().and_then(|e| e.ty.as_deref()) else {
            return false;
        };
        self.0.iter().any(|rule| {
            rule.exception_type == ty
                && rule.tags.iter().all(|(key, values)| {
                    let value = match tag_value(event.tags.as_ref(), key) {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Number(n)) => n.to_string(),
                        Some(Value::Bool(b)) => b.to_string(),
                        _ => return false,
                    };
                    values.contains(&value)
                })
        })
    }
}

/// Tags arrive as an object or as `[key, value]` pairs.
fn tag_value<'a>(tags: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    match tags? {
        Value::Object(map) => map.get(key),
        Value::Array(pairs) => pairs
            .iter()
            .find(|p| p.get(0).and_then(Value::as_str) == Some(key))
            .and_then(|p| p.get(1)),
        _ => None,
    }
}

/// Cheap (small string allocs + one fingerprint hash + one JSON
/// serialize); intentionally runs in the HTTP handler instead of the
/// digest task so the digest's only blocking work is SQLite I/O.
///
/// Scrubbing comes first, so nothing it removes reaches a title, a log
/// line, an alert or the database. The SDK's own fingerprint list is the
/// exception: it is hashed as sent (per-build offsets must keep splitting
/// groups) and scrubbed before it is stored.
pub fn prepare(project: String, mut event: Event) -> IngestEvent {
    crate::scrub::scrub_event(&mut event);
    let fingerprint = fingerprint::derive(&event);
    if let Some(list) = event.fingerprint.as_mut() {
        crate::scrub::scrub_strings(list);
    }
    let page = pages(&event);
    let release = event
        .release
        .clone()
        .filter(|r| !r.is_empty())
        .map(|r| truncate_chars(r, MAX_LABEL_CHARS));
    let title = truncate_chars(event.title(), MAX_LABEL_CHARS);
    let level = event.level.map(level_str);

    let exception_type = event
        .primary_exception()
        .and_then(|e| e.ty.clone())
        .unwrap_or_else(|| "<no exception>".to_string());
    let first_frame = event
        .primary_exception()
        .and_then(|e| e.first_frame())
        .map(|f| {
            format!(
                "{}:{}",
                f.function.as_deref().unwrap_or("?"),
                f.lineno.unwrap_or(0)
            )
        })
        .unwrap_or_else(|| "<no frames>".to_string());
    let culprit = event
        .primary_exception()
        .and_then(|e| e.first_frame())
        .map(|f| {
            let func = f.function.as_deref().unwrap_or("?");
            let file = f.filename.as_deref().unwrap_or("?");
            truncate_chars(format!("{func} in {file}"), MAX_LABEL_CHARS)
        });

    let event_id = event.event_id.simple().to_string();
    // `serde_json::to_string` is infallible here — `Event` is a struct of
    // strings/numbers/nested options, no map keys whose Display can
    // fail. The `expect` documents the invariant rather than papering
    // over a real error.
    let payload =
        serde_json::to_string(&event).expect("Event always serializes (no non-string map keys)");

    IngestEvent {
        project,
        fingerprint,
        title,
        level,
        culprit,
        exception_type,
        first_frame,
        received_at: Utc::now(),
        event_id,
        payload,
        release,
        page,
        _budget: None,
    }
}

pub async fn run(
    store: Store,
    mut events: mpsc::Receiver<IngestEvent>,
    fanout: broadcast::Sender<ServerMessage>,
    alerts: AlertSinks,
) -> Result<(), DaemonError> {
    tracing::info!("digest task started");

    let mut buf: Vec<IngestEvent> = Vec::with_capacity(BATCH_SIZE);
    loop {
        // Block on the first event in this batch.
        let first = match events.recv().await {
            Some(e) => e,
            None => break,
        };
        buf.push(first);

        // Opportunistically drain whatever else is already queued. We
        // intentionally do NOT wait — under low load the channel is
        // empty after the first recv and we dispatch a batch of one
        // immediately. Under sustained load the channel is rarely
        // empty after the previous batch's commit, so each batch fills
        // close to BATCH_SIZE without us synthesizing latency.
        while buf.len() < BATCH_SIZE {
            match events.try_recv() {
                Ok(e) => buf.push(e),
                Err(_) => break,
            }
        }

        process_batch(&store, &fanout, &alerts, &mut buf).await;
    }

    tracing::info!("digest task exiting (ingest channel closed)");
    Ok(())
}

/// Persist a batch of `IngestEvent`s under one transaction, then broadcast
/// one ServerMessage per result. Empties `buf` when done. Per-batch DB
/// errors are logged and the batch is dropped — the next batch starts
/// fresh, the digest task does not exit on transient SQLite errors.
async fn process_batch(
    store: &Store,
    fanout: &broadcast::Sender<ServerMessage>,
    alerts: &AlertSinks,
    buf: &mut Vec<IngestEvent>,
) {
    if buf.is_empty() {
        return;
    }
    let inputs: Vec<BatchUpsertInput<'_>> = buf
        .iter()
        .map(|rec| BatchUpsertInput {
            project: &rec.project,
            fp: &rec.fingerprint,
            title: &rec.title,
            culprit: rec.culprit.as_deref(),
            level: rec.level.as_deref(),
            now: rec.received_at,
            event_id: &rec.event_id,
            payload: &rec.payload,
            release: rec.release.as_deref(),
        })
        .collect();

    let mut attempt = 0;
    let results: Vec<UpsertResult> = loop {
        match store.upsert_batch_with_events(&inputs).await {
            Ok(r) => break r,
            Err(err) if attempt < BATCH_RETRIES => {
                attempt += 1;
                tracing::warn!(%err, attempt, "digest: upsert_batch failed, retrying");
                tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64)).await;
            }
            Err(err) => {
                // Each attempt rolled back, so nothing is half-written. Give
                // up loudly rather than crash the digest task and strand
                // every subsequent event on the channel.
                tracing::error!(%err, batch_size = buf.len(), "digest: upsert_batch failed");
                buf.clear();
                return;
            }
        }
    };

    for (rec, result) in buf.iter().zip(results) {
        if result.regressed {
            tracing::warn!(
                project = %rec.project,
                issue_id = result.issue.id,
                fingerprint = %rec.fingerprint,
                release = rec.release.as_deref().unwrap_or("-"),
                "issue regressed: {}",
                rec.title,
            );
        } else if result.new_release {
            tracing::info!(
                project = %rec.project,
                issue_id = result.issue.id,
                fingerprint = %rec.fingerprint,
                release = rec.release.as_deref().unwrap_or("-"),
                "issue seen in a newer release: {}",
                rec.title,
            );
        } else {
            tracing::info!(
                project = %rec.project,
                issue_id = result.issue.id,
                fingerprint = %rec.fingerprint,
                exception = %rec.exception_type,
                frame = %rec.first_frame,
                "ingest: {}",
                rec.title,
            );
        }

        let kind = if rec.page {
            Some(TriggerKind::Page)
        } else if result.created {
            Some(TriggerKind::NewIssue)
        } else if result.regressed {
            Some(TriggerKind::Regression)
        } else if result.new_release {
            Some(TriggerKind::NewRelease)
        } else {
            None
        };
        if let Some(kind) = kind {
            alerts.fire(webhook::Trigger {
                issue: result.issue.clone(),
                kind,
                release: rec.release.clone(),
            });
        }

        let msg = if result.created {
            ServerMessage::IssueCreated {
                issue: result.issue,
            }
        } else {
            ServerMessage::IssueUpdated {
                issue: result.issue,
            }
        };
        let _ = fanout.send(msg);
    }
    buf.clear();
}

fn level_str(l: Level) -> String {
    match l {
        Level::Debug => "debug",
        Level::Info => "info",
        Level::Warning => "warning",
        Level::Error => "error",
        Level::Fatal => "fatal",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_with_exception(ty: &str, func: &str, lineno: u32) -> Event {
        let raw = format!(
            r#"{{"timestamp":"2026-01-01T00:00:00Z","level":"error","exception":{{"values":[{{"type":"{ty}","value":"v","stacktrace":{{"frames":[{{"function":"{func}","filename":"src/x.rs","lineno":{lineno},"in_app":true}}]}}}}]}}}}"#
        );
        serde_json::from_str(&raw).expect("valid event JSON")
    }

    #[test]
    fn prepare_fills_derived_fields() {
        let ev = event_with_exception("Boom", "f", 42);
        let rec = prepare("p1".into(), ev);
        assert_eq!(rec.project, "p1");
        assert_eq!(rec.exception_type, "Boom");
        assert_eq!(rec.first_frame, "f:42");
        assert!(rec.title.contains("Boom"));
        assert_eq!(rec.level.as_deref(), Some("error"));
        // Same input → same fingerprint (algorithm is deterministic).
        let ev2 = event_with_exception("Boom", "f", 42);
        let rec2 = prepare("p1".into(), ev2);
        assert_eq!(rec.fingerprint, rec2.fingerprint);
    }

    fn ev(v: serde_json::Value) -> Event {
        serde_json::from_value(v).expect("valid event JSON")
    }

    #[test]
    fn prepare_scrubs_before_title_and_payload() {
        let rec = prepare(
            "p".into(),
            ev(serde_json::json!({
                "exception": { "values": [{ "type": "FlashError", "value": "car WBS8M9C50J5K12345" }] },
                "user": { "ip_address": "203.0.113.9" },
            })),
        );
        assert_eq!(rec.title, "FlashError: car <vin>");
        assert!(!rec.payload.contains("WBS8M9C50J5K12345"));
        assert!(!rec.payload.contains("203.0.113.9"));
    }

    #[test]
    fn prepare_groups_messages_that_differ_only_by_address() {
        let a = prepare(
            "p".into(),
            ev(serde_json::json!({ "message": "read at 0x80040000" })),
        );
        let b = prepare(
            "p".into(),
            ev(serde_json::json!({ "message": "read at 0x80041000" })),
        );
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn prepare_hashes_the_sdk_fingerprint_as_sent_and_stores_it_scrubbed() {
        let raw = |off: &str| {
            ev(serde_json::json!({
                "message": "m",
                "fingerprint": ["unhandled", "E1", off],
            }))
        };
        let a = prepare("p".into(), raw("0x002f5c1f"));
        let b = prepare("p".into(), raw("0x002f6000"));
        assert_ne!(
            a.fingerprint, b.fingerprint,
            "per-build offsets still split groups"
        );
        assert!(a
            .payload
            .contains(r#""fingerprint":["unhandled","E1","<hex>"]"#));
    }

    #[test]
    fn prepare_pages_on_fatal_erased_flash() {
        let page = |level: &str, tags: serde_json::Value| {
            prepare(
                "p".into(),
                ev(serde_json::json!({ "level": level, "tags": tags })),
            )
            .page
        };
        assert!(page("fatal", serde_json::json!({ "flash.erased": "true" })));
        assert!(page("fatal", serde_json::json!({ "flash.erased": true })));
        assert!(page("fatal", serde_json::json!([["flash.erased", "true"]])));
        assert!(!page(
            "error",
            serde_json::json!({ "flash.erased": "true" })
        ));
        assert!(!page(
            "fatal",
            serde_json::json!({ "flash.erased": "false" })
        ));
        assert!(!page("fatal", serde_json::json!({})));
    }

    fn licensing(status: serde_json::Value) -> Event {
        ev(serde_json::json!({
            "level": "warning",
            "exception": { "values": [{ "type": "licensing_refused", "value": "v" }] },
            "tags": { "api.status": status, "api.code": "bad_credentials" },
        }))
    }

    #[test]
    fn drop_rules_match_the_type_and_every_tag_condition() {
        let rules = DropRules::parse(
            " licensing_refused:api.status=401|403|404|409|410|429 ; flash_ok;\
             flash_failed:flash.erased=false,flash.kind=adapter|timeout ",
        )
        .unwrap();
        assert!(rules.drops(&licensing(serde_json::json!("401"))));
        assert!(rules.drops(&licensing(serde_json::json!(429))));
        assert!(!rules.drops(&licensing(serde_json::json!("500"))));
        assert!(!rules.drops(&licensing(serde_json::json!("400"))));

        let typed = |ty: &str, tags: serde_json::Value| {
            ev(serde_json::json!({
                "exception": { "values": [{ "type": ty, "value": "v" }] },
                "tags": tags,
            }))
        };
        assert!(rules.drops(&typed("flash_ok", serde_json::json!({}))));
        assert!(!rules.drops(&typed("flash_okay", serde_json::json!({}))));
        let flash = |erased: &str, kind: &str| {
            typed(
                "flash_failed",
                serde_json::json!([["flash.erased", erased], ["flash.kind", kind]]),
            )
        };
        assert!(rules.drops(&flash("false", "adapter")));
        assert!(
            !rules.drops(&flash("true", "adapter")),
            "all conditions hold"
        );
        assert!(!rules.drops(&flash("false", "rejected")));
        assert!(
            !rules.drops(&typed("flash_failed", serde_json::json!({}))),
            "a missing tag never holds"
        );
        assert!(!rules.drops(&ev(serde_json::json!({ "message": "flash_ok" }))));
    }

    #[test]
    fn drop_rules_parse_strictly() {
        assert!(DropRules::parse("").unwrap().is_empty());
        assert!(DropRules::parse(" ; ").unwrap().is_empty());
        assert!(!DropRules::default().drops(&licensing(serde_json::json!("401"))));
        assert!(DropRules::parse(":api.status=401").is_err());
        assert!(DropRules::parse("licensing_refused:api.status").is_err());
        assert!(DropRules::parse("licensing_refused:api.status=").is_err());
        assert!(DropRules::parse("licensing_refused:=401").is_err());
        assert!(DropRules::parse("two words").is_err());
    }

    #[test]
    fn prepare_carries_the_release() {
        let rec = prepare(
            "p".into(),
            ev(
                serde_json::json!({ "message": "m", "release": "com.zeusmotorsport.suite@1.2.3+45" }),
            ),
        );
        assert_eq!(
            rec.release.as_deref(),
            Some("com.zeusmotorsport.suite@1.2.3+45")
        );
        let empty = prepare(
            "p".into(),
            ev(serde_json::json!({ "message": "m", "release": "" })),
        );
        assert_eq!(empty.release, None, "an empty release is no release");
    }

    #[test]
    fn prepare_handles_event_without_exception() {
        let raw = r#"{"timestamp":"2026-01-01T00:00:00Z","message":"hi"}"#;
        let ev: Event = serde_json::from_str(raw).expect("valid event JSON");
        let rec = prepare("p1".into(), ev);
        assert_eq!(rec.exception_type, "<no exception>");
        assert_eq!(rec.first_frame, "<no frames>");
        assert_eq!(rec.level, None);
        assert_eq!(rec.title, "hi");
    }
}
