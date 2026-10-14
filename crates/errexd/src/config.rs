use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use crate::client_ip::{self, ClientIpPolicy};
use crate::telegram::TelegramConfig;

/// `host:port`, bracketing a bare IPv6 host (`::1` -> `[::1]:9090`).
fn bind_addr(host: &str, port: u16) -> Result<SocketAddr, std::net::AddrParseError> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.contains(':') {
        format!("[{host}]:{port}").parse()
    } else {
        format!("{host}:{port}").parse()
    }
}

/// Daemon configuration. Every flag also reads from an `ERREX_*` env var so
/// `docker compose` can configure without overriding the entrypoint.
#[derive(Debug, Clone, Parser)]
#[command(name = "errexd", version, about = "errex error-tracking daemon")]
pub struct Config {
    /// Directory holding the SQLite database (and future data files).
    #[arg(long, env = "ERREX_DATA_DIR", default_value = "./data")]
    pub data_dir: PathBuf,

    /// Bind address for the HTTP ingest server. Loopback by default so a
    /// fresh `errexd` invocation is not reachable off-host until the
    /// operator opts in to a public bind. Docker port mapping requires the
    /// container's process to listen on `0.0.0.0`, so the supplied
    /// docker-compose.yml sets `ERREX_HOST=0.0.0.0` explicitly — the change
    /// is local-developer-friendly and prevents accidental public exposure
    /// on bare metal.
    #[arg(long, env = "ERREX_HOST", default_value = "127.0.0.1")]
    pub http_host: String,

    /// HTTP port. Reads from `ERREX_PORT` first, else falls back to
    /// `PORT` (the convention Railway / Fly / Heroku / Render all set on
    /// their side), else defaults to 9090. The fallback chain is what
    /// makes a one-click deploy "just work" without operator
    /// boilerplate. Resolved by [`Config::resolved_http_port`].
    #[arg(long, env = "ERREX_PORT")]
    pub http_port: Option<u16>,

    /// Bind address for the MCP server (AI agents). Loopback by default —
    /// the MCP listener is currently a stub that drops every connection,
    /// and a public bind here is gratuitous attack surface. Docker compose
    /// sets `ERREX_MCP_HOST=0.0.0.0` only when the operator explicitly
    /// wants the port exposed.
    #[arg(long, env = "ERREX_MCP_HOST", default_value = "127.0.0.1")]
    pub mcp_host: String,

    #[arg(long, env = "ERREX_MCP_PORT", default_value_t = 9092)]
    pub mcp_port: u16,

    /// Daemon log level. Accepts `trace`, `debug`, `info`, `warn`, `error`.
    /// Per-target directives (`RUST_LOG=foo::bar=debug`) are not supported —
    /// the directive parser pulled in ~1 MB of regex-automata rodata that
    /// self-hosters never exercised.
    #[arg(long, env = "ERREX_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// When true, the HTTP server permits CORS requests from the Vite dev
    /// server (http://localhost:5173). Off in production so the embedded SPA
    /// is the only browser surface.
    #[arg(long, env = "ERREX_DEV_MODE", default_value_t = false)]
    pub dev_mode: bool,

    /// When true, the ingest endpoint requires a `sentry_key` matching the
    /// configured project's token (see `errexd project add`). Defaults to
    /// `true` so a fresh deploy is fail-closed — an operator who knows the
    /// daemon sits behind a private network can opt out with
    /// `ERREX_REQUIRE_AUTH=false`. Sentry SDKs always send `sentry_key`, so
    /// this is the safe default for any internet-reachable bind.
    #[arg(long, env = "ERREX_REQUIRE_AUTH", default_value_t = true)]
    pub require_auth: bool,

    /// Days of event payload retention. The daemon purges older events
    /// hourly. Issue rows (counts, first/last seen) are kept regardless.
    /// `0` disables retention — events live forever until the disk fills.
    #[arg(long, env = "ERREX_RETENTION_DAYS", default_value_t = 30)]
    pub retention_days: u32,

    /// Payloads kept per issue; older ones roll off hourly while the issue
    /// row (counts, first/last seen) stays. The admin UI's value wins when
    /// it is non-zero. `0` here and there = unlimited. Sized so a public DSN
    /// cannot fill a small disk: one noisy issue keeps 200 samples.
    #[arg(long, env = "ERREX_EVENTS_PER_ISSUE_MAX", default_value_t = 200)]
    pub events_per_issue_max: u32,

