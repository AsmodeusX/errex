# Configuration

Everything you need to operate an errex deployment: env vars, the `errexd`
CLI, and how to wire up SDKs and webhooks.

## Environment variables

All configuration is via env vars. Defaults are sensible for a single-host
self-host.

| Variable | Default | What it does |
|---|---|---|
| `ERREX_DATA_DIR` | `./data` | SQLite file location |
| `ERREX_PORT` | `PORT` env, else `9090` | HTTP + SPA + WebSocket fan-out (single axum listener). Reads `PORT` automatically so Railway / Fly / Render / Heroku one-click deploys "just work". |
| `ERREX_MCP_PORT` | `9092` | MCP listener (stub) |
| `ERREX_LOG_LEVEL` | `info` | tracing filter |
| `ERREX_DEV_MODE` | `false` | Enable CORS for the Vite dev server |
| `ERREX_HOST` | `127.0.0.1` | HTTP bind address. A bare IPv6 address (`::1`) is accepted. |
| `ERREX_REQUIRE_AUTH` | `true` | Validate `sentry_key` on ingest. Set `false` only on a private network. |
| `ERREX_RETENTION_DAYS` | `30` | Purge events older than N days; `0` disables |
| `ERREX_EVENTS_PER_ISSUE_MAX` | `200` | Payloads kept per issue (issue counts survive); `0` = unlimited |
| `ERREX_ISSUES_PER_PROJECT_MAX` | `5000` | Issues kept per project, least recently seen dropped first; `0` = unlimited |
| `ERREX_RATE_LIMIT_PER_MIN` | `6000` | Per-project ingest cap; `0` = unlimited. A 429 carries `Retry-After` and `X-Sentry-Rate-Limits`. |
| `ERREX_RATE_LIMIT_BURST` | `200` | Token-bucket burst capacity |
| `ERREX_TRUST_PROXY_HEADERS` | `false` | Login lockout reads the first `X-Forwarded-For` hop |
| `ERREX_CLOUDFLARE_CLIENT_IP` | `false` | Login lockout reads `CF-Connecting-IP`, believed only from `ERREX_CLOUDFLARE_TRUSTED_PEERS`. Exclusive with `ERREX_TRUST_PROXY_HEADERS`. |
| `ERREX_CLOUDFLARE_TRUSTED_PEERS` | `cloudflare` | Comma-separated CIDRs; `cloudflare` = Cloudflare's published ranges. Add `::1/128` when a local reverse proxy that admits only Cloudflare sits in between. |
| `ERREX_TELEGRAM_BOT_TOKEN_FILE` | _(unset)_ | File holding the Telegram bot token (preferred; works with systemd `LoadCredential=`) |
| `ERREX_TELEGRAM_BOT_TOKEN` | _(unset)_ | Telegram bot token, when a file is not practical |
| `ERREX_TELEGRAM_CHAT_ID` | _(unset)_ | Group or channel to alert (`-100…` or `@name`). Telegram alerts are on when this and a token are set. |
| `ERREX_TELEGRAM_MAX_PER_MIN` | `10` | Telegram messages per minute for new issues and regressions; `0` = unlimited. Pages have their own budget. |
| `ERREX_PUBLIC_URL` | `http://localhost:9090` | **Set this for any non-localhost deployment.** Embedded in DSNs, webhook payloads, and the dashboard's curl example. The daemon warns at boot if you bound to a public interface but left this at the default. |
| `ERREX_ADMIN_TOKEN` | _(unset)_ | Setup-wizard token; required to bootstrap the first admin user |

## Custom domain — one env var

To put errex behind `https://errex.example.com`, set exactly one variable:

```bash
ERREX_PUBLIC_URL=https://errex.example.com
```

That's it. The DSN handed to SDKs becomes
`https://<token>@errex.example.com/<project>` (the canonical Sentry shape
every SDK parses). Webhook payloads link back to
`https://errex.example.com/issues/<id>`. The dashboard's curl example
points at `https://errex.example.com/api/<project>/envelope/`.

Per-platform recipes:

### Railway / Fly / Render / Heroku

1. Add a custom domain in the platform UI.
2. Set the service env vars:
   - `ERREX_PUBLIC_URL=https://errex.example.com`
   - `ERREX_ADMIN_TOKEN=$(openssl rand -hex 16)` (first run only)
3. Deploy. The platform sets `PORT`; errexd reads it automatically.
   No `ERREX_PORT` override needed.

### Caddy

```Caddyfile
errex.example.com {
    reverse_proxy localhost:9090
}
```

```bash
ERREX_PUBLIC_URL=https://errex.example.com docker compose -f docker/docker-compose.yml up -d
```

Caddy handles TLS automatically.

### Nginx / Traefik

Forward `Host`, `X-Forwarded-Proto`, and the `Upgrade`/`Connection`
headers required for the WebSocket. The SPA builds its WS URL from
`window.location`, so as long as the proxy upgrade path is wired,
realtime updates work without extra config.

```nginx
server {
    listen 443 ssl;
    server_name errex.example.com;

    location / {
        proxy_pass http://127.0.0.1:9090;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
    }
}
```

### Ports

errex listens on two ports by default:

- `9090` — HTTP (Sentry ingest, REST API, embedded SPA, WebSocket upgrade)
- `9092` — MCP listener (stub for AI agents)

