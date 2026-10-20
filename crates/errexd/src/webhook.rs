//! Outbound webhook delivery for issue alerts.
//!
//! The digest task pushes `Trigger`s onto an mpsc channel; this task drains
//! them, looks up the project's webhook URL, and POSTs a Slack/Discord/Teams-
//! compatible JSON payload. Failures are logged and dropped — webhook
//! delivery is best-effort, not transactional. (If you need durable
//! delivery, fan out to your own queue.)
//!
//! Lightweight: a single shared hyper Client (HTTP/1, rustls + webpki-roots,
//! ring crypto) and one task. No reqwest — the builder/multipart/cookie/
//! redirect machinery was dead weight at idle for the fire-and-forget JSON
//! POSTs we actually issue.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use url::Url;

use errex_proto::{Issue, IssueStatus};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::store::Store;

pub(crate) type WebhookClient = Client<HttpsConnector<HttpConnector<CheckedResolver>>, Full<Bytes>>;

pub(crate) fn build_client() -> WebhookClient {
    let mut http = HttpConnector::new_with_resolver(CheckedResolver);
    // Bound the TCP handshake — webhooks are not a load-bearing path,
    // so a slow connect must not stall the task. Mirrors the prior
    // `reqwest::ClientBuilder::connect_timeout(2s)`.
    http.set_connect_timeout(Some(Duration::from_secs(2)));
    http.enforce_http(false);

    let https: HttpsConnector<HttpConnector<CheckedResolver>> = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);

    Client::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .build(https)
}

/// Total per-call deadline, including DNS, TCP, TLS, and the response.
/// Webhooks redirect to `https://hooks.slack.com/...` style endpoints
/// where 10 s is generous; anything slower is failing somewhere.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Response bytes read from a webhook endpoint before giving up on the body.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

async fn post_json(client: &WebhookClient, url: &str, body: &Value) -> Result<u16, String> {
    send_json(client, url, body).await.map(|(status, _)| status)
}

/// POST `body` as JSON and return the status with up to
/// [`MAX_RESPONSE_BYTES`] of the response body.
pub(crate) async fn send_json(
    client: &WebhookClient,
    url: &str,
    body: &Value,
) -> Result<(u16, Bytes), String> {
    let body_bytes = serde_json::to_vec(body).map_err(|e| format!("serialize body: {e}"))?;
    let req = Request::builder()
        .method(Method::POST)
        .uri(url)
        .header("content-type", "application/json")
        .header("user-agent", concat!("errexd/", env!("CARGO_PKG_VERSION")))
        .body(Full::new(Bytes::from(body_bytes)))
        .map_err(|_| "build request".to_string())?;

    let exchange = async {
        let resp = client
            .request(req)
            .await
            .map_err(|e| format!("send: {e}"))?;
        let status = resp.status().as_u16();
        // Draining also returns the connection to the pool.
        let body = Limited::new(resp.into_body(), MAX_RESPONSE_BYTES)
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        Ok((status, body))
    };
    tokio::time::timeout(SEND_TIMEOUT, exchange)
        .await
        .unwrap_or_else(|_| Err("timeout".into()))
}

/// Test mock servers listen on loopback, so tests allow private destinations.
const ALLOW_PRIVATE_DESTINATIONS: bool = cfg!(test);

/// Resolves webhook hosts and refuses the lookup if any answer is private.
/// The connector dials only the addresses returned here, so a DNS answer
/// cannot change between the check and the connection.
#[derive(Clone)]
pub(crate) struct CheckedResolver;

impl tower_service::Service<Name> for CheckedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if !ALLOW_PRIVATE_DESTINATIONS {
                reject_private_addrs(addrs.iter().map(SocketAddr::ip))
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
            }
            Ok(addrs.into_iter())
        })
    }
}

fn reject_private_addrs(ips: impl Iterator<Item = IpAddr>) -> Result<(), String> {
    for ip in ips {
        if ip_is_private_or_local(ip) {
            return Err("destination resolves to a private address".into());
        }
    }
    Ok(())
}