    /// Issues kept per project; the least recently seen are dropped hourly.
    /// Same precedence and `0` meaning as `events_per_issue_max`.
    #[arg(long, env = "ERREX_ISSUES_PER_PROJECT_MAX", default_value_t = 5000)]
    pub issues_per_project_max: u32,

    /// Per-project ingest rate cap, events per minute. `0` = unlimited.
    /// The bucket also has burst capacity (`rate_limit_burst`) so short
    /// spikes don't get truncated; only sustained over-rate is rejected.
    ///
    /// Default 6000/min (≈100 events/sec per project): high enough that a
    /// healthy app's spike is allowed through, low enough that one
    /// misbehaving SDK can't consume the daemon's whole digest budget on
    /// a small VM. Set to `0` explicitly to disable.
    #[arg(long, env = "ERREX_RATE_LIMIT_PER_MIN", default_value_t = 6000)]
    pub rate_limit_per_min: u32,

    /// Burst capacity for the per-project rate limiter. Ignored when
    /// `rate_limit_per_min == 0`.
    #[arg(long, env = "ERREX_RATE_LIMIT_BURST", default_value_t = 200)]
    pub rate_limit_burst: u32,

    /// Externally-reachable base URL of this daemon. Embedded in webhook
    /// payloads + DSNs returned to the SPA so SDKs configured by users land
    /// on the right host. Defaults to the local daemon, which is rarely
    /// what an alert recipient or remote SDK needs.
    #[arg(
        long,
        env = "ERREX_PUBLIC_URL",
        default_value = "http://localhost:9090"
    )]
    pub public_url: String,

    /// Bearer token guarding `/api/admin/*` endpoints. Unset by default →
    /// admin endpoints respond 503. Operators set this to enable the SPA's
    /// project-management UI; empty string is treated as unset.
    #[arg(long, env = "ERREX_ADMIN_TOKEN", default_value = "")]
    pub admin_token: Option<String>,

    /// When true, the daemon trusts the first hop in the `X-Forwarded-For`
    /// header as the client IP for lockout / audit purposes. Off by default
    /// because the daemon's stock deploy is reachable directly — an attacker
    /// could otherwise rotate this header per request to bypass the per-IP
    /// lockout policy. Operators who front the daemon with a reverse proxy
    /// that strips and re-emits a single XFF should opt in via
    /// `ERREX_TRUST_PROXY_HEADERS=true`.
    #[arg(long, env = "ERREX_TRUST_PROXY_HEADERS", default_value_t = false)]
    pub trust_proxy_headers: bool,

    /// Read the client address from Cloudflare's `CF-Connecting-IP`, but only
    /// on connections from `cloudflare_trusted_peers`. For a daemon behind
    /// Cloudflare's proxy. Exclusive with `trust_proxy_headers`.
    #[arg(long, env = "ERREX_CLOUDFLARE_CLIENT_IP", default_value_t = false)]
    pub cloudflare_client_ip: bool,

    /// Comma-separated peers whose `CF-Connecting-IP` is believed. The word
    /// `cloudflare` is Cloudflare's published ranges; add the local reverse
    /// proxy (e.g. `::1/128`) when one sits in between and itself admits
    /// only Cloudflare.
    #[arg(
        long,
        env = "ERREX_CLOUDFLARE_TRUSTED_PEERS",
        value_delimiter = ',',
        default_value = "cloudflare"
    )]
    pub cloudflare_trusted_peers: Vec<String>,

    /// File holding the Telegram bot token (from @BotFather). Preferred over
    /// `telegram_bot_token`: under systemd use `LoadCredential=` and point
    /// this at `%d/<name>`. Telegram alerts are on when a token and
    /// `telegram_chat_id` are both set.
    #[arg(long, env = "ERREX_TELEGRAM_BOT_TOKEN_FILE")]
    pub telegram_bot_token_file: Option<PathBuf>,

    /// Telegram bot token, when a file is not practical.
    #[arg(long, env = "ERREX_TELEGRAM_BOT_TOKEN", hide_env_values = true)]
    pub telegram_bot_token: Option<String>,

    /// Chat to alert: a group or channel id (`-100…`) or `@channelname`.
    #[arg(long, env = "ERREX_TELEGRAM_CHAT_ID", allow_hyphen_values = true)]
    pub telegram_chat_id: Option<String>,

    /// Bot API base URL. Changed only by tests.
    #[arg(
        long,
        env = "ERREX_TELEGRAM_API_BASE",
        default_value = "https://api.telegram.org"
    )]
    pub telegram_api_base: String,

    /// Telegram messages per minute for new issues and regressions; the rest
    /// are counted and reported in the next message. Pages have their own
    /// budget at Telegram's group limit.
    #[arg(long, env = "ERREX_TELEGRAM_MAX_PER_MIN", default_value_t = 10)]
    pub telegram_max_per_min: u32,
}

