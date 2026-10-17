//! End-to-end ingest pipeline: envelope in, digest task persists, API out.
//!
//! `api.rs` drains the ingest channel without persisting; these tests run
//! the real digest task against a tempdir Store so they can assert on what
//! is stored and what `/api/issues/:id/event` returns.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tower::util::ServiceExt;

#[path = "../src/auth.rs"]
mod auth;
#[path = "../src/client_ip.rs"]
mod client_ip;
#[path = "../src/crypto.rs"]
mod crypto;
#[path = "../src/digest.rs"]
mod digest;
#[path = "../src/error.rs"]
mod error;
#[path = "../src/fingerprint.rs"]
mod fingerprint;
#[path = "../src/ingest.rs"]
mod ingest;
#[path = "../src/lockout.rs"]
mod lockout;
#[path = "../src/metrics.rs"]
mod metrics;
#[path = "../src/rate_limit.rs"]
mod rate_limit;
#[path = "../src/scrub.rs"]
mod scrub;
#[path = "../src/spa.rs"]
mod spa;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/user_agent.rs"]
mod user_agent;
#[path = "../src/webhook.rs"]
mod webhook;
#[path = "../src/ws.rs"]
mod ws;

use store::Store;

fn unique_tempdir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "errexd-pipeline-{}-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct Pipeline {
    router: axum::Router,
    store: Store,
    alerts: mpsc::Receiver<webhook::Trigger>,
    _dir: PathBuf,
}

async fn pipeline() -> Pipeline {
    pipeline_with(digest::DropRules::default()).await
}

async fn pipeline_with(drop_rules: digest::DropRules) -> Pipeline {
    let dir = unique_tempdir();
    let store = Store::open(&dir.join("errex.db")).await.unwrap();
    store.migrate().await.unwrap();
    let (tx, rx) = mpsc::channel(16);
    let (fanout_tx, _) = tokio::sync::broadcast::channel(8);
    let (webhook_tx, alerts) = mpsc::channel(8);
    tokio::spawn(digest::run(
        store.clone(),
        rx,
        fanout_tx.clone(),
        webhook::AlertSinks {
            webhook: webhook_tx.clone(),
            telegram: None,
        },
    ));
    let state = Arc::new(ingest::AppState {
        events: tx,
        store: store.clone(),
        fanout: fanout_tx,
        require_auth: false,
        rate_limiter: Arc::new(rate_limit::RateLimiter::new(0, 0)),
        setup_token: None,
        public_url: "http://test.local:9090".into(),
        dev_mode: true,
        client_ip: client_ip::ClientIpPolicy::Peer,
        metrics: Arc::new(metrics::Metrics::new()),
        webhook_capacity: 8,
        webhook_sender: webhook_tx,
        ingest_inflight: Arc::new(tokio::sync::Semaphore::new(8)),
        ingest_budget: Arc::new(tokio::sync::Semaphore::new(ingest::INGEST_BUDGET_KIB)),
        drop_rules,
    });
    Pipeline {
        router: ingest::build_router(state),
        store,
        alerts,
        _dir: dir,
    }
}

