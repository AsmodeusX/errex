//! Telegram alerts: one bot posting to one group or channel.
//!
//! Fed by the same triggers as the webhooks (new issue, regression) plus
//! the paging rule (`digest::pages`), through its own channel and task so a
//! slow webhook endpoint never delays a page.
//!
//! Rate limits: new issues and regressions share a per-minute budget
//! (`ERREX_TELEGRAM_MAX_PER_MIN`); what it holds back is counted and
//! reported in the next message that goes out. Pages draw from a separate
//! budget at Telegram's own group limit, so a storm of new issues cannot
//! starve one. A 429 from Telegram is honoured once, up to a minute.
//!
//! The bot token is part of the request path. It is never logged: delivery
//! failures log the status and Telegram's description, never the URL.

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::rate_limit::TokenBucket;
use crate::webhook::{self, Trigger, TriggerKind};

#[derive(Clone)]
pub struct TelegramConfig {
    pub api_base: String,
    pub token: String,
    pub chat_id: String,
    pub max_per_min: u32,
}

// The token must not reach a log line through `{:?}`.
impl std::fmt::Debug for TelegramConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramConfig")
            .field("api_base", &self.api_base)
            .field("token", &"<redacted>")
            .field("chat_id", &self.chat_id)
            .field("max_per_min", &self.max_per_min)
            .finish()
    }
}

/// Telegram's documented ceiling for one group: 20 messages a minute.
const PAGE_PER_MIN: f64 = 20.0;
/// Longest wait honoured from a 429's `retry_after`.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// Alert budgets. `max_per_min == 0` leaves new issues and regressions
/// unlimited; pages always have their own budget.
struct Budget {
    normal: TokenBucket,
    normal_cap: f64,
    page: TokenBucket,
    held_back: u32,
}

impl Budget {
    fn new(max_per_min: u32, now: Instant) -> Self {
        let normal_cap = f64::from(max_per_min);
        Self {
            normal: TokenBucket::new(normal_cap, now),
            normal_cap,
            page: TokenBucket::new(PAGE_PER_MIN, now),
            held_back: 0,
        }
    }