impl Config {
    /// `ERREX_PORT` > `PORT` (PaaS convention) > 9090 default.
    pub fn resolved_http_port(&self) -> u16 {
        if let Some(p) = self.http_port {
            return p;
        }
        if let Ok(s) = std::env::var("PORT") {
            if let Ok(p) = s.parse::<u16>() {
                return p;
            }
        }
        9090
    }

    pub fn http_addr(&self) -> SocketAddr {
        bind_addr(&self.http_host, self.resolved_http_port()).expect("valid http bind addr")
    }

    pub fn mcp_addr(&self) -> SocketAddr {
        bind_addr(&self.mcp_host, self.mcp_port).expect("valid mcp bind addr")
    }

    pub fn client_ip_policy(&self) -> Result<ClientIpPolicy, String> {
        match (self.cloudflare_client_ip, self.trust_proxy_headers) {
            (true, true) => {
                Err("ERREX_CLOUDFLARE_CLIENT_IP and ERREX_TRUST_PROXY_HEADERS are exclusive".into())
            }
            (true, false) => {
                let trusted = client_ip::parse_trusted(&self.cloudflare_trusted_peers)?;
                if trusted.is_empty() {
                    return Err("ERREX_CLOUDFLARE_TRUSTED_PEERS is empty".into());
                }
                Ok(ClientIpPolicy::Cloudflare { trusted })
            }
            (false, true) => Ok(ClientIpPolicy::ForwardedFor),
            (false, false) => Ok(ClientIpPolicy::Peer),
        }
    }