impl Pipeline {
    async fn ingest(&self, event: Value) {
        let body = format!("{{}}\n{{\"type\":\"event\"}}\n{event}\n");
        let res = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/suite-prod/envelope/")
                    .header("cf-connecting-ip", "203.0.113.77")
                    .header("x-forwarded-for", "203.0.113.78")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// Waits for the digest task to persist the first issue's event and
    /// returns it as `/api/issues/:id/event` serves it.
    async fn stored_event(&self) -> Value {
        for _ in 0..100 {
            let issues = self
                .store
                .list_issues_by_project("suite-prod")
                .await
                .unwrap();
            if let Some(issue) = issues.first() {
                if self.store.latest_event(issue.id).await.unwrap().is_some() {
                    return self.api_event(issue.id).await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("event never persisted");
    }

    async fn api_event(&self, id: i64) -> Value {
        self.store
            .create_user("viewer", "hash", store::Role::Viewer)
            .await
            .unwrap();
        self.store
            .create_session("s-viewer", "viewer", None, None)
            .await
            .unwrap();
        let res = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/issues/{id}/event"))
                    .header("cookie", "errex_session=s-viewer")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }
}

#[tokio::test]
async fn unknown_fields_survive_storage_and_reach_the_api() {
    let p = pipeline().await;
    p.ingest(json!({
        "message": "m",
        "sdk": { "name": "zeus.reporter", "version": "1.0.0" },
        "dist": "45",
        "debug_meta": { "images": [{ "type": "elf", "debug_id": "0f3a5b7c-0000-4000-8000-000000000001" }] },
        "exception": { "values": [{
            "type": "StateError",
            "mechanism": { "type": "FlutterError", "handled": false },
            "stacktrace": { "frames": [{ "function": "f", "instruction_addr": "0x2f5c1f" }] }
        }]}
    }))
    .await;
    let got = p.stored_event().await;
    let payload = &got["payload"];
    assert_eq!(payload["sdk"]["name"], "zeus.reporter");
    assert_eq!(payload["dist"], "45");
    assert_eq!(
        payload["debug_meta"]["images"][0]["debug_id"],
        "0f3a5b7c-0000-4000-8000-000000000001"
    );
    assert_eq!(
        payload["exception"]["values"][0]["mechanism"]["handled"],
        false
    );
    assert_eq!(
        payload["exception"]["values"][0]["stacktrace"]["frames"][0]["instruction_addr"],
        "0x2f5c1f"
    );
}

#[tokio::test]
async fn nothing_scrubbed_or_identifying_is_stored() {
    let p = pipeline().await;
    p.ingest(json!({
        "level": "error",
        "message": "flash of WBS8M9C50J5K12345 failed at 0x80040000 for a@b.com",
        "user": { "ip_address": "203.0.113.9", "id": "u1" },
        "request": { "env": { "REMOTE_ADDR": "203.0.113.10" } },
        "tags": { "zfl": "payload", "flash.stage": "erase" },
        "extra": { "calibration": "AAAA", "flash": { "acked": 3 } },
        "breadcrumbs": { "values": [{ "message": "Bearer abc123" }] }
    }))
    .await;
    let got = p.stored_event().await;
    let text = got.to_string();
    for needle in [
        "WBS8M9C50J5K12345",
        "0x80040000",
        "a@b.com",
        "203.0.113.",
        "abc123",
        "calibration",
        "zfl",
        "ip_address",
        "REMOTE_ADDR",
    ] {
        assert!(!text.contains(needle), "{needle} reached storage: {text}");
    }
    assert_eq!(
        got["payload"]["message"],
        "flash of <vin> failed at <hex> for <email>"
    );
    assert_eq!(got["payload"]["tags"]["flash.stage"], "erase");
    assert_eq!(got["payload"]["extra"]["flash"]["acked"], 3);
}

#[tokio::test]
async fn a_fatal_erased_flash_pages_on_every_event() {
    let mut p = pipeline().await;
    let ev = json!({
        "level": "fatal",
        "release": "com.zeusmotorsport.suite@1.2.3+45",
        "fingerprint": ["flash", "timeout", "erase"],
        "message": "dme silent after erase",
        "tags": { "flash.erased": "true" }
    });
    p.ingest(ev.clone()).await;
    p.ingest(ev).await;
    for _ in 0..2 {
        let t = tokio::time::timeout(std::time::Duration::from_secs(2), p.alerts.recv())
            .await
            .expect("page fired")
            .unwrap();
        assert_eq!(t.kind, webhook::TriggerKind::Page);
        assert_eq!(
            t.release.as_deref(),
            Some("com.zeusmotorsport.suite@1.2.3+45")
        );
    }
}

#[tokio::test]
async fn a_drop_rule_answers_ok_and_stores_nothing() {
    let mut p = pipeline_with(
        digest::DropRules::parse("licensing_refused:api.status=401|403|404|409|410|429").unwrap(),
    )
    .await;
    let refused = |status: &str| {
        json!({
            "level": "warning",
            "fingerprint": ["licensing", "auth/login", "bad_credentials"],
            "exception": { "values": [{ "type": "licensing_refused", "value": "refused" }] },
            "tags": { "api.status": status, "api.code": "bad_credentials" }
        })
    };
    // `ingest` asserts the 200 an SDK needs to stop retrying.
    p.ingest(refused("401")).await;
    p.ingest(refused("429")).await;
    p.ingest(refused("503")).await;
    let got = p.stored_event().await;
    assert_eq!(got["payload"]["tags"]["api.status"], "503");
    let issues = p.store.list_issues_by_project("suite-prod").await.unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(
        issues[0].event_count, 1,
        "the dropped events were not stored"
    );
    let alert = p.alerts.recv().await.unwrap();
    assert_eq!(alert.kind, webhook::TriggerKind::NewIssue);
    assert!(
        p.alerts.try_recv().is_err(),
        "one alert, for the stored event"
    );
}