    /// `Some(n)` when the alert may go out, `n` being how many were held
    /// back since the last one that did.
    fn admit(&mut self, kind: TriggerKind, now: Instant) -> Option<u32> {
        let ok = match kind {
            TriggerKind::Page => self
                .page
                .try_acquire(PAGE_PER_MIN, PAGE_PER_MIN / 60.0, now),
            _ if self.normal_cap == 0.0 => true,
            _ => self
                .normal
                .try_acquire(self.normal_cap, self.normal_cap / 60.0, now),
        };
        if ok {
            Some(std::mem::take(&mut self.held_back))
        } else {
            self.held_back += 1;
            None
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Message text in Telegram's HTML parse mode.
pub fn format_message(t: &Trigger, public_url: &str, held_back: u32) -> String {
    let issue = &t.issue;
    let link = format!("{}/issues/{}", public_url.trim_end_matches('/'), issue.id);
    let mut text = format!(
        "<b>{}</b>\n{}\n\nProject: {}\nLevel: {}\nRelease: {}\nEvents: {}\n{}",
        escape(t.kind.label()),
        escape(&issue.title),
        escape(&issue.project),
        escape(issue.level.as_deref().unwrap_or("-")),
        escape(t.release.as_deref().unwrap_or("-")),
        issue.event_count,
        escape(&link),
    );
    if held_back > 0 {
        text.push_str(&format!(
            "\n\n({held_back} earlier alert(s) held back by the rate limit)"
        ));
    }
    text
}

fn retry_after(body: &[u8]) -> Duration {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/parameters/retry_after").and_then(Value::as_u64))
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5))
        .min(MAX_RETRY_WAIT)
}

fn description(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

pub async fn run(cfg: TelegramConfig, public_url: String, mut rx: mpsc::Receiver<Trigger>) {
    let client = webhook::build_client();
    let url = format!("{}/bot{}/sendMessage", cfg.api_base, cfg.token);
    let mut budget = Budget::new(cfg.max_per_min, Instant::now());
    tracing::info!(chat_id = %cfg.chat_id, "telegram task started");

    while let Some(trigger) = rx.recv().await {
        if !webhook::should_alert(&trigger) {
            continue;
        }
        let Some(held_back) = budget.admit(trigger.kind, Instant::now()) else {
            continue;
        };
        let body = json!({
            "chat_id": cfg.chat_id,
            "text": format_message(&trigger, &public_url, held_back),
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });
        deliver(&client, &url, &body, &trigger).await;
    }
    tracing::info!("telegram task exiting");
}

async fn deliver(client: &webhook::WebhookClient, url: &str, body: &Value, t: &Trigger) {
    for attempt in 0..2 {
        match webhook::send_json(client, url, body).await {
            Ok((code, _)) if (200..300).contains(&code) => {
                tracing::info!(
                    project = %t.issue.project,
                    issue_id = t.issue.id,
                    kind = ?t.kind,
                    "telegram delivered",
                );
                return;
            }
            Ok((429, resp)) if attempt == 0 => {
                tokio::time::sleep(retry_after(&resp)).await;
            }
            Ok((code, resp)) => {
                tracing::warn!(
                    project = %t.issue.project,
                    status = code,
                    description = %description(&resp),
                    "telegram returned non-2xx",
                );
                return;
            }
            Err(_) => {
                tracing::warn!(project = %t.issue.project, "telegram send failed");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Path;
    use axum::routing::post;
    use axum::{Json, Router};
    use chrono::Utc;
    use errex_proto::{Fingerprint, Issue, IssueStatus};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    fn issue() -> Issue {
        Issue {
            id: 42,
            project: "suite-prod".into(),
            fingerprint: Fingerprint::new("abc"),
            title: "FlashError: car <vin> & co".into(),
            culprit: None,
            level: Some("fatal".into()),
            status: IssueStatus::Unresolved,
            event_count: 7,
            first_seen: Utc::now(),
            last_seen: Utc::now(),
        }
    }

    fn trigger(kind: TriggerKind) -> Trigger {
        Trigger {
            issue: issue(),
            kind,
            release: Some("com.zeusmotorsport.suite@1.2.3+45".into()),
        }
    }

    // ----- text -----

    #[test]
    fn message_names_project_level_title_release_count_and_link() {
        let text = format_message(
            &trigger(TriggerKind::NewIssue),
            "https://bt.example.com/",
            0,
        );
        assert!(text.starts_with("<b>New issue</b>"));
        assert!(text.contains("FlashError: car &lt;vin&gt; &amp; co"));
        assert!(text.contains("Project: suite-prod"));
        assert!(text.contains("Level: fatal"));
        assert!(text.contains("Release: com.zeusmotorsport.suite@1.2.3+45"));
        assert!(text.contains("Events: 7"));
        assert!(text.contains("https://bt.example.com/issues/42"));
        assert!(!text.contains("held back"));
    }

    #[test]
    fn message_reports_alerts_held_back() {
        let text = format_message(&trigger(TriggerKind::Page), "https://x", 3);
        assert!(text.starts_with("<b>PAGE"));
        assert!(text.contains("3 earlier alert(s) held back"));
    }

    #[test]
    fn debug_output_hides_the_token() {
        let cfg = TelegramConfig {
            api_base: "https://api.telegram.org".into(),
            token: "123:SECRET".into(),
            chat_id: "-100".into(),
            max_per_min: 10,
        };
        assert!(!format!("{cfg:?}").contains("SECRET"));
    }

    // ----- budget -----

    #[test]
    fn budget_holds_back_and_counts_then_recovers() {
        let t0 = Instant::now();
        let mut b = Budget::new(2, t0);
        assert_eq!(b.admit(TriggerKind::NewIssue, t0), Some(0));
        assert_eq!(b.admit(TriggerKind::Regression, t0), Some(0));
        assert_eq!(b.admit(TriggerKind::NewIssue, t0), None);
        assert_eq!(b.admit(TriggerKind::NewIssue, t0), None);
        // 30 s at 2/min refills one token; the next message reports two.
        let later = t0 + Duration::from_secs(30);
        assert_eq!(b.admit(TriggerKind::NewIssue, later), Some(2));
    }

    #[test]
    fn pages_are_not_starved_by_ordinary_alerts() {
        let t0 = Instant::now();
        let mut b = Budget::new(1, t0);
        assert_eq!(b.admit(TriggerKind::NewIssue, t0), Some(0));
        assert_eq!(b.admit(TriggerKind::NewIssue, t0), None);
        assert_eq!(b.admit(TriggerKind::Page, t0), Some(1));
        for _ in 1..20 {
            assert!(b.admit(TriggerKind::Page, t0).is_some());
        }
        assert_eq!(b.admit(TriggerKind::Page, t0), None, "Telegram's own limit");
    }

    #[test]
    fn zero_means_unlimited_for_ordinary_alerts() {
        let t0 = Instant::now();
        let mut b = Budget::new(0, t0);
        for _ in 0..100 {
            assert_eq!(b.admit(TriggerKind::NewIssue, t0), Some(0));
        }
    }

    #[test]
    fn retry_after_is_read_and_capped() {
        assert_eq!(
            retry_after(br#"{"ok":false,"parameters":{"retry_after":3}}"#),
            Duration::from_secs(3)
        );
        assert_eq!(
            retry_after(br#"{"parameters":{"retry_after":3600}}"#),
            MAX_RETRY_WAIT
        );
        assert_eq!(retry_after(b"junk"), Duration::from_secs(5));
    }

    // ----- delivery against a mock Bot API -----

    type Seen = Arc<Mutex<Vec<(String, Value)>>>;

    /// Mock Bot API. Answers each request with the next status in
    /// `statuses` (the last one repeats) and records path segment + body.
    async fn mock(statuses: Vec<u16>) -> (String, Seen) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let rec = seen.clone();
        let app = Router::new().route(
            "/:bot/sendMessage",
            post(move |Path(bot): Path<String>, Json(body): Json<Value>| {
                let rec = rec.clone();
                let statuses = statuses.clone();
                async move {
                    let n = {
                        let mut g = rec.lock().unwrap();
                        g.push((bot, body));
                        g.len()
                    };
                    let code = statuses[(n - 1).min(statuses.len() - 1)];
                    let body = if code == 429 {
                        json!({"ok": false, "error_code": 429, "parameters": {"retry_after": 1}})
                    } else {
                        json!({"ok": true, "result": {}})
                    };
                    (axum::http::StatusCode::from_u16(code).unwrap(), Json(body))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn cfg(api_base: String, max_per_min: u32) -> TelegramConfig {
        TelegramConfig {
            api_base,
            token: "123:abc".into(),
            chat_id: "-1001234".into(),
            max_per_min,
        }
    }

    async fn wait_for(seen: &Seen, n: usize, within: Duration) -> Vec<(String, Value)> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if seen.lock().unwrap().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        seen.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn delivers_to_the_configured_chat() {
        let (base, seen) = mock(vec![200]).await;
        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(run(cfg(base, 10), "https://bt.example.com".into(), rx));
        tx.send(trigger(TriggerKind::NewIssue)).await.unwrap();
        let got = wait_for(&seen, 1, Duration::from_secs(2)).await;
        assert_eq!(got.len(), 1);
        let (bot, body) = &got[0];
        assert_eq!(bot, "bot123:abc");
        assert_eq!(body["chat_id"], "-1001234");
        assert_eq!(body["parse_mode"], "HTML");
        assert!(body["text"].as_str().unwrap().contains("Events: 7"));
        drop(tx);
        let _ = task.await;
    }

    #[tokio::test]
    async fn a_429_is_retried_after_the_advertised_wait() {
        let (base, seen) = mock(vec![429, 200]).await;
        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(run(cfg(base, 10), "https://x".into(), rx));
        tx.send(trigger(TriggerKind::Page)).await.unwrap();
        let got = wait_for(&seen, 2, Duration::from_secs(4)).await;
        assert_eq!(got.len(), 2, "one retry after retry_after");
        drop(tx);
        let _ = task.await;
    }

    #[tokio::test]
    async fn muted_issues_are_quiet_but_pages_go_out() {
        let (base, seen) = mock(vec![200]).await;
        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(run(cfg(base, 10), "https://x".into(), rx));
        let mut muted = trigger(TriggerKind::NewIssue);
        muted.issue.status = IssueStatus::Muted;
        tx.send(muted.clone()).await.unwrap();
        muted.kind = TriggerKind::Page;
        tx.send(muted).await.unwrap();
        let got = wait_for(&seen, 1, Duration::from_secs(2)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(got[0].1["text"].as_str().unwrap().starts_with("<b>PAGE"));
        drop(tx);
        let _ = task.await;
    }

    #[tokio::test]
    async fn ordinary_alerts_over_the_limit_are_held_back() {
        let (base, seen) = mock(vec![200]).await;
        let (tx, rx) = mpsc::channel(8);
        let task = tokio::spawn(run(cfg(base, 2), "https://x".into(), rx));
        for _ in 0..5 {
            tx.send(trigger(TriggerKind::NewIssue)).await.unwrap();
        }
        tx.send(trigger(TriggerKind::Page)).await.unwrap();
        let got = wait_for(&seen, 3, Duration::from_secs(2)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            seen.lock().unwrap().len(),
            3,
            "two ordinary alerts and the page"
        );
        assert!(got[2].1["text"]
            .as_str()
            .unwrap()
            .contains("3 earlier alert(s) held back"));
        drop(tx);
        let _ = task.await;
    }
}