/// Validate a configured webhook URL before storing it. The webhook task
/// will POST to whatever lands here, so this is the SSRF gate: a hostile
/// admin (or hijacked admin cookie) that could otherwise pivot to AWS
/// IMDS, GCP metadata, internal services on the same host, or RFC1918
/// neighbors is rejected up front.
///
/// Checks (synchronous, no DNS to keep tests hermetic):
///   * scheme must be `http` or `https`
///   * URL must not embed userinfo (`http://user:pass@…`)
///   * if the host is an IP literal, it must not be in a private,
///     loopback, link-local, multicast, broadcast, or unspecified range
///   * if the host is a name, common loopback / internal-zone TLDs are
///     refused (`localhost`, `.local`, `.internal`, `.lan`, `.intranet`,
///     `.corp`, `.home.arpa`)
///
/// Names are checked again at delivery time by `CheckedResolver`, which
/// catches a public name that resolves to a private address.
pub fn validate_url(raw: &str) -> Result<(), &'static str> {
    let url = Url::parse(raw).map_err(|_| "invalid url")?;
    match url.scheme() {
        "http" | "https" => {}
        _ => return Err("scheme must be http or https"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("userinfo not allowed in webhook url");
    }
    let host = url.host_str().ok_or("missing host")?;
    // host_str() returns IPv6 hosts wrapped in `[…]` literals; strip them
    // so `parse::<IpAddr>()` recognises the address.
    let host_for_parse = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host_for_parse.parse::<IpAddr>() {
        if ip_is_private_or_local(ip) {
            return Err("private, loopback, or link-local addresses not allowed");
        }
        return Ok(());
    }
    let lower = host.trim_end_matches('.').to_ascii_lowercase();
    if lower == "localhost" || hostname_in_internal_zone(&lower) {
        return Err("loopback / internal-zone hostnames not allowed");
    }
    Ok(())
}

fn hostname_in_internal_zone(host: &str) -> bool {
    const INTERNAL_SUFFIXES: &[&str] = &[
        ".localhost",
        ".local",
        ".internal",
        ".lan",
        ".intranet",
        ".corp",
        ".home.arpa",
    ];
    INTERNAL_SUFFIXES.iter().any(|s| host.ends_with(s))
}

fn ip_is_private_or_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // CGNAT 100.64.0.0/10 — common cloud carrier-internal range
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_is_private_or_local(IpAddr::V4(v4));
            }
            let segs = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // unique local fc00::/7
                || (segs[0] & 0xfe00) == 0xfc00
                // link-local fe80::/10
                || (segs[0] & 0xffc0) == 0xfe80
        }
    }
}

/// What the digest task fires when an alert-worthy event happens.
#[derive(Debug, Clone)]
pub struct Trigger {
    pub issue: Issue,
    pub kind: TriggerKind,
    /// Release of the event that fired the trigger. Issues do not carry one.
    pub release: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// First occurrence of a fingerprint — always notify.
    NewIssue,
    /// Resolved issue saw an event from a release newer than the one it was
    /// resolved in — also notify.
    Regression,
    /// An open issue's first event from a release newer than every release
    /// that reported it before. Once per issue per release.
    NewRelease,
    /// An event matched the paging rule (`digest::pages`). Fires on every
    /// such event, whatever the issue's state: each one is a different car.
    Page,
}

impl TriggerKind {
    pub fn label(self) -> &'static str {
        match self {
            TriggerKind::NewIssue => "New issue",
            TriggerKind::Regression => "Regression",
            TriggerKind::NewRelease => "New release",
            TriggerKind::Page => "PAGE: fatal with erased flash",
        }
    }
    pub fn slack_color(self) -> &'static str {
        // Slack/Discord both interpret these named colors.
        match self {
            TriggerKind::NewIssue | TriggerKind::Page => "danger",
            TriggerKind::Regression | TriggerKind::NewRelease => "warning",
        }
    }
}

impl Trigger {
    /// The alert's first line: the kind, naming the release where the kind
    /// is about one.
    pub fn headline(&self) -> String {
        match (self.kind, self.release.as_deref()) {
            (TriggerKind::Regression, Some(release)) => format!("Regressed in {release}"),
            (TriggerKind::NewRelease, Some(release)) => format!("Now also in {release}"),
            (kind, _) => kind.label().to_string(),
        }
    }
}

/// Muted and ignored issues stay quiet, except for a page.
pub(crate) fn should_alert(t: &Trigger) -> bool {
    t.kind == TriggerKind::Page
        || !matches!(t.issue.status, IssueStatus::Muted | IssueStatus::Ignored)
}

/// The digest task's alert outputs. Each sink has its own bounded channel
/// and task, so a slow webhook endpoint never delays a Telegram page. A full
/// channel drops the alert (surfaced through `/metrics` for the webhook).
#[derive(Debug, Clone)]
pub struct AlertSinks {
    pub webhook: mpsc::Sender<Trigger>,
    pub telegram: Option<mpsc::Sender<Trigger>>,
}