If you put errex behind a reverse proxy, forward everything to `9090` —
the SPA, REST API, and WebSocket fan-out all share that listener. The SPA
builds its WS URL from `location.host`, so anything that already proxies
HTTP correctly will handle `/ws/<project>` upgrades without extra config
(make sure the proxy passes through `Upgrade`/`Connection` headers).

## First-run setup

The setup wizard at `/setup` requires a one-shot **setup token** so an
attacker can't claim the admin slot before you do. Set
`ERREX_ADMIN_TOKEN` to a value you control before starting the daemon
the first time:

```yaml
# docker/docker-compose.yml
environment:
  ERREX_ADMIN_TOKEN: ${ERREX_ADMIN_TOKEN:-changeme}
```

```bash
ERREX_ADMIN_TOKEN=$(openssl rand -hex 16) docker compose -f docker/docker-compose.yml up -d
```

Open <http://localhost:9090/setup>, paste the token, choose a username
and password. After the first admin exists, the setup endpoint refuses
all further calls regardless of token.

## Project management

All project ops are CLI subcommands on the `errexd` binary. They open the
SQLite file directly (WAL mode), so you can run them while the daemon is
up.

```bash
errexd project add <name> [--public-url URL]    # create + emit DSN
errexd project list                              # show tokens + last-used
errexd project rotate <name>                     # invalidate previous DSN
errexd project set-webhook <name> <url>          # Slack/Discord/Teams URL
errexd project unset-webhook <name>
```

Inside Docker:

```bash
docker compose -f docker/docker-compose.yml exec errexd errexd project list
```

`project add` prints both the SDK-facing DSN and a curl-friendly URL:

```
project:    my-app
token:      3f4a9b8e2c1d4f5e8a7b6c5d4e3f2a1b
dsn:        https://3f4a9b8e2c1d4f5e8a7b6c5d4e3f2a1b@errex.example.com/my-app
ingest_url: https://errex.example.com/api/my-app/envelope/?sentry_key=3f4a...
```

The `dsn` is the canonical Sentry shape — every official SDK parses it.
The `ingest_url` is for hand-testing with curl; the SDK derives it
internally from the DSN.

Ingest requires this token by default. `ERREX_REQUIRE_AUTH=false` turns
the check off, which is only reasonable on a private network.

## Wiring up an SDK

errex speaks the [Sentry envelope](https://develop.sentry.dev/sdk/envelopes/)
wire format. Any official `@sentry/*` SDK works unchanged — just paste
the DSN.

JavaScript / TypeScript:

```js
import * as Sentry from '@sentry/browser';
Sentry.init({
  dsn: 'https://3f4a...@errex.example.com/my-app'
});
```

Python:

```python
import sentry_sdk
sentry_sdk.init(dsn="https://3f4a...@errex.example.com/my-app")
```

The SDK constructs the actual ingest URL (`/api/my-app/envelope/`) from
the DSN and authenticates with the standard `X-Sentry-Auth` header. errex
also accepts the token via `?sentry_key=` query param for curl-style
testing.

## Webhook alerts

Set a webhook URL on a project and errex will POST a Slack-compatible
payload on:

- **First occurrence** of a new fingerprint (color: `danger`)
- **Regression** — a resolved issue saw a new event (color: `warning`)

Slack / Discord (with the `/slack` suffix) / Teams "Incoming Webhook"
all accept the same shape.

```bash
errexd project set-webhook my-app https://hooks.slack.com/services/T0/B0/XXXX
```

Muted and ignored issues never fire webhooks, with one exception: a
**page**. An event with level `fatal` and the tag `flash.erased=true`
(an ECU left with erased flash) alerts on every occurrence, whatever the
issue's state.

## Telegram alerts

One bot posts to one group or channel, on the same triggers as the
webhooks plus pages. Each message names the project, level, title,
release, event count and the issue's dashboard link. New issues and
regressions share `ERREX_TELEGRAM_MAX_PER_MIN`; what that holds back is
counted in the next message. Pages have their own budget at Telegram's
group limit, so a burst of new issues cannot delay one.

```bash
ERREX_TELEGRAM_BOT_TOKEN_FILE=/run/credentials/errexd.service/telegram_bot_token
ERREX_TELEGRAM_CHAT_ID=-1001234567890
```

A token without a chat id, or the reverse, stops the daemon at boot.

## What ingest keeps

Every field an SDK sends is kept, including ones errex does not model
(`debug_meta`, `sdk`, `dist`, `threads`, an exception's `mechanism`, a
frame's `instruction_addr`), and returned by `/api/issues/:id/event`. The
dashboard shows them under **Additional data**.

Before anything is stored, logged or alerted on, the daemon scrubs the
event:

- `user` and `request` are dropped, and so is every key the licensing
  server refuses as non-metadata (`attachment`, `crash_dump`,
  `screenshot`, `calibration`, `zfl`, `zfle`, `diff`, … — see
  `crates/errexd/src/scrub.rs`), at any depth, plus frame `vars`.
- In messages, exception values, breadcrumbs, tags and `extra`: VINs
  become `<vin>`, `0x` plus 4+ hex digits and bare hex runs of 8+ become
  `<hex>`, e-mail addresses `<email>`, Bearer tokens, JWTs and long
  base64 runs `<secret>`. `extra.dart_stack` keeps its hex so it can be
  symbolicated.

Ingest never reads, logs or stores the client's address.

## Dashboard keyboard shortcuts

- `Cmd+K` — command palette
- `j` / `k` — navigate the issue list
- `e` — resolve
- `m` — mute
- `a` — assign to me
- `/` — focus the filter input