    /// Telegram settings, `None` when alerts are not configured. Half a
    /// configuration is an error rather than silence.
    pub fn telegram(&self) -> Result<Option<TelegramConfig>, String> {
        let token = match &self.telegram_bot_token_file {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|e| format!("reading telegram token file {}: {e}", path.display()))?,
            ),
            None => self.telegram_bot_token.clone(),
        };
        let token = token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let chat_id = self
            .telegram_chat_id
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty());
        match (token, chat_id) {
            (None, None) => Ok(None),
            (Some(token), Some(chat_id)) => Ok(Some(TelegramConfig {
                api_base: self.telegram_api_base.trim_end_matches('/').to_string(),
                token,
                chat_id: chat_id.to_string(),
                max_per_min: self.telegram_max_per_min,
            })),
            (Some(_), None) => {
                Err("a Telegram bot token is set but ERREX_TELEGRAM_CHAT_ID is not".into())
            }
            (None, Some(_)) => Err("ERREX_TELEGRAM_CHAT_ID is set but no bot token is".into()),
        }
    }

    /// Default `public_url` is the local-loopback fallback. Detect it so
    /// `main` can log a warning when the daemon is bound to a public
    /// interface but the operator forgot to set `ERREX_PUBLIC_URL` to
    /// the real hostname — DSNs and webhook links would otherwise point
    /// at `localhost:9090` which is useless to remote SDKs.
    pub fn public_url_is_default(&self) -> bool {
        self.public_url == "http://localhost:9090"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// `rate_limit_per_min` defaults to a non-zero value so a self-host
    /// daemon ships with backpressure on by default. The exact number
    /// can change but `0` (unlimited) is wrong as a default.
    #[test]
    fn rate_limit_default_is_nonzero() {
        let cfg = Config::parse_from(["errexd"]);
        assert!(
            cfg.rate_limit_per_min > 0,
            "rate_limit_per_min default must be > 0, got {}",
            cfg.rate_limit_per_min
        );
    }

    /// Explicit `--http-port` always wins over the PORT env fallback.
    /// We don't mutate env vars in tests (cargo runs tests in
    /// parallel; PORT manipulation would race with other tests reading
    /// env), so this asserts only the deterministic path: explicit
    /// value present → use it.
    #[test]
    fn resolved_http_port_explicit_wins() {
        let cfg = Config::parse_from(["errexd", "--http-port", "12345"]);
        assert_eq!(cfg.resolved_http_port(), 12345);
    }

    #[test]
    fn public_url_default_detection() {
        let cfg = Config::parse_from(["errexd"]);
        assert!(cfg.public_url_is_default());
        let cfg2 = Config::parse_from(["errexd", "--public-url", "https://errex.example.com"]);
        assert!(!cfg2.public_url_is_default());
    }

    /// Fail-closed default: a fresh daemon without explicit configuration
    /// must not be reachable off-host. Operators in docker / on remote
    /// hosts opt in to `0.0.0.0` deliberately — a public bind is never the
    /// implicit behaviour.
    #[test]
    fn http_host_default_is_loopback() {
        let cfg = Config::parse_from(["errexd"]);
        assert_eq!(cfg.http_host, "127.0.0.1");
    }

    #[test]
    fn mcp_host_default_is_loopback() {
        let cfg = Config::parse_from(["errexd"]);
        assert_eq!(cfg.mcp_host, "127.0.0.1");
    }

    /// Fail-closed default: ingest is auth-gated unless the operator
    /// explicitly opts out. Without this, a public daemon would happily
    /// accept anonymous events from anyone on the network and let an
    /// attacker exhaust the digest budget.
    #[test]
    fn ipv6_hosts_bind() {
        let cfg = Config::parse_from(["errexd", "--http-host", "::1", "--http-port", "9090"]);
        assert_eq!(cfg.http_addr(), "[::1]:9090".parse().unwrap());
        let cfg = Config::parse_from(["errexd", "--http-host", "[::1]", "--http-port", "9090"]);
        assert_eq!(cfg.http_addr(), "[::1]:9090".parse().unwrap());
        let cfg = Config::parse_from(["errexd", "--mcp-host", "::1"]);
        assert_eq!(cfg.mcp_addr(), "[::1]:9092".parse().unwrap());
    }

    #[test]
    fn caps_default_to_bounded_values() {
        let cfg = Config::parse_from(["errexd"]);
        assert_eq!(cfg.events_per_issue_max, 200);
        assert_eq!(cfg.issues_per_project_max, 5000);
    }

    #[test]
    fn client_ip_policy_follows_the_flags() {
        let cfg = Config::parse_from(["errexd"]);
        assert_eq!(cfg.client_ip_policy().unwrap(), ClientIpPolicy::Peer);
        let cfg = Config::parse_from(["errexd", "--trust-proxy-headers"]);
        assert_eq!(
            cfg.client_ip_policy().unwrap(),
            ClientIpPolicy::ForwardedFor
        );
        let cfg = Config::parse_from([
            "errexd",
            "--cloudflare-client-ip",
            "--cloudflare-trusted-peers",
            "cloudflare,::1/128",
        ]);
        match cfg.client_ip_policy().unwrap() {
            ClientIpPolicy::Cloudflare { trusted } => {
                assert_eq!(trusted.len(), client_ip::CLOUDFLARE_RANGES.len() + 1)
            }
            other => panic!("expected Cloudflare, got {other:?}"),
        }
        let both =
            Config::parse_from(["errexd", "--cloudflare-client-ip", "--trust-proxy-headers"]);
        assert!(both.client_ip_policy().is_err());
        let bad = Config::parse_from([
            "errexd",
            "--cloudflare-client-ip",
            "--cloudflare-trusted-peers",
            "not-a-range",
        ]);
        assert!(bad.client_ip_policy().is_err());
    }

    #[test]
    fn telegram_is_off_unless_configured() {
        let cfg = Config::parse_from(["errexd"]);
        assert!(cfg.telegram().unwrap().is_none());
    }

    #[test]
    fn telegram_token_file_is_read_and_trimmed() {
        let dir = std::env::temp_dir().join(format!("errexd-tg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, "123:abc\n").unwrap();
        let cfg = Config::parse_from([
            "errexd",
            "--telegram-bot-token-file",
            file.to_str().unwrap(),
            "--telegram-chat-id",
            "-1001234",
            "--telegram-api-base",
            "http://127.0.0.1:1/",
        ]);
        let tg = cfg.telegram().unwrap().unwrap();
        assert_eq!(tg.token, "123:abc");
        assert_eq!(tg.chat_id, "-1001234");
        assert_eq!(tg.api_base, "http://127.0.0.1:1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn half_a_telegram_configuration_is_an_error() {
        let cfg = Config::parse_from(["errexd", "--telegram-bot-token", "123:abc"]);
        assert!(cfg.telegram().is_err());
        let cfg = Config::parse_from(["errexd", "--telegram-chat-id", "-100"]);
        assert!(cfg.telegram().is_err());
    }

    #[test]
    fn require_auth_default_is_true() {
        let cfg = Config::parse_from(["errexd"]);
        assert!(cfg.require_auth, "ingest must be auth-gated by default");
    }
}