impl AlertSinks {
    pub fn fire(&self, t: Trigger) {
        if let Some(tg) = &self.telegram {
            let _ = tg.try_send(t.clone());
        }
        let _ = self.webhook.try_send(t);
    }
}

/// Build a Slack-compatible message body. Discord and Teams' "Slack
/// compatible" webhook endpoints accept the same shape.
pub fn build_payload(t: &Trigger, public_url: &str) -> Value {
    let issue = &t.issue;
    let title = format!("{}: {}", t.headline(), issue.title);
    let link = format!("{}/issues/{}", public_url.trim_end_matches('/'), issue.id);
    let mut fields = vec![
        json!({"title": "Project", "value": issue.project, "short": true}),
        json!({"title": "Events",  "value": issue.event_count.to_string(), "short": true}),
    ];
    if let Some(level) = &issue.level {
        fields.push(json!({"title": "Level", "value": level, "short": true}));
    }
    if let Some(release) = &t.release {
        fields.push(json!({"title": "Release", "value": release, "short": true}));
    }
    if let Some(culprit) = &issue.culprit {
        fields.push(json!({"title": "Culprit", "value": culprit, "short": false}));
    }
    json!({
        "text": title,
        "attachments": [{
            "title": issue.title,
            "title_link": link,
            "color": t.kind.slack_color(),
            "fields": fields,
            "fallback": title,
            "ts": issue.last_seen.timestamp(),
        }]
    })
}

/// Spawned task. Reads triggers, drops them silently for muted/ignored
/// issues, looks up the project's webhook URL, fires the POST.
pub async fn run(store: Store, public_url: String, mut rx: mpsc::Receiver<Trigger>) {
    // Hyper Client does not follow redirects on its own — that matches the
    // SSRF policy: a 30x pointing at an internal address would re-do the
    // pivot the configured-URL validator just refused. Followers can update
    // their endpoint instead of relying on us to chase 302s.
    let client = build_client();

    tracing::info!("webhook task started");
    while let Some(trigger) = rx.recv().await {
        // Don't notify on muted/ignored issues — that's the whole point of
        // those statuses. Regression of a previously-resolved issue is
        // explicitly NOT muted because the regression event already flipped
        // it to unresolved before reaching here.
        if !should_alert(&trigger) {
            continue;
        }

        let url = match store.project_by_name(&trigger.issue.project).await {
            Ok(Some(p)) => p.webhook_url,
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(%err, project = %trigger.issue.project, "webhook: project lookup failed");
                continue;
            }
        };
        let Some(url) = url else { continue };

        let body = build_payload(&trigger, &public_url);
        let res = post_json(&client, &url, &body).await;
        // Surface the most recent delivery outcome on the project row so the
        // /projects/[name] console can render "last delivery: 200 · 12s ago"
        // (or 404, or "never delivered") without a separate history table.
        // Status 0 is the "transport failure" sentinel — see the migration.
        let status_code = match &res {
            Ok(code) => *code,
            Err(_) => 0,
        };
        store
            .record_webhook_attempt(&trigger.issue.project, status_code)
            .await;
        match res {
            Ok(code) if (200..300).contains(&code) => {
                tracing::info!(
                    project = %trigger.issue.project,
                    issue_id = trigger.issue.id,
                    kind = ?trigger.kind,
                    "webhook delivered",
                );
            }
            Ok(code) => {
                tracing::warn!(
                    project = %trigger.issue.project,
                    issue_id = trigger.issue.id,
                    status = code,
                    "webhook returned non-2xx",
                );
            }
            Err(err) => {
                tracing::warn!(err, project = %trigger.issue.project, "webhook send failed");
            }
        }
    }
    tracing::info!("webhook task exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use errex_proto::Fingerprint;

    fn issue() -> Issue {
        Issue {
            id: 42,
            project: "demo".into(),
            fingerprint: Fingerprint::new("abc"),
            title: "TypeError: x".into(),
            culprit: Some("checkout in pay.ts".into()),
            level: Some("error".into()),
            status: IssueStatus::Unresolved,
            event_count: 7,
            first_seen: Utc::now(),
            last_seen: Utc::now(),
            first_release: None,
            last_release: None,
            newest_release: None,
            resolved_in_release: None,
        }
    }

    // ----- validate_url -----
    //
    // SSRF gate. Each rejected case below is a real-world pivot we don't
    // want a hostile (or hijacked) admin to be able to fire from the
    // webhook config endpoint.

    #[test]
    fn validate_url_accepts_public_https() {
        assert!(validate_url("https://hooks.slack.com/services/ABC/DEF").is_ok());
        assert!(validate_url("https://discord.com/api/webhooks/x/y").is_ok());
        assert!(validate_url("http://example.com:8080/hook").is_ok());
    }

    #[test]
    fn validate_url_rejects_loopback_ipv4() {
        assert!(validate_url("http://127.0.0.1/hook").is_err());
        assert!(validate_url("http://127.0.0.1:9090/hook").is_err());
    }

    #[test]
    fn validate_url_rejects_aws_imds() {
        // 169.254.169.254 — the most common cloud-metadata SSRF target.
        assert!(validate_url("http://169.254.169.254/latest/meta-data/").is_err());
    }

    #[test]
    fn validate_url_rejects_rfc1918_ipv4() {
        assert!(validate_url("http://10.0.0.1/").is_err());
        assert!(validate_url("http://192.168.1.50/x").is_err());
        assert!(validate_url("http://172.16.0.1/x").is_err());
    }

    #[test]
    fn validate_url_rejects_unspecified_and_broadcast() {
        assert!(validate_url("http://0.0.0.0/").is_err());
        assert!(validate_url("http://255.255.255.255/").is_err());
    }

    #[test]
    fn validate_url_rejects_loopback_ipv6() {
        assert!(validate_url("http://[::1]/").is_err());
    }

    #[test]
    fn validate_url_rejects_unique_local_ipv6() {
        assert!(validate_url("http://[fc00::1]/").is_err());
        assert!(validate_url("http://[fd12:3456::1]/").is_err());
    }

    #[test]
    fn validate_url_rejects_link_local_ipv6() {
        assert!(validate_url("http://[fe80::1]/").is_err());
    }

    #[test]
    fn validate_url_rejects_localhost_hostname() {
        assert!(validate_url("http://localhost/x").is_err());
        assert!(validate_url("http://localhost:9090/x").is_err());
    }

    #[test]
    fn validate_url_rejects_internal_zone_tlds() {
        assert!(validate_url("http://api.internal/").is_err());
        assert!(validate_url("http://server.local/").is_err());
        assert!(validate_url("http://x.lan/").is_err());
        assert!(validate_url("http://app.intranet/").is_err());
        assert!(validate_url("http://a.b.localhost/").is_err());
    }

    #[test]
    fn validate_url_rejects_userinfo() {
        assert!(validate_url("http://user:pass@example.com/").is_err());
        assert!(validate_url("http://user@example.com/").is_err());
    }

    #[test]
    fn validate_url_rejects_non_http_schemes() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("ftp://example.com/").is_err());
        assert!(validate_url("gopher://example.com/").is_err());
        // `data:` URIs would never reach here as a webhook, but a careless
        // operator paste shouldn't be allowed to misconfigure either.
        assert!(validate_url("data:text/plain,hi").is_err());
    }

    #[test]
    fn validate_url_rejects_unparseable_input() {
        assert!(validate_url("not a url").is_err());
        assert!(validate_url("").is_err());
    }

    #[test]
    fn payload_for_new_issue_uses_danger_color() {
        let t = Trigger {
            issue: issue(),
            kind: TriggerKind::NewIssue,
            release: None,
        };
        let v = build_payload(&t, "https://errex.example.com");
        let attachments = v.get("attachments").and_then(|a| a.as_array()).unwrap();
        assert_eq!(
            attachments[0].get("color").and_then(|c| c.as_str()),
            Some("danger")
        );
        assert!(v
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap()
            .starts_with("New issue:"));
    }

    #[test]
    fn payload_for_regression_uses_warning_color() {
        let t = Trigger {
            issue: issue(),
            kind: TriggerKind::Regression,
            release: None,
        };
        let v = build_payload(&t, "https://errex.example.com");
        let attachments = v.get("attachments").and_then(|a| a.as_array()).unwrap();
        assert_eq!(
            attachments[0].get("color").and_then(|c| c.as_str()),
            Some("warning")
        );
        assert!(v
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap()
            .starts_with("Regression:"));
    }

    #[test]
    fn regression_and_new_release_payloads_name_the_release() {
        let t = |kind| Trigger {
            issue: issue(),
            kind,
            release: Some("zeus-suite@0.0.15+12".into()),
        };
        let v = build_payload(&t(TriggerKind::Regression), "https://errex.example.com");
        assert_eq!(
            v["text"].as_str().unwrap(),
            "Regressed in zeus-suite@0.0.15+12: TypeError: x"
        );
        let v = build_payload(&t(TriggerKind::NewRelease), "https://errex.example.com");
        assert_eq!(
            v["text"].as_str().unwrap(),
            "Now also in zeus-suite@0.0.15+12: TypeError: x"
        );
        assert_eq!(v.pointer("/attachments/0/color").unwrap(), "warning");
    }

    #[test]
    fn payload_carries_the_release_when_known() {
        let t = Trigger {
            issue: issue(),
            kind: TriggerKind::Page,
            release: Some("com.zeusmotorsport.suite@1.2.3+45".into()),
        };
        let v = build_payload(&t, "https://errex.example.com");
        let fields = v
            .pointer("/attachments/0/fields")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(fields
            .iter()
            .any(|f| f["title"] == "Release" && f["value"] == "com.zeusmotorsport.suite@1.2.3+45"));
        assert!(v["text"].as_str().unwrap().starts_with("PAGE"));
    }

    #[test]
    fn muted_issues_stay_quiet_except_for_a_page() {
        let mut muted = issue();
        muted.status = IssueStatus::Muted;
        let t = |kind| Trigger {
            issue: muted.clone(),
            kind,
            release: None,
        };
        assert!(!should_alert(&t(TriggerKind::NewIssue)));
        assert!(!should_alert(&t(TriggerKind::Regression)));
        assert!(!should_alert(&t(TriggerKind::NewRelease)));
        assert!(should_alert(&t(TriggerKind::Page)));
    }

    #[tokio::test]
    async fn alert_sinks_fan_out_to_every_channel() {
        let (wh_tx, mut wh_rx) = mpsc::channel(1);
        let (tg_tx, mut tg_rx) = mpsc::channel(1);
        let sinks = AlertSinks {
            webhook: wh_tx,
            telegram: Some(tg_tx),
        };
        sinks.fire(Trigger {
            issue: issue(),
            kind: TriggerKind::Page,
            release: None,
        });
        assert_eq!(wh_rx.recv().await.unwrap().kind, TriggerKind::Page);
        assert_eq!(tg_rx.recv().await.unwrap().kind, TriggerKind::Page);
    }

    #[test]
    fn payload_includes_issue_link() {
        let t = Trigger {
            issue: issue(),
            kind: TriggerKind::NewIssue,
            release: None,
        };
        let v = build_payload(&t, "https://errex.example.com/");
        let link = v
            .pointer("/attachments/0/title_link")
            .and_then(|l| l.as_str())
            .unwrap();
        assert_eq!(link, "https://errex.example.com/issues/42");
    }

    #[test]
    fn payload_includes_project_and_event_count_fields() {
        let t = Trigger {
            issue: issue(),
            kind: TriggerKind::NewIssue,
            release: None,
        };
        let v = build_payload(&t, "https://errex.example.com");
        let fields = v
            .pointer("/attachments/0/fields")
            .and_then(|f| f.as_array())
            .unwrap();
        let titles: Vec<_> = fields
            .iter()
            .filter_map(|f| f.get("title").and_then(|t| t.as_str()))
            .collect();
        assert!(titles.contains(&"Project"));
        assert!(titles.contains(&"Events"));
    }

    // ----- delivery + health recording (integration) -----
    //
    // These tests boot a one-shot axum server on a random port so the real
    // reqwest client in `run` actually performs an HTTP round-trip. We then
    // observe the side-effect on the Store, which is what the projects
    // console reads.

    use crate::store::Store;
    use axum::{routing::post, Router};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::TcpListener;

    fn unique_tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "errexd-webhook-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Boot a tiny HTTP server that returns the configured status code for
    /// every POST. Returns the bound URL plus a JoinHandle the test can drop
    /// to terminate the server.
    async fn spawn_mock(status: u16) -> (String, tokio::task::JoinHandle<()>) {
        let code = Arc::new(AtomicU16::new(status));
        let code_for_handler = code.clone();
        let app = Router::new().route(
            "/hook",
            post(move || {
                let code = code_for_handler.clone();
                async move {
                    axum::http::StatusCode::from_u16(code.load(Ordering::Relaxed))
                        .unwrap_or(axum::http::StatusCode::OK)
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/hook"), handle)
    }

    /// Build an Issue whose project name matches the one we'll create in the
    /// store, so the webhook task's project lookup succeeds.
    fn issue_for(project: &str) -> Issue {
        let mut i = issue();
        i.project = project.to_string();
        i
    }

    /// Poll the project row up to 1s waiting for `last_webhook_status` to be
    /// non-None. Returns the observed status. Avoids a hard-coded sleep.
    async fn wait_for_status(store: &Store, project: &str) -> Option<i16> {
        for _ in 0..50 {
            let p = store.project_by_name(project).await.unwrap().unwrap();
            if p.last_webhook_status.is_some() {
                return p.last_webhook_status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        None
    }

    #[tokio::test]
    async fn successful_delivery_records_status_200() {
        let dir = unique_tempdir();
        let store = Store::open(&dir.join("errex.db")).await.unwrap();
        store.migrate().await.unwrap();
        store.create_project("p").await.unwrap();
        let (url, _server) = spawn_mock(200).await;
        store.set_project_webhook("p", Some(&url)).await.unwrap();

        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(run(store.clone(), "http://example".into(), rx));

        tx.send(Trigger {
            issue: issue_for("p"),
            kind: TriggerKind::NewIssue,
            release: None,
        })
        .await
        .unwrap();

        assert_eq!(wait_for_status(&store, "p").await, Some(200));
        drop(tx);
        let _ = task.await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn non_2xx_response_records_actual_status_code() {
        let dir = unique_tempdir();
        let store = Store::open(&dir.join("errex.db")).await.unwrap();
        store.migrate().await.unwrap();
        store.create_project("p").await.unwrap();
        let (url, _server) = spawn_mock(502).await;
        store.set_project_webhook("p", Some(&url)).await.unwrap();

        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(run(store.clone(), "http://example".into(), rx));

        tx.send(Trigger {
            issue: issue_for("p"),
            kind: TriggerKind::NewIssue,
            release: None,
        })
        .await
        .unwrap();

        assert_eq!(
            wait_for_status(&store, "p").await,
            Some(502),
            "console must reflect the upstream's failure status, not 200",
        );
        drop(tx);
        let _ = task.await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unreachable_endpoint_records_zero() {
        let dir = unique_tempdir();
        let store = Store::open(&dir.join("errex.db")).await.unwrap();
        store.migrate().await.unwrap();
        store.create_project("p").await.unwrap();
        // Reserved address that should refuse immediately on most hosts.
        // The webhook client has a 10s timeout but a connection-refused
        // returns much faster; the test still bounds via wait_for_status.
        store
            .set_project_webhook("p", Some("http://127.0.0.1:1/hook"))
            .await
            .unwrap();

        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(run(store.clone(), "http://example".into(), rx));

        tx.send(Trigger {
            issue: issue_for("p"),
            kind: TriggerKind::NewIssue,
            release: None,
        })
        .await
        .unwrap();

        // Generous 5s for transport failure to land — refused on most hosts
        // but slower under contention or strict firewalls.
        let mut observed = None;
        for _ in 0..250 {
            let p = store.project_by_name("p").await.unwrap().unwrap();
            if p.last_webhook_status.is_some() {
                observed = p.last_webhook_status;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            observed,
            Some(0),
            "transport failure must record sentinel 0"
        );

        drop(tx);
        let _ = task.await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_webhook_url_does_not_record_anything() {
        // A project with no webhook URL must leave last_webhook_status as
        // None — otherwise the console can't tell "never tried" from
        // "tried and failed".
        let dir = unique_tempdir();
        let store = Store::open(&dir.join("errex.db")).await.unwrap();
        store.migrate().await.unwrap();
        store.create_project("p").await.unwrap();

        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(run(store.clone(), "http://example".into(), rx));

        tx.send(Trigger {
            issue: issue_for("p"),
            kind: TriggerKind::NewIssue,
            release: None,
        })
        .await
        .unwrap();

        // Give the task time to drain the channel and skip the delivery.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let p = store.project_by_name("p").await.unwrap().unwrap();
        assert!(p.last_webhook_status.is_none());

        drop(tx);
        let _ = task.await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;

    #[test]
    fn rejects_mapped_loopback_and_trailing_dot_hosts() {
        assert!(validate_url("http://[::ffff:127.0.0.1]/hook").is_err());
        assert!(validate_url("http://localhost./hook").is_err());
    }

    #[test]
    fn refuses_resolved_private_addresses() {
        assert!(reject_private_addrs(["10.0.0.5".parse().unwrap()].into_iter()).is_err());
        assert!(reject_private_addrs(["93.184.216.34".parse().unwrap()].into_iter()).is_ok());
    }
}
